use super::*;
use crate::fixtures::{ANTHROPIC_REQUEST, RESPONSES_REQUEST};
use crate::ir::Replay;

fn sealed(source: &str, replay: &Replay) -> String {
    envelope::seal(source, replay)
}

#[test]
fn a_request_with_nothing_to_remove_goes_as_it_came() {
    let body = Bytes::from_static(ANTHROPIC_REQUEST.as_bytes());
    let passed = anthropic(body.clone()).unwrap();
    assert_eq!(passed.model, "claude-sonnet-4-5");
    assert!(passed.stream);
    assert_eq!(passed.removed, 0);
    assert!(!passed.thinking_removed);
    // The very same bytes: whitespace, unknown members and all.
    assert_eq!(passed.body.as_ptr(), body.as_ptr());

    let body = Bytes::from_static(RESPONSES_REQUEST.as_bytes());
    let passed = responses(body.clone()).unwrap();
    assert_eq!(passed.model, "gpt-5-codex");
    assert_eq!(passed.body.as_ptr(), body.as_ptr());
}

/// ADR 0015, decision 8, the move case: a Claude Code session ran on a
/// Chat Completions route (its reasoning came back as thinking blocks
/// sealed in envelopes), then moved to an Anthropic route mid tool loop.
#[test]
fn after_a_move_no_envelope_reaches_anthropic() {
    let chat = sealed("openrouter", &Replay::Text);
    let body = format!(
        r#"{{"model":"claude-sonnet-4-5","max_tokens":32000,"stream":true,
            "thinking":{{"type":"enabled","budget_tokens":8000}},
            "messages":[
              {{"role":"user","content":"Write two files."}},
              {{"role":"assistant","content":[
                {{"type":"thinking","thinking":"Two files.","signature":"{chat}"}},
                {{"type":"text","text":"Writing."}},
                {{"type":"tool_use","id":"call_1","name":"Write","input":{{"file_path":"a"}}}}]}},
              {{"role":"user","content":[{{"type":"tool_result","tool_use_id":"call_1","content":"ok"}}]}}]}}"#
    );
    let passed = anthropic(Bytes::from(body)).unwrap();
    let sent = std::str::from_utf8(&passed.body).unwrap();
    assert!(!sent.contains(envelope::PREFIX), "{sent}");
    assert_eq!(passed.removed, 1);
    // The last turn now calls a tool without thinking first, which
    // Anthropic refuses while thinking: this request goes without.
    assert!(passed.thinking_removed);
    assert!(!sent.contains(r#""thinking":{"#), "{sent}");
    // Everything else is still there.
    let request: crate::wire::anthropic::Request = serde_json::from_str(sent).unwrap();
    assert_eq!(request.messages.len(), 3);
    assert_eq!(request.max_tokens, Some(32_000));
}

/// ADR 0015, decision 8, the mixed history: Anthropic's own thinking,
/// redacted thinking, and envelopes from other providers in one
/// conversation, with a tool loop that started on Anthropic.
#[test]
fn in_a_mixed_history_only_anthropic_s_thinking_stays() {
    let chat = sealed("openrouter", &Replay::Text);
    let openai = sealed("openai", &Replay::ResponsesEncrypted("gAAAA".into()));
    let own = sealed(
        "anthropic",
        &Replay::AnthropicSignature("EqQBsealed".into()),
    );
    let body = format!(
        r#"{{"model":"claude-sonnet-4-5","max_tokens":32000,
            "thinking":{{"type":"enabled","budget_tokens":8000}},
            "context_management":{{"edits":[]}},
            "messages":[
              {{"role":"user","content":"one"}},
              {{"role":"assistant","content":[
                {{"type":"thinking","thinking":"a","signature":"{chat}"}},
                {{"type":"text","text":"from a Chat route"}}]}},
              {{"role":"user","content":"two"}},
              {{"role":"assistant","content":[
                {{"type":"thinking","thinking":"b","signature":"{openai}"}},
                {{"type":"thinking","thinking":"c","signature":"{own}"}},
                {{"type":"thinking","thinking":"d","signature":""}},
                {{"type":"text","text":"from elsewhere"}}]}},
              {{"role":"user","content":"three"}},
              {{"role":"assistant","content":[
                {{"type":"thinking","thinking":"e","signature":"EqQBnative"}},
                {{"type":"redacted_thinking","data":"EmwKnative"}},
                {{"type":"tool_use","id":"toolu_1","name":"Read","input":{{}}}}]}},
              {{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"x"}}]}}]}}"#
    );
    let passed = anthropic(Bytes::from(body)).unwrap();
    let sent = std::str::from_utf8(&passed.body).unwrap();
    assert!(!sent.contains(envelope::PREFIX), "{sent}");
    assert_eq!(passed.removed, 4);
    for kept in [
        "EqQBnative",
        "EmwKnative",
        "context_management",
        "from a Chat route",
    ] {
        assert!(sent.contains(kept), "{kept} missing: {sent}");
    }
    // The tool loop started with Anthropic's own thinking: it keeps it.
    assert!(!passed.thinking_removed);
    assert!(sent.contains(r#""thinking":{"type":"enabled""#), "{sent}");
}

#[test]
fn codex_s_history_loses_only_envelopes_before_openai() {
    let anthropic_reasoning = sealed("anthropic", &Replay::AnthropicSignature("EqQB".into()));
    let body = format!(
        r#"{{"model":"gpt-5-codex","stream":true,"input":[
            {{"role":"user","content":"go"}},
            {{"type":"reasoning","summary":[],"encrypted_content":"{anthropic_reasoning}"}},
            {{"type":"reasoning","summary":[],"encrypted_content":"gAAAAnative"}},
            {{"type":"ghost_snapshot","ghost_commit":{{"id":"0"}}}},
            {{"type":"function_call","call_id":"c","name":"shell","arguments":"{{}}"}}]}}"#
    );
    let passed = responses(Bytes::from(body)).unwrap();
    let sent = std::str::from_utf8(&passed.body).unwrap();
    assert_eq!(passed.removed, 1);
    assert!(!sent.contains(envelope::PREFIX), "{sent}");
    for kept in [
        "gAAAAnative",
        "ghost_snapshot",
        "function_call",
        r#""role":"user""#,
    ] {
        assert!(sent.contains(kept), "{kept} missing: {sent}");
    }
}

