//! The upstream client: one call to a provider, with timeouts, bounds,
//! retries before anything reached the harness, and failures classified
//! (ADR 0015, decisions 9 and 11).
//!
//! [Decision Log]
//! - Purpose: the proxy's only way out of the node. It holds a provider
//!   key for the length of a call and must neither leak it nor let a slow,
//!   silent or hostile provider hold a session, or the node, forever.
//! - Constraint: the workspace's TLS is rustls with `ring`, never a
//!   process-wide provider (ADR 0005's amendment); upstream URLs come only
//!   from the node owner's `models.toml`, so the client must not be steered
//!   elsewhere (no redirects, no system proxy); a response must be bounded
//!   in time and, when read whole, in size.
//! - Alternatives: a client written on `hyper-util` (pooling, HTTP/2 and
//!   streaming by hand); `reqwest` with its defaults (`aws-lc-rs`, system
//!   proxy, redirects).
//! - Choice: `reqwest` with default features off, the rustls config built
//!   here (`ring`, the operating system's roots, HTTP/2 and HTTP/1.1 by
//!   ALPN) handed to it whole, redirects off, no proxy. A connect timeout,
//!   a first-byte timeout (until the response's headers), and an idle
//!   timeout between chunks of the body, so a provider that stops talking
//!   mid-answer ends the call instead of holding it.
//! - Retries happen only here, before the response's headers, which is
//!   before the proxy has written anything to the harness: on a connection
//!   failure, a timeout, 429, 500, 502, 503, 504 or 529, waiting as the
//!   provider's `Retry-After` says (capped) or backing off. After the
//!   headers, a failure goes to the harness, whose own retry takes over.
//! - Error bodies are read bounded, stripped of the key and cut short
//!   before anything else sees them (a provider may echo it back).
//!
//! [Decision Log] (the security review of milestone 5, findings 2 and 3)
//! - Problem: a provider that echoes the key does it in more places than
//!   an error status: an error event inside a successful (200) stream, a
//!   failed response object with status 200. Those reached the harness
//!   untouched, since an [`Answer`] knew nothing of the key. And an error
//!   body Vessel could not parse was cut to its display length before the
//!   key was replaced, so a key across the cut survived but for its tail.
//! - Choice: a [`Redactor`] for the whole call, held by every [`Answer`]
//!   ([`Answer::redactor`]) for the proxy to apply to everything it writes
//!   from the provider's errors (see `vessel-node`'s proxy); and an error
//!   body is redacted whole (it is bounded, `ERROR_BODY_LIMIT`) before
//!   it is cut. A body over the bound is not read at all.
//! - The key is replaced as it is and as JSON escapes it (`/` as `\/`);
//!   [`Redactor::json`] also finds it inside JSON strings however they
//!   are escaped, by decoding them.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};

use crate::ir::{Failure, FailureClass};
use crate::wire;

/// A provider's base URL (reqwest's, re-exported so callers need not
/// depend on it).
pub use reqwest::Url;

/// An API a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Api {
    /// The Anthropic Messages API.
    AnthropicMessages,
    /// The `OpenAI` Responses API.
    OpenAiResponses,
    /// The `OpenAI`-compatible Chat Completions API.
    OpenAiChat,
}

impl Api {
    /// Its name in `models.toml`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiResponses => "openai-responses",
            Self::OpenAiChat => "openai-chat-completions",
        }
    }

    /// The API `name` names in `models.toml`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Self::AnthropicMessages,
            Self::OpenAiResponses,
            Self::OpenAiChat,
        ]
        .into_iter()
        .find(|api| api.name() == name)
    }

    /// The path of its generating endpoint under a provider's base URL:
    /// Anthropic's base URL is the host (`https://api.anthropic.com`), an
    /// `OpenAI`-style one includes its version (`https://api.openai.com/v1`,
    /// `https://openrouter.ai/api/v1`).
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "/v1/messages",
            Self::OpenAiResponses => "/responses",
            Self::OpenAiChat => "/chat/completions",
        }
    }
}

/// A provider's key: kept out of `Debug`, put only in the authorization
/// header.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key.
    #[must_use]
    pub const fn new(key: String) -> Self {
        Self(key)
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(..)")
    }
}

/// What replaces a key in text a call produced.
const REDACTED: &str = "[redacted]";

