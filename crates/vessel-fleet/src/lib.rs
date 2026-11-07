//! Fleet bundles (ADR 0012): what the fleet's home publishes, and how each
//! node writes it into its harness homes.
//!
//! ```text
//! home:    Sources ──bundle──▶ Manifest + BlobStore     (fleet.manifest, fleet.blob)
//! member:  Manifest + BlobStore ──plan──▶ Plan ──apply──▶ files in each Target
//!                                          ▲                   │
//!                                          └──── Managed ◀─────┘   (managed.json)
//! launch:  McpDefinitions + secrets ──resolve──▶ ResolvedMcpServers
//! transfer: files in a home ──snapshot──▶ staging BlobStore   (session.export, memory.export)
//!           received BlobStore ──place──▶ files in another home (session.import, memory.import)
//! ```
//!
//! No network and no node: the node moves manifests and blobs between
//! machines and decides when to bundle and apply; this crate only reads
//! and writes files.
//!
//! [Decision Log]
//! - Purpose: one person's instructions, skills and MCP server definitions,
//!   kept in one place (the home), reach every profile of every machine,
//!   without ever losing a local edit and without touching anything Vessel
//!   did not write (ADR 0012).
//! - Constraint: the home's own harness homes may already point at the
//!   sources by symlink (the operator's single-source scheme); targets
//!   there must be left alone, and nothing may ever be written into a
//!   source. A target directory may hold symlinks to anywhere.
//! - Bundle layout: `instructions.md` for the instructions file and
//!   `skills/<name>/...` for each skill, `<name>` being the `name` in its
//!   `SKILL.md` (the operator's sync script names skills the same way, so
//!   a directory named differently, or in another script, is linked under
//!   its English name). A target maps them to its own file names
//!   ([`Target`]).
//! - Content addressed: blobs are stored and fetched by SHA-256, so equal
//!   content is stored and sent once, and a member fetches only what it
//!   lacks. The home copies sources into its store when it bundles, so a
//!   manifest and its blobs always agree even if a source changes later.
//! - Copies with recorded hashes ([`Managed`], `managed.json`, the role of
//!   Paseo's `.paseo-managed-files.json`): a file whose content is still
//!   what Vessel wrote is Vessel's to update or remove; any other content
//!   is someone's edit, kept, with the incoming version written beside it
//!   (`<name>.vessel-incoming`) and listed as a conflict. Paseo instead
//!   overwrites edited files on update. A file Vessel wrote that was
//!   deleted on the node stays deleted, its record kept, and is listed as a
//!   conflict of its own kind ([`ConflictKind::Deleted`]) until a person
//!   restores it ([`Managed::restore`], `vessel fleet restore <path>`):
//!   deleting is an edit too. The other way to settle it, dropping the
//!   record when the user removes it by hand, was not taken: it asks people
//!   to edit `managed.json`, which nothing else does.
//! - Bundle paths other than `instructions.md` and `skills/...` are reserved
//!   for later versions (PROTOCOL.md, 5): a member ignores them, so a newer
//!   home does not stop an older member.
//! - Symlinks below a target's root are never followed: a link into a
//!   source means the target already has it (skipped), any other link is
//!   someone's arrangement (skipped, reported).
//! - Acting where it checked: each target root is made canonical once,
//!   records are kept by canonical root ([`Managed`]), and every place is
//!   reached through directories held open from that root without
//!   following a symlink (`dir`, `openat` on Unix). Each step looks again
//!   right before it acts; a removal deletes only a regular file that
//!   still has the recorded content, is not adopted, and is not the same
//!   file as a placed one. A [`Plan`] is made only by [`plan()`] and
//!   carries the protected sources, so [`apply()`] cannot be handed steps
//!   or sources of anyone's choosing. (The independent review of F1 found
//!   each of these failing; `tests/fleet/safety.rs` keeps them.)
//! - Alternatives: symlinks to Vessel's copy (cannot tell an edit from an
//!   update, and an edit through the link changes every profile); `git`
//!   as the transport (a remote on every machine, and still no answer for
//!   MCP servers); `globset` and a YAML parser (the skip globs only need
//!   `*` and `?` within one name, and only one frontmatter field is read).
//! - Edges: `-> proto` (the manifest and MCP types), `-> harness` (each
//!   harness's instruction file and skills directory names,
//!   [`Target::for_harness`]).

mod apply;
mod blobs;
mod bundle;
mod dir;
mod files;
mod managed;
mod place;
mod plan;
mod rel;
mod resolve;
mod skill;
mod target;
mod transfer;

pub use apply::{Applied, ApplyError, Conflict, ConflictKind, apply};
pub use blobs::{BlobStore, BlobWriter, InvalidBlob, PastEnd};
pub use bundle::{
    BundleError, Bundled, InvalidSkipGlob, MAX_FILE, SkipGlob, SourceProblem, Sources, bundle,
};
pub use managed::{Content, Managed, ManagedError, Record};
pub use place::MissingSource;
pub use plan::{Place, Plan, PlanError, Skip, SkipReason, Step, plan};
pub use resolve::resolve;
pub use skill::{SkillName, SkillNameError, skill_name};
pub use target::Target;
pub use transfer::{
    Incoming, Limits, OnConflict, PlaceError, Placed, Snapshot, SnapshotError, place, snapshot,
};

/// Write `contents` to `path` atomically and owner-only (0600 on Unix),
/// creating its directory owner-only (0700) if missing: how Vessel keeps
/// its own state files (`managed.json`, and the node's fleet files).
///
/// # Errors
/// The directory or the file cannot be written.
pub fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        files::private_dir(dir)?;
    }
    files::replace(path, files::Mode::Private, contents)
}

/// The instructions file's place in a bundle.
pub const INSTRUCTIONS: &str = "instructions.md";

/// The directory skills sit under in a bundle.
pub const SKILLS: &str = "skills";

/// The suffix of an incoming version written beside a locally edited file.
pub const INCOMING_SUFFIX: &str = ".vessel-incoming";
