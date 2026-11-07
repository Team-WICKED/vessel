//! The node's model proxy: a listener on `127.0.0.1` that routed sessions'
//! harnesses call instead of their vendor (ADR 0015).
//!
//! [Decision Log]
//! - Purpose: a harness's model call reaches the route's provider in the
//!   provider's API, its answer comes back in the harness's, and the call
//!   is recorded in the session's log.
//! - Who may call: loopback only; a request with an `Origin` header is
//!   refused (a browser page aimed at the port sends one, a harness does
//!   not); then a live session's token (`Authorization: Bearer`, or
//!   `x-api-key` for the Anthropic front), or 401 with nothing of the
//!   request logged. The token decides the session and the route; nothing
//!   in a request decides where the proxy connects.
//! - The model a harness asks for must be its route's model or small model
//!   (decision 5): the harness is pinned to them, so another name is a
//!   mistake, refused with both names, never quietly replaced.
//! - Same API on both sides: the request's own bytes go up (less what
//!   decision 8 forbids, `vessel_llm::passthrough`) and the answer's bytes
//!   come down, read alongside for usage. Different APIs: the request is
//!   translated through the neutral form, the upstream always streams, and
//!   the answer is written in the harness's API, with keepalives while the
//!   provider is silent ([`KEEPALIVE`]).
//! - Streaming: a task reads the provider and writes into a small bounded
//!   channel the response body drains; a harness that stops reading slows
//!   the task, one that goes away ends it (and the upstream call).
//! - Every call, finished or failed, becomes one `ModelCall` record, and
//!   one log line: route, model, outcome, tokens, time; never content.
//! - Not built here: compressed request bodies (Codex compresses only for
//!   its built-in `openai` provider, which a routed session does not use)
//!   are refused; `count_tokens` is answered with an estimate (a quarter
//!   of the request's bytes) rather than a provider call.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use futures_util::StreamExt as _;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_util::sync::CancellationToken;
use vessel_llm::client::{Answer, Api, Endpoint};
use vessel_llm::sse::{SseDecoder, SseEvent, SseFrame, encode_comment, encode_event};
use vessel_llm::wire::{anthropic, responses};
use vessel_llm::{front, ir, passthrough, upstream};
use vessel_proto::{
    DurationMs, ModelCall, ModelFailure, ModelId, ModelStop, TokenCount, TokenUsage,
};

use super::Models;
use super::config::{Flavor, Provider, Route, model_api};
use super::token::Holder;

/// How long the proxy lets a harness wait without a byte before it sends a
/// keepalive (Claude Code and Codex give up on a silent connection).
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Bytes waiting between the task reading the provider and the response.
const PIPE: usize = 32;

/// Serve the proxy on `listener` until `stop`.
pub(crate) async fn serve(models: Arc<Models>, listener: TcpListener, stop: CancellationToken) {
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(responses_call))
        .route("/v1/models", get(list_models))
        .fallback(fallback)
        // A GET with `Upgrade: websocket` on `/v1/responses` is Codex's
        // WebSocket transport: it must get the fallback's 426, not 405.
        .method_not_allowed_fallback(fallback)
        .layer(DefaultBodyLimit::max(super::MAX_REQUEST))
        .with_state(models);
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await;
    if let Err(error) = served {
        tracing::warn!(%error, "the model proxy stopped");
    }
}

/// The API a harness speaks to the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Front {
    Anthropic,
    Responses,
}

impl Front {
    const fn api(self) -> Api {
        match self {
            Self::Anthropic => Api::AnthropicMessages,
            Self::Responses => Api::OpenAiResponses,
        }
    }
}

/// Why the proxy answers a request itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    Unauthorized,
    Forbidden,
    NotFound,
    Unsupported,
    Invalid,
    Busy,
    Upgrade,
}

impl Refusal {
    const fn status(self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            Self::Upgrade => StatusCode::UPGRADE_REQUIRED,
        }
    }
}

