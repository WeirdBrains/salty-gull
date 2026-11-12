use super::*;
use salty_client::{Check, Config as ClientConfig, Plan, Transport, Updater};
use salty_core::Store;
use salty_core::testkit::*;
use salty_core::wire::BundleWire;
use std::io::{Read, Write};
use std::sync::Arc;
use weirdnet_update::manifest::{Arch, FullPackage, PackageFormat, Platform};
use weirdnet_update::policy::Flavor;
use weirdnet_update::target::Device;
use weirdnet_update::{ConsentMode, Hash32, Policy, UpdateKind};

#[derive(Clone)]
struct Mem(Arc<Store>);

impl Transport for Mem {
    fn bundle(&self, app: &str, channel: &str) -> salty_client::Result<BundleWire> {
        Ok(BundleWire::from_bundle(&self.0.bundle(app, channel)?)?)
    }
    fn blob(&self, hash: &Hash32) -> salty_client::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::fs::File::open(self.0.blob_path(hash).expect("blob"))?))
    }
}

fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o755);
        h.set_cksum();
        b.append_data(&mut h, path, *data).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&b.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

/// A portable install (launcher dir with `app/bin/app`) and a server offering one update.
struct World {
    _server: tempfile::TempDir,
    install: tempfile::TempDir,
    store: Arc<Store>,
    pin: weirdnet_update::RootPin,
}

const ID: &str = "morph-stable-1";

fn world(format: PackageFormat, pkg_name: &str, pkg: &[u8]) -> World {
    let server = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(server.path(), "weirdbrains").unwrap());
    let r = root("weirdbrains", 1, &[&key(1)], &key(2));
    store.publish_root(&signed_root(&r, &[&key(1)]), NOW).unwrap();
    store.put_blob(&Hash32::of(pkg), pkg).unwrap();
    let mut m = manifest("morph", "stable", 1, "1.1.0", &[(pkg_name, pkg)]);
    m.kind = UpdateKind::Full(FullPackage { platform: Platform::Linux, arch: Arch::X86_64, format, signing_identity: "id".into() });
    store.publish_manifest(&signed_manifest(&m, &key(2)), NOW).unwrap();
    let install = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(install.path().join("app/bin")).unwrap();
    std::fs::write(install.path().join("app/bin/app"), b"1.0.0").unwrap();
    World { _server: server, install, store, pin: r.pin() }
}

impl World {
    fn cfg(&self, entry: Option<&str>) -> Config {
        Config { app_dir: "app".into(), entry: entry.map(Into::into), state_dir: "state".into(), reinstall_args: vec![] }
    }
    fn updater(&self) -> Updater<Mem> {
        let cfg = ClientConfig {
            dir: self.install.path().join("state"),
            app: "morph".into(),
            channel: "stable".into(),
            pin: self.pin,
            client_version: semver::Version::new(1, 0, 0),
            base_release: None,
            policy: Policy { flavor: Flavor::Desktop, dart_patches: false },
            signing_identity: None,
            minisign_key: None,
            target: None,
        };
        Updater::open(cfg, Mem(self.store.clone())).unwrap()
    }
    /// Checks, downloads and unpacks the update, as the running app would.
    fn stage(&self) -> PathBuf {
        let mut u = self.updater();
        u.set_mode(ConsentMode::Auto, NOW).unwrap();
        assert!(matches!(u.check(&Device::new([1; 32], NOW), NOW).unwrap(), Check::Offer { .. }));
        u.download(ID).unwrap();
        let Plan::Swap { dir } = u.install_plan(ID).unwrap() else { panic!("swap") };
        dir
    }
    fn env<'a>(l: &'a Launch, k: &str) -> Option<&'a str> {
        l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
}

fn run(base: &Path, cfg: &Config) -> Launch {
    match prepare(base, cfg).unwrap() {
        Action::Run(l) => l,
        other => panic!("{other:?}"),
    }
}

#[test]
fn runs_the_shipped_build_then_the_trial_then_the_committed_update() {
    let w = world(PackageFormat::TarGz, "morph.tar.gz", &tarball(&[("bin/app", b"1.1.0")]));
    let base = w.install.path();
    let cfg = w.cfg(Some("bin/app"));
    let l = run(base, &cfg);
    assert_eq!(l.program, base.join("app/bin/app"));
    assert_eq!(World::env(&l, "SALTY_GULL_LAUNCHED"), Some("1"));
    assert_eq!(World::env(&l, "SALTY_GULL_TRIAL"), None);

    let dir = w.stage();
    let l = run(base, &cfg);
    assert_eq!(l.program, dir.join("bin/app"));
    assert_eq!(World::env(&l, "SALTY_GULL_TRIAL"), Some(ID));
    // The new build reaches ready in its own process.
    assert_eq!(w.updater().mark_ready().unwrap().as_deref(), Some(ID));
    let l = run(base, &cfg);
    assert_eq!((&l.program, World::env(&l, "SALTY_GULL_TRIAL")), (&dir.join("bin/app"), None));
}

#[test]
fn two_starts_without_ready_roll_back_to_the_shipped_build() {
    let w = world(PackageFormat::TarGz, "morph.tar.gz", &tarball(&[("bin/app", b"1.1.0")]));
    let base = w.install.path();
    let cfg = w.cfg(Some("bin/app"));
    let dir = w.stage();
    for _ in 0..2 {
        assert_eq!(run(base, &cfg).program, dir.join("bin/app"));
    }
    let l = run(base, &cfg);
    assert_eq!(l.program, base.join("app/bin/app"));
    assert_eq!(World::env(&l, "SALTY_GULL_ROLLED_BACK"), Some(ID));
    assert!(!dir.exists());
}

#[test]
fn a_trial_build_without_its_program_rolls_back_at_once() {
    let w = world(PackageFormat::TarGz, "morph.tar.gz", &tarball(&[("bin/other", b"1.1.0")]));
    let base = w.install.path();
    w.stage();
    let l = run(base, &w.cfg(Some("bin/app")));
    assert_eq!(l.program, base.join("app/bin/app"));
    assert_eq!(World::env(&l, "SALTY_GULL_ROLLED_BACK"), Some(ID));
}

#[test]
fn single_file_builds_run_that_file() {
    let w = world(PackageFormat::Exe, "Morph-1.1.0.AppImage", b"appimage");
    std::fs::remove_dir_all(w.install.path().join("app/bin")).unwrap();
    std::fs::write(w.install.path().join("app/Morph-1.0.0.AppImage"), b"1.0.0").unwrap();
    let cfg = w.cfg(None);
    assert_eq!(run(w.install.path(), &cfg).program, w.install.path().join("app/Morph-1.0.0.AppImage"));
    let dir = w.stage();
    assert_eq!(run(w.install.path(), &cfg).program, dir.join("Morph-1.1.0.AppImage"));
}

#[test]
fn config_paths_expand_variables() {
    // An always-set variable, so the test never changes the environment.
    let path = std::env::var("PATH").unwrap();
    assert_eq!(expand("${PATH}/x").unwrap(), format!("{path}/x"));
    assert!(expand("${SALTY_GULL_SURELY_UNSET_VAR}").is_err());
    assert!(expand("${OPEN").is_err());
    let cfg: Config = serde_json::from_str(r#"{"app_dir":"app","state_dir":"state"}"#).unwrap();
    assert!(cfg.entry.is_none());
    assert!(serde_json::from_str::<Config>(r#"{"app_dir":"a","state_dir":"s","oops":1}"#).is_err());
}
