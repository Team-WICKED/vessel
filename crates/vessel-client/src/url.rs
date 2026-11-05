//! Where a node's RPC endpoint is.

use std::fmt;
use std::str::FromStr;

use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::http::uri::{
    Authority, InvalidUri, InvalidUriParts, Parts, PathAndQuery, Scheme,
};
use vessel_proto::DEFAULT_PORT;

/// The websocket URL of a node's `/rpc` endpoint, always `ws://` or
/// `wss://` with a host.
///
/// Parsed leniently from what people type, normalised once:
///
/// | input | URL |
/// | --- | --- |
/// | `gpu-box` | `ws://gpu-box:7077/rpc` |
/// | `127.0.0.1:9000` | `ws://127.0.0.1:9000/rpc` |
/// | `http://host:7077` | `ws://host:7077/rpc` |
/// | `https://host` | `wss://host/rpc` |
/// | `ws://host:7077/rpc` | unchanged |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeUrl(Uri);

/// Text that does not name a node endpoint.
#[derive(Debug, thiserror::Error)]
pub enum InvalidNodeUrl {
    /// Not a URL at all.
    #[error("not a URL")]
    Syntax(#[from] InvalidUri),
    /// A URL, but its parts do not combine into a websocket URL.
    #[error("not a usable URL")]
    Parts(#[from] InvalidUriParts),
    /// A scheme other than `ws`, `wss`, `http` or `https`.
    #[error("unsupported scheme `{0}` (use ws, wss, http or https)")]
    Scheme(String),
    /// No host.
    #[error("the URL names no host")]
    MissingHost,
}

impl NodeUrl {
    /// The URL.
    #[must_use]
    pub const fn as_uri(&self) -> &Uri {
        &self.0
    }

    /// Whether the connection is encrypted (`wss`).
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.0.scheme_str() == Some("wss")
    }
}

impl FromStr for NodeUrl {
    type Err = InvalidNodeUrl;

    fn from_str(text: &str) -> Result<Self, InvalidNodeUrl> {
        let text = text.trim();
        let bare = !text.contains("://");
        let uri: Uri = if bare {
            format!("ws://{text}").parse()?
        } else {
            text.parse()?
        };
        let Parts {
            scheme,
            authority,
            path_and_query,
            ..
        } = uri.into_parts();
        let scheme = match scheme.as_ref().map(Scheme::as_str) {
            Some("ws" | "http") => "ws",
            Some("wss" | "https") => "wss",
            Some(other) => return Err(InvalidNodeUrl::Scheme(other.to_owned())),
            None => return Err(InvalidNodeUrl::MissingHost),
        };
        let mut authority = authority.ok_or(InvalidNodeUrl::MissingHost)?;
        if authority.host().is_empty() {
            return Err(InvalidNodeUrl::MissingHost);
        }
        // A bare host means a Vessel node on its default port; an explicit
        // scheme without a port means that scheme's port (a proxy, say).
        if bare && authority.port().is_none() {
            authority = format!("{}:{DEFAULT_PORT}", authority.host()).parse::<Authority>()?;
        }
        let path_and_query = match path_and_query {
            Some(path) if path.path() != "/" => path,
            Some(_) | None => PathAndQuery::from_static("/rpc"),
        };
        let mut parts = Parts::default();
        parts.scheme = Some(scheme.parse()?);
        parts.authority = Some(authority);
        parts.path_and_query = Some(path_and_query);
        Ok(Self(Uri::from_parts(parts)?))
    }
}

impl fmt::Display for NodeUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> String {
        text.parse::<NodeUrl>().unwrap().to_string()
    }

    #[test]
    fn normalises_what_people_type() {
        assert_eq!(url("gpu-box"), "ws://gpu-box:7077/rpc");
        assert_eq!(url(" 127.0.0.1:9000 "), "ws://127.0.0.1:9000/rpc");
        assert_eq!(url("[::1]"), "ws://[::1]:7077/rpc");
        assert_eq!(url("http://host:7077"), "ws://host:7077/rpc");
        assert_eq!(url("http://host:7077/"), "ws://host:7077/rpc");
        assert_eq!(url("https://host"), "wss://host/rpc");
        assert_eq!(url("ws://host:1/custom"), "ws://host:1/custom");
        assert!("wss://host".parse::<NodeUrl>().unwrap().is_tls());
    }

    #[test]
    fn rejects_other_schemes_and_hostless_urls() {
        assert!(matches!(
            "ftp://host".parse::<NodeUrl>(),
            Err(InvalidNodeUrl::Scheme(scheme)) if scheme == "ftp"
        ));
        assert!("ws://".parse::<NodeUrl>().is_err());
        assert!("".parse::<NodeUrl>().is_err());
    }
}
