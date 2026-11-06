//! Routes to a paired node and what their failures mean: [`Route`],
//! [`RouteError`], [`ReachError`].
//!
//! The rules are in [`super::connector`]'s decision log; this file holds
//! them as code, so the CLI (one connection) and a resuming stream (many)
//! read the same verdict.

use std::fmt;
use std::time::Duration;

use vessel_proto::{RelayClose, RouteUrl};

use super::SecureConnectError;
use crate::error::chain;

/// One way to a paired node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The node's own address (its `/secure`).
    Direct(RouteUrl),
    /// A relay the node keeps registered with.
    Relay(RouteUrl),
}

impl fmt::Display for Route {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct(url) => write!(formatter, "direct {url}"),
            Self::Relay(url) => write!(formatter, "relay {url}"),
        }
    }
}

/// Why one route gave no connection.
#[derive(Debug, thiserror::Error)]
pub enum RouteError<E> {
    /// The carrier could not be opened.
    #[error("could not connect")]
    Dial {
        /// Why.
        #[source]
        error: E,
        /// Whether the dialer says a later dial may succeed.
        transient: bool,
    },
    /// The carrier did not open in time.
    #[error("no connection within {} s", .0.as_secs())]
    DialTimeout(Duration),
    /// The far end closed with `4401` after the secure handshake: whoever
    /// answered does not hold the key pinned at pairing.
    #[error(
        "whoever answered does not hold this node's key (the node was re-keyed, or another \
         node has this address now); pair it again"
    )]
    WrongKey,
    /// The relay closed the connection with one of its codes.
    #[error("{}", relay_close(*.0))]
    Relay(RelayClose),
    /// The secure channel, the preamble or the protocol handshake failed.
    #[error(transparent)]
    Open(SecureConnectError),
}

impl<E> RouteError<E> {
    /// Whether another attempt over this route cannot help: a wrong key, a
    /// refused device, tampering, a refusal from the relay that waiting
    /// does not lift. A network that failed or was slow may recover.
    #[must_use]
    pub fn is_final(&self) -> bool {
        match self {
            Self::Dial { transient, .. } => !transient,
            Self::DialTimeout(_) => false,
            Self::WrongKey => true,
            Self::Relay(close) => !close.is_transient(),
            Self::Open(error) => error.is_final(),
        }
    }

    /// Whether the node itself answered, after proving its key: every
    /// route leads to the same node, so trying the others is pointless.
    #[must_use]
    pub fn is_answer(&self) -> bool {
        match self {
            Self::Open(error) => error.is_answer(),
            Self::Dial { .. } | Self::DialTimeout(_) | Self::WrongKey | Self::Relay(_) => false,
        }
    }
}

const fn relay_close(close: RelayClose) -> &'static str {
    match close {
        RelayClose::NoSuchNode => {
            "the node is not connected to this relay (it may be restarting or offline)"
        }
        RelayClose::NotAccepted => "the node did not take the connection in time",
        RelayClose::TooLarge => "a message was over the relay's size limit",
        RelayClose::RateLimited => "the relay is limiting connections from here; try again later",
    }
}

/// One route and why it failed.
#[derive(Debug)]
pub struct RouteFailure<E> {
    /// Which route.
    pub route: Route,
    /// Why.
    pub error: RouteError<E>,
}

/// No route gave a connection to the paired node.
#[derive(Debug)]
pub struct ReachError<E> {
    failures: Vec<RouteFailure<E>>,
}

impl<E> ReachError<E> {
    pub(crate) const fn new(failures: Vec<RouteFailure<E>>) -> Self {
        Self { failures }
    }

    /// Every route that was tried, in order. Empty if the node has no
    /// route at all.
    #[must_use]
    pub fn failures(&self) -> &[RouteFailure<E>] {
        &self.failures
    }

    /// Whether trying again cannot help: no route at all, or some route
    /// gave a final verdict (see [`RouteError::is_final`]). A route that
    /// was merely down does not outweigh one that showed a wrong key or a
    /// refused device.
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.failures.is_empty() || self.failures.iter().any(|failure| failure.error.is_final())
    }
}

impl<E: std::error::Error + 'static> fmt::Display for ReachError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.failures.is_empty() {
            return formatter.write_str("the node has no route to try");
        }
        formatter.write_str("could not reach the node")?;
        let mut separator = ": ";
        for failure in &self.failures {
            write!(
                formatter,
                "{separator}{}: {}",
                failure.route,
                chain(&failure.error)
            )?;
            separator = "; ";
        }
        Ok(())
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ReachError<E> {}
