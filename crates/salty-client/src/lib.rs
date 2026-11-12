//! `salty-client`: the device side of Salty Gull for desktop full updates (G3 MVP).
//!
//! The loop, all state kept in one app-owned directory:
//!
//! 1. [`Updater::check`] fetches the bundle, verifies it with `weirdnet-update` against the
//!    root pin compiled into the app (anti-rollback state persists), evaluates targeting and
//!    rollout on the device, and applies the user's consent policy. Updates are never forced.
//! 2. [`Updater::accept`] / [`Updater::decline`] record the user's answer to a prompt.
//! 3. [`Updater::download`] fetches each file, checks size and BLAKE3, and stages the update.
//! 4. On the next start the app calls [`Updater::begin_boot`] and runs the staged build when
//!    told to; after its first frame and its own ready signal it calls [`Updater::mark_ready`].
//!    Two failed trial boots roll back automatically and the update is refused from then on.
//!
//! Platform installers (swapping an app directory, MSI, `.app`, deb/rpm through the system
//! prompt) take the staged directory from here; they are separate from this crate.
#![forbid(unsafe_code)]

pub mod install;
mod transport;

pub use install::Plan;
pub use transport::{HttpTransport, Transport};

use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use weirdnet_update::health::{BootChoice, BootHealth};
use weirdnet_update::policy::{ConsentLog, Decision, decide};
use weirdnet_update::target::Device;
use weirdnet_update::verify::Applicability;
use weirdnet_update::{Client, ClientState, Hash32, Policy, RootPin, Timestamp, UpdateKind, UpdateManifest, VerifiedUpdate, VerifyOptions};

/// Errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Verification or a record format failed.
    #[error("{0}")]
    Update(#[from] weirdnet_update::Error),
    /// Wire format.
    #[error("{0}")]
    Wire(#[from] salty_core::Error),
    /// Network.
    #[error("transport: {0}")]
    Transport(String),
    /// Local files.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// State files.
    #[error("state file: {0}")]
    State(String),
    /// Wrong call order or bad input.
    #[error("{0}")]
    Invalid(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// What the app is and where its state lives.
#[derive(Clone, Debug)]
pub struct Config {
    /// App-owned directory for updater state and staged updates.
    pub dir: PathBuf,
    /// App id.
    pub app: String,
    /// Channel.
    pub channel: String,
    /// The org root fingerprint compiled into the app.
    pub pin: RootPin,
    /// The running version.
    pub client_version: Version,
    /// The installed base release, if known.
    pub base_release: Option<String>,
    /// Distribution flavor and options.
    pub policy: Policy,
    /// The platform signing identity of the running build; full updates must keep it.
    pub signing_identity: Option<String>,
    /// The publisher's minisign public key (base64); when set, full packages must carry a
    /// valid `.minisig`.
    pub minisign_key: Option<String>,
}

/// The result of a check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// Nothing newer on the channel.
    UpToDate,
    /// The channel has an update this device isn't targeted by, or isn't in the rollout yet.
    NotForThisDevice(Applicability),
    /// An update to act on, with what the consent policy says to do.
    Offer {
        /// The verified manifest.
        manifest: Box<UpdateManifest>,
        /// Install, ask, notify or skip.
        decision: Decision,
    },
}

#[derive(Default, Serialize, Deserialize)]
struct Local {
    consent: ConsentLog,
    health: BootHealth,
    /// Capabilities the user granted before.
    granted: BTreeSet<String>,
    /// The verified manifest of the last offer, kept until it is staged and committed.
    pending: Option<Pending>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    manifest: UpdateManifest,
    manifest_hash: Hash32,
}

/// The updater for one app and channel.
pub struct Updater<T: Transport> {
    cfg: Config,
    transport: T,
    client: Option<Client>,
    local: Local,
}

fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(Error::Invalid(format!("unsafe path in manifest: {rel}")));
    }
    Ok(base.join(p))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}

