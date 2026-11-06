//! A secure channel is a `FrameTransport`: the RPC layer runs on it as is.

use futures_util::{SinkExt as _, StreamExt as _};
use vessel_proto::{ClientFrame, ClientInfo, ErrorCode, ServerFrame, WireError};
use vessel_rpc::{ClientConfig, ConnectError, Connection, FrameTransport};

use super::support::connected;
use crate::{MemoryCarrier, SecureChannel};

#[test]
fn a_secure_channel_is_a_frame_transport() {
    fn transport<T: FrameTransport>() {}
    transport::<SecureChannel<MemoryCarrier>>();
}

#[tokio::test]
async fn an_rpc_connection_runs_over_a_secure_channel() {
    let (client, mut node) = connected().await;
    let node_side = async {
        let hello = node.next().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<ClientFrame>(&hello).unwrap(),
            ClientFrame::Hello(_)
        ));
        let refusal = ServerFrame::Rejected {
            error: WireError::new(ErrorCode::Unauthorized, "through the channel"),
        };
        node.send(serde_json::to_string(&refusal).unwrap())
            .await
            .unwrap();
        node
    };
    let info = ClientInfo {
        name: "vessel-secure-test".to_owned(),
        version: "0".to_owned(),
    };
    let config = ClientConfig::default();
    let (connected, _node) = tokio::join!(Connection::connect(client, info, &config), node_side);
    let Err(ConnectError::Rejected(error)) = connected else {
        panic!("expected the node's refusal to arrive through the channel");
    };
    assert_eq!(error.message, "through the channel");
}
