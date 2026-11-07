//! Protocol 5 against older connections: a raw client that offers only one
//! version, so the server cannot negotiate up.

use vessel_proto::{CreateSession, GetManifest, Hello};

use super::*;

/// A raw client speaking only `version`, past the welcome.
async fn raw_client(version: u32) -> (MemoryTransport, Served) {
    let (mut client_end, server_end) = pair();
    let served = serve(server_end);
    let version = ProtocolVersion::new(version);
    let hello = ClientFrame::Hello(Hello {
        protocol: ProtocolRange {
            min: version,
            max: version,
        },
        client: client_info(),
    });
    client_end
        .send(serde_json::to_string(&hello).unwrap())
        .await
        .unwrap();
    let welcome = client_end.next().await.unwrap().unwrap();
    let Ok(ServerFrame::Welcome(welcome)) = serde_json::from_str::<ServerFrame>(&welcome) else {
        panic!("expected a welcome, got {welcome}");
    };
    assert_eq!(welcome.protocol, version);
    (client_end, served)
}

/// Send one call and return the text of the next frame.
async fn call_raw(client: &mut MemoryTransport, id: u64, call: Call) -> String {
    let request = ClientFrame::Request {
        id: RequestId::new(id),
        call,
    };
    client
        .send(serde_json::to_string(&request).unwrap())
        .await
        .unwrap();
    client.next().await.unwrap().unwrap()
}

fn refusal(text: &str) -> WireError {
    let Ok(ServerFrame::Response {
        outcome: Outcome::Err(error),
        ..
    }) = serde_json::from_str::<ServerFrame>(text)
    else {
        panic!("expected an error response, got {text}");
    };
    error
}

/// Neither a method from a later version nor a field from a later version
/// reaches the handler on an older connection.
#[tokio::test]
async fn an_older_connection_cannot_use_newer_methods_or_fields() {
    let (mut client, mut served) = raw_client(4).await;

    let error = refusal(&call_raw(&mut client, 1, GetManifest.into_call()).await);
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert!(
        error.message.contains("needs protocol 5"),
        "{}",
        error.message
    );

    let spec = spec().with_profile("alt".parse().unwrap());
    let error = refusal(&call_raw(&mut client, 2, CreateSession { spec }.into_call()).await);
    assert_eq!(error.code, ErrorCode::Unsupported);

    // A call this version has still works on the same connection.
    let text = call_raw(&mut client, 3, GetNode.into_call()).await;
    assert!(text.contains(r#""outcome":{"ok""#), "{text}");
    assert_eq!(served.seen.recv().await.unwrap().method(), GetNode::METHOD);
    assert!(served.seen.try_recv().is_err());
}

/// An event from a later version reaches an older connection as its
/// stand-in, at the same position; a current connection gets the event.
#[tokio::test]
async fn an_older_connection_reads_a_stand_in_for_a_newer_event() {
    for (version, present, absent) in [
        (
            4,
            "vessel: profile changed from default to alt",
            "profile_changed",
        ),
        (5, "profile_changed", "harness_log"),
    ] {
        let (mut client, _served) = raw_client(version).await;
        let read = ReadEvents {
            session: moved(),
            from: EventSeq::new(2),
            limit: 10,
            wait: None,
        };
        let text = call_raw(&mut client, 1, read.into_call()).await;
        assert!(text.contains(present), "protocol {version}: {text}");
        assert!(!text.contains(absent), "protocol {version}: {text}");
        assert!(text.contains(r#""seq":2"#), "protocol {version}: {text}");
    }
}

/// `call_any` holds an already-built call to the negotiated version, as
/// typed calls are: one newer than the connection is not sent.
#[tokio::test]
async fn an_already_built_call_newer_than_the_connection_is_not_sent() {
    let (client_end, mut server_end) = pair();
    // A node that speaks protocol 4 only.
    let older_node = async move {
        let hello = server_end.next().await.unwrap().unwrap();
        let _hello: ClientFrame = serde_json::from_str(&hello).unwrap();
        let welcome = ServerFrame::Welcome(vessel_proto::Welcome {
            protocol: ProtocolVersion::new(4),
            node: node(),
        });
        server_end
            .send(serde_json::to_string(&welcome).unwrap())
            .await
            .unwrap();
        server_end
    };
    let config = ClientConfig::default();
    let (client, _server_end) = tokio::join!(
        Connection::connect(client_end, client_info(), &config),
        older_node
    );
    let client = client.unwrap();
    let call = UnaryCall::try_from(ListProfiles { harness: None }.into_call()).unwrap();
    assert!(matches!(
        client.call_any(call).await,
        Err(CallError::NotInProtocol {
            method: vessel_proto::Method::ListProfiles,
            ..
        })
    ));
}
