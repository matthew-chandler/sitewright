//! Fixture tests for the Anthropic adapter against a local mock server.

#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use sw_core::Secrets;
use sw_core::config::{AuthStyle, ProviderConfig, ProviderKind};
use sw_llm::{ChatRequest, LlmError, Message, StopReason, ToolDef, Usage, build_provider, collect};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(server: &MockServer) -> ProviderConfig {
    ProviderConfig {
        id: "claude".into(),
        kind: ProviderKind::Anthropic,
        base_url: Some(server.uri()),
        model: "claude-test".into(),
        auth: AuthStyle::Bearer,
        key_ref: Some("k".into()),
        timeout_secs: 5,
        max_retries: 1,
        max_output_tokens: None,
        extra_body: toml::Table::new(),
    }
}

fn secrets() -> Secrets {
    let mut s = Secrets::default();
    s.keys.insert("k".into(), "sk-ant-test".into());
    s
}

fn sse(events: &[Value]) -> ResponseTemplate {
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
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
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        }],
    }
}

fn start(input: u64, cached: u64) -> Value {
    json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": input, "cache_read_input_tokens": cached, "output_tokens": 1}}})
}

fn stop(reason: &str, output: u64) -> [Value; 2] {
    [
        json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": output}}),
        json!({"type": "message_stop"}),
    ]
}

#[tokio::test]
async fn plain_text_with_the_required_headers_and_body() {
    let server = MockServer::start().await;
    let [delta, end] = stop("end_turn", 7);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2026-10-21"))
        .respond_with(sse(&[
            start(20, 100),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": ", world"}}),
            json!({"type": "content_block_stop", "index": 0}),
            delta,
            end,
        ]))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&config(&server), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got.text, "Hello, world");
    assert_eq!(got.stop, Some(StopReason::EndTurn));
    assert_eq!(
        got.usage,
        Usage {
            input_tokens: 120,
            output_tokens: 7
        },
        "cached input counts as input"
    );
    assert_eq!(
        got.raw,
        Some(json!([{"type": "text", "text": "Hello, world"}]))
    );

    let sent: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(sent["model"], "claude-test");
    assert_eq!(sent["max_tokens"], 64000);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["system"], "You build sites.");
    assert_eq!(sent["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(
        sent["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
    );
    assert_eq!(sent["tools"][0]["name"], "write_file");
    assert_eq!(
        sent["tools"][0]["input_schema"]["required"],
        json!(["path"])
    );
    assert_eq!(sent["tools"][0]["eager_input_streaming"], true);
    // Sampling parameters are rejected by the current API version: never send them.
    for forbidden in ["temperature", "top_p", "top_k", "thinking"] {
        assert!(sent.get(forbidden).is_none(), "{forbidden}");
    }
}

#[tokio::test]
async fn tool_use_round_trip_replays_thinking_blocks_unchanged() {
    let server = MockServer::start().await;
    let [delta, end] = stop("tool_use", 40);
    Mock::given(method("POST"))
        .respond_with(sse(&[
            start(50, 0),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Plan the page."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-abc"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Writing."}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "write_file", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"index"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": ".html\", \"content\": \"<h1>caf\u{e9}</h1>\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "list_files", "input": {}}}),
            json!({"type": "content_block_stop", "index": 3}),
            delta,
            end,
        ]))
        .mount(&server)
        .await;

    let provider = build_provider(&config(&server), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got.text, "Writing.");
    assert_eq!(got.stop, Some(StopReason::ToolUse));
    assert_eq!(got.tool_calls.len(), 2);
    assert_eq!(got.tool_calls[0].id, "toolu_1");
    assert_eq!(
        got.tool_calls[0].arguments,
        json!({"path": "index.html", "content": "<h1>caf\u{e9}</h1>"})
    );
    assert_eq!(got.tool_calls[1].arguments, json!({}));
    let raw = got.raw.clone().unwrap();
    assert_eq!(
        raw[0],
        json!({"type": "thinking", "thinking": "Plan the page.", "signature": "sig-abc"})
    );
    assert_eq!(raw[2]["input"]["path"], "index.html");

    // Next call: the assistant turn goes back block for block, and both tool results
    // share one user message.
    let mut next = request();
    next.messages.push(Message::Assistant {
        text: got.text.clone(),
        tool_calls: got.tool_calls.clone(),
        raw: got.raw.clone(),
    });
    next.messages.push(Message::Tool {
        call_id: "toolu_1".into(),
        name: "write_file".into(),
        content: "{\"ok\":true}".into(),
        is_error: false,
    });
    next.messages.push(Message::Tool {
        call_id: "toolu_2".into(),
        name: "list_files".into(),
        content: "{\"error\":\"nope\"}".into(),
        is_error: true,
    });
    let _ = collect(provider.stream(next).await.unwrap()).await;
    let sent: Value = server.received_requests().await.unwrap()[1]
        .body_json()
        .unwrap();
    assert_eq!(sent["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        sent["messages"][1],
        json!({"role": "assistant", "content": raw})
    );
    assert_eq!(
        sent["messages"][2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "{\"ok\":true}", "is_error": false},
            {"type": "tool_result", "tool_use_id": "toolu_2", "content": "{\"error\":\"nope\"}", "is_error": true},
        ]})
    );
}

