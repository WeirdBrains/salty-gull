use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

/// Salty Gull single-org update server.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Data directory (database, blobs, admin token hash).
    #[arg(long, env = "SALTY_HOME")]
    home: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve HTTP.
    Serve {
        /// The org this server serves.
        #[arg(long, env = "SALTY_ORG")]
        org: String,
        /// Listen address.
        #[arg(long, env = "SALTY_BIND", default_value = "127.0.0.1:8470")]
        bind: String,
    },
    /// Issue a new admin token (replaces the old one). Printed once; only its hash is kept.
    AdminToken,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("salty-server: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.cmd {
        Cmd::AdminToken => {
            let token = salty_server::issue_admin_token(&cli.home)?;
            println!("{token}");
            eprintln!("salty-server: store this token now; it is not shown again");
        }
        Cmd::Serve { org, bind } => {
            let store = salty_core::Store::open(&cli.home, &org)?;
            let admin_token_hash = salty_server::read_admin_token_hash(&cli.home)?;
            if admin_token_hash.is_none() {
                eprintln!("salty-server: no admin token issued; publishing is disabled (run `salty-server admin-token`)");
            }
            let app = Arc::new(salty_server::App { store, admin_token_hash });
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            eprintln!("salty-server: serving org {org} on http://{}", listener.local_addr()?);
            axum::serve(listener, salty_server::router(app))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
    }
    Ok(())
}
