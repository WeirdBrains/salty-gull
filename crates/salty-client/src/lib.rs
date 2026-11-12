//! `salty-client`: the device side of Salty Gull for desktop full updates (G3 MVP).
//!
//! The loop, all state kept in one app-owned directory:
//!
//! 1. [`Updater::check`] fetches the bundle, verifies it with `weirdnet-update` against the
//!    root pin compiled into the app (anti-rollback state persists), evaluates targeting and
//!    rollout on the device, and applies the user's consent policy. Updates are never forced.
//! 2. [`Updater::accept`] / [`Updater::decline`] record the user's answer to a prompt.
//! 3. [`Updater::download`] fetches each file, checks size and BLAKE3, and stages the update.
//! 4. [`Updater::install_plan`] unpacks swap formats into `versions/<id>` or returns the
//!    installer command (MSI, dmg, deb/rpm through the system prompt) for the app to run after
//!    the user confirms; then the app calls [`Updater::installed`].
//! 5. On the next start the app (or the `salty-launch` launcher, through [`begin_boot`]) gets a
//!    [`Boot`]: which build to run, and whether it is on trial. After its first frame and its own
//!    ready signal the app calls [`Updater::mark_ready`]. Two failed trial boots roll back to the
//!    previous good build automatically and the update is refused from then on.
#![forbid(unsafe_code)]

pub mod install;
mod transport;

pub use install::{Command, Plan};
pub use transport::{HttpTransport, Transport};

use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use weirdnet_update::health::{BootChoice, BootHealth};
use weirdnet_update::manifest::{Arch, InstallMode, PackageFormat, Platform};
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
    /// What this build is (platform, CPU, package format). Full updates for anything else are
    /// refused, so one channel can't hand a Linux archive to a Windows install.
    pub target: Option<Target>,
}

/// The platform, CPU and package format of the running build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// OS.
    pub platform: Platform,
    /// CPU.
    pub arch: Arch,
    /// How this build was installed.
    pub format: PackageFormat,
}

/// What to run at this start; from [`Updater::begin_boot`] or [`begin_boot`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Boot {
    /// The directory to run or load from: a swapped-in build (`versions/<id>`) or staged assets
    /// (`updates/<id>`). `None` means the installed build.
    pub dir: Option<PathBuf>,
    /// The update on trial at this start. Call [`Updater::mark_ready`] once the app is healthy.
    pub trial: Option<String>,
    /// The update that just failed its trial and was rolled back.
    pub rolled_back: Option<String>,
    /// After a rollback of an installer format (MSI): the command that reinstalls the previous
    /// good package. Run it, then start again. `None` when there is no earlier package kept.
    pub reinstall: Option<Command>,
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
    /// The update staged for a trial boot.
    #[serde(default)]
    staged: Option<Slot>,
    /// The last committed update: the build to run when nothing is on trial.
    #[serde(default)]
    good: Option<Slot>,
    /// Updates that failed their trial. Applied to the trust state when the updater opens, so a
    /// launcher can roll back without it.
    #[serde(default)]
    bad: BTreeSet<String>,
}

/// An update's local files, relative to the state directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Slot {
    update_id: String,
    /// What to run or load from (`versions/<id>` for swaps, `updates/<id>` for assets).
    run: Option<String>,
    /// A kept installer package (MSI) for reinstalling after a later failed update.
    package: Option<String>,
    /// For installer formats: the version the installer puts in place. A start that reports
    /// another version is the old build (the installer did not run), so it commits nothing.
    #[serde(default)]
    version: Option<String>,
}

fn remove_dirs(base: &Path, id: &str) -> Result<()> {
    for d in [base.join("updates").join(id), base.join("versions").join(id)] {
        if d.exists() {
            fs::remove_dir_all(d)?;
        }
    }
    Ok(())
}

impl Local {
    fn load(dir: &Path) -> Result<Self> {
        match fs::read(dir.join("local.json")) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| Error::State(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, dir: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| Error::State(e.to_string()))?;
        write_atomic(&dir.join("local.json"), &json)
    }

