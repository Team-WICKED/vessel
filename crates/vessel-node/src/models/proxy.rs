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
//!
//! [Decision Log] (the security review of milestone 5)
//! - Finding 4, admission before the body: every check that needs only
//!   the headers runs before a byte of the body is read (`Origin`,
//!   `Content-Encoding`, the token, then `Host`, the route, the limits),
//!   and the body is then read under [`super::MAX_REQUEST`] and
//!   [`BODY_DEADLINE`]; the places in the limits are held from admission
//!   to the call's record. Connections and header reads are bounded by
//!   the accept loop (`listen`).
//! - `Host` (defence in depth against DNS rebinding): only
//!   `127.0.0.1:<port>` and `localhost:<port>`, checked after the token,
//!   so a request without one learns nothing more than 401.
//! - Finding 2, the key in a 200 answer: a translated answer's failures
//!   are redacted before they are written ([`Pump`]); a same-API stream is
//!   decoded event by event and each event written again, its data
//!   redacted, an error event's JSON strings decoded to find the key
//!   however escaped ([`relay`]); a whole same-API answer is redacted the
//!   same way. Writing events again (rather than the provider's chunks)
//!   is what makes a key cut across two chunks whole before anything is
//!   released; it loses only `id:` and `retry:`, which neither API uses.
//! - Finding 5, a bounded answer: every answer the proxy reads as a
//!   stream counts its bytes against [`Bounds::answer`] (the client's
//!   body bound, 32 MiB), translated or not, streamed to the harness or
//!   collected: past it the provider is cut off and the harness gets a
//!   classified failure. Everything a translation keeps (collected
//!   messages, tool arguments, signatures, the Responses front's final
//!   snapshot) grows from those bytes, so it is bounded with them.
//! - Downstream stalls: every write toward the harness has the client's
//!   idle timeout ([`Bounds::send`]); a harness that stops reading is
//!   treated as gone, and the provider cut off. A same-API stream Vessel
//!   cannot decode is a classified failure, not passed on unread.
//!
//! [Decision Log] (the verification round on the review's fixes)
//! - Problem: a same-API stream was classified first (is this an error
//!   event?) and only an error event's JSON was decoded; anything else
//!   had its raw text matched. An escaped `"\u0065rror"` type, an event
//!   name of the provider's choosing, or an escaped key in a text delta
//!   reached the harness, which decodes them, at every piece size. And a
//!   same-API stream that simply stopped (a clean end after a delta, no
//!   `message_stop`) was passed on as if it had ended.
//! - Choice: nothing is classified before it is checked. Every event the
//!   provider sends, same API or translated, has its name and its data
//!   checked once, the data decoded as JSON, member names included
//!   (`Redactor::holds`, [`quotes_key`]). An event holding the key is not
//!   written at all: the call ends there with a classified failure in the
//!   front's form, the provider's own if the event was an error (its
//!   message redacted), else [`quoted`]. A same-API stream's end is read
//!   by its decoder as well: an answer that never ended is reported to
//!   the harness as an error event and recorded as truncated.
//! - Alternative: rewrite such an event with the key replaced. The answer
//!   would go on, looking whole, with a hole where the provider wrote the
//!   key; a provider quoting the key is broken, and the harness is better
//!   told so.
//! - Cost: each event's data is decoded once more when it holds a
//!   backslash (without one, decoding changes no string).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use futures_util::StreamExt as _;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_util::sync::CancellationToken;
use vessel_llm::client::{Answer, Api, Endpoint, Redactor};
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

/// How long a request's body may take to arrive once it is admitted.
const BODY_DEADLINE: Duration = Duration::from_secs(30);

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
        .with_state(models);
    super::listen::run(listener, app, stop).await;
}

/// The bounds on what one call reads and writes.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    /// The provider's answer, as it arrives (finding 5).
    answer: usize,
    /// One write toward the harness (a harness that stops reading).
    send: Duration,
}

