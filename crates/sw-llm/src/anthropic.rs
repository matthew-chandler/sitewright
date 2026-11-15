//! The Anthropic adapter (Messages API with streaming and tool use).
//!
//! Claude's turn is a list of content blocks: text, `tool_use`, and (on models that
//! think) `thinking`. The API expects an assistant turn to be sent back exactly as it was
//! produced, thinking blocks and their signatures included, so this adapter rebuilds the
//! blocks from the stream, hands them to the agent as [`Event::Raw`], and replays them
//! verbatim on the next call.

use std::time::Duration;

use serde_json::{Map, Value, json};
use sw_core::config::ProviderConfig;

use crate::error::LlmError;
use crate::http;
use crate::sse::SseEvent;
use crate::{ChatRequest, Event, EventStream, LlmProvider, Message, StopReason, ToolCall, Usage};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2026-10-21";
/// `max_tokens` is required. Streaming makes a generous ceiling safe, and whole files
/// are written in one tool call, so do not lowball it.
const DEFAULT_MAX_TOKENS: u32 = 64_000;

pub struct Anthropic {
    id: String,
    model: String,
    url: String,
    key: Option<String>,
    timeout: Duration,
    max_retries: u32,
    max_tokens: u32,
    extra_body: Map<String, Value>,
    client: reqwest::Client,
}

impl std::fmt::Debug for Anthropic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Anthropic")
            .field("id", &self.id)
            .field("model", &self.model)
            .field("url", &self.url)
            .finish()
    }
}

impl Anthropic {
    pub fn new(config: &ProviderConfig, key: Option<String>) -> Result<Self, LlmError> {
        let base = config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
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
            url: format!("{base}/v1/messages"),
            key,
            timeout,
            max_retries: config.max_retries,
            max_tokens: config.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            extra_body,
            client: http::client(timeout)?,
        })
    }

    /// The JSON body for a request.
    fn body(&self, request: &ChatRequest) -> Value {
        let mut messages: Vec<Value> = Vec::with_capacity(request.messages.len());
        for message in &request.messages {
            match message {
                Message::User { content } => {
                    messages.push(
                        json!({ "role": "user", "content": [{ "type": "text", "text": content }] }),
                    );
                }
                Message::Assistant {
                    text,
                    tool_calls,
                    raw,
                } => {
                    // Replay the model's own blocks when we have them; otherwise rebuild.
                    let content = match raw {
                        Some(Value::Array(blocks)) if !blocks.is_empty() => {
                            Value::Array(blocks.clone())
                        }
                        _ => {
                            let mut blocks = Vec::new();
                            if !text.is_empty() {
                                blocks.push(json!({ "type": "text", "text": text }));
                            }
                            for call in tool_calls {
                                blocks.push(json!({ "type": "tool_use", "id": call.id, "name": call.name, "input": tool_input(&call.arguments) }));
                            }
                            Value::Array(blocks)
                        }
                    };
                    messages.push(json!({ "role": "assistant", "content": content }));
                }
                Message::Tool {
                    call_id,
                    content,
                    is_error,
                    ..
                } => {
                    let block = json!({ "type": "tool_result", "tool_use_id": call_id, "content": content, "is_error": is_error });
                    // All results for one assistant turn go in a single user message.
                    match messages.last_mut() {
                        Some(last)
                            if last["role"] == "user"
                                && last["content"][0]["type"] == "tool_result" =>
                        {
                            if let Some(blocks) = last["content"].as_array_mut() {
                                blocks.push(block);
                            }
                        }
                        _ => messages.push(json!({ "role": "user", "content": [block] })),
                    }
                }
            }
        }

        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("max_tokens".into(), json!(self.max_tokens));
        body.insert("stream".into(), json!(true));
        // Cache the growing prefix (tools, system, earlier turns): an agent run resends
        // it on every call.
        body.insert("cache_control".into(), json!({ "type": "ephemeral" }));
        if !request.system.is_empty() {
            body.insert("system".into(), json!(request.system));
        }
        body.insert("messages".into(), Value::Array(messages));
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                        // Stream large inputs (file contents) as they are generated. The
                        // decoder below and the tools themselves validate the result.
                        "eager_input_streaming": true,
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        for (key, value) in &self.extra_body {
            body.insert(key.clone(), value.clone());
        }
        Value::Object(body)
    }
}

/// A `tool_use` block's input must be an object.
fn tool_input(arguments: &Value) -> Value {
    match arguments {
        Value::Object(map) if !map.contains_key("_invalid_json") => arguments.clone(),
        _ => json!({}),
    }
}

