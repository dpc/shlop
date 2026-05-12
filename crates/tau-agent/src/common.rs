//! Types shared between the agent's LLM backends (Chat Completions
//! and Responses). Lives outside `mod openai` so neither backend
//! depends on the other.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tau_proto::{
    AgentToolCall, CborValue, ConversationMessage, PromptOriginator, SessionId, ToolDefinition,
};

/// The parts of a prompt needed by an LLM backend client.
pub struct PromptPayload<'a> {
    pub system_prompt: &'a str,
    pub messages: &'a [ConversationMessage],
    pub tools: &'a [ToolDefinition],
    /// Per-prompt model knobs (effort / verbosity / thinking-summary).
    /// Each field is honored only when the backend's config reports
    /// support for the corresponding provider feature.
    pub params: tau_proto::ModelParams,
    /// Whether the model may emit tool-call output on this turn.
    /// `Auto` (default) lets the model decide; `None` forces a text
    /// answer. Tools and `system_prompt` are still passed verbatim
    /// in either case so the prompt-cache prefix stays stable.
    pub tool_choice: tau_proto::ToolChoice,
    /// Hint from the harness for stateful chaining: the previous
    /// turn's `response_id` and the index in `messages` where new
    /// content for this turn begins. Backends that don't support
    /// stateful chaining (Chat Completions) ignore this and replay
    /// the full `messages` slice. The Responses backend slices
    /// `messages[index..]` and sets `previous_response_id` +
    /// `store: true` on the upstream call.
    pub previous_response: Option<PreviousResponse<'a>>,
    /// Who originated this prompt — the interactive user, or an
    /// extension-side sub-agent query (most notably `core-delegate`).
    /// Folded into the wire `prompt_cache_key` so concurrent
    /// delegated turns don't share a routing bucket with the user's
    /// own turns (OpenAI's deployment checklist warns that >15 RPM
    /// per `(prefix, prompt_cache_key)` overflows to additional
    /// machines and degrades hit rate).
    pub originator: &'a PromptOriginator,
    /// Harness session this prompt belongs to. Used by the Responses
    /// WebSocket pool to key per-conversation connections — same
    /// session stays on the same socket across turns, so the
    /// connection-local `previous_response_id` cache stays warm.
    /// Backends without a connection pool ignore this.
    pub session_id: &'a SessionId,
}

/// See [`PromptPayload::previous_response`].
#[derive(Clone, Copy)]
pub struct PreviousResponse<'a> {
    pub id: &'a str,
    pub message_index: usize,
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
    /// Provider-supplied `response.id`, used by the harness to chain
    /// the next turn off this one via `previous_response_id`. Only
    /// populated by the Responses backend; the Chat Completions
    /// backend leaves this `None`.
    pub response_id: Option<String>,
    /// Provider-supplied `phase` label on the assistant `message`
    /// output item (`commentary` vs. `final_answer`). Only populated
    /// by the Responses backend when its `supports_phase` flag is on
    /// and the model emitted one. Surfaced on
    /// [`tau_proto::AgentResponseFinished`] so the harness persists
    /// it for later replay.
    pub phase: Option<tau_proto::MessagePhase>,
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
            response_id: None,
            phase: None,
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

/// Maps `Verbosity` to the wire string OpenAI's `verbosity` /
/// `text.verbosity` field accepts. There is no "off" sentinel — the
/// caller gates the field on a provider-level `supports_verbosity`
/// flag instead.
pub fn verbosity_wire(level: tau_proto::Verbosity) -> &'static str {
    level.as_openai_wire()
}

/// Produce the wire `prompt_cache_key` for an outgoing request from a
/// per-`(base_url, model_id, cwd)` base key resolved upstream and the
/// originator of the current prompt.
///
/// User turns pass `base` through unchanged so a single interactive
/// session's successive turns keep landing on the same cache machine.
/// Extension-originated turns (e.g. `core-delegate` sub-agents) get a
/// distinct key derived from the extension's *name* so:
///   - sub-agent traffic doesn't pile onto the user agent's routing bucket —
///     OpenAI's deployment checklist warns that >15 RPM per `(prefix,
///     prompt_cache_key)` overflows to additional machines and degrades cache
///     effectiveness, and a parallel-delegate turn easily blows past that on a
///     shared key;
///   - the sub-agent's own multi-turn loop still reuses *its* cache because the
///     query id is intentionally NOT mixed in.
///
/// `None` in / `None` out: when the resolver chose not to send a
/// prompt cache key (provider doesn't support it, cwd unreadable),
/// no key is sent regardless of originator.
#[must_use]
pub fn mix_originator_into_cache_key(
    base: Option<&str>,
    originator: &PromptOriginator,
) -> Option<String> {
    let base = base?;
    match originator {
        PromptOriginator::User => Some(base.to_owned()),
        PromptOriginator::Extension { name, .. } => {
            let mut hasher = blake3::Hasher::new();
            hasher.update(base.as_bytes());
            hasher.update(b"\0ext:");
            hasher.update(name.as_str().as_bytes());
            Some(format!("tau-{}", hasher.finalize().to_hex()))
        }
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
