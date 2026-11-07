//! `models.toml`: the providers this node calls and the routes profiles
//! name (ADR 0015, decision 4).
//!
//! ```toml
//! [providers.openrouter]
//! api = "openai-chat-completions"
//! base_url = "https://openrouter.ai/api/v1"
//! key = { secret = "OPENROUTER_API_KEY" }   # a name in secrets.toml
//! reasoning = "openrouter"                  # flags, each optional
//!
//! [models.qwen-coder]
//! provider = "openrouter"
//! model = "qwen/qwen3-coder"
//! small_model = "qwen/qwen3-coder-flash"
//! max_output_tokens = 32000
//! ```
//!
//! A provider's quirks are flags here, named by what they change, never
//! guessed from a model's name (opencodex's lesson): which field bounds the
//! output, how reasoning is asked for, whether reasoning goes back, whether
//! sampling settings are taken. A flag that means nothing for the
//! provider's API is refused, as is any key the file does not know, so a
//! typo is an error, not a silent default.

use std::collections::BTreeMap;

use serde::Deserialize;
use vessel_llm::client::{Api, Url};
use vessel_llm::upstream::chat::{MaxTokensField, ReasoningStyle};
use vessel_proto::{ModelApi, ModelId, ModelRoute, ModelRouteName, ProviderName, SecretName};

/// Why `models.toml` cannot be used. Messages name the provider or route
/// and the key; the file holds no secret, but they quote no value anyway,
/// as `profiles.toml`'s errors do not.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ModelsError {
    /// The file could not be read.
    #[error("cannot read models.toml")]
    Read(#[source] vessel_store::StoreError),
    /// Other users can read it.
    #[error("models.toml is readable by other users; run `chmod 600` on it")]
    NotOwnerOnly,
    /// It is not TOML of the expected shape.
    #[error("models.toml, {position}: {message}")]
    Toml {
        /// Where (line and column), if known.
        position: String,
        /// What is wrong.
        message: String,
    },
    /// A provider is wrong.
    #[error("models.toml, provider `{provider}`: {problem}")]
    Provider {
        /// The provider as written.
        provider: String,
        /// What is wrong.
        problem: String,
    },
    /// A route is wrong.
    #[error("models.toml, model `{route}`: {problem}")]
    Route {
        /// The route as written.
        route: String,
        /// What is wrong.
        problem: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    providers: BTreeMap<String, RawProvider>,
    #[serde(default)]
    models: BTreeMap<String, RawRoute>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    api: String,
    base_url: String,
    #[serde(default)]
    key: Option<RawKey>,
    // Chat Completions.
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    max_tokens_field: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    replay_reasoning: Option<bool>,
    // Responses.
    #[serde(default)]
    sampling: Option<bool>,
    // Anthropic Messages.
    #[serde(default)]
    adaptive_thinking: Option<bool>,
    #[serde(default)]
    default_max_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKey {
    secret: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    provider: String,
    model: String,
    #[serde(default)]
    small_model: Option<String>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
}

/// A provider: where its API is, its key's name, and its quirks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Provider {
    pub(crate) name: ProviderName,
    pub(crate) base_url: Url,
    pub(crate) key: Option<SecretName>,
    pub(crate) flavor: Flavor,
}

/// A provider's API, with the flags that API has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Flavor {
    Anthropic {
        adaptive_thinking: bool,
        default_max_tokens: u32,
    },
    Responses {
        sampling: bool,
    },
    Chat {
        parallel_tool_calls: bool,
        max_tokens_field: MaxTokensField,
        reasoning: ReasoningStyle,
        replay_reasoning: bool,
    },
}

impl Flavor {
    pub(crate) const fn api(&self) -> Api {
        match self {
            Self::Anthropic { .. } => Api::AnthropicMessages,
            Self::Responses { .. } => Api::OpenAiResponses,
            Self::Chat { .. } => Api::OpenAiChat,
        }
    }
}