#[async_trait::async_trait]
impl LlmProvider for Anthropic {
    fn id(&self) -> &str {
        &self.id
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let body = self.body(&request);
        let response = http::send_with_retries(self.max_retries, self.timeout, || {
            let builder = self
                .client
                .post(&self.url)
                .header("anthropic-version", API_VERSION)
                .json(&body);
            match &self.key {
                Some(key) => builder.header("x-api-key", key),
                None => builder,
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

/// Rebuilds content blocks from the event stream.
#[derive(Debug, Default)]
struct Decoder {
    /// Finished and in-progress blocks, by index.
    blocks: Vec<Block>,
    usage: Usage,
    stop: Option<StopReason>,
    finished: bool,
}

#[derive(Debug, Default)]
struct Block {
    /// The block as it will be replayed.
    value: Value,
    /// Accumulated `input_json_delta` fragments, for `tool_use` blocks.
    partial_json: String,
    /// The parsed tool call arguments (or the `_invalid_json` marker).
    arguments: Option<Value>,
}

impl Decoder {
    fn step(&mut self, event: Option<SseEvent>) -> Result<Vec<Event>, LlmError> {
        let Some(event) = event else {
            if self.finished {
                return Ok(Vec::new());
            }
            return Err(LlmError::Network(
                "the response stream ended before the model finished".into(),
            ));
        };
        if self.finished || event.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let data: Value = serde_json::from_str(&event.data)
            .map_err(|e| LlmError::Protocol(format!("unreadable stream event ({e})")))?;
        let mut out = Vec::new();
        match data["type"].as_str().unwrap_or("") {
            "message_start" => self.read_usage(&data["message"]["usage"]),
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(self.blocks.len() as u64) as usize;
                if self.blocks.len() <= index {
                    self.blocks.resize_with(index + 1, Block::default);
                }
                self.blocks[index].value = data["content_block"].clone();
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let Some(block) = self.blocks.get_mut(index) else {
                    return Err(LlmError::Protocol(
                        "a delta arrived for a block that was never started".into(),
                    ));
                };
                let delta = &data["delta"];
                let append = |block: &mut Block, field: &str, piece: &str| {
                    let current = block.value[field].as_str().unwrap_or("").to_owned();
                    block.value[field] = json!(current + piece);
                };
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or("");
                        append(block, "text", text);
                        if !text.is_empty() {
                            out.push(Event::TextDelta(text.to_owned()));
                        }
                    }
                    "input_json_delta" => block
                        .partial_json
                        .push_str(delta["partial_json"].as_str().unwrap_or("")),
                    "thinking_delta" => {
                        append(block, "thinking", delta["thinking"].as_str().unwrap_or(""))
                    }
                    "signature_delta" => append(
                        block,
                        "signature",
                        delta["signature"].as_str().unwrap_or(""),
                    ),
                    _ => {} // deltas we do not need (citations and future kinds)
                }
            }
            "content_block_stop" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                if let Some(block) = self.blocks.get_mut(index)
                    && block.value["type"] == "tool_use"
                {
                    let raw = block.partial_json.trim();
                    let parsed = if raw.is_empty() {
                        // No fragments: the input came whole in content_block_start.
                        Some(block.value["input"].clone())
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({}))
                    } else {
                        match serde_json::from_str::<Value>(raw) {
                            Ok(value @ Value::Object(_)) => value,
                            _ => json!({ "_invalid_json": raw }),
                        }
                    };
                    block.value["input"] = tool_input(&parsed);
                    out.push(Event::ToolCall(ToolCall {
                        id: block.value["id"].as_str().unwrap_or("").to_owned(),
                        name: block.value["name"].as_str().unwrap_or("").to_owned(),
                        arguments: parsed.clone(),
                    }));
                    block.arguments = Some(parsed);
                }
            }
            "message_delta" => {
                self.read_usage(&data["usage"]);
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop = Some(match reason {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        other => StopReason::Other(other.to_owned()),
                    });
                }
            }
            "message_stop" => {
                self.finished = true;
                let blocks: Vec<Value> = std::mem::take(&mut self.blocks)
                    .into_iter()
                    .map(|b| b.value)
                    .filter(|v| !v.is_null())
                    .collect();
                let has_calls = blocks.iter().any(|b| b["type"] == "tool_use");
                out.push(Event::Raw(Value::Array(blocks)));
                out.push(Event::Usage(self.usage));
                let stop = self.stop.take().unwrap_or(if has_calls {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                });
                out.push(Event::Stop(stop));
            }
            "error" => {
                let message = data["error"]["message"]
                    .as_str()
                    .unwrap_or("the provider reported an error")
                    .to_owned();
                return Err(match data["error"]["type"].as_str() {
                    Some("rate_limit_error") => LlmError::RateLimited {
                        message,
                        retry_after: None,
                    },
                    Some("authentication_error" | "permission_error") => LlmError::Auth {
                        status: 401,
                        message,
                    },
                    Some("invalid_request_error") => LlmError::BadRequest {
                        status: 400,
                        message,
                    },
                    _ => LlmError::Server {
                        status: 529,
                        message,
                    },
                });
            }
            _ => {} // ping and future event types
        }
        Ok(out)
    }

    /// Usage arrives in `message_start` (input side) and `message_delta` (output side).
    /// Cached input is still input: count reads and writes with it.
    fn read_usage(&mut self, usage: &Value) {
        let field = |name: &str| usage[name].as_u64();
        if let Some(input) = field("input_tokens") {
            self.usage.input_tokens = input
                + field("cache_creation_input_tokens").unwrap_or(0)
                + field("cache_read_input_tokens").unwrap_or(0);
        }
        if let Some(output) = field("output_tokens") {
            self.usage.output_tokens = output;
        }
    }
}
