//! Publisher → server → device over real HTTP on 127.0.0.1, with temp dirs only.

use salty_client::{Check, Config, HttpTransport, Updater};
use std::sync::Arc;
use weirdnet_update::policy::{Decision, Flavor};
use weirdnet_update::target::Device;
use weirdnet_update::{Policy, RootPin, Timestamp};

fn serve(home: &std::path::Path) -> String {
    let store = salty_core::Store::open(home, "weirdbrains").unwrap();
    let app = Arc::new(salty_server::App { store, admin_token_hash: salty_server::read_admin_token_hash(home).unwrap() });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, salty_server::router(app)).await.unwrap();
        });
    });
    format!("http://{addr}")
}

#[test]
fn publish_then_device_updates() {
    let keys = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let build = tempfile::tempdir().unwrap();
    let device_dir = tempfile::tempdir().unwrap();
    let now = Timestamp::now();

    let root_key = keys.path().join("root.key");
    let release_key = keys.path().join("release.key");
    saltyctl::new_key(&root_key).unwrap();
    let release_pub = saltyctl::new_key(&release_key).unwrap();
    assert!(saltyctl::new_key(&root_key).is_err(), "never overwrites a key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&root_key).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let spec = saltyctl::RootSpec {
        org: "weirdbrains".into(),
        version: 1,
        keys: Vec::new(),
        threshold: 1,
        release: vec![release_pub],
        release_threshold: 1,
        expires_days: None,
    };
    let root = saltyctl::make_root(&spec, &[saltyctl::read_key(&root_key).unwrap()], now).unwrap();
    let pin = RootPin::from_hex(&saltyctl::root_pin(&root).unwrap()).unwrap();

    std::fs::write(build.path().join("morph"), b"morph 1.1.0").unwrap();
    let mspec = saltyctl::ManifestSpec {
        app: "morph".into(),
        channel: "stable".into(),
        seq: 1,
        version: semver::Version::new(1, 1, 0),
        platform: "linux".into(),
        arch: "x86-64".into(),
        format: "exe".into(),
        signing_identity: "minisign-RWQ-test".into(),
        notes: "Faster startup.".into(),
        expires_days: 30,
    };
    let manifest = saltyctl::make_manifest(&mspec, build.path(), &saltyctl::read_key(&release_key).unwrap(), now).unwrap();

    let token = salty_server::issue_admin_token(home.path()).unwrap();
    let url = serve(home.path());

    let wrong = saltyctl::Server::new(&url, "sg_admin_wrong".into());
    let e = wrong.publish_root(&root).unwrap_err().to_string();
    assert!(e.contains("401"), "{e}");

    let srv = saltyctl::Server::new(&url, token);
    assert!(srv.publish_manifest(&manifest, build.path()).unwrap_err().to_string().contains("publish a root first"));
    assert_eq!(srv.publish_root(&root).unwrap()["version"], 1);
    let p = srv.publish_manifest(&manifest, build.path()).unwrap();
    assert_eq!(p["update_id"], "morph-stable-1");

    let cfg = Config {
        dir: device_dir.path().to_owned(),
        app: "morph".into(),
        channel: "stable".into(),
        pin,
        client_version: semver::Version::new(1, 0, 0),
        base_release: None,
        policy: Policy { flavor: Flavor::Desktop, dart_patches: false },
        signing_identity: Some("minisign-RWQ-test".into()),
        minisign_key: None,
        target: None,
    };
    let mut u = Updater::open(cfg, HttpTransport::new(&url)).unwrap();
    let device = Device::new([3; 32], now);
    let Check::Offer { manifest, decision } = u.check(&device, now).unwrap() else { panic!("no offer") };
    assert_eq!(manifest.notes, "Faster startup.");
    assert!(matches!(decision, Decision::Ask { .. }));
    u.accept("morph-stable-1", now).unwrap();
    u.download("morph-stable-1").unwrap();
    let salty_client::Plan::Swap { dir } = u.install_plan("morph-stable-1").unwrap() else { panic!("exe swaps") };
    assert_eq!(std::fs::read(dir.join("morph")).unwrap(), b"morph 1.1.0");
    assert_eq!(u.begin_boot().unwrap().dir, Some(dir));
    assert_eq!(u.mark_ready().unwrap().as_deref(), Some("morph-stable-1"));

    // MCP over HTTP through the saltyctl forwarder.
    let agent = ureq::AgentBuilder::new().build();
    let list = saltyctl::mcp_forward(&agent, &url, None, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap().unwrap();
    let names: Vec<String> = serde_json::from_str::<serde_json::Value>(&list).unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["list_channels", "channel_history", "root_info"]);
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"channel_history","arguments":{"app":"morph","channel":"stable"}}}"#;
    let v: serde_json::Value = serde_json::from_str(&saltyctl::mcp_forward(&agent, &url, None, call).unwrap().unwrap()).unwrap();
    assert_eq!(v["result"]["structuredContent"]["updates"][0]["update_id"], "morph-stable-1");
    assert_eq!(v["result"]["isError"], false);
    let missing = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"channel_history","arguments":{}}}"#;
    let v: serde_json::Value = serde_json::from_str(&saltyctl::mcp_forward(&agent, &url, None, missing).unwrap().unwrap()).unwrap();
    assert_eq!(v["result"]["isError"], true);
    assert!(saltyctl::mcp_forward(&agent, &url, None, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap().is_none());
}
