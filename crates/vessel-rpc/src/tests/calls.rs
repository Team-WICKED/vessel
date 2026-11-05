//! Calls: every method in both directions, cancellation, bad requests.

use super::*;

#[tokio::test]
async fn every_unary_method_round_trips() {
    let mut running = start(ClientConfig::default()).await;
    let client = &running.client;
    assert_eq!(client.welcome().protocol, ProtocolVersion::new(2));

    let input = SendInput {
        session: session(1),
        input: SessionInput::Terminal {
            input: TerminalInput::Data {
                data: TerminalBytes::new(bytes::Bytes::from_static(&[0xED, 0x95, 0x9C])),
            },
        },
    };
    let read = ReadEvents {
        session: session(1),
        from: EventSeq::new(3),
        limit: 10,
    };
    let create = CreateSession { spec: spec() };

    assert_eq!(client.call(GetNode).await.unwrap().name, "test-node");
    assert_eq!(client.call(ListHarnesses).await.unwrap().len(), 1);
    assert_eq!(
        client.call(ListSessions).await.unwrap(),
        vec![info(session(1), spec())]
    );
    assert_eq!(
        client
            .call(GetSession {
                session: session(1)
            })
            .await
            .unwrap(),
        info(session(1), spec())
    );
    assert_eq!(
        client.call(create.clone()).await.unwrap(),
        info(session(2), spec())
    );
    assert_eq!(client.call(input.clone()).await.unwrap(), Ack);
    let page = client.call(read).await.unwrap();
    assert_eq!(page.records, vec![record(3)]);
    assert_eq!(page.next, EventSeq::new(4));
    // `session.unsubscribe` for an unknown id surfaces the node's error code.
    let unknown = Unsubscribe {
        subscription: vessel_proto::SubscriptionId::new(77),
    };
    assert_eq!(
        client.call(unknown).await.unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );
    let missing = GetSession {
        session: session(3),
    };
    assert_eq!(
        client.call(missing).await.unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );

    // The node received exactly what was sent, in order.
    let expected = [
        GetNode.into_call(),
        ListHarnesses.into_call(),
        ListSessions.into_call(),
        GetSession {
            session: session(1),
        }
        .into_call(),
        create.into_call(),
        input.into_call(),
        read.into_call(),
        unknown.into_call(),
        missing.into_call(),
    ];
    for call in expected {
        assert_eq!(running.seen.recv().await.unwrap(), call);
    }
}

#[tokio::test]
async fn dropping_a_call_aborts_it_on_the_node() {
    let running = start(ClientConfig::default()).await;
    let client = running.client.clone();
    let call = tokio::spawn(async move {
        client
            .call(KillSession {
                session: session(1),
            })
            .await
    });
    running.probe.started.notified().await;
    call.abort();
    running.probe.aborted.notified().await;
    // The connection is still usable.
    assert_eq!(
        running.client.call(GetNode).await.unwrap().name,
        "test-node"
    );
}

#[tokio::test]
async fn unknown_methods_are_answered_by_id() {
    let (mut client_end, server_end) = pair();
    let _served = serve(server_end);
    let hello = ClientFrame::Hello(vessel_proto::Hello {
        protocol: ProtocolRange::CURRENT,
        client: client_info(),
    });
    client_end
        .send(serde_json::to_string(&hello).unwrap())
        .await
        .unwrap();
    let _welcome = client_end.next().await.unwrap().unwrap();
    let call = r#"{"type":"request","id":5,"call":{"method":"session.frobnicate","params":null}}"#;
    client_end.send(call.to_owned()).await.unwrap();
    let answer = client_end.next().await.unwrap().unwrap();
    let ServerFrame::Response {
        id,
        outcome: Outcome::Err(error),
    } = serde_json::from_str(&answer).unwrap()
    else {
        panic!("expected an error response, got {answer}");
    };
    assert_eq!(id, RequestId::new(5));
    assert_eq!(error.code, ErrorCode::InvalidRequest);
}
