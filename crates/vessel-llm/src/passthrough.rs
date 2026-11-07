//! The passthrough: the harness and the provider speak the same API
//! (Claude Code to Anthropic, Codex to `OpenAI` Responses), so the request
//! goes through as the harness wrote it and the answer as the provider
//! wrote it. Only the authorization changes, which is the node's business;
//! the answer is read alongside for usage (the upstream's own decoder,
//! [`crate::upstream`]).
//!
//! [Decision Log]
//! - Purpose: the common case (each harness on its own vendor's route)
//!   must lose nothing: cache breakpoints, server tools, beta fields,
//!   signatures, whatever the API adds next.
//! - Constraint: one change is required (ADR 0015, decision 8): a session
//!   that ran on another route has reasoning from another provider in its
//!   history, and it must not reach this one. A `vsl1:` envelope must
//!   never reach Anthropic (it refuses the request, and the reasoning is
//!   another provider's) nor `OpenAI` (it refuses the encrypted content).
//! - Alternatives: decode the whole request into the wire types and write
//!   it again (drops every field Vessel does not model); edit the bytes
//!   with string search (one escaped quote away from corrupting a
//!   prompt).
//! - Choice: read the request as an object of raw members
//!   ([`RawObject`]); decode only the members this needs (`model`,
//!   `stream`, the conversation's blocks or items) into typed probes; if
//!   nothing must change, forward the original bytes untouched; if
//!   something must, drop exactly those blocks or items and write the
//!   object again, every other member as it came (whitespace aside).
//! - Anthropic also refuses a thinking request whose last assistant turn
//!   calls a tool without starting with thinking, which is what a history
//!   from another provider looks like once its envelopes are gone; such a
//!   request goes without `thinking` (the model answers that turn without
//!   extended thinking, and the next turn has it again).
//! - Cost: a native signature names no provider, so two providers with the
//!   same API (Anthropic and a proxy in front of it) cannot tell each
//!   other's apart; the second refuses the first's, as it would without
//!   Vessel.
//!
//! [Decision Log] (the security review of milestone 5, finding 1)
//! - Problem: JSON lets an object name a member twice, and readers
//!   disagree on which one counts. The proxy read the first `model` and
//!   forwarded the bytes; a provider reading the last one ran a model the
//!   route does not allow, under the node's key. A thinking block with two
//!   `signature`s failed its probe, and a block that could not be probed
//!   was kept, so a `vsl1:` envelope reached Anthropic.
//! - Choice: every object whose members decide admission or stripping
//!   (the request, its messages, an assistant turn's blocks, its input
//!   items) is read as a [`RawObject`], which refuses a member named twice
//!   (after unescaping, as every reader compares names); and a block or
//!   item that cannot be probed refuses the request instead of passing.
//!   Members Vessel does not look at stay opaque: an object nested in a
//!   tool's input may repeat a name, since nothing here decides by it.
//! - Stripping looks for `vsl1:` anywhere in a signature or a redacted
//!   block's data, not only at its start: neither Anthropic's signatures
//!   nor `OpenAI`'s encrypted content (base64) can hold a `:`.

use std::collections::HashSet;
use std::fmt;

use bytes::Bytes;
use serde::de::{DeserializeOwned, Error as _, MapAccess, Visitor};
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use vessel_proto::RawJson;

use crate::ir::envelope;
use crate::wire::TextOr;
use crate::wire::open::open_str;

/// Why a request cannot be passed through.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PassthroughError {
    /// The body is not a JSON object, or names a member twice.
    #[error("the request body is not a JSON object Vessel can read: {0}")]
    NotAnObject(serde_json::Error),
    /// A member the proxy needs is missing or of the wrong type.
    #[error("the request's `{member}` is missing or malformed")]
    Member {
        /// The member.
        member: &'static str,
    },
    /// Writing the request again failed (it does not).
    #[error("encoding JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// A request read for passing through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passed {
    /// The model it asks for.
    pub model: String,
    /// Whether it asks for a stream.
    pub stream: bool,
    /// The body to send: the harness's own bytes, unless something had to
    /// go.
    pub body: Bytes,
    /// Blocks or items removed, for the node's log.
    pub removed: usize,
    /// Whether `thinking` was removed (Anthropic only).
    pub thinking_removed: bool,
}

/// A JSON object as its members, in order, each kept as raw JSON: the
/// form in which a passthrough reads a request without dropping what it
/// does not know. A member is read by decoding it into a typed struct.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawObject(Vec<(String, RawJson)>);