    fn good_dir(&self, base: &Path) -> Option<PathBuf> {
        self.good.as_ref()?.run.as_ref().map(|r| base.join(r)).filter(|d| d.exists())
    }

    fn begin_boot(&mut self, base: &Path) -> Result<Boot> {
        Ok(match self.health.begin_boot() {
            BootChoice::Candidate(id) => {
                let run = self.staged.as_ref().filter(|s| s.update_id == id).and_then(|s| s.run.clone());
                Boot { dir: run.map(|r| base.join(r)), trial: Some(id), ..Boot::default() }
            }
            BootChoice::RollBack(id) => {
                self.bad.insert(id.clone());
                remove_dirs(base, &id)?;
                self.staged = None;
                if self.pending.as_ref().is_some_and(|p| p.manifest.update_id == id) {
                    self.pending = None;
                }
                let reinstall =
                    self.good.as_ref().and_then(|g| g.package.as_ref()).map(|p| base.join(p)).filter(|p| p.exists()).map(|p| install::reinstall(&p));
                Boot { dir: self.good_dir(base), rolled_back: Some(id), reinstall, ..Boot::default() }
            }
            BootChoice::Current => Boot { dir: self.good_dir(base), ..Boot::default() },
        })
    }

    /// Makes the committed trial the good build and drops the previous one's files.
    fn commit(&mut self, base: &Path, id: &str) -> Result<()> {
        let Some(slot) = self.staged.take().filter(|s| s.update_id == id) else { return Ok(()) };
        if slot.run.as_deref() == Some(&format!("versions/{id}")) {
            // The archive was unpacked; the download is no longer needed.
            let dl = base.join("updates").join(id);
            if dl.exists() {
                fs::remove_dir_all(dl)?;
            }
        }
        if let Some(old) = self.good.replace(slot)
            && old.update_id != id
        {
            remove_dirs(base, &old.update_id)?;
        }
        Ok(())
    }
}