/// Removes one call's key from what its provider sent: every error
/// message, inline error and failed response passes through it before
/// anything leaves the proxy. `Debug` shows nothing of the key; a call
/// without a key redacts nothing.
#[derive(Clone, Default)]
pub struct Redactor {
    /// The key as written, and as JSON escapes it when that differs.
    forms: Vec<Arc<str>>,
}

impl Redactor {
    /// A redactor for `key`.
    #[must_use]
    pub fn new(key: Option<&ApiKey>) -> Self {
        let Some(key) = key.filter(|key| !key.0.is_empty()) else {
            return Self::default();
        };
        let mut forms: Vec<Arc<str>> = vec![Arc::from(key.0.as_str())];
        let escaped = key.0.replace('/', "\\/");
        if escaped != key.0 {
            forms.push(Arc::from(escaped.as_str()));
        }
        Self { forms }
    }

    /// Whether `text` holds the key.
    #[must_use]
    pub fn finds(&self, text: &str) -> bool {
        self.forms.iter().any(|form| text.contains(&**form))
    }

    /// `text` with every occurrence of the key replaced.
    #[must_use]
    pub fn text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if !self.finds(text) {
            return Cow::Borrowed(text);
        }
        let mut redacted = text.to_owned();
        for form in &self.forms {
            redacted = redacted.replace(&**form, REDACTED);
        }
        Cow::Owned(redacted)
    }

    /// A failure with its message redacted.
    #[must_use]
    pub fn failure(&self, mut failure: Failure) -> Failure {
        if let Cow::Owned(message) = self.text(&failure.message) {
            failure.message = message;
        }
        failure
    }

    /// JSON text with the key replaced in every string, found after
    /// decoding (so however the provider escaped it), and in the text
    /// itself when it is not JSON. Unchanged text comes back borrowed.
    #[must_use]
    pub fn json<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.forms.is_empty() {
            return Cow::Borrowed(text);
        }
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) else {
            return self.text(text);
        };
        if !self.scrub(&mut value) {
            return self.text(text);
        }
        serde_json::to_string(&value).map_or_else(
            |_never| Cow::Owned(self.text(text).into_owned()),
            Cow::Owned,
        )
    }

    /// Replace the key in every string of `value`; whether any held it.
    fn scrub(&self, value: &mut serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(text) => match self.text(text) {
                Cow::Owned(redacted) => {
                    *text = redacted;
                    true
                }
                Cow::Borrowed(_) => false,
            },
            serde_json::Value::Array(items) => items
                .iter_mut()
                .fold(false, |found, item| self.scrub(item) || found),
            serde_json::Value::Object(members) => members
                .values_mut()
                .fold(false, |found, member| self.scrub(member) || found),
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
                false
            }
        }
    }
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Redactor(..)")
    }
}

/// Where a call goes.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The API.
    pub api: Api,
    /// The provider's base URL, from `models.toml` alone.
    pub base_url: Url,
    /// Its key, if it takes one (a local server may not).
    pub key: Option<ApiKey>,
}

/// The bounds on one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// To establish a connection.
    pub connect: Duration,
    /// From sending the request to the response's headers (a local server
    /// may load its model first).
    pub first_byte: Duration,
    /// Between two chunks of the response's body (opencodex's stall
    /// budget: a reasoning model may think for minutes, sending only
    /// keepalives).
    pub idle: Duration,
    /// A body read whole (an error, a non-streamed answer).
    pub max_body: usize,
    /// Retries before the response's headers.
    pub retries: u32,
    /// The longest `Retry-After` honoured by a retry here; a longer one goes
    /// to the harness.
    pub max_retry_wait: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_byte: Duration::from_secs(300),
            idle: Duration::from_secs(300),
            max_body: 32 * 1024 * 1024,
            retries: 2,
            max_retry_wait: Duration::from_secs(10),
        }
    }
}

/// Why the client could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// The operating system gave no usable root certificate.
    #[error("no root certificates could be loaded from the operating system")]
    NoRoots,
    /// rustls refused the configuration.
    #[error("TLS could not be configured: {0}")]
    Tls(#[source] rustls::Error),
    /// reqwest refused the configuration.
    #[error("the HTTP client could not be built: {0}")]
    Http(#[source] reqwest::Error),
}

/// Calls providers. Cheap to clone (one connection pool).
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    limits: Limits,
}

