//! `fleet.toml`: what the home bundles. Read by the node ([`read_sources`]),
//! written once by `vessel fleet init` ([`init`]), edited by people after.
//!
//! ```toml
//! instructions = "~/.codex/AGENTS.md"
//! skills = "~/.codex/skills"
//! skip = [".system", "paseo*"]
//!
//! [mcp.servers.docs]
//! type = "stdio"
//! command = "npx"
//! args = ["-y", "@example/docs-mcp"]
//! env = { DOCS_TOKEN = { secret = "DOCS_TOKEN" } }
//! ```
//!
//! [Decision Log]
//! - Typed and strict (unknown keys are errors), so a misspelt `skils`
//!   never publishes a bundle without skills. A file that does not parse
//!   keeps the previous bundle published; the error is in `vessel fleet
//!   status` and the node's log.
//! - Paths are absolute or start with `~/` (the node user's home): the
//!   node's working directory means nothing to the person editing the file.
//! - MCP servers are written in the manifest's own shape (`type =
//!   "stdio"` or `"http"`), so the file and the protocol never drift; a
//!   value is text or `{ secret = "NAME" }`, never a secret's value (F3
//!   resolves names on each node from its own `secrets.toml`).
//! - `init` writes the file through `toml_edit`, so any path is quoted
//!   correctly, and reads it back with the node's own parser before
//!   keeping it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use vessel_fleet::{InvalidSkipGlob, SkipGlob, Sources};
use vessel_proto::{ConfigValue, McpServerName, McpTransport};

use super::files::{FileError, FleetFiles};

/// `fleet.toml` as typed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    skills: Option<String>,
    #[serde(default)]
    skip: Vec<String>,
    #[serde(default)]
    mcp: RawMcp,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawMcp {
    #[serde(default)]
    servers: BTreeMap<McpServerName, McpTransport<ConfigValue>>,
}

/// Why `fleet.toml` cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum SourcesError {
    /// It could not be read.
    #[error(transparent)]
    File(#[from] FileError),
    /// It is not TOML of the right shape.
    #[error("{}: {message}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// What the parser said (names keys and positions, never values
        /// beyond the file's own text).
        message: String,
    },
    /// A path is neither absolute nor under `~/`.
    #[error("{}: `{key}` must be an absolute path or start with `~/`", path.display())]
    Relative {
        /// The file.
        path: PathBuf,
        /// Which key.
        key: &'static str,
    },
    /// A path starts with `~/` and the node user has no home directory.
    #[error("{}: `~/` in `{key}` but this user has no home directory", path.display())]
    NoHome {
        /// The file.
        path: PathBuf,
        /// Which key.
        key: &'static str,
    },
    /// A `skip` entry is not a pattern.
    #[error("{}: {source}", path.display())]
    Skip {
        /// The file.
        path: PathBuf,
        /// Which pattern.
        #[source]
        source: InvalidSkipGlob,
    },
    /// Neither `instructions` nor `skills`: nothing to bundle.
    #[error("{}: names neither `instructions` nor `skills`", path.display())]
    Empty {
        /// The file.
        path: PathBuf,
    },
}

/// The home's sources from `fleet.toml`, if there is one. Blocks.
pub(crate) fn read_sources(
    files: &FleetFiles,
    user_home: Option<&Path>,
) -> Result<Option<(String, Sources)>, SourcesError> {
    let path = files.sources();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(FileError::io(path, error).into()),
    };
    let sources = parse(path, &text, user_home)?;
    Ok(Some((text, sources)))
}

