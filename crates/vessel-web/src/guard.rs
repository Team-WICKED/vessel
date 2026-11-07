//! What every request passes before it reaches a handler, and what every
//! response carries (ADR 0017).
//!
//! [Decision Log]
//! - Purpose: the server drives the fleet with the user's credentials, so
//!   a web page on another site must not reach it, even through the
//!   browser of the user who runs it.
//! - DNS rebinding: a site can point its own name at `127.0.0.1` and have
//!   the browser treat our server as same-origin. Every request's `Host`
//!   must therefore be one of loopback's names with our port, else `421`.
//! - Cross-origin: a page elsewhere may send requests (a form, a script's
//!   `fetch`, a websocket). The websocket upgrade must carry our exact
//!   `Origin`; browsers always send one on a websocket, so a missing one
//!   is refused. Other API requests carry the token in a header a
//!   cross-origin page cannot set without a CORS preflight we never
//!   answer, and an `Origin` there, when present, must be ours too. No CORS
//!   header is ever sent.
//! - Headers: a Content Security Policy with no inline script and no other
//!   origin, no framing, no `Referer`, no type sniffing.
//! - Alternatives: an allowlist that accepts any literal IP (Paseo's
//!   default: a rebinding site's own IP passes); trusting a missing
//!   `Origin` on the upgrade (a non-browser client could, but it has no
//!   reason to be here without the token anyway, and browsers never omit
//!   it).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{
    CONTENT_SECURITY_POLICY, HOST, ORIGIN, REFERRER_POLICY, UPGRADE, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};

/// The policy every response carries: this origin only, no inline script,
/// no plugins, no framing.
///
/// Styles may be inline: xterm.js (the terminal view) writes its theme and
/// cell sizes into `<style>` elements it creates, and offers no nonce.
/// Scripts stay `'self'` only, so inline styles could only matter after an
/// injection of markup, which the page never renders from session content
/// (React escapes text; Markdown is rendered with raw HTML off).
const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; \
                   style-src 'self' 'unsafe-inline'; font-src 'self'; object-src 'none'; \
                   base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The names this server answers to, and the origins its pages have.
#[derive(Debug)]
pub(crate) struct Allowed {
    hosts: [String; 3],
    origins: [String; 3],
}

impl Allowed {
    /// For a server listening on `address` (loopback; its port is what
    /// matters).
    pub(crate) fn for_address(address: SocketAddr) -> Self {
        let port = address.port();
        let hosts = [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        let origins = hosts.clone().map(|host| format!("http://{host}"));
        Self { hosts, origins }
    }

    fn host(&self, headers: &HeaderMap) -> bool {
        headers
            .get(HOST)
            .and_then(|host| host.to_str().ok())
            .is_some_and(|host| self.hosts.iter().any(|allowed| allowed == host))
    }

    fn origin(&self, origin: &HeaderValue) -> bool {
        origin
            .to_str()
            .is_ok_and(|origin| self.origins.iter().any(|allowed| allowed == origin))
    }
}

/// Check `Host` and `Origin` (module doc), run the handler, add the
/// security headers.
pub(crate) async fn guard(
    State(allowed): State<Arc<Allowed>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = match refusal(&allowed, &request) {
        Some(refused) => refused,
        None => next.run(request).await,
    };
    let headers = response.headers_mut();
    let _previous = headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    let _previous = headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    let _previous = headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    let _previous = headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

/// Why `request` must not go further, if it must not.
fn refusal(allowed: &Allowed, request: &Request) -> Option<Response> {
    let headers = request.headers();
    if !allowed.host(headers) {
        return Some((StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response());
    }
    if !request.uri().path().starts_with("/api/") {
        return None;
    }
    let upgrade = headers.contains_key(UPGRADE);
    let origin_ok = match headers.get(ORIGIN) {
        Some(origin) => allowed.origin(origin),
        None => !upgrade,
    };
    (!origin_ok).then(|| (StatusCode::FORBIDDEN, "another origin").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_names_with_our_port_are_ours() {
        let allowed = Allowed::for_address("127.0.0.1:7079".parse().unwrap());
        let host = |value: &'static str| {
            let mut headers = HeaderMap::new();
            let _previous = headers.insert(HOST, HeaderValue::from_static(value));
            allowed.host(&headers)
        };
        for good in ["127.0.0.1:7079", "localhost:7079", "[::1]:7079"] {
            assert!(host(good), "{good}");
        }
        for bad in [
            "127.0.0.1:7080",
            "127.0.0.1",
            "evil.example:7079",
            "192.168.1.2:7079",
            "",
        ] {
            assert!(!host(bad), "{bad}");
        }
        assert!(!allowed.host(&HeaderMap::new()), "no Host at all");
        assert!(allowed.origin(&HeaderValue::from_static("http://localhost:7079")));
        assert!(!allowed.origin(&HeaderValue::from_static("https://localhost:7079")));
        assert!(!allowed.origin(&HeaderValue::from_static("http://evil.example:7079")));
        assert!(!allowed.origin(&HeaderValue::from_static("null")));
    }
}
