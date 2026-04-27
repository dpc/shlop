//! OpenAI-compatible chat completions client.
//!
//! Works with any endpoint speaking the OpenAI chat completions API:
//! llama.cpp, vLLM, Ollama, OpenAI, etc.

use std::io::BufRead;

use serde::{Deserialize, Serialize};
use tau_proto::{
    AgentToolCall, CborValue, ContentBlock, ConversationMessage, ConversationRole, ToolDefinition,
};

/// The parts of a prompt needed by the OpenAI client.
pub struct PromptPayload<'a> {
    pub system_prompt: &'a str,
    pub messages: &'a [ConversationMessage],
    pub tools: &'a [ToolDefinition],
    /// Reasoning effort. `Off` disables; otherwise rendered into
    /// `reasoning_effort` (Chat Completions) or `reasoning.effort`
    /// (Responses), iff the provider supports it.
    pub effort: tau_proto::Effort,
}

/// Configuration for the OpenAI-compatible backend.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    /// Whether the provider's API accepts a `reasoning_effort` field.
    /// Read from `models.json5` provider compat flags.
    pub supports_reasoning_effort: bool,
}

/// Error from the OpenAI client.
#[derive(Debug)]
pub enum OpenAiError {
    Http(Box<ureq::Error>),
    HttpStatus(u16, String),
    Io(std::io::Error),
    Json(serde_json::Error),
    #[allow(dead_code)]
    NoChoices,
}

impl std::fmt::Display for OpenAiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::HttpStatus(code, body) => write!(f, "HTTP {code}: {body}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::NoChoices => f.write_str("API returned no choices"),
        }
    }
}

impl std::error::Error for OpenAiError {}

/// Accumulated streaming state.
pub struct StreamState {
    pub text: String,
    pub tool_calls: Vec<ToolCallAccumulator>,
    pub input_tokens: Option<u64>,
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
                }
            })
            .collect()
    }
}

/// Calls the chat completions endpoint with streaming. Invokes the
/// callback with the accumulated text on each content delta.
/// Returns the final state (text + tool calls).
pub fn chat_completion_stream(
    config: &OpenAiConfig,
    request: &PromptPayload<'_>,
    mut on_update: impl FnMut(&str),
) -> Result<StreamState, OpenAiError> {
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));

    let body = build_request(config, request, true);
    let body_str = serde_json::to_string(&body).map_err(OpenAiError::Json)?;

    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", config.api_key))
        .send_string(&body_str)
        .map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                OpenAiError::HttpStatus(code, body)
            }
            other => OpenAiError::Http(Box::new(other)),
        })?;

    let reader = std::io::BufReader::new(response.into_reader());
    let mut state = StreamState::new();

    for line in reader.lines() {
        let line = line.map_err(OpenAiError::Io)?;

        // SSE format: lines starting with "data: "
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };

        if data == "[DONE]" {
            break;
        }

        let chunk: StreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };

        if state.input_tokens.is_none() {
            state.input_tokens = chunk.usage.and_then(|usage| usage.prompt_tokens);
        }

        // Accumulate text content.
        if let Some(content) = choice.delta.content {
            state.text.push_str(&content);
            on_update(&state.text);
        }

        // Accumulate tool calls.
        if let Some(tool_calls) = choice.delta.tool_calls {
            for tc in tool_calls {
                let index = tc.index.unwrap_or(0) as usize;

                // Extend the list if needed.
                while state.tool_calls.len() <= index {
                    state.tool_calls.push(ToolCallAccumulator {
                        id: String::new(),
                        name: String::new(),
                        arguments_json: String::new(),
                    });
                }

                let acc = &mut state.tool_calls[index];
                if let Some(id) = tc.id {
                    acc.id = id;
                }
                if let Some(function) = tc.function {
                    if let Some(name) = function.name {
                        acc.name = name;
                    }
                    if let Some(args) = function.arguments {
                        acc.arguments_json.push_str(&args);
                    }
                }
            }
        }
    }

    Ok(state)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CompletionRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    stream: bool,
    /// Standard OpenAI Chat Completions reasoning control. Sent only
    /// when the provider supports it and the user picked a non-Off
    /// effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct ApiToolCall {
    id: String,
    r#type: String,
    function: ApiFunction,
}

