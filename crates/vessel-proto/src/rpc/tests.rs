use chrono::DateTime;

use super::*;
use crate::agent::AgentEvent;
use crate::event::{EventRecord, SessionEvent};
use crate::harness::HarnessKind;
use crate::id::{DeviceId, EventSeq, RequestId, SubscriptionId};
use crate::session::{SessionMode, SessionSpec};
use crate::terminal::TerminalSize;

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
        [
            "node.stop",
            "device.invite",
            "device.list",
            "device.revoke",
            "profile.reload",
            "fleet.members"
        ]
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

const V4: ProtocolVersion = ProtocolVersion::new(4);
const V5: ProtocolVersion = ProtocolVersion::new(5);

#[test]
fn every_method_knows_its_first_version() {
    assert_eq!(Method::NodeInfo.since(), ProtocolVersion::new(2));
    assert_eq!(Method::StopNode.since(), ProtocolVersion::new(3));
    assert_eq!(Method::RevokeDevice.since(), V4);
    for method in Method::ALL {
        assert!(method.since() <= crate::PROTOCOL_VERSION, "{method:?}");
        let fleet = method.as_str().starts_with("fleet.");
        let profile = method.as_str().starts_with("profile.") || *method == Method::MoveProfile;
        assert_eq!(method.since() == V5, fleet || profile, "{method:?}");
        assert_eq!(
            method.scope() == Scope::Sync,
            fleet && *method != Method::ListMembers
        );
    }
}

#[test]
fn a_new_field_raises_what_a_call_needs() {
    let terminal = SessionMode::Terminal {
        args: Vec::new(),
        size: TerminalSize::default(),
    };
    let plain = SessionSpec::new(HarnessKind::Codex, terminal);
    let create = |spec| CreateSession { spec }.into_call();
    assert_eq!(create(plain.clone()).since(), ProtocolVersion::new(2));
    let profiled = plain.with_profile("alt".parse().unwrap());
    assert_eq!(create(profiled).since(), V5);

    let invite = |scopes| {
        InviteDevice {
            label: None,
            ttl_secs: 60,
            scopes,
        }
        .into_call()
    };
    assert_eq!(invite(None).since(), V4);
    assert_eq!(invite(Some(vec![Scope::Sync])).since(), V5);
}

fn record(seq: u64, event: SessionEvent) -> EventRecord {
    EventRecord {
        seq: EventSeq::new(seq),
        at: DateTime::UNIX_EPOCH,
        event,
    }
}

fn moved() -> SessionEvent {
    SessionEvent::ProfileChanged {
        from: None,
        to: Some("alt".parse().unwrap()),
    }
}

#[test]
fn older_connections_get_stand_ins_at_the_same_position() {
    let event = ServerFrame::Event {
        subscription: SubscriptionId::new(1),
        record: record(7, moved()),
    };
    assert_eq!(event.for_protocol(V5), None);
    let Some(ServerFrame::Event {
        record: stand_in, ..
    }) = event.for_protocol(V4)
    else {
        panic!("an event frame stays an event frame");
    };
    assert_eq!(stand_in.seq, EventSeq::new(7));
    assert_eq!(
        stand_in.event,
        SessionEvent::Agent {
            event: AgentEvent::HarnessLog {
                line: "vessel: profile changed from default to alt".to_owned()
            }
        }
    );

    let status = record_status();
    let page = |records| ServerFrame::Response {
        id: RequestId::new(3),
        outcome: Outcome::Ok(Reply::ReadEvents(EventPage {
            records,
            next: EventSeq::new(9),
            at_end: true,
        })),
    };
    assert_eq!(page(vec![status.clone()]).for_protocol(V4), None);
    let Some(ServerFrame::Response {
        outcome: Outcome::Ok(Reply::ReadEvents(downgraded)),
        ..
    }) = page(vec![status.clone(), record(8, moved())]).for_protocol(V4)
    else {
        panic!("a page stays a page");
    };
    assert_eq!(downgraded.records.first(), Some(&status));
    assert_eq!(
        downgraded.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
        [EventSeq::new(1), EventSeq::new(8)]
    );
    let text = serde_json::to_string(&downgraded).unwrap();
    assert!(!text.contains("profile_changed"), "{text}");
}

fn record_status() -> EventRecord {
    record(
        1,
        SessionEvent::Status {
            status: crate::session::SessionStatus::Running,
        },
    )
}

#[test]
fn older_connections_do_not_see_newer_scopes() {
    let device = DeviceInfo {
        id: DeviceId::random(),
        label: "member".to_owned(),
        created_at: DateTime::UNIX_EPOCH,
        last_seen: None,
        scopes: vec![Scope::Read, Scope::Sync],
        revoked_at: None,
    };
    let frame = ServerFrame::Response {
        id: RequestId::new(1),
        outcome: Outcome::Ok(Reply::ListDevices(vec![device])),
    };
    assert_eq!(frame.for_protocol(V5), None);
    let Some(ServerFrame::Response {
        outcome: Outcome::Ok(Reply::ListDevices(devices)),
        ..
    }) = frame.for_protocol(V4)
    else {
        panic!("a device list stays a device list");
    };
    assert_eq!(
        devices
            .iter()
            .flat_map(|d| d.scopes.clone())
            .collect::<Vec<_>>(),
        [Scope::Read]
    );
}
