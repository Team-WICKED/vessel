//! Vessel's web UI (ADR 0017): the local server `vessel ui` runs.
//!
//! It serves the single-page app embedded at build time (`build.rs`) on
//! loopback, and is the browser's only way to the fleet: the browser
//! redeems a one-use code ([`LaunchToken`]) for a session with mutual proofs,
//! and never holds a node's credentials or speaks Noise.
//!
//! Modules:
//! - `auth`: one-use bootstrap and role-separated HMAC session proofs;
//! - `transport`: counted TCP admission, bounded headers and shutdown;
//! - `server`: the router, loopback-only serving, the HTTP API;
//! - `guard`: the `Host` and `Origin` checks and the security headers on
//!   every response;
//! - `files`: the embedded files, with the app's routes falling back to
//!   `index.html`;
//! - `socket`: the browser's websocket: the hello within a deadline, then
//!   the bridge;
//! - `bridge`: one tab's calls, streams and ops;
//! - `registry`: one connection per node, shared by every tab, with its
//!   state pushed to them;
//! - [`NodeSource`]: the seam through which the CLI gives the nodes;
//! - [`wire`]: the frames the browser and the server exchange.
//!
//! This crate must not know how a book entry becomes a connection (tokens,
//! SSH, credentials): that is the CLI's, given through [`NodeSource`].

mod auth;
mod bridge;
mod files;
mod guard;
mod registry;
mod server;
mod socket;
mod source;
mod token;
mod transport;
pub mod wire;

pub use server::{ServeError, UiConfig, serve};
pub use source::NodeSource;
pub use token::{LaunchToken, MintError};
