//! Everything Vessel knows about specific harnesses.
//!
//! Owns: which binary each [`HarnessKind`] is, how to find it ([`Lookup`]),
//! how to ask it for its version ([`discover`]), and how to turn a
//! [`SessionSpec`] into a [`CommandSpec`] for terminal or structured (ACP)
//! mode ([`launch`]).
//!
//! Must not: run sessions, speak ACP, or leak harness specifics. Nothing
//! outside this crate matches on `HarnessKind` to decide behaviour; callers
//! ask this crate instead (`AGENTS.md` section 3). Inside it, the only such
//! `match` is `harnesses::descriptor`, and each harness's knowledge lives in
//! its own module under `harnesses/`.
//!
//! [Decision Log]
//! - Purpose: keep per-harness facts (binary names, ACP flags, adapters) in
//!   one place, so adding a harness or following a CLI change touches one
//!   file.
//! - Alternatives: a trait with one implementation per harness (dynamic or
//!   generic dispatch for what is, today, pure data); string-keyed config
//!   files (untyped, and a typo becomes a runtime error).
//! - Choice: each harness module exports a `const` [`harnesses::Descriptor`]
//!   (binary name plus how structured mode is launched), selected by one
//!   exhaustive `match`. If a harness needs logic rather than data later,
//!   its module grows a function and the descriptor points to it.
//! - Model selection is deliberately absent: harness CLIs disagree on flags
//!   (`opencode acp` has none), while ACP session config options are
//!   harness-independent, so `vessel-agent` selects the model over ACP.
//!
//! [`HarnessKind`]: vessel_proto::HarnessKind
//! [`SessionSpec`]: vessel_proto::SessionSpec
//! [`CommandSpec`]: vessel_proc::CommandSpec

mod discover;
mod error;
#[cfg(feature = "fakes")]
pub mod fakes;
mod harnesses;
mod launch;
mod lookup;

pub use discover::discover;
pub use error::HarnessError;
pub use launch::launch;
pub use lookup::Lookup;