fn parse(path: &Path, text: &str, user_home: Option<&Path>) -> Result<Sources, SourcesError> {
    let raw: RawFile = toml_edit::de::from_str(text).map_err(|error| SourcesError::Parse {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    let place = |value: Option<String>, key: &'static str| {
        value
            .map(|value| expand(path, key, &value, user_home))
            .transpose()
    };
    let instructions = place(raw.instructions, "instructions")?;
    let skills = place(raw.skills, "skills")?;
    if instructions.is_none() && skills.is_none() {
        return Err(SourcesError::Empty {
            path: path.to_owned(),
        });
    }
    let skip = raw
        .skip
        .iter()
        .map(|glob| glob.parse::<SkipGlob>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| SourcesError::Skip {
            path: path.to_owned(),
            source,
        })?;
    Ok(Sources {
        instructions,
        skills,
        skip,
        mcp: raw.mcp.servers,
    })
}

/// `~/x` under the user's home; an absolute path as it is.
fn expand(
    path: &Path,
    key: &'static str,
    value: &str,
    user_home: Option<&Path>,
) -> Result<PathBuf, SourcesError> {
    if let Some(rest) = value.strip_prefix("~/") {
        let home = user_home.ok_or_else(|| SourcesError::NoHome {
            path: path.to_owned(),
            key,
        })?;
        return Ok(home.join(rest));
    }
    let absolute = PathBuf::from(value);
    if absolute.is_absolute() {
        Ok(absolute)
    } else {
        Err(SourcesError::Relative {
            path: path.to_owned(),
            key,
        })
    }
}

/// What `vessel fleet init` writes.
#[derive(Debug, Clone, Default)]
pub struct FleetInit {
    /// The instructions file (absolute).
    pub instructions: Option<PathBuf>,
    /// The skills directory (absolute).
    pub skills: Option<PathBuf>,
    /// Names to leave out.
    pub skip: Vec<SkipGlob>,
}

/// Why `vessel fleet init` did not make this node a home.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// It already is one.
    #[error("{} exists: this node is already the fleet's home (edit the file to change it)", .0.display())]
    Exists(PathBuf),
    /// It is a member of another home.
    #[error("this node is a member of a fleet; `vessel fleet leave` first")]
    Member,
    /// Nothing to bundle.
    #[error("give the instructions file, the skills directory, or both")]
    Empty,
    /// A path that is not absolute, or not UTF-8.
    #[error("{}: not an absolute UTF-8 path", .0.display())]
    Path(PathBuf),
    /// The written file did not read back (a bug).
    #[error(transparent)]
    Sources(#[from] SourcesError),
    /// It could not be written.
    #[error(transparent)]
    File(#[from] FileError),
}

/// Make this node the fleet's home: write `fleet.toml`. Blocks.
///
/// # Errors
/// See [`InitError`].
pub fn init(files: &FleetFiles, init: &FleetInit) -> Result<PathBuf, InitError> {
    let path = files.sources().to_owned();
    if path.exists() {
        return Err(InitError::Exists(path));
    }
    if files.membership().exists() {
        return Err(InitError::Member);
    }
    if init.instructions.is_none() && init.skills.is_none() {
        return Err(InitError::Empty);
    }
    let text_of = |value: &Option<PathBuf>| -> Result<Option<String>, InitError> {
        value
            .as_ref()
            .map(|path| {
                path.to_str()
                    .filter(|_utf8| path.is_absolute())
                    .map(str::to_owned)
                    .ok_or_else(|| InitError::Path(path.clone()))
            })
            .transpose()
    };
    let mut document = toml_edit::DocumentMut::new();
    document.decor_mut().set_prefix(
        "# The fleet's home: what every member gets (ADR 0012). Edit freely; the\n\
         # node notices within seconds. Paths are absolute or start with `~/`.\n\
         # MCP servers: [mcp.servers.<name>] with type = \"stdio\" (command,\n\
         # args, env) or \"http\" (url, headers); a secret is { secret = \"NAME\" }.\n",
    );
    if let Some(instructions) = text_of(&init.instructions)? {
        let _new = document.insert("instructions", toml_edit::value(instructions));
    }
    if let Some(skills) = text_of(&init.skills)? {
        let _new = document.insert("skills", toml_edit::value(skills));
    }
    let mut skip = toml_edit::Array::new();
    for glob in &init.skip {
        skip.push(glob.to_string());
    }
    let _new = document.insert("skip", toml_edit::value(skip));
    let text = document.to_string();
    let _checked = parse(&path, &text, None)?;
    vessel_fleet::write_private(&path, text.as_bytes())
        .map_err(|error| FileError::io(&path, error))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Result<Sources, SourcesError> {
        parse(Path::new("fleet.toml"), text, Some(Path::new("/home/u")))
    }

    /// An absolute path on this OS (`/srv` is not one on Windows), as a
    /// TOML literal string.
    fn absolute(name: &str) -> (PathBuf, String) {
        let path = std::env::current_dir().unwrap().join(name);
        let text = format!("'{}'", path.display());
        (path, text)
    }

    #[test]
    fn the_adr_example_reads_with_the_type_key() {
        let (skills, skills_text) = absolute("skills");
        let sources = parsed(&format!(
            r#"
instructions = "~/.codex/AGENTS.md"
skills = {skills_text}
skip = [".system", "paseo*"]

[mcp.servers.docs]
type = "stdio"
command = "npx"
args = ["-y", "@example/docs-mcp"]
env = {{ DOCS_TOKEN = {{ secret = "DOCS_TOKEN" }}, MODE = "plain" }}
"#
        ))
        .unwrap();
        assert_eq!(
            sources.instructions,
            Some(Path::new("/home/u").join(".codex/AGENTS.md"))
        );
        assert_eq!(sources.skills, Some(skills));
        assert_eq!(sources.skip.len(), 2);
        assert_eq!(sources.mcp.len(), 1);
    }

    #[test]
    fn mistakes_are_errors_not_guesses() {
        let (_x, x) = absolute("x");
        assert!(matches!(
            parsed(&format!("skils = {x}")),
            Err(SourcesError::Parse { .. })
        ));
        assert!(matches!(
            parsed("skills = \"relative\""),
            Err(SourcesError::Relative { .. })
        ));
        assert!(matches!(parsed(""), Err(SourcesError::Empty { .. })));
        assert!(matches!(
            parsed(&format!("skills = {x}\nskip = [\"a/b\"]")),
            Err(SourcesError::Skip { .. })
        ));
        assert!(matches!(
            parse(Path::new("f"), "skills = \"~/x\"", None),
            Err(SourcesError::NoHome { .. })
        ));
    }
}
