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

#[cfg(test)]
mod fixtures;
pub mod sse;
pub mod wire;
