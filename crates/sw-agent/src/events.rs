//! The events a run produces. They are stored in `agent_events`, streamed to the UI and
//! CLI, and can be replayed later.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Longest string kept inside a stored tool call or result; longer ones are shortened.
const MAX_FIELD_CHARS: usize = 600;

/// One step of a run, as seen from outside.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// The run began.
    Started {
        site: String,
        provider: String,
        model: String,
    },
    /// A fragment of the model's text, sent live only (never stored).
    TextDelta { delta: String },
    /// The model's complete text for one turn.
    Text { text: String },
    /// The model asked for a tool. Long arguments (file contents) are shortened.
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    /// The tool's answer. Long results are shortened.
    ToolResult {
        id: String,
        name: String,
        is_error: bool,
        content: String,
    },
    /// Running totals after a model call.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: Option<f64>,
    },
    /// The agent asked for something only a person may approve.
    Confirmation {
        action: String,
        site: String,
        message: String,
    },
    /// The run ended. Always the last event.
    Done { status: String, message: String },
}

impl RunEvent {
    /// Whether this event is kept in the database (text deltas are not).
    pub fn is_stored(&self) -> bool {
        !matches!(self, RunEvent::TextDelta { .. })
    }
}

/// Shorten long strings anywhere inside a JSON value, for display and storage.
pub fn shorten_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(shorten(s)),
        Value::Array(items) => Value::Array(items.iter().map(shorten_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), shorten_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Shorten a long string, saying how much was left out.
pub fn shorten(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_FIELD_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(MAX_FIELD_CHARS).collect();
    format!("{head}… ({total} characters in total)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn long_fields_are_shortened_everywhere() {
        let long = "x".repeat(5000);
        let value = json!({"path": "index.html", "content": long, "nested": [{"a": long}]});
        let short = shorten_value(&value);
        assert_eq!(short["path"], "index.html");
        assert!(
            short["content"]
                .as_str()
                .unwrap()
                .ends_with("(5000 characters in total)")
        );
        assert!(short["nested"][0]["a"].as_str().unwrap().chars().count() < 700);
    }

    #[test]
    fn events_serialize_with_a_type_tag() {
        let event = RunEvent::Done {
            status: "succeeded".into(),
            message: "ok".into(),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"type": "done", "status": "succeeded", "message": "ok"})
        );
        assert!(!RunEvent::TextDelta { delta: "a".into() }.is_stored());
    }
}
