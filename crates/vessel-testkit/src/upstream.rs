//! A fake model provider (feature `upstream`): a loopback HTTP server that
//! answers every request with the next reply a test scripted, and records
//! what it was asked.
//!
//! [Decision Log]
//! - Purpose: the routing proxy (milestone 5) calls providers; its tests
//!   must not (AGENTS.md section 8), yet must see the real client and
//!   proxy code handle streams, error statuses, drops, silences and hangs.
//! - Alternatives: a mock HTTP crate (a new dependency for what a few
//!   dozen lines of `axum` do); a trait over the client (a second code path
//!   that the real one is not).
//! - Choice: a real server on `127.0.0.1:0`, plain HTTP, any path and
//!   method, so the client under test is the one that ships, pointed at a
//!   base URL. Replies are bytes the test gives (the stream fixtures of
//!   `vessel-llm`, an error body), written in pieces so every chunk
//!   boundary is exercised; a request with nothing scripted gets a 500
//!   saying so.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::Response;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// What the fake answers one request with.
#[derive(Debug, Clone)]
pub enum Reply {
    /// 200 with a `text/event-stream` body, written `piece` bytes at a time.
    Stream {
        /// The body.
        body: Bytes,
        /// How many bytes per write (at least one).
        piece: usize,
    },
    /// `status` with a JSON body, and a `Retry-After` header if given.
    Status {
        /// The status.
        status: u16,
        /// The body.
        body: Bytes,
        /// The `Retry-After` header, in seconds.
        retry_after: Option<u64>,
    },
    /// 200, the first `cut` bytes of `body`, then the connection breaks.
    Drop {
        /// The body.
        body: Bytes,
        /// How much of it is sent.
        cut: usize,
    },
    /// 200, `body`, then silence on an open connection for `silence`.
    Stall {
        /// What is sent before the silence.
        body: Bytes,
        /// How long the silence lasts.
        silence: Duration,
    },
    /// No answer, not even headers, for `wait`.
    Hang {
        /// How long.
        wait: Duration,
    },
    /// 302 to `location` (on this same server, so a client that followed
    /// it would show up as a second request).
    Redirect {
        /// Where to.
        location: String,
    },
}

/// A request the fake received.
#[derive(Debug, Clone)]
pub struct Seen {
    /// Its path and query.
    pub path: String,
    /// Its headers, names lowercase.
    pub headers: Vec<(String, String)>,
    /// Its body.
    pub body: Bytes,
}

impl Seen {
    /// The value of header `name` (lowercase), if present.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Default)]
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    seen: Mutex<Vec<Seen>>,
}

/// The fake provider. Stops when dropped.
#[derive(Debug)]
pub struct FakeUpstream {
    addr: SocketAddr,
    script: Arc<Script>,
    task: JoinHandle<()>,
}

impl FakeUpstream {
    /// Start listening on a loopback port.
    ///
    /// # Errors
    /// The port could not be bound.
    pub async fn start() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let script = Arc::new(Script::default());
        let app = axum::Router::new()
            .fallback(answer)
            .with_state(Arc::clone(&script));
        // A server that stops fails the test's next request, which says
        // more than anything printed here would.
        let task = tokio::spawn(async move {
            let _stopped = axum::serve(listener, app).await;
        });
        Ok(Self { addr, script, task })
    }

    /// Its base URL (`http://127.0.0.1:<port>`), to which a test appends
    /// an API's version prefix where the API's base URL has one.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Queue the reply to the next request.
    pub fn reply(&self, reply: Reply) {
        self.script
            .replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(reply);
    }

    /// Every request received so far.
    #[must_use]
    pub fn seen(&self) -> Vec<Seen> {
        self.script
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn answer(State(script): State<Arc<Script>>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    script
        .seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Seen {
            path: parts
                .uri
                .path_and_query()
                .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string),
            headers: headers(&parts.headers),
            body,
        });
    let next = script
        .replies
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop_front();
    match next {
        None => respond(
            StatusCode::INTERNAL_SERVER_ERROR,
            Body::from("the fake upstream has no reply scripted"),
            None,
        ),
        Some(Reply::Stream { body, piece }) => respond(
            StatusCode::OK,
            Body::from_stream(pieces(body, piece)),
            Some(EVENTS),
        ),
        Some(Reply::Status {
            status,
            body,
            retry_after,
        }) => {
            let mut response = respond(
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Body::from(body),
                Some("application/json"),
            );
            if let Some(seconds) = retry_after {
                let _replaced = response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.into());
            }
            response
        }
        Some(Reply::Drop { body, cut }) => {
            let sent = body.slice(..cut.min(body.len()));
            // The pause lets the headers and the first bytes go out, so the
            // break comes mid-answer, not before it.
            let stream = futures_util::stream::unfold(Some(sent), |state| async move {
                if let Some(sent) = state {
                    return Some((Ok(sent), None));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                let dropped = io::Error::new(io::ErrorKind::ConnectionReset, "dropped on cue");
                Some((Err(dropped), None))
            });
            respond(StatusCode::OK, Body::from_stream(stream), Some(EVENTS))
        }
        Some(Reply::Stall { body, silence }) => {
            let stream = futures_util::stream::unfold(Some(body), move |state| async move {
                if let Some(body) = state {
                    return Some((Ok::<Bytes, io::Error>(body), None));
                }
                tokio::time::sleep(silence).await;
                None
            });
            respond(StatusCode::OK, Body::from_stream(stream), Some(EVENTS))
        }
        Some(Reply::Hang { wait }) => {
            tokio::time::sleep(wait).await;
            respond(StatusCode::GATEWAY_TIMEOUT, Body::empty(), None)
        }
        Some(Reply::Redirect { location }) => {
            let mut response = respond(StatusCode::FOUND, Body::empty(), None);
            if let Ok(location) = header::HeaderValue::from_str(&location) {
                let _replaced = response.headers_mut().insert(header::LOCATION, location);
            }
            response
        }
    }
}

const EVENTS: &str = "text/event-stream";

fn respond(status: StatusCode, body: Body, content_type: Option<&'static str>) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        let _replaced = response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(content_type),
        );
    }
    response
}

fn headers(map: &HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// `body` in pieces of `piece` bytes, yielding between them so each goes
/// out as its own write.
fn pieces(
    body: Bytes,
    piece: usize,
) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send {
    let piece = piece.max(1);
    futures_util::stream::unfold(body, move |mut rest| async move {
        if rest.is_empty() {
            return None;
        }
        let next = rest.split_to(piece.min(rest.len()));
        tokio::task::yield_now().await;
        Some((Ok(next), rest))
    })
}
