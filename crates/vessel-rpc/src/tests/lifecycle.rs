//! Connection lifecycle: handshake outcomes, disconnects and shutdown.

use super::*;

#[tokio::test]
async fn protocol_mismatch_is_rejected() {
    let (mut client_end, server_end) = pair();
    let served = serve(server_end).task;
    let hello =
        r#"{"type":"hello","protocol":{"min":3,"max":4},"client":{"name":"future","version":"9"}}"#;
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

#[tokio::test]
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
