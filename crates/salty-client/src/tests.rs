use super::*;
use salty_core::Store;
use salty_core::testkit::*;
use salty_core::wire::BundleWire;
use std::sync::{Arc, Mutex};
use weirdnet_update::manifest::{FullPackage, PackageFormat, Platform};
use weirdnet_update::policy::{AskReason, Flavor, SkipReason};
use weirdnet_update::{ConsentMode, Signed};

/// Serves straight from a store; `override_manifest` replaces the latest one (replay tests),
/// `tamper` flips a byte of every blob.
#[derive(Clone)]
struct Mem {
    store: Arc<Store>,
    override_manifest: Arc<Mutex<Option<Signed>>>,
    tamper: bool,
}

impl Transport for Mem {
    fn bundle(&self, app: &str, channel: &str) -> Result<BundleWire> {
        let mut b = self.store.bundle(app, channel)?;
        if let Some(m) = self.override_manifest.lock().unwrap().clone() {
            b.manifest = Some(m);
        }
        Ok(BundleWire::from_bundle(&b)?)
    }
    fn blob(&self, hash: &Hash32) -> Result<Box<dyn Read + Send>> {
        let mut data = fs::read(self.store.blob_path(hash).ok_or_else(|| Error::Transport("404".into()))?)?;
        if self.tamper {
            data[0] ^= 1;
        }
        Ok(Box::new(std::io::Cursor::new(data)))
    }
}

/// A tar.gz with `bin/morph` (executable) and `README`.
fn tarball(extra: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, data, mode) in [("bin/morph", &b"morph 1.1.0"[..], 0o755), ("README", &b"hello"[..], 0o644)] {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        b.append_data(&mut h, path, data).unwrap();
    }
    extra(&mut b);
    let tar = b.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn minisign_pair() -> minisign::KeyPair {
    minisign::KeyPair::generate_unencrypted_keypair().unwrap()
}

fn minisig(kp: &minisign::KeyPair, data: &[u8]) -> Vec<u8> {
    minisign::sign(Some(&kp.pk), &kp.sk, std::io::Cursor::new(data), None, None).unwrap().to_string().into_bytes()
}

struct World {
    _server: tempfile::TempDir,
    device_dir: tempfile::TempDir,
    store: Arc<Store>,
    pin: RootPin,
    minisign_pk: String,
}

const IDENTITY: &str = "minisign-test";

fn publish(store: &Store, seq: u64, version: &str, format: PackageFormat, files: &[(&str, &[u8])]) {
    for (_, d) in files {
        store.put_blob(&Hash32::of(d), d).unwrap();
    }
    let mut m = manifest("morph", "stable", seq, version, files);
    m.kind = UpdateKind::Full(FullPackage {
        platform: Platform::Linux,
        arch: weirdnet_update::manifest::Arch::X86_64,
        format,
        signing_identity: IDENTITY.into(),
    });
    store.publish_manifest(&signed_manifest(&m, &key(2)), NOW).unwrap();
}

fn world_with(format: PackageFormat, pkg: &[u8], sig: Option<&[u8]>) -> World {
    let server = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(server.path(), "weirdbrains").unwrap());
    let r = root("weirdbrains", 1, &[&key(1)], &key(2));
    store.publish_root(&signed_root(&r, &[&key(1)]), NOW).unwrap();
    let kp = minisign_pair();
    let own_sig = minisig(&kp, pkg);
    let sig = sig.unwrap_or(&own_sig);
    publish(&store, 1, "1.1.0", format, &[("morph.pkg", pkg), ("morph.pkg.minisig", sig)]);
    World { _server: server, device_dir: tempfile::tempdir().unwrap(), store, pin: r.pin(), minisign_pk: kp.pk.to_base64() }
}

fn world() -> World {
    world_with(PackageFormat::TarGz, &tarball(|_| {}), None)
}

impl World {
    fn mem(&self) -> Mem {
        Mem { store: self.store.clone(), override_manifest: Arc::default(), tamper: false }
    }
    fn cfg(&self) -> Config {
        Config {
            dir: self.device_dir.path().to_owned(),
            app: "morph".into(),
            channel: "stable".into(),
            pin: self.pin,
            client_version: Version::new(1, 0, 0),
            base_release: None,
            policy: Policy { flavor: Flavor::Desktop, dart_patches: false },
            signing_identity: Some(IDENTITY.into()),
            minisign_key: Some(self.minisign_pk.clone()),
        }
    }
    fn updater(&self) -> Updater<Mem> {
        Updater::open(self.cfg(), self.mem()).unwrap()
    }
    fn auto(&self) -> Updater<Mem> {
        let mut u = self.updater();
        u.set_mode(ConsentMode::Auto, NOW).unwrap();
        u
    }
}

