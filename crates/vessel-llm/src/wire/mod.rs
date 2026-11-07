//! What each model API says on the wire, typed once at the edge (AGENTS.md
//! section 2): requests, responses, stream events and error bodies of the
//! Anthropic Messages API ([`anthropic`]), the `OpenAI` Responses API
//! ([`responses`]) and the `OpenAI`-compatible Chat Completions API
//! ([`chat`]).
//!
//! [Decision Log]
//! - Purpose: the proxy decodes what a harness sends and what a provider
//!   answers, and encodes the other side. Typed structs make every field
//!   it reads, and every field it writes, a compile-time fact.
//! - Constraint: the APIs grow faster than Vessel; a harness update must
//!   not break the proxy, and the proxy must never read a tool's arguments
//!   or schema (they are the agent's business, and arbitrary).
//! - Alternatives: decoding into `Value` and reading fields ad hoc
//!   (opencodex's way; forbidden here, and every read is a guess at run
//!   time); one set of structs shared by the APIs (they agree on little:
//!   roles, tool calls, reasoning and usage all differ); strict structs
//!   refusing unknown kinds (a Claude Code or Codex release would break
//!   every routed session).
//! - Choice: each struct mirrors its API's field names and JSON types
//!   (token counts and indexes are plain integers here; the intermediate
//!   form and `vessel-proto` give them meaning). Fields the proxy never
//!   inspects (a tool's input schema, a call's input, citations) are
//!   [`RawJson`](vessel_proto::RawJson). Families of kinds (content blocks,
//!   items, stream events) and enumerated strings are open: an unknown kind
//!   or value decodes into an explicit `Unknown` variant (the `open`
//!   module's macros). Fields of a request that a translator cannot carry
//!   to another API are not modelled, so decoding drops them; a same-API
//!   passthrough forwards the original bytes and never re-encodes these
//!   structs, so it loses nothing.
//! - Encoding skips absent optional fields, except where the real API
//!   writes `null` and clients may test for it (a message's `stop_reason`
//!   at its start, a response's `usage` before it completes).
//! - Cost: three parallel sets of structs that follow the providers'
//!   documentation and must follow its changes; a field left unmodelled is
//!   dropped, without a word, when translating.

use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod chat;
pub(crate) mod open;
pub mod responses;

pub use open::TextOr;

/// A payload with no fields of its own (`ping`, `message_stop`, a tool
/// choice of `none`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nothing {}

/// A part that is only text (`text`, `input_text`, `summary_text`,
/// `reasoning_text`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextPart {
    /// The text.
    pub text: String,
}

/// A field an API lets be a mode name or an object: a tool choice of
/// `"auto"` or of one named tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrOr<S, O> {
    /// The string form.
    Str(S),
    /// The object form.
    Object(O),
}

impl<S: Serialize, O: Serialize> Serialize for StrOr<S, O> {
    fn serialize<Ser: serde::Serializer>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error> {
        match self {
            Self::Str(value) => value.serialize(serializer),
            Self::Object(value) => value.serialize(serializer),
        }
    }
}

impl<'de, S: Deserialize<'de>, O: Deserialize<'de>> Deserialize<'de> for StrOr<S, O> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrOrVisitor(std::marker::PhantomData))
    }
}

struct StrOrVisitor<S, O>(std::marker::PhantomData<(S, O)>);

impl<'de, S: Deserialize<'de>, O: Deserialize<'de>> serde::de::Visitor<'de> for StrOrVisitor<S, O> {
    type Value = StrOr<S, O>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a string or an object")
    }

    fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
        S::deserialize(serde::de::value::StrDeserializer::<E>::new(text)).map(StrOr::Str)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        O::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(StrOr::Object)
    }
}
