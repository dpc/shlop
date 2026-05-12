use tau_config::settings::PromptCacheRetention;
use tau_proto::{ContentBlock, ConversationMessage, ConversationRole};

use super::*;
use crate::common::{LlmError, PreviousResponse};

#[test]
fn build_request_includes_prompt_cache_fields_when_configured() {
    let config = ResponsesConfig {
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        prompt_cache_key: Some("tau:seed".into()),
        prompt_cache_retention: Some(PromptCacheRetention::InMemory),
    };
    let request = PromptPayload {
        system_prompt: "system",
        messages: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let prompt_cache_key = body["prompt_cache_key"].as_str().expect("prompt_cache_key");

    assert_eq!(prompt_cache_key, "tau:seed");
    assert_eq!(body["prompt_cache_retention"], "in_memory");
}

#[test]
fn build_request_omits_prompt_cache_fields_without_seed_or_retention() {
    let config = ResponsesConfig {
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        prompt_cache_key: None,
        prompt_cache_retention: None,
    };
    let request = PromptPayload {
        system_prompt: "system",
        messages: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let object = body.as_object().expect("request object");

    assert!(!object.contains_key("prompt_cache_key"));
    assert!(!object.contains_key("prompt_cache_retention"));
}

/// First turn (no chain established): the request must contain the
/// full transcript, `store: false`, and no `previous_response_id`.
/// This is the baseline that future stateful-chain optimizations are
/// compared against; if it ever flips, every turn would start
/// charging for stored responses by accident.
#[test]
fn build_request_first_turn_replays_full_history_without_chain() {
    let config = chain_test_config();
    let messages = vec![user_text("hello"), assistant_text("hi there")];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(body["store"], false);
    assert!(
        body.as_object()
            .unwrap()
            .get("previous_response_id")
            .is_none()
    );
    let input = body["input"].as_array().expect("input array");
    // Two messages → two `input` items (one user text, one assistant message).
    assert_eq!(
        input.len(),
        2,
        "full history must be replayed when chain is absent"
    );
}

/// Stateful-chain turn: when the harness supplies a
/// `previous_response`, the request body slices off the prefix
/// already covered by that response and pins the prior `response.id`.
/// `store` stays `false` — the Codex endpoint *rejects* `store: true`
/// (`HTTP 400 {"detail":"Store must be set to false"}`) even when
/// chaining, in contrast with the public Responses API. Tau today
/// only routes Responses through Codex, so this asserts the Codex
/// shape; a future public-API path would need a separate test.
#[test]
fn build_request_chain_turn_sends_delta_and_previous_response_id() {
    let config = chain_test_config();
    // Full transcript: 1 user, 1 assistant, 1 user tool-result.
    // Chain anchor was captured after the assistant turn
    // (message_index = 2), so only the trailing tool-result should
    // make it into the request.
    let messages = vec![
        user_text("first turn"),
        assistant_text("first response"),
        user_text("second turn"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: Some(PreviousResponse {
            id: "resp_abc",
            message_index: 2,
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(
        body["store"], false,
        "Codex rejects store=true even when chaining"
    );
    assert_eq!(body["previous_response_id"], "resp_abc");
    let input = body["input"].as_array().expect("input array");
    assert_eq!(
        input.len(),
        1,
        "only messages after the anchor should be sent"
    );
    assert_eq!(input[0]["content"][0]["text"], "second turn");
}

/// Defensive: a stale `message_index` (somehow larger than the
/// assembled transcript) must NOT panic and must NOT chain — fall
/// back to a full-replay first-turn-style request so the conversation
/// keeps working instead of crashing the agent.
#[test]
fn build_request_chain_with_oob_index_falls_back_to_full_replay() {
    let config = chain_test_config();
    let messages = vec![user_text("only")];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: Some(PreviousResponse {
            id: "resp_abc",
            message_index: 99,
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(body["store"], false);
    assert!(
        body.as_object()
            .unwrap()
            .get("previous_response_id")
            .is_none()
    );
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input.len(), 1);
}

/// A 4xx body that mentions `previous_response` is the signal that
/// the upstream's stored state for our chain has been evicted. We
/// detect this so the in-agent fallback can re-send with a full
/// transcript without bothering the user.
#[test]
fn stale_chain_error_detection() {
    let stale = LlmError::HttpStatus(
        400,
        r#"{"error":{"message":"previous_response_id 'resp_x' not found"}}"#.into(),
    );
    assert!(is_stale_chain_error(&stale));

    let unrelated = LlmError::HttpStatus(400, r#"{"error":{"message":"bad request"}}"#.into());
    assert!(!is_stale_chain_error(&unrelated));

    // 500s flow through the harness's transient-retry path, not the
    // chain-fallback path — even if the body happens to mention the
    // chain.
    let server = LlmError::HttpStatus(503, "previous_response upstream blip".into());
    assert!(!is_stale_chain_error(&server));
}

/// Regression: `prompt_cache_key` must still ride along on chained
/// (`previous_response_id`) turns. Without it the Codex backend would
/// route the chain continuation to a different machine on each turn
/// and squander the warm cache the chain is supposed to preserve.
#[test]
fn build_request_chain_turn_still_emits_prompt_cache_key() {
    let config = ResponsesConfig {
        prompt_cache_key: Some("tau-base".into()),
        ..chain_test_config()
    };
    let messages = vec![
        user_text("first turn"),
        assistant_text("first response"),
        user_text("second turn"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: Some(PreviousResponse {
            id: "resp_abc",
            message_index: 2,
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    assert_eq!(body["previous_response_id"], "resp_abc");
    assert_eq!(body["prompt_cache_key"], "tau-base");
}

/// The Responses backend must split the wire `prompt_cache_key` for
/// extension-originated turns just like the Chat Completions backend
/// does. Both paths share the same `mix_originator_into_cache_key`
/// helper, but this test pins the wiring at the request-build layer
/// so a future refactor can't silently regress it on one path.
#[test]
fn build_request_prompt_cache_key_differs_for_extension_originator() {
    let config = ResponsesConfig {
        prompt_cache_key: Some("tau-base".into()),
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("core-delegate"),
        query_id: "delegate-1".into(),
    };
    let user_request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };
    let ext_request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &ext,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let user_body = serde_json::to_value(build_request(&config, &user_request)).expect("serialize");
    let ext_body = serde_json::to_value(build_request(&config, &ext_request)).expect("serialize");

    assert_eq!(user_body["prompt_cache_key"], "tau-base");
    assert!(ext_body["prompt_cache_key"].is_string());
    assert_ne!(ext_body["prompt_cache_key"], user_body["prompt_cache_key"]);
}

/// `ToolChoice::None` emits `tool_choice: "none"` on the Responses
/// body while leaving the `tools` array fully declared. The harness
/// uses this for non-tool extension side queries (e.g. idle summary)
/// so the model is forced to produce a text reply while the cache
/// prefix (system_prompt + tools) keeps matching the parent
/// conversation's. Verified here on a request that carries real
/// tool definitions.
#[test]
fn build_request_emits_tool_choice_none_while_keeping_tools_declared() {
    let config = chain_test_config();
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        description: None,
        parameters: None,
    };
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::None,
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");

    assert_eq!(body["tool_choice"], "none");
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        1,
        "tools must stay declared so the cache prefix matches"
    );
}

fn chain_test_config() -> ResponsesConfig {
    ResponsesConfig {
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        prompt_cache_key: None,
        prompt_cache_retention: None,
    }
}

fn phase_test_config() -> ResponsesConfig {
    ResponsesConfig {
        supports_phase: true,
        ..chain_test_config()
    }
}

fn user_text(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: ConversationRole::User,
        content: vec![ContentBlock::Text { text: text.into() }],
        phase: None,
    }
}

fn assistant_text(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: ConversationRole::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        phase: None,
    }
}

fn assistant_text_with_phase(text: &str, phase: tau_proto::MessagePhase) -> ConversationMessage {
    ConversationMessage {
        role: ConversationRole::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        phase: Some(phase),
    }
}

/// When `supports_phase` is on, every assistant `message` item must
/// carry a `phase` field. A stored `Commentary` value rides straight
/// through; absence of a stored value falls back to `final_answer`
/// per the OpenAI deployment-checklist guidance for legacy history.
#[test]
fn build_request_stamps_phase_on_assistant_messages_when_supported() {
    let config = phase_test_config();
    let messages = vec![
        user_text("hello"),
        assistant_text_with_phase("draft", tau_proto::MessagePhase::Commentary),
        user_text("more"),
        assistant_text("legacy turn without phase"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let input = body["input"].as_array().expect("input");

    let assistant_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|item| item["role"].as_str() == Some("assistant"))
        .collect();
    assert_eq!(assistant_items.len(), 2, "two assistant messages expected");
    assert_eq!(assistant_items[0]["phase"], "commentary");
    assert_eq!(
        assistant_items[1]["phase"], "final_answer",
        "legacy assistant message must default to final_answer per OpenAI guidance"
    );
}

/// `supports_phase: false` keeps the field off the wire entirely,
/// even when the stored message carries one. This is the safety
/// gate that lets older Codex models (which would reject unknown
/// fields) keep working as the harness sends them history that may
/// have been captured against a newer model.
#[test]
fn build_request_omits_phase_when_unsupported() {
    let config = chain_test_config(); // supports_phase: false
    let messages = vec![assistant_text_with_phase(
        "draft",
        tau_proto::MessagePhase::Commentary,
    )];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let assistant_item = input
        .iter()
        .find(|i| i["role"].as_str() == Some("assistant"))
        .expect("assistant message");
    assert!(
        assistant_item.as_object().unwrap().get("phase").is_none(),
        "phase must not be sent when the backend doesn't advertise support"
    );
}

/// Tool-call boundaries flush a pending text block into its own
/// assistant `message` item. That intermediate flush must carry
/// `phase` too — otherwise a mixed text+tool_use turn would
/// half-pass-through with a phase on the trailing flush only.
#[test]
fn build_request_stamps_phase_on_pre_tool_call_text_flush() {
    let config = phase_test_config();
    let messages = vec![ConversationMessage {
        role: ConversationRole::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "thinking out loud".into(),
            },
            ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "shell".into(),
                input: tau_proto::CborValue::Null,
            },
            ContentBlock::Text {
                text: "trailing".into(),
            },
        ],
        phase: Some(tau_proto::MessagePhase::Commentary),
    }];
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &messages,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };
    let body = serde_json::to_value(build_request(&config, &request)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let assistant_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|item| item["role"].as_str() == Some("assistant"))
        .collect();
    assert_eq!(
        assistant_items.len(),
        2,
        "pre-tool-call text and trailing text each become their own assistant message"
    );
    for (i, item) in assistant_items.iter().enumerate() {
        assert_eq!(
            item["phase"], "commentary",
            "assistant message #{i} must carry the captured phase"
        );
    }
}

/// `parse_phase_from_item` is the bridge from the Responses-API
/// `output_item.*` shape into our typed `MessagePhase`. The two
/// known wire strings round-trip; unknown values (forward
/// compatibility) come back as `None` rather than panicking.
#[test]
fn parse_phase_from_item_recognizes_wire_strings() {
    let commentary = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "commentary",
    });
    assert_eq!(
        parse_phase_from_item(&commentary),
        Some(tau_proto::MessagePhase::Commentary)
    );

    let final_ans = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "final_answer",
    });
    assert_eq!(
        parse_phase_from_item(&final_ans),
        Some(tau_proto::MessagePhase::FinalAnswer)
    );

    let unknown_future = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "rumination",
    });
    assert_eq!(parse_phase_from_item(&unknown_future), None);

    let no_phase = serde_json::json!({
        "type": "message",
        "role": "assistant",
    });
    assert_eq!(parse_phase_from_item(&no_phase), None);

    let function_call = serde_json::json!({
        "type": "function_call",
        "phase": "commentary",
    });
    assert_eq!(
        parse_phase_from_item(&function_call),
        None,
        "non-message items must not have their `phase` field harvested"
    );
}

