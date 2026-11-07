//! The web UI's files, embedded at build time (`build.rs`), and how they
//! are served.
//!
//! The app routes in the browser, so a path that names no file and has no
//! extension (`/nodes/gpu-box/sessions`) is answered with `index.html`;
//! one with an extension that is not there (`/missing.js`) is a `404`, so a
//! broken bundle shows as a missing file rather than as a page. Hashed
//! assets revalidate even when their filenames are hashed: a different
//! process could have held this port and poisoned the browser cache;
//! `index.html` is never cached, so a new build is picked up at once.
//! Nothing here touches the disk: the table is all there is.

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse as _, Response};

/// One embedded file.
#[derive(Debug)]
pub(crate) struct File {
    /// Its URL path, from `/`.
    pub(crate) path: &'static str,
    /// Its `Content-Type`.
    pub(crate) kind: &'static str,
    /// Its content.
    pub(crate) etag: &'static str,
    /// Content bytes.
    pub(crate) bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/files.rs"));

/// The file at `path`, if embedded (`FILES` is sorted by path).
fn find(path: &str) -> Option<&'static File> {
    FILES
        .binary_search_by(|file| file.path.cmp(path))
        .ok()
        .and_then(|index| FILES.get(index))
}

/// Answer a request for a file.
pub(crate) async fn serve(method: Method, uri: Uri, request: HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path();
    if path.starts_with("/api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let last = path.rsplit('/').next().unwrap_or_default();
    let file = match find(path) {
        Some(file) => file,
        None if path == "/" || !last.contains('.') => match find("/index.html") {
            Some(index) => index,
            None => return StatusCode::NOT_FOUND.into_response(),
        },
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let index = file.path == "/index.html";
    let cache = if index { "no-store" } else { "no-cache" };
    let unchanged = !index
        && request
            .get(IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|tag| tag.trim().trim_start_matches("W/") == file.etag)
            });
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else if method == Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        file.bytes.into_response()
    };
    let headers = response.headers_mut();
    let _previous = headers.insert(CONTENT_TYPE, HeaderValue::from_static(file.kind));
    let _previous = headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    if !index {
        let _previous = headers.insert(ETAG, HeaderValue::from_static(file.etag));
    }
    response
}

/// Whether the real bundle is embedded (else the placeholder page that
/// says how to build it).
pub(crate) const fn bundled() -> bool {
    BUNDLED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_has_an_index() {
        assert!(FILES.windows(2).all(|pair| pair[0].path < pair[1].path));
        let index = find("/index.html").unwrap();
        assert_eq!(index.kind, "text/html; charset=utf-8");
        assert!(FILES.iter().all(|file| file.path.starts_with('/')));
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[tokio::test]
    async fn assets_revalidate_by_content_and_index_never_revalidates() {
        let file = FILES
            .iter()
            .find(|file| file.path != "/index.html")
            .unwrap();
        let response = serve(Method::GET, file.path.parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(response.headers().get(CACHE_CONTROL).unwrap(), "no-cache");
        assert_eq!(response.headers().get(ETAG).unwrap(), file.etag);
        let mut request = HeaderMap::new();
        let _previous = request.insert(IF_NONE_MATCH, HeaderValue::from_static(file.etag));
        let response = serve(Method::GET, file.path.parse().unwrap(), request.clone()).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let index = find("/index.html").unwrap();
        let _previous = request.insert(IF_NONE_MATCH, HeaderValue::from_static(index.etag));
        let response = serve(Method::GET, "/".parse().unwrap(), request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(CACHE_CONTROL).unwrap(), "no-store");
    }
}