/// The proxy's own answer, in `front`'s error shape.
fn refuse(front: Front, refusal: Refusal, message: &str) -> Response {
    let body = match front {
        Front::Anthropic => {
            let kind = match refusal {
                Refusal::Unauthorized => anthropic::ErrorKind::Authentication,
                Refusal::Forbidden => anthropic::ErrorKind::Permission,
                Refusal::NotFound => anthropic::ErrorKind::NotFound,
                Refusal::Busy => anthropic::ErrorKind::RateLimit,
                Refusal::Unsupported | Refusal::Invalid | Refusal::Upgrade => {
                    anthropic::ErrorKind::InvalidRequest
                }
            };
            json(&anthropic::ErrorBody {
                tag: anthropic::ErrorTag::Error,
                error: anthropic::ErrorDetail {
                    kind,
                    message: message.to_owned(),
                },
            })
        }
        Front::Responses => {
            let mut body = front::responses::refusal(message.to_owned());
            body.error.kind = Some(match refusal {
                Refusal::Unauthorized => responses::ErrorType::Authentication,
                Refusal::Busy => responses::ErrorType::RateLimit,
                Refusal::Forbidden
                | Refusal::NotFound
                | Refusal::Unsupported
                | Refusal::Invalid
                | Refusal::Upgrade => responses::ErrorType::InvalidRequest,
            });
            json(&body)
        }
    };
    let mut response = (refusal.status(), body).into_response();
    if refusal == Refusal::Busy {
        let _replaced = response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}

/// `value` as a JSON body.
fn json<T: Serialize>(value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(_never) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// A request let in: whose it is, its route, and its places in the limits.
struct Admitted {
    holder: Holder,
    route: Route,
    provider: Provider,
    slots: Slots,
}

/// A call's places in the session's and the node's limits, given back when
/// its record is written.
type Slots = (OwnedSemaphorePermit, OwnedSemaphorePermit);

/// The checks every model call passes, in order: no `Origin`, no
/// compressed body, a live session's token, a route still defined, room in
/// the limits.
fn admit(models: &Models, headers: &HeaderMap, front: Front) -> Result<Admitted, Box<Response>> {
    if headers.contains_key(header::ORIGIN) {
        tracing::info!("refused a model call with an Origin header");
        return Err(Box::new(refuse(
            front,
            Refusal::Forbidden,
            "requests from web pages are refused",
        )));
    }
    if headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding.as_bytes() != b"identity")
    {
        return Err(Box::new(refuse(
            front,
            Refusal::Unsupported,
            "compressed request bodies are not accepted",
        )));
    }
    let presented = token(headers);
    let Some(holder) = presented.and_then(|token| models.holder(token)) else {
        tracing::info!("refused a model call without a live session's token");
        return Err(Box::new(refuse(
            front,
            Refusal::Unauthorized,
            "this proxy takes a running Vessel session's model token",
        )));
    };
    let Some((route, provider)) = models.route(&holder.route) else {
        return Err(Box::new(refuse(
            front,
            Refusal::Invalid,
            &format!(
                "the session's model route `{}` is no longer in this node's models.toml",
                holder.route
            ),
        )));
    };
    let Some(slots) = models.slot(holder.session.id()) else {
        tracing::info!(session = %holder.session.id(), "refused a model call past the limits");
        return Err(Box::new(refuse(
            front,
            Refusal::Busy,
            "too many model calls at once on this session or node",
        )));
    };
    Ok(Admitted {
        holder,
        route,
        provider,
        slots,
    })
}

/// The token a request presents: `Authorization: Bearer`, or `x-api-key`.
fn token(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    bearer.or_else(|| {
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
    })
}

/// The model asked for must be the route's (decision 5).
fn check_model(front: Front, route: &Route, asked: &str) -> Result<(), Box<Response>> {
    if route.allows(asked) {
        return Ok(());
    }
    Err(Box::new(refuse(
        front,
        Refusal::Invalid,
        &format!(
            "this session's model route `{}` allows {}; the request asked for `{asked}`",
            route.name,
            route.allowed()
        ),
    )))
}

/// `POST /v1/messages`: the Anthropic front (Claude Code).
async fn messages(State(models): State<Arc<Models>>, headers: HeaderMap, body: Bytes) -> Response {
    let admitted = match admit(&models, &headers, Front::Anthropic) {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    if admitted.provider.flavor.api() == Api::AnthropicMessages {
        let passed = match passthrough::anthropic(body) {
            Ok(passed) => passed,
            Err(error) => return refuse(Front::Anthropic, Refusal::Invalid, &error.to_string()),
        };
        if let Err(response) = check_model(Front::Anthropic, &admitted.route, &passed.model) {
            return *response;
        }
        log_passthrough(&passed);
        let forwarded = forward_headers(&headers);
        return pass(models, admitted, Front::Anthropic, passed, forwarded).await;
    }
    let request: anthropic::Request = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return refuse(Front::Anthropic, Refusal::Invalid, &error.to_string()),
    };
    let stream = request.stream.unwrap_or(false);
    let (neutral, dropped) = match front::anthropic::request(request) {
        Ok(translated) => translated,
        Err(error) => return refuse(Front::Anthropic, Refusal::Invalid, &error.to_string()),
    };
    log_dropped(&dropped);
    translate(models, admitted, Front::Anthropic, neutral, stream).await
}

/// `POST /v1/responses`: the Responses front (Codex).
async fn responses_call(
    State(models): State<Arc<Models>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let admitted = match admit(&models, &headers, Front::Responses) {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    if admitted.provider.flavor.api() == Api::OpenAiResponses {
        let passed = match passthrough::responses(body) {
            Ok(passed) => passed,
            Err(error) => return refuse(Front::Responses, Refusal::Invalid, &error.to_string()),
        };
        if let Err(response) = check_model(Front::Responses, &admitted.route, &passed.model) {
            return *response;
        }
        log_passthrough(&passed);
        return pass(models, admitted, Front::Responses, passed, Vec::new()).await;
    }
    let request: responses::Request = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return refuse(Front::Responses, Refusal::Invalid, &error.to_string()),
    };
    let stream = request.stream.unwrap_or(false);
    let (neutral, dropped) = match front::responses::request(request) {
        Ok(translated) => translated,
        Err(error) => return refuse(Front::Responses, Refusal::Invalid, &error.to_string()),
    };
    log_dropped(&dropped);
    translate(models, admitted, Front::Responses, neutral, stream).await
}

/// `POST /v1/messages/count_tokens`: an estimate, a quarter of the
/// request's bytes (Claude Code shows it; no provider is called).
async fn count_tokens(
    State(models): State<Arc<Models>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = admit(&models, &headers, Front::Anthropic) {
        return *response;
    }
    let estimate = u64::try_from(body.len()).unwrap_or(u64::MAX).div_ceil(4);
    json(&anthropic::TokenCount {
        input_tokens: estimate,
    })
}

/// `GET /v1/models`: the models the session's route allows, in the shape
/// of the API the caller speaks (Anthropic's when it sends
/// `anthropic-version`).
async fn list_models(State(models): State<Arc<Models>>, headers: HeaderMap) -> Response {
    let front = if headers.contains_key("anthropic-version") {
        Front::Anthropic
    } else {
        Front::Responses
    };
    let admitted = match admit(&models, &headers, front) {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    let ids: Vec<&ModelId> = std::iter::once(&admitted.route.model)
        .chain(admitted.route.small_model.as_ref())
        .collect();
    match front {
        Front::Anthropic => json(&AnthropicModels {
            data: ids
                .iter()
                .map(|id| AnthropicModel {
                    kind: "model",
                    id: id.as_str(),
                    display_name: id.as_str(),
                    created_at: "1970-01-01T00:00:00Z",
                })
                .collect(),
            has_more: false,
            first_id: ids.first().map(|id| id.as_str()),
            last_id: ids.last().map(|id| id.as_str()),
        }),
        Front::Responses => json(&OpenAiModels {
            object: "list",
            data: ids
                .iter()
                .map(|id| OpenAiModel {
                    id: id.as_str(),
                    object: "model",
                    owned_by: "vessel",
                })
                .collect(),
        }),
    }
}

#[derive(Serialize)]
struct AnthropicModels<'a> {
    data: Vec<AnthropicModel<'a>>,
    has_more: bool,
    first_id: Option<&'a str>,
    last_id: Option<&'a str>,
}

#[derive(Serialize)]
struct AnthropicModel<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    display_name: &'a str,
    created_at: &'static str,
}