// -----------------------------------------------------------------------
// WebSocket envelope wrapping
// -----------------------------------------------------------------------

/// The WS guide requires every client frame to carry `type:
/// "response.create"` at the top level. The HTTP body does not.
/// [`build_ws_envelope`] is the only place we add the tag — pin it
/// here so a future refactor that drops the wrapper struct can't
/// silently regress it.
#[test]
fn ws_envelope_adds_type_and_drops_stream() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        messages: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        previous_response: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
    };

    let http_body = serde_json::to_value(build_request(&config, &request)).expect("http body");
    let ws_body = serde_json::to_value(build_ws_envelope(&config, &request)).expect("ws envelope");

    assert_eq!(ws_body["type"], "response.create");
    assert!(
        ws_body.as_object().unwrap().get("stream").is_none(),
        "WS frame must omit `stream` — the WS guide says it's not used and the field is transport-implicit"
    );
    // Every other body shape stays identical so the request-build
    // tests already in this file double as WS-envelope coverage.
    assert_eq!(ws_body["model"], http_body["model"]);
    assert_eq!(ws_body["store"], http_body["store"]);
    assert_eq!(ws_body["input"], http_body["input"]);
}

// -----------------------------------------------------------------------
// apply_event — shared event applicator for SSE + WS
// -----------------------------------------------------------------------