#[test]
fn malformed_requests_are_refused() {
    for body in ["[]", "not json", r#"{"messages":[]}"#, r#"{"model":7}"#] {
        assert!(anthropic(Bytes::from(body)).is_err(), "{body}");
        assert!(responses(Bytes::from(body)).is_err(), "{body}");
    }
    assert!(anthropic(Bytes::from(r#"{"model":"m","stream":"yes"}"#)).is_err());
}

#[test]
fn a_raw_object_keeps_its_members_in_order() {
    let mut object: RawObject = serde_json::from_str(r#"{"b":1,"a":{"x":[1,2]},"c":"z"}"#).unwrap();
    assert_eq!(object.get::<u32>("b").unwrap().unwrap(), 1);
    assert!(object.get::<u32>("missing").is_none());
    assert!(object.get::<u32>("c").unwrap().is_err());
    object.set("b", RawJson::from_text("2").unwrap());
    object.set("d", RawJson::from_text("null").unwrap());
    assert!(object.remove("c"));
    assert!(!object.remove("c"));
    assert_eq!(
        serde_json::to_string(&object).unwrap(),
        r#"{"b":2,"a":{"x":[1,2]},"d":null}"#
    );
}

/// The security review's finding 1: a member named twice is read
/// differently by different readers, so a request naming `model` (or
/// anything else the proxy decides by) twice is refused, escaped spellings
/// included, on both APIs.
#[test]
fn a_member_named_twice_is_refused() {
    let anthropic_bodies = [
        r#"{"model":"allowed","model":"forbidden","messages":[]}"#,
        r#"{"model":"allowed","model":"forbidden","messages":[]}"#,
        r#"{"model":"allowed","stream":false,"stream":true,"messages":[]}"#,
        r#"{"model":"allowed","messages":[{"role":"user","role":"assistant","content":"x"}]}"#,
        r#"{"model":"allowed","messages":[{"role":"assistant","content":[
            {"type":"thinking","thinking":"t","signature":"EqQB","signature":"vsl1:x:text:"}]}]}"#,
        r#"{"model":"allowed","messages":[{"role":"assistant","content":[
            {"type":"text","type":"thinking","thinking":"t","signature":"vsl1:x:text:"}]}]}"#,
    ];
    for body in anthropic_bodies {
        let refused = anthropic(Bytes::from(body.to_owned()));
        assert!(refused.is_err(), "{body}");
    }
    let responses_bodies = [
        r#"{"model":"allowed","model":"forbidden","input":"hi"}"#,
        r#"{"model":"allowed","input":[{"type":"reasoning","encrypted_content":"gAAA",
            "encrypted_content":"vsl1:x:text:"}]}"#,
    ];
    for body in responses_bodies {
        let refused = responses(Bytes::from(body.to_owned()));
        assert!(refused.is_err(), "{body}");
    }
    // What the proxy does not decide by stays the harness's: an object
    // inside a tool's input may repeat a name.
    let opaque = r#"{"model":"allowed","messages":[{"role":"assistant","content":[
        {"type":"tool_use","id":"t1","name":"Write","input":{"a":1,"a":2}}]}]}"#;
    assert_eq!(anthropic(Bytes::from(opaque)).unwrap().model, "allowed");
}