impl Client {
    /// A client trusting the operating system's root certificates.
    ///
    /// # Errors
    /// No root certificate could be loaded, or the TLS or HTTP
    /// configuration was refused.
    pub fn new(limits: Limits) -> Result<Self, ClientError> {
        let found = rustls_native_certs::load_native_certs();
        let mut roots = rustls::RootCertStore::empty();
        let (_added, _ignored) = roots.add_parsable_certificates(found.certs);
        if roots.is_empty() {
            return Err(ClientError::NoRoots);
        }
        Self::with_roots(roots, limits)
    }

    /// A client trusting exactly `roots`.
    ///
    /// # Errors
    /// The TLS or HTTP configuration was refused.
    pub fn with_roots(roots: rustls::RootCertStore, limits: Limits) -> Result<Self, ClientError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(ClientError::Tls)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(limits.connect)
            .build()
            .map_err(ClientError::Http)?;
        Ok(Self { http, limits })
    }

    /// The bounds on its calls.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Send `body` to `endpoint`, with `headers` from the harness that the
    /// API lets through (Anthropic's version and betas), retrying before
    /// the response's headers. The answer is the response, its body not
    /// yet read.
    ///
    /// # Errors
    /// The provider could not be reached, did not answer in time, or
    /// answered with an error status: a failure classified for the
    /// harness, its message stripped of the key.
    pub async fn send(
        &self,
        endpoint: &Endpoint,
        body: Bytes,
        headers: &[(HeaderName, HeaderValue)],
    ) -> Result<Answer, Failure> {
        let url = url(endpoint);
        let mut attempt = 0_u32;
        loop {
            let failure = match self.attempt(endpoint, &url, body.clone(), headers).await {
                Ok(answer) => return Ok(answer),
                Err(failure) => failure,
            };
            let retryable = matches!(
                failure.class,
                FailureClass::RateLimited
                    | FailureClass::Overloaded
                    | FailureClass::UpstreamError
                    | FailureClass::Timeout
            );
            let wait = failure.retry_after.unwrap_or_else(|| backoff(attempt));
            if !retryable || attempt >= self.limits.retries || wait > self.limits.max_retry_wait {
                return Err(failure);
            }
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(wait).await;
        }
    }

    async fn attempt(
        &self,
        endpoint: &Endpoint,
        url: &Url,
        body: Bytes,
        headers: &[(HeaderName, HeaderValue)],
    ) -> Result<Answer, Failure> {
        let mut request = self
            .http
            .post(url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(key) = &endpoint.key {
            request = match endpoint.api {
                Api::AnthropicMessages => request.header("x-api-key", &key.0),
                Api::OpenAiResponses | Api::OpenAiChat => request.bearer_auth(&key.0),
            };
        }
        if endpoint.api == Api::AnthropicMessages
            && !headers.iter().any(|(name, _)| name == "anthropic-version")
        {
            request = request.header("anthropic-version", ANTHROPIC_VERSION);
        }
        let response = match tokio::time::timeout(self.limits.first_byte, request.send()).await {
            Err(_elapsed) => {
                return Err(Failure::new(
                    FailureClass::Timeout,
                    "the provider did not answer in time",
                ));
            }
            Ok(Err(error)) => return Err(transport_failure(&error)),
            Ok(Ok(response)) => response,
        };
        let status = response.status();
        let redactor = Redactor::new(endpoint.key.as_ref());
        if status.is_success() {
            return Ok(Answer {
                response,
                limits: self.limits,
                redactor,
            });
        }
        let retry_after = retry_after(response.headers());
        let body = Answer {
            response,
            limits: self.limits,
            redactor,
        }
        .read_bounded(ERROR_BODY_LIMIT)
        .await
        .unwrap_or_default();
        let mut failure = status_failure(endpoint, status.as_u16(), &body);
        failure.retry_after = retry_after;
        Err(failure)
    }
}

/// The Anthropic API version sent when the harness gave none.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How much of an error body is read.
const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// How long an error message may be once it leaves the client.
const MESSAGE_LIMIT: usize = 512;

fn url(endpoint: &Endpoint) -> Url {
    let mut url = endpoint.base_url.clone();
    let path = format!(
        "{}{}",
        url.path().trim_end_matches('/'),
        endpoint.api.path()
    );
    url.set_path(&path);
    url
}

/// The wait before retry `attempt` (0-based), when the provider named
/// none: half a second, then a second, then two.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500_u64.saturating_mul(2_u64.saturating_pow(attempt)))
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn transport_failure(error: &reqwest::Error) -> Failure {
    let class = if error.is_timeout() {
        FailureClass::Timeout
    } else {
        FailureClass::UpstreamError
    };
    let what = if error.is_connect() {
        "the provider could not be reached"
    } else {
        "the connection to the provider failed"
    };
    Failure::new(class, what)
}