#[derive(Serialize)]
struct OpenAiModels<'a> {
    object: &'static str,
    data: Vec<OpenAiModel<'a>>,
}

#[derive(Serialize)]
struct OpenAiModel<'a> {
    id: &'a str,
    object: &'static str,
    owned_by: &'static str,
}

/// Any other path: a WebSocket upgrade gets 426 (Codex then uses HTTP), the
/// rest a JSON 404.
async fn fallback(headers: HeaderMap) -> Response {
    let front = if headers.contains_key("anthropic-version") {
        Front::Anthropic
    } else {
        Front::Responses
    };
    if headers
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return refuse(front, Refusal::Upgrade, "this proxy speaks HTTP only");
    }
    refuse(front, Refusal::NotFound, "no such endpoint on this proxy")
}

/// The headers of Claude Code's that an Anthropic provider needs: the API
/// version and the betas. Never its authorization (the session's token).
fn forward_headers(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    ["anthropic-version", "anthropic-beta"]
        .into_iter()
        .flat_map(|name| {
            headers
                .get_all(name)
                .iter()
                .map(move |value| (HeaderName::from_static(name), value.clone()))
        })
        .collect()
}

fn log_passthrough(passed: &passthrough::Passed) {
    if passed.removed > 0 || passed.thinking_removed {
        tracing::info!(
            removed = passed.removed,
            thinking_removed = passed.thinking_removed,
            "passed a request through less another provider's reasoning"
        );
    }
}

fn log_dropped(dropped: &[vessel_llm::error::Dropped]) {
    for item in dropped {
        tracing::debug!(kind = %item.kind, place = item.place, "left out in translation");
    }
}

