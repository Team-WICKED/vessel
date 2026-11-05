//! The bearer-token check on `/rpc` (ADR 0005).
//!
//! Done on the HTTP upgrade request, before any protocol frame is read, so
//! an unauthenticated peer never reaches the RPC layer.

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use vessel_store::NodeToken;

/// Whether `headers` carry `Authorization: Bearer <token>` with this node's
/// token. The scheme name is case-insensitive (RFC 9110); the token is
/// compared in constant time.
pub(crate) fn authorized(headers: &HeaderMap, token: &NodeToken) -> bool {
    let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some((scheme, presented)) = value.trim().split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && token.verify(presented.trim())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        drop(headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap()));
        headers
    }

    #[test]
    fn accepts_only_this_nodes_token() {
        let dir = tempfile::tempdir().unwrap();
        let token = vessel_store::DataDir::new(dir.path())
            .load_or_create_token()
            .unwrap();
        let right = format!("Bearer {}", token.expose());
        assert!(authorized(&headers(&right), &token));
        assert!(authorized(
            &headers(&right.replace("Bearer", "bearer")),
            &token
        ));
        assert!(!authorized(&headers("Bearer nope"), &token));
        assert!(!authorized(&headers(token.expose()), &token));
        assert!(!authorized(&HeaderMap::new(), &token));
    }
}