impl Bounds {
    fn of(models: &Models) -> Self {
        let limits = models.client().limits();
        Self {
            answer: limits.max_body,
            send: limits.idle,
        }
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
    TooLarge,
    Slow,
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
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Slow => StatusCode::REQUEST_TIMEOUT,
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
                Refusal::TooLarge => anthropic::ErrorKind::RequestTooLarge,
                Refusal::Slow => anthropic::ErrorKind::Timeout,
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
                | Refusal::Upgrade
                | Refusal::TooLarge
                | Refusal::Slow => responses::ErrorType::InvalidRequest,
            });
            json(&body)
        }
    };
    let mut response = (refusal.status(), body).into_response();
    // A refused connection does not stay: renewing it with one refused
    // request after another held a place without a token (`listen`). Its
    // body, if any, is unread, which ends it anyway.
    let _replaced = response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
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

/// The checks every model call passes before its body is read, in order:
/// no `Origin`, no compressed body, a live session's token, the proxy's
/// own address as `Host`, a route still defined, room in the limits.
fn admit(models: &Models, parts: &Parts, front: Front) -> Result<Admitted, Box<Response>> {
    let headers = &parts.headers;
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
    // A harness's connection: never closed to make room (`listen`).
    if let Some(proof) = parts.extensions.get::<super::listen::Proof>() {
        proof.prove(holder.session.id());
    }
    if !own_host(models, parts) {
        tracing::info!("refused a model call naming another host");
        return Err(Box::new(refuse(
            front,
            Refusal::Forbidden,
            "this proxy answers only requests to 127.0.0.1 or localhost at its own port",
        )));
    }
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

/// Whether the request names the proxy itself: `Host` (or, without one,
/// the request target's authority) is `127.0.0.1:<port>` or
/// `localhost:<port>`. A page that reached the port through DNS rebinding
/// names its own host.
fn own_host(models: &Models, parts: &Parts) -> bool {
    let Some(port) = models.port() else {
        return false;
    };
    let named = parts
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            parts
                .uri
                .authority()
                .map(axum::http::uri::Authority::as_str)
        });
    let Some((host, named_port)) = named.and_then(|named| named.rsplit_once(':')) else {
        return false;
    };
    named_port.parse::<u16>().ok() == Some(port)
        && (host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost"))
}

/// A request's body, read once it is admitted: at most
/// [`super::MAX_REQUEST`] bytes within [`BODY_DEADLINE`].
async fn read_body(body: Body, front: Front) -> Result<Bytes, Box<Response>> {
    let limit = super::MAX_REQUEST;
    let reading = async {
        let mut stream = body.into_data_stream();
        let mut read = bytes::BytesMut::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_broken| Refusal::Invalid)?;
            if read.len().saturating_add(chunk.len()) > limit {
                return Err(Refusal::TooLarge);
            }
            read.extend_from_slice(&chunk);
        }
        Ok(read.freeze())
    };
    match tokio::time::timeout(BODY_DEADLINE, reading).await {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(Refusal::TooLarge)) => Err(Box::new(refuse(
            front,
            Refusal::TooLarge,
            &format!("request bodies are limited to {limit} bytes"),
        ))),
        Ok(Err(_broken)) => Err(Box::new(refuse(
            front,
            Refusal::Invalid,
            "the request's body could not be read",
        ))),
        Err(_elapsed) => Err(Box::new(refuse(
            front,
            Refusal::Slow,
            "the request's body did not arrive in time",
        ))),
    }
}

