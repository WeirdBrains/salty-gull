//! `saltyctl`: the publisher's side of Salty Gull (G3 MVP).
//!
//! Keys are generated and used locally; the server never sees them. Root keys sign roots,
//! delegated release keys sign manifests, and `publish` uploads blobs first and the manifest
//! last, so a device never sees a manifest whose files are missing.
#![forbid(unsafe_code)]

use semver::Version;
use std::fs;
use std::path::{Path, PathBuf};
use weirdnet_update::envelope::Domain;
use weirdnet_update::manifest::{Arch, FullPackage, PackageFormat, Platform};
use weirdnet_update::{Delegation, FileEntry, Hash32, PublicKey, Root, Signed, SigningKey, Timestamp, UpdateKind, UpdateManifest};

/// Errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Record or key failure.
    #[error("{0}")]
    Update(#[from] weirdnet_update::Error),
    /// Local files.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The server refused or could not be reached.
    #[error("server: {0}")]
    Server(String),
    /// Bad input.
    #[error("{0}")]
    Invalid(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Writes a new private key (hex seed, mode 0600) to `path`; refuses to overwrite.
pub fn new_key(path: &Path) -> Result<PublicKey> {
    let key = SigningKey::generate()?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    let text = zeroize::Zeroizing::new(format!("{}\n", hex::encode(*key.to_seed())));
    std::io::Write::write_all(&mut f, text.as_bytes())?;
    Ok(key.public())
}

/// Reads a private key file.
pub fn read_key(path: &Path) -> Result<SigningKey> {
    let text = zeroize::Zeroizing::new(fs::read_to_string(path).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?);
    let bytes =
        zeroize::Zeroizing::new(hex::decode(text.trim()).map_err(|_| Error::Invalid(format!("{}: not a saltyctl key file", path.display())))?);
    let seed: zeroize::Zeroizing<[u8; 32]> = bytes
        .as_slice()
        .try_into()
        .map(zeroize::Zeroizing::new)
        .map_err(|_| Error::Invalid(format!("{}: not a saltyctl key file", path.display())))?;
    Ok(SigningKey::from_seed(&seed))
}

/// Reads a public key in hex.
pub fn parse_public(s: &str) -> Result<PublicKey> {
    Ok(PublicKey::from_hex(s.trim())?)
}

/// Options for a root.
pub struct RootSpec {
    /// Org id on the server.
    pub org: String,
    /// Root version (1 for the first).
    pub version: u32,
    /// Root public keys (default: the signers').
    pub keys: Vec<PublicKey>,
    /// Root threshold.
    pub threshold: u8,
    /// Delegated release keys.
    pub release: Vec<PublicKey>,
    /// Release threshold.
    pub release_threshold: u8,
    /// Days until the root expires.
    pub expires_days: Option<u64>,
}

/// Builds and signs a root. For a rotation, `signers` must include a threshold of the old
/// keys and of the new ones.
pub fn make_root(spec: &RootSpec, signers: &[SigningKey], now: Timestamp) -> Result<Signed> {
    let keys = if spec.keys.is_empty() { signers.iter().map(|k| k.public()).collect() } else { spec.keys.clone() };
    let root = Root {
        version: spec.version,
        org: spec.org.clone(),
        keys,
        threshold: spec.threshold,
        release: spec.release.iter().map(|k| Delegation::any(*k)).collect(),
        release_threshold: spec.release_threshold,
        timestamp: Vec::new(),
        timestamp_threshold: 0,
        expires: spec.expires_days.map(|d| now.plus(d * 86_400)),
    };
    root.validate()?;
    let refs: Vec<&SigningKey> = signers.iter().collect();
    Ok(Signed::sign(Domain::Root, &root, &refs)?)
}

/// The pin (hex) to compile into the app for a first root.
pub fn root_pin(env: &Signed) -> Result<String> {
    let root: Root = env.decode()?;
    root.validate()?;
    Ok(root.pin().0.to_hex())
}

/// Options for a full-package manifest.
pub struct ManifestSpec {
    /// App id.
    pub app: String,
    /// Channel.
    pub channel: String,
    /// Sequence number (strictly increasing per channel).
    pub seq: u64,
    /// Version of the build.
    pub version: Version,
    /// Target OS (`linux`, `macos`, `windows`, `freebsd`, ...).
    pub platform: String,
    /// CPU (`x86-64`, `aarch64`).
    pub arch: String,
    /// Package format (`tar-gz`, `zip`, `exe`, `msi`, `app`, `deb`, `rpm`, ...).
    pub format: String,
    /// The platform signing identity the build keeps.
    pub signing_identity: String,
    /// Release notes.
    pub notes: String,
    /// Days until the manifest expires.
    pub expires_days: u64,
}

fn parse_enum<T: serde::de::DeserializeOwned>(what: &str, s: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.into())).map_err(|_| Error::Invalid(format!("unknown {what} {s:?}")))
}

