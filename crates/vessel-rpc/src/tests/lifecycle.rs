//! Connection lifecycle: handshake outcomes, disconnects and shutdown.

use super::*;

#[tokio::test]
async fn protocol_mismatch_is_rejected() {
    let (mut client_end, server_end) = pair();
    let served = serve(server_end).task;
    let hello =
        r#"{"type":"hello","protocol":{"min":6,"max":7},"client":{"name":"future","version":"9"}}"#;
    client_end.send(hello.to_owned()).await.unwrap();
    let answer = client_end.next().await.unwrap().unwrap();
    let ServerFrame::Rejected { error } = serde_json::from_str(&answer).unwrap() else {
        panic!("expected rejected, got {answer}");
    };
    assert_eq!(error.code, ErrorCode::ProtocolMismatch);
    assert!(
        client_end.next().await.is_none(),
        "node closes after rejecting"
    );
    assert!(matches!(
        served.await.unwrap(),
        Err(ServeError::Rejected(_))
    ));
}

#[tokio::test]
async fn client_reports_rejection() {
    let (client_end, mut server_end) = pair();
    let node_side = tokio::spawn(async move {
        let _hello = server_end.next().await;
        let rejected = ServerFrame::Rejected {
            error: WireError::new(ErrorCode::ProtocolMismatch, "too old"),
        };
        server_end
            .send(serde_json::to_string(&rejected).unwrap())
            .await
            .unwrap();
    });
    let result = Connection::connect(client_end, client_info(), &ClientConfig::default()).await;
    let Err(ConnectError::Rejected(error)) = result else {
        panic!("expected a rejection, got {result:?}");
    };
    assert_eq!(error.code, ErrorCode::ProtocolMismatch);
    node_side.await.unwrap();
}

/// A node may refuse before reading `Hello` and close at once (a wrong
/// token on the local socket, ADR 0007), so sending `Hello` fails. The
/// refusal already on its way says why; it wins over the broken pipe.
#[tokio::test]
async fn a_refusal_is_reported_even_when_hello_cannot_be_sent() {
    let (client_end, mut server_end) = pair();
    let rejected = ServerFrame::Rejected {
        error: WireError::new(ErrorCode::Unauthorized, "missing or wrong token"),
    };
    server_end
        .send(serde_json::to_string(&rejected).unwrap())
        .await
        .unwrap();
    drop(server_end);
    let result = Connection::connect(client_end, client_info(), &ClientConfig::default()).await;
    let Err(ConnectError::Rejected(error)) = result else {
        panic!("expected a rejection, got {result:?}");
    };
    assert_eq!(error.code, ErrorCode::Unauthorized);
}

/// A call still running when the server begins shutting down is answered
/// before the connection closes: `node.stop{now}` starts the shutdown from
/// inside its own call, and its client must still get the `Ack`.
#[tokio::test]
async fn a_call_running_at_shutdown_is_still_answered() {
    let running = start(ClientConfig::default()).await;
    assert_eq!(
        running
            .client
            .call(StopNode {
                when: StopWhen::Now
            })
            .await,
        Ok(Ack)
    );
    running.served.await.unwrap().unwrap();
    running.client.closed().await;
}

/// A call that does not finish within the close timeout is given up: its
/// client sees `Disconnected`, and the shutdown does not wait for it.
#[tokio::test(start_paused = true)]
async fn disconnect_fails_pending_calls() {
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
    running.shutdown.cancel();
    assert_eq!(call.await.unwrap(), Err(CallError::Disconnected));
    running.served.await.unwrap().unwrap();
    running.client.closed().await;
    assert_eq!(
        running.client.call(GetNode).await,
        Err(CallError::Disconnected)
    );
}

#[tokio::test(start_paused = true)]
async fn silent_clients_time_out() {
    let (_client_end, server_end) = pair();
    let served = serve(server_end).task;
    assert!(matches!(
        served.await.unwrap(),
        Err(ServeError::HandshakeTimeout)
    ));
}

#[tokio::test(start_paused = true)]
async fn shutdown_does_not_wait_for_a_client_that_stopped_reading() {
    let (mut client_end, server_end) = pair();
    let served = serve(server_end);
    let hello = ClientFrame::Hello(vessel_proto::Hello {
        protocol: ProtocolRange::CURRENT,
        client: client_info(),
    });
    client_end
        .send(serde_json::to_string(&hello).unwrap())
        .await
        .unwrap();
    let _welcome = client_end.next().await.unwrap().unwrap();
    // Far more answers than the transport buffers, and never read.
    for id in 0..64 {
        let request = ClientFrame::Request {
            id: RequestId::new(id),
            call: GetNode.into_call(),
        };
        client_end
            .send(serde_json::to_string(&request).unwrap())
            .await
            .unwrap();
    }
    served.shutdown.cancel();
    served.task.await.unwrap().unwrap();
}

/// A frame the writer already took off its queue but could not write yet
/// (the client was not reading) when the shutdown began is still written,
/// in order, by the close: no answer goes missing.
#[tokio::test]
async fn a_frame_waiting_for_the_client_when_shutdown_begins_is_not_lost() {
    let (mut client_end, server_end) = pair();
    let mut served = serve(server_end);
    let hello = ClientFrame::Hello(vessel_proto::Hello {
        protocol: ProtocolRange::CURRENT,
        client: client_info(),
    });
    client_end
        .send(serde_json::to_string(&hello).unwrap())
        .await
        .unwrap();
    let _welcome = client_end.next().await.unwrap().unwrap();
    // More answers than the transport buffers (16), not read yet: the
    // writer ends up holding one it cannot write.
    let calls = 20;
    for id in 0..calls {
        let request = ClientFrame::Request {
            id: RequestId::new(id),
            call: GetNode.into_call(),
        };
        client_end
            .send(serde_json::to_string(&request).unwrap())
            .await
            .unwrap();
    }
    for _ in 0..calls {
        let _seen = served.seen.recv().await.unwrap();
    }
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    served.shutdown.cancel();
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    let mut answered = Vec::new();
    while let Some(frame) = client_end.next().await {
        let Ok(ServerFrame::Response { id, .. }) = serde_json::from_str(&frame.unwrap()) else {
            panic!("expected only responses");
        };
        answered.push(id);
    }
    assert_eq!(answered, (0..calls).map(RequestId::new).collect::<Vec<_>>());
    served.task.await.unwrap().unwrap();
}
