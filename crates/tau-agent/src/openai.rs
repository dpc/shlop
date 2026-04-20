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
}

/// Configuration for the OpenAI-compatible backend.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
}

/// Error from the OpenAI client.
#[derive(Debug)]
pub enum OpenAiError {
    Http(Box<ureq::Error>),
    Io(std::io::Error),
    Json(serde_json::Error),
    #[allow(dead_code)]
    NoChoices,
}

impl std::fmt::Display for OpenAiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
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
}

/// Accumulates one tool call across streaming chunks.
pub struct ToolCallAccumulator {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

impl StreamState {
    fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
        }
    }

    /// Returns the final tool calls with parsed arguments.
    pub fn into_tool_calls(self) -> Vec<AgentToolCall> {
        self.tool_calls
            .into_iter()
            .map(|tc| {
                let args: serde_json::Value =
                    serde_json::from_str(&tc.arguments_json).unwrap_or(serde_json::Value::Null);
                AgentToolCall {
                    id: tc.id,
                    name: tc.name,
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
        .map_err(|e| OpenAiError::Http(Box::new(e)))?;

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

    CompletionRequest {
        model: config.model_id.clone(),
        messages,
        tools,
        tool_choice,
        stream,
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
                            tool_call_id: Some(tool_use_id.clone()),
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
                            id: id.clone(),
                            r#type: "function".to_owned(),
                            function: ApiFunction {
                                name: name.clone(),
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
            name: tool.name.clone(),
            description: tool.description.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// Streaming response parsing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
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

// ---------------------------------------------------------------------------
// CBOR ↔ JSON value conversion
// ---------------------------------------------------------------------------

fn cbor_to_json(v: &CborValue) -> serde_json::Value {
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

fn json_to_cbor(v: &serde_json::Value) -> CborValue {
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
