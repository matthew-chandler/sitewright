//! The Gemini adapter (`streamGenerateContent` with function calling).
//!
//! Gemini's turn is a list of parts: text, `functionCall`, and on thinking models parts
//! that carry a `thoughtSignature`. Those must be sent back as they came, so, as with
//! Anthropic, the model's parts are kept as [`Event::Raw`] and replayed.

use std::time::Duration;

use serde_json::{Map, Value, json};
use sw_core::config::ProviderConfig;

use crate::error::LlmError;
use crate::http;
use crate::sse::SseEvent;
use crate::{ChatRequest, Event, EventStream, LlmProvider, Message, StopReason, ToolCall, Usage};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

pub struct Gemini {
    id: String,
    model: String,
    url: String,
    key: Option<String>,
    timeout: Duration,
    max_retries: u32,
    max_output_tokens: Option<u32>,
    extra_body: Map<String, Value>,
    client: reqwest::Client,
}

impl std::fmt::Debug for Gemini {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gemini")
            .field("id", &self.id)
            .field("model", &self.model)
            .finish()
    }
}

impl Gemini {
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
        let model = config.model.trim_start_matches("models/");
        if model.is_empty()
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(LlmError::Config(format!(
                "provider {:?}: {:?} is not a valid Gemini model name",
                config.id, config.model
            )));
        }
        let extra_body = match serde_json::to_value(&config.extra_body) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
        Ok(Self {
            id: config.id.clone(),
            model: model.to_owned(),
            // The key goes in a header, never in the URL, so it cannot end up in logs.
            url: format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse"),
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
        let mut contents: Vec<Value> = Vec::with_capacity(request.messages.len());
        for message in &request.messages {
            match message {
                Message::User { content } => {
                    contents.push(json!({ "role": "user", "parts": [{ "text": content }] }))
                }
                Message::Assistant {
                    text,
                    tool_calls,
                    raw,
                } => {
                    let parts = match raw {
                        Some(Value::Array(parts)) if !parts.is_empty() => {
                            Value::Array(parts.clone())
                        }
                        _ => {
                            let mut parts = Vec::new();
                            if !text.is_empty() {
                                parts.push(json!({ "text": text }));
                            }
                            for call in tool_calls {
                                parts.push(json!({ "functionCall": { "name": call.name, "args": call_args(&call.arguments) } }));
                            }
                            Value::Array(parts)
                        }
                    };
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
                Message::Tool {
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    // The response must be an object; tool results are JSON text.
                    let parsed =
                        serde_json::from_str::<Value>(content).unwrap_or_else(|_| json!(content));
                    let response = if *is_error {
                        json!({ "error": parsed })
                    } else {
                        json!({ "result": parsed })
                    };
                    let part =
                        json!({ "functionResponse": { "name": name, "response": response } });
                    // All responses for one model turn go in a single user turn.
                    match contents.last_mut() {
                        Some(last)
                            if last["role"] == "user"
                                && last["parts"][0].get("functionResponse").is_some() =>
                        {
                            if let Some(parts) = last["parts"].as_array_mut() {
                                parts.push(part);
                            }
                        }
                        _ => contents.push(json!({ "role": "user", "parts": [part] })),
                    }
                }
            }
        }

        let mut body = Map::new();
        if !request.system.is_empty() {
            body.insert(
                "systemInstruction".into(),
                json!({ "parts": [{ "text": request.system }] }),
            );
        }
        body.insert("contents".into(), Value::Array(contents));
        if !request.tools.is_empty() {
            let declarations: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    let mut declaration = Map::new();
                    declaration.insert("name".into(), json!(t.name));
                    declaration.insert("description".into(), json!(t.description));
                    // Gemini rejects an object schema with no properties: leave it out.
                    let has_properties = t.parameters["properties"]
                        .as_object()
                        .is_some_and(|p| !p.is_empty());
                    if has_properties {
                        declaration.insert("parameters".into(), gemini_schema(&t.parameters));
                    }
                    Value::Object(declaration)
                })
                .collect();
            body.insert(
                "tools".into(),
                json!([{ "functionDeclarations": declarations }]),
            );
        }
        if let Some(max) = self.max_output_tokens {
            body.insert("generationConfig".into(), json!({ "maxOutputTokens": max }));
        }
        for (key, value) in &self.extra_body {
            body.insert(key.clone(), value.clone());
        }
        Value::Object(body)
    }
}

fn call_args(arguments: &Value) -> Value {
    match arguments {
        Value::Object(map) if !map.contains_key("_invalid_json") => arguments.clone(),
        _ => json!({}),
    }
}

