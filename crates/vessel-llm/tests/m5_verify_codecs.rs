//! Run: copy to crates/vessel-llm/tests/m5_verify_codecs.rs, then
//! `cargo test -p vessel-llm --test m5_verify_codecs`
#![cfg(test)]
use bytes::Bytes;
use vessel_llm::{front, passthrough, upstream, wire};

#[test]
fn verify_duplicates_rejected_at_decision_depths_but_tool_payloads_preserved() {
    for body in [
        r#"{"model":"m","mo\u0064el":"other","messages":[]}"#,
        r#"{"model":"m","messages":[{"role":"user","role":"assistant","content":[]}]}"#,
        r#"{"model":"m","messages":[{"role":"assistant","content":[],"content":[]}]}"#,
        r#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"thinking","type":"text","signature":"x"}]}]}"#,
        r#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"x","id":"y","name":"fn","input":{}}]}]}"#,
        r#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"x","d\u0061ta":"y"}]}]}"#,
    ] {
        assert!(passthrough::anthropic(Bytes::from(body)).is_err(), "{body}");
    }
    for body in [
        r#"{"model":"m","input":[{"type":"function_call","name":"a","name":"b","arguments":"{}"}]}"#,
        r#"{"model":"m","input":[{"type":"reasoning","encrypted_content":"x","encrypted_content":"y"}]}"#,
        r#"{"model":"m","input":[{"role":"user","role":"assistant","content":"a"}]}"#,
    ] {
        assert!(passthrough::responses(Bytes::from(body)).is_err(), "{body}");
    }
    for body in [
        r#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"fn","input":{"a":1,"a":2}}]}]}"#,
        r#"{"model":"m","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}]}"#,
    ] {
        assert_eq!(
            passthrough::anthropic(Bytes::from(body)).unwrap().body,
            body.as_bytes()
        );
    }
    let body = r#"{"model":"m","input":[{"type":"function_call","name":"a","arguments":"{\"a\":1,\"a\":2}"}]}"#;
    assert_eq!(
        passthrough::responses(Bytes::from(body)).unwrap().body,
        body.as_bytes()
    );
}
#[test]
fn verify_nested_redacted_envelopes_and_valid_native_payloads() {
    for payload in [
        "vsl1:local:anthropic-redacted:vsl1:other:text:",
        "vsl1:local:anthropic-redacted:prefix-vsl1:other:text:",
        "vsl1:local:anthropic:vsl1:other:anthropic:x",
    ] {
        let request: wire::responses::Request=serde_json::from_str(&format!(r#"{{"model":"m","reasoning":{{"effort":"medium"}},"input":[{{"type":"reasoning","summary":[],"encrypted_content":"{payload}"}}]}}"#)).unwrap();
        let (neutral, _) = front::responses::request(request).unwrap();
        let up = upstream::anthropic::request(
            &neutral,
            &upstream::anthropic::Options {
                provider: "local".into(),
                default_max_tokens: 32000,
                adaptive_thinking: false,
            },
        )
        .unwrap();
        assert!(!serde_json::to_string(&up).unwrap().contains("vsl1:"));
    }
    for payload in [
        "vsl1:local:anthropic-redacted:vsl1:other:text:",
        "prefix-vsl1:other:text:",
    ] {
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"assistant","content":[{{"type":"redacted_thinking","data":"{payload}"}}]}}]}}"#
        );
        let passed = passthrough::anthropic(Bytes::from(body)).unwrap();
        assert_eq!(passed.removed, 1);
        assert!(!std::str::from_utf8(&passed.body).unwrap().contains("vsl1:"));
    }
    let body = r#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"EmwKbase64native"}]}]}"#;
    assert_eq!(
        passthrough::anthropic(Bytes::from(body)).unwrap().body,
        body.as_bytes()
    );
}
#[tokio::test]
async fn verify_untyped_http_error_with_escaped_key_is_redacted() {
    use vessel_llm::client::{Api, ApiKey, Client, Endpoint, Limits};
    use vessel_testkit::upstream::{FakeUpstream, Reply};
    const KEY: &str = "sk-FAKE-verify-escaped-key";
    let fake = FakeUpstream::start().await.unwrap();
    let encoded = KEY.bytes().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _written = write!(out, "\\u{byte:04x}");
        out
    });
    fake.reply(Reply::Status {
        status: 401,
        body: Bytes::from(format!(r#"{{"detail":"{encoded}"}}"#)),
        retry_after: None,
    });
    let client = Client::with_roots(rustls::RootCertStore::empty(), Limits::default()).unwrap();
    let endpoint = Endpoint {
        api: Api::OpenAiResponses,
        base_url: fake.url().parse().unwrap(),
        key: Some(ApiKey::new(KEY.into())),
    };
    let failure = client.send(&endpoint, Bytes::new(), &[]).await.unwrap_err();
    let raw = failure
        .message
        .strip_prefix("the provider answered 401: ")
        .unwrap();
    let decoded: serde_json::Value = serde_json::from_str(raw).unwrap();
    assert!(
        !decoded.to_string().contains(KEY),
        "escaped key survives nonstandard HTTP error body: {raw}"
    );
}
#[test]
fn verify_bom_prefix_is_chunk_invariant() {
    use vessel_llm::sse::{SseDecoder, SseFrame};
    let bytes = "\u{feff}event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".as_bytes();
    for split in [1, 2, 3, 4] {
        let mut decoder = SseDecoder::default();
        let mut frames = Vec::new();
        decoder.feed(&bytes[..split], &mut frames).unwrap();
        decoder.feed(&bytes[split..], &mut frames).unwrap();
        decoder.finish(&mut frames).unwrap();
        let names: Vec<_> = frames
            .into_iter()
            .filter_map(|frame| match frame {
                SseFrame::Event(event) => event.event,
                SseFrame::Comment => None,
            })
            .collect();
        assert_eq!(
            names,
            ["message_stop"],
            "BOM split at byte {split} loses event field"
        );
    }
}
