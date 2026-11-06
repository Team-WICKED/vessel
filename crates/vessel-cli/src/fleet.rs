//! `vessel fleet ...` (ADR 0012): making a node the home, inviting and
//! joining members, and showing where the fleet stands.
//!
//! [Decision Log]
//! - Local and remote: `init`, `join`, `leave`, `status` and `restore`
//!   act on this machine's node through its data directory
//!   (`vessel_node::fleet`), so they work whether the node runs or not,
//!   and a running node picks the change up within seconds. `invite` and
//!   `members` are calls to a node (`--node`), normally the home.
//! - `join` reads the link from standard input and confirms the home's
//!   key fingerprint, exactly as `vessel nodes add --pair` does (shared in
//!   `pair`): the link is a single-use secret.
//! - `init` makes relative paths absolute against the current directory,
//!   since the node reads them from another one.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use vessel_node::fleet::{self, ConflictWhy, FleetFiles, FleetInit, FleetStatus, Role, SkipGlob};
use vessel_proto::{FleetMember, Scope};

use crate::args::{FleetCommand, Target};
use crate::connect::{connect, data_dir};
use crate::error::CliError;
use crate::{devices, io, pair};

/// `vessel fleet <command>`.
pub(crate) async fn command(target: &Target, command: FleetCommand) -> Result<(), CliError> {
    let files = FleetFiles::new(&data_dir(target)?);
    match command {
        FleetCommand::Init {
            instructions,
            skills,
            skip,
        } => init(files, instructions, skills, skip).await,
        FleetCommand::Invite { label, ttl } => {
            let client = connect(target).await?.client;
            let invitation = client
                .invite_with_scopes(label, ttl, vec![Scope::Sync])
                .await?;
            devices::show_invitation(&invitation, "on the joining node run\n  vessel fleet join")
                .await
        }
        FleetCommand::Join { yes } => join(files, yes).await,
        FleetCommand::Leave => {
            let was = blocking(move || fleet::leave(&files).map_err(CliError::from)).await?;
            io::note(if was {
                "left the fleet; the files it wrote stay as they are\n"
            } else {
                "this node is not a member of a fleet\n"
            })
            .await?;
            Ok(())
        }
        FleetCommand::Status => {
            let status = blocking(move || fleet::status(&files).map_err(CliError::from)).await?;
            io::out(render_status(&status).as_bytes()).await?;
            Ok(())
        }
        FleetCommand::Members => {
            let client = connect(target).await?.client;
            let members = client.fleet_members().await?;
            io::out(render_members(&members).as_bytes()).await?;
            Ok(())
        }
        FleetCommand::Restore { path } => {
            let path = absolute(&path)?;
            let shown = path.display().to_string();
            let restored =
                blocking(move || fleet::restore(&files, &path).map_err(CliError::from)).await?;
            io::note(&if restored == 0 {
                format!(
                    "nothing to restore at {shown}: no fleet file Vessel wrote is missing there\n"
                )
            } else {
                format!(
                    "restored {restored} file(s) at {shown}; the node writes them again within \
                     a few seconds (when it runs)\n"
                )
            })
            .await?;
            Ok(())
        }
    }
}

async fn init(
    files: FleetFiles,
    instructions: Option<PathBuf>,
    skills: Option<PathBuf>,
    skip: Vec<SkipGlob>,
) -> Result<(), CliError> {
    let init = FleetInit {
        instructions: instructions.as_deref().map(absolute).transpose()?,
        skills: skills.as_deref().map(absolute).transpose()?,
        skip,
    };
    let path = blocking(move || fleet::init(&files, &init).map_err(CliError::from)).await?;
    io::note(&format!(
        "this node is the fleet's home: {} names what members get\n\
         invite a node with `vessel fleet invite`, then run `vessel fleet join` there\n",
        path.display()
    ))
    .await?;
    Ok(())
}

async fn join(files: FleetFiles, yes: bool) -> Result<(), CliError> {
    let offer =
        pair::offer_from_stdin(yes, "vessel fleet invite", "join this node's fleet?").await?;
    let joined = fleet::join(&files, &offer, &node_name()).await?;
    io::note(&format!(
        "joined the fleet of {} ({}) via {}; this node follows it within a few seconds \
         (once it runs: `vessel node start`)\n",
        joined.home.name, joined.home.id, joined.route
    ))
    .await?;
    Ok(())
}

/// What this node calls itself on its fleet's home: the host's name.
pub(crate) fn node_name() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .unwrap_or_else(|| "node".to_owned())
}

