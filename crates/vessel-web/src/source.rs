//! [`NodeSource`]: where the bridge's nodes come from.
//!
//! [Decision Log]
//! - Purpose: the browser names nodes; something must turn a name into a
//!   connection. That needs the CLI's knowledge: the book, token files,
//!   SSH, device credentials, pinned ids (ADR 0017: the browser never
//!   holds any of it).
//! - Alternatives: the bridge resolving book entries itself (it would
//!   duplicate `vessel-cli`'s connect logic and need `vessel-remote`); a
//!   boxed `dyn` source (one production implementation and test fakes
//!   need no dynamic dispatch, AGENTS.md section 2).
//! - Choice: a small generic trait. `vessel-cli` implements it with the
//!   book and its `NodeConnector`; the bridge's tests with in-process
//!   nodes. The bridge only routes: what a node allows is the node's to
//!   decide.

use std::future::Future;

use vessel_client::Connector;
use vessel_proto::NodeId;

use crate::wire::{BookNode, NodeKey};

/// The nodes `vessel ui` offers, and how to reach each.
pub trait NodeSource: Send + Sync + 'static {
    /// Opens connections to one node.
    type Connector: Connector + 'static;
    /// Why the source could not answer (a book that does not parse, no
    /// such node, a missing token file). Shown to the user as text.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The nodes to list, in order.
    fn nodes(&self) -> impl Future<Output = Result<Vec<BookNode>, Self::Error>> + Send;

    /// How to reach `node`.
    fn connector(
        &self,
        node: &NodeKey,
    ) -> impl Future<Output = Result<Self::Connector, Self::Error>> + Send;

    /// `node` answered as `id`: a book node seen for the first time gets
    /// its id pinned, as the CLI does on first contact.
    fn seen(&self, node: &NodeKey, id: NodeId) -> impl Future<Output = ()> + Send;

    /// Pair with the node `link` offers and add it to the book as `name`;
    /// the node as the book has it then.
    fn pair(
        &self,
        name: &str,
        link: &str,
    ) -> impl Future<Output = Result<BookNode, Self::Error>> + Send;
}