/// An error status as a failure, its message taken from the API's error
/// body where it has one, else the body itself: whole (it is bounded),
/// redacted, then cut (finding 3: cut first, a key across the cut would
/// no longer match).
fn status_failure(endpoint: &Endpoint, status: u16, body: &[u8]) -> Failure {
    let message = error_message(endpoint.api, body)
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    let message = redact(&message, endpoint.key.as_ref());
    let class = match status {
        401..=403 => FailureClass::Unauthorized,
        408 | 504 => FailureClass::Timeout,
        429 => FailureClass::RateLimited,
        503 | 529 => FailureClass::Overloaded,
        500..=599 => FailureClass::UpstreamError,
        _ if looks_like_context_overflow(&message) => FailureClass::ContextTooLong,
        _ => FailureClass::RefusedRequest,
    };
    let message = if message.is_empty() {
        format!("the provider answered {status}")
    } else {
        format!("the provider answered {status}: {message}")
    };
    Failure::new(class, message)
}

fn error_message(api: Api, body: &[u8]) -> Option<String> {
    match api {
        Api::AnthropicMessages => serde_json::from_slice::<wire::anthropic::ErrorBody>(body)
            .ok()
            .map(|body| body.error.message),
        Api::OpenAiResponses => serde_json::from_slice::<wire::responses::ErrorBody>(body)
            .ok()
            .map(|body| body.error.message),
        Api::OpenAiChat => serde_json::from_slice::<wire::chat::ErrorBody>(body)
            .ok()
            .map(|body| body.error.message),
    }
}

