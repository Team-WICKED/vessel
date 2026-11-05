//! `vesseld`: the standalone node binary.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use vessel_node::Config;

/// Vessel node daemon.
#[derive(Debug, Parser)]
#[command(name = "vesseld", version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "VESSEL_BIND", default_value_t = Config::default().bind)]
    bind: SocketAddr,
    /// Node name shown to clients (defaults to hostname).
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let config = Config {
        bind: args.bind,
        name: args.name,
        data_dir: args.data_dir,
    };
    match vessel_node::serve(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "node failed");
            ExitCode::FAILURE
        }
    }
}
