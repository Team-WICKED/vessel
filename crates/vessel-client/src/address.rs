//! Where a node is: [`NodeAddress`], [`SshTarget`], [`RemotePath`],
//! [`SecureTarget`].
//!
//! Plain data: parsing and checking only, no connecting. The websocket
//! side connects through [`WsConnector`](crate::WsConnector); the SSH side
//! through `vessel-remote`, which turns an [`SshTarget`] into an `ssh`
//! command line.
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
use vessel_proto::{NodePublicKey, RouteUrl};

use crate::url::NodeUrl;

/// How to reach a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeAddress {
    /// Its `/rpc` websocket, with a bearer token.
    Ws(NodeUrl),
    /// `ssh` to the machine, then `vesseld attach` (ADR 0007); no client
    /// token.
    Ssh(SshTarget),
    /// A secure channel to the node's pinned key, directly or through a
    /// relay, authenticated by a device credential (ADR 0009, ADR 0010).
    /// Only the book makes these: they come from pairing, never from text.
    Secure(SecureTarget),
}

/// A node reached over a secure channel: the key pinned at pairing, and the
/// routes to try (direct addresses first, then relays, plan decision 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureTarget {
    /// The node's static key; every handshake checks it.
    pub key: NodePublicKey,
    /// Relays, in the order to try them.
    pub relays: Vec<RouteUrl>,
    /// Addresses to dial directly, tried before the relays.
    pub direct: Vec<RouteUrl>,
}

impl SecureTarget {
    /// Every route, in the order to try them.
    pub fn routes(&self) -> impl Iterator<Item = &RouteUrl> {
        self.direct.iter().chain(&self.relays)
    }
}

impl fmt::Display for SecureTarget {
    /// The key, shortened, and the first route: enough to tell nodes apart
    /// in a list.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = self.key.to_string();
        let short = key.get(..15).unwrap_or(&key);
        write!(formatter, "{short}… ")?;
        let mut routes = self.routes();
        match routes.next() {
            Some(first) => {
                write!(formatter, "via {first}")?;
                let more = routes.count();
                if more > 0 {
                    write!(formatter, " (+{more})")?;
                }
                Ok(())
            }
            None => formatter.write_str("(no route)"),
        }
    }
}

/// Text that names no node address.
#[derive(Debug, thiserror::Error)]
pub enum InvalidAddress {
    /// An `ssh://` URL or `user@host` that is not usable.
    #[error(transparent)]
    Ssh(#[from] InvalidSshTarget),
    /// Not a websocket URL either.
    #[error(transparent)]
    Ws(#[from] crate::url::InvalidNodeUrl),
}

impl FromStr for NodeAddress {
    type Err = InvalidAddress;

    /// `ssh://[user@]host[:port]` is an SSH node; anything else is read as
    /// a websocket URL ([`NodeUrl`]'s lenient forms, so `gpu-box` is
    /// `ws://gpu-box:7077/rpc`). `user@host` without `ssh://` is an SSH
    /// node too: a websocket URL with a user makes no sense, and that is
    /// what people type after `ssh`.
    fn from_str(text: &str) -> Result<Self, InvalidAddress> {
        if text.starts_with(SSH_SCHEME) || (text.contains('@') && !text.contains("://")) {
            return Ok(Self::Ssh(text.parse()?));
        }
        Ok(Self::Ws(text.parse()?))
    }
}

impl fmt::Display for NodeAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ws(url) => url.fmt(formatter),
            Self::Ssh(target) => write!(formatter, "{SSH_SCHEME}{}", target.authority()),
            Self::Secure(target) => target.fmt(formatter),
        }
    }
}

const SSH_SCHEME: &str = "ssh://";

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

/// A path on a remote Unix machine: absolute, or under the remote user's
/// home (`~` or `~/...`). Held as text, since it names a file on another
/// machine and never touches the local file system. No control
/// characters (they would change the remote shell command).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePath(String);

/// Text that is not a [`RemotePath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a remote path (absolute, or starting with `~/`, without control characters)")]
pub struct InvalidRemotePath(String);

impl RemotePath {
    /// The node's default data directory, `~/.vessel`.
    #[must_use]
    pub fn default_data_dir() -> Self {
        Self("~/.vessel".to_owned())
    }

    /// The path as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path below the remote home, without `~/`: `Some("x/y")` for
    /// `~/x/y`, `Some("")` for `~`; `None` for an absolute path.
    #[must_use]
    pub fn home_relative(&self) -> Option<&str> {
        if self.0 == "~" {
            return Some("");
        }
        self.0.strip_prefix("~/")
    }

    /// `segment` below this path. `segment` is a fixed relative path
    /// (`bin/vesseld-0.2.0`), checked like a path.
    ///
    /// # Errors
    /// `segment` is empty, absolute, or has control characters.
    pub fn join(&self, segment: &str) -> Result<Self, InvalidRemotePath> {
        if segment.is_empty() || segment.starts_with('/') || segment.contains(char::is_control) {
            return Err(InvalidRemotePath(segment.to_owned()));
        }
        let mut path = self.0.trim_end_matches('/').to_owned();
        path.push('/');
        path.push_str(segment);
        Ok(Self(path))
    }
}

impl FromStr for RemotePath {
    type Err = InvalidRemotePath;

    fn from_str(text: &str) -> Result<Self, InvalidRemotePath> {
        let rooted = text.starts_with('/') || text == "~" || text.starts_with("~/");
        if !rooted || text.contains(char::is_control) {
            return Err(InvalidRemotePath(text.to_owned()));
        }
        Ok(Self(text.to_owned()))
    }
}

impl fmt::Display for RemotePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
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

    #[test]
    fn node_addresses_tell_ssh_from_websockets() {
        let ssh_url: NodeAddress = "ssh://me@box".parse().unwrap();
        assert!(matches!(ssh_url, NodeAddress::Ssh(_)));
        assert_eq!(ssh_url.to_string(), "ssh://me@box");
        let user_at: NodeAddress = "me@box".parse().unwrap();
        assert!(matches!(user_at, NodeAddress::Ssh(_)));
        let ws: NodeAddress = "gpu-box".parse().unwrap();
        assert_eq!(ws.to_string(), "ws://gpu-box:7077/rpc");
        let http: NodeAddress = "http://box:9000".parse().unwrap();
        assert!(matches!(http, NodeAddress::Ws(_)));
        assert!("ftp://box".parse::<NodeAddress>().is_err());
    }

    #[test]
    fn remote_paths_are_rooted_and_join_fixed_segments() {
        let home: RemotePath = "~/.vessel".parse().unwrap();
        assert_eq!(home, RemotePath::default_data_dir());
        assert_eq!(home.home_relative(), Some(".vessel"));
        assert_eq!(
            home.join("bin/vesseld-0.2.0").unwrap().as_str(),
            "~/.vessel/bin/vesseld-0.2.0"
        );
        let absolute: RemotePath = "/srv/vessel/".parse().unwrap();
        assert_eq!(absolute.home_relative(), None);
        assert_eq!(absolute.join("bin").unwrap().as_str(), "/srv/vessel/bin");
        assert_eq!("~".parse::<RemotePath>().unwrap().home_relative(), Some(""));
        for text in ["relative/dir", "", "~user/dir", "/a\nb", "/tab\there"] {
            assert!(text.parse::<RemotePath>().is_err(), "{text:?}");
        }
        assert!(home.join("/etc").is_err());
        assert!(home.join("").is_err());
        assert!(home.join("a\rb").is_err());
    }
}