/// The output bound an Anthropic upstream gets when the harness names none
/// and the provider's `default_max_tokens` does not either.
const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// A route: a provider and its model ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) name: ModelRouteName,
    pub(crate) provider: ProviderName,
    pub(crate) model: ModelId,
    pub(crate) small_model: Option<ModelId>,
    pub(crate) max_output_tokens: Option<u32>,
}

impl Route {
    /// Whether the harness may ask for `model` on this route: the route's
    /// model or its small model, nothing else (ADR 0015, decision 5).
    pub(crate) fn allows(&self, model: &str) -> bool {
        self.model.as_str() == model
            || self
                .small_model
                .as_ref()
                .is_some_and(|small| small.as_str() == model)
    }

    /// The model ids this route allows, for a refusal's message.
    pub(crate) fn allowed(&self) -> String {
        match &self.small_model {
            Some(small) => format!("`{}` or `{small}`", self.model),
            None => format!("`{}`", self.model),
        }
    }
}

/// Everything `models.toml` defines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Routes {
    providers: BTreeMap<ProviderName, Provider>,
    routes: BTreeMap<ModelRouteName, Route>,
}

impl Routes {
    /// The route named `name` and its provider.
    pub(crate) fn get(&self, name: &ModelRouteName) -> Option<(&Route, &Provider)> {
        let route = self.routes.get(name)?;
        let provider = self.providers.get(&route.provider)?;
        Some((route, provider))
    }

    /// Every route as clients see it: no URL, header or secret name.
    pub(crate) fn list(&self) -> Vec<ModelRoute> {
        self.routes
            .values()
            .filter_map(|route| {
                let provider = self.providers.get(&route.provider)?;
                Some(ModelRoute {
                    name: route.name.clone(),
                    provider: provider.name.clone(),
                    api: model_api(provider.flavor.api()),
                    model: route.model.clone(),
                    small_model: route.small_model.clone(),
                })
            })
            .collect()
    }

    /// How many routes there are.
    pub(crate) fn len(&self) -> usize {
        self.routes.len()
    }
}

/// An API in the protocol's terms.
pub(crate) const fn model_api(api: Api) -> ModelApi {
    match api {
        Api::AnthropicMessages => ModelApi::AnthropicMessages,
        Api::OpenAiResponses => ModelApi::OpenAiResponses,
        Api::OpenAiChat => ModelApi::OpenAiChatCompletions,
    }
}

/// Parse `models.toml`.
///
/// # Errors
/// The text is not TOML of the expected shape, or a provider or route is
/// wrong.
pub(crate) fn parse(text: &str) -> Result<Routes, ModelsError> {
    let raw: RawFile = toml_edit::de::from_str(text).map_err(|error| ModelsError::Toml {
        position: error
            .span()
            .map_or_else(|| "somewhere".to_owned(), |span| position(text, span.start)),
        message: error.message().to_owned(),
    })?;
    let mut providers = BTreeMap::new();
    for (written, raw) in raw.providers {
        let fail = |problem: String| ModelsError::Provider {
            provider: written.clone(),
            problem,
        };
        let name: ProviderName = written.parse().map_err(|error| fail(format!("{error}")))?;
        let provider = provider(name.clone(), raw).map_err(fail)?;
        let _new = providers.insert(name, provider);
    }
    let mut routes = BTreeMap::new();
    for (written, raw) in raw.models {
        let fail = |problem: String| ModelsError::Route {
            route: written.clone(),
            problem,
        };
        let name: ModelRouteName = written.parse().map_err(|error| fail(format!("{error}")))?;
        let provider: ProviderName = raw
            .provider
            .parse()
            .map_err(|error| fail(format!("{error}")))?;
        if !providers.contains_key(&provider) {
            return Err(fail(format!("no provider `{provider}` is defined")));
        }
        let model = model_id(&raw.model).map_err(fail)?;
        let small_model = raw
            .small_model
            .as_deref()
            .map(model_id)
            .transpose()
            .map_err(fail)?;
        if raw.max_output_tokens == Some(0) {
            return Err(fail("`max_output_tokens` must be above 0".to_owned()));
        }
        let _new = routes.insert(
            name.clone(),
            Route {
                name,
                provider,
                model,
                small_model,
                max_output_tokens: raw.max_output_tokens,
            },
        );
    }
    Ok(Routes { providers, routes })
}

