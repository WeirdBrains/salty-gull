//! `salty-launch` binary: see the library docs.
#![forbid(unsafe_code)]

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("salty-launch: {e}");
            ExitCode::from(70)
        }
    }
}

fn run() -> Result<ExitCode, salty_launch::Error> {
    let me = salty_launch::self_path().map_err(|e| salty_launch::Error(e.to_string()))?;
    let (base, cfg) = salty_launch::load(&me)?;
    let launch = match salty_launch::prepare(&base, &cfg)? {
        salty_launch::Action::Run(l) => l,
        salty_launch::Action::Reinstalling { rolled_back } => {
            eprintln!("salty-launch: update {rolled_back} failed to start twice; reinstalling the previous version. Start again when it finishes.");
            return Ok(ExitCode::SUCCESS);
        }
    };
    let mut cmd = Command::new(&launch.program);
    cmd.args(std::env::args_os().skip(1)).envs(launch.env);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        Err(salty_launch::Error(format!("{}: {e}", launch.program.display())))
    }
    #[cfg(not(unix))]
    {
        let st = cmd.status().map_err(|e| salty_launch::Error(format!("{}: {e}", launch.program.display())))?;
        Ok(ExitCode::from(st.code().unwrap_or(1).clamp(0, 255) as u8))
    }
}
