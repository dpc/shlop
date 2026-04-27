//! First-party agent process.
//!
//! Receives `SessionPromptCreated` from the harness and emits
//! `AgentResponseUpdated` / `AgentResponseFinished` events.

pub(crate) mod openai;
mod responses;

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_config::settings::{self, ModelRegistry, ProviderConfig};
use tau_proto::{
    Ack, AgentPromptSubmitted, AgentResponseFinished, AgentResponseUpdated, ClientKind, Event,
    EventName, EventReader, EventSelector, EventWriter, LifecycleHello, LifecycleReady,
    LifecycleSubscribe, PROTOCOL_VERSION,
};

/// Runs the agent on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the agent over arbitrary reader/writer streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));

    let model_registry = settings::load_models().unwrap_or_default();
    let auth_store = tau_provider::storage::load().unwrap_or_default();

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent".into(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SESSION_PROMPT_CREATED),
            EventSelector::Exact(EventName::LIFECYCLE_DISCONNECT),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("agent ready".to_owned()),
    }))?;
    writer.flush()?;

    loop {
        let Some(event) = reader.read_event()? else {
            return Ok(());
        };
        // Peel the LogEvent envelope. The agent processes one prompt at
        // a time (serial), so acks are trivially in order: ack right
        // after handling whatever is inside.
        let (log_id, inner) = event.peel_log();
        match inner {
            Event::SessionPromptCreated(prompt) => {
                let session_prompt_id = prompt.session_prompt_id.clone();

                // Announce we accepted the prompt.
                writer.write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
                    session_prompt_id: session_prompt_id.clone(),
                }))?;
                writer.flush()?;

                // Resolve backend from the model specified in the prompt.
                let backend = prompt
                    .model
                    .as_deref()
                    .and_then(|m| resolve_backend(m, &model_registry, &auth_store));

                match backend {
                    Some(BackendConfig::ChatCompletions(cfg)) => {
                        handle_chat_completions(&session_prompt_id, &cfg, &prompt, &mut writer)?;
                    }
                    Some(BackendConfig::Responses(cfg)) => {
                        handle_responses(&session_prompt_id, &cfg, &prompt, &mut writer)?;
                    }
                    None => {
                        let msg = match &prompt.model {
                            Some(m) => format!("cannot resolve model config for: {m}"),
                            None => "no model specified".to_owned(),
                        };
                        writer.write_event(&Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id,
                                text: Some(msg),
                                tool_calls: Vec::new(),
                                input_tokens: None,
                                cached_tokens: None,
                                thinking: None,
                            },
                        ))?;
                        writer.flush()?;
                    }
                }
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
        }
        if let Some(id) = log_id {
            writer.write_event(&Event::Ack(Ack { up_to: id }))?;
            writer.flush()?;
        }
    }
}

// ---------------------------------------------------------------------------
// Backend config resolution
// ---------------------------------------------------------------------------

enum BackendConfig {
    ChatCompletions(openai::OpenAiConfig),
    Responses(responses::ResponsesConfig),
}

/// Resolve a `"provider/model_id"` string into a backend config.
fn resolve_backend(
    model: &str,
    models: &ModelRegistry,
    auth_store: &tau_provider::storage::AuthStore,
) -> Option<BackendConfig> {
    let (provider_name, model_id) = model.split_once('/')?;
    let provider = models.providers.get(provider_name)?;
    let auth_type = provider
        .auth
        .as_deref()
        .unwrap_or(if provider.api_key.is_some() {
            "api-key"
        } else {
            "none"
        });

    match auth_type {
        "openai-codex" => {
            // Codex subscription → Responses API via chatgpt.com.
            use tau_provider::storage::Credentials;
            let creds = auth_store.providers.get(provider_name)?;
            let (access_token, account_id) = match creds {
                Credentials::Oauth {
                    access_token,
                    account_id,
                    ..
                } => (access_token.clone(), account_id.clone()),
                _ => return None,
            };
            Some(BackendConfig::Responses(responses::ResponsesConfig {
                base_url: "https://chatgpt.com/backend-api".to_owned(),
                api_key: access_token,
                model_id: model_id.to_owned(),
                account_id,
                supports_reasoning_effort: provider.compat.supports_reasoning_effort,
                supports_reasoning_summary: supports_reasoning_summary(
                    provider,
                    "https://chatgpt.com/backend-api",
                ),
                prompt_cache_key: prompt_cache_key(
                    provider,
                    "https://chatgpt.com/backend-api",
                    model_id,
                ),
                prompt_cache_retention: prompt_cache_retention(
                    provider,
                    "https://chatgpt.com/backend-api",
                ),
            }))
        }
        "github-copilot" => {
            // Copilot → Chat Completions with token from auth.json.
            use tau_provider::storage::Credentials;
            let creds = auth_store.providers.get(provider_name)?;
            let access_token = match creds {
                Credentials::Oauth { access_token, .. } => access_token.clone(),
                _ => return None,
            };
            let base_url = extract_copilot_base_url(&access_token)
                .unwrap_or_else(|| "https://api.individual.githubcopilot.com".to_owned());
            let prompt_cache_key = prompt_cache_key(provider, &base_url, model_id);
            let prompt_cache_retention = prompt_cache_retention(provider, &base_url);
            Some(BackendConfig::ChatCompletions(openai::OpenAiConfig {
                base_url,
                api_key: access_token,
                model_id: model_id.to_owned(),
                supports_reasoning_effort: provider.compat.supports_reasoning_effort,
                prompt_cache_key,
                prompt_cache_retention,
            }))
        }
        "api-key" | "none" | _ => {
            // Standard Chat Completions API.
            let base_url = provider.base_url.clone().or_else(|| {
                // Check auth.json for API key providers without base_url.
                use tau_provider::storage::Credentials;
                match auth_store.providers.get(provider_name)? {
                    Credentials::ApiKey { .. } => Some("https://api.openai.com/v1".to_owned()),
                    _ => None,
                }
            })?;
            let api_key = provider.api_key.clone().unwrap_or_default();
            let prompt_cache_key = prompt_cache_key(provider, &base_url, model_id);
            let prompt_cache_retention = prompt_cache_retention(provider, &base_url);
            Some(BackendConfig::ChatCompletions(openai::OpenAiConfig {
                base_url,
                api_key,
                model_id: model_id.to_owned(),
                supports_reasoning_effort: provider.compat.supports_reasoning_effort,
                prompt_cache_key,
                prompt_cache_retention,
            }))
        }
    }
}

