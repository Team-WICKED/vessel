//! `NodeClient::connect` over a real websocket on loopback: the token goes
//! in the `Authorization` header and a refusal is reported as such.

use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

use super::*;
use crate::ws::WsTransport;
use crate::{BearerToken, ConnectError, Connector as _, NodeUrl, WsConnector};

const TOKEN: &str = "s3cret-token";

/// Accept one websocket on loopback, refusing it unless it carries `TOKEN`,
/// and serve the fake node on it. Returns the URL to connect to.
async fn serve_one() -> NodeUrl {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(tokio::spawn(async move {
        let (socket, _peer) = listener.accept().await.unwrap();
        let check = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
            let expected = format!("Bearer {TOKEN}");
            if request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                == Some(expected.as_str())
            {
                Ok(response)
            } else {
                let mut refusal = ErrorResponse::new(None);
                *refusal.status_mut() = StatusCode::UNAUTHORIZED;
                Err(refusal)
            }
        };
        let Ok(websocket) = tokio_tungstenite::accept_hdr_async(socket, check).await else {
            return;
        };
        let served = server(Arc::new(FakeNode::new(0)))
            .serve(WsTransport::new(websocket), |peer| Context {
                outbound: peer.outbound,
            })
            .await;
        served.unwrap();
    }));
    format!("http://{address}").parse().unwrap()
}

#[tokio::test]
async fn connects_with_the_token_and_streams() {
    let url = serve_one().await;
    let token: BearerToken = TOKEN.parse().unwrap();
    let client = NodeClient::connect(&url, &token, client_info())
        .await
        .unwrap();
    assert_eq!(client.node_info().await.unwrap().name, "fake");
    let mut stream = client.follow(session_id(), EventSeq::ZERO).await.unwrap();
    let mut seen = 0;
    while let Some(record) = stream.next().await {
        assert_eq!(record.unwrap().seq.get(), seen);
        seen = seen.saturating_add(1);
    }
    assert_eq!(seen, LOG_LEN);
}

#[tokio::test]
async fn a_wrong_token_is_reported_as_unauthorized() {
    let url = serve_one().await;
    let token: BearerToken = "wrong".parse().unwrap();
    let result = NodeClient::connect(&url, &token, client_info()).await;
    assert!(
        matches!(result, Err(ConnectError::Unauthorized)),
        "{result:?}"
    );
}

#[test]
fn tokens_are_never_printed() {
    let token: BearerToken = TOKEN.parse().unwrap();
    assert_eq!(format!("{token:?}"), "BearerToken(..)");
    assert!(" ".parse::<BearerToken>().is_err());
    assert!("has space".parse::<BearerToken>().is_err());
}

#[tokio::test]
async fn tls_urls_are_refused_before_any_tls_code_runs() {
    let url: NodeUrl = "https://example.invalid".parse().unwrap();
    let token: BearerToken = TOKEN.parse().unwrap();
    let result = NodeClient::connect(&url, &token, client_info()).await;
    assert!(
        matches!(result, Err(ConnectError::TlsUnsupported)),
        "{result:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_node_that_never_answers_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Accepts the TCP connection, then never speaks HTTP.
    let silent = tokio::spawn(async move { listener.accept().await });
    let url: NodeUrl = address.to_string().parse().unwrap();
    let token: BearerToken = TOKEN.parse().unwrap();
    let config = ClientConfig {
        handshake_timeout: std::time::Duration::from_secs(5),
        ..ClientConfig::default()
    };
    let result = NodeClient::connect_with(&url, &token, client_info(), &config).await;
    assert!(
        matches!(result, Err(ConnectError::Timeout { .. })),
        "{result:?}"
    );
    drop(silent);
}

/// `WsConnector` dials like `connect`, and says which failures are worth
/// another try: a node that is not there (yet) is; a refused token is not.
#[tokio::test]
async fn the_websocket_connector_classifies_its_failures() {
    let url = serve_one().await;
    let connector = WsConnector::new(url.clone(), TOKEN.parse().unwrap());
    assert_eq!(connector.url(), &url);
    let client = connector.connect(client_info()).await.unwrap();
    assert_eq!(client.node().name, "fake");

    let refused = WsConnector::new(serve_one().await, "wrong".parse().unwrap());
    let error = refused.connect(client_info()).await.unwrap_err();
    assert!(matches!(error, ConnectError::Unauthorized), "{error:?}");
    assert!(!refused.is_transient(&error));

    // A port nothing listens on: bind one, then close it.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let absent = WsConnector::new(address.to_string().parse().unwrap(), TOKEN.parse().unwrap());
    let error = absent.connect(client_info()).await.unwrap_err();
    assert!(matches!(error, ConnectError::Websocket { .. }), "{error:?}");
    assert!(absent.is_transient(&error));
}
