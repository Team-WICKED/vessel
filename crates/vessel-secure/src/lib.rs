//! Vessel's end-to-end secure channel (ADR 0009): Noise
//! `NK_25519_ChaChaPoly_SHA256` from a client to a node's static key, over
//! any carrier of binary messages, as a `FrameTransport` of protocol frames.
//!
//! The same channel runs over a relay, directly on a node's `/secure`
//! endpoint, and in memory. It knows nothing of the protocol: it moves
//! UTF-8 frames of up to 16 MiB, and what a relay or a network sees is
//! sealed records.
//!
//! [Decision Log]
//! - Purpose: reach a node through a relay that is not trusted, without a
//!   second security mode beside it (`docs/LESSONS.md`, Paseo's two).
//! - Constraint: one pattern, one suite, published test vectors for it, no
//!   C or assembly in the build (Windows clients), no `unsafe` of ours.
//! - Alternatives (ADR 0009): TLS end to end through the relay; a
//!   hand-rolled NK on the same primitives; `snow`.
//! - Choice: `snow` without its `std` feature (which would pull `ring`),
//!   with only the pure-Rust primitives of our suite. This crate wraps it
//!   so that nothing else in the workspace touches Noise: the handshake
//!   ([`initiate`], [`respond`]), the record layer ([`SecureChannel`]), the
//!   relay's possession proof ([`proof_challenge`], [`answer_proof`]), and
//!   what is derived from keys ([`NodeKeypair`], [`routing_id`],
//!   [`fingerprint`]).
//! - Edges: `-> proto` only, for the key types that travel on the wire
//!   (ADR 0001).
//! - Trade-off: `snow` has no formal audit (plan, decision 8). Its
//!   primitives are audited; the composition is covered by the published
//!   vectors and our tamper and fuzz tests.
//!
//! Layout: `handshake` runs NK and builds a [`SecureChannel`] (`channel`),
//! which chunks frames into records (`record`) over a [`Carrier`]
//! (`carrier`). `keys` and `proof` serve the node and the relay.

mod carrier;
mod channel;
mod error;
mod handshake;
mod keys;
mod proof;
mod record;

#[cfg(test)]
mod tests;

use std::time::Duration;

pub use carrier::{Carrier, LengthPrefixed, MemoryCarrier, memory_pair};
pub use channel::SecureChannel;
pub use error::{SecureError, Tamper};
pub use handshake::{RespondError, initiate, initiate_with, respond, respond_with, try_respond};
pub use keys::{NodeKeypair, fingerprint, routing_id};
pub use proof::{PROOF_PROLOGUE, ProofChallenge, answer_proof, proof_challenge};
pub use record::MAX_RECORD;

/// The one Noise suite (ADR 0009).
pub const SUITE: &str = "Noise_NK_25519_ChaChaPoly_SHA256";

/// The prologue of every channel handshake: the channel's version. A peer
/// with another one fails the handshake; nothing is negotiated.
pub const PROLOGUE: &[u8] = b"vessel/secure/1";

/// The protocol's frame limit, 16 MiB (`vessel_rpc::MAX_FRAME`).
pub const DEFAULT_MAX_FRAME: u32 = 16 * 1024 * 1024;

/// The default deadline for a whole handshake.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Limits of a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// The longest frame either side may send; a longer announced frame
    /// closes the channel before anything is buffered.
    pub max_frame: u32,
    /// How long the whole handshake may take.
    pub handshake_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_frame: DEFAULT_MAX_FRAME,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}
