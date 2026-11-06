use super::*;
use crate::id::RequestId;

#[test]
fn unit_requests_round_trip() {
    let frame = ClientFrame::Request {
        id: RequestId::new(7),
        call: GetNode.into_call(),
    };
    let json = serde_json::to_string(&frame).unwrap();
    assert_eq!(
        json,
        r#"{"type":"request","id":7,"call":{"method":"node.info","params":null}}"#
    );
    assert_eq!(serde_json::from_str::<ClientFrame>(&json).unwrap(), frame);
}

#[test]
fn every_method_declares_a_scope_and_admin_is_rare() {
    let admin: Vec<_> = Method::ALL
        .iter()
        .filter(|method| method.scope() == Scope::Admin)
        .map(|method| method.as_str())
        .collect();
    assert_eq!(
        admin,
        ["node.stop", "device.invite", "device.list", "device.revoke"]
    );
    assert_eq!(Method::SendInput.scope(), Scope::Operate);
    assert_eq!(Method::ReadEvents.scope(), Scope::Read);
}

#[test]
fn replies_bind_to_their_request() {
    let reply = Reply::SendInput(Ack);
    assert_eq!(SendInput::response_from(reply.clone()), Ok(Ack));
    assert_eq!(
        KillSession::response_from(reply),
        Err(UnexpectedReply {
            expected: Method::KillSession,
            got: Method::SendInput,
        })
    );
}
