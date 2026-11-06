//! A node reached with the system `ssh` client: [`SshTarget`].
//!
//! [Decision Log]
//! - Purpose: the CLI, the node book and the SSH runner all handle "a node
//!   reached over SSH"; one checked type keeps them from each parsing and
//!   validating it their own way.
//! - Constraint: the host, user and paths end up as `ssh` arguments and in
//!   a remote shell command (ADR 0007). A host that starts with `-` would
//!   be read by `ssh` as an option (`-oProxyCommand=...` runs a local
//!   command), and control characters in a remote path would change the
//!   shell command. Those are refused here, at parse time, so no later
//!   layer can forget to.
//! - Choice: an [`SshTarget`] is a host, an optional user and port, and an
//!   optional remote data directory, each checked; it accepts
//!   `[user@]host` (what people type after `ssh`) and
//!   `ssh://[user@]host[:port]` (OpenSSH's URL form). The user's
//!   `~/.ssh/config` still applies, since `ssh` reads it: a `Host` alias
//!   is a valid host here.

use std::fmt;
use std::str::FromStr;

use tokio_tungstenite::tungstenite::http::Uri;

use super::RemotePath;

pub(super) const SSH_SCHEME: &str = "ssh://";

/// A machine reached with the system `ssh` client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// A host name, an address, or a `Host` alias from `~/.ssh/config`.
    /// Never starts with `-`; no spaces or control characters.
    pub host: String,
    /// The login; `None` leaves it to `ssh` (its config, then the local
    /// user name).
    pub user: Option<String>,
    /// The port; `None` leaves it to `ssh`.
    pub port: Option<u16>,
    /// The node's data directory on that machine; `None` is the node's
    /// default (`~/.vessel`).
    pub data_dir: Option<RemotePath>,
}

/// Why text is not an [`SshTarget`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSshTarget {
    /// No host.
    #[error("no host")]
    MissingHost,
    /// A host or user that `ssh` would misread (a leading `-`, spaces,
    /// control characters, `/`).
    #[error("`{0}` is not a usable ssh host or user name")]
    Unsafe(String),
    /// An `ssh://` URL that does not parse, or has a path or a query.
    #[error("`{0}` is not an ssh://[user@]host[:port] URL")]
    Url(String),
}

impl SshTarget {
    /// An SSH target for `host`, with everything else left to `ssh`.
    ///
    /// # Errors
    /// The host is empty or unsafe (see [`SshTarget::host`]).
    pub fn new(host: impl Into<String>) -> Result<Self, InvalidSshTarget> {
        let host = host.into();
        check_word(&host)?;
        Ok(Self {
            host,
            user: None,
            port: None,
            data_dir: None,
        })
    }

    /// `[user@]host[:port]`, with IPv6 hosts in brackets when a port
    /// follows.
    #[must_use]
    pub fn authority(&self) -> String {
        let mut text = String::new();
        if let Some(user) = &self.user {
            text.push_str(user);
            text.push('@');
        }
        match self.port {
            Some(port) if self.host.contains(':') => {
                text.push('[');
                text.push_str(&self.host);
                text.push_str("]:");
                text.push_str(&port.to_string());
            }
            Some(port) => {
                text.push_str(&self.host);
                text.push(':');
                text.push_str(&port.to_string());
            }
            None => text.push_str(&self.host),
        }
        text
    }
}

impl fmt::Display for SshTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.port.is_some() {
            formatter.write_str(SSH_SCHEME)?;
        }
        formatter.write_str(&self.authority())
    }
}

impl FromStr for SshTarget {
    type Err = InvalidSshTarget;

    /// `[user@]host` or `ssh://[user@]host[:port]` (a trailing `/` is
    /// allowed; nothing else after the authority).
    fn from_str(text: &str) -> Result<Self, InvalidSshTarget> {
        if text.starts_with(SSH_SCHEME) {
            return parse_url(text);
        }
        let (user, host) = match text.rsplit_once('@') {
            Some((user, host)) => (Some(user), host),
            None => (None, text),
        };
        let mut target = Self::new(host)?;
        if let Some(user) = user {
            check_word(user)?;
            target.user = Some(user.to_owned());
        }
        Ok(target)
    }
}

fn parse_url(text: &str) -> Result<SshTarget, InvalidSshTarget> {
    let invalid = || InvalidSshTarget::Url(text.to_owned());
    let uri: Uri = text.parse().map_err(|_syntax| invalid())?;
    let path_ok = uri
        .path_and_query()
        .is_none_or(|path| path.as_str().is_empty() || path.as_str() == "/");
    let authority = uri.authority().ok_or_else(invalid)?;
    if !path_ok {
        return Err(invalid());
    }
    let host = authority.host();
    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if bare.is_empty() {
        return Err(InvalidSshTarget::MissingHost);
    }
    let mut target = SshTarget::new(bare)?;
    target.port = authority.port_u16();
    if let Some((user, _host)) = authority.as_str().rsplit_once('@') {
        check_word(user)?;
        target.user = Some(user.to_owned());
    }
    Ok(target)
}

/// A host or user name `ssh` takes as one plain word.
fn check_word(word: &str) -> Result<(), InvalidSshTarget> {
    if word.is_empty() {
        return Err(InvalidSshTarget::MissingHost);
    }
    let unsafe_char = |c: char| c.is_whitespace() || c.is_control() || c == '/' || c == '@';
    if word.starts_with('-') || word.contains(unsafe_char) {
        return Err(InvalidSshTarget::Unsafe(word.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(text: &str) -> SshTarget {
        text.parse().unwrap()
    }

    #[test]
    fn ssh_targets_parse_from_what_people_type() {
        let plain = ssh("gpu-box");
        assert_eq!(
            (plain.host.as_str(), plain.user, plain.port),
            ("gpu-box", None, None)
        );
        let with_user = ssh("me@gpu-box");
        assert_eq!(with_user.user.as_deref(), Some("me"));
        assert_eq!(with_user.to_string(), "me@gpu-box");
        let url = ssh("ssh://me@10.0.0.5:2222");
        assert_eq!(url.host, "10.0.0.5");
        assert_eq!(url.user.as_deref(), Some("me"));
        assert_eq!(url.port, Some(2222));
        assert_eq!(url.to_string(), "ssh://me@10.0.0.5:2222");
        let v6 = ssh("ssh://[::1]:22/");
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.authority(), "[::1]:22");
        assert_eq!(ssh("ssh://box").port, None);
    }

    #[test]
    fn hosts_ssh_would_misread_are_refused() {
        for text in [
            "",
            "-oProxyCommand=evil",
            "me@-oProxyCommand=evil",
            "-l@box",
            "two words",
            "box\nsecond",
            "ssh://",
            "ssh://box/path",
            "ssh://box?x=1",
            "@box",
        ] {
            assert!(text.parse::<SshTarget>().is_err(), "{text:?}");
        }
        assert!(SshTarget::new("-p").is_err());
    }
}