/// What a call came to, for its record.
#[derive(Debug, Clone, Default)]
struct Outcome {
    usage: ir::Usage,
    stop: Option<ir::StopReason>,
    failure: Option<ir::FailureClass>,
}

impl Outcome {
    fn see(&mut self, event: &ir::Event) {
        match event {
            ir::Event::Usage(usage) => self.usage = *usage,
            ir::Event::Stop(stop) => self.stop = Some(*stop),
            ir::Event::Failed(failure) => self.failure = Some(failure.class),
            ir::Event::Text(_)
            | ir::Event::Thinking(_)
            | ir::Event::ThinkingEnd(_)
            | ir::Event::ToolCallStart { .. }
            | ir::Event::ToolArgs(_)
            | ir::Event::ToolCallEnd => {}
        }
    }

    fn failed(class: ir::FailureClass) -> Self {
        Self {
            failure: Some(class),
            ..Self::default()
        }
    }
}

/// One call's record and log line.
struct Record {
    holder: Holder,
    route: Route,
    api: Api,
    model: String,
    started: Instant,
    slots: Slots,
}

impl Record {
    fn new(admitted: Admitted, api: Api, model: String) -> (Self, Provider) {
        let Admitted {
            holder,
            route,
            provider,
            slots,
        } = admitted;
        (
            Self {
                holder,
                route,
                api,
                model,
                started: Instant::now(),
                slots,
            },
            provider,
        )
    }

    fn finish(self, outcome: &Outcome) {
        // The call is over: its places in the limits go back.
        drop(self.slots);
        let failure = outcome.failure.map(failure_class).or_else(|| {
            // An answer that never stopped and never failed was cut off
            // (the harness went away, or the stream broke).
            outcome.stop.is_none().then_some(ModelFailure::Truncated)
        });
        let call = ModelCall {
            route: self.route.name.clone(),
            model: ModelId::new(self.model.as_str()),
            api: model_api(self.api),
            usage: usage(&outcome.usage),
            stop: match (failure, outcome.stop) {
                (Some(_), _) | (None, None) => ModelStop::Error,
                (None, Some(stop)) => stop_of(stop),
            },
            failure,
            duration_ms: DurationMs::from_duration(self.started.elapsed()),
        };
        tracing::info!(
            session = %self.holder.session.id(),
            route = %call.route,
            model = %call.model,
            stop = ?call.stop,
            failure = ?call.failure,
            input = call.usage.total_input().get(),
            output = call.usage.output.get(),
            ms = call.duration_ms.get(),
            "model call"
        );
        self.holder.session.model_call(call);
    }
}

const fn failure_class(class: ir::FailureClass) -> ModelFailure {
    match class {
        ir::FailureClass::RateLimited => ModelFailure::RateLimited,
        ir::FailureClass::Overloaded => ModelFailure::Overloaded,
        ir::FailureClass::UpstreamError => ModelFailure::UpstreamError,
        ir::FailureClass::RefusedRequest => ModelFailure::RefusedRequest,
        ir::FailureClass::ContextTooLong => ModelFailure::ContextTooLong,
        ir::FailureClass::Timeout => ModelFailure::Timeout,
        ir::FailureClass::Truncated => ModelFailure::Truncated,
        ir::FailureClass::Unauthorized => ModelFailure::Unauthorized,
    }
}

const fn stop_of(stop: ir::StopReason) -> ModelStop {
    match stop {
        ir::StopReason::EndTurn => ModelStop::EndTurn,
        ir::StopReason::ToolUse => ModelStop::ToolUse,
        ir::StopReason::MaxTokens => ModelStop::MaxTokens,
        ir::StopReason::StopSequence => ModelStop::StopSequence,
        ir::StopReason::Refusal => ModelStop::Refusal,
        ir::StopReason::PauseTurn => ModelStop::Pause,
    }
}

const fn usage(usage: &ir::Usage) -> TokenUsage {
    TokenUsage {
        input: TokenCount::new(usage.input),
        cache_read: TokenCount::new(usage.cache_read),
        cache_write: TokenCount::new(usage.cache_write),
        output: TokenCount::new(usage.output),
        reasoning: TokenCount::new(usage.reasoning),
    }
}

/// The call's endpoint, with its key read now.
async fn endpoint(models: &Models, provider: &Provider) -> Result<Endpoint, String> {
    Ok(Endpoint {
        api: provider.flavor.api(),
        base_url: provider.base_url.clone(),
        key: models.key(provider).await?,
    })
}

