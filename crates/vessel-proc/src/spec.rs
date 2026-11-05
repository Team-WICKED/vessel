//! [`CommandSpec`]: a command as plain data.

use std::ffi::OsString;
use std::path::PathBuf;

/// A command to run, as plain data.
///
/// Built by `vessel-harness` and executed by [`spawn`](crate::spawn) or by
/// `vessel-pty`. Keeping it inert data (instead of a `std::process::Command`)
/// lets launch decisions be unit-tested without starting anything, and lets
/// different runners (pipes, PTY) execute the same description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable to run: an absolute path resolved by the caller, so the
    /// runner never searches `PATH` itself.
    pub program: PathBuf,
    /// Arguments, passed verbatim.
    pub args: Vec<OsString>,
    /// Changes to the inherited environment, applied in order (a later entry
    /// for the same name wins).
    pub env: Vec<(OsString, EnvValue)>,
    /// Working directory; `None` inherits the caller's.
    pub cwd: Option<PathBuf>,
}

/// One change to an inherited environment variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    /// Set the variable to this value.
    Set(OsString),
    /// Remove the variable.
    Remove,
}

impl CommandSpec {
    /// A command with no arguments, no environment changes and the inherited
    /// working directory.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// Append one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable.
    #[must_use]
    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((name.into(), EnvValue::Set(value.into())));
        self
    }

    /// Remove an inherited environment variable.
    #[must_use]
    pub fn env_remove(mut self, name: impl Into<OsString>) -> Self {
        self.env.push((name.into(), EnvValue::Remove));
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// The change this spec makes to the variable `name`, if any (the last
    /// entry for that name, since later entries win).
    #[must_use]
    pub fn env_change(&self, name: &str) -> Option<&EnvValue> {
        self.env
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// The equivalent `std::process::Command`, with stdio left at defaults.
    #[must_use]
    pub fn to_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        let _ = command.args(&self.args);
        for (name, value) in &self.env {
            let _ = match value {
                EnvValue::Set(value) => command.env(name, value),
                EnvValue::Remove => command.env_remove(name),
            };
        }
        if let Some(cwd) = &self.cwd {
            let _ = command.current_dir(cwd);
        }
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_env_entries_win() {
        let spec = CommandSpec::new("/bin/true")
            .env("A", "1")
            .env_remove("A")
            .env("B", "2");
        assert_eq!(spec.env_change("A"), Some(&EnvValue::Remove));
        assert_eq!(spec.env_change("B"), Some(&EnvValue::Set("2".into())));
        assert_eq!(spec.env_change("C"), None);
    }

    #[test]
    fn builder_collects_arguments_in_order() {
        let spec = CommandSpec::new("/bin/echo")
            .arg("a")
            .args(["b", "c"])
            .cwd("/tmp");
        assert_eq!(spec.args, ["a", "b", "c"]);
        assert_eq!(spec.cwd, Some(PathBuf::from("/tmp")));
    }
}