fn prompt_cache_key(
    provider: &ProviderConfig,
    base_url: &str,
    model_id: &str,
) -> Option<String> {
    if !supports_prompt_cache_key(provider, base_url) {
        return None;
    }

    let cwd = std::env::current_dir().ok()?;
    Some(openai::prompt_cache_key(base_url, model_id, &cwd))
}

fn prompt_cache_retention(
    provider: &ProviderConfig,
    base_url: &str,
) -> Option<settings::PromptCacheRetention> {
    if supports_prompt_cache_retention(provider, base_url) {
        provider.prompt_cache_retention
    } else {
        None
    }
}

fn supports_prompt_cache_key(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_prompt_cache_key || is_builtin_openai_prompt_cache_api(base_url)
}

/// Whether to send `reasoning.summary` to this provider on the
/// Responses path. Auto-enabled on the public OpenAI API and the
/// Codex backend; otherwise gated behind `supportsReasoningSummary`.
fn supports_reasoning_summary(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_reasoning_summary || is_builtin_openai_prompt_cache_api(base_url)
}

fn supports_prompt_cache_retention(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_prompt_cache_retention || is_builtin_openai_prompt_cache_api(base_url)
}

fn is_builtin_openai_prompt_cache_api(base_url: &str) -> bool {
    matches!(
        base_url.trim_end_matches('/'),
        "https://api.openai.com/v1" | "https://chatgpt.com/backend-api"
    )
}

/// Parse `proxy-ep` from a Copilot token string.
fn extract_copilot_base_url(token: &str) -> Option<String> {
    for part in token.split(';') {
        if let Some(ep) = part.strip_prefix("proxy-ep=") {
            return Some(format!("https://{ep}"));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// LLM backends
// ---------------------------------------------------------------------------

fn handle_chat_completions<W: Write>(
    session_prompt_id: &str,
    config: &openai::OpenAiConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut EventWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let request = openai::PromptPayload {
        system_prompt: &prompt.system_prompt,
        messages: &prompt.messages,
        tools: &prompt.tools,
        effort: prompt.effort,
        thinking_summary: prompt.thinking_summary,
    };

    match openai::chat_completion_stream(config, &request, |text_so_far, thinking_so_far| {
        let _ = writer.write_event(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: session_prompt_id.into(),
            text: text_so_far.to_owned(),
            thinking: thinking_so_far.map(str::to_owned),
        }));
        let _ = writer.flush();
    }) {
        Ok(state) => finish_stream(session_prompt_id, state, writer)?,
        Err(error) => finish_error(session_prompt_id, error, writer)?,
    }
    Ok(())
}

fn handle_responses<W: Write>(
    session_prompt_id: &str,
    config: &responses::ResponsesConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut EventWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let request = openai::PromptPayload {
        system_prompt: &prompt.system_prompt,
        messages: &prompt.messages,
        tools: &prompt.tools,
        effort: prompt.effort,
        thinking_summary: prompt.thinking_summary,
    };

    match responses::responses_stream(config, &request, |text_so_far, thinking_so_far| {
        let _ = writer.write_event(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: session_prompt_id.into(),
            text: text_so_far.to_owned(),
            thinking: thinking_so_far.map(str::to_owned),
        }));
        let _ = writer.flush();
    }) {
        Ok(state) => finish_stream(session_prompt_id, state, writer)?,
        Err(error) => finish_error(session_prompt_id, error, writer)?,
    }
    Ok(())
}

