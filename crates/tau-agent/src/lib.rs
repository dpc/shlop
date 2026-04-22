//! First-party agent process.
//!
//! Receives `SessionPromptCreated` from the harness and emits
//! `AgentResponseUpdated` / `AgentResponseFinished` events.

pub(crate) mod openai;
mod responses;

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_config::settings::{self, ModelRegistry};
use tau_proto::{
    AgentPromptSubmitted, AgentResponseFinished, AgentResponseUpdated, ClientKind, Event,
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
        client_name: "tau-agent".to_owned(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SessionPromptCreated),
            EventSelector::Exact(EventName::LifecycleDisconnect),
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
        match event {
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
                            },
                        ))?;
                        writer.flush()?;
                    }
                }
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
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
            Some(BackendConfig::ChatCompletions(openai::OpenAiConfig {
                base_url,
                api_key: access_token,
                model_id: model_id.to_owned(),
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
            Some(BackendConfig::ChatCompletions(openai::OpenAiConfig {
                base_url,
                api_key,
                model_id: model_id.to_owned(),
            }))
        }
    }
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
    };

    match openai::chat_completion_stream(config, &request, |text_so_far| {
        let _ = writer.write_event(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: session_prompt_id.into(),
            text: text_so_far.to_owned(),
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
    };

    match responses::responses_stream(config, &request, |text_so_far| {
        let _ = writer.write_event(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: session_prompt_id.into(),
            text: text_so_far.to_owned(),
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
    let text = if state.text.is_empty() {
        None
    } else {
        Some(state.text.clone())
    };
    writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        text,
        tool_calls: state.into_tool_calls(),
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
        client_name: "tau-agent-echo".to_owned(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SessionPromptCreated),
            EventSelector::Exact(EventName::LifecycleDisconnect),
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
        match event {
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
                            id: call_id,
                            name: "read".to_owned(),
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("path".to_owned()),
                                CborValue::Text(path.trim().to_owned()),
                            )]),
                        }
                    } else if let Some(cmd) = user_text.strip_prefix("shell ") {
                        AgentToolCall {
                            id: call_id,
                            name: "shell".to_owned(),
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("command".to_owned()),
                                CborValue::Text(cmd.trim().to_owned()),
                            )]),
                        }
                    } else {
                        AgentToolCall {
                            id: call_id,
                            name: "echo".to_owned(),
                            arguments: CborValue::Text(user_text),
                        }
                    };

                    writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                        session_prompt_id: spid,
                        text: None,
                        tool_calls: vec![tool_call],
                    }))?;
                }
                writer.flush()?;
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
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
}
