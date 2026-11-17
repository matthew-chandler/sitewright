//! The Copilot adapter against a scripted stand-in for GitHub's runtime, speaking the
//! runtime's JSON-RPC protocol over in-memory pipes. The real runtime is not run here.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{Value, json};
use sw_core::config::{AuthStyle, ProviderConfig, ProviderKind};
use sw_llm::copilot::{Connection, Connector, Copilot, connect_streams};
use sw_llm::{
    ChatRequest, LlmError, LlmProvider, Message, StopReason, ToolCall, ToolDef, Usage, collect,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

fn config() -> ProviderConfig {
    ProviderConfig {
        id: "copilot".into(),
        kind: ProviderKind::Copilot,
        base_url: None,
        model: "gpt-test".into(),
        auth: AuthStyle::Bearer,
        key_ref: Some("k".into()),
        timeout_secs: 5,
        max_retries: 0,
        max_output_tokens: None,
        extra_body: toml::Table::new(),
    }
}

async fn write_framed(writer: &mut DuplexStream, message: &Value) {
    let body = serde_json::to_vec(message).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await.unwrap();
    writer.write_all(&body).await.unwrap();
    writer.flush().await.unwrap();
}

async fn read_framed(reader: &mut DuplexStream) -> Option<Value> {
    let mut header = String::new();
    while !header.ends_with("\r\n\r\n") {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).await.ok()?;
        header.push(byte[0] as char);
    }
    let length: usize = header
        .trim()
        .strip_prefix("Content-Length: ")?
        .parse()
        .ok()?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// What the stand-in does when the prompt arrives.
#[derive(Clone, Copy)]
enum Script {
    /// Say something, call `write_file` twice, then finish after both results.
    TwoTools,
    /// Answer in one piece, without streaming.
    PlainAnswer,
    /// Report a session error.
    Fails,
}

#[derive(Default)]
struct Seen {
    requests: Vec<Value>,
    connections: usize,
}

struct Fake {
    script: Script,
    seen: Arc<Mutex<Seen>>,
}

async fn event(writer: &mut DuplexStream, session: &str, kind: &str, data: Value) {
    let message = json!({
        "jsonrpc": "2.0",
        "method": "session.event",
        "params": {
            "sessionId": session,
            "event": {
                "id": format!("evt-{}", rand::random::<u64>()),
                "timestamp": "2026-01-01T00:00:00Z",
                "type": kind,
                "data": data,
            },
        },
    });
    write_framed(writer, &message).await;
}

async fn serve(
    script: Script,
    seen: Arc<Mutex<Seen>>,
    mut reader: DuplexStream,
    mut writer: DuplexStream,
) {
    let mut session = String::new();
    let mut answered = 0;
    while let Some(request) = read_framed(&mut reader).await {
        let Some(method) = request["method"].as_str().map(str::to_owned) else {
            continue;
        };
        seen.lock().unwrap().requests.push(request.clone());
        if request.get("id").is_none() {
            continue;
        }
        let result = match method.as_str() {
            "connect" => json!({"ok": true, "protocolVersion": 3, "version": "test"}),
            "session.create" => {
                session = request["params"]["sessionId"].as_str().unwrap().to_owned();
                json!({"sessionId": session, "workspacePath": "/tmp/none"})
            }
            "session.send" => json!({"messageId": "m1"}),
            "session.accounts.login.begin" => json!({
                "flowId": "flow-1",
                "step": {
                    "kind": "device-code",
                    "userCode": "ABCD-1234",
                    "verificationUri": "https://github.com/login/device",
                    "expiresIn": 900,
                    "interval": 5,
                },
            }),
            // First the runtime asks to keep the credentials in a file, then finishes.
            "session.accounts.login.advance" if request["params"]["input"] == "yes" => json!({
                "kind": "completed",
                "result": {"status": "completed", "login": "octocat", "host": "github.com"},
            }),
            "session.accounts.login.advance" => json!({
                "kind": "completed",
                "result": {"status": "needs-plaintext-consent"},
            }),
            _ => json!({}),
        };
        write_framed(
            &mut writer,
            &json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
        )
        .await;

        match (method.as_str(), script) {
            ("session.send", Script::TwoTools) => {
                for part in ["Writing ", "two files."] {
                    event(
                        &mut writer,
                        &session,
                        "assistant.message_delta",
                        json!({"messageId": "a1", "deltaContent": part}),
                    )
                    .await;
                }
                event(
                    &mut writer,
                    &session,
                    "assistant.message",
                    json!({"messageId": "a1", "content": "Writing two files."}),
                )
                .await;
                event(
                    &mut writer,
                    &session,
                    "assistant.usage",
                    json!({"inputTokens": 100, "outputTokens": 20}),
                )
                .await;
                for (n, path) in [(1, "index.html"), (2, "style.css")] {
                    event(
                        &mut writer,
                        &session,
                        "external_tool.requested",
                        json!({
                            "requestId": format!("req-{n}"),
                            "sessionId": session,
                            "toolCallId": format!("tc-{n}"),
                            "toolName": "write_file",
                            "arguments": {"path": path},
                        }),
                    )
                    .await;
                }
            }
            ("session.tools.handlePendingToolCall", Script::TwoTools) => {
                answered += 1;
                if answered == 2 {
                    event(
                        &mut writer,
                        &session,
                        "assistant.message",
                        json!({"messageId": "a2", "content": "Both files are written."}),
                    )
                    .await;
                    event(
                        &mut writer,
                        &session,
                        "assistant.usage",
                        json!({"inputTokens": 150, "outputTokens": 8}),
                    )
                    .await;
                    event(&mut writer, &session, "session.idle", json!({})).await;
                }
            }
            ("session.send", Script::PlainAnswer) => {
                event(
                    &mut writer,
                    &session,
                    "assistant.message",
                    json!({"messageId": "a1", "content": "Hello."}),
                )
                .await;
                event(&mut writer, &session, "session.idle", json!({})).await;
            }
            ("session.send", Script::Fails) => {
                event(
                    &mut writer,
                    &session,
                    "session.error",
                    json!({"errorType": "rate_limit", "message": "slow down"}),
                )
                .await;
            }
            _ => {}
        }
    }
}

#[async_trait::async_trait]
impl Connector for Fake {
    async fn connect(&self) -> Result<Connection, LlmError> {
        self.seen.lock().unwrap().connections += 1;
        let (client_write, server_read) = duplex(1 << 16);
        let (server_write, client_read) = duplex(1 << 16);
        let server = tokio::spawn(serve(
            self.script,
            self.seen.clone(),
            server_read,
            server_write,
        ));
        let client = connect_streams(client_read, client_write, std::env::temp_dir()).await?;
        Ok(Connection {
            client,
            guard: Box::new(AbortOnDrop(server)),
            pid: None,
        })
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn provider(script: Script) -> (Copilot, Arc<Mutex<Seen>>) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let fake = Fake {
        script,
        seen: seen.clone(),
    };
    (Copilot::with_connector(&config(), Box::new(fake)), seen)
}

fn request() -> ChatRequest {
    ChatRequest {
        system: "You build sites.".into(),
        messages: vec![Message::User {
            content: "Make a page".into(),
        }],
        tools: vec![ToolDef {
            name: "write_file".into(),
            description: "Write a file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }],
    }
}

fn sent(seen: &Arc<Mutex<Seen>>, method: &str) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r["method"] == method)
        .cloned()
        .collect()
}

#[tokio::test]
async fn tool_calls_end_the_response_and_their_results_continue_the_same_session() {
    let (copilot, seen) = provider(Script::TwoTools);
    let mut request = request();

    let first = collect(copilot.stream(request.clone()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(first.text, "Writing two files.");
    assert_eq!(first.stop, Some(StopReason::ToolUse));
    assert_eq!(
        first.usage,
        Usage {
            input_tokens: 100,
            output_tokens: 20
        }
    );
    let calls: Vec<(&str, &str)> = first
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.arguments["path"].as_str().unwrap()))
        .collect();
    assert_eq!(calls, [("tc-1", "index.html"), ("tc-2", "style.css")]);
    // The runtime is still waiting: nothing has been answered yet.
    assert!(sent(&seen, "session.tools.handlePendingToolCall").is_empty());

    request.messages.push(Message::Assistant {
        text: first.text,
        tool_calls: first.tool_calls.clone(),
        raw: None,
    });
    for (call, (content, is_error)) in first
        .tool_calls
        .iter()
        .zip([("wrote index.html", false), ("disk full", true)])
    {
        request.messages.push(Message::Tool {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: content.into(),
            is_error,
        });
    }
    let second = collect(copilot.stream(request).await.unwrap())
        .await
        .unwrap();
    assert_eq!(second.text, "Both files are written.");
    assert_eq!(second.stop, Some(StopReason::EndTurn));
    assert!(second.tool_calls.is_empty());
    // Usage is per response, not a running total.
    assert_eq!(
        second.usage,
        Usage {
            input_tokens: 150,
            output_tokens: 8
        }
    );

    // One runtime, one session, one prompt: the second call continued the first.
    assert_eq!(seen.lock().unwrap().connections, 1);
    assert_eq!(sent(&seen, "session.create").len(), 1);
    assert_eq!(sent(&seen, "session.send").len(), 1);
    let answers = sent(&seen, "session.tools.handlePendingToolCall");
    assert_eq!(answers.len(), 2);
    let ok = answers
        .iter()
        .find(|a| a["params"]["requestId"] == "req-1")
        .unwrap();
    assert_eq!(ok["params"]["result"], "wrote index.html");
    let failed = answers
        .iter()
        .find(|a| a["params"]["requestId"] == "req-2")
        .unwrap();
    assert_eq!(failed["params"]["result"]["textResultForLlm"], "disk full");
    assert_eq!(failed["params"]["result"]["resultType"], "failure");
}

#[tokio::test]
async fn the_session_gets_our_prompt_and_tools_and_none_of_the_runtimes_own() {
    let (copilot, seen) = provider(Script::PlainAnswer);
    let answer = collect(copilot.stream(request()).await.unwrap())
        .await
        .unwrap();
    // A message that was not streamed still arrives as text.
    assert_eq!(answer.text, "Hello.");
    assert_eq!(answer.stop, Some(StopReason::EndTurn));

    let create = &sent(&seen, "session.create")[0]["params"];
    assert_eq!(create["model"], "gpt-test");
    assert_eq!(create["systemMessage"]["mode"], "replace");
    assert_eq!(create["systemMessage"]["content"], "You build sites.");
    assert_eq!(create["availableTools"], json!(["write_file"]));
    assert_eq!(create["tools"][0]["name"], "write_file");
    assert_eq!(create["tools"].as_array().unwrap().len(), 1);
    assert_eq!(
        sent(&seen, "session.send")[0]["params"]["prompt"],
        "Make a page"
    );
}

#[tokio::test]
async fn a_request_without_tools_still_restricts_the_runtime() {
    let (copilot, seen) = provider(Script::PlainAnswer);
    let mut request = request();
    request.tools.clear();
    collect(copilot.stream(request).await.unwrap())
        .await
        .unwrap();
    let create = &sent(&seen, "session.create")[0]["params"];
    assert_eq!(create["availableTools"], json!(["sitewright_no_tools"]));
}

#[tokio::test]
async fn a_session_error_fails_the_response() {
    let (copilot, _seen) = provider(Script::Fails);
    let mut stream = copilot.stream(request()).await.unwrap();
    let mut error = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            error = Some(e);
        }
    }
    assert!(matches!(error, Some(LlmError::RateLimited { .. })));
}

