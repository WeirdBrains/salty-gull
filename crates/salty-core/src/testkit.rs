//! Test fixtures: deterministic keys, a 1-of-1 root and full-package manifests.

use semver::Version;
use weirdnet_update::envelope::Domain;
use weirdnet_update::manifest::{Arch, FullPackage, PackageFormat, Platform};
use weirdnet_update::{Delegation, FileEntry, Root, Signed, SigningKey, Timestamp, UpdateKind, UpdateManifest};

/// Fixed test time.
pub const NOW: Timestamp = Timestamp(1_800_000_000);

/// A deterministic key.
pub fn key(n: u8) -> SigningKey {
    SigningKey::from_seed(&[n; 32])
}

/// A root for `org` with root key `key(1)` and release key `key(2)`.
pub fn root(org: &str, version: u32, root_keys: &[&SigningKey], release: &SigningKey) -> Root {
    Root {
        version,
        org: org.into(),
        keys: root_keys.iter().map(|k| k.public()).collect(),
        threshold: 1,
        release: vec![Delegation::any(release.public())],
        release_threshold: 1,
        timestamp: Vec::new(),
        timestamp_threshold: 0,
        expires: None,
    }
}

/// `root` signed by `signers`.
pub fn signed_root(root: &Root, signers: &[&SigningKey]) -> Signed {
    Signed::sign(Domain::Root, root, signers).expect("sign root")
}

/// A full tar.gz update for Linux x86_64 carrying `files`.
pub fn manifest(app: &str, channel: &str, seq: u64, version: &str, files: &[(&str, &[u8])]) -> UpdateManifest {
    UpdateManifest {
        app: app.into(),
        channel: channel.into(),
        update_id: format!("{app}-{channel}-{seq}"),
        seq,
        kind: UpdateKind::Full(FullPackage {
            platform: Platform::Linux,
            arch: Arch::X86_64,
            format: PackageFormat::TarGz,
            signing_identity: "minisign-test".into(),
        }),
        version: Version::parse(version).expect("version"),
        base_releases: Vec::new(),
        files: files.iter().map(|(p, d)| FileEntry::describe(p, d)).collect(),
        min_client: None,
        max_client: None,
        capabilities: Vec::new(),
        labels: Default::default(),
        targeting: None,
        rollout: None,
        rollback_of: None,
        notes: String::new(),
        created: NOW.minus(60),
        expires: NOW.plus(30 * 24 * 3600),
    }
}

/// `m` signed by `release`.
pub fn signed_manifest(m: &UpdateManifest, release: &SigningKey) -> Signed {
    Signed::sign(Domain::Manifest, m, &[release]).expect("sign manifest")
}