fn model_id(text: &str) -> Result<ModelId, String> {
    if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
        return Err("a model id is 1-256 characters, none of them control characters".to_owned());
    }
    Ok(ModelId::new(text))
}

fn provider(name: ProviderName, raw: RawProvider) -> Result<Provider, String> {
    let api = Api::from_name(&raw.api).ok_or_else(|| {
        format!(
            "`api` is one of `{}`, `{}` or `{}`",
            Api::AnthropicMessages.name(),
            Api::OpenAiResponses.name(),
            Api::OpenAiChat.name()
        )
    })?;
    let base_url = base_url(&raw.base_url)?;
    let key = raw
        .key
        .map(|key| key.secret.parse::<SecretName>())
        .transpose()
        .map_err(|error| format!("`key.secret`: {error}"))?;
    let only = |flag: &str, set: bool, for_api: Api| {
        if set && api != for_api {
            Err(format!(
                "`{flag}` is only for `api = \"{}\"`",
                for_api.name()
            ))
        } else {
            Ok(())
        }
    };
    only(
        "parallel_tool_calls",
        raw.parallel_tool_calls.is_some(),
        Api::OpenAiChat,
    )?;
    only(
        "max_tokens_field",
        raw.max_tokens_field.is_some(),
        Api::OpenAiChat,
    )?;
    only("reasoning", raw.reasoning.is_some(), Api::OpenAiChat)?;
    only(
        "replay_reasoning",
        raw.replay_reasoning.is_some(),
        Api::OpenAiChat,
    )?;
    only("sampling", raw.sampling.is_some(), Api::OpenAiResponses)?;
    only(
        "adaptive_thinking",
        raw.adaptive_thinking.is_some(),
        Api::AnthropicMessages,
    )?;
    only(
        "default_max_tokens",
        raw.default_max_tokens.is_some(),
        Api::AnthropicMessages,
    )?;
    let flavor = match api {
        Api::AnthropicMessages => Flavor::Anthropic {
            adaptive_thinking: raw.adaptive_thinking.unwrap_or(false),
            default_max_tokens: match raw.default_max_tokens {
                Some(0) => return Err("`default_max_tokens` must be above 0".to_owned()),
                Some(tokens) => tokens,
                None => DEFAULT_MAX_TOKENS,
            },
        },
        Api::OpenAiResponses => Flavor::Responses {
            sampling: raw.sampling.unwrap_or(false),
        },
        Api::OpenAiChat => Flavor::Chat {
            parallel_tool_calls: raw.parallel_tool_calls.unwrap_or(true),
            max_tokens_field: match raw.max_tokens_field.as_deref() {
                None | Some("max_tokens") => MaxTokensField::MaxTokens,
                Some("max_completion_tokens") => MaxTokensField::MaxCompletionTokens,
                Some(_) => {
                    return Err(
                        "`max_tokens_field` is `max_tokens` or `max_completion_tokens`".to_owned(),
                    );
                }
            },
            reasoning: match raw.reasoning.as_deref() {
                None | Some("none") => ReasoningStyle::None,
                Some("effort") => ReasoningStyle::Effort,
                Some("openrouter") => ReasoningStyle::OpenRouter,
                Some(_) => {
                    return Err("`reasoning` is `none`, `effort` or `openrouter`".to_owned());
                }
            },
            replay_reasoning: raw.replay_reasoning.unwrap_or(false),
        },
    };
    Ok(Provider {
        name,
        base_url,
        key,
        flavor,
    })
}

/// A provider's base URL: `http` or `https`, a host, nothing after the
/// path. The proxy connects only where this file says (no SSRF from a
/// request); plain `http` is for local servers (Ollama, vLLM).
fn base_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|error| format!("`base_url`: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("`base_url` is an `http` or `https` URL".to_owned());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("`base_url` names no host".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("`base_url` holds no credentials; use `key`".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("`base_url` has no query or fragment".to_owned());
    }
    Ok(url)
}

