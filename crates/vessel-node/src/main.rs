//! `vesseld`: the standalone node binary. Runs until `Ctrl-C` or `SIGTERM`,
//! then stops cleanly (sessions end as `Interrupted`, logs committed).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use vessel_node::{Config, DataDir, Node};

/// Vessel node daemon.
#[derive(Debug, Parser)]
#[command(name = "vesseld", version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "VESSEL_BIND", default_value = "127.0.0.1:7077")]
    bind: SocketAddr,
    /// Node name shown to clients (defaults to the hostname).
    #[arg(long, env = "VESSEL_NAME")]
    name: Option<String>,
    /// Data directory (defaults to ~/.vessel).
    #[arg(long, env = "VESSEL_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_unset| "info".into()),
        )
        .with_writer(std::io::stderr)
        // Colours only for a person at a terminal, not for log files.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let args = Args::parse();
    let Some(root) = args.data_dir.or_else(DataDir::default_root) else {
        tracing::error!("no home directory; pass --data-dir");
        return ExitCode::FAILURE;
    };
    let mut config = Config::new(DataDir::new(root));
    config.bind = args.bind;
    config.name = args.name;

    let node = match Node::start(config).await {
        Ok(node) => node,
        Err(error) => {
            tracing::error!(error = %chain(&error), "node failed to start");
            return ExitCode::FAILURE;
        }
    };
    vessel_node::signal().await;
    tracing::info!("stopping");
    match node.stop().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %chain(&error), "node failed");
            ExitCode::FAILURE
        }
    }
}

/// The error and its causes on one line.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
