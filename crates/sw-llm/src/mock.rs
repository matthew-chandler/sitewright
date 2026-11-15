//! A scripted provider for tests: it replays prepared responses and records the requests
//! it was given, so agent behaviour can be tested without a network or a model.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures_util::stream;
use serde_json::Value;

use crate::error::LlmError;
use crate::{ChatRequest, Event, EventStream, LlmProvider, StopReason, ToolCall, Usage};

/// One scripted model turn.
#[derive(Debug, Clone)]
pub enum Turn {
    /// A successful response made of these events (a `Stop` is appended if missing).
    Events(Vec<Event>),
    /// The call fails with this error.
    Fail(LlmError),
}

impl Turn {
    /// The model says `text` and ends its turn.
    pub fn text(text: &str) -> Self {
        Turn::Events(vec![
            Event::TextDelta(text.to_owned()),
            Event::Usage(Usage {
                input_tokens: 100,
                output_tokens: 20,
            }),
            Event::Stop(StopReason::EndTurn),
        ])
    }

    /// The model calls the given tools, as `(name, arguments)` pairs.
    pub fn tools(calls: &[(&str, Value)]) -> Self {
        let mut events: Vec<Event> = calls
            .iter()
            .enumerate()
            .map(|(i, (name, arguments))| {
                Event::ToolCall(ToolCall {
                    id: format!("call_{i}"),
                    name: (*name).to_owned(),
                    arguments: arguments.clone(),
                })
            })
            .collect();
        events.push(Event::Usage(Usage {
            input_tokens: 100,
            output_tokens: 20,
        }));
        events.push(Event::Stop(StopReason::ToolUse));
        Turn::Events(events)
    }
}

/// Replays [`Turn`]s in order. When the script runs out, every further call fails.
#[derive(Debug)]
pub struct ScriptedProvider {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<ChatRequest>>,
    /// When set, this turn is repeated forever once the script is exhausted.
    repeat: Option<Turn>,
}

impl ScriptedProvider {
    pub fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
            repeat: None,
        }
    }

    /// A provider that answers every call with the same turn. For testing limits.
    pub fn repeating(turn: Turn) -> Self {
        Self {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            repeat: Some(turn),
        }
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn model(&self) -> &str {
        "mock-model"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
        let next = self
            .turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .or_else(|| self.repeat.clone());
        match next {
            Some(Turn::Events(mut events)) => {
                if !matches!(events.last(), Some(Event::Stop(_))) {
                    events.push(Event::Stop(StopReason::EndTurn));
                }
                Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
            }
            Some(Turn::Fail(error)) => Err(error),
            None => Err(LlmError::Protocol(
                "the scripted provider has no more turns".into(),
            )),
        }
    }
}
