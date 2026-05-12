//! Types shared between the agent's LLM backends (Chat Completions
//! and Responses). Lives outside `mod openai` so neither backend
//! depends on the other.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tau_proto::{AgentToolCall, CborValue, ConversationMessage, ToolDefinition};

/// The parts of a prompt needed by an LLM backend client.
pub struct PromptPayload<'a> {
    pub system_prompt: &'a str,
    pub messages: &'a [ConversationMessage],
    pub tools: &'a [ToolDefinition],
    /// Reasoning effort. `Off` disables; otherwise rendered into
    /// `reasoning_effort` (Chat Completions) or `reasoning.effort`
    /// (Responses), iff the provider supports it.
    pub effort: tau_proto::Effort,
    /// Whether to ask the provider for a visible reasoning summary,
    /// and at what verbosity. Only honored on backends whose config
    /// reports `supports_reasoning_summary`.
    pub thinking_summary: tau_proto::ThinkingSummary,
}

/// Transport / protocol error returned from any LLM backend stream.
#[derive(Debug)]
pub enum LlmError {
    Http(Box<ureq::Error>),
    HttpStatus(u16, String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::HttpStatus(code, body) => write!(f, "HTTP {code}: {body}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
        }
    }
}

impl std::error::Error for LlmError {}

impl LlmError {
    /// Whether this error is plausibly transient and worth retrying.
    ///
    /// We treat transport hiccups, mid-stream IO breaks, and
    /// server-side stream errors (overload, upstream timeout) as
    /// retryable. JSON parse failures, missing-choices, and 4xx
    /// statuses other than 408/425/429 are treated as our bug or a
    /// deterministic request-level rejection — retrying just burns
    /// quota.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http(_) => Some(Duration::ZERO),
            Self::Io(_) => Some(Duration::ZERO),
            Self::Json(_) => None,
            Self::HttpStatus(code, body) => match *code {
                408 | 425 => Some(Duration::ZERO),
                429 => usage_limit_retry_after(body),
                500..=599 => Some(Duration::ZERO),
                // Code 0 is synthesized by the Responses backend for
                // SSE-level events: the body is prefixed with
                // "stream error:" (mid-stream provider hiccup —
                // overload, upstream timeout, gateway reset),
                // "response failed:" (deterministic model error),
                // or "response incomplete:" (request-level cap).
                // Only the first class is worth retrying.
                0 if body.starts_with("stream error:") => Some(Duration::ZERO),
                _ => None,
            },
        }
    }
}

fn usage_limit_retry_after(body: &str) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if error.get("type")?.as_str()? != "usage_limit_reached" {
        return None;
    }
    if let Some(seconds) = error
        .get("resets_in_seconds")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(Duration::from_secs(seconds));
    }
    let resets_at = error.get("resets_at")?.as_u64()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(resets_at.saturating_sub(now)))
}

/// Accumulated streaming state shared by both backends.
pub struct StreamState {
    pub text: String,
    pub tool_calls: Vec<ToolCallAccumulator>,
    pub input_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Provider-supplied reasoning summary accumulated so far. `None`
    /// when the provider hasn't emitted any summary content (or when
    /// summaries weren't requested).
    pub thinking: Option<String>,
}

/// Accumulates one tool call across streaming chunks.
pub struct ToolCallAccumulator {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

impl StreamState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            thinking: None,
        }
    }

    /// Returns the final tool calls with parsed arguments.
    ///
    /// Accumulators with an empty `name` are dropped as stream
    /// artifacts. Both the Responses and Chat Completions paths
    /// eagerly extend `tool_calls` from argument-delta events so the
    /// index stays addressable; if the matching `output_item.added`
    /// (or `function.name` delta) never arrives, the slot stays
    /// nameless. Shipping it downstream would surface as an
    /// `invalid_tool` rejection in the harness, but the real fix is
    /// to not manufacture the call in the first place.
    pub fn into_tool_calls(self) -> Vec<AgentToolCall> {
        self.tool_calls
            .into_iter()
            .filter(|tc| !tc.name.is_empty())
            .map(|tc| {
                let args: serde_json::Value =
                    serde_json::from_str(&tc.arguments_json).unwrap_or(serde_json::Value::Null);
                AgentToolCall {
                    id: tc.id.into(),
                    name: tc.name.into(),
                    arguments: json_to_cbor(&args),
                    display: None,
                }
            })
            .collect()
    }
}

/// Maps `Effort` to the wire string the OpenAI Responses /
/// Chat Completions APIs accept. `Off` returns `None` so the field is
/// omitted from the request entirely.
pub fn effort_wire(level: tau_proto::Effort) -> Option<&'static str> {
    use tau_proto::Effort::*;
    match level {
        Off => None,
        Minimal => Some("minimal"),
        Low => Some("low"),
        Medium => Some("medium"),
        High => Some("high"),
        XHigh => Some("xhigh"),
    }
}

// ---------------------------------------------------------------------------
// CBOR ↔ JSON value conversion
// ---------------------------------------------------------------------------

pub fn cbor_to_json(v: &CborValue) -> serde_json::Value {
    match v {
        CborValue::Null => serde_json::Value::Null,
        CborValue::Bool(b) => serde_json::Value::Bool(*b),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            serde_json::json!(n)
        }
        CborValue::Float(f) => serde_json::json!(f),
        CborValue::Text(s) => serde_json::Value::String(s.clone()),
        CborValue::Bytes(bytes) => serde_json::Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            bytes,
        )),
        CborValue::Array(arr) => serde_json::Value::Array(arr.iter().map(cbor_to_json).collect()),
        CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    CborValue::Text(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                map.insert(key, cbor_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        CborValue::Tag(_, inner) => cbor_to_json(inner),
        other => {
            tracing::warn!(target: crate::LOG_TARGET, "unsupported CBOR value in tool input: {other:?}");
            serde_json::Value::Null
        }
    }
}

pub fn json_to_cbor(v: &serde_json::Value) -> CborValue {
    match v {
        serde_json::Value::Null => CborValue::Null,
        serde_json::Value::Bool(b) => CborValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CborValue::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                CborValue::Integer(u.into())
            } else if let Some(f) = n.as_f64() {
                CborValue::Float(f)
            } else {
                CborValue::Null
            }
        }
        serde_json::Value::String(s) => CborValue::Text(s.clone()),
        serde_json::Value::Array(arr) => CborValue::Array(arr.iter().map(json_to_cbor).collect()),
        serde_json::Value::Object(map) => CborValue::Map(
            map.iter()
                .map(|(k, v)| (CborValue::Text(k.clone()), json_to_cbor(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests;
