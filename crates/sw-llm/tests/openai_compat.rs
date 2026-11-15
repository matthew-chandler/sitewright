//! Fixture tests for the OpenAI-compatible adapter against a local mock server.

#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use sw_core::Secrets;
use sw_core::config::{AuthStyle, ProviderConfig, ProviderKind};
use sw_llm::{
    ChatRequest, LlmError, Message, StopReason, ToolCall, ToolDef, Usage, build_provider, collect,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn config(server: &MockServer, auth: AuthStyle) -> ProviderConfig {
    ProviderConfig {
        id: "test".into(),
        kind: ProviderKind::OpenaiCompat,
        base_url: Some(format!("{}/openai/v1/", server.uri())),
        model: "gpt-test".into(),
        auth,
        key_ref: Some("k".into()),
        timeout_secs: 5,
        max_retries: 2,
        max_output_tokens: Some(1234),
        extra_body: toml::from_str("reasoning_effort = \"low\"").unwrap(),
    }
}

fn secrets() -> Secrets {
    let mut s = Secrets::default();
    s.keys.insert("k".into(), "sk-test".into());
    s
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn text_chunk(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]})
}

fn finish(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn usage(input: u64, output: u64) -> Value {
    json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output}})
}

fn request() -> ChatRequest {
    ChatRequest {
        system: "You build sites.".into(),
        messages: vec![Message::User {
            content: "hello".into(),
        }],
        tools: vec![ToolDef {
            name: "write_file".into(),
            description: "Write a file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }],
    }
}

#[tokio::test]
async fn plain_text_with_bearer_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(sse(&[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]}),
            text_chunk("Hello"),
            text_chunk(", world"),
            finish("stop"),
            usage(11, 3),
        ]))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&config(&server, AuthStyle::Bearer), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got.text, "Hello, world");
    assert!(got.tool_calls.is_empty());
    assert_eq!(
        got.usage,
        Usage {
            input_tokens: 11,
            output_tokens: 3
        }
    );
    assert_eq!(got.stop, Some(StopReason::EndTurn));

    // The request body carries the normalized conversation in wire format.
    let sent: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(sent["model"], "gpt-test");
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
    assert_eq!(sent["max_completion_tokens"], 1234);
    assert_eq!(sent["reasoning_effort"], "low");
    assert_eq!(
        sent["messages"][0],
        json!({"role": "system", "content": "You build sites."})
    );
    assert_eq!(
        sent["messages"][1],
        json!({"role": "user", "content": "hello"})
    );
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "write_file");
}

#[tokio::test]
async fn tool_call_round_trip_with_fragmented_arguments_and_azure_auth() {
    let server = MockServer::start().await;
    let call = |index: u64, id: Option<&str>, name: Option<&str>, args: &str| {
        json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"tool_calls": [
            {"index": index, "id": id, "type": "function", "function": {"name": name, "arguments": args}}
        ]}}]})
    };
    Mock::given(method("POST"))
        .and(header("api-key", "sk-test"))
        .respond_with(sse(&[
            text_chunk("Writing files."),
            call(0, Some("call_a"), Some("write_file"), ""),
            call(0, None, None, "{\"path\":\"index"),
            call(1, Some("call_b"), Some("publish_site"), "{\"mess"),
            call(0, None, None, ".html\",\"content\":\"<h1>caf\u{e9}</h1>\"}"),
            call(1, None, None, "age\":\"first\"}"),
            finish("tool_calls"),
            usage(50, 20),
        ]))
        .mount(&server)
        .await;

    let provider = build_provider(&config(&server, AuthStyle::ApiKeyHeader), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got.text, "Writing files.");
    assert_eq!(got.stop, Some(StopReason::ToolUse));
    assert_eq!(
        got.tool_calls,
        vec![
            ToolCall {
                id: "call_a".into(),
                name: "write_file".into(),
                arguments: json!({"path": "index.html", "content": "<h1>caf\u{e9}</h1>"}),
            },
            ToolCall {
                id: "call_b".into(),
                name: "publish_site".into(),
                arguments: json!({"message": "first"})
            },
        ]
    );

    // Second call: the assistant turn and tool results go back in wire format.
    let mut next = request();
    next.messages.push(Message::Assistant {
        text: String::new(),
        tool_calls: got.tool_calls.clone(),
    });
    next.messages.push(Message::Tool {
        call_id: "call_a".into(),
        name: "write_file".into(),
        content: "{\"ok\":true}".into(),
        is_error: false,
    });
    collect(provider.stream(next).await.unwrap()).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let sent: Value = requests[1].body_json().unwrap();
    let assistant = &sent["messages"][2];
    assert_eq!(assistant["role"], "assistant");
    assert!(assistant["content"].is_null());
    assert_eq!(assistant["tool_calls"][0]["id"], "call_a");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "write_file");
    let args: Value = serde_json::from_str(
        assistant["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args["path"], "index.html");
    assert_eq!(
        sent["messages"][3],
        json!({"role": "tool", "tool_call_id": "call_a", "content": "{\"ok\":true}"})
    );
}

#[tokio::test]
async fn invalid_tool_arguments_are_reported_not_dropped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "write_file", "arguments": "{\"path\": "}}
            ]}}]}),
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"tool_calls": [
                {"index": 1, "id": "c2", "function": {"name": "list_files", "arguments": ""}}
            ]}}]}),
            // Some servers say "stop" even when they returned tool calls.
            finish("stop"),
        ]))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server, AuthStyle::Bearer), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(
        got.tool_calls[0].arguments,
        json!({"_invalid_json": "{\"path\":"})
    );
    assert_eq!(got.tool_calls[1].arguments, json!({}));
    assert_eq!(got.stop, Some(StopReason::ToolUse));
}

