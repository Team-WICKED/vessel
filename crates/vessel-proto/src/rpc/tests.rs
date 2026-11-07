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
            "fleet.members",
            "fleet.forget",
            "memory.export",
            "memory.import",
            "secret.set",
            "secret.list",
            "secret.delete",
            "secret.copy"
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
const V6: ProtocolVersion = ProtocolVersion::new(6);

#[test]
fn every_method_knows_its_first_version() {
    assert_eq!(Method::NodeInfo.since(), ProtocolVersion::new(2));
    assert_eq!(Method::StopNode.since(), ProtocolVersion::new(3));
    assert_eq!(Method::RevokeDevice.since(), V4);
    let v5_fleet = [
        Method::GetManifest,
        Method::ReadBlob,
        Method::WatchFleet,
        Method::ReportApplied,
        Method::ListMembers,
    ];
    for method in Method::ALL {
        assert!(method.since() <= crate::PROTOCOL_VERSION, "{method:?}");
        let name = method.as_str();
        let fleet = name.starts_with("fleet.");
        let profile = name.starts_with("profile.") || *method == Method::MoveProfile;
        let v6 = (fleet && !v5_fleet.contains(method))
            || ["session.export", "session.import"].contains(&name)
            || ["memory.", "transfer.", "secret."]
                .iter()
                .any(|prefix| name.starts_with(prefix));
        assert_eq!(method.since() == V6, v6, "{method:?}");
        assert_eq!(
            method.since() == V5,
            (fleet || profile) && !v6,
            "{method:?}"
        );
    }
}

#[test]
fn every_hop_of_the_envelope_has_its_own_scope() {
    // What a member holds on its home: `sync` for the bundle, `relay` only
    // when its owner asked; `relay` opens nothing but passing calls on.
    for method in Method::ALL {
        assert_eq!(
            method.scope() == Scope::Relay,
            *method == Method::Relay,
            "{method:?}"
        );
    }
    assert_eq!(Method::Forward.scope(), Scope::Read);
    assert_eq!(Method::Deliver.scope(), Scope::Read);
    // Every secret row is `admin`, and no row returns a value.
    for method in [
        Method::SetSecret,
        Method::ListSecrets,
        Method::DeleteSecret,
        Method::CopySecret,
    ] {
        assert_eq!(method.scope(), Scope::Admin, "{method:?}");
    }
    // Memory is kept per account on purpose: moving it is `admin`.
    assert_eq!(Method::ExportMemory.scope(), Scope::Admin);
    assert_eq!(Method::ImportMemory.scope(), Scope::Admin);
    assert_eq!(Method::ExportSession.scope(), Scope::Operate);
}

#[test]
fn the_forwardable_set_is_pinned() {
    // Changing this list is a decision about what other nodes may do here
    // (`Method::forwardable` says why each group is in or out).
    let forwardable: Vec<_> = Method::ALL
        .iter()
        .filter(|method| method.forwardable())
        .map(|method| method.as_str())
        .collect();
    assert_eq!(
        forwardable,
        [
            "node.info",
            "harness.list",
            "session.list",
            "session.get",
            "session.create",
            "session.send",
            "session.kill",
            "session.read",
            "profile.list",
            "session.move_profile",
            "session.export",
            "session.import",
            "memory.export",
            "memory.import",
            "transfer.read",
            "transfer.close",
            "secret.set",
            "secret.list"
        ]
    );
}

