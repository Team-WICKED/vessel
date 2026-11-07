//! `vessel fleet secret set|list|delete|copy` (ADR 0012, plan F3): this
//! node's secrets for the fleet's MCP servers.
//!
//! [Decision Log]
//! - A value never comes from the command line, which every user of the
//!   machine can read (`ps`, `/proc`) and the shell keeps in its history:
//!   it is typed at a prompt that does not show it, or piped in (`... |
//!   vessel fleet secret set NAME`; one trailing newline is dropped, so a
//!   value can span lines, such as a PEM key).
//! - The prompt reads the terminal in raw mode, byte by byte: the
//!   workspace builds `crossterm` without its event reader, and raw mode
//!   already turns echo off. Enter ends, Backspace and Ctrl-U edit, Ctrl-C
//!   and Ctrl-D cancel; anything else is part of the value as typed.
//! - Nothing prints a value: `list` prints names, `set` and `delete` say
//!   what happened by name.
//! - `copy` asks the running node to send its value to another fleet node
//!   (`secret.copy`); the value goes node to node through the home and
//!   never through this command. The target is a whole node id, never a
//!   name or a prefix: a value must not reach a node that merely matched.

use std::io::{IsTerminal as _, Read as _};

use vessel_node::fleet::{self, FleetFiles};

use vessel_proto::{NodeId, SecretName};

use crate::args::{SecretCommand, Target};
use crate::connect::connect;
use crate::error::CliError;
use crate::io;

/// The longest value accepted, in bytes.
const MAX_VALUE: u64 = 64 * 1024;

/// `vessel fleet secret <command>`.
pub(crate) async fn command(
    target: &Target,
    files: FleetFiles,
    command: SecretCommand,
) -> Result<(), CliError> {
    match command {
        SecretCommand::Set { name } => {
            let at_terminal = std::io::stdin().is_terminal();
            if at_terminal {
                io::note(&format!("value of {name} (not shown): ")).await?;
            }
            let value = tokio::task::spawn_blocking(move || {
                if at_terminal {
                    read_hidden()
                } else {
                    read_piped()
                }
            })
            .await??;
            if at_terminal {
                io::note("\n").await?;
            }
            if value.is_empty() {
                return Err(CliError::Usage("an empty secret is not set"));
            }
            let shown = name.clone();
            tokio::task::spawn_blocking(move || fleet::set_secret(&files, &name, &value)).await??;
            io::note(&format!(
                "set {shown}; sessions this node starts from now on use it\n"
            ))
            .await?;
        }
        SecretCommand::List => {
            let names = tokio::task::spawn_blocking(move || fleet::secret_names(&files)).await??;
            let mut text = String::new();
            for name in &names {
                text.push_str(name.as_str());
                text.push('\n');
            }
            if text.is_empty() {
                io::note("no secrets set on this node\n").await?;
            } else {
                io::out(text.as_bytes()).await?;
            }
        }
        SecretCommand::Delete { name } => {
            let shown = name.clone();
            let deleted =
                tokio::task::spawn_blocking(move || fleet::delete_secret(&files, &name)).await??;
            io::note(&if deleted {
                format!("deleted {shown}\n")
            } else {
                format!("{shown} was not set\n")
            })
            .await?;
        }
        SecretCommand::Copy { name, to } => copy(target, name, to).await?,
    }
    Ok(())
}

/// `vessel fleet secret copy NAME --to NODE_ID`: the node sends its value
/// to the fleet node with exactly that id, through the fleet's home. The
/// value never passes through this command.
async fn copy(target: &Target, name: SecretName, to: NodeId) -> Result<(), CliError> {
    let client = connect(target).await?.client;
    client.secret_copy(name.clone(), to).await?;
    io::note(&format!("copied {name} to node {to}\n")).await?;
    Ok(())
}

/// Standard input, piped: all of it, without one trailing newline.
fn read_piped() -> Result<String, CliError> {
    let mut bytes = Vec::new();
    let _read = std::io::stdin()
        .lock()
        .take(MAX_VALUE.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_VALUE {
        return Err(CliError::Usage("a secret is at most 64 KiB"));
    }
    let mut text = String::from_utf8(bytes)
        .map_err(|_not_utf8| CliError::Usage("a secret must be UTF-8 text"))?;
    if text.ends_with('\n') {
        let _newline = text.pop();
        if text.ends_with('\r') {
            let _return = text.pop();
        }
    }
    Ok(text)
}

/// Raw mode while alive, so typing is not shown.
struct NoEcho;

impl NoEcho {
    fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        if let Err(error) = crossterm::terminal::disable_raw_mode() {
            tracing::warn!(%error, "could not restore the terminal; run `reset`");
        }
    }
}

/// A line typed at the terminal, not shown.
fn read_hidden() -> Result<String, CliError> {
    let _no_echo = NoEcho::enable()?;
    let mut value = Vec::new();
    let mut stdin = std::io::stdin().lock();
    let mut byte = [0_u8; 1];
    loop {
        if stdin.read(&mut byte)? == 0 {
            break;
        }
        let [typed] = byte;
        match typed {
            b'\r' | b'\n' => break,
            // Ctrl-C, Ctrl-D: give up.
            0x03 | 0x04 => return Err(CliError::Usage("no secret was set")),
            // Backspace (DEL or ^H): drop the last character, not byte.
            0x7f | 0x08 => {
                let text = String::from_utf8_lossy(&value).into_owned();
                let mut chars = text.chars();
                let _last = chars.next_back();
                value = chars.as_str().as_bytes().to_vec();
            }
            // Ctrl-U: start again.
            0x15 => value.clear(),
            other => value.push(other),
        }
        if u64::try_from(value.len()).unwrap_or(u64::MAX) > MAX_VALUE {
            return Err(CliError::Usage("a secret is at most 64 KiB"));
        }
    }
    String::from_utf8(value).map_err(|_not_utf8| CliError::Usage("a secret must be UTF-8 text"))
}
