//! Model routing (protocol 7, milestone 5, ADR 0015): what clients see of
//! the routes a node offers and of the model calls a session makes.
//!
//! [Decision Log]
//! - Purpose: a session on a routed profile talks to a provider through the
//!   node's proxy; its owner should see which model answered, how many
//!   tokens each call took, and why a call failed, without the proxy ever
//!   putting a prompt, a completion or a key where a client can read it.
//! - Constraint: usage must add up the same way whatever API the provider
//!   speaks: Anthropic counts cached input apart from input, `OpenAI` counts
//!   it inside; `OpenAI` reports reasoning tokens inside output, Anthropic
//!   not at all.
//! - Choice: one convention, fixed here and applied by the proxy at each
//!   API's edge: [`TokenUsage::input`] excludes cached tokens,
//!   [`TokenUsage::reasoning`] is part of [`TokenUsage::output`]. A call is
//!   one [`ModelCall`] event in the session's log; failures are a closed
//!   set of classes ([`ModelFailure`]), never the provider's message.
//! - Routes cross the wire by name, provider, API and model ids
//!   ([`ModelRoute`]); base URLs, headers and secret names stay on the node.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::id::ModelId;
use crate::name::{ModelRouteName, ProviderName};

/// An API a provider or a harness speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelApi {
    /// The Anthropic Messages API (Claude Code's).
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    /// The `OpenAI` Responses API (Codex's).
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// The `OpenAI`-compatible Chat Completions API (`OpenRouter`, Ollama,
    /// vLLM, ...).
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
}

/// A number of tokens.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TokenCount(u64);

impl TokenCount {
    /// `count` tokens.
    #[must_use]
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The sum, saturating.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

/// The tokens one call used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens neither read from nor written to a prompt cache.
    #[serde(default)]
    pub input: TokenCount,
    /// Input tokens read from a prompt cache.
    #[serde(default)]
    pub cache_read: TokenCount,
    /// Input tokens written to a prompt cache.
    #[serde(default)]
    pub cache_write: TokenCount,
    /// Output tokens, reasoning included.
    #[serde(default)]
    pub output: TokenCount,
    /// Of the output, the tokens spent reasoning, where the provider says.
    #[serde(default)]
    pub reasoning: TokenCount,
}

impl TokenUsage {
    /// Every input token, cached or not.
    #[must_use]
    pub const fn total_input(&self) -> TokenCount {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// How a model call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStop {
    /// The model finished its turn.
    EndTurn,
    /// The model called tools.
    ToolUse,
    /// The model reached its output bound.
    MaxTokens,
    /// The model met a stop sequence.
    StopSequence,
    /// The model refused, or a content filter stopped it.
    Refusal,
    /// A long provider-side turn paused, to be continued.
    Pause,
    /// The call failed (see [`ModelCall::failure`]).
    Error,
}

/// Why a model call failed: its class, never the provider's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFailure {
    /// The provider limits the rate.
    RateLimited,
    /// The provider is overloaded.
    Overloaded,
    /// The provider failed otherwise.
    UpstreamError,
    /// The provider refused the request as it is.
    RefusedRequest,
    /// The conversation outgrew the model's context window.
    ContextTooLong,
    /// The provider did not answer in time, or went silent.
    Timeout,
    /// The answer was cut short.
    Truncated,
    /// The provider refused the node's key.
    Unauthorized,
}

impl ModelFailure {
    /// Its name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Overloaded => "overloaded",
            Self::UpstreamError => "upstream_error",
            Self::RefusedRequest => "refused_request",
            Self::ContextTooLong => "context_too_long",
            Self::Timeout => "timeout",
            Self::Truncated => "truncated",
            Self::Unauthorized => "unauthorized",
        }
    }
}

/// A duration in whole milliseconds.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct DurationMs(u64);

impl DurationMs {
    /// `duration`, in whole milliseconds (saturating).
    #[must_use]
    pub fn from_duration(duration: Duration) -> Self {
        Self(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
    }

    /// The milliseconds.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// As a [`Duration`].
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        Duration::from_millis(self.0)
    }
}

/// One call a session made to a model through the node's proxy: metadata
/// only, never prompt or completion content, tool arguments or keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCall {
    /// The route the session runs on.
    pub route: ModelRouteName,
    /// The provider's model that was called.
    pub model: ModelId,
    /// The API the provider speaks.
    pub api: ModelApi,
    /// The tokens it used (zero where the provider said nothing).
    #[serde(default)]
    pub usage: TokenUsage,
    /// How it ended.
    pub stop: ModelStop,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ModelFailure>,
    /// From the proxy's first byte out to the answer's end.
    pub duration_ms: DurationMs,
}

/// The route a session was launched with (protocol 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModel {
    /// The route.
    pub route: ModelRouteName,
    /// The provider's model the harness is pinned to.
    pub model: ModelId,
}

/// `model.list` (protocol 7): the routes this node offers, for pickers and
/// `vessel models`. Needs `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListModels;

/// A route a node offers (`model.list`, protocol 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRoute {
    /// Its name, which profiles use.
    pub name: ModelRouteName,
    /// The provider it calls.
    pub provider: ProviderName,
    /// The API that provider speaks.
    pub api: ModelApi,
    /// The provider's model.
    pub model: ModelId,
    /// The provider's model for a harness's small, fast calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<ModelId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_reads_as_its_wire_form() {
        let call = ModelCall {
            route: "qwen-coder".parse().unwrap(),
            model: ModelId::new("qwen/qwen3-coder"),
            api: ModelApi::OpenAiChatCompletions,
            usage: TokenUsage {
                input: TokenCount::new(72),
                cache_read: TokenCount::new(128),
                output: TokenCount::new(50),
                reasoning: TokenCount::new(10),
                ..TokenUsage::default()
            },
            stop: ModelStop::ToolUse,
            failure: None,
            duration_ms: DurationMs::from_duration(Duration::from_millis(1234)),
        };
        let text = serde_json::to_string(&call).unwrap();
        assert_eq!(
            text,
            r#"{"route":"qwen-coder","model":"qwen/qwen3-coder","api":"openai_chat_completions","usage":{"input":72,"cache_read":128,"cache_write":0,"output":50,"reasoning":10},"stop":"tool_use","duration_ms":1234}"#
        );
        assert_eq!(serde_json::from_str::<ModelCall>(&text).unwrap(), call);
        assert_eq!(call.usage.total_input(), TokenCount::new(200));
    }

    #[test]
    fn route_and_provider_names_take_no_colon() {
        assert!("openrouter".parse::<ProviderName>().is_ok());
        assert!("open:router".parse::<ProviderName>().is_err());
        assert!("qwen-coder.fast".parse::<ModelRouteName>().is_ok());
        assert!("".parse::<ModelRouteName>().is_err());
        assert_eq!(
            DurationMs::from_duration(Duration::MAX),
            DurationMs(u64::MAX)
        );
    }
}
