//! OpenAI Codex Responses API client (ChatGPT subscriptions).
//!
//! Endpoint: `POST {base_url}/codex/responses`
//! SSE streaming with `response.output_text.delta` events.

use std::io::BufRead;

use serde::Serialize;
use tau_proto::{ContentBlock, ConversationMessage, ConversationRole};

use crate::openai::{OpenAiError, PromptPayload, StreamState, cbor_to_json};

/// Config for the Codex Responses API.
#[derive(Clone, Debug)]
pub struct ResponsesConfig {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    /// `chatgpt-account-id` header extracted from JWT.
    pub account_id: Option<String>,
}

/// Calls the Codex Responses API with SSE streaming.
pub fn responses_stream(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    mut on_update: impl FnMut(&str),
) -> Result<StreamState, OpenAiError> {
    let url = format!("{}/codex/responses", config.base_url.trim_end_matches('/'));

    let body = build_request(config, request);
    let body_str = serde_json::to_string(&body).map_err(OpenAiError::Json)?;

    let mut req = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream")
        .set("Authorization", &format!("Bearer {}", config.api_key))
        .set("OpenAI-Beta", "responses=experimental");

    if let Some(ref account_id) = config.account_id {
        req = req.set("chatgpt-account-id", account_id);
    }

    let response = req.send_string(&body_str).map_err(|e| match e {
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

        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };

        if data == "[DONE]" {
            break;
        }

        let event: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = event["type"].as_str().unwrap_or("");

        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    state.text.push_str(delta);
                    on_update(&state.text);
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(delta) = event["delta"].as_str() {
                    while state.tool_calls.len() <= output_index {
                        state.tool_calls.push(crate::openai::ToolCallAccumulator {
                            id: String::new(),
                            name: String::new(),
                            arguments_json: String::new(),
                        });
                    }
                    state.tool_calls[output_index]
                        .arguments_json
                        .push_str(delta);
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.get("item") {
                    if item["type"].as_str() == Some("function_call") {
                        let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                        while state.tool_calls.len() <= output_index {
                            state.tool_calls.push(crate::openai::ToolCallAccumulator {
                                id: String::new(),
                                name: String::new(),
                                arguments_json: String::new(),
                            });
                        }
                        if let Some(id) = item["call_id"].as_str() {
                            state.tool_calls[output_index].id = id.to_owned();
                        }
                        if let Some(name) = item["name"].as_str() {
                            state.tool_calls[output_index].name = decode_tool_name(name);
                        }
                    }
                }
            }
            "response.completed" | "response.done" => {
                break;
            }
            "response.incomplete" => {
                let reason = event
                    .get("response")
                    .and_then(|r| r["incomplete_details"]["reason"].as_str())
                    .unwrap_or("unknown reason");
                return Err(OpenAiError::HttpStatus(
                    0,
                    format!("response incomplete: {reason}"),
                ));
            }
            "response.failed" => {
                let detail = event
                    .get("response")
                    .and_then(|r| {
                        r["error"]["message"]
                            .as_str()
                            .or_else(|| r["error"]["code"].as_str())
                    })
                    .unwrap_or("unknown error");
                return Err(OpenAiError::HttpStatus(
                    0,
                    format!("response failed: {detail}"),
                ));
            }
            "error" => {
                let detail = event["error"]["message"]
                    .as_str()
                    .or_else(|| event["message"].as_str())
                    .unwrap_or("unknown error");
                return Err(OpenAiError::HttpStatus(
                    0,
                    format!("stream error: {detail}"),
                ));
            }
            _ => {}
        }
    }

    Ok(state)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<serde_json::Value>,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

fn build_request(config: &ResponsesConfig, request: &PromptPayload<'_>) -> ResponsesRequest {
    let instructions = if request.system_prompt.is_empty() {
        None
    } else {
        Some(request.system_prompt.to_owned())
    };

    let mut input = Vec::new();
    for msg in request.messages {
        convert_message(msg, &mut input);
    }

    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|t| {
            let mut tool = serde_json::json!({
                "type": "function",
                "name": encode_tool_name(&t.name),
                "strict": serde_json::Value::Null,
            });
            if let Some(ref desc) = t.description {
                tool["description"] = serde_json::Value::String(desc.clone());
            }
            if let Some(ref params) = t.parameters {
                tool["parameters"] = params.clone();
            }
            tool
        })
        .collect();

    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some("auto".to_owned())
    };

    ResponsesRequest {
        model: config.model_id.clone(),
        instructions,
        input,
        stream: true,
        store: false,
        tools,
        tool_choice,
    }
}

// ---------------------------------------------------------------------------
// Tool name encoding
// ---------------------------------------------------------------------------

/// Encode tool name for the API: replace non-`[a-zA-Z0-9_-]` with `_`.
fn encode_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Decode tool name from the API response.
///
/// Tool names no longer contain dots, so this is now an identity function.
/// Kept as a hook in case future encoding transforms are needed.
fn decode_tool_name(name: &str) -> String {
    name.to_owned()
}

// ---------------------------------------------------------------------------
// Conversation conversion
// ---------------------------------------------------------------------------

/// Counter for generating unique message IDs.
static MSG_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_msg_id() -> String {
    let n = MSG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("msg_{n}")
}

fn convert_message(msg: &ConversationMessage, out: &mut Vec<serde_json::Value>) {
    match msg.role {
        ConversationRole::User => {
            // Collect text blocks into one user message, emit tool results separately.
            let mut text_items: Vec<serde_json::Value> = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_items.push(serde_json::json!({
                            "type": "input_text",
                            "text": text,
                        }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {
                        // Flush any pending text first.
                        if !text_items.is_empty() {
                            out.push(serde_json::json!({
                                "role": "user",
                                "content": text_items,
                            }));
                            text_items = Vec::new();
                        }
                        let output = if *is_error {
                            format!("ERROR: {content}")
                        } else {
                            content.clone()
                        };
                        out.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": output,
                        }));
                    }
                    ContentBlock::ToolUse { .. } => {}
                }
            }
            if !text_items.is_empty() {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": text_items,
                }));
            }
        }
        ConversationRole::Assistant => {
            // Emit tool calls as individual function_call items,
            // text as a message item.
            let mut text_parts = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        // Emit any pending text first.
                        if !text_parts.is_empty() {
                            out.push(serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "id": next_msg_id(),
                                "status": "completed",
                                "content": [{
                                    "type": "output_text",
                                    "text": text_parts.join("\n"),
                                    "annotations": [],
                                }],
                            }));
                            text_parts.clear();
                        }
                        let args_json = cbor_to_json(input);
                        let fc_id = if id.starts_with("fc_") {
                            id.clone()
                        } else {
                            format!("fc_{id}")
                        };
                        out.push(serde_json::json!({
                            "type": "function_call",
                            "id": fc_id,
                            "call_id": id,
                            "name": encode_tool_name(name.as_str()),
                            "arguments": serde_json::to_string(&args_json).unwrap_or_default(),
                        }));
                    }
                    ContentBlock::ToolResult { .. } => {}
                }
            }
            if !text_parts.is_empty() {
                out.push(serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "id": next_msg_id(),
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": text_parts.join("\n"),
                        "annotations": [],
                    }],
                }));
            }
        }
    }
}
