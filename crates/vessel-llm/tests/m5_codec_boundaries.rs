//! Run: copy to crates/vessel-llm/tests/m5_codec_boundaries.rs, then
//! `cargo test -p vessel-llm --test m5_codec_boundaries -- --nocapture`

use bytes::Bytes;
use vessel_llm::{front, passthrough, upstream, wire};

#[test]
fn duplicate_signature_must_not_bypass_foreign_thinking_stripping() {
    let body = Bytes::from_static(br#"{"model":"allowed","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"foreign","signature":"native","signature":"vsl1:other:text:"}]}]}"#);
    if let Ok(passed) = passthrough::anthropic(body) {
        assert!(
            !std::str::from_utf8(&passed.body).unwrap().contains("vsl1:"),
            "ambiguous signature is forwarded intact to Anthropic: {}",
            std::str::from_utf8(&passed.body).unwrap()
        );
    }
}

#[test]
fn nested_envelope_must_never_reach_anthropic_signature() {
    let request: wire::responses::Request = serde_json::from_str(
        r#"{
        "model":"allowed", "reasoning":{"effort":"medium"},
        "input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"foreign"}],
                  "encrypted_content":"vsl1:local:anthropic:vsl1:other:text:"}]
    }"#,
    )
    .unwrap();
    let (neutral, _) = front::responses::request(request).unwrap();
    let up = upstream::anthropic::request(
        &neutral,
        &upstream::anthropic::Options {
            provider: "local".to_owned(),
            default_max_tokens: 32000,
            adaptive_thinking: false,
        },
    )
    .unwrap();
    let serialized = serde_json::to_string(&up).unwrap();
    assert!(
        !serialized.contains("vsl1:"),
        "Vessel envelope reaches Anthropic: {serialized}"
    );
}

#[tokio::test]
async fn untyped_error_redaction_must_precede_truncation() {
    use vessel_llm::client::{Api, ApiKey, Client, Endpoint, Limits};
    use vessel_testkit::upstream::{FakeUpstream, Reply};
    const KEY: &str = "sk-FAKE-REVIEW-super-secret-tail";
    let fake = FakeUpstream::start().await.unwrap();
    let prefix = KEY.strip_suffix('l').unwrap();
    fake.reply(Reply::Status {
        status: 401,
        body: Bytes::from(format!("{}{KEY}", "x".repeat(512 - prefix.len()))),
        retry_after: None,
    });
    let endpoint = Endpoint {
        api: Api::OpenAiChat,
        base_url: fake.url().parse().unwrap(),
        key: Some(ApiKey::new(KEY.to_owned())),
    };
    let client = Client::with_roots(rustls::RootCertStore::empty(), Limits::default()).unwrap();
    let failure = client.send(&endpoint, Bytes::new(), &[]).await.unwrap_err();
    assert!(
        !failure.message.contains(prefix),
        "all but the final key character survives truncation before redaction: {}",
        failure.message
    );
}
