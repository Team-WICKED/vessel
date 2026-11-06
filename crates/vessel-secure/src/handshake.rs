//! The Noise NK handshake: the client knows the node's static key, the node
//! knows nothing of the client (ADR 0009, ADR 0010).
//!
//! ```text
//! -> e, es    48 bytes: the client's ephemeral key, an empty payload's tag
//! <- e, ee    48 bytes: the node's ephemeral key, an empty payload's tag
//! ```
//!
//! [Decision Log]
//! - Purpose: authenticate the node to the client and agree on transport
//!   keys, with nothing for a relay to read or change unnoticed.
//! - Constraint: one mode only (`docs/LESSONS.md`): no negotiation, no
//!   capability flags, no fallback.
//! - Choice: empty payloads and messages of exactly 48 bytes. Anything else
//!   is refused before `snow` sees it. The version lives in the prologue,
//!   so a peer of another version fails the handshake instead of
//!   negotiating.
//! - A deadline covers the whole handshake (10 s by default), so a peer
//!   that stops answering cannot hold a connection open.
//! - The client cannot tell a node with another key from a dropped
//!   connection: NK gives the client no answer to read when the node fails
//!   the first message, and the node simply closes. The node, and an
//!   impostor's answer at the client, fail with
//!   [`SecureError::Handshake`].

use std::future::Future;

use futures_util::{SinkExt as _, StreamExt as _};
use snow::{Builder, HandshakeState};
use vessel_proto::NodePublicKey;

use crate::carrier::Carrier;
use crate::channel::SecureChannel;
use crate::error::SecureError;
use crate::keys::NodeKeypair;
use crate::{Config, PROLOGUE, SUITE};

/// Every handshake message: an ephemeral key (32) and an empty payload's
/// authentication tag (16).
pub(crate) const HANDSHAKE_LEN: usize = 48;

/// Client side: run the handshake to the node whose key is `node`.
///
/// # Errors
/// [`SecureError::Handshake`] when whoever answered does not hold the key;
/// [`SecureError::Timeout`], [`SecureError::Carrier`], or
/// [`SecureError::HandshakeMessage`] otherwise.
pub async fn initiate<C: Carrier>(
    carrier: C,
    node: &NodePublicKey,
) -> Result<SecureChannel<C>, SecureError> {
    initiate_with(carrier, node, &Config::default()).await
}

/// [`initiate`] with explicit limits.
///
/// # Errors
/// As [`initiate`].
pub async fn initiate_with<C: Carrier>(
    mut carrier: C,
    node: &NodePublicKey,
    config: &Config,
) -> Result<SecureChannel<C>, SecureError> {
    let mut state = initiator(PROLOGUE, node).map_err(SecureError::Handshake)?;
    within(config, async {
        let first = write_handshake(&mut state)?;
        carrier
            .send(first.into())
            .await
            .map_err(SecureError::Carrier)?;
        let second = receive(&mut carrier).await?;
        read_handshake(&mut state, &second)
    })
    .await?;
    SecureChannel::new(carrier, state, config)
}

/// Node side: answer a client's handshake with the node's key pair.
///
/// # Errors
/// [`SecureError::Handshake`] when the client used another key or version;
/// [`SecureError::Timeout`], [`SecureError::Carrier`], or
/// [`SecureError::HandshakeMessage`] otherwise.
pub async fn respond<C: Carrier>(
    carrier: C,
    keypair: &NodeKeypair,
) -> Result<SecureChannel<C>, SecureError> {
    respond_with(carrier, keypair, &Config::default()).await
}

/// [`respond`] with explicit limits.
///
/// # Errors
/// As [`respond`].
pub async fn respond_with<C: Carrier>(
    mut carrier: C,
    keypair: &NodeKeypair,
    config: &Config,
) -> Result<SecureChannel<C>, SecureError> {
    let mut state = responder(PROLOGUE, keypair).map_err(SecureError::Handshake)?;
    within(config, async {
        let first = receive(&mut carrier).await?;
        read_handshake(&mut state, &first)?;
        let second = write_handshake(&mut state)?;
        carrier
            .send(second.into())
            .await
            .map_err(SecureError::Carrier)
    })
    .await?;
    SecureChannel::new(carrier, state, config)
}

async fn within<T>(
    config: &Config,
    work: impl Future<Output = Result<T, SecureError>>,
) -> Result<T, SecureError> {
    tokio::time::timeout(config.handshake_timeout, work)
        .await
        .map_err(|_elapsed| SecureError::Timeout(config.handshake_timeout))?
}

async fn receive<C: Carrier>(carrier: &mut C) -> Result<bytes::Bytes, SecureError> {
    carrier
        .next()
        .await
        .ok_or_else(SecureError::eof)?
        .map_err(SecureError::Carrier)
}

/// The suite with `prologue`: [`PROLOGUE`] for channels, another label for
/// other uses of the same pattern (the relay's possession proof).
pub(crate) fn builder(prologue: &[u8]) -> Result<Builder<'_>, snow::Error> {
    Builder::new(SUITE.parse()?).prologue(prologue)
}

/// The client's handshake state towards `node`.
pub(crate) fn initiator(
    prologue: &[u8],
    node: &NodePublicKey,
) -> Result<HandshakeState, snow::Error> {
    builder(prologue)?
        .remote_public_key(node.as_bytes())?
        .build_initiator()
}

/// The node's handshake state.
pub(crate) fn responder(
    prologue: &[u8],
    keypair: &NodeKeypair,
) -> Result<HandshakeState, snow::Error> {
    builder(prologue)?
        .local_private_key(keypair.secret())?
        .build_responder()
}

/// Our next handshake message, with an empty payload.
pub(crate) fn write_handshake(state: &mut HandshakeState) -> Result<Vec<u8>, SecureError> {
    let mut message = vec![0_u8; HANDSHAKE_LEN];
    let len = state
        .write_message(&[], &mut message)
        .map_err(SecureError::Handshake)?;
    message.truncate(len);
    Ok(message)
}

/// The peer's next handshake message, which must be 48 bytes with an empty
/// payload.
pub(crate) fn read_handshake(
    state: &mut HandshakeState,
    message: &[u8],
) -> Result<(), SecureError> {
    if message.len() != HANDSHAKE_LEN {
        return Err(SecureError::HandshakeMessage { len: message.len() });
    }
    // The payload is the message minus the key and the tag: empty, so the
    // output buffer is too.
    let _empty = state
        .read_message(message, &mut [])
        .map_err(SecureError::Handshake)?;
    Ok(())
}