#[tokio::test]
async fn results_for_an_unknown_session_start_a_new_one_with_the_history() {
    let (copilot, seen) = provider(Script::PlainAnswer);
    let mut request = request();
    request.messages.push(Message::Assistant {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: "gone".into(),
            name: "write_file".into(),
            arguments: json!({"path": "a"}),
        }],
        raw: None,
    });
    request.messages.push(Message::Tool {
        call_id: "gone".into(),
        name: "write_file".into(),
        content: "wrote a".into(),
        is_error: false,
    });
    let answer = collect(copilot.stream(request).await.unwrap())
        .await
        .unwrap();
    assert_eq!(answer.text, "Hello.");
    let prompt = sent(&seen, "session.send")[0]["params"]["prompt"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(prompt.contains("[result of write_file]\nwrote a"));
}

#[tokio::test]
async fn dropping_a_response_midway_does_not_leave_the_session_parked() {
    let (copilot, seen) = provider(Script::TwoTools);
    let mut stream = copilot.stream(request()).await.unwrap();
    // Read one event, then walk away, as a cancelled run does.
    assert!(matches!(stream.next().await, Some(Ok(_))));
    drop(stream);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    // A new conversation starts cleanly on a fresh session.
    let again = collect(copilot.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(again.stop, Some(StopReason::ToolUse));
    assert_eq!(sent(&seen, "session.create").len(), 2);
}

#[tokio::test]
async fn signing_in_shows_a_code_and_finishes_in_the_background() {
    use sw_llm::copilot::{SignIn, sign_in_state, sign_in_with};
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let fake = Fake {
        script: Script::PlainAnswer,
        seen: seen.clone(),
    };
    assert_eq!(sign_in_state(), SignIn::Idle);
    let shown = sign_in_with(&fake).await.unwrap();
    assert_eq!(
        shown,
        SignIn::Waiting {
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
        }
    );
    let begin = &sent(&seen, "session.accounts.login.begin")[0]["params"];
    assert_eq!(begin["kind"], "githubDotCom");
    assert_eq!(begin["method"], "device-code");

    let mut state = sign_in_state();
    for _ in 0..100 {
        if !matches!(state, SignIn::Waiting { .. }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        state = sign_in_state();
    }
    assert_eq!(
        state,
        SignIn::Done {
            login: Some("octocat".into())
        }
    );
    // Consent to file storage was given exactly once.
    let advances = sent(&seen, "session.accounts.login.advance");
    assert_eq!(advances.len(), 2);
    assert_eq!(advances[1]["params"]["input"], "yes");
    // The sign-in session offers the runtime nothing to do.
    let create = &sent(&seen, "session.create")[0]["params"];
    assert_eq!(create["availableTools"], json!(["sitewright_no_tools"]));
}