/// A neutral-form failure as `front`'s answer before anything was written.
fn failure_response(front: Front, failure: &ir::Failure) -> Response {
    match front {
        Front::Anthropic => {
            let answer = front::anthropic::error_response(failure);
            let mut response = (
                StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY),
                json(&answer.body),
            )
                .into_response();
            if let Some(wait) = answer.retry_after
                && let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string())
            {
                let _replaced = response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
        // Codex reads failures inside the stream, with the codes it acts
        // on; so does a non-streaming caller, from the response's body.
        Front::Responses => {
            let mut encoder = front::responses::StreamEncoder::new("", "", &[]);
            let mut out = Vec::new();
            encoder.push(ir::Event::Failed(failure.clone()), &mut out);
            sse_response(out.iter().filter_map(responses_event).collect())
        }
    }
}

fn sse_response(chunks: Vec<Bytes>) -> Response {
    let stream = futures_util::stream::iter(chunks.into_iter().map(Ok::<Bytes, std::io::Error>));
    sse_body(Body::from_stream(stream))
}

fn sse_body(body: Body) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

fn anthropic_event(event: &anthropic::StreamEvent) -> Option<Bytes> {
    let data = serde_json::to_string(event).ok()?;
    encode_event(Some(event.kind()), &data).ok()
}

fn responses_event(event: &responses::StreamEvent) -> Option<Bytes> {
    let data = serde_json::to_string(event).ok()?;
    encode_event(Some(event.kind()), &data).ok()
}

/// The request's own bytes up, the answer's bytes down (same API).
async fn pass(
    models: Arc<Models>,
    admitted: Admitted,
    front: Front,
    passed: passthrough::Passed,
    headers: Vec<(HeaderName, HeaderValue)>,
) -> Response {
    let (record, provider) = Record::new(admitted, front.api(), passed.model.clone());
    let endpoint = match endpoint(&models, &provider).await {
        Ok(endpoint) => endpoint,
        Err(message) => {
            record.finish(&Outcome::failed(ir::FailureClass::Unauthorized));
            return refuse(front, Refusal::Forbidden, &message);
        }
    };
    let answer = match models.client().send(&endpoint, passed.body, &headers).await {
        Ok(answer) => answer,
        Err(failure) => {
            record.finish(&Outcome::failed(failure.class));
            return failure_response(front, &failure);
        }
    };
    if !passed.stream {
        return match answer.body().await {
            Ok(body) => {
                record.finish(&whole_outcome(front, &body));
                ([(header::CONTENT_TYPE, "application/json")], body).into_response()
            }
            Err(failure) => {
                record.finish(&Outcome::failed(failure.class));
                failure_response(front, &failure)
            }
        };
    }
    let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(PIPE);
    drop(tokio::spawn(relay(answer, front, sender, record)));
    sse_body(Body::from_stream(tokio_stream(receiver)))
}

/// A non-streamed answer's usage and stop.
fn whole_outcome(front: Front, body: &[u8]) -> Outcome {
    match front {
        Front::Anthropic => serde_json::from_slice::<anthropic::Message>(body).map_or_else(
            |_unreadable| Outcome::default(),
            |message| Outcome {
                usage: ir::Usage {
                    input: message.usage.input_tokens.unwrap_or(0),
                    cache_read: message.usage.cache_read_input_tokens.unwrap_or(0),
                    cache_write: message.usage.cache_creation_input_tokens.unwrap_or(0),
                    output: message.usage.output_tokens.unwrap_or(0),
                    reasoning: 0,
                },
                stop: Some(match message.stop_reason {
                    Some(anthropic::StopReason::ToolUse) => ir::StopReason::ToolUse,
                    Some(
                        anthropic::StopReason::MaxTokens
                        | anthropic::StopReason::ContextWindowExceeded,
                    ) => ir::StopReason::MaxTokens,
                    Some(anthropic::StopReason::StopSequence) => ir::StopReason::StopSequence,
                    Some(anthropic::StopReason::Refusal) => ir::StopReason::Refusal,
                    Some(anthropic::StopReason::PauseTurn) => ir::StopReason::PauseTurn,
                    Some(anthropic::StopReason::EndTurn | anthropic::StopReason::Unknown(_))
                    | None => ir::StopReason::EndTurn,
                }),
                failure: None,
            },
        ),
        Front::Responses => serde_json::from_slice::<responses::Response>(body).map_or_else(
            |_unreadable| Outcome::default(),
            |response| Outcome {
                usage: response
                    .usage
                    .map_or_else(ir::Usage::default, |usage| ir::Usage {
                        input: usage
                            .input_tokens
                            .saturating_sub(usage.input_tokens_details.cached_tokens),
                        cache_read: usage.input_tokens_details.cached_tokens,
                        cache_write: 0,
                        output: usage.output_tokens,
                        reasoning: usage.output_tokens_details.reasoning_tokens,
                    }),
                stop: Some(match response.status {
                    responses::ResponseStatus::Incomplete => ir::StopReason::MaxTokens,
                    responses::ResponseStatus::Queued
                    | responses::ResponseStatus::InProgress
                    | responses::ResponseStatus::Completed
                    | responses::ResponseStatus::Failed
                    | responses::ResponseStatus::Cancelled
                    | responses::ResponseStatus::Unknown(_) => ir::StopReason::EndTurn,
                }),
                failure: None,
            },
        ),
    }
}