#[tokio::test]
async fn an_assistant_turn_without_raw_blocks_is_rebuilt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[
            start(1, 0),
            stop("end_turn", 1)[0].clone(),
            stop("end_turn", 1)[1].clone(),
        ]))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server), &secrets()).unwrap();
    let mut req = request();
    req.messages.push(Message::Assistant {
        text: "Writing.".into(),
        tool_calls: vec![sw_llm::ToolCall {
            id: "t1".into(),
            name: "write_file".into(),
            arguments: json!({"path": "a.html"}),
        }],
        raw: None,
    });
    req.messages.push(Message::Tool {
        call_id: "t1".into(),
        name: "write_file".into(),
        content: "{}".into(),
        is_error: false,
    });
    collect(provider.stream(req).await.unwrap()).await.unwrap();
    let sent: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(
        sent["messages"][1]["content"],
        json!([{"type": "text", "text": "Writing."}, {"type": "tool_use", "id": "t1", "name": "write_file", "input": {"path": "a.html"}}])
    );
}

#[tokio::test]
async fn truncated_tool_input_is_reported_and_refusals_and_limits_are_distinguished() {
    let server = MockServer::start().await;
    let [delta, end] = stop("max_tokens", 64000);
    Mock::given(method("POST"))
        .respond_with(sse(&[
            start(5, 0),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "write_file", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"index.html\", \"content\": \"<h1>cut"}}),
            json!({"type": "content_block_stop", "index": 0}),
            delta,
            end,
        ]))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got.stop, Some(StopReason::MaxTokens));
    assert!(got.tool_calls[0].arguments.get("_invalid_json").is_some());
    assert_eq!(
        got.raw.unwrap()[0]["input"],
        json!({}),
        "the replayed block still has an object input"
    );

    let server = MockServer::start().await;
    let [delta, end] = stop("refusal", 0);
    Mock::given(method("POST"))
        .respond_with(sse(&[start(5, 0), delta, end]))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got.stop, Some(StopReason::Other("refusal".into())));
}

#[tokio::test]
async fn http_and_mid_stream_errors_are_classified() {
    let cases = [
        (
            401,
            json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}),
        ),
        (
            400,
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "max_tokens: too large"}}),
        ),
    ];
    for (status, body) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let provider = build_provider(&config(&server), &secrets()).unwrap();
        let err = provider.stream(request()).await.err().unwrap();
        match (status, &err) {
            (401, LlmError::Auth { message, .. }) => assert_eq!(message, "invalid x-api-key"),
            (400, LlmError::BadRequest { message, .. }) => {
                assert_eq!(message, "max_tokens: too large")
            }
            _ => panic!("{status}: {err:?}"),
        }
        assert!(!err.to_string().contains("sk-ant-test"));
    }

    // 529 overloaded is retried (max_retries = 1, so two attempts).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(529).set_body_json(json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})))
        .expect(2)
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server), &secrets()).unwrap();
    assert!(matches!(
        provider.stream(request()).await.err().unwrap(),
        LlmError::Server { status: 529, .. }
    ));

    // An error event in the middle of a stream, and a stream cut short.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[start(1, 0), json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})]))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server), &secrets()).unwrap();
    let err = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Server { .. }), "{err:?}");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[start(1, 0)]))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server), &secrets()).unwrap();
    let err = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Network(_)), "{err:?}");
}
