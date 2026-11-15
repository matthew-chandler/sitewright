//! The OpenAI-compatible adapter (Chat Completions with streaming and tool calls).
//!
//! Covers OpenAI, Azure OpenAI (v1 endpoint, where the model is the deployment name),
//! OpenRouter, Groq, DeepSeek, Ollama, vLLM, LM Studio and anything else that speaks
//! `POST {base_url}/chat/completions`.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use sw_core::config::{AuthStyle, ProviderConfig};

use crate::error::LlmError;
use crate::http;
use crate::sse::SseEvent;
use crate::{ChatRequest, Event, EventStream, LlmProvider, Message, StopReason, ToolCall, Usage};

pub struct OpenAiCompat {
    id: String,
    model: String,
    url: String,
    auth: AuthStyle,
    key: Option<String>,
    timeout: Duration,
    max_retries: u32,
    max_output_tokens: Option<u32>,
    extra_body: Map<String, Value>,
    client: reqwest::Client,
}

impl std::fmt::Debug for OpenAiCompat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is deliberately left out.
        f.debug_struct("OpenAiCompat")
            .field("id", &self.id)
            .field("model", &self.model)
            .field("url", &self.url)
            .finish()
    }
}

impl OpenAiCompat {
    pub fn new(config: &ProviderConfig, key: Option<String>) -> Result<Self, LlmError> {
        let base = config.base_url.as_deref().ok_or_else(|| {
            LlmError::Config(format!("provider {:?} needs a base_url", config.id))
        })?;
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(LlmError::Config(format!(
                "provider {:?}: base_url must start with http:// or https://",
                config.id
            )));
        }
        let extra_body = match serde_json::to_value(&config.extra_body) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        Ok(Self {
            id: config.id.clone(),
            model: config.model.clone(),
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            auth: config.auth,
            key,
            timeout,
            max_retries: config.max_retries,
            max_output_tokens: config.max_output_tokens,
            extra_body,
            client: http::client(timeout)?,
        })
    }

    /// The JSON body for a request.
    fn body(&self, request: &ChatRequest) -> Value {
        let mut messages = Vec::with_capacity(request.messages.len() + 1);
        if !request.system.is_empty() {
            messages.push(json!({ "role": "system", "content": request.system }));
        }
        for message in &request.messages {
            messages.push(match message {
                Message::User { content } => json!({ "role": "user", "content": content }),
                Message::Assistant { text, tool_calls, .. } => {
                    let mut m = Map::new();
                    m.insert("role".into(), json!("assistant"));
                    // Content must be null, not "", when there are only tool calls.
                    m.insert("content".into(), if text.is_empty() { Value::Null } else { json!(text) });
                    if !tool_calls.is_empty() {
                        let calls: Vec<Value> = tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": { "name": c.name, "arguments": c.arguments.to_string() },
                                })
                            })
                            .collect();
                        m.insert("tool_calls".into(), Value::Array(calls));
                    }
                    Value::Object(m)
                }
                Message::Tool { call_id, content, .. } => {
                    json!({ "role": "tool", "tool_call_id": call_id, "content": content })
                }
            });
        }

        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("messages".into(), Value::Array(messages));
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({ "include_usage": true }));
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": { "name": t.name, "description": t.description, "parameters": t.parameters },
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        if let Some(max) = self.max_output_tokens {
            body.insert("max_completion_tokens".into(), json!(max));
        }
        for (key, value) in &self.extra_body {
            body.insert(key.clone(), value.clone());
        }
        Value::Object(body)
    }
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiCompat {
    fn id(&self) -> &str {
        &self.id
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let body = self.body(&request);
        let response = http::send_with_retries(self.max_retries, self.timeout, || {
            let builder = self.client.post(&self.url).json(&body);
            match (&self.key, self.auth) {
                (Some(key), AuthStyle::Bearer) => builder.bearer_auth(key),
                (Some(key), AuthStyle::ApiKeyHeader) => builder.header("api-key", key),
                (None, _) => builder,
            }
        })
        .await?;
        Ok(http::translate(
            http::sse_events(response, self.timeout),
            Decoder::default(),
            Decoder::step,
        ))
    }
}

/// Reassembles streamed chunks into events.
#[derive(Debug, Default)]
struct Decoder {
    /// Tool calls being assembled, by their `index` in the response.
    calls: Vec<PartialCall>,
    usage: Option<Usage>,
    stop: Option<StopReason>,
    /// Whether any tool call was emitted during this response.
    saw_calls: bool,
    finished: bool,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<CallDelta>,
}

#[derive(Deserialize)]
struct CallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl Decoder {
    fn step(&mut self, event: Option<SseEvent>) -> Result<Vec<Event>, LlmError> {
        let Some(event) = event else {
            return self.finish();
        };
        if self.finished {
            return Ok(Vec::new());
        }
        let data = event.data.trim();
        if data == "[DONE]" {
            return self.finish();
        }
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let chunk: Chunk = serde_json::from_str(data)
            .map_err(|e| LlmError::Protocol(format!("unreadable stream chunk ({e})")))?;
        if let Some(error) = chunk.error {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string());
            return Err(LlmError::Server {
                status: 200,
                message,
            });
        }

        let mut out = Vec::new();
        if let Some(usage) = chunk.usage {
            self.usage = Some(Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            });
        }
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content
                && !text.is_empty()
            {
                out.push(Event::TextDelta(text));
            }
            for delta in choice.delta.tool_calls {
                // Servers that do not send an index stream one call at a time.
                let index = delta.index.unwrap_or_else(|| {
                    self.calls
                        .len()
                        .saturating_sub(usize::from(delta.id.is_none()))
                });
                if self.calls.len() <= index {
                    self.calls.resize_with(index + 1, PartialCall::default);
                }
                let call = &mut self.calls[index];
                if let Some(id) = delta.id {
                    call.id = id;
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        call.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        call.arguments.push_str(&arguments);
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                let calls = self.take_calls();
                self.saw_calls |= !calls.is_empty();
                out.extend(calls);
                self.stop = Some(match reason.as_str() {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    other => StopReason::Other(other.to_owned()),
                });
            }
        }
        Ok(out)
    }

    /// Emit the buffered tool calls as complete events.
    fn take_calls(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.calls)
            .into_iter()
            .enumerate()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(index, call)| {
                let raw = call.arguments.trim();
                let arguments = if raw.is_empty() {
                    json!({})
                } else {
                    match serde_json::from_str::<Value>(raw) {
                        Ok(value @ Value::Object(_)) => value,
                        _ => json!({ "_invalid_json": raw }),
                    }
                };
                let id = if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                };
                Event::ToolCall(ToolCall {
                    id,
                    name: call.name,
                    arguments,
                })
            })
            .collect()
    }

    fn finish(&mut self) -> Result<Vec<Event>, LlmError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut out = self.take_calls();
        let had_calls = self.saw_calls || !out.is_empty();
        let Some(stop) = self.stop.take() else {
            return Err(LlmError::Network(
                "the response stream ended before the model finished".into(),
            ));
        };
        if let Some(usage) = self.usage.take() {
            out.push(Event::Usage(usage));
        }
        // Some servers report "stop" even when they returned tool calls.
        out.push(Event::Stop(if had_calls {
            StopReason::ToolUse
        } else {
            stop
        }));
        Ok(out)
    }
}
