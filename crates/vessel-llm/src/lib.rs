//! Model APIs for the node's routing proxy (milestone 5,
//! `docs/plans/backend-m5.md`): what the Anthropic Messages, `OpenAI`
//! Responses and `OpenAI` Chat Completions APIs say on the wire, how their
//! streams are framed, and how one is translated into another.
//!
//! This crate owns model APIs and nothing about Vessel's sessions, nodes or
//! harnesses: who may call the proxy, with which route and key, and where
//! usage is recorded belong to `vessel-node`; how a harness is pointed at
//! the proxy belongs to `vessel-harness`. So everything here can be tested
//! alone, against recorded streams.
//!
//! Layout:
//!
//! - [`sse`]: Server-Sent Events, decoded incrementally from any chunking
//!   and encoded back.
//! - [`wire`]: each API's requests, responses, stream events and errors,
//!   typed.
//! - [`ir`]: the neutral request and stream every translation goes
//!   through.
//! - [`front`]: the APIs harnesses speak, into and out of the neutral form.
//! - [`upstream`]: the APIs providers speak, out of and into it.
//! - [`passthrough`]: the same API on both sides, the request as the
//!   harness wrote it, less what decision 8 of ADR 0015 forbids.
//! - [`error`]: what translation refuses, and what it leaves out.

pub mod error;
#[cfg(test)]
mod fixtures;
pub mod front;
pub mod ir;
#[cfg(test)]
mod matrix;
pub mod passthrough;
pub mod sse;
pub mod upstream;
pub mod wire;