/// Whether a provider's refusal says the conversation is too long (each
/// says it its own way).
fn looks_like_context_overflow(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "prompt is too long",
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "too many tokens",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

/// `message` with the key removed, then cut to a bounded length.
fn redact(message: &str, key: Option<&ApiKey>) -> String {
    let mut message = Redactor::new(key).text(message).into_owned();
    if message.len() > MESSAGE_LIMIT {
        let mut end = MESSAGE_LIMIT;
        while !message.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        message.truncate(end);
        message.push('…');
    }
    message
}

/// A provider's successful response, its body not yet read.
#[derive(Debug)]
pub struct Answer {
    response: reqwest::Response,
    limits: Limits,
    redactor: Redactor,
}

impl Answer {
    /// What removes the call's key from what the provider sent: a
    /// successful status does not mean the body holds no error.
    #[must_use]
    pub fn redactor(&self) -> Redactor {
        self.redactor.clone()
    }

    /// The next chunk of the body; `None` at its end.
    ///
    /// # Errors
    /// The provider was silent for longer than the idle timeout, or the
    /// connection failed.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Failure> {
        match tokio::time::timeout(self.limits.idle, self.response.chunk()).await {
            Err(_elapsed) => Err(Failure::new(
                FailureClass::Timeout,
                "the provider went silent mid-answer",
            )),
            Ok(Err(error)) => Err(transport_failure(&error)),
            Ok(Ok(chunk)) => Ok(chunk),
        }
    }

    /// The whole body, for a non-streamed answer.
    ///
    /// # Errors
    /// As for [`chunk`](Self::chunk), or the body is larger than the
    /// client's bound.
    pub async fn body(self) -> Result<Bytes, Failure> {
        let limit = self.limits.max_body;
        self.read_bounded(limit).await
    }

    async fn read_bounded(mut self, limit: usize) -> Result<Bytes, Failure> {
        let mut body = BytesMut::new();
        while let Some(chunk) = self.chunk().await? {
            if body.len().saturating_add(chunk.len()) > limit {
                return Err(Failure::new(
                    FailureClass::UpstreamError,
                    format!("the provider's answer is larger than {limit} bytes"),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(api: Api, base: &str) -> Endpoint {
        Endpoint {
            api,
            base_url: Url::parse(base).unwrap(),
            key: Some(ApiKey::new("sk-secret-key".into())),
        }
    }

    #[test]
    fn paths_join_each_api_s_base() {
        assert_eq!(
            url(&endpoint(
                Api::AnthropicMessages,
                "https://api.anthropic.com"
            ))
            .as_str(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            url(&endpoint(Api::OpenAiChat, "https://openrouter.ai/api/v1/")).as_str(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            url(&endpoint(Api::OpenAiResponses, "http://127.0.0.1:8080/v1")).as_str(),
            "http://127.0.0.1:8080/v1/responses"
        );
        for api in [
            Api::AnthropicMessages,
            Api::OpenAiResponses,
            Api::OpenAiChat,
        ] {
            assert_eq!(Api::from_name(api.name()), Some(api));
        }
        assert_eq!(Api::from_name("gemini"), None);
    }

    #[test]
    fn statuses_and_bodies_become_classes_without_the_key() {
        let anthropic = endpoint(Api::AnthropicMessages, "https://a");
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;
        assert_eq!(
            status_failure(&anthropic, 400, body).class,
            FailureClass::ContextTooLong
        );
        let echoed = br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key sk-secret-key"}}"#;
        let failure = status_failure(&anthropic, 401, echoed);
        assert_eq!(failure.class, FailureClass::Unauthorized);
        assert!(!failure.message.contains("sk-secret-key"), "{failure:?}");
        assert!(failure.message.contains("[redacted]"));

        let chat = endpoint(Api::OpenAiChat, "https://c");
        let numeric = br#"{"error":{"message":"This model's maximum context length is 32768 tokens","code":400}}"#;
        assert_eq!(
            status_failure(&chat, 400, numeric).class,
            FailureClass::ContextTooLong
        );
        assert_eq!(
            status_failure(&chat, 429, b"").class,
            FailureClass::RateLimited
        );
        assert_eq!(
            status_failure(&chat, 529, b"").class,
            FailureClass::Overloaded
        );
        assert_eq!(
            status_failure(&chat, 502, b"<html>").class,
            FailureClass::UpstreamError
        );
        assert_eq!(
            status_failure(&chat, 404, b"").class,
            FailureClass::RefusedRequest
        );

        let long = "é".repeat(MESSAGE_LIMIT);
        let cut = redact(&long, None);
        assert!(cut.len() <= MESSAGE_LIMIT.saturating_add('…'.len_utf8()));
        assert!(
            format!("{:?}", ApiKey::new("sk-secret-key".into()))
                .find("sk-")
                .is_none()
        );
    }

    /// The security review's finding 3: an error body Vessel cannot parse
    /// is redacted whole before it is cut, so a key across the cut does
    /// not survive minus its tail.
    #[test]
    fn an_untyped_body_is_redacted_before_it_is_cut() {
        let chat = endpoint(Api::OpenAiChat, "https://c");
        let key = "sk-secret-key";
        for before in [MESSAGE_LIMIT.saturating_sub(5), MESSAGE_LIMIT, 1, 0] {
            let body = format!("{}{key} trailing", "x".repeat(before));
            let failure = status_failure(&chat, 401, body.as_bytes());
            assert!(!failure.message.contains("sk-secret"), "{failure:?}");
            assert!(!failure.message.contains("sk-sec"), "{failure:?}");
        }
    }

    /// One call's redactor: its key as written, as JSON escapes a `/`, and
    /// inside JSON strings however escaped; nothing without a key.
    #[test]
    fn a_redactor_finds_the_key_in_every_form() {
        let redactor = Redactor::new(Some(&ApiKey::new("sk-a/b-key".into())));
        assert_eq!(redactor.text("bad key sk-a/b-key."), "bad key [redacted].");
        assert_eq!(
            redactor.text(r"bad key sk-a\/b-key."),
            "bad key [redacted]."
        );
        assert!(matches!(redactor.text("nothing"), Cow::Borrowed("nothing")));
        let escaped = r#"{"error":{"message":"bad \u0073k-a\/b-key","code":7}}"#;
        let json = redactor.json(escaped);
        assert!(!json.contains("u0073k"), "{json}");
        assert!(json.contains("[redacted]"), "{json}");
        assert!(json.contains(r#""code":7"#), "{json}");
        assert!(matches!(redactor.json(r#"{"ok":true}"#), Cow::Borrowed(_)));
        assert_eq!(redactor.json("not json sk-a/b-key"), "not json [redacted]");
        let failure = redactor.failure(Failure::new(FailureClass::UpstreamError, "sk-a/b-key"));
        assert_eq!(failure.message, "[redacted]");
        assert_eq!(format!("{redactor:?}"), "Redactor(..)");
        assert_eq!(Redactor::new(None).text("sk-a/b-key"), "sk-a/b-key");
    }

    #[test]
    fn backoff_doubles() {
        assert_eq!(backoff(0), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(u32::MAX), Duration::from_millis(u64::MAX));
    }
}
