//! [`Pinned`]: a connector that only accepts one node.
//!
//! [Decision Log]
//! - Purpose: the book records a node's id on first contact (ADR 0007,
//!   the plan's C5). Later connections to the same name must reach the
//!   same node: a reinstalled machine, a reused address or a DNS change
//!   would otherwise be sent the user's commands and keystrokes. A
//!   reconnect is the same question, asked without the user watching.
//! - Alternatives: check the id in each connector (the SSH one and the
//!   websocket one would each repeat it); check it once in the CLI after
//!   the first connection (a [`ResumingStream`](crate::ResumingStream)
//!   redials on its own and would skip the check).
//! - Choice: a wrapper around any [`Connector`]. It knows the expected id
//!   from the book, or takes the first one it sees, and refuses any other
//!   with an error that is not transient, so a resuming stream stops
//!   instead of retrying. Writing a newly seen id into the book is the
//!   caller's job ([`Pinned::pinned`]); this crate's connectors do not
//!   touch files.

use std::sync::OnceLock;

use vessel_proto::{ClientInfo, NodeId};

use crate::client::NodeClient;
use crate::connector::Connector;

/// Connects through `C`, then refuses any node but the pinned one.
#[derive(Debug)]
pub struct Pinned<C> {
    inner: C,
    id: OnceLock<NodeId>,
}

/// Why a [`Pinned`] connection failed.
#[derive(Debug, thiserror::Error)]
pub enum PinnedError<E> {
    /// The inner connector failed.
    #[error(transparent)]
    Connect(E),
    /// A different node answered.
    #[error(
        "a different node answered (id {found}, expected {expected}); if the machine was \
         reinstalled on purpose, remove the node's `id` from your book"
    )]
    Changed {
        /// The pinned id.
        expected: NodeId,
        /// The id of the node that answered.
        found: NodeId,
    },
}

impl<C> Pinned<C> {
    /// Accept only `expected`; with `None`, accept the first node that
    /// answers and only that one afterwards.
    #[must_use]
    pub fn new(inner: C, expected: Option<NodeId>) -> Self {
        let id = OnceLock::new();
        if let Some(expected) = expected {
            let _set = id.set(expected);
        }
        Self { inner, id }
    }

    /// The pinned id: the expected one, or the first one seen.
    #[must_use]
    pub fn pinned(&self) -> Option<NodeId> {
        self.id.get().copied()
    }

    /// The wrapped connector.
    #[must_use]
    pub const fn inner(&self) -> &C {
        &self.inner
    }
}

impl<C: Connector> Connector for Pinned<C> {
    type Error = PinnedError<C::Error>;

    async fn connect(&self, client: ClientInfo) -> Result<NodeClient, Self::Error> {
        let connection = self
            .inner
            .connect(client)
            .await
            .map_err(PinnedError::Connect)?;
        let found = connection.node().id;
        let expected = *self.id.get_or_init(|| found);
        if expected != found {
            tracing::warn!(%expected, %found, "a different node answered; refusing it");
            return Err(PinnedError::Changed { expected, found });
        }
        Ok(connection)
    }

    fn is_transient(&self, error: &Self::Error) -> bool {
        match error {
            PinnedError::Connect(error) => self.inner.is_transient(error),
            PinnedError::Changed { .. } => false,
        }
    }
}