#[test]
fn a_forwarded_call_nests_its_inner_call() {
    let inner = ReadEvents {
        session: crate::id::SessionId::random(),
        from: EventSeq::ZERO,
        limit: 10,
        wait: Some(30),
    }
    .into_call();
    let call = Forward {
        node: crate::id::NodeId::random(),
        call: Box::new(inner.clone()),
    }
    .into_call();
    let json = serde_json::to_string(&call).unwrap();
    assert!(
        json.starts_with(r#"{"method":"fleet.forward","params":{"node":""#),
        "{json}"
    );
    assert!(
        json.contains(r#""call":{"method":"session.read""#),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<Call>(&json).unwrap(), call);
    // The envelope needs 6 on this connection; the inner call is checked
    // where it is delivered.
    assert_eq!(call.since(), V6);
    assert_eq!(inner.since(), V6);
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
    // A protocol 5 node would refuse a scope it does not know.
    assert_eq!(invite(Some(vec![Scope::Sync, Scope::Relay])).since(), V6);

    let read = |wait| {
        ReadEvents {
            session: crate::id::SessionId::random(),
            from: EventSeq::ZERO,
            limit: 10,
            wait,
        }
        .into_call()
    };
    assert_eq!(read(None).since(), ProtocolVersion::new(2));
    assert_eq!(read(Some(30)).since(), V6);
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

/// Protocol 7's model call: a protocol 6 client gets a line with the
/// model and the token counts in its place, or the failure's class; never
/// anything else of the call.
#[test]
fn a_model_call_reaches_older_clients_as_a_line() {
    let call: crate::model::ModelCall = serde_json::from_str(
        r#"{"route":"qwen-coder","model":"qwen/qwen3-coder","api":"openai_chat_completions",
            "usage":{"input":72,"cache_read":128,"output":50},"stop":"tool_use","duration_ms":900}"#,
    )
    .unwrap();
    let event = SessionEvent::ModelCall(call.clone());
    assert_eq!(event.for_protocol(ProtocolVersion::new(7)), None);
    assert_eq!(
        event.for_protocol(V6),
        Some(SessionEvent::Agent {
            event: AgentEvent::HarnessLog {
                line: "vessel: model call to qwen/qwen3-coder: 200 in, 50 out".to_owned()
            }
        })
    );
    let failed = SessionEvent::ModelCall(crate::model::ModelCall {
        stop: crate::model::ModelStop::Error,
        failure: Some(crate::model::ModelFailure::ContextTooLong),
        ..call
    });
    assert_eq!(
        failed.for_protocol(V4),
        Some(SessionEvent::Agent {
            event: AgentEvent::HarnessLog {
                line: "vessel: model call to qwen/qwen3-coder failed (context_too_long)".to_owned()
            }
        })
    );
    let text = serde_json::to_string(&failed).unwrap();
    assert!(
        text.starts_with(r#"{"type":"model_call","route":"qwen-coder""#),
        "{text}"
    );
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
        scopes: vec![Scope::Read, Scope::Sync, Scope::Relay],
        revoked_at: None,
    };
    let frame = ServerFrame::Response {
        id: RequestId::new(1),
        outcome: Outcome::Ok(Reply::ListDevices(vec![device])),
    };
    assert_eq!(frame.for_protocol(V6), None);
    let scopes_at = |protocol| {
        let Some(ServerFrame::Response {
            outcome: Outcome::Ok(Reply::ListDevices(devices)),
            ..
        }) = frame.for_protocol(protocol)
        else {
            panic!("a device list stays a device list");
        };
        devices
            .iter()
            .flat_map(|d| d.scopes.clone())
            .collect::<Vec<_>>()
    };
    // A protocol 5 client cannot decode `relay`, a protocol 4 one `sync`.
    assert_eq!(scopes_at(V5), [Scope::Read, Scope::Sync]);
    assert_eq!(scopes_at(V4), [Scope::Read]);
}

#[test]
fn where_a_session_came_from_is_optional_and_tagged() {
    let spec = SessionSpec::new(
        HarnessKind::ClaudeCode,
        SessionMode::Structured {
            model: None,
            resume: None,
        },
    );
    let mut info = SessionInfo::new(crate::id::SessionId::random(), spec, DateTime::UNIX_EPOCH);
    // A session of this node only looks as it did in protocol 5.
    let local = serde_json::to_string(&info).unwrap();
    assert!(
        !local.contains("via") && !local.contains("origin"),
        "{local}"
    );
    info.via = Some(crate::id::NodeId::random());
    info.origin = Some(crate::session::SessionOrigin::Imported {
        node: crate::id::NodeId::random(),
        session: crate::id::SessionId::random(),
    });
    let imported = serde_json::to_string(&info).unwrap();
    assert!(
        imported.contains(r#""origin":{"kind":"imported","node":""#),
        "{imported}"
    );
    assert_eq!(
        serde_json::from_str::<SessionInfo>(&imported).unwrap(),
        info
    );
}
