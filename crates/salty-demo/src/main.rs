//! `salty-demo`: a stand-in app for real-machine tests (BSD VMs, Windows MSI).
//!
//! Build variants with `SALTY_DEMO_VERSION=1.1.0` (the version it reports) and
//! `SALTY_DEMO_BROKEN=1` (a build that exits before it is ready, to test rollback).
//! Config: `salty-demo.json` next to the binary, or the file in `SALTY_DEMO_CONFIG`.
//!
//! - `salty-demo` / `salty-demo run`: start (boot, unless a launcher already did), then ready.
//! - `salty-demo update [--installer-arg ARG]...`: check, download, install; installer commands
//!   run right away (the test stands in for the user's confirmation).
#![forbid(unsafe_code)]

use salty_client::{Check, Config, HttpTransport, Plan, Target, Updater};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::ExitCode;
use weirdnet_update::manifest::{Arch, PackageFormat, Platform};
use weirdnet_update::policy::{Decision, Flavor};
use weirdnet_update::target::Device;
use weirdnet_update::{ConsentMode, Policy, RootPin, Timestamp};

const VERSION: &str = match option_env!("SALTY_DEMO_VERSION") {
    Some(v) => v,
    None => "1.0.0",
};
const BROKEN: bool = option_env!("SALTY_DEMO_BROKEN").is_some();

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DemoConfig {
    server: String,
    pin: String,
    app: String,
    channel: String,
    /// `${VAR}` expands from the environment.
    state_dir: String,
    platform: Platform,
    arch: Arch,
    format: PackageFormat,
    #[serde(default)]
    signing_identity: Option<String>,
    #[serde(default)]
    minisign_key: Option<String>,
}

fn expand(s: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let j = tail.find('}').ok_or("unclosed ${")?;
        out.push_str(&std::env::var(&tail[..j]).map_err(|_| format!("{} is not set", &tail[..j]))?);
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn updater() -> Result<Updater<HttpTransport>, String> {
    let path = match std::env::var_os("SALTY_DEMO_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => salty_launch::self_path().map_err(|e| e.to_string())?.with_file_name("salty-demo.json"),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let c: DemoConfig = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    // A launcher passes the state dir it used; otherwise the config's.
    let dir = match std::env::var_os("SALTY_GULL_STATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(expand(&c.state_dir)?),
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let cfg = Config {
        dir,
        app: c.app,
        channel: c.channel,
        pin: RootPin::from_hex(&c.pin).map_err(|e| e.to_string())?,
        client_version: semver::Version::parse(VERSION).map_err(|e| e.to_string())?,
        base_release: None,
        policy: Policy { flavor: Flavor::Desktop, dart_patches: false },
        signing_identity: c.signing_identity,
        minisign_key: c.minisign_key,
        target: Some(Target { platform: c.platform, arch: c.arch, format: c.format }),
    };
    Updater::open(cfg, HttpTransport::new(&c.server)).map_err(|e| e.to_string())
}

fn run() -> Result<(), String> {
    let mut u = updater()?;
    if std::env::var_os("SALTY_GULL_LAUNCHED").is_none() {
        let b = u.begin_boot().map_err(|e| e.to_string())?;
        if let Some(id) = &b.rolled_back {
            println!("rolled back {id}");
        }
        if let Some(cmd) = b.reinstall {
            println!("reinstalling the previous version: {} {}", cmd.program, cmd.args.join(" "));
            return cmd.spawn().map_err(|e| e.to_string());
        }
    }
    if BROKEN {
        return Err(format!("salty-demo {VERSION} is a broken build"));
    }
    let committed = u.mark_ready().map_err(|e| e.to_string())?;
    println!("salty-demo {VERSION} ready{}", committed.map(|c| format!(" (committed {c})")).unwrap_or_default());
    Ok(())
}

fn update(installer_args: &[String]) -> Result<(), String> {
    let mut u = updater()?;
    let now = Timestamp::now();
    u.set_mode(ConsentMode::Auto, now).map_err(|e| e.to_string())?;
    let id = match u.check(&Device::new([7; 32], now), now).map_err(|e| e.to_string())? {
        Check::UpToDate => {
            println!("up to date ({VERSION})");
            return Ok(());
        }
        Check::NotForThisDevice(a) => return Err(format!("not for this device: {a:?}")),
        Check::Offer { manifest, decision } => {
            if let Decision::Notify { .. } = decision {
                println!("{}", u.notice(&manifest.update_id).map_err(|e| e.to_string())?.unwrap_or_default());
                return Ok(());
            }
            println!("offer {} {} ({decision:?})", manifest.update_id, manifest.version);
            manifest.update_id
        }
    };
    u.download(&id).map_err(|e| e.to_string())?;
    match u.install_plan(&id).map_err(|e| e.to_string())? {
        Plan::Swap { dir } => println!("staged {id} in {}", dir.display()),
        Plan::PlatformInstaller(mut cmd) => {
            // The installer replaces this app's files: record it, hand off and exit.
            cmd.args.extend(installer_args.iter().cloned());
            u.installed(&id).map_err(|e| e.to_string())?;
            println!("started {} {}", cmd.program, cmd.args.join(" "));
            cmd.spawn().map_err(|e| e.to_string())?;
        }
        Plan::PrivilegedPackageManager(mut cmd) => {
            cmd.args.extend(installer_args.iter().cloned());
            println!("running {} {}", cmd.program, cmd.args.join(" "));
            cmd.run().map_err(|e| e.to_string())?;
            u.installed(&id).map_err(|e| e.to_string())?;
            println!("installed {id}");
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        None | Some("run") => run(),
        Some("version") => {
            println!("{VERSION}");
            Ok(())
        }
        Some("update") => {
            let mut extra = Vec::new();
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match (a.as_str(), it.next()) {
                    ("--installer-arg", Some(v)) => extra.push(v.clone()),
                    _ => return fail(format!("unknown argument {a}")),
                }
            }
            update(&extra)
        }
        Some(other) => Err(format!("unknown command {other}")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

fn fail(e: String) -> ExitCode {
    eprintln!("salty-demo: {e}");
    ExitCode::from(3)
}
