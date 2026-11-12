use super::*;
use salty_core::Store;
use salty_core::testkit::*;
use salty_core::wire::BundleWire;
use std::sync::{Arc, Mutex};
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

struct World {
    _server: tempfile::TempDir,
    device_dir: tempfile::TempDir,
    store: Arc<Store>,
    pin: RootPin,
}

const BUILD: &[u8] = b"morph 1.1.0 build";

fn world() -> World {
    let server = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(server.path(), "weirdbrains").unwrap());
    let r = root("weirdbrains", 1, &[&key(1)], &key(2));
    store.publish_root(&signed_root(&r, &[&key(1)]), NOW).unwrap();
    store.put_blob(&Hash32::of(BUILD), BUILD).unwrap();
    let m = manifest("morph", "stable", 1, "1.1.0", &[("bin/morph", BUILD)]);
    store.publish_manifest(&signed_manifest(&m, &key(2)), NOW).unwrap();
    World { _server: server, device_dir: tempfile::tempdir().unwrap(), store, pin: r.pin() }
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
        }
    }
    fn updater(&self) -> Updater<Mem> {
        Updater::open(self.cfg(), self.mem()).unwrap()
    }
}

fn device() -> Device {
    Device::new([7; 32], NOW)
}

#[test]
fn ask_accept_download_boot_commit() {
    let w = world();
    let mut u = w.updater();
    let Check::Offer { manifest, decision } = u.check(&device(), NOW).unwrap() else { panic!("expected an offer") };
    assert_eq!(manifest.update_id, "morph-stable-1");
    assert!(matches!(decision, Decision::Ask { reason: AskReason::PerUpdate, .. }), "{decision:?}");
    assert!(u.download("morph-stable-1").is_err(), "never without consent");
    u.accept("morph-stable-1", NOW).unwrap();
    let dir = u.download("morph-stable-1").unwrap();
    assert_eq!(fs::read(dir.join("bin/morph")).unwrap(), BUILD);
    // Next start: state survives a reopen.
    let mut u = w.updater();
    assert_eq!(u.begin_boot().unwrap(), Some(dir));
    assert_eq!(u.mark_ready().unwrap().as_deref(), Some("morph-stable-1"));
    assert_eq!(u.check(&device(), NOW).unwrap(), Check::UpToDate);
}

#[test]
fn auto_mode_installs_and_off_checks_nothing() {
    let w = world();
    let mut u = w.updater();
    u.set_mode(ConsentMode::Auto, NOW).unwrap();
    let Check::Offer { decision, .. } = u.check(&device(), NOW).unwrap() else { panic!() };
    assert_eq!(decision, Decision::Install);
    u.download("morph-stable-1").unwrap();
    u.set_mode(ConsentMode::Off, NOW).unwrap();
    assert_eq!(u.check(&device(), NOW).unwrap(), Check::UpToDate);
}

#[test]
fn declined_update_is_not_offered_again() {
    let w = world();
    let mut u = w.updater();
    u.check(&device(), NOW).unwrap();
    u.decline("morph-stable-1", NOW).unwrap();
    let Check::Offer { decision, .. } = u.check(&device(), NOW).unwrap() else { panic!() };
    assert_eq!(decision, Decision::Skip(SkipReason::Declined));
}

#[test]
fn tampered_blob_is_not_staged() {
    let w = world();
    let mut u = Updater::open(w.cfg(), Mem { tamper: true, ..w.mem() }).unwrap();
    u.set_mode(ConsentMode::Auto, NOW).unwrap();
    u.check(&device(), NOW).unwrap();
    assert!(u.download("morph-stable-1").is_err());
    assert!(!w.device_dir.path().join("updates/morph-stable-1").exists());
    assert_eq!(u.begin_boot().unwrap(), None);
}

#[test]
fn two_failed_boots_roll_back_and_the_update_is_refused() {
    let w = world();
    let mut u = w.updater();
    u.set_mode(ConsentMode::Auto, NOW).unwrap();
    u.check(&device(), NOW).unwrap();
    u.download("morph-stable-1").unwrap();
    assert!(u.begin_boot().unwrap().is_some());
    assert!(u.begin_boot().unwrap().is_some(), "second trial");
    assert_eq!(u.begin_boot().unwrap(), None, "rolled back");
    assert!(!w.device_dir.path().join("updates/morph-stable-1").exists());
    assert!(matches!(u.check(&device(), NOW), Err(Error::Update(weirdnet_update::Error::KnownBad(_)))));
}

#[test]
fn wrong_pin_and_replayed_manifest_are_refused() {
    let w = world();
    let mut cfg = w.cfg();
    cfg.pin = RootPin::for_keys(&[key(9).public()], 1);
    assert!(Updater::open(cfg, w.mem()).unwrap().check(&device(), NOW).is_err());

    let mem = w.mem();
    let older = w.store.latest("morph", "stable").unwrap();
    let m2 = manifest("morph", "stable", 2, "1.2.0", &[("bin/morph", BUILD)]);
    w.store.publish_manifest(&signed_manifest(&m2, &key(2)), NOW).unwrap();
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
