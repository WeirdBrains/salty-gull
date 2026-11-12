use clap::{Parser, Subcommand};
use std::path::PathBuf;
use weirdnet_update::Timestamp;

/// Salty Gull publisher CLI.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a key file (private; mode 0600). Prints the public key.
    KeyNew { out: PathBuf },
    /// Print a key file's public key.
    KeyPublic { key: PathBuf },
    /// Build and sign an org root.
    RootNew {
        #[arg(long)]
        org: String,
        #[arg(long, default_value_t = 1)]
        version: u32,
        /// Root key files that sign (for a rotation: old and new keys).
        #[arg(long = "sign", required = true)]
        sign: Vec<PathBuf>,
        /// Root public keys (hex); default: the signers' keys.
        #[arg(long = "root-key")]
        root_keys: Vec<String>,
        #[arg(long, default_value_t = 1)]
        threshold: u8,
        /// Delegated release public keys (hex).
        #[arg(long = "release-key", required = true)]
        release: Vec<String>,
        #[arg(long, default_value_t = 1)]
        release_threshold: u8,
        #[arg(long)]
        expires_days: Option<u64>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Print the root pin to compile into the app.
    RootPin { root: PathBuf },
    /// Build and sign a full-package manifest over every file in a directory.
    Manifest {
        #[arg(long)]
        app: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        seq: u64,
        #[arg(long)]
        version: semver::Version,
        #[arg(long)]
        platform: String,
        #[arg(long)]
        arch: String,
        #[arg(long)]
        format: String,
        #[arg(long)]
        signing_identity: String,
        #[arg(long, default_value = "")]
        notes: String,
        #[arg(long, default_value_t = 30)]
        expires_days: u64,
        /// Release key file.
        #[arg(long)]
        key: PathBuf,
        /// Directory with the files to ship.
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Publish a root to the server.
    PublishRoot {
        root: PathBuf,
        #[arg(long, env = "SALTY_SERVER")]
        server: String,
    },
    /// MCP over stdio, forwarded to the server's `/mcp` (uses SALTY_TOKEN when set).
    Mcp {
        #[arg(long, env = "SALTY_SERVER")]
        server: String,
    },
    /// Upload a manifest's files, then the manifest.
    Publish {
        manifest: PathBuf,
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, env = "SALTY_SERVER")]
        server: String,
    },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("saltyctl: {e}");
        std::process::exit(1);
    }
}

fn token() -> saltyctl::Result<String> {
    std::env::var("SALTY_TOKEN").map_err(|_| saltyctl::Error::Invalid("set SALTY_TOKEN to the server's admin token".into()))
}

fn run(cli: Cli) -> saltyctl::Result<()> {
    let now = Timestamp::now();
    match cli.cmd {
        Cmd::KeyNew { out } => println!("{}", saltyctl::new_key(&out)?.to_hex()),
        Cmd::KeyPublic { key } => println!("{}", saltyctl::read_key(&key)?.public().to_hex()),
        Cmd::RootNew { org, version, sign, root_keys, threshold, release, release_threshold, expires_days, out } => {
            let signers = sign.iter().map(|p| saltyctl::read_key(p)).collect::<saltyctl::Result<Vec<_>>>()?;
            let spec = saltyctl::RootSpec {
                org,
                version,
                keys: root_keys.iter().map(|k| saltyctl::parse_public(k)).collect::<saltyctl::Result<_>>()?,
                threshold,
                release: release.iter().map(|k| saltyctl::parse_public(k)).collect::<saltyctl::Result<_>>()?,
                release_threshold,
                expires_days,
            };
            let env = saltyctl::make_root(&spec, &signers, now)?;
            std::fs::write(&out, env.to_bytes()?)?;
            println!("{}", saltyctl::root_pin(&env)?);
        }
        Cmd::RootPin { root } => println!("{}", saltyctl::root_pin(&saltyctl::read_envelope(&root)?)?),
        Cmd::Manifest { app, channel, seq, version, platform, arch, format, signing_identity, notes, expires_days, key, dir, out } => {
            let spec = saltyctl::ManifestSpec { app, channel, seq, version, platform, arch, format, signing_identity, notes, expires_days };
            let env = saltyctl::make_manifest(&spec, &dir, &saltyctl::read_key(&key)?, now)?;
            std::fs::write(&out, env.to_bytes()?)?;
        }
        Cmd::PublishRoot { root, server } => {
            let r = saltyctl::Server::new(&server, token()?).publish_root(&saltyctl::read_envelope(&root)?)?;
            println!("{r}");
        }
        Cmd::Mcp { server } => saltyctl::mcp_stdio(&server, std::env::var("SALTY_TOKEN").ok().as_deref())?,
        Cmd::Publish { manifest, dir, server } => {
            let r = saltyctl::Server::new(&server, token()?).publish_manifest(&saltyctl::read_envelope(&manifest)?, &dir)?;
            println!("{r}");
        }
    }
    Ok(())
}
