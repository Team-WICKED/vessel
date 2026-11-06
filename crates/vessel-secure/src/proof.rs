//! The possession proof a relay asks of a registering node (ADR 0009).
//!
//! [Decision Log]
//! - Purpose: a relay must not let anyone register a node's routing id
//!   without holding the node's private key, or a stranger could take the
//!   node's slot and black-hole its clients.
//! - Alternatives: a signature (X25519 keys do not sign; a second key type
//!   would be needed); a challenge encrypted to the key (a new
//!   construction).
//! - Choice: the same Noise NK handshake, with the relay as the initiator
//!   towards the claimed key. Only the key's holder can read the first
//!   message and answer it; the relay accepts the registration if the
//!   answer authenticates. Its fresh ephemeral key makes every challenge,
//!   and so every answer, single-use.
//! - The prologue differs from the channel's ([`PROOF_PROLOGUE`]), so a
//!   challenge cannot be passed to a node's `/secure` endpoint as a client
//!   handshake, and a node's proof answer is useless as a channel handshake.

use std::fmt;

use snow::HandshakeState;
use vessel_proto::{HandshakeMessage, NodePublicKey};

use crate::error::SecureError;
use crate::handshake::{initiator, read_handshake, responder, write_handshake};
use crate::keys::NodeKeypair;

/// The prologue of the possession proof's handshake.
pub const PROOF_PROLOGUE: &[u8] = b"vessel/relay-proof/1";

/// The relay's half of a possession proof, waiting for the node's answer.
pub struct ProofChallenge {
    state: HandshakeState,
}

impl fmt::Debug for ProofChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProofChallenge").finish_non_exhaustive()
    }
}

/// Relay side: challenge whoever claims `key`. Send the message as
/// `Prove`, then [`ProofChallenge::verify`] the `Proof` that comes back.
///
/// # Errors
/// Only if the Noise state machine fails, which is a bug.
pub fn proof_challenge(
    key: &NodePublicKey,
) -> Result<(ProofChallenge, HandshakeMessage), SecureError> {
    let mut state = initiator(PROOF_PROLOGUE, key).map_err(SecureError::Handshake)?;
    let message = message(write_handshake(&mut state)?)?;
    Ok((ProofChallenge { state }, message))
}

impl ProofChallenge {
    /// Whether `answer` proves the key: it authenticates only if it was made
    /// with the private key for this very challenge.
    #[must_use]
    pub fn verify(mut self, answer: &HandshakeMessage) -> bool {
        read_handshake(&mut self.state, answer.as_bytes()).is_ok()
    }
}

/// Node side: answer a relay's challenge with the node's key pair.
///
/// # Errors
/// [`SecureError::Handshake`] or [`SecureError::HandshakeMessage`] when the
/// challenge is not for this key or not a challenge at all.
pub fn answer_proof(
    keypair: &NodeKeypair,
    challenge: &HandshakeMessage,
) -> Result<HandshakeMessage, SecureError> {
    let mut state = responder(PROOF_PROLOGUE, keypair).map_err(SecureError::Handshake)?;
    read_handshake(&mut state, challenge.as_bytes())?;
    message(write_handshake(&mut state)?)
}

fn message(bytes: Vec<u8>) -> Result<HandshakeMessage, SecureError> {
    let len = bytes.len();
    HandshakeMessage::new(bytes).map_err(|_too_long| SecureError::HandshakeMessage { len })
}
