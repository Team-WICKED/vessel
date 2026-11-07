//! Translating a [`CommandSpec`] into `portable-pty`'s command type.
//!
//! The only place PTY-specific launch policy lives (today: the default
//! `TERM`). Must not spawn anything.

use portable_pty::CommandBuilder;
use vessel_proc::{CommandSpec, EnvValue};

/// `TERM` given to programs whose spec does not mention it.
///
/// The node's own `TERM` (if it has one at all, which a service usually
/// does not) describes the node's terminal, not the clients'. Every client
/// terminal Vessel targets implements xterm's 256-colour feature set.
const DEFAULT_TERM: &str = "xterm-256color";

/// The `portable-pty` command for `spec`.
///
/// `portable-pty` starts from the node's environment, so `spec.env` is
/// applied as changes on top of it, in order, like `vessel_proc::spawn`
/// does.
pub(crate) fn command_builder(spec: &CommandSpec) -> CommandBuilder {
    let mut builder = CommandBuilder::new(&spec.program);
    builder.args(&spec.args);
    for (name, value) in &spec.env {
        match value {
            EnvValue::Set(value) => builder.env(name, value),
            EnvValue::Remove => builder.env_remove(name),
        }
    }
    if spec.env_change("TERM").is_none() {
        builder.env("TERM", DEFAULT_TERM);
    }
    if let Some(cwd) = &spec.cwd {
        builder.cwd(cwd);
    }
    builder
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn term_defaults_only_when_the_spec_is_silent() {
        let silent = command_builder(&CommandSpec::new("/bin/true"));
        assert_eq!(silent.get_env("TERM"), Some(OsStr::new(DEFAULT_TERM)));

        let set = command_builder(&CommandSpec::new("/bin/true").env("TERM", "dumb"));
        assert_eq!(set.get_env("TERM"), Some(OsStr::new("dumb")));

        let removed = command_builder(&CommandSpec::new("/bin/true").env_remove("TERM"));
        assert_eq!(removed.get_env("TERM"), None);
    }

    #[test]
    fn arguments_and_directory_are_carried_over() {
        let builder = command_builder(&CommandSpec::new("/bin/echo").args(["a", "b"]).cwd("/tmp"));
        assert_eq!(builder.get_argv(), &["/bin/echo", "a", "b"]);
        assert_eq!(
            builder.get_cwd().map(std::ffi::OsString::as_os_str),
            Some(OsStr::new("/tmp"))
        );
    }
}