fn device() -> Device {
    Device::new([7; 32], NOW)
}

const ID: &str = "morph-stable-1";

#[test]
fn ask_accept_download_swap_boot_commit() {
    let w = world();
    let mut u = w.updater();
    let Check::Offer { manifest, decision } = u.check(&device(), NOW).unwrap() else { panic!("expected an offer") };
    assert_eq!(manifest.update_id, ID);
    assert!(matches!(decision, Decision::Ask { reason: AskReason::PerUpdate, .. }), "{decision:?}");
    assert!(u.download(ID).is_err(), "never without consent");
    u.accept(ID, NOW).unwrap();
    u.download(ID).unwrap();
    let Plan::Swap { dir } = u.install_plan(ID).unwrap() else { panic!("tar.gz swaps") };
    assert_eq!(fs::read(dir.join("bin/morph")).unwrap(), b"morph 1.1.0");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(fs::metadata(dir.join("bin/morph")).unwrap().permissions().mode() & 0o111, 0);
    }
    // Next start: state survives a reopen.
    let mut u = w.updater();
    assert_eq!(u.begin_boot().unwrap(), Some(dir));
    assert_eq!(u.mark_ready().unwrap().as_deref(), Some(ID));
    assert_eq!(u.check(&device(), NOW).unwrap(), Check::UpToDate);
}

#[test]
fn auto_mode_installs_and_off_checks_nothing() {
    let w = world();
    let mut u = w.auto();
    let Check::Offer { decision, .. } = u.check(&device(), NOW).unwrap() else { panic!() };
    assert_eq!(decision, Decision::Install);
    u.download(ID).unwrap();
    u.set_mode(ConsentMode::Off, NOW).unwrap();
    assert_eq!(u.check(&device(), NOW).unwrap(), Check::UpToDate);
}

#[test]
fn declined_update_is_not_offered_again() {
    let w = world();
    let mut u = w.updater();
    u.check(&device(), NOW).unwrap();
    u.decline(ID, NOW).unwrap();
    let Check::Offer { decision, .. } = u.check(&device(), NOW).unwrap() else { panic!() };
    assert_eq!(decision, Decision::Skip(SkipReason::Declined));
}

#[test]
fn tampered_blob_is_not_staged() {
    let w = world();
    let mut u = Updater::open(w.cfg(), Mem { tamper: true, ..w.mem() }).unwrap();
    u.set_mode(ConsentMode::Auto, NOW).unwrap();
    u.check(&device(), NOW).unwrap();
    assert!(u.download(ID).is_err());
    assert!(!w.device_dir.path().join("updates").join(ID).exists());
    assert_eq!(u.begin_boot().unwrap(), None);
}

#[test]
fn minisign_signature_from_another_key_is_refused() {
    let pkg = tarball(|_| {});
    let other = minisig(&minisign_pair(), &pkg);
    let w = world_with(PackageFormat::TarGz, &pkg, Some(&other));
    let mut u = w.auto();
    u.check(&device(), NOW).unwrap();
    let e = u.download(ID).unwrap_err().to_string();
    assert!(e.contains("minisign"), "{e}");
    assert!(!w.device_dir.path().join("updates").join(ID).exists());
}

#[test]
fn signing_identity_must_not_change() {
    let w = world();
    let mut cfg = w.cfg();
    cfg.signing_identity = Some("someone-else".into());
    let e = Updater::open(cfg, w.mem()).unwrap().check(&device(), NOW).unwrap_err().to_string();
    assert!(e.contains("signing identity changed"), "{e}");
}

#[test]
fn two_failed_boots_roll_back_and_the_update_is_refused() {
    let w = world();
    let mut u = w.auto();
    u.check(&device(), NOW).unwrap();
    u.download(ID).unwrap();
    let Plan::Swap { dir } = u.install_plan(ID).unwrap() else { panic!() };
    assert_eq!(u.begin_boot().unwrap(), Some(dir.clone()));
    assert!(u.begin_boot().unwrap().is_some(), "second trial");
    assert_eq!(u.begin_boot().unwrap(), None, "rolled back");
    assert!(!dir.exists());
    assert!(!w.device_dir.path().join("updates").join(ID).exists());
    assert!(matches!(u.check(&device(), NOW), Err(Error::Update(weirdnet_update::Error::KnownBad(_)))));
}

