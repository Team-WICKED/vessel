//! Where a node is: [`NodeAddress`], [`SshTarget`], [`RemotePath`],
//! [`SecureTarget`].
//!
//! Plain data: parsing and checking only, no connecting. The websocket
//! side connects through [`WsConnector`](crate::WsConnector); the SSH side
//! through `vessel-remote`, which turns an [`SshTarget`] into an `ssh`
//! command line.
//!
//! One file per kind of address: `ssh` (an [`SshTarget`] and the checks
//! that keep `ssh` from misreading it), `remote_path` (a data directory on
//! the machine behind it) and `secure` (a paired node's key and routes).

mod remote_path;
mod secure;
mod ssh;

use std::fmt;
use std::str::FromStr;

pub use remote_path::{InvalidRemotePath, RemotePath};
pub use secure::SecureTarget;
use ssh::SSH_SCHEME;
pub use ssh::{InvalidSshTarget, SshTarget};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
