//! Vessel's web UI (ADR 0017): the local server `vessel ui` runs.
//!
//! It serves the single-page app embedded at build time (`build.rs`) on
//! loopback, and is the browser's only way to the fleet: the browser
//! authenticates to it with a token minted per launch ([`LaunchToken`]),
//! and never holds a node's credentials or speaks Noise.
//!
//! Modules:
//! - `server`: the router, loopback-only serving, the HTTP API;
//! - `guard`: the `Host` and `Origin` checks and the security headers on
//!   every response;
//! - `files`: the embedded files, with the app's routes falling back to
//!   `index.html`;
//! - `socket`: the browser's websocket: the hello within a deadline, then
//!   the bridge;
//! - [`wire`]: the frames the browser and the server exchange.
//!
//! This crate must not know how a book entry becomes a connection (tokens,
//! SSH, credentials): that is the CLI's, given through a seam.

mod files;
mod guard;
mod server;
mod socket;
mod token;
pub mod wire;

pub use server::{ServeError, UiConfig, serve};
pub use token::{LaunchToken, MintError};