/// Gemini accepts an OpenAPI-style subset of JSON Schema. Drop the keywords it rejects.
fn gemini_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    !matches!(key.as_str(), "additionalProperties" | "$schema" | "$id")
                })
                .map(|(key, value)| (key.clone(), gemini_schema(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(gemini_schema).collect()),
        other => other.clone(),
    }
}

#[async_trait::async_trait]
impl LlmProvider for Gemini {
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
            match &self.key {
                Some(key) => builder.header("x-goog-api-key", key),
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

#[derive(Debug, Default)]
struct Decoder {
    /// Every part of the model's turn, in order, for replay.
    parts: Vec<Value>,
    calls: usize,
    usage: Option<Usage>,
    finish: Option<String>,
    finished: bool,
}

impl Decoder {
    fn step(&mut self, event: Option<SseEvent>) -> Result<Vec<Event>, LlmError> {
        let Some(event) = event else {
            return self.finish();
        };
        if self.finished || event.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let chunk: Value = serde_json::from_str(&event.data)
            .map_err(|e| LlmError::Protocol(format!("unreadable stream chunk ({e})")))?;
        if let Some(error) = chunk.get("error") {
            let message = error["message"]
                .as_str()
                .unwrap_or("the provider reported an error")
                .to_owned();
            return Err(match error["code"].as_u64() {
                Some(429) => LlmError::RateLimited {
                    message,
                    retry_after: None,
                },
                Some(401 | 403) => LlmError::Auth {
                    status: 401,
                    message,
                },
                Some(400) => LlmError::BadRequest {
                    status: 400,
                    message,
                },
                code => LlmError::Server {
                    status: code.unwrap_or(500) as u16,
                    message,
                },
            });
        }
        if let Some(reason) = chunk["promptFeedback"]["blockReason"].as_str() {
            self.finish = Some(format!("blocked: {reason}"));
        }

        let mut out = Vec::new();
        if let Some(meta) = chunk.get("usageMetadata") {
            let field = |name: &str| meta[name].as_u64().unwrap_or(0);
            self.usage = Some(Usage {
                input_tokens: field("promptTokenCount"),
                // Thinking tokens are billed as output.
                output_tokens: field("candidatesTokenCount") + field("thoughtsTokenCount"),
            });
        }
        let candidate = &chunk["candidates"][0];
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(call) = part.get("functionCall") {
                self.calls += 1;
                let id = call["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{}", self.calls));
                let arguments = if call["args"].is_object() {
                    call["args"].clone()
                } else {
                    json!({})
                };
                out.push(Event::ToolCall(ToolCall {
                    id,
                    name: call["name"].as_str().unwrap_or("").to_owned(),
                    arguments,
                }));
                self.parts.push(part.clone());
            } else if let Some(text) = part["text"].as_str() {
                let is_thought = part["thought"].as_bool() == Some(true);
                if !is_thought && !text.is_empty() {
                    out.push(Event::TextDelta(text.to_owned()));
                }
                // Merge consecutive plain text chunks; keep anything with extra fields
                // (thoughts, signatures) as its own part.
                let plain = part.as_object().is_some_and(|o| o.len() == 1);
                match self.parts.last_mut() {
                    Some(last)
                        if plain
                            && last
                                .as_object()
                                .is_some_and(|o| o.len() == 1 && o.contains_key("text")) =>
                    {
                        let merged = last["text"].as_str().unwrap_or("").to_owned() + text;
                        last["text"] = json!(merged);
                    }
                    _ => self.parts.push(part.clone()),
                }
            } else {
                self.parts.push(part.clone());
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.finish = Some(reason.to_owned());
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<Event>, LlmError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let Some(reason) = self.finish.take() else {
            return Err(LlmError::Network(
                "the response stream ended before the model finished".into(),
            ));
        };
        let mut out = vec![Event::Raw(Value::Array(std::mem::take(&mut self.parts)))];
        if let Some(usage) = self.usage.take() {
            out.push(Event::Usage(usage));
        }
        out.push(Event::Stop(match reason.as_str() {
            _ if self.calls > 0 => StopReason::ToolUse,
            "STOP" => StopReason::EndTurn,
            "MAX_TOKENS" => StopReason::MaxTokens,
            other => StopReason::Other(other.to_ascii_lowercase()),
        }));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_keywords_gemini_rejects_are_removed_at_every_depth() {
        let schema = json!({
            "type": "object", "additionalProperties": false, "$schema": "x",
            "properties": { "a": { "type": "object", "additionalProperties": false, "properties": { "b": { "type": "string" } } } },
            "required": ["a"],
        });
        let cleaned = gemini_schema(&schema);
        assert_eq!(
            cleaned,
            json!({ "type": "object", "properties": { "a": { "type": "object", "properties": { "b": { "type": "string" } } } }, "required": ["a"] })
        );
    }
}