/// The upstream's decoder for a same-API answer, read only for its
/// outcome.
enum Tap {
    Anthropic(upstream::anthropic::StreamDecoder),
    Responses(upstream::responses::StreamDecoder),
}

impl Tap {
    fn push(&mut self, event: &SseEvent, out: &mut Vec<ir::Event>) {
        match self {
            Self::Anthropic(decoder) => decoder.push(event, out),
            Self::Responses(decoder) => decoder.push(event, out),
        }
    }

    fn finish(&mut self, out: &mut Vec<ir::Event>) {
        match self {
            Self::Anthropic(decoder) => decoder.finish(out),
            Self::Responses(decoder) => decoder.finish(out),
        }
    }
}

/// Copy a same-API stream to the harness, reading it alongside for the
/// record.
async fn relay(
    answer: Answer,
    front: Front,
    sender: mpsc::Sender<Result<Bytes, std::io::Error>>,
    record: Record,
) {
    let mut tap = match front {
        Front::Anthropic => Tap::Anthropic(upstream::anthropic::StreamDecoder::new()),
        Front::Responses => Tap::Responses(upstream::responses::StreamDecoder::new()),
    };
    let mut sse = SseDecoder::default();
    let mut outcome = Outcome::default();
    let mut frames = Vec::new();
    let mut events = Vec::new();
    let mut chunks = chunks(answer);
    while let Some(chunk) = chunks.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(failure) => {
                outcome.failure = Some(failure.class);
                break;
            }
        };
        if sse.feed(&bytes, &mut frames).is_ok() {
            for frame in frames.drain(..) {
                if let SseFrame::Event(event) = frame {
                    tap.push(&event, &mut events);
                }
            }
            for event in events.drain(..) {
                outcome.see(&event);
            }
        }
        if sender.send(Ok(bytes)).await.is_err() {
            // The harness went away.
            break;
        }
    }
    if outcome.failure.is_none() && outcome.stop.is_none() {
        tap.finish(&mut events);
        for event in events.drain(..) {
            outcome.see(&event);
        }
    }
    record.finish(&outcome);
}

/// An answer's body as a stream of chunks (an `unfold` keeps the chunk in
/// flight across keepalive ticks, so the idle timeout keeps counting).
fn chunks(
    answer: Answer,
) -> impl futures_util::Stream<Item = Result<Bytes, ir::Failure>> + Unpin + Send {
    Box::pin(futures_util::stream::unfold(
        Some(answer),
        |state| async move {
            let mut answer = state?;
            match answer.chunk().await {
                Ok(Some(bytes)) => Some((Ok(bytes), Some(answer))),
                Ok(None) => None,
                Err(failure) => Some((Err(failure), None)),
            }
        },
    ))
}

fn tokio_stream(
    receiver: mpsc::Receiver<Result<Bytes, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    })
}

/// The neutral request up in the provider's API, the answer down in the
/// harness's.
async fn translate(
    models: Arc<Models>,
    admitted: Admitted,
    front: Front,
    mut neutral: ir::Request,
    stream: bool,
) -> Response {
    if let Err(response) = check_model(front, &admitted.route, &neutral.model) {
        return *response;
    }
    if neutral.max_output_tokens.is_none() {
        neutral.max_output_tokens = admitted.route.max_output_tokens;
    }
    let api = admitted.provider.flavor.api();
    let (record, provider) = Record::new(admitted, api, neutral.model.clone());
    let body = match upstream_body(&provider, &neutral) {
        Ok(body) => body,
        Err(error) => {
            record.finish(&Outcome::failed(ir::FailureClass::RefusedRequest));
            return refuse(front, Refusal::Invalid, &error.to_string());
        }
    };
    let endpoint = match endpoint(&models, &provider).await {
        Ok(endpoint) => endpoint,
        Err(message) => {
            record.finish(&Outcome::failed(ir::FailureClass::Unauthorized));
            return refuse(front, Refusal::Forbidden, &message);
        }
    };
    let answer = match models.client().send(&endpoint, body, &[]).await {
        Ok(answer) => answer,
        Err(failure) => {
            record.finish(&Outcome::failed(failure.class));
            return failure_response(front, &failure);
        }
    };
    let source = provider.name.to_string();
    let encoder = match front {
        Front::Anthropic => Encoder::Anthropic(front::anthropic::StreamEncoder::new(
            &neutral.model,
            &source,
        )),
        Front::Responses => Encoder::Responses(front::responses::StreamEncoder::new(
            &neutral.model,
            &source,
            &neutral.tools,
        )),
    };
    let decoder = match provider.flavor {
        Flavor::Anthropic { .. } => Decoder::Anthropic(upstream::anthropic::StreamDecoder::new()),
        Flavor::Responses { .. } => Decoder::Responses(upstream::responses::StreamDecoder::new()),
        Flavor::Chat { .. } => Decoder::Chat(upstream::chat::StreamDecoder::new()),
    };
    let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(PIPE);
    let pump = Pump {
        decoder,
        encoder,
        record,
    };
    if stream {
        drop(tokio::spawn(pump.run(answer, sender)));
        return sse_body(Body::from_stream(tokio_stream(receiver)));
    }
    pump.collect(answer, front).await
}