#[test]
fn notify_only_formats_never_download() {
    let w = world_with(PackageFormat::PkgTarZst, b"pkg", None);
    let mut u = w.auto();
    let Check::Offer { decision, .. } = u.check(&device(), NOW).unwrap() else { panic!() };
    assert_eq!(decision, Decision::Notify { security: false });
    assert!(u.download(ID).is_err());
    assert_eq!(u.notice(ID).unwrap().unwrap(), "morph 1.1.0 is available: update morph with your AUR helper (for example `yay -Syu morph`).");
}

#[test]
fn deb_and_msi_go_through_the_system_prompt_or_installer() {
    let w = world_with(PackageFormat::Deb, b"deb", None);
    let mut u = w.auto();
    u.check(&device(), NOW).unwrap();
    u.download(ID).unwrap();
    let Plan::PrivilegedPackageManager(cmd) = u.install_plan(ID).unwrap() else { panic!() };
    assert_eq!(cmd.program, "pkexec");
    assert_eq!(&cmd.args[..3], ["apt-get", "install", "--yes"]);
    assert!(cmd.args[3].ends_with("updates/morph-stable-1/morph.pkg"));
    assert_eq!(u.begin_boot().unwrap(), None, "the package manager replaces the app; nothing to trial-boot here");

    let w = world_with(PackageFormat::Msi, b"msi", None);
    let mut u = w.auto();
    u.check(&device(), NOW).unwrap();
    u.download(ID).unwrap();
    let Plan::PlatformInstaller(cmd) = u.install_plan(ID).unwrap() else { panic!() };
    assert_eq!(cmd.program, "msiexec");
    assert!(cmd.args.contains(&"MSIINSTALLPERUSER=1".to_owned()));
}

#[test]
fn archives_cannot_escape_or_carry_links() {
    let evil = tarball(|b| {
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.as_gnu_mut().unwrap().name[..9].copy_from_slice(b"../escape");
        h.set_cksum();
        b.append(&h, &b"x"[..]).unwrap();
    });
    let w = world_with(PackageFormat::TarGz, &evil, None);
    let mut u = w.auto();
    u.check(&device(), NOW).unwrap();
    u.download(ID).unwrap();
    assert!(u.install_plan(ID).is_err());
    assert!(!w.device_dir.path().join("escape").exists());
    assert!(!w.device_dir.path().join("versions").join(ID).exists());

    let link = tarball(|b| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_cksum();
        b.append_link(&mut h, "bin/evil", "/etc/passwd").unwrap();
    });
    let w = world_with(PackageFormat::TarGz, &link, None);
    let mut u = w.auto();
    u.check(&device(), NOW).unwrap();
    u.download(ID).unwrap();
    assert!(u.install_plan(ID).unwrap_err().to_string().contains("not allowed"));
}

#[test]
fn wrong_pin_and_replayed_manifest_are_refused() {
    let w = world();
    let mut cfg = w.cfg();
    cfg.pin = RootPin::for_keys(&[key(9).public()], 1);
    assert!(Updater::open(cfg, w.mem()).unwrap().check(&device(), NOW).is_err());

    let mem = w.mem();
    let older = w.store.latest("morph", "stable").unwrap();
    let pkg = tarball(|_| {});
    let kp = minisign_pair();
    publish(&w.store, 2, "1.2.0", PackageFormat::TarGz, &[("morph.pkg", &pkg), ("morph.pkg.minisig", &minisig(&kp, &pkg))]);
    let mut u = Updater::open(w.cfg(), mem.clone()).unwrap();
    assert!(matches!(u.check(&device(), NOW).unwrap(), Check::Offer { .. }));
    // A mirror replays seq 1 after the device saw seq 2, even across a restart.
    *mem.override_manifest.lock().unwrap() = older;
    let mut u = Updater::open(w.cfg(), mem).unwrap();
    assert!(matches!(u.check(&device(), NOW), Err(Error::Update(weirdnet_update::Error::Replay { .. }))));
}

#[test]
fn manifest_paths_cannot_escape_the_update_dir() {
    assert!(safe_join(Path::new("/x"), "../etc/passwd").is_err());
    assert!(safe_join(Path::new("/x"), "/etc/passwd").is_err());
    assert!(safe_join(Path::new("/x"), "bin/morph").is_ok());
}
