//! The `claude` CLI's stream-JSON stdio protocol.
//!
//! Newline-delimited JSON in both directions. This module only parses and
//! serialises; policy lives in [`crate::claude`].
//!
//! Three properties of the real stream drive the design here, all observed in
//! the verification spike:
//!
//! - `system/init` repeats **every turn**, so it must not be treated as a
//!   one-time handshake.
//! - Non-JSON lines do appear on stdout (a bare English sentence was observed
//!   after `--resume <unknown>`), so the reader tolerates garbage.
//! - Event types not seen during the spike exist (`rate_limit_event`,
//!   `compact_boundary`, `session_end`, …), so unknown types are skipped and
//!   logged rather than treated as fatal.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One line read from the agent process.
#[derive(Debug, Clone)]
pub enum Incoming {
    /// Session initialised. Repeats per turn.
    Init {
        session_id: String,
        tools: Vec<String>,
    },
    /// A complete assistant message.
    Assistant { content: Vec<ContentBlock> },
    /// A user-role message, which is how tool results come back.
    User { content: Vec<ContentBlock> },
    /// A partial-message delta for live rendering.
    StreamDelta { kind: String, text: String },
    /// The agent is asking Forge for a permission decision.
    ControlRequest {
        request_id: String,
        tool_name: String,
        input: Value,
        reason: Option<String>,
    },
    /// Turn finished.
    Result {
        session_id: String,
        is_error: bool,
        cost_usd: Option<f64>,
    },
    /// A recognised-but-unhandled event. Never fatal.
    Other { kind: String },
    /// A line that was not JSON at all.
    Unparsed(String),
}

#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        is_error: bool,
        content: String,
    },
}

/// Parse one line. Never fails: unrecognised input becomes `Unparsed`/`Other`.
pub fn parse_line(line: &str) -> Incoming {
    let line = line.trim();
    if line.is_empty() {
        return Incoming::Other {
            kind: "empty".into(),
        };
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Incoming::Unparsed(line.to_string());
    };

    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "system" if v.get("subtype").and_then(|s| s.as_str()) == Some("init") => Incoming::Init {
            session_id: str_field(&v, "session_id"),
            tools: v
                .get("tools")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        },

        "assistant" => Incoming::Assistant {
            content: blocks(&v),
        },
        "user" => Incoming::User {
            content: blocks(&v),
        },

        "stream_event" => {
            let d = v
                .get("event")
                .and_then(|e| e.get("delta"))
                .cloned()
                .unwrap_or(Value::Null);
            let kind = d
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let text = d
                .get("text")
                .or_else(|| d.get("thinking"))
                .or_else(|| d.get("partial_json"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            Incoming::StreamDelta { kind, text }
        }

        "control_request" => {
            let r = v.get("request").cloned().unwrap_or(Value::Null);
            Incoming::ControlRequest {
                request_id: str_field(&v, "request_id"),
                tool_name: str_field(&r, "tool_name"),
                input: r.get("input").cloned().unwrap_or(Value::Null),
                reason: r
                    .get("decision_reason")
                    .and_then(|x| x.as_str())
                    .map(String::from),
            }
        }

        "result" => Incoming::Result {
            session_id: str_field(&v, "session_id"),
            is_error: v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false),
            cost_usd: v.get("total_cost_usd").and_then(|c| c.as_f64()),
        },

        other => Incoming::Other {
            kind: other.to_string(),
        },
    }
}

fn str_field(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn blocks(v: &Value) -> Vec<ContentBlock> {
    let Some(arr) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|b| match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => Some(ContentBlock::Text(str_field(b, "text"))),
            Some("thinking") => Some(ContentBlock::Thinking(str_field(b, "thinking"))),
            Some("tool_use") => Some(ContentBlock::ToolUse {
                id: str_field(b, "id"),
                name: str_field(b, "name"),
                input: b.get("input").cloned().unwrap_or(Value::Null),
            }),
            Some("tool_result") => Some(ContentBlock::ToolResult {
                id: str_field(b, "tool_use_id"),
                is_error: b.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false),
                content: match b.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                },
            }),
            _ => None,
        })
        .collect()
}

/// A user turn written to the agent's stdin.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserTurn {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: UserMessage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserMessage {
    pub role: &'static str,
    pub content: String,
}

impl UserTurn {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: "user",
            message: UserMessage {
                role: "user",
                content: text.into(),
            },
        }
    }
}

/// Serialise a permission decision as a `control_response`.
pub fn control_response(request_id: &str, decision: &crate::Decision) -> String {
    use crate::Decision;

    let response = match decision {
        Decision::Allow => serde_json::json!({ "behavior": "allow" }),
        Decision::AllowWithInput(input) => {
            serde_json::json!({ "behavior": "allow", "updatedInput": input })
        }
        Decision::Deny { message } => {
            serde_json::json!({ "behavior": "deny", "message": message })
        }
    };

    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        }
    })
    .to_string()
}