/// The request in the provider's API.
fn upstream_body(
    provider: &Provider,
    neutral: &ir::Request,
) -> Result<Bytes, vessel_llm::error::TranslateError> {
    let source = provider.name.to_string();
    let bytes = match &provider.flavor {
        Flavor::Anthropic {
            adaptive_thinking,
            default_max_tokens,
        } => serde_json::to_vec(&upstream::anthropic::request(
            neutral,
            &upstream::anthropic::Options {
                provider: source,
                default_max_tokens: *default_max_tokens,
                adaptive_thinking: *adaptive_thinking,
            },
        )?)?,
        Flavor::Responses { sampling } => serde_json::to_vec(&upstream::responses::request(
            neutral,
            &upstream::responses::Options {
                provider: source,
                sampling: *sampling,
            },
        )?)?,
        Flavor::Chat {
            parallel_tool_calls,
            max_tokens_field,
            reasoning,
            replay_reasoning,
        } => serde_json::to_vec(&upstream::chat::request(
            neutral,
            &upstream::chat::Options {
                provider: source,
                parallel_tool_calls: *parallel_tool_calls,
                max_tokens_field: *max_tokens_field,
                reasoning: *reasoning,
                replay_reasoning: *replay_reasoning,
            },
        )?)?,
    };
    Ok(Bytes::from(bytes))
}

enum Decoder {
    Anthropic(upstream::anthropic::StreamDecoder),
    Responses(upstream::responses::StreamDecoder),
    Chat(upstream::chat::StreamDecoder),
}

impl Decoder {
    fn push(&mut self, event: &SseEvent, out: &mut Vec<ir::Event>) {
        match self {
            Self::Anthropic(decoder) => decoder.push(event, out),
            Self::Responses(decoder) => decoder.push(event, out),
            Self::Chat(decoder) => decoder.push(event, out),
        }
    }

    fn finish(&mut self, out: &mut Vec<ir::Event>) {
        match self {
            Self::Anthropic(decoder) => decoder.finish(out),
            Self::Responses(decoder) => decoder.finish(out),
            Self::Chat(decoder) => decoder.finish(out),
        }
    }
}

enum Encoder {
    Anthropic(front::anthropic::StreamEncoder),
    Responses(front::responses::StreamEncoder),
}

/// What a front writes: SSE bytes, and the typed events for a caller that
/// asked for no stream.
enum Written {
    Anthropic(Vec<anthropic::StreamEvent>),
    Responses(Vec<responses::StreamEvent>),
}

impl Written {
    fn bytes(&self) -> Vec<Bytes> {
        match self {
            Self::Anthropic(events) => events.iter().filter_map(anthropic_event).collect(),
            Self::Responses(events) => events.iter().filter_map(responses_event).collect(),
        }
    }
}

impl Encoder {
    fn push(&mut self, event: ir::Event) -> Written {
        match self {
            Self::Anthropic(encoder) => {
                let mut out = Vec::new();
                encoder.push(event, &mut out);
                Written::Anthropic(out)
            }
            Self::Responses(encoder) => {
                let mut out = Vec::new();
                encoder.push(event, &mut out);
                Written::Responses(out)
            }
        }
    }

    fn finish(&mut self) -> Written {
        match self {
            Self::Anthropic(encoder) => {
                let mut out = Vec::new();
                encoder.finish(&mut out);
                Written::Anthropic(out)
            }
            Self::Responses(encoder) => {
                let mut out = Vec::new();
                encoder.finish(&mut out);
                Written::Responses(out)
            }
        }
    }

    fn keepalive(&self) -> Bytes {
        match self {
            Self::Anthropic(_) => anthropic_event(&front::anthropic::StreamEncoder::ping())
                .unwrap_or_else(|| encode_comment("keepalive")),
            Self::Responses(_) => encode_comment("keepalive"),
        }
    }
}

/// Reads the provider's stream, translates it, and writes the harness's.
struct Pump {
    decoder: Decoder,
    encoder: Encoder,
    record: Record,
}