/// `path` made absolute against the current directory.
fn absolute(path: &Path) -> Result<PathBuf, CliError> {
    Ok(std::path::absolute(path)?)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, CliError> + Send + 'static,
) -> Result<T, CliError> {
    tokio::task::spawn_blocking(work).await?
}

const TIME: &str = "%Y-%m-%d %H:%M:%S UTC";

/// What `vessel fleet status` prints.
fn render_status(status: &FleetStatus) -> String {
    let mut text = String::new();
    let state = &status.state;
    let _infallible = match &status.role {
        Role::None => writeln!(
            text,
            "role       none (`vessel fleet init` makes this node a home; `vessel fleet join` a member)"
        ),
        Role::Home { sources } => writeln!(text, "role       home ({})", sources.display()),
        Role::Member { home, joined_at } => writeln!(
            text,
            "role       member of {home}, since {}",
            joined_at.format(TIME)
        ),
    };
    if let Some(generation) = state.generation {
        let at = state
            .applied_at
            .map(|at| format!(", applied {}", at.format(TIME)))
            .unwrap_or_default();
        let _infallible = writeln!(text, "generation {}{at}", generation.get());
    }
    if let Some(contacted) = state.contacted_at {
        let _infallible = writeln!(text, "home seen  {}", contacted.format(TIME));
    }
    if state.skipped > 0 {
        let _infallible = writeln!(
            text,
            "skipped    {} place(s): sources, symlinks, homes not set up here",
            state.skipped
        );
    }
    if !state.conflicts.is_empty() {
        let _infallible = writeln!(text, "left as they are:");
    }
    for conflict in &state.conflicts {
        let (why, hint) = match conflict.kind {
            ConflictWhy::Edited => ("edited", String::new()),
            ConflictWhy::NotWritten => ("not Vessel's", String::new()),
            ConflictWhy::Deleted => (
                "deleted",
                format!(" (vessel fleet restore {})", conflict.path.display()),
            ),
        };
        let incoming = conflict
            .incoming
            .as_ref()
            .map(|incoming| format!("; incoming version at {}", incoming.display()))
            .unwrap_or_default();
        let _infallible = writeln!(
            text,
            "  {why:<13} {}  {}{incoming}{hint}",
            conflict.bundle.as_str(),
            conflict.path.display()
        );
    }
    for failure in &state.failures {
        let _infallible = writeln!(
            text,
            "failed     {}: {}",
            failure.path.display(),
            failure.error
        );
    }
    for problem in &state.problems {
        let _infallible = writeln!(text, "left out   {problem}");
    }
    if let Some(error) = &state.error {
        let _infallible = writeln!(text, "error      {error}");
    }
    text
}

/// What `vessel fleet members` prints.
fn render_members(members: &[FleetMember]) -> String {
    if members.is_empty() {
        return "no members yet (`vessel fleet invite`)\n".to_owned();
    }
    let mut text = String::new();
    for member in members {
        let id = member.device.to_string();
        let short = id.get(..8).unwrap_or(&id);
        let applied = member.applied.map_or_else(
            || "never reported".to_owned(),
            |generation| format!("generation {}", generation.get()),
        );
        let at = member
            .reported_at
            .map(|at| format!(" at {}", at.format(TIME)))
            .unwrap_or_default();
        let _infallible = writeln!(text, "{short}  {:<20} {applied}{at}", member.label);
        for conflict in &member.conflicts {
            let _infallible = writeln!(text, "          left as it is: {}", conflict.as_str());
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use vessel_node::fleet::{ConflictEntry, FleetState};
    use vessel_proto::Generation;

    use super::*;

    #[test]
    fn status_names_the_role_and_how_to_settle_a_deleted_file() {
        let status = FleetStatus {
            role: Role::Member {
                home: vessel_proto::NodeId::random(),
                joined_at: Utc::now(),
            },
            state: FleetState {
                generation: Some(Generation::new(4)),
                conflicts: vec![ConflictEntry {
                    bundle: "skills/review/SKILL.md".parse().unwrap(),
                    path: PathBuf::from("/home/u/.codex/skills/review/SKILL.md"),
                    kind: ConflictWhy::Deleted,
                    incoming: None,
                }],
                ..FleetState::default()
            },
        };
        let text = render_status(&status);
        assert!(text.contains("role       member of "), "{text}");
        assert!(text.contains("generation 4"), "{text}");
        assert!(
            text.contains("deleted       skills/review/SKILL.md"),
            "{text}"
        );
        assert!(
            text.contains("(vessel fleet restore /home/u/.codex/skills/review/SKILL.md)"),
            "{text}"
        );
    }
}
