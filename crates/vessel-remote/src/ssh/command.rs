//! [`RemoteCommand`]: a line of remote shell text that no value can turn
//! into shell syntax (see the module above for why).

use std::fmt;

use vessel_client::RemotePath;

/// A line of remote shell text, built from fixed words and quoted values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommand {
    line: String,
}

impl RemoteCommand {
    /// A command whose program is a fixed word (`sh`, `uname`).
    #[must_use]
    pub fn program(word: &'static str) -> Self {
        Self {
            line: word.to_owned(),
        }
    }

    /// A command whose program is a file on the remote machine.
    #[must_use]
    pub fn path_program(path: &RemotePath) -> Self {
        Self {
            line: quote_path(path),
        }
    }

    /// Add a fixed word (an option, a subcommand).
    #[must_use]
    pub fn arg(mut self, word: &'static str) -> Self {
        self.line.push(' ');
        self.line.push_str(word);
        self
    }

    /// Add a value, quoted so the remote shell reads it back unchanged.
    #[must_use]
    pub fn value(mut self, text: &str) -> Self {
        self.line.push(' ');
        self.line.push_str(&quote(text));
        self
    }

    /// Add a remote path, quoted (a home-relative one as `"$HOME"/'…'`).
    #[must_use]
    pub fn path(mut self, path: &RemotePath) -> Self {
        self.line.push(' ');
        self.line.push_str(&quote_path(path));
        self
    }

    /// The line as `ssh` sends it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.line
    }
}

impl fmt::Display for RemoteCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.line)
    }
}

/// `text` in single quotes, each `'` written as `'\''`.
fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len().saturating_add(2));
    quoted.push('\'');
    for c in text.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}

fn quote_path(path: &RemotePath) -> String {
    match path.home_relative() {
        Some("") => "\"$HOME\"".to_owned(),
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None => quote(path.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RemotePath {
        text.parse().unwrap()
    }

    #[test]
    fn values_are_single_quoted_and_paths_keep_home() {
        let command = RemoteCommand::path_program(&path("~/.vessel/bin/vesseld-0.2.0"))
            .arg("--data-dir")
            .path(&path("~/.vessel"))
            .arg("attach");
        assert_eq!(
            command.as_str(),
            "\"$HOME\"/'.vessel/bin/vesseld-0.2.0' --data-dir \"$HOME\"/'.vessel' attach"
        );
        let absolute = RemoteCommand::program("ls").path(&path("/srv/it's here"));
        assert_eq!(absolute.as_str(), "ls '/srv/it'\\''s here'");
        assert_eq!(
            RemoteCommand::program("cd").path(&path("~")).as_str(),
            "cd \"$HOME\""
        );
        let hostile = RemoteCommand::program("echo").value("$(rm -rf ~); `x` \"y\" 'z'");
        assert_eq!(
            hostile.as_str(),
            "echo '$(rm -rf ~); `x` \"y\" '\\''z'\\'''"
        );
    }

    /// The quoting, read back by a real `sh`, gives the original text.
    #[cfg(unix)]
    #[test]
    fn sh_reads_quoted_values_back_unchanged() {
        let texts = [
            "plain",
            "it's",
            "''",
            "$HOME `id` $(id) \"q\" \\ ; & | > <",
            "a\tb",
        ];
        for text in texts {
            let line = RemoteCommand::program("printf").value("%s").value(text);
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(line.as_str())
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(output.stdout).unwrap(), text, "{line}");
        }
        let home = RemoteCommand::program("printf")
            .value("%s")
            .path(&path("~/a b/'c'"));
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(home.as_str())
            .env("HOME", "/home/x y")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "/home/x y/a b/'c'"
        );
    }
}