impl Pump {
    /// The translated answer, streamed to `sender` with keepalives.
    async fn run(mut self, answer: Answer, sender: mpsc::Sender<Result<Bytes, std::io::Error>>) {
        let mut outcome = Outcome::default();
        let mut chunks = chunks(answer);
        let mut sse = SseDecoder::default();
        let start = tokio::time::Instant::now()
            .checked_add(KEEPALIVE)
            .unwrap_or_else(tokio::time::Instant::now);
        let mut ticker = tokio::time::interval_at(start, KEEPALIVE);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut gone = false;
        while !self.ended() && !gone {
            let chunk = tokio::select! {
                chunk = chunks.next() => chunk,
                _tick = ticker.tick() => {
                    gone = sender.send(Ok(self.encoder.keepalive())).await.is_err();
                    continue;
                }
            };
            ticker.reset();
            let Some(written) = self.step(chunk, &mut sse, &mut outcome) else {
                break;
            };
            for bytes in written.bytes() {
                if sender.send(Ok(bytes)).await.is_err() {
                    gone = true;
                    break;
                }
            }
        }
        if gone && outcome.stop.is_none() && outcome.failure.is_none() {
            outcome.failure = Some(ir::FailureClass::Truncated);
        }
        self.record.finish(&outcome);
    }

    /// The translated answer, whole, for a caller that asked for no stream.
    async fn collect(mut self, answer: Answer, front: Front) -> Response {
        let mut outcome = Outcome::default();
        let mut chunks = chunks(answer);
        let mut sse = SseDecoder::default();
        let mut anthropic_out = front::anthropic::Collector::default();
        let mut responses_out = front::responses::Collector::default();
        while !self.ended() {
            let chunk = chunks.next().await;
            let Some(written) = self.step(chunk, &mut sse, &mut outcome) else {
                break;
            };
            match written {
                Written::Anthropic(events) => {
                    for event in events {
                        anthropic_out.push(event);
                    }
                }
                Written::Responses(events) => {
                    for event in events {
                        responses_out.push(event);
                    }
                }
            }
        }
        self.record.finish(&outcome);
        match front {
            Front::Anthropic => match anthropic_out.finish() {
                Ok(message) => json(&message),
                Err(error) => refuse(Front::Anthropic, Refusal::Invalid, &error.message),
            },
            Front::Responses => match responses_out.finish() {
                Some(response) => json(&response),
                None => refuse(Front::Responses, Refusal::Invalid, "the answer never ended"),
            },
        }
    }

    fn ended(&self) -> bool {
        match &self.encoder {
            Encoder::Anthropic(encoder) => encoder.ended(),
            Encoder::Responses(encoder) => encoder.ended(),
        }
    }

    /// One chunk of the provider's stream (`None`: its end) translated to
    /// the front's events; `None` once the answer is over.
    fn step(
        &mut self,
        chunk: Option<Result<Bytes, ir::Failure>>,
        sse: &mut SseDecoder,
        outcome: &mut Outcome,
    ) -> Option<Written> {
        if self.ended() {
            return None;
        }
        let mut neutral = Vec::new();
        match chunk {
            Some(Ok(bytes)) => {
                let mut frames = Vec::new();
                if sse.feed(&bytes, &mut frames).is_err() {
                    neutral.push(ir::Event::Failed(ir::Failure::new(
                        ir::FailureClass::UpstreamError,
                        "the provider's stream is not Server-Sent Events Vessel can read",
                    )));
                }
                for frame in frames {
                    if let SseFrame::Event(event) = frame {
                        self.decoder.push(&event, &mut neutral);
                    }
                }
            }
            Some(Err(failure)) => neutral.push(ir::Event::Failed(failure)),
            None => self.decoder.finish(&mut neutral),
        }
        let at_end = chunk_ended(&neutral);
        let mut written = match &self.encoder {
            Encoder::Anthropic(_) => Written::Anthropic(Vec::new()),
            Encoder::Responses(_) => Written::Responses(Vec::new()),
        };
        for event in neutral {
            outcome.see(&event);
            merge(&mut written, self.encoder.push(event));
        }
        if at_end {
            merge(&mut written, self.encoder.finish());
        }
        Some(written)
    }
}

/// Whether the provider's answer is over in these events.
fn chunk_ended(events: &[ir::Event]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, ir::Event::Stop(_) | ir::Event::Failed(_)))
}

fn merge(into: &mut Written, more: Written) {
    match (into, more) {
        (Written::Anthropic(into), Written::Anthropic(more)) => into.extend(more),
        (Written::Responses(into), Written::Responses(more)) => into.extend(more),
        (Written::Anthropic(_), Written::Responses(_))
        | (Written::Responses(_), Written::Anthropic(_)) => {}
    }
}