/// Every file under `dir`, as (relative `/` path, absolute path), sorted.
pub fn list_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for e in fs::read_dir(dir)? {
            let e = e?;
            let p = e.path();
            let t = e.file_type()?;
            if t.is_dir() {
                walk(base, &p, out)?;
            } else if t.is_file() {
                let rel = p.strip_prefix(base).map_err(|_| Error::Invalid("path outside dir".into()))?;
                let rel = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
                out.push((rel, p));
            } else {
                return Err(Error::Invalid(format!("{}: only regular files are published", p.display())));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Builds and signs a full-package manifest over every file in `dir`.
pub fn make_manifest(spec: &ManifestSpec, dir: &Path, key: &SigningKey, now: Timestamp) -> Result<Signed> {
    let mut files = Vec::new();
    for (rel, path) in list_files(dir)? {
        files.push(FileEntry::describe(&rel, &fs::read(path)?));
    }
    let m = UpdateManifest {
        app: spec.app.clone(),
        channel: spec.channel.clone(),
        update_id: format!("{}-{}-{}", spec.app, spec.channel, spec.seq),
        seq: spec.seq,
        kind: UpdateKind::Full(FullPackage {
            platform: parse_enum::<Platform>("platform", &spec.platform)?,
            arch: parse_enum::<Arch>("arch", &spec.arch)?,
            format: parse_enum::<PackageFormat>("format", &spec.format)?,
            signing_identity: spec.signing_identity.clone(),
        }),
        version: spec.version.clone(),
        base_releases: Vec::new(),
        files,
        min_client: None,
        max_client: None,
        capabilities: Vec::new(),
        labels: Default::default(),
        targeting: None,
        rollout: None,
        rollback_of: None,
        notes: spec.notes.clone(),
        created: now,
        expires: now.plus(spec.expires_days * 86_400),
    };
    m.validate()?;
    Ok(Signed::sign(Domain::Manifest, &m, &[key])?)
}

/// Reads an envelope file.
pub fn read_envelope(path: &Path) -> Result<Signed> {
    Ok(Signed::from_bytes(&fs::read(path)?)?)
}

/// A publishing connection.
pub struct Server {
    base: String,
    token: String,
    agent: ureq::Agent,
}

fn server_err(e: ureq::Error) -> Error {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(body);
            Error::Server(format!("{code}: {msg}"))
        }
        e => Error::Server(e.to_string()),
    }
}

impl Server {
    /// `base` like `https://updates.example`; `token` is the admin token (never printed).
    pub fn new(base: &str, token: String) -> Self {
        Self { base: base.trim_end_matches('/').to_owned(), token, agent: ureq::AgentBuilder::new().build() }
    }

    fn post(&self, path: &str, body: &[u8]) -> Result<serde_json::Value> {
        let r = self
            .agent
            .post(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/cbor")
            .send_bytes(body)
            .map_err(server_err)?;
        serde_json::from_str(&r.into_string()?).map_err(|e| Error::Server(e.to_string()))
    }

    /// Publishes a root.
    pub fn publish_root(&self, env: &Signed) -> Result<serde_json::Value> {
        self.post("/v1/roots", &env.to_bytes()?)
    }

    /// Uploads each file the manifest lists (from `dir`), then the manifest.
    pub fn publish_manifest(&self, env: &Signed, dir: &Path) -> Result<serde_json::Value> {
        let m: UpdateManifest = env.decode()?;
        for f in &m.files {
            let data = fs::read(dir.join(&f.path))?;
            if Hash32::of(&data) != f.blake3 {
                return Err(Error::Invalid(format!("{}: changed since the manifest was signed", f.path)));
            }
            self.agent
                .put(&format!("{}/v1/blobs/{}", self.base, f.blake3.to_hex()))
                .set("Authorization", &format!("Bearer {}", self.token))
                .set("Content-Type", "application/octet-stream")
                .send_bytes(&data)
                .map_err(server_err)?;
        }
        self.post("/v1/manifests", &env.to_bytes()?)
    }
}