#[derive(Serialize)]
struct ApiFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ApiTool {
    r#type: String,
    function: ApiToolFunction,
}

#[derive(Serialize)]
struct ApiToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

fn build_request(
    config: &OpenAiConfig,
    request: &PromptPayload<'_>,
    stream: bool,
) -> CompletionRequest {
    let mut messages = Vec::new();

    if !request.system_prompt.is_empty() {
        messages.push(ApiMessage {
            role: "system".to_owned(),
            content: Some(request.system_prompt.to_owned()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    for msg in request.messages {
        convert_message(msg, &mut messages);
    }

    let tools: Vec<ApiTool> = request.tools.iter().map(convert_tool_definition).collect();
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some("auto".to_owned())
    };

    let reasoning_effort = if config.supports_reasoning_effort {
        effort_wire(request.effort)
    } else {
        None
    };

    CompletionRequest {
        model: config.model_id.clone(),
        messages,
        tools,
        tool_choice,
        stream,
        reasoning_effort,
    }
}

/// Maps `Effort` to the wire string the OpenAI Responses /
/// Chat Completions APIs accept. `Off` returns `None` so the field is
/// omitted from the request entirely.
pub(crate) fn effort_wire(level: tau_proto::Effort) -> Option<&'static str> {
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

fn convert_message(msg: &ConversationMessage, out: &mut Vec<ApiMessage>) {
    match msg.role {
        ConversationRole::User => {
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        out.push(ApiMessage {
                            role: "user".to_owned(),
                            content: Some(text.clone()),
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        out.push(ApiMessage {
                            role: "tool".to_owned(),
                            content: Some(content.clone()),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.to_string()),
                            name: None,
                        });
                    }
                    ContentBlock::ToolUse { .. } => {}
                }
            }
        }
        ConversationRole::Assistant => {
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        let args_json = cbor_to_json(input);
                        tool_calls.push(ApiToolCall {
                            id: id.to_string(),
                            r#type: "function".to_owned(),
                            function: ApiFunction {
                                name: name.as_str().to_owned(),
                                arguments: Some(
                                    serde_json::to_string(&args_json).unwrap_or_default(),
                                ),
                                description: None,
                                parameters: None,
                            },
                        });
                    }
                    ContentBlock::ToolResult { .. } => {}
                }
            }

            let content = if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            };

            out.push(ApiMessage {
                role: "assistant".to_owned(),
                content,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                name: None,
            });
        }
    }
}

fn convert_tool_definition(tool: &ToolDefinition) -> ApiTool {
    ApiTool {
        r#type: "function".to_owned(),
        function: ApiToolFunction {
            name: tool.name.as_str().to_owned(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// Streaming response parsing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: Option<u32>,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
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
        CborValue::Bytes(_) => serde_json::Value::Null,
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
        _ => serde_json::Value::Null,
    }
}

pub fn json_to_cbor(v: &serde_json::Value) -> CborValue {
    match v {
        serde_json::Value::Null => CborValue::Null,
        serde_json::Value::Bool(b) => CborValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CborValue::Integer(i.into())
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
mod tests {
    use super::*;

    #[test]
    fn into_tool_calls_drops_nameless_accumulator_artifacts() {
        // The streaming paths eagerly extend `tool_calls` from
        // argument-delta events so the index stays addressable. If
        // the matching name-carrying event never arrives (partial
        // item, reasoning noise, stream cancellation), the slot stays
        // nameless. Shipping it downstream would trigger a visible
        // `invalid_tool` rejection in the harness and confuse the
        // model, which never intended a second tool call.
        let state = StreamState {
            text: String::new(),
            tool_calls: vec![
                ToolCallAccumulator {
                    id: String::new(),
                    name: String::new(),
                    arguments_json: String::from("{\"stray\": \"delta\"}"),
                },
                ToolCallAccumulator {
                    id: "call_real".into(),
                    name: "shell".into(),
                    arguments_json: "{\"command\":\"ls\"}".into(),
                },
            ],
            input_tokens: None,
        };

        let calls = state.into_tool_calls();
        assert_eq!(calls.len(), 1, "nameless accumulator must be dropped");
        assert_eq!(calls[0].id.as_str(), "call_real");
        assert_eq!(calls[0].name.as_str(), "shell");
    }
}
