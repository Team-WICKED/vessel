//! Calls that need more than the stream tests show: `node.stop`.

use futures_util::{SinkExt as _, StreamExt as _};
use vessel_proto::{ProtocolVersion, ServerFrame, Welcome};

use super::*;

#[tokio::test]
async fn stop_node_reaches_the_node() {
    let node = Arc::new(FakeNode::new(0));
    let client = connect(Arc::clone(&node)).await;
    client.stop_node(StopWhen::WhenIdle).await.unwrap();
    assert_eq!(*node.stopped.lock().unwrap(), Some(StopWhen::WhenIdle));
    client.stop_node(StopWhen::Now).await.unwrap();
    assert_eq!(*node.stopped.lock().unwrap(), Some(StopWhen::Now));
}

/// A node that speaks only protocol 2 is never sent `node.stop`.
#[tokio::test]
async fn stop_node_is_refused_locally_on_protocol_2() {
    let (client_end, mut node_end) = vessel_rpc::memory::pair(NonZeroUsize::new(4).unwrap());
    let old_node = tokio::spawn(async move {
        let _hello = node_end.next().await.unwrap().unwrap();
        let welcome = ServerFrame::Welcome(Welcome {
            protocol: ProtocolVersion::new(2),
            node: node(),
        });
        node_end
            .send(serde_json::to_string(&welcome).unwrap())
            .await
            .unwrap();
        // Anything sent after the handshake would arrive here.
        node_end.next().await
    });
    let client = NodeClient::over(client_end, client_info(), &ClientConfig::default())
        .await
        .unwrap();
    assert_eq!(client.protocol(), ProtocolVersion::new(2));
    let error = client.stop_node(StopWhen::Now).await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported));
    drop(client);
    assert!(old_node.await.unwrap().is_none(), "nothing was sent");
}