impl RawObject {
    /// Decode member `key` into `T`: `None` if absent.
    ///
    /// # Errors
    /// The member is not a `T`.
    #[must_use]
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<Result<T, serde_json::Error>> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| serde_json::from_str(value.as_text()))
    }

    /// Replace member `key`, or add it.
    pub fn set(&mut self, key: &str, value: RawJson) {
        match self.0.iter_mut().find(|(name, _)| name == key) {
            Some((_, slot)) => *slot = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }

    /// Remove member `key`; whether it was there.
    #[must_use = "whether the member was there"]
    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|(name, _)| name != key);
        self.0.len() != before
    }
}

impl Serialize for RawObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for RawObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(RawObjectVisitor)
    }
}

struct RawObjectVisitor;

impl<'de> Visitor<'de> for RawObjectVisitor {
    type Value = RawObject;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawObject, A::Error> {
        let mut members = Vec::with_capacity(map.size_hint().unwrap_or(0).min(64));
        let mut names = HashSet::new();
        while let Some((name, value)) = map.next_entry::<String, RawJson>()? {
            // Readers disagree on which of two same-named members counts
            // (see the module's second Decision Log), so neither does.
            if !names.insert(name.clone()) {
                return Err(A::Error::custom(format_args!(
                    "the member `{name}` appears twice"
                )));
            }
            members.push((name, value));
        }
        Ok(RawObject(members))
    }
}

fn member<T: DeserializeOwned>(
    object: &RawObject,
    name: &'static str,
) -> Result<T, PassthroughError> {
    object
        .get(name)
        .and_then(Result::ok)
        .ok_or(PassthroughError::Member { member: name })
}

fn optional<T: DeserializeOwned>(
    object: &RawObject,
    name: &'static str,
) -> Result<Option<T>, PassthroughError> {
    object
        .get(name)
        .transpose()
        .map_err(|_malformed| PassthroughError::Member { member: name })
}

open_str! {
    /// The content blocks a passthrough looks at.
    enum BlockKind {
        "thinking" => Thinking,
        "redacted_thinking" => RedactedThinking,
        "tool_use" => ToolUse,
    }
}

/// What a passthrough reads of a content block: its kind, and the opaque
/// field that says whose reasoning it is (a thinking block's `signature`,
/// a redacted one's `data`).
struct BlockProbe {
    kind: BlockKind,
    opaque: Option<String>,
}

/// Probe an assistant turn's block, refusing one that is not an object
/// with distinct members, or whose fields are of the wrong type: a block
/// that cannot be read is never forwarded unread.
fn block_probe(block: &RawJson) -> Result<BlockProbe, PassthroughError> {
    let malformed = || PassthroughError::Member { member: "content" };
    let object: RawObject = serde_json::from_str(block.as_text()).map_err(|_bad| malformed())?;
    let kind: BlockKind = object
        .get("type")
        .and_then(Result::ok)
        .ok_or_else(malformed)?;
    let opaque = match kind {
        BlockKind::Thinking => optional::<String>(&object, "signature")?,
        BlockKind::RedactedThinking => optional::<String>(&object, "data")?,
        BlockKind::ToolUse | BlockKind::Unknown(_) => None,
    };
    Ok(BlockProbe { kind, opaque })
}

open_str! {
    /// The kinds of thinking a request asks for.
    enum ThinkingKind {
        "disabled" => Disabled,
    }
}

/// What a passthrough reads of a request's `thinking`.
#[derive(Deserialize)]
struct ThinkingProbe {
    #[serde(rename = "type")]
    kind: ThinkingKind,
}

/// Claude Code's request, for an Anthropic upstream: every thinking block
/// whose signature is not Anthropic's (a Vessel envelope, or none) is
/// removed, and `thinking` goes when the last assistant turn would then
/// break Anthropic's rule for it.
///
/// # Errors
/// The body is not an object, or its `model`, `stream` or `messages` are
/// malformed.
pub fn anthropic(body: Bytes) -> Result<Passed, PassthroughError> {
    let mut object: RawObject =
        serde_json::from_slice(&body).map_err(PassthroughError::NotAnObject)?;
    let model: String = member(&object, "model")?;
    let stream = optional::<bool>(&object, "stream")?.unwrap_or(false);
    let mut messages: Vec<RawObject> = optional(&object, "messages")?.unwrap_or_default();

    let mut removed = 0_usize;
    for message in &mut messages {
        if optional::<String>(message, "role")?.as_deref() != Some("assistant") {
            continue;
        }
        let Some(TextOr::Parts(blocks)) = optional::<TextOr<RawJson>>(message, "content")? else {
            continue;
        };
        let before = blocks.len();
        let mut kept: Vec<RawJson> = Vec::with_capacity(before);
        for block in blocks {
            if !foreign_thinking(&block_probe(&block)?) {
                kept.push(block);
            }
        }
        if kept.len() != before {
            removed = removed.saturating_add(before.saturating_sub(kept.len()));
            message.set("content", RawJson::from_serialize(&kept)?);
        }
    }

    let thinking_on = optional::<ThinkingProbe>(&object, "thinking")?
        .is_some_and(|thinking| thinking.kind != ThinkingKind::Disabled);
    let thinking_removed = thinking_on && last_turn_calls_without_thinking(&messages)?;
    if removed == 0 && !thinking_removed {
        return Ok(Passed {
            model,
            stream,
            body,
            removed,
            thinking_removed,
        });
    }
    if removed > 0 {
        object.set("messages", RawJson::from_serialize(&messages)?);
    }
    if thinking_removed {
        let _was_there = object.remove("thinking");
    }
    Ok(Passed {
        model,
        stream,
        body: Bytes::from(serde_json::to_vec(&object)?),
        removed,
        thinking_removed,
    })
}