/// Admit a request, then read its body.
async fn admit_and_read(
    models: &Models,
    request: Request,
    front: Front,
) -> Result<(Admitted, Parts, Bytes), Box<Response>> {
    let (parts, body) = request.into_parts();
    let admitted = admit(models, &parts, front)?;
    let body = read_body(body, front).await?;
    Ok((admitted, parts, body))
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
async fn messages(State(models): State<Arc<Models>>, request: Request) -> Response {
    let (admitted, parts, body) = match admit_and_read(&models, request, Front::Anthropic).await {
        Ok(read) => read,
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
        let forwarded = forward_headers(&parts.headers);
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
async fn responses_call(State(models): State<Arc<Models>>, request: Request) -> Response {
    let (admitted, _parts, body) = match admit_and_read(&models, request, Front::Responses).await {
        Ok(read) => read,
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
async fn count_tokens(State(models): State<Arc<Models>>, request: Request) -> Response {
    let body = match admit_and_read(&models, request, Front::Anthropic).await {
        Ok((_admitted, _parts, body)) => body,
        Err(response) => return *response,
    };
    let estimate = u64::try_from(body.len()).unwrap_or(u64::MAX).div_ceil(4);
    json(&anthropic::TokenCount {
        input_tokens: estimate,
    })
}

/// `GET /v1/models`: the models the session's route allows, in the shape
/// of the API the caller speaks (Anthropic's when it sends
/// `anthropic-version`).
async fn list_models(State(models): State<Arc<Models>>, request: Request) -> Response {
    let (parts, _body) = request.into_parts();
    let front = if parts.headers.contains_key("anthropic-version") {
        Front::Anthropic
    } else {
        Front::Responses
    };
    let admitted = match admit(&models, &parts, front) {
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
    /// The first failure, as the harness is told it (already redacted).
    detail: Option<ir::Failure>,
}

impl Outcome {
    fn see(&mut self, event: &ir::Event) {
        match event {
            ir::Event::Usage(usage) => self.usage = *usage,
            ir::Event::Stop(stop) => self.stop = Some(*stop),
            ir::Event::Failed(failure) => {
                self.failure = Some(failure.class);
                if self.detail.is_none() {
                    self.detail = Some(failure.clone());
                }
            }
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
        Front::Responses => sse_response(failure_frames(front, failure)),
    }
}

/// A failure as SSE events in `front`'s form, for a stream already under
/// way or one that never started: Anthropic's `error` event, or a
/// Responses `response.failed` (with its `response.created`).
fn failure_frames(front: Front, failure: &ir::Failure) -> Vec<Bytes> {
    match front {
        Front::Anthropic => {
            let answer = front::anthropic::error_response(failure);
            serde_json::to_string(&answer.body)
                .ok()
                .and_then(|data| encode_event(Some("error"), &data).ok())
                .into_iter()
                .collect()
        }
        Front::Responses => {
            let mut encoder = front::responses::StreamEncoder::new("", "", &[]);
            let mut out = Vec::new();
            encoder.push(ir::Event::Failed(failure.clone()), &mut out);
            out.iter().filter_map(responses_event).collect()
        }
    }
}

/// The failure of an answer past [`Bounds::answer`] (finding 5).
fn too_large(limit: usize) -> ir::Failure {
    ir::Failure::new(
        ir::FailureClass::UpstreamError,
        format!("the provider's answer grew past {limit} bytes; the proxy cut it off"),
    )
}

/// The failure of a stream Vessel cannot read as Server-Sent Events.
fn unreadable() -> ir::Failure {
    ir::Failure::new(
        ir::FailureClass::UpstreamError,
        "the provider's stream is not Server-Sent Events Vessel can read",
    )
}

/// Write `items` toward the harness, each within `deadline`; `false` once
/// the harness has gone or stopped reading.
async fn deliver(
    sender: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    items: impl IntoIterator<Item = Bytes>,
    deadline: Duration,
) -> bool {
    for bytes in items {
        match tokio::time::timeout(deadline, sender.send(Ok(bytes))).await {
            Ok(Ok(())) => {}
            Ok(Err(_gone)) => return false,
            Err(_stalled) => {
                tracing::info!("a harness stopped reading its model answer; the call ends");
                return false;
            }
        }
    }
    true
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
    let redactor = answer.redactor();
    if !passed.stream {
        return match answer.body().await {
            Ok(body) => match redact_whole(&redactor, body) {
                // A 200 may carry a failed response, quoting the key.
                Ok(body) => {
                    record.finish(&whole_outcome(front, &body));
                    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
                }
                Err(withheld) => {
                    record.finish(&Outcome::failed(withheld.class));
                    failure_response(front, &withheld)
                }
            },
            Err(failure) => {
                record.finish(&Outcome::failed(failure.class));
                failure_response(front, &redactor.failure(failure))
            }
        };
    }
    let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(PIPE);
    drop(tokio::spawn(relay(
        answer,
        front,
        sender,
        record,
        Bounds::of(&models),
    )));
    sse_body(Body::from_stream(tokio_stream(receiver)))
}

/// A whole answer with the key removed from its JSON strings and member
/// names (or from its text, if it is not JSON); the provider's bytes when
/// it holds none. One that still holds it, escaped in JSON `serde_json`
/// cannot decode, is withheld: the failure to answer instead.
fn redact_whole(redactor: &Redactor, body: Bytes) -> Result<Bytes, ir::Failure> {
    let text = String::from_utf8_lossy(&body);
    match redactor.json(&text) {
        Some(std::borrow::Cow::Borrowed(_)) => Ok(body),
        Some(std::borrow::Cow::Owned(redacted)) => Ok(Bytes::from(redacted)),
        None => Err(quoted()),
    }
}

/// Whether a provider's event holds the call's key anywhere: in its name,
/// or in its data as written or decoded as JSON, member names included.
fn quotes_key(redactor: &Redactor, event: &SseEvent) -> bool {
    event
        .event
        .as_deref()
        .is_some_and(|name| redactor.holds(name))
        || redactor.holds(&event.data)
}

/// The failure written in place of an event that quoted the key: the
/// provider's own, redacted, if the event was one; else [`quoted`].
fn withheld(redactor: &Redactor, own: Option<ir::Failure>) -> ir::Failure {
    own.map_or_else(quoted, |failure| redactor.failure(failure))
}

/// The failure in place of an answer that quoted the call's key. Not a
/// transient class: the same request would most likely be answered the
/// same way, and a harness retries a transient failure as a new, billed
/// call (Claude Code backs off on `overloaded_error`).
fn quoted() -> ir::Failure {
    ir::Failure::new(
        ir::FailureClass::RefusedRequest,
        "the provider's answer quoted the call's key; the proxy withheld it and ended the call",
    )
}

fn failure_of(event: ir::Event) -> Option<ir::Failure> {
    if let ir::Event::Failed(failure) = event {
        Some(failure)
    } else {
        None
    }
}

/// A same-API stream's decoded `frames` as the harness gets them, into
/// `out`: each event written again as it came (the tap reads it into
/// `events` for the record), a comment as a keepalive. An event quoting
/// the key is not written: what replaces it comes back, and the frames
/// after it are dropped.
fn release(
    frames: &mut Vec<SseFrame>,
    tap: &mut Tap,
    redactor: &Redactor,
    events: &mut Vec<ir::Event>,
    out: &mut Vec<Bytes>,
) -> Option<ir::Failure> {
    for frame in frames.drain(..) {
        match frame {
            SseFrame::Event(event) => {
                let before = events.len();
                tap.push(&event, events);
                if quotes_key(redactor, &event) {
                    let own = events.drain(before..).find_map(failure_of);
                    return Some(withheld(redactor, own));
                }
                // `None` only for a name SSE cannot carry, which the
                // decoder never yields.
                out.extend(encode_event(event.event.as_deref(), &event.data).ok());
            }
            SseFrame::Comment => out.push(encode_comment("keepalive")),
        }
    }
    None
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
                detail: None,
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
                detail: None,
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

/// Copy a same-API stream to the harness event by event (see [`release`]),
/// reading it alongside for the record, within [`Bounds`]; a stream that
/// cannot be decoded, grows past the bound, quotes the key or ends before
/// its answer did ends in a failure in the front's form.
async fn relay(
    answer: Answer,
    front: Front,
    sender: mpsc::Sender<Result<Bytes, std::io::Error>>,
    record: Record,
    bounds: Bounds,
) {
    let redactor = answer.redactor();
    let mut tap = match front {
        Front::Anthropic => Tap::Anthropic(upstream::anthropic::StreamDecoder::new()),
        Front::Responses => Tap::Responses(upstream::responses::StreamDecoder::new()),
    };
    let mut sse = SseDecoder::default();
    let mut outcome = Outcome::default();
    let mut frames = Vec::new();
    let mut events = Vec::new();
    let mut budget = bounds.answer;
    let mut chunks = chunks(answer);
    loop {
        let (ended, mut failure) = match chunks.next().await {
            None => (
                true,
                std::mem::take(&mut sse)
                    .finish(&mut frames)
                    .err()
                    .map(|_unreadable| unreadable()),
            ),
            Some(Err(failure)) => (true, Some(failure)),
            Some(Ok(bytes)) => match budget.checked_sub(bytes.len()) {
                None => (true, Some(too_large(bounds.answer))),
                Some(left) => {
                    budget = left;
                    let fed = sse.feed(&bytes, &mut frames);
                    (fed.is_err(), fed.err().map(|_unreadable| unreadable()))
                }
            },
        };
        let mut out = Vec::new();
        if let Some(withheld) = release(&mut frames, &mut tap, &redactor, &mut events, &mut out) {
            failure = Some(withheld);
        }
        if ended && failure.is_none() {
            // A clean end: the tap knows whether the answer ended first,
            // and the harness is told if it did not.
            let mut last = Vec::new();
            tap.finish(&mut last);
            failure = last.into_iter().find_map(failure_of);
        }
        for mut event in events.drain(..) {
            // Only the record reads these; it keeps no key either.
            if let ir::Event::Failed(failure) = &mut event {
                *failure = redactor.failure(failure.clone());
            }
            outcome.see(&event);
        }
        if let Some(failure) = &failure {
            let failure = redactor.failure(failure.clone());
            out.extend(failure_frames(front, &failure));
            outcome.see(&ir::Event::Failed(failure));
        }
        if !deliver(&sender, out, bounds.send).await {
            // The harness went away, or stopped reading.
            if outcome.failure.is_none() && outcome.stop.is_none() {
                outcome.failure = Some(ir::FailureClass::Truncated);
            }
            break;
        }
        if ended || failure.is_some() {
            break;
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
    let bounds = Bounds::of(&models);
    let pump = Pump {
        decoder,
        encoder,
        record,
        redactor: answer.redactor(),
        budget: bounds.answer,
        bounds,
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
    /// Removes the call's key from every failure before it is written.
    redactor: Redactor,
    /// What is left of [`Bounds::answer`].
    budget: usize,
    bounds: Bounds,
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
        let deadline = self.bounds.send;
        while !self.ended() && !gone {
            let chunk = tokio::select! {
                chunk = chunks.next() => chunk,
                _tick = ticker.tick() => {
                    gone = !deliver(&sender, [self.encoder.keepalive()], deadline).await;
                    continue;
                }
            };
            ticker.reset();
            let Some(written) = self.step(chunk, &mut sse, &mut outcome) else {
                break;
            };
            gone = !deliver(&sender, written.bytes(), deadline).await;
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
        // A failure (the provider's, redacted, or the answer's bound) is
        // answered as a failure, with the status its class calls for.
        if let Some(failure) = &outcome.detail {
            return failure_response(front, failure);
        }
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
            Some(Ok(bytes)) => match self.budget.checked_sub(bytes.len()) {
                // Past the bound: the provider is cut off (the caller stops
                // reading once the answer has ended).
                None => neutral.push(ir::Event::Failed(too_large(self.bounds.answer))),
                Some(left) => {
                    self.budget = left;
                    let mut frames = Vec::new();
                    if sse.feed(&bytes, &mut frames).is_err() {
                        neutral.push(ir::Event::Failed(unreadable()));
                    }
                    for frame in frames {
                        if let SseFrame::Event(event) = frame {
                            let before = neutral.len();
                            self.decoder.push(&event, &mut neutral);
                            if quotes_key(&self.redactor, &event) {
                                // Decoded, it would be written in the
                                // front's form with the key spelled out.
                                let own = neutral.drain(before..).find_map(failure_of);
                                neutral.push(ir::Event::Failed(withheld(&self.redactor, own)));
                                break;
                            }
                        }
                    }
                }
            },
            Some(Err(failure)) => neutral.push(ir::Event::Failed(failure)),
            None => self.decoder.finish(&mut neutral),
        }
        let at_end = chunk_ended(&neutral);
        let mut written = match &self.encoder {
            Encoder::Anthropic(_) => Written::Anthropic(Vec::new()),
            Encoder::Responses(_) => Written::Responses(Vec::new()),
        };
        for mut event in neutral {
            // An inline error from a 200 stream may quote the key.
            if let ir::Event::Failed(failure) = &mut event {
                *failure = self.redactor.failure(failure.clone());
            }
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
