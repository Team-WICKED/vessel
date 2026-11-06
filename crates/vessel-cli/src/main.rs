//! `vessel`: the command-line client for Vessel nodes.
//!
//! Commands: `node start`, `node install`, `nodes`, `status`, `harnesses`,
//! `sessions`, `run`, `attach`, `send`, `log`, `kill`. Everything goes through
//! `vessel-client`; this crate only parses arguments, resolves the token,
//! and turns results into text. Output goes to stdout, notes and errors to
//! stderr, so `vessel log ID > file` captures exactly the session's output.

mod args;
mod attach;
mod commands;
mod connect;
mod devices;
mod error;
mod io;
mod log;
mod nodes;
mod pair;
mod remote;
mod render;
mod selector;
mod view;

use std::process::ExitCode;

use clap::Parser as _;

use crate::args::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // A node shows its lifecycle; client commands only warnings.
    let default_level = match cli.command {
        Command::Node { .. } => "info",
        Command::Nodes { .. }
        | Command::Status
        | Command::Harnesses
        | Command::Sessions
        | Command::Run(_)
        | Command::Attach { .. }
        | Command::Send { .. }
        | Command::Log { .. }
        | Command::Kill { .. } => "warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_unset| default_level.into()),
        )
        .with_writer(std::io::stderr)
        // Colours only for a person at a terminal, not for log files.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    match commands::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if io::note(&format!("error: {}\n", chain(&error)))
                .await
                .is_err()
            {
                tracing::error!("could not report an error on stderr");
            }
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