impl<T: Transport> Updater<T> {
    /// Opens the updater, loading saved state from `cfg.dir`.
    pub fn open(cfg: Config, transport: T) -> Result<Self> {
        fs::create_dir_all(cfg.dir.join("updates"))?;
        fs::create_dir_all(cfg.dir.join("versions"))?;
        let local = match fs::read(cfg.dir.join("local.json")) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| Error::State(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Local::default(),
            Err(e) => return Err(e.into()),
        };
        let client = match fs::read(cfg.dir.join("trust.cbor")) {
            Ok(b) => Some(Client::from_state(cfg.pin, ClientState::from_bytes(&b)?, Timestamp::now())?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        Ok(Self { cfg, transport, client, local })
    }

    fn save(&self) -> Result<()> {
        if let Some(c) = &self.client {
            write_atomic(&self.cfg.dir.join("trust.cbor"), &c.state().to_bytes()?)?;
        }
        let json = serde_json::to_vec_pretty(&self.local).map_err(|e| Error::State(e.to_string()))?;
        write_atomic(&self.cfg.dir.join("local.json"), &json)
    }

    /// The user's consent log (mode changes and per-update answers).
    pub fn consent(&self) -> &ConsentLog {
        &self.local.consent
    }

    /// Switches the app's update mode (ask, auto, notify-only, off).
    pub fn set_mode(&mut self, mode: weirdnet_update::ConsentMode, now: Timestamp) -> Result<()> {
        self.local.consent.set_mode(self.cfg.policy.flavor, mode, now)?;
        self.save()
    }

    /// Checks the channel. `device` carries this device's targeting attributes.
    pub fn check(&mut self, device: &Device, now: Timestamp) -> Result<Check> {
        if self.local.consent.mode(self.cfg.policy.flavor) == weirdnet_update::ConsentMode::Off {
            return Ok(Check::UpToDate);
        }
        let bundle = self.transport.bundle(&self.cfg.app, &self.cfg.channel)?.to_bundle()?;
        if bundle.manifest.is_none() {
            return Ok(Check::UpToDate);
        }
        if self.client.is_none() {
            let first = bundle.roots.first().ok_or_else(|| Error::Invalid("server sent no root".into()))?;
            self.client = Some(Client::new(self.cfg.pin, first, now)?);
        }
        let client = self.client.as_mut().expect("client set above");
        // The chain starts at the pinned root; send only the rotations after it.
        let have = client.root().version as usize;
        let rest = weirdnet_update::UpdateBundle { roots: bundle.roots.iter().skip(have).cloned().collect(), ..bundle };
        let mut opts = VerifyOptions::new(now, self.cfg.client_version.clone());
        opts.base_release = self.cfg.base_release.clone();
        let verified = client.verify(&self.cfg.app, &self.cfg.channel, &rest, &opts)?;
        let result = self.offer(verified, device);
        self.save()?;
        result
    }

    fn offer(&mut self, v: VerifiedUpdate, device: &Device) -> Result<Check> {
        let m = &v.manifest;
        if let (Some(p), Some(want)) = (install::full(m), &self.cfg.signing_identity)
            && &p.signing_identity != want
        {
            return Err(Error::Invalid(format!("{}: signing identity changed ({} → {})", m.update_id, want, p.signing_identity)));
        }
        let current = match &m.kind {
            UpdateKind::Full(_) => m.version <= self.cfg.client_version,
            _ => false,
        };
        if current || self.committed_id() == Some(m.update_id.as_str()) {
            return Ok(Check::UpToDate);
        }
        match v.applies(device) {
            Applicability::Applies => {}
            other => return Ok(Check::NotForThisDevice(other)),
        }
        let mut decision = decide(&self.cfg.policy, &self.local.consent, m, &self.local.granted);
        if Self::notify_only(m) && !matches!(decision, Decision::Skip(_)) {
            decision = Decision::Notify { security: m.labels.security.is_some() };
        }
        self.local.pending = Some(Pending { manifest: m.clone(), manifest_hash: v.manifest_hash });
        Ok(Check::Offer { manifest: Box::new(m.clone()), decision })
    }

    fn notify_only(m: &UpdateManifest) -> bool {
        install::full(m).is_some_and(|p| p.format.install_mode() == weirdnet_update::manifest::InstallMode::NotifyOnly)
    }

    /// The text to show for an update in a notify-only format (TAUR, Nix, Homebrew, winget).
    pub fn notice(&self, update_id: &str) -> Result<Option<String>> {
        let m = &self.pending(update_id)?.manifest;
        Ok(install::full(m).and_then(|p| install::notice(m, p)))
    }

    fn committed_id(&self) -> Option<&str> {
        match &self.local.health.state {
            weirdnet_update::health::HealthState::Committed { update_id } => Some(update_id),
            _ => None,
        }
    }

    fn pending(&self, update_id: &str) -> Result<&Pending> {
        self.local
            .pending
            .as_ref()
            .filter(|p| p.manifest.update_id == update_id)
            .ok_or_else(|| Error::Invalid(format!("{update_id} is not the update on offer; check again")))
    }

    /// Records that the user accepted the update (and the capabilities it lists).
    pub fn accept(&mut self, update_id: &str, now: Timestamp) -> Result<()> {
        let caps = self.pending(update_id)?.manifest.capabilities.clone();
        self.local.consent.decide(update_id, true, now);
        self.local.granted.extend(caps);
        self.save()
    }

    /// Records that the user declined the update. It is not offered again.
    pub fn decline(&mut self, update_id: &str, now: Timestamp) -> Result<()> {
        self.pending(update_id)?;
        self.local.consent.decide(update_id, false, now);
        self.local.pending = None;
        self.save()
    }

    /// Downloads, verifies and stages the update on offer. Allowed after auto-mode `Install`
    /// or an accepted prompt. Returns the staged directory.
    pub fn download(&mut self, update_id: &str) -> Result<PathBuf> {
        let p = self.pending(update_id)?.clone();
        if Self::notify_only(&p.manifest) {
            return Err(Error::Invalid(format!("{update_id} is installed by the package manager; show the notice instead")));
        }
        let decision = decide(&self.cfg.policy, &self.local.consent, &p.manifest, &self.local.granted);
        if decision != Decision::Install {
            return Err(Error::Invalid(format!("{update_id} needs the user's consent first")));
        }
        let updates = self.cfg.dir.join("updates");
        let part = updates.join(format!("{update_id}.part"));
        let done = updates.join(update_id);
        if part.exists() {
            fs::remove_dir_all(&part)?;
        }
        fs::create_dir_all(&part)?;
        for f in &p.manifest.files {
            let dest = safe_join(&part, &f.path)?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut src = self.transport.blob(&f.blake3)?;
            let mut out = fs::File::create(&dest)?;
            let mut check = f.verifier();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                check.update(&buf[..n])?;
                out.write_all(&buf[..n])?;
            }
            check.finish()?;
            out.sync_all()?;
        }
        if done.exists() {
            fs::remove_dir_all(&done)?;
        }
        if let (Some(_), Some(key)) = (install::full(&p.manifest), &self.cfg.minisign_key) {
            install::verify_minisign(&part, install::package_file(&p.manifest)?, key)?;
        }
        fs::rename(&part, &done)?;
        if install::full(&p.manifest).is_none() {
            // Assets and Wasm load from the staged directory on the next start.
            self.local.health.stage(update_id);
        }
        self.save()?;
        Ok(done)
    }

    /// For a downloaded full update: how to install it. Swap formats are unpacked into a
    /// versioned directory and staged for a trial boot; installer and package-manager formats
    /// return the command to run once the user confirms it.
    pub fn install_plan(&mut self, update_id: &str) -> Result<Plan> {
        let p = self.pending(update_id)?.clone();
        let full = install::full(&p.manifest).ok_or_else(|| Error::Invalid(format!("{update_id} is not a full update")))?;
        let staged = self.cfg.dir.join("updates").join(update_id);
        if !staged.is_dir() {
            return Err(Error::Invalid(format!("{update_id} is not downloaded")));
        }
        let plan = install::plan(&p.manifest, full, &staged, &self.cfg.dir.join("versions"))?;
        if matches!(plan, Plan::Swap { .. }) {
            self.local.health.stage(update_id);
            self.save()?;
        }
        Ok(plan)
    }

    /// Call first on every start. Returns which build to run: the staged candidate's directory,
    /// or `None` for the current build (after a rollback too).
    pub fn begin_boot(&mut self) -> Result<Option<PathBuf>> {
        let choice = self.local.health.begin_boot();
        let out = match choice {
            BootChoice::Candidate(id) => {
                let v = self.cfg.dir.join("versions").join(&id);
                Some(if v.is_dir() { v } else { self.cfg.dir.join("updates").join(id) })
            }
            BootChoice::RollBack(id) => {
                if let Some(c) = self.client.as_mut() {
                    c.mark_bad(&id);
                }
                for dir in [self.cfg.dir.join("updates").join(&id), self.cfg.dir.join("versions").join(&id)] {
                    if dir.exists() {
                        fs::remove_dir_all(dir)?;
                    }
                }
                self.local.pending = None;
                None
            }
            BootChoice::Current => None,
        };
        self.save()?;
        Ok(out)
    }

    /// Call after the first frame and the app's own ready signal. Commits a trial update.
    pub fn mark_ready(&mut self) -> Result<Option<String>> {
        let committed = self.local.health.mark_ready();
        // Only a just-committed trial consumes the pending offer; an offer still waiting for
        // consent survives restarts.
        if let Some(id) = &committed
            && self.local.pending.as_ref().is_some_and(|p| &p.manifest.update_id == id)
            && let (Some(p), Some(c)) = (self.local.pending.take(), self.client.as_mut())
        {
            c.mark_installed(&VerifiedUpdate { manifest: p.manifest, manifest_hash: p.manifest_hash, freshness: None });
        }
        self.save()?;
        Ok(committed)
    }
}

#[cfg(test)]
mod tests;
