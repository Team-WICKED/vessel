//! `vessel nodes add NAME --pair`: pair this machine with a node from a
//! pairing link, and add the node to the book (ADR 0010, plan P4).
//!
//! [Decision Log]
//! - The link is read from standard input: pasted at a prompt, or piped.
//!   Never from the command line, which every user of the machine can read
//!   (`ps`, `/proc`) and the shell keeps in its history: someone else could
//!   redeem the link first. `--pair LINK` is refused (`args`).
//! - Before anything is sent, the node's id, its key's fingerprint, its
//!   routes and the link's expiry are shown, and a person at a terminal
//!   confirms. The fingerprint is what `vessel node status` prints on the
//!   node, for comparing out of band. `--yes` skips the question, and is
//!   required when the link is piped in, since no terminal is left to
//!   answer it.
//! - The name is checked free first, so a taken name never spends an
//!   invitation.
//! - The credential file is written before the book entry: an entry
//!   without its credential would be unusable, while a credential without
//!   an entry is an unused file (and the device can be revoked on the
//!   node).
//! - The pairing itself is `vessel_client::secure::pair_over` over the
//!   link's routes, direct addresses first, with the system's TLS roots
//!   for `wss://` relays.

use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use chrono::Utc;
use vessel_client::book::{BookEntry, BookFile, NodeName};
use vessel_client::secure::{RouteTimeouts, WsDialer, pair_over};
use vessel_client::{CredentialSource, DeviceCredential, NodeAddress, SecureTarget, fingerprint};
use vessel_proto::{PairingLink, PairingOffer};

use crate::args::Target;
use crate::error::CliError;
use crate::{io, nodes};

/// `vessel nodes add NAME --pair [--yes] [--default]`.
pub(crate) async fn add(
    target: &Target,
    name: NodeName,
    yes: bool,
    default: bool,
) -> Result<(), CliError> {
    let book = nodes::book_path(target)?;
    let credentials = config_dir(target)?;
    ensure_free(&book, &name).await?;
    let offer = offer_from_stdin(yes, "vessel node pair", "pair with this node?").await?;

    let dialer =
        WsDialer::for_routes(offer.direct.iter().chain(&offer.relays)).map_err(CliError::Tls)?;
    let device_name = hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .unwrap_or_else(|| "device".to_owned());
    let (paired, route) = pair_over(&dialer, &offer, &device_name, RouteTimeouts::default())
        .await
        .map_err(CliError::Reach)?;

    let path = DeviceCredential::path_in(&credentials, offer.node);
    let entry = BookEntry {
        address: NodeAddress::Secure(SecureTarget {
            key: offer.key,
            relays: offer.relays.clone(),
            direct: offer.direct.clone(),
        }),
        token: None,
        credential: Some(CredentialSource::File(shorten(&path))),
        id: Some(offer.node),
    };
    let saved = {
        let book = book.clone();
        let name = name.clone();
        tokio::task::spawn_blocking(move || -> Result<(), CliError> {
            paired
                .credential
                .write(&path)
                .map_err(CliError::Credential)?;
            let mut file = BookFile::open(&book)?;
            file.add(&name, &entry)?;
            if default {
                file.set_default(Some(name.as_str()))?;
            }
            Ok(())
        })
        .await?
    };
    saved?;
    io::note(&format!(
        "paired with {} ({}) via {route}; added {name} to {}\n",
        paired.node.name,
        paired.node.id,
        book.display()
    ))
    .await?;
    Ok(())
}

/// A pairing link read from standard input (pasted at a prompt, or
/// piped), shown, and confirmed unless `yes`: shared by `vessel nodes add
/// --pair` and `vessel fleet join`. `from` names the command that made the
/// link, `question` what is asked.
pub(crate) async fn offer_from_stdin(
    yes: bool,
    from: &str,
    question: &str,
) -> Result<PairingOffer, CliError> {
    let at_terminal = std::io::stdin().is_terminal();
    if !at_terminal && !yes {
        return Err(no_terminal());
    }
    let mut lines = io::input_lines()?;
    offer_from_lines(&mut lines, yes, from, question).await
}

/// [`offer_from_stdin`] on stdin lines the caller already reads (`vessel
/// init` asks other questions on the same input: a second reader would
/// wait behind the first one forever).
pub(crate) async fn offer_from_lines(
    lines: &mut tokio::sync::mpsc::Receiver<String>,
    yes: bool,
    from: &str,
    question: &str,
) -> Result<PairingOffer, CliError> {
    let at_terminal = std::io::stdin().is_terminal();
    if !at_terminal && !yes {
        return Err(no_terminal());
    }
    if at_terminal {
        io::note(&format!("paste the pairing link from `{from}`: ")).await?;
    }
    let link = loop {
        match lines.recv().await {
            Some(line) if line.trim().is_empty() => {}
            Some(line) => break line,
            None => return Err(CliError::NoPairingLink),
        }
    };
    let offer = link
        .trim()
        .parse::<PairingLink>()
        .and_then(|link| link.decode())
        .map_err(CliError::PairingLink)?;
    io::note(&describe(&offer)).await?;
    if !yes {
        io::note(&format!("{question} [y/N] ")).await?;
        let answer = lines.recv().await.unwrap_or_default();
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err(CliError::NotPaired);
        }
    }
    Ok(offer)
}

fn no_terminal() -> CliError {
    CliError::Usage(
        "with the link piped in there is no terminal to confirm the node; check its key \
         fingerprint and pass --yes",
    )
}

/// Where credentials go: `<config_dir>/vessel/credentials/`.
fn config_dir(target: &Target) -> Result<PathBuf, CliError> {
    target
        .config_dir
        .clone()
        .or_else(dirs::config_dir)
        .ok_or(CliError::NoHome)
}

/// Refuse a name the book already has, before an invitation is spent.
async fn ensure_free(book: &Path, name: &NodeName) -> Result<(), CliError> {
    let path = book.to_owned();
    let name = name.clone();
    tokio::task::spawn_blocking(move || -> Result<(), CliError> {
        let file = BookFile::open(&path)?;
        if file.book().get(name.as_str()).is_some() {
            return Err(vessel_client::book::BookError::Exists { path, name }.into());
        }
        Ok(())
    })
    .await?
}

/// What the link says, for the person deciding.
fn describe(offer: &PairingOffer) -> String {
    let mut text = format!(
        "node      {}\nkey       {}\n",
        offer.node,
        fingerprint(&offer.key)
    );
    for route in &offer.direct {
        let _infallible = writeln!(text, "direct    {route}");
    }
    for route in &offer.relays {
        let _infallible = writeln!(text, "relay     {route}");
    }
    let left = offer.expires.signed_duration_since(Utc::now());
    let _infallible = writeln!(
        text,
        "expires   {} (in {} min)\ncompare the key with `vessel node status` on that node",
        offer.expires.format("%Y-%m-%d %H:%M:%S UTC"),
        left.num_minutes().max(0)
    );
    text
}

/// `path` with the home directory written as `~`, so the book reads the
/// same on another machine of the same user.
fn shorten(path: &Path) -> PathBuf {
    dirs::home_dir()
        .and_then(|home| {
            path.strip_prefix(home)
                .ok()
                .map(|rest| Path::new("~").join(rest))
        })
        .unwrap_or_else(|| path.to_owned())
}
