//! The Vessel envelope: a reasoning block's provider-specific form, wrapped
//! so it can travel through a harness that speaks another API and come back
//! (ADR 0015, decision 8).
//!
//! [Decision Log]
//! - Purpose: Claude Code keeps reasoning only as thinking blocks with a
//!   signature, Codex only as reasoning items with encrypted content. When
//!   a route's provider speaks another API, its reasoning must still go
//!   back to it on the next turn (`OpenAI` refuses a tool loop whose
//!   reasoning is missing; Anthropic refuses one whose thinking is), and
//!   must never go anywhere else.
//! - Choice: the harness's opaque field (a thinking block's `signature`, a
//!   reasoning item's `encrypted_content`) carries
//!   `vsl1:<provider>:<kind>:<payload>`: the node's name for the provider
//!   that made it, the kind of payload, and the payload as the provider
//!   gave it. An upstream replays a block only when the provider and the
//!   kind are its own; every other block is dropped before the call. An
//!   Anthropic upstream drops every block that is not natively Anthropic's
//!   or an envelope of its own, so a `vsl1:` string never reaches it.
//! - Provider names are the node's (`models.toml`); they hold no `:`
//!   (the node checks), so the first `:` after the prefix ends one. A
//!   malformed envelope opens as text from no provider: replayed nowhere.
//! - Cost: the harness's transcript holds the provider's name and its
//!   opaque reasoning, which it held anyway (as the provider's answer).

use super::Replay;

/// What every envelope starts with.
pub const PREFIX: &str = "vsl1:";

/// Whether an opaque field holds an envelope.
#[must_use]
pub fn is_envelope(text: &str) -> bool {
    text.starts_with(PREFIX)
}

/// Wrap `replay`, made by the provider the node calls `source`.
#[must_use]
pub fn seal(source: &str, replay: &Replay) -> String {
    let (kind, payload) = match replay {
        Replay::Text => ("text", ""),
        Replay::AnthropicSignature(signature) => ("anthropic", signature.as_str()),
        Replay::AnthropicRedacted(data) => ("anthropic-redacted", data.as_str()),
        Replay::ResponsesEncrypted(content) => ("responses", content.as_str()),
    };
    format!("{PREFIX}{source}:{kind}:{payload}")
}

/// Unwrap an envelope: the provider that made it and its form. `None` when
/// `text` is not an envelope at all; a malformed one opens as text from no
/// provider.
#[must_use]
pub fn open(text: &str) -> Option<(String, Replay)> {
    let rest = text.strip_prefix(PREFIX)?;
    let opened = rest.split_once(':').and_then(|(source, rest)| {
        let (kind, payload) = rest.split_once(':')?;
        let payload = payload.to_owned();
        let replay = match kind {
            "text" => Replay::Text,
            "anthropic" => Replay::AnthropicSignature(payload),
            "anthropic-redacted" => Replay::AnthropicRedacted(payload),
            "responses" => Replay::ResponsesEncrypted(payload),
            _ => return None,
        };
        Some((source.to_owned(), replay))
    });
    Some(opened.unwrap_or_else(|| (String::new(), Replay::Text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_form_comes_back_as_it_went() {
        for replay in [
            Replay::Text,
            Replay::AnthropicSignature("EqQB/sig+=".into()),
            Replay::AnthropicRedacted("EmwK".into()),
            Replay::ResponsesEncrypted("gAAAAB:with:colons".into()),
        ] {
            let sealed = seal("openrouter", &replay);
            assert!(is_envelope(&sealed));
            assert_eq!(open(&sealed), Some(("openrouter".into(), replay)));
        }
    }

    #[test]
    fn a_native_signature_is_no_envelope_and_a_broken_one_is_from_nowhere() {
        assert_eq!(open("EqQBCkYIARgC"), None);
        assert!(!is_envelope("EqQBCkYIARgC"));
        for broken in ["vsl1:", "vsl1:openrouter", "vsl1:openrouter:mystery:x"] {
            assert_eq!(
                open(broken),
                Some((String::new(), Replay::Text)),
                "{broken}"
            );
        }
    }
}
