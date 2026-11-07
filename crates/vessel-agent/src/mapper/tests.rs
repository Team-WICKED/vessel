//! Fixture tests for the ACP mapping.
//!
//! Fixtures live in `tests/fixtures/acp/`: ACP v1 JSON as harness adapters
//! send it, each paired with the Vessel event it must become.
//! `session_updates.json` covers every mapped shape;
//! `claude-agent-acp-0.100.0.updates.json` is recorded from a real
//! `claude-agent-acp` 0.100.0 session (2026-11-05), trimmed: session id
//! replaced, `_meta` dropped, the account's `_auth/status_update` and the
//! user's own slash commands removed.

use serde::Deserialize;

use super::*;

const SESSION_UPDATES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/acp/session_updates.json"
));
const RECORDED_CLAUDE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/acp/claude-agent-acp-0.100.0.updates.json"
));
const PERMISSION_REQUEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/acp/permission_request.json"
));

#[derive(Deserialize)]
struct Case {
    name: String,
    notification: acp::SessionNotification,
    #[serde(default)]
    expected: Option<AgentEvent>,
    #[serde(default)]
    skipped: Option<String>,
}

#[test]
fn session_update_fixtures_map_as_recorded() {
    for fixture in [SESSION_UPDATES, RECORDED_CLAUDE] {
        check(fixture);
    }
}

fn check(fixture: &str) {
    let cases: Vec<Case> = serde_json::from_str(fixture).unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let mapped = session_update(case.notification.update);
        match (&case.expected, &case.skipped, &mapped) {
            (Some(expected), None, Update::Event(event)) => {
                assert_eq!(event, expected, "{}", case.name);
            }
            (None, Some(kind), Update::Skipped(skipped)) => {
                assert_eq!(&format!("{skipped:?}"), kind, "{}", case.name);
            }
            (expected, skipped, mapped) => {
                panic!(
                    "{}: expected {expected:?} / skipped {skipped:?}, got {mapped:?}",
                    case.name
                );
            }
        }
    }
}

#[test]
fn permission_request_fixture_maps_options_and_announces_the_call() {
    let request: acp::RequestPermissionRequest = serde_json::from_str(PERMISSION_REQUEST).unwrap();
    let options = permission_options(request.options);
    let kinds: Vec<_> = options.iter().map(|option| option.kind).collect();
    assert_eq!(
        kinds,
        [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::RejectOnce
        ]
    );
    assert_eq!(options[2].id, PermissionOptionId::new("reject"));

    let Some(AgentEvent::ToolCall(call)) = permission_tool_call(request.tool_call.clone(), false)
    else {
        panic!("an unannounced call must be announced");
    };
    assert_eq!(call.id, ToolCallId::new("toolu_05"));
    assert_eq!(call.title, "Run cargo test");
    assert_eq!(call.kind, ToolKind::Execute);
    assert_eq!(
        call.raw_input.unwrap().as_text(),
        r#"{"command":"cargo test"}"#
    );

    let Some(AgentEvent::ToolCallUpdate(update)) = permission_tool_call(request.tool_call, true)
    else {
        panic!("a known call with new fields gets an update");
    };
    assert_eq!(update.title.as_deref(), Some("Run cargo test"));
    let bare = acp::ToolCallUpdate::new("toolu_05", acp::ToolCallUpdateFields::new());
    assert_eq!(permission_tool_call(bare, true), None);
}

#[test]
fn unmodelled_content_is_forwarded_and_survives_the_round_trip() {
    let image = acp::ContentBlock::Image(acp::ImageContent::new("aGVsbG8=", "image/png"));
    let ContentBlock::Opaque { raw } = content(image.clone()) else {
        panic!("images are forwarded opaquely");
    };
    assert!(
        raw.as_text().contains(r#""type":"image""#),
        "{}",
        raw.as_text()
    );
    let back = prompt_block(ContentBlock::Opaque { raw }).unwrap();
    assert_eq!(back, image);
}

#[test]
fn prompts_map_to_acp_content() {
    assert_eq!(
        prompt_block(ContentBlock::Text {
            text: "hi".to_owned()
        })
        .unwrap(),
        acp::ContentBlock::Text(acp::TextContent::new("hi"))
    );
    assert_eq!(
        prompt_block(ContentBlock::ResourceLink {
            uri: "file:///a".to_owned(),
            name: "a".to_owned()
        })
        .unwrap(),
        acp::ContentBlock::ResourceLink(acp::ResourceLink::new("a", "file:///a"))
    );
    let not_acp = RawJson::from_text(r#"{"type":"hologram"}"#).unwrap();
    assert!(prompt_block(ContentBlock::Opaque { raw: not_acp }).is_err());
}

#[test]
fn answers_and_stop_reasons_map_both_ways() {
    assert_eq!(
        permission_outcome(&PermissionOutcome::Selected {
            option: PermissionOptionId::new("allow")
        }),
        acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new("allow"))
    );
    assert_eq!(
        permission_outcome(&PermissionOutcome::Cancelled),
        acp::RequestPermissionOutcome::Cancelled
    );
    assert_eq!(
        stop_reason(acp::StopReason::Cancelled),
        StopReason::Cancelled
    );
    assert_eq!(
        stop_reason(acp::StopReason::MaxTokens),
        StopReason::MaxTokens
    );
    assert_eq!(stop_reason(acp::StopReason::Refusal), StopReason::Refusal);
}