/// For launchers: starts a boot from the state directory alone (no network, no keys). Same as
/// [`Updater::begin_boot`]; the app must not call that again in the process it starts.
pub fn begin_boot(state_dir: &Path) -> Result<Boot> {
    let mut local = Local::load(state_dir)?;
    let boot = local.begin_boot(state_dir)?;
    local.save(state_dir)?;
    Ok(boot)
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
        let local = Local::load(&cfg.dir)?;
        let mut client = match fs::read(cfg.dir.join("trust.cbor")) {
            Ok(b) => Some(Client::from_state(cfg.pin, ClientState::from_bytes(&b)?, Timestamp::now())?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(c) = client.as_mut() {
            local.bad.iter().for_each(|id| c.mark_bad(id));
        }
        Ok(Self { cfg, transport, client, local })
    }

    fn save(&self) -> Result<()> {
        if let Some(c) = &self.client {
            write_atomic(&self.cfg.dir.join("trust.cbor"), &c.state().to_bytes()?)?;
        }
        self.local.save(&self.cfg.dir)
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
            let mut c = Client::new(self.cfg.pin, first, now)?;
            self.local.bad.iter().for_each(|id| c.mark_bad(id));
            self.client = Some(c);
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
        if let (Some(p), Some(t)) = (install::full(m), &self.cfg.target)
            && (p.platform, p.arch, p.format) != (t.platform, t.arch, t.format)
        {
            return Err(Error::Invalid(format!(
                "{}: the update is a {:?} {:?} {:?} package; this build is {:?} {:?} {:?}",
                m.update_id, p.platform, p.arch, p.format, t.platform, t.arch, t.format
            )));
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
            self.stage(Slot { update_id: update_id.into(), run: Some(format!("updates/{update_id}")), package: None, version: None });
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
        let mut staged = self.cfg.dir.join("updates").join(update_id);
        if full.format == PackageFormat::Msi {
            // Kept for reinstalling after a later failed update; the installer runs it from here.
            let keep = self.cfg.dir.join("versions").join(update_id);
            if staged.is_dir() {
                if keep.exists() {
                    fs::remove_dir_all(&keep)?;
                }
                fs::rename(&staged, &keep)?;
            }
            staged = keep;
        }
        if !staged.is_dir() {
            return Err(Error::Invalid(format!("{update_id} is not downloaded")));
        }
        let plan = install::plan(&p.manifest, full, &staged, &self.cfg.dir.join("versions"))?;
        if matches!(plan, Plan::Swap { .. }) {
            self.stage(Slot { update_id: update_id.into(), run: Some(format!("versions/{update_id}")), package: None, version: None });
            self.save()?;
        }
        Ok(plan)
    }

    fn stage(&mut self, slot: Slot) {
        self.local.health.stage(&slot.update_id);
        self.local.staged = Some(slot);
    }

    /// Call after the package-manager command from [`Updater::install_plan`] succeeded, or
    /// right before handing an installer the app's own files (MSI: start `msiexec` detached
    /// with [`Command::spawn`] and exit). The next start runs the new build on trial; a start
    /// that is still the old version commits nothing and the update stays on offer. MSI
    /// packages are kept so a later failed update can reinstall this one.
    pub fn installed(&mut self, update_id: &str) -> Result<()> {
        let p = self.pending(update_id)?.clone();
        let full = install::full(&p.manifest).ok_or_else(|| Error::Invalid(format!("{update_id} is not a full update")))?;
        if !matches!(full.format.install_mode(), InstallMode::PlatformInstaller | InstallMode::PrivilegedPackageManager) {
            return Err(Error::Invalid(format!("{update_id} is not installed by an installer")));
        }
        let package = if full.format == PackageFormat::Msi {
            let rel = format!("versions/{update_id}/{}", install::package_file(&p.manifest)?);
            if !self.cfg.dir.join(&rel).is_file() {
                return Err(Error::Invalid(format!("{update_id}: call install_plan first")));
            }
            Some(rel)
        } else {
            let staged = self.cfg.dir.join("updates").join(update_id);
            if staged.exists() {
                fs::remove_dir_all(&staged)?;
            }
            None
        };
        self.stage(Slot { update_id: update_id.into(), run: None, package, version: Some(p.manifest.version.to_string()) });
        self.save()
    }

    /// Call first on every start (unless a launcher already did, see [`begin_boot`]).
    pub fn begin_boot(&mut self) -> Result<Boot> {
        let boot = self.local.begin_boot(&self.cfg.dir)?;
        if let (Some(id), Some(c)) = (&boot.rolled_back, self.client.as_mut()) {
            c.mark_bad(id);
        }
        self.save()?;
        Ok(boot)
    }

    /// Call after the first frame and the app's own ready signal. Commits a trial update.
    pub fn mark_ready(&mut self) -> Result<Option<String>> {
        // Another process (the launcher) may have started this boot: reload its state.
        let fresh = Local::load(&self.cfg.dir)?;
        self.local.health = fresh.health;
        self.local.staged = fresh.staged;
        self.local.good = fresh.good;
        self.local.bad = fresh.bad;
        if let weirdnet_update::health::HealthState::Trial { update_id, .. } = &self.local.health.state
            && let Some(slot) = self.local.staged.as_ref().filter(|s| &s.update_id == update_id)
            && slot.version.as_ref().is_some_and(|v| *v != self.cfg.client_version.to_string())
        {
            // The installer never replaced this build: back to the previous good state.
            self.local.health.state = match &self.local.good {
                Some(g) => weirdnet_update::health::HealthState::Committed { update_id: g.update_id.clone() },
                None => weirdnet_update::health::HealthState::Idle,
            };
            self.local.staged = None;
            self.save()?;
            return Ok(None);
        }
        let committed = self.local.health.mark_ready();
        if let Some(id) = &committed {
            self.local.commit(&self.cfg.dir, id)?;
        }
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
