//! Everything Vessel knows about specific harnesses.
//!
//! Owns: which binary each [`HarnessKind`] is, how to find it ([`Lookup`]),
//! how to ask it for its version ([`discover`]), and how to turn a
//! [`SessionSpec`] into a [`CommandSpec`] for terminal or structured (ACP)
//! mode ([`launch`]). Since milestone 4 also: harness profiles and
//! `profiles.toml` ([`Profile`], [`parse_profiles`]), each harness's home
//! and login check ([`home`], [`login_check`]), where a session's files are
//! and linking them into another home ([`session_files`],
//! [`link_session`]), MCP servers for terminal sessions ([`McpFile`]), and
//! where shared instructions and skills live in a home ([`layout`]).
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
//! - Choice: each harness module exports a `const` `harnesses::Descriptor`
//!   (binary, default profile name, how structured mode is launched, home,
//!   login check, session layout, terminal MCP form), selected by one
//!   exhaustive `match`. Where a harness needs logic rather than data
//!   (finding a transcript, writing MCP servers), its module has a function
//!   and the descriptor points to it.
//! - Model selection is deliberately absent: harness CLIs disagree on flags
//!   (`opencode acp` has none), while ACP session config options are
//!   harness-independent, so `vessel-agent` selects the model over ACP.
//!
//! Dependencies (`AGENTS.md` section 9), beyond the M1 set:
//! - `toml_edit` (`serde` feature), already the workspace's TOML library
//!   (`vessel-client` reads `config.toml` with it): `profiles.toml` into
//!   typed structs, and TOML values for Codex's `-c` overrides, quoted by
//!   the same library that defines the syntax. No second TOML crate.
//! - `serde` and `serde_json`: the profile structs, and Claude Code's
//!   `--mcp-config` JSON file.
//! - `getrandom`, already in the workspace: unpredictable names for MCP
//!   configuration files in the shared temporary directory.
//!
//! [`HarnessKind`]: vessel_proto::HarnessKind
//! [`SessionSpec`]: vessel_proto::SessionSpec
//! [`CommandSpec`]: vessel_proc::CommandSpec

mod discover;
mod error;
#[cfg(feature = "fakes")]
pub mod fakes;
mod harnesses;
mod home;
mod launch;
mod layout;
mod lookup;
mod mcp;
mod profile;
mod sessions;

pub use discover::discover;
pub use error::HarnessError;
pub use home::{home, login_check};
pub use launch::{Launch, launch};
pub use layout::{HarnessLayout, layout};
pub use lookup::Lookup;
pub use mcp::{EncodeProblem, McpError, McpFile, WrittenMcpFile};
pub use profile::{
    Position, Profile, ProfileProblem, Profiles, ProfilesError, parse_profiles, same_variable,
};
pub use sessions::{
    LinkError, Linked, SessionFiles, SessionFilesError, link_session, session_files,
};
