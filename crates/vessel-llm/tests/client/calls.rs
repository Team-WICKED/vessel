use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{HeaderName, HeaderValue};
use vessel_llm::client::{Answer, Api, ApiKey, Client, Endpoint, Limits};
use vessel_llm::ir::FailureClass;
use vessel_testkit::upstream::{FakeUpstream, Reply};

const CHAT_STREAM: &str = include_str!("../fixtures/chat-parallel-tool-calls.sse");

fn quick() -> Limits {
    Limits {
        connect: Duration::from_secs(5),
        first_byte: Duration::from_secs(5),
        idle: Duration::from_secs(5),
        max_body: 1024 * 1024,
        retries: 2,
        max_retry_wait: Duration::from_secs(1),
    }
}

fn client(limits: Limits) -> Client {
    Client::with_roots(rustls::RootCertStore::empty(), limits).unwrap()
}

fn endpoint(api: Api, base: &str) -> Endpoint {
    Endpoint {
        api,
        base_url: reqwest::Url::parse(base).unwrap(),
        key: Some(ApiKey::new("sk-test-key".into())),
    }
}

async fn read_all(answer: &mut Answer) -> Result<Vec<u8>, vessel_llm::ir::Failure> {
    let mut body = Vec::new();
    while let Some(chunk) = answer.chunk().await? {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[tokio::test]
async fn a_stream_arrives_whole_with_the_key_only_in_its_header() {
    let fake = FakeUpstream::start().await.unwrap();
    fake.reply(Reply::Stream {
        body: Bytes::from_static(CHAT_STREAM.as_bytes()),
        piece: 7,
    });
    let chat = endpoint(Api::OpenAiChat, &format!("{}/v1", fake.url()));
    let mut answer = client(quick())
        .send(&chat, Bytes::from_static(b"{\"model\":\"m\"}"), &[])
        .await
        .unwrap();
    assert_eq!(read_all(&mut answer).await.unwrap(), CHAT_STREAM.as_bytes());

    let seen = fake.seen();
    let [request] = seen.as_slice() else {
        panic!("{seen:?}");
    };
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.header("authorization"), Some("Bearer sk-test-key"));
    assert_eq!(request.header("anthropic-version"), None);
    assert_eq!(request.body, Bytes::from_static(b"{\"model\":\"m\"}"));
}

#[tokio::test]
async fn anthropic_gets_its_headers() {
    let fake = FakeUpstream::start().await.unwrap();
    fake.reply(Reply::Stream {
        body: Bytes::from_static(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        piece: 64,
    });
    fake.reply(Reply::Stream {
        body: Bytes::new(),
        piece: 1,
    });
    let anthropic = endpoint(Api::AnthropicMessages, &fake.url());
    let client = client(quick());
    let mut answer = client.send(&anthropic, Bytes::new(), &[]).await.unwrap();
    let _stop = read_all(&mut answer).await.unwrap();
    let forwarded = [
        (
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        ),
        (
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static("interleaved-thinking-2025-05-14"),
        ),
    ];
    let mut answer = client
        .send(&anthropic, Bytes::new(), &forwarded)
        .await
        .unwrap();
    let _empty = read_all(&mut answer).await.unwrap();

    let seen = fake.seen();
    let [first, second] = seen.as_slice() else {
        panic!("{seen:?}");
    };
    assert_eq!(first.path, "/v1/messages");
    assert_eq!(first.header("x-api-key"), Some("sk-test-key"));
    assert_eq!(first.header("authorization"), None);
    assert_eq!(first.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        second.header("anthropic-beta"),
        Some("interleaved-thinking-2025-05-14")
    );
    assert_eq!(
        second
            .headers
            .iter()
            .filter(|(name, _)| name == "anthropic-version")
            .count(),
        1
    );
}

#[tokio::test]
async fn transient_failures_are_retried_before_the_headers() {
    let fake = FakeUpstream::start().await.unwrap();
    fake.reply(Reply::Status {
        status: 529,
        body: Bytes::from_static(
            br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ),
        retry_after: Some(0),
    });
    fake.reply(Reply::Status {
        status: 503,
        body: Bytes::new(),
        retry_after: Some(0),
    });
    fake.reply(Reply::Stream {
        body: Bytes::from_static(b"data: [DONE]\n\n"),
        piece: 4,
    });
    let chat = endpoint(Api::OpenAiChat, &fake.url());
    let mut answer = client(quick())
        .send(&chat, Bytes::new(), &[])
        .await
        .unwrap();
    assert_eq!(read_all(&mut answer).await.unwrap(), b"data: [DONE]\n\n");
    assert_eq!(fake.seen().len(), 3);
}

#[tokio::test]
async fn a_long_wait_or_a_refusal_goes_to_the_harness_at_once() {
    let fake = FakeUpstream::start().await.unwrap();
    fake.reply(Reply::Status {
        status: 429,
        body: Bytes::from_static(br#"{"error":{"message":"Slow down.","code":429}}"#),
        retry_after: Some(60),
    });
    let chat = endpoint(Api::OpenAiChat, &fake.url());
    let client = client(quick());
    let failure = client.send(&chat, Bytes::new(), &[]).await.unwrap_err();
    assert_eq!(failure.class, FailureClass::RateLimited);
    assert_eq!(failure.retry_after, Some(Duration::from_secs(60)));
    assert!(failure.message.contains("Slow down."), "{failure:?}");
    assert_eq!(fake.seen().len(), 1);

    fake.reply(Reply::Status {
        status: 400,
        body: Bytes::from_static(
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
        ),
        retry_after: None,
    });
    let anthropic = endpoint(Api::AnthropicMessages, &fake.url());
    let failure = client
        .send(&anthropic, Bytes::new(), &[])
        .await
        .unwrap_err();
    assert_eq!(failure.class, FailureClass::ContextTooLong);
    assert_eq!(fake.seen().len(), 2);

    // An echoed key never leaves the client.
    fake.reply(Reply::Status {
        status: 401,
        body: Bytes::from_static(
            br#"{"error":{"message":"Incorrect API key provided: sk-test-key."}}"#,
        ),
        retry_after: None,
    });
    let failure = client.send(&chat, Bytes::new(), &[]).await.unwrap_err();
    assert_eq!(failure.class, FailureClass::Unauthorized);
    assert!(!failure.message.contains("sk-test-key"), "{failure:?}");
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let fake = FakeUpstream::start().await.unwrap();
    fake.reply(Reply::Redirect {
        location: "/elsewhere".into(),
    });
    fake.reply(Reply::Stream {
        body: Bytes::from_static(b"data: [DONE]\n\n"),
        piece: 4,
    });
    let chat = endpoint(Api::OpenAiChat, &fake.url());
    let failure = client(quick())
        .send(&chat, Bytes::new(), &[])
        .await
        .unwrap_err();
    assert_eq!(failure.class, FailureClass::RefusedRequest);
    assert_eq!(fake.seen().len(), 1);
}

#[tokio::test]
async fn drops_silences_and_hangs_end_the_call() {
    let fake = FakeUpstream::start().await.unwrap();
    let chat = endpoint(Api::OpenAiChat, &fake.url());
    let limits = Limits {
        first_byte: Duration::from_millis(300),
        idle: Duration::from_millis(300),
        retries: 0,
        ..quick()
    };
    let client = client(limits);

    fake.reply(Reply::Drop {
        body: Bytes::from_static(CHAT_STREAM.as_bytes()),
        cut: 50,
    });
    let mut answer = client.send(&chat, Bytes::new(), &[]).await.unwrap();
    let failure = read_all(&mut answer).await.unwrap_err();
    assert_eq!(failure.class, FailureClass::UpstreamError);

    fake.reply(Reply::Stall {
        body: Bytes::from_static(b": keepalive\n\n"),
        silence: Duration::from_secs(5),
    });
    let mut answer = client.send(&chat, Bytes::new(), &[]).await.unwrap();
    let failure = read_all(&mut answer).await.unwrap_err();
    assert_eq!(failure.class, FailureClass::Timeout);

    fake.reply(Reply::Hang {
        wait: Duration::from_secs(5),
    });
    let failure = client.send(&chat, Bytes::new(), &[]).await.unwrap_err();
    assert_eq!(failure.class, FailureClass::Timeout);
}

#[tokio::test]
async fn a_whole_body_is_bounded() {
    let fake = FakeUpstream::start().await.unwrap();
    fake.reply(Reply::Stream {
        body: Bytes::from(vec![b'x'; 4096]),
        piece: 512,
    });
    let chat = endpoint(Api::OpenAiChat, &fake.url());
    let limits = Limits {
        max_body: 1000,
        ..quick()
    };
    let answer = client(limits).send(&chat, Bytes::new(), &[]).await.unwrap();
    let failure = answer.body().await.unwrap_err();
    assert_eq!(failure.class, FailureClass::UpstreamError);
}