/// `response.output_text.delta` accumulates into `state.text` and
/// fires `on_update` once per delta. Mirrors the original SSE-only
/// behavior — keeps the WS path equivalent.
#[test]
fn apply_event_text_delta_accumulates_and_notifies() {
    let mut state = crate::common::StreamState::new();
    let mut updates: Vec<String> = Vec::new();
    let mut on_update = |text: &str, _thinking: Option<&str>| {
        updates.push(text.to_owned());
    };

    for chunk in ["hel", "lo, ", "world"] {
        let ev = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": chunk,
        });
        let done = apply_event(&mut state, &ev, &mut on_update).expect("apply ok");
        assert!(!done, "text delta should not terminate the stream");
    }
    assert_eq!(state.text, "hello, world");
    assert_eq!(updates, vec!["hel", "hello, ", "hello, world"]);
}

#[test]
fn apply_event_completed_terminates_and_captures_response_id() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};
    let ev = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "resp_xyz",
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7,
                "input_tokens_details": { "cached_tokens": 5 },
            },
        },
    });
    let done = apply_event(&mut state, &ev, &mut on_update).expect("apply ok");
    assert!(done, "response.completed must terminate the stream");
    assert_eq!(state.response_id.as_deref(), Some("resp_xyz"));
    assert_eq!(state.input_tokens, Some(42));
    assert_eq!(state.output_tokens, Some(7));
    assert_eq!(state.cached_tokens, Some(5));
}

#[test]
fn apply_event_function_call_assembles_tool_call() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call_a",
                "name": "shell",
            },
        }),
        &mut on_update,
    )
    .expect("ok");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"cmd\":\"ls\"}",
        }),
        &mut on_update,
    )
    .expect("ok");

    assert_eq!(state.tool_calls.len(), 1);
    assert_eq!(state.tool_calls[0].id, "call_a");
    assert_eq!(state.tool_calls[0].name, "shell");
    assert_eq!(state.tool_calls[0].arguments_json, "{\"cmd\":\"ls\"}");
}

#[test]
fn apply_event_failed_returns_error() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &str, _: Option<&str>| {};
    let ev = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": { "message": "model overloaded" },
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("response failed"));
            assert!(body.contains("model overloaded"));
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}