#[tokio::test]
async fn auth_and_bad_request_errors_are_not_retried() {
    for (status, body) in [
        (401, json!({"error": {"message": "Incorrect API key"}})),
        (400, json!({"error": {"message": "bad tool schema"}})),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let provider = build_provider(&config(&server, AuthStyle::Bearer), &secrets()).unwrap();
        let err = provider.stream(request()).await.err().unwrap();
        match (status, &err) {
            (401, LlmError::Auth { message, .. }) => assert_eq!(message, "Incorrect API key"),
            (400, LlmError::BadRequest { message, .. }) => assert_eq!(message, "bad tool schema"),
            _ => panic!("unexpected error for {status}: {err:?}"),
        }
        assert!(!err.to_string().contains("sk-test"));
    }
}

#[tokio::test]
async fn rate_limits_and_server_errors_are_retried() {
    // 429 with Retry-After, then 503, then success.
    let server = MockServer::start().await;
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = attempts.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            match counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => ResponseTemplate::new(429)
                    .insert_header("retry-after", "0.05")
                    .set_body_json(json!({"error": {"message": "slow down"}})),
                1 => ResponseTemplate::new(503).set_body_string("upstream unavailable"),
                _ => sse(&[text_chunk("ok"), finish("stop")]),
            }
        })
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server, AuthStyle::Bearer), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got.text, "ok");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);

    // Retries run out: the last error is surfaced.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_json(json!({"error": {"message": "quota exceeded"}})),
        )
        .expect(3)
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server, AuthStyle::Bearer), &secrets()).unwrap();
    let err = provider.stream(request()).await.err().unwrap();
    assert!(
        matches!(&err, LlmError::RateLimited { message, .. } if message == "quota exceeded"),
        "{err:?}"
    );
}

#[tokio::test]
async fn truncated_and_malformed_streams_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {}\n\n", text_chunk("partial")),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server, AuthStyle::Bearer), &secrets()).unwrap();
    let err = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Network(_)), "{err:?}");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("data: {not json}\n\n", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server, AuthStyle::Bearer), &secrets()).unwrap();
    let err = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn missing_key_and_unreachable_host_are_clear_errors() {
    let server = MockServer::start().await;
    let err = build_provider(&config(&server, AuthStyle::Bearer), &Secrets::default()).unwrap_err();
    assert!(err.to_string().contains("not in secrets.toml"), "{err}");

    let mut cfg = config(&server, AuthStyle::Bearer);
    cfg.base_url = Some("http://127.0.0.1:9/v1".into());
    cfg.max_retries = 0;
    let provider = build_provider(&cfg, &secrets()).unwrap();
    let err = provider.stream(request()).await.err().unwrap();
    assert!(matches!(err, LlmError::Network(_)), "{err:?}");
}