/// A block or item that cannot be read is refused rather than forwarded
/// unread (it used to pass, envelope and all).
#[test]
fn an_unreadable_block_or_item_refuses_the_request() {
    for body in [
        r#"{"model":"m","messages":[{"role":"assistant","content":["just a string"]}]}"#,
        r#"{"model":"m","messages":[{"role":"assistant","content":[{"thinking":"no type"}]}]}"#,
        r#"{"model":"m","messages":[{"role":"assistant","content":[
            {"type":"thinking","thinking":"t","signature":42}]}]}"#,
    ] {
        assert!(anthropic(Bytes::from(body)).is_err(), "{body}");
    }
    for body in [
        r#"{"model":"m","input":[{"type":"reasoning","encrypted_content":7}]}"#,
        r#"{"model":"m","input":["not an item"]}"#,
    ] {
        assert!(responses(Bytes::from(body)).is_err(), "{body}");
    }
}

/// `vsl1:` anywhere in a signature, a redacted block's data or encrypted
/// content marks it as not the provider's own.
#[test]
fn an_envelope_anywhere_in_an_opaque_field_is_stripped() {
    let body = r#"{"model":"m","messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"t","signature":"EqQBvsl1:x:text:"},
        {"type":"redacted_thinking","data":"vsl1:other:anthropic-redacted:EmwK"},
        {"type":"thinking","thinking":"kept","signature":"EqQBnative"},
        {"type":"text","text":"vsl1: in text is just text"}]}]}"#;
    let passed = anthropic(Bytes::from(body)).unwrap();
    assert_eq!(passed.removed, 2);
    let sent = std::str::from_utf8(&passed.body).unwrap();
    assert!(sent.contains("EqQBnative"), "{sent}");
    assert!(sent.contains("vsl1: in text is just text"), "{sent}");
    assert!(!sent.contains("vsl1:x"), "{sent}");
    assert!(!sent.contains("vsl1:other"), "{sent}");

    let body = r#"{"model":"m","input":[
        {"type":"reasoning","summary":[],"encrypted_content":"gAAAvsl1:x:text:"},
        {"type":"reasoning","summary":[],"encrypted_content":"gAAAnative"}]}"#;
    let passed = responses(Bytes::from(body)).unwrap();
    assert_eq!(passed.removed, 1);
    assert!(!std::str::from_utf8(&passed.body).unwrap().contains("vsl1:"));
}