/// Line and column of byte `at` in `text`, 1-based.
fn position(text: &str, at: usize) -> String {
    let before = text.get(..at).unwrap_or(text);
    let line = before.matches('\n').count().saturating_add(1);
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |last| last.chars().count())
        .saturating_add(1);
    format!("line {line}, column {column}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[providers.openrouter]
api = "openai-chat-completions"
base_url = "https://openrouter.ai/api/v1"
key = { secret = "OPENROUTER_API_KEY" }
reasoning = "openrouter"
replay_reasoning = true

[providers.anthropic]
api = "anthropic-messages"
base_url = "https://api.anthropic.com"
key = { secret = "ANTHROPIC_API_KEY" }

[providers.local]
api = "openai-chat-completions"
base_url = "http://127.0.0.1:11434/v1"

[models.qwen-coder]
provider = "openrouter"
model = "qwen/qwen3-coder"
small_model = "qwen/qwen3-coder-flash"
max_output_tokens = 32000

[models.sonnet]
provider = "anthropic"
model = "claude-sonnet-4-5"
"#;

    #[test]
    fn the_example_parses() {
        let routes = parse(EXAMPLE).unwrap();
        assert_eq!(routes.len(), 2);
        let (route, provider) = routes.get(&"qwen-coder".parse().unwrap()).unwrap();
        assert!(route.allows("qwen/qwen3-coder"));
        assert!(route.allows("qwen/qwen3-coder-flash"));
        assert!(!route.allows("claude-opus-4-1"));
        assert_eq!(
            route.allowed(),
            "`qwen/qwen3-coder` or `qwen/qwen3-coder-flash`"
        );
        assert_eq!(provider.key, Some("OPENROUTER_API_KEY".parse().unwrap()));
        assert_eq!(
            provider.flavor,
            Flavor::Chat {
                parallel_tool_calls: true,
                max_tokens_field: MaxTokensField::MaxTokens,
                reasoning: ReasoningStyle::OpenRouter,
                replay_reasoning: true,
            }
        );
        // What clients see: no URL, no secret name.
        let listed = serde_json::to_string(&routes.list()).unwrap();
        assert!(!listed.contains("openrouter.ai"), "{listed}");
        assert!(!listed.contains("OPENROUTER_API_KEY"), "{listed}");
        assert!(
            listed.contains(r#""api":"openai_chat_completions""#),
            "{listed}"
        );
        assert_eq!(parse("").unwrap(), Routes::default());
    }

    #[test]
    fn mistakes_are_named_where_they_are() {
        let wrong = |text: &str, says: &str| {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains(says), "{error}");
        };
        wrong(
            "[providers.a]\napi = \"gemini\"\nbase_url = \"https://x\"\n",
            "provider `a`: `api` is one of",
        );
        wrong(
            "[providers.a]\napi = \"openai-responses\"\nbase_url = \"file:///etc\"\n",
            "`base_url` is an `http` or `https` URL",
        );
        wrong(
            "[providers.a]\napi = \"openai-responses\"\nbase_url = \"https://u:p@x\"\n",
            "holds no credentials",
        );
        wrong(
            "[providers.a]\napi = \"openai-responses\"\nbase_url = \"https://x\"\nreasoning = \"effort\"\n",
            "`reasoning` is only for `api = \"openai-chat-completions\"`",
        );
        wrong(
            "[providers.a]\napi = \"openai-responses\"\nbase_url = \"https://x\"\ntypo = 1\n",
            "unknown field `typo`",
        );
        wrong(
            "[providers.\"a:b\"]\napi = \"openai-responses\"\nbase_url = \"https://x\"\n",
            "not a valid model provider name",
        );
        wrong(
            "[models.r]\nprovider = \"missing\"\nmodel = \"m\"\n",
            "model `r`: no provider `missing` is defined",
        );
        wrong("[models.r\n", "line 1");
    }
}
