//! Sitewright LLM access: the [`LlmProvider`] trait and its provider adapters.
//!
//! The agent talks to every model through one normalized shape: a system prompt, a list of
//! [`Message`]s and [`ToolDef`]s go in, a stream of [`Event`]s comes out. Each adapter
//! translates that shape to and from one wire format. There is no LLM framework
//! underneath, only `reqwest`.

pub mod anthropic;
pub mod copilot;
pub mod error;
pub mod gemini;
pub mod http;
pub mod mock;
pub mod openai_compat;
pub mod sse;

use std::pin::Pin;
use std::sync::Arc;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use sw_core::Secrets;
use sw_core::config::{ProviderConfig, ProviderKind};

pub use error::LlmError;

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: String,
    },
    /// What the model said and which tools it asked for.
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
        /// The provider's own form of this turn, when it must be sent back unchanged
        /// (Anthropic thinking blocks, Gemini thought signatures). Only meaningful to the
        /// adapter that produced it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<serde_json::Value>,
    },
    /// The result of one tool call, answering the `ToolCall` with the same `call_id`.
    Tool {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

/// A tool the model may call. `parameters` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The arguments as a JSON object. If the model produced invalid JSON, this is
    /// `{"_invalid_json": "<raw text>"}` so the agent can report the problem back.
    pub arguments: serde_json::Value,
}

/// Everything needed for one model call.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}

/// Token counts for one model call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its answer.
    EndTurn,
    /// The model wants tool results before continuing.
    ToolUse,
    /// The output token limit cut the answer short.
    MaxTokens,
    /// Anything else the provider reported (content filter, and so on).
    Other(String),
}

/// One item in the normalized response stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A piece of the model's text answer.
    TextDelta(String),
    /// A complete tool call. Adapters buffer partial arguments and emit this once.
    ToolCall(ToolCall),
    /// The provider's own form of the whole assistant turn, to be stored in
    /// [`Message::Assistant::raw`] and replayed on the next call.
    Raw(serde_json::Value),
    /// Token usage for this call. May arrive more than once; later values replace earlier.
    Usage(Usage),
    /// The last event of a successful stream.
    Stop(StopReason),
}

/// The response to one model call.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event, LlmError>> + Send>>;

/// A model endpoint that can stream a chat completion with tool calls.
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync + std::fmt::Debug {
    /// The configured provider id, for example `azure`.
    fn id(&self) -> &str;
    /// The model (or deployment) name requests are sent to.
    fn model(&self) -> &str;
    /// Start one model call. Errors before the first event (authentication, rate limits)
    /// are returned here, after retries; errors mid-stream arrive as stream items.
    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError>;
}

/// Build the adapter for a configured provider, looking its key up in `secrets`.
pub fn build_provider(
    config: &ProviderConfig,
    secrets: &Secrets,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let key = match &config.key_ref {
        Some(name) => Some(
            secrets
                .key(name)
                .ok_or_else(|| {
                    LlmError::Config(format!(
                        "provider {:?} needs the key {name:?}, which is not in secrets.toml",
                        config.id
                    ))
                })?
                .to_owned(),
        ),
        None => None,
    };
    match config.kind {
        ProviderKind::OpenaiCompat => Ok(Arc::new(openai_compat::OpenAiCompat::new(config, key)?)),
        ProviderKind::Anthropic => Ok(Arc::new(anthropic::Anthropic::new(config, key)?)),
        ProviderKind::Gemini => Ok(Arc::new(gemini::Gemini::new(config, key)?)),
        ProviderKind::Copilot => Ok(Arc::new(copilot::Copilot::new(config, key)?)),
    }
}

/// Collect a whole response: the text, the tool calls, usage and the stop reason.
/// Convenient for tests and for one-shot calls such as "Test connection".
pub async fn collect(mut stream: EventStream) -> Result<Collected, LlmError> {
    use futures_util::StreamExt;
    let mut out = Collected::default();
    while let Some(event) = stream.next().await {
        match event? {
            Event::TextDelta(text) => out.text.push_str(&text),
            Event::ToolCall(call) => out.tool_calls.push(call),
            Event::Usage(usage) => out.usage = usage,
            Event::Raw(raw) => out.raw = Some(raw),
            Event::Stop(reason) => out.stop = Some(reason),
        }
    }
    Ok(out)
}

/// A fully received response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Collected {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop: Option<StopReason>,
    pub raw: Option<serde_json::Value>,
}

/// The outcome of [`test_connection`].
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionTest {
    pub ok: bool,
    pub latency_ms: u64,
    /// What happened, in words.
    pub detail: String,
}

/// Send a tiny request that asks the model to call a tool, and report whether it did.
/// This exercises exactly what the agent needs: authentication, streaming and tool calls.
pub async fn test_connection(provider: &dyn LlmProvider) -> ConnectionTest {
    let request = ChatRequest {
        system: "You are a connection test. Call the `ping` tool once with the value \"pong\" and say nothing else.".into(),
        messages: vec![Message::User { content: "Run the connection test now.".into() }],
        tools: vec![ToolDef {
            name: "ping".into(),
            description: "Connection test. Call it with value \"pong\".".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"],
                "additionalProperties": false,
            }),
        }],
    };
    let started = std::time::Instant::now();
    let outcome = match provider.stream(request).await {
        Ok(stream) => collect(stream).await,
        Err(e) => Err(e),
    };
    let latency_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(response) if response.tool_calls.iter().any(|c| c.name == "ping") => ConnectionTest {
            ok: true,
            latency_ms,
            detail: format!(
                "{} answered with a tool call, as the agent needs",
                provider.model()
            ),
        },
        Ok(_) => ConnectionTest {
            ok: false,
            latency_ms,
            detail: format!(
                "{} answered, but did not call the test tool. The agent needs a model with reliable tool calling.",
                provider.model()
            ),
        },
        Err(e) => ConnectionTest {
            ok: false,
            latency_ms,
            detail: e.to_string(),
        },
    }
}