/// A thinking block Anthropic did not sign, or a redacted one it did not
/// make: a Vessel envelope (or one inside its opaque field), or none.
fn foreign_thinking(probe: &BlockProbe) -> bool {
    matches!(
        probe.kind,
        BlockKind::Thinking | BlockKind::RedactedThinking
    ) && probe
        .opaque
        .as_deref()
        .is_none_or(|opaque| opaque.is_empty() || envelope::mentions(opaque))
}

/// Whether the last assistant turn calls a tool without starting with
/// thinking.
fn last_turn_calls_without_thinking(messages: &[RawObject]) -> Result<bool, PassthroughError> {
    for message in messages.iter().rev() {
        if optional::<String>(message, "role")?.as_deref() != Some("assistant") {
            continue;
        }
        let Some(TextOr::Parts(blocks)) = optional::<TextOr<RawJson>>(message, "content")? else {
            return Ok(false);
        };
        let kinds = blocks
            .iter()
            .map(|block| block_probe(block).map(|probe| probe.kind))
            .collect::<Result<Vec<BlockKind>, _>>()?;
        let calls = kinds.contains(&BlockKind::ToolUse);
        let starts_thinking = matches!(
            kinds.first(),
            Some(BlockKind::Thinking | BlockKind::RedactedThinking)
        );
        return Ok(calls && !starts_thinking);
    }
    Ok(false)
}

open_str! {
    /// The input items a passthrough looks at.
    enum ItemKind {
        "reasoning" => Reasoning,
    }
}

/// Whether an input item is reasoning whose encrypted content is a Vessel
/// envelope (or holds one), refusing an item that is not an object with
/// distinct members or whose fields are of the wrong type.
fn foreign_reasoning(item: &RawJson) -> Result<bool, PassthroughError> {
    let malformed = || PassthroughError::Member { member: "input" };
    let object: RawObject = serde_json::from_str(item.as_text()).map_err(|_bad| malformed())?;
    let kind = optional::<ItemKind>(&object, "type")?;
    if kind != Some(ItemKind::Reasoning) {
        return Ok(false);
    }
    let content = optional::<String>(&object, "encrypted_content")?;
    Ok(content.is_some_and(|content| envelope::mentions(&content)))
}

/// Codex's request, for a Responses upstream: every reasoning item whose
/// encrypted content is a Vessel envelope is removed.
///
/// # Errors
/// The body is not an object, or its `model`, `stream` or `input` are
/// malformed.
pub fn responses(body: Bytes) -> Result<Passed, PassthroughError> {
    let mut object: RawObject =
        serde_json::from_slice(&body).map_err(PassthroughError::NotAnObject)?;
    let model: String = member(&object, "model")?;
    let stream = optional::<bool>(&object, "stream")?.unwrap_or(false);
    let Some(TextOr::Parts(items)) = optional::<TextOr<RawJson>>(&object, "input")? else {
        return Ok(Passed {
            model,
            stream,
            body,
            removed: 0,
            thinking_removed: false,
        });
    };
    let before = items.len();
    let mut kept: Vec<RawJson> = Vec::with_capacity(before);
    for item in items {
        if !foreign_reasoning(&item)? {
            kept.push(item);
        }
    }
    let removed = before.saturating_sub(kept.len());
    if removed == 0 {
        return Ok(Passed {
            model,
            stream,
            body,
            removed,
            thinking_removed: false,
        });
    }
    object.set("input", RawJson::from_serialize(&kept)?);
    Ok(Passed {
        model,
        stream,
        body: Bytes::from(serde_json::to_vec(&object)?),
        removed,
        thinking_removed: false,
    })
}

#[cfg(test)]
mod tests;
