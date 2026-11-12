//! `salty-launch`: the launcher shipped with portable builds (tar.gz, zip, AppImage, a bare
//! exe) and optionally with MSI installs.
//!
//! Layout next to the launcher binary:
//!
//! ```text
//! morph              <- this launcher, renamed
//! salty-launch.json  <- {"app_dir": "app", "entry": "bin/morph", "state_dir": "state"}
//! app/               <- the build that shipped
//! state/             <- salty-client state: versions/<id>, updates/<id>, local.json
//! ```
//!
//! Each start calls [`salty_client::begin_boot`] and runs the entry from the chosen build: the
//! update on trial, the last committed update, or `app_dir`. Two starts that never reach
//! `mark_ready` roll back. After an MSI rollback the launcher reinstalls the kept previous MSI.
//! The app gets `SALTY_GULL_LAUNCHED=1` and must not call `begin_boot` again.
#![forbid(unsafe_code)]

use salty_client::Boot;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The config file name, next to the launcher.
pub const CONFIG: &str = "salty-launch.json";

/// `salty-launch.json`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The build that shipped, relative to the launcher's directory (or absolute).
    pub app_dir: String,
    /// The program inside a build. When absent, a build with exactly one file runs that file
    /// (AppImage or exe updates).
    #[serde(default)]
    pub entry: Option<String>,
    /// salty-client state, relative to the launcher's directory or absolute; `${VAR}` expands
    /// from the environment (for example `${LOCALAPPDATA}/Morph/updates`).
    pub state_dir: String,
    /// Extra arguments for a reinstall after a rollback (for example `["/passive"]`).
    #[serde(default)]
    pub reinstall_args: Vec<String>,
}

/// What to run.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    /// The program.
    pub program: PathBuf,
    /// Environment for the app.
    pub env: Vec<(String, String)>,
}

/// What this start does.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run the app.
    Run(Launch),
    /// An update failed and the previous package's installer was started (detached, because it
    /// replaces this launcher's files too). Start again once it finishes.
    Reinstalling {
        /// The update that was rolled back.
        rolled_back: String,
    },
}

/// Launcher errors.
#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err(e: impl std::fmt::Display) -> Error {
    Error(e.to_string())
}

/// Expands `${VAR}`; an unset variable is an error.
pub fn expand(s: &str) -> Result<String, Error> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let j = tail.find('}').ok_or_else(|| err(format!("unclosed ${{ in {s}")))?;
        let name = &tail[..j];
        out.push_str(&std::env::var(name).map_err(|_| err(format!("{name} is not set (in {s})")))?);
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn resolve(base: &Path, s: &str) -> Result<PathBuf, Error> {
    let p = PathBuf::from(expand(s)?);
    Ok(if p.is_absolute() { p } else { base.join(p) })
}

/// The program in a build directory.
pub fn entry(dir: &Path, entry: Option<&str>) -> Option<PathBuf> {
    if let Some(e) = entry {
        let p = dir.join(e);
        return p.is_file().then_some(p);
    }
    let files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && !p.to_string_lossy().ends_with(salty_client::install::MINISIG))
        .collect();
    match files.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// Picks the build for this start. A trial build without its entry counts as a failed start.
pub fn prepare(base: &Path, cfg: &Config) -> Result<Action, Error> {
    let state = resolve(base, &cfg.state_dir)?;
    std::fs::create_dir_all(&state).map_err(err)?;
    let app_dir = resolve(base, &cfg.app_dir)?;
    for _ in 0..4 {
        let Boot { dir, trial, rolled_back, reinstall } = salty_client::begin_boot(&state).map_err(err)?;
        if let (Some(mut cmd), Some(id)) = (reinstall, rolled_back.clone()) {
            cmd.args.extend(cfg.reinstall_args.iter().cloned());
            cmd.spawn().map_err(|e| err(format!("reinstalling the previous version failed: {e}")))?;
            return Ok(Action::Reinstalling { rolled_back: id });
        }
        let build = dir.clone().unwrap_or_else(|| app_dir.clone());
        let Some(program) = entry(&build, cfg.entry.as_deref()) else {
            if trial.is_some() {
                continue; // counts as a failed boot; the next round rolls back
            }
            return Err(err(format!("no program to run in {}", build.display())));
        };
        let mut env = vec![
            ("SALTY_GULL_LAUNCHED".to_owned(), "1".to_owned()),
            ("SALTY_GULL_STATE_DIR".to_owned(), state.to_string_lossy().into_owned()),
            ("SALTY_GULL_BUILD_DIR".to_owned(), build.to_string_lossy().into_owned()),
        ];
        if let Some(id) = trial {
            env.push(("SALTY_GULL_TRIAL".into(), id));
        }
        if let Some(id) = rolled_back {
            env.push(("SALTY_GULL_ROLLED_BACK".into(), id));
        }
        return Ok(Action::Run(Launch { program, env }));
    }
    Err(err("the update on trial has no program to run and did not roll back"))
}

/// This program's real path (symlinks resolved), so the config is found next to the binary.
pub fn self_path() -> std::io::Result<PathBuf> {
    std::env::current_exe()?.canonicalize()
}

/// Reads the config next to `launcher`.
pub fn load(launcher: &Path) -> Result<(PathBuf, Config), Error> {
    let base = launcher.parent().ok_or_else(|| err("launcher has no directory"))?.to_owned();
    let text = std::fs::read_to_string(base.join(CONFIG)).map_err(|e| err(format!("{}: {e}", base.join(CONFIG).display())))?;
    let cfg = serde_json::from_str(&text).map_err(|e| err(format!("{CONFIG}: {e}")))?;
    Ok((base, cfg))
}

#[cfg(test)]
mod tests;
