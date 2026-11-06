//! What an `ssh` run produces, and how it fails.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Lines of stderr kept for error messages.
pub const STDERR_LINES: usize = 64;

/// `ssh`'s exit code for its own failures (it could not connect, log in,
/// or lost the connection).
pub const SSH_FAILED: i32 = 255;

/// The last [`STDERR_LINES`] lines a command wrote to stderr.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StderrTail {
    lines: VecDeque<String>,
}

impl StderrTail {
    pub(super) fn push(&mut self, line: String) {
        if self.lines.len() >= STDERR_LINES {
            let _oldest = self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The kept lines, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    /// The last non-empty line, usually the one that says what failed.
    #[must_use]
    pub fn last(&self) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
    }

    /// Whether any kept line contains `text`.
    #[must_use]
    pub fn contains(&self, text: &str) -> bool {
        self.lines.iter().any(|line| line.contains(text))
    }
}

/// Where a run's stdin comes from.
#[derive(Debug, Clone, Copy)]
pub enum Input<'a> {
    /// Nothing (stdin is closed at once).
    Empty,
    /// These bytes.
    Bytes(&'a [u8]),
    /// This local file, streamed.
    File(&'a Path),
}

/// A finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// Its exit code; `None` if `ssh` was killed by a signal.
    pub code: Option<i32>,
    /// What it printed (at most 1 MiB).
    pub stdout: Vec<u8>,
    /// The end of what it, or `ssh`, wrote to stderr.
    pub stderr: StderrTail,
}

impl Finished {
    /// Whether it exited 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Stdout as text (lossy).
    #[must_use]
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// `Ok` if it exited 0, else why not.
    ///
    /// # Errors
    /// [`Failed`] with the exit code and the stderr tail.
    pub fn check(self) -> Result<Self, Failed> {
        if self.success() {
            Ok(self)
        } else {
            Err(Failed {
                code: self.code,
                stderr: self.stderr,
            })
        }
    }
}

/// A command that did not exit 0.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", describe(*code, stderr))]
pub struct Failed {
    /// The exit code; 255 is `ssh` itself.
    pub code: Option<i32>,
    /// The end of stderr.
    pub stderr: StderrTail,
}

fn describe(code: Option<i32>, stderr: &StderrTail) -> String {
    let what = match code {
        Some(SSH_FAILED) => "ssh failed".to_owned(),
        Some(code) => format!("the remote command exited {code}"),
        None => "ssh was killed".to_owned(),
    };
    match stderr.last() {
        Some(line) => format!("{what}: {line}"),
        None => what,
    }
}

/// Why `ssh` could not be run at all.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    /// The `ssh` program could not be started.
    #[error("could not run `{}` (is the OpenSSH client installed?)", program.display())]
    Spawn {
        /// What was run.
        program: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },
    /// Reading its output, or waiting for it, failed.
    #[error("talking to ssh failed")]
    Io(#[source] std::io::Error),
    /// A file to send could not be read.
    #[error("cannot read {}", path.display())]
    Input {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stderr_tail_keeps_the_last_lines() {
        let mut tail = StderrTail::default();
        for index in 0..100 {
            tail.push(format!("line {index}"));
        }
        tail.push(String::new());
        assert_eq!(tail.lines().count(), STDERR_LINES);
        assert_eq!(tail.lines().next(), Some("line 37"));
        assert_eq!(tail.last(), Some("line 99"));
        assert!(tail.contains("line 50"));
        let failed = Failed {
            code: Some(SSH_FAILED),
            stderr: tail,
        };
        assert_eq!(failed.to_string(), "ssh failed: line 99");
    }
}
