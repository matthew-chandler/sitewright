//! Fixture tests for the Gemini adapter against a local mock server.

#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use sw_core::Secrets;
use sw_core::config::{AuthStyle, ProviderConfig, ProviderKind};
use sw_llm::{ChatRequest, LlmError, Message, StopReason, ToolDef, Usage, build_provider, collect};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(server: &MockServer) -> ProviderConfig {
    ProviderConfig {
        id: "gemini".into(),
        kind: ProviderKind::Gemini,
        base_url: Some(server.uri()),
        model: "gemini-test".into(),
        auth: AuthStyle::Bearer,
        key_ref: Some("k".into()),
        timeout_secs: 5,
        max_retries: 1,
        max_output_tokens: Some(9000),
        extra_body: toml::Table::new(),
    }
}

fn secrets() -> Secrets {
    let mut s = Secrets::default();
    s.keys.insert("k".into(), "g-test-key".into());
    s
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let body: String = chunks
        .iter()
        .map(|c| format!("data: {c}\r\n\r\n"))
        .collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn request() -> ChatRequest {
    ChatRequest {
        system: "You build sites.".into(),
        messages: vec![Message::User {
            content: "hello".into(),
        }],
        tools: vec![
            ToolDef {
                name: "write_file".into(),
                description: "Write a file".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false}),
            },
            ToolDef {
                name: "list_files".into(),
                description: "List files".into(),
                parameters: json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            },
        ],
    }
}

fn text(t: &str) -> Value {
    json!({"candidates": [{"content": {"role": "model", "parts": [{"text": t}]}}]})
}

#[tokio::test]
async fn plain_text_with_the_key_in_a_header_not_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-test:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("x-goog-api-key", "g-test-key"))
        .respond_with(sse(&[
            text("Hello"),
            text(", world"),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}, "finishReason": "STOP"}],
                   "usageMetadata": {"promptTokenCount": 30, "candidatesTokenCount": 5, "thoughtsTokenCount": 12}}),
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
            input_tokens: 30,
            output_tokens: 17
        },
        "thinking tokens are output"
    );
    assert_eq!(got.raw, Some(json!([{"text": "Hello, world"}])));

    let received = &server.received_requests().await.unwrap()[0];
    assert!(!received.url.as_str().contains("g-test-key"));
    let sent: Value = received.body_json().unwrap();
    assert_eq!(
        sent["systemInstruction"],
        json!({"parts": [{"text": "You build sites."}]})
    );
    assert_eq!(
        sent["contents"],
        json!([{"role": "user", "parts": [{"text": "hello"}]}])
    );
    assert_eq!(sent["generationConfig"]["maxOutputTokens"], 9000);
    let declarations = &sent["tools"][0]["functionDeclarations"];
    assert_eq!(
        declarations[0]["parameters"],
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
    );
    assert!(
        declarations[1].get("parameters").is_none(),
        "a tool without arguments has no parameters schema"
    );
}

#[tokio::test]
async fn function_call_round_trip_keeps_thought_signatures() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Planning.", "thought": true}]}}]}),
            text("Writing."),
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"name": "write_file", "args": {"path": "index.html", "content": "<h1>hi</h1>"}}, "thoughtSignature": "sig-1"},
                {"functionCall": {"name": "list_files", "args": {}}},
            ]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 40, "candidatesTokenCount": 20}}),
        ]))
        .mount(&server)
        .await;

    let provider = build_provider(&config(&server), &secrets()).unwrap();
    let got = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got.text, "Writing.", "thought text is not shown");
    assert_eq!(
        got.stop,
        Some(StopReason::ToolUse),
        "STOP with function calls means the model wants results"
    );
    assert_eq!(got.tool_calls.len(), 2);
    assert_eq!(
        (
            got.tool_calls[0].id.as_str(),
            got.tool_calls[0].name.as_str()
        ),
        ("call_1", "write_file")
    );
    assert_eq!(got.tool_calls[0].arguments["path"], "index.html");
    let raw = got.raw.clone().unwrap();
    assert_eq!(raw.as_array().unwrap().len(), 4);
    assert_eq!(raw[2]["thoughtSignature"], "sig-1");

    let mut next = request();
    next.messages.push(Message::Assistant {
        text: got.text.clone(),
        tool_calls: got.tool_calls.clone(),
        raw: got.raw.clone(),
    });
    next.messages.push(Message::Tool {
        call_id: "call_1".into(),
        name: "write_file".into(),
        content: "{\"written\":\"index.html\"}".into(),
        is_error: false,
    });
    next.messages.push(Message::Tool {
        call_id: "call_2".into(),
        name: "list_files".into(),
        content: "plain text failure".into(),
        is_error: true,
    });
    let _ = collect(provider.stream(next).await.unwrap()).await;
    let sent: Value = server.received_requests().await.unwrap()[1]
        .body_json()
        .unwrap();
    assert_eq!(sent["contents"][1], json!({"role": "model", "parts": raw}));
    assert_eq!(
        sent["contents"][2],
        json!({"role": "user", "parts": [
            {"functionResponse": {"name": "write_file", "response": {"result": {"written": "index.html"}}}},
            {"functionResponse": {"name": "list_files", "response": {"error": "plain text failure"}}},
        ]})
    );
}

#[tokio::test]
async fn finish_reasons_and_errors_are_classified() {
    for (reason, expected) in [
        ("MAX_TOKENS", StopReason::MaxTokens),
        ("SAFETY", StopReason::Other("safety".into())),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sse(&[json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": reason}]})]))
            .mount(&server)
            .await;
        let provider = build_provider(&config(&server), &secrets()).unwrap();
        assert_eq!(
            collect(provider.stream(request()).await.unwrap())
                .await
                .unwrap()
                .stop,
            Some(expected)
        );
    }

    for (status, check) in [(403u16, "auth"), (400, "bad"), (429, "rate")] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("retry-after", "0")
                    .set_body_json(
                        json!({"error": {"code": status, "message": "nope", "status": "X"}}),
                    ),
            )
            .mount(&server)
            .await;
        let provider = build_provider(&config(&server), &secrets()).unwrap();
        let err = provider.stream(request()).await.err().unwrap();
        let ok = match check {
            "auth" => matches!(err, LlmError::Auth { .. }),
            "bad" => matches!(err, LlmError::BadRequest { .. }),
            _ => matches!(err, LlmError::RateLimited { .. }),
        };
        assert!(ok, "{status}: {err:?}");
        assert!(err.to_string().contains("nope") && !err.to_string().contains("g-test-key"));
    }

    // A stream that stops without a finish reason, and an error object mid-stream.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[text("partial")]))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server), &secrets()).unwrap();
    assert!(matches!(
        collect(provider.stream(request()).await.unwrap())
            .await
            .unwrap_err(),
        LlmError::Network(_)
    ));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[
            text("partial"),
            json!({"error": {"code": 503, "message": "overloaded"}}),
        ]))
        .mount(&server)
        .await;
    let provider = build_provider(&config(&server), &secrets()).unwrap();
    assert!(matches!(
        collect(provider.stream(request()).await.unwrap())
            .await
            .unwrap_err(),
        LlmError::Server { status: 503, .. }
    ));

    let mut bad = config(&server);
    bad.model = "../other".into();
    assert!(build_provider(&bad, &secrets()).is_err());
}
