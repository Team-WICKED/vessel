//! [`launch`]: turn a session spec into the command that runs it.

use std::path::PathBuf;

use vessel_proc::CommandSpec;
use vessel_proto::{SessionMode, SessionSpec};

use crate::error::HarnessError;
use crate::harnesses::{Structured, descriptor};
use crate::lookup::Lookup;

/// The command that runs `spec`.
///
/// Terminal mode runs the harness binary with the spec's arguments. Structured
/// mode runs whatever speaks ACP for that harness: the binary itself with its
/// ACP arguments, or a separate adapter (which needs only the adapter to be
/// installed). The working directory defaults to the node user's home
/// directory and must exist: running an agent in a different directory than
/// asked is worse than refusing.
///
/// Does filesystem lookups; call it from a blocking context.
///
/// # Errors
/// [`HarnessError::NotInstalled`] or [`HarnessError::StructuredUnavailable`]
/// when the program is missing, [`HarnessError::NoSuchDirectory`] for a bad
/// working directory.
pub fn launch(lookup: &Lookup, spec: &SessionSpec) -> Result<CommandSpec, HarnessError> {
    let harness = spec.harness;
    let descriptor = descriptor(harness);
    let installed = |binary: &'static str| {
        lookup
            .find(binary)
            .ok_or(HarnessError::NotInstalled { harness, binary })
    };
    let command = match &spec.mode {
        SessionMode::Terminal { args, .. } => {
            CommandSpec::new(installed(descriptor.binary)?).args(args)
        }
        SessionMode::Structured { .. } => match &descriptor.structured {
            Structured::Native { args } => {
                CommandSpec::new(installed(descriptor.binary)?).args(args.iter())
            }
            Structured::Adapter { binary, install } => {
                let program =
                    lookup
                        .find(binary)
                        .ok_or_else(|| HarnessError::StructuredUnavailable {
                            harness,
                            reason: adapter_missing(binary, install),
                        })?;
                CommandSpec::new(program)
            }
        },
    };
    Ok(match working_dir(spec)? {
        Some(cwd) => command.cwd(cwd),
        None => command,
    })
}

/// Why structured mode is unavailable when an adapter is missing.
pub(crate) fn adapter_missing(binary: &str, install: &str) -> String {
    format!("its ACP adapter `{binary}` is not installed (install it with `{install}`)")
}

/// The spec's directory, or the home directory; `None` only when the node
/// user has no home directory, in which case the node's own is inherited.
fn working_dir(spec: &SessionSpec) -> Result<Option<PathBuf>, HarnessError> {
    let Some(cwd) = spec.cwd.clone().or_else(dirs::home_dir) else {
        return Ok(None);
    };
    if cwd.is_dir() {
        Ok(Some(cwd))
    } else {
        Err(HarnessError::NoSuchDirectory { cwd })
    }
}