fn finish_stream<W: Write>(
    session_prompt_id: &str,
    state: openai::StreamState,
    writer: &mut EventWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let text_empty = state.text.is_empty();
    let text_content = state.text.clone();
    let input_tokens = state.input_tokens;
    let cached_tokens = state.cached_tokens;
    let thinking = state.thinking.clone();
    let tool_calls = state.into_tool_calls();
    let text = if text_empty {
        if tool_calls.is_empty() {
            Some("(agent returned an empty response)".to_owned())
        } else {
            None
        }
    } else {
        Some(text_content)
    };
    writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        text,
        tool_calls,
        input_tokens,
        cached_tokens,
        thinking,
    }))?;
    writer.flush()?;
    Ok(())
}

fn finish_error<W: Write>(
    session_prompt_id: &str,
    error: openai::OpenAiError,
    writer: &mut EventWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        text: Some(format!("LLM error: {error}")),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
    }))?;
    writer.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Echo agent (for tests)
// ---------------------------------------------------------------------------

/// A simple echo agent for integration tests. Echoes user text back
/// as a `echo` tool call, or returns tool results as text.
pub fn run_echo<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    use tau_proto::{AgentToolCall, CborValue, ContentBlock, ConversationRole};

    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent-echo".into(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SESSION_PROMPT_CREATED),
            EventSelector::Exact(EventName::LIFECYCLE_DISCONNECT),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("echo agent ready".to_owned()),
    }))?;
    writer.flush()?;

    let mut next_call = 1_u64;

    loop {
        let Some(event) = reader.read_event()? else {
            return Ok(());
        };
        let (log_id, inner) = event.peel_log();
        match inner {
            Event::SessionPromptCreated(prompt) => {
                let spid = prompt.session_prompt_id.clone();
                writer.write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
                    session_prompt_id: spid.clone(),
                }))?;

                // If last message is a tool result, return it as text.
                let is_tool_result = prompt.messages.last().is_some_and(|m| {
                    m.role == ConversationRole::User
                        && m.content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                });
                if is_tool_result {
                    let text = prompt
                        .messages
                        .last()
                        .and_then(|m| {
                            m.content.iter().find_map(|b| match b {
                                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                                _ => None,
                            })
                        })
                        .unwrap_or_default();
                    writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                        session_prompt_id: spid,
                        text: Some(text),
                        tool_calls: Vec::new(),
                        input_tokens: None,
                        cached_tokens: None,
                        thinking: None,
                    }))?;
                } else {
                    // Find user text and make a tool call.
                    let user_text = prompt
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == ConversationRole::User)
                        .and_then(|m| {
                            m.content.iter().find_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                        })
                        .unwrap_or_default();

                    let call_id = format!("call-{next_call}");
                    next_call += 1;

                    let tool_call = if let Some(path) = user_text.strip_prefix("read ") {
                        AgentToolCall {
                            id: call_id.into(),
                            name: "read".into(),
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("path".to_owned()),
                                CborValue::Text(path.trim().to_owned()),
                            )]),
                        }
                    } else if let Some(cmd) = user_text.strip_prefix("shell ") {
                        AgentToolCall {
                            id: call_id.into(),
                            name: "shell".into(),
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("command".to_owned()),
                                CborValue::Text(cmd.trim().to_owned()),
                            )]),
                        }
                    } else {
                        AgentToolCall {
                            id: call_id.into(),
                            name: "echo".into(),
                            arguments: CborValue::Text(user_text),
                        }
                    };

                    writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                        session_prompt_id: spid,
                        text: None,
                        tool_calls: vec![tool_call],
                        input_tokens: None,
                        cached_tokens: None,
                        thinking: None,
                    }))?;
                }
                writer.flush()?;
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
        }
        if let Some(id) = log_id {
            writer.write_event(&Event::Ack(Ack { up_to: id }))?;
            writer.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_config_resolves_none() {
        let models = ModelRegistry::default();
        let auth = tau_provider::storage::AuthStore::default();
        assert!(resolve_backend("fake/model", &models, &auth).is_none());
    }

    #[test]
    fn public_openai_api_enables_prompt_cache_support() {
        let provider = ProviderConfig::default();

        assert!(supports_prompt_cache_key(
            &provider,
            "https://api.openai.com/v1"
        ));
        assert!(supports_prompt_cache_retention(
            &provider,
            "https://api.openai.com/v1/"
        ));
    }

    #[test]
    fn codex_backend_enables_prompt_cache_support() {
        let provider = ProviderConfig::default();

        assert!(supports_prompt_cache_key(
            &provider,
            "https://chatgpt.com/backend-api"
        ));
        assert!(supports_prompt_cache_retention(
            &provider,
            "https://chatgpt.com/backend-api/"
        ));
    }

    #[test]
    fn provider_flags_enable_prompt_cache_support_for_non_openai_backends() {
        let provider = ProviderConfig {
            compat: settings::ProviderCompat {
                supports_prompt_cache_key: true,
                supports_prompt_cache_retention: true,
                ..settings::ProviderCompat::default()
            },
            ..ProviderConfig::default()
        };

        assert!(supports_prompt_cache_key(
            &provider,
            "https://example.com/v1"
        ));
        assert!(supports_prompt_cache_retention(
            &provider,
            "https://example.com/v1"
        ));
    }
}
