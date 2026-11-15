use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

#[derive(Debug, Parser)]
#[command(name = "sitewright", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon.
    Serve {
        /// Address to listen on.
        #[arg(long, env = "SITEWRIGHT_LISTEN", default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    sw_core::crypto::install_default_provider();
    match Cli::parse().command {
        Command::Serve { listen } => {
            let listener = TcpListener::bind(listen).await?;
            sitewright::serve(listener, shutdown_signal()).await
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("SITEWRIGHT_LOG_FORMAT").is_ok_and(|v| v == "json");
    if json {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
