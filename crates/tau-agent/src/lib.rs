//! First-party agent process.
//!
//! Receives `SessionPromptCreated` from the harness and emits
//! `AgentResponseUpdated` / `AgentResponseFinished` events.

pub(crate) mod openai;
mod responses;

use std::collections::VecDeque;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use backon::BackoffBuilder;
use tau_config::settings::{self, ModelRegistry, ProviderConfig};
use tau_proto::{
    Ack, AgentPromptSubmitted, AgentResponseFinished, AgentResponseUpdated, ClientKind, Event,
    EventName, EventSelector, Frame, FrameReader, FrameWriter, Hello, Message, PROTOCOL_VERSION,
    Ready, Subscribe,
};

/// `tracing` target for events emitted from the agent. Matches the
/// convention described in [`tau_extension`]: a short identifier the
/// user can name in `TAU_LOG=agent=trace` to dump every prompt
/// the harness hands the agent.
pub const LOG_TARGET: &str = "agent";

/// Runs the agent on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging();
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the agent over arbitrary reader/writer streams.
///
/// The reader is moved to a background thread so the main loop can
/// `recv_timeout` on it during retry-backoff sleeps and wake early
/// when the harness disconnects (or queues another event).
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    let model_registry = settings::load_models().unwrap_or_default();

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent".into(),
        client_kind: ClientKind::Agent,
    })))?;
    writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SESSION_PROMPT_CREATED),
            EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
        ],
    })))?;
    writer.write_frame(&Frame::Message(Message::Ready(Ready {
        message: Some("agent ready".to_owned()),
    })))?;
    writer.flush()?;

    // Pump frames from the reader into a channel. The main loop consumes
    // from `frame_rx`; backoff sleeps use `recv_timeout` on the same
    // receiver so a `Disconnect` message (or sender drop on EOF) wakes
    // us out of a wait we'd otherwise be deaf to.
    let (frame_tx, frame_rx) = mpsc::channel::<Frame>();
    thread::spawn(move || {
        let mut reader = FrameReader::new(BufReader::new(reader));
        loop {
            match reader.read_frame() {
                Ok(Some(frame)) => {
                    if frame_tx.send(frame).is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => return,
            }
        }
    });

    let mut deferred: VecDeque<Frame> = VecDeque::new();

    loop {
        let frame = match deferred.pop_front() {
            Some(e) => e,
            None => match frame_rx.recv() {
                Ok(e) => e,
                Err(_) => return Ok(()),
            },
        };
        // Peel the LogEvent envelope. The agent processes one prompt at
        // a time (serial), so acks are trivially in order: ack right
        // after handling whatever is inside.
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::SessionPromptCreated(prompt)) => {
                let session_prompt_id = prompt.session_prompt_id.clone();

                // Full prompt dump for debugging. Off by default;
                // enable with `TAU_LOG=agent=trace`. Pretty JSON
                // is the most readable form for a multi-screen
                // payload, and the prompt is already fully
                // serializable.
                if tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
                    match serde_json::to_string_pretty(&prompt) {
                        Ok(json) => tracing::trace!(
                            target: LOG_TARGET,
                            session_prompt_id = %session_prompt_id,
                            "agent prompt:\n{json}"
                        ),
                        Err(error) => tracing::trace!(
                            target: LOG_TARGET,
                            session_prompt_id = %session_prompt_id,
                            "agent prompt (failed to serialize for log: {error})"
                        ),
                    }
                }

                // Announce we accepted the prompt.
                writer.write_frame(&Frame::Event(Event::AgentPromptSubmitted(
                    AgentPromptSubmitted {
                        session_prompt_id: session_prompt_id.clone(),
                        originator: prompt.originator.clone(),
                    },
                )))?;
                writer.flush()?;

                let mut retry_ctx = RetryContext {
                    frame_rx: &frame_rx,
                    deferred: &mut deferred,
                };

                // Resolve backend from the model specified in the prompt.
                // Reload auth on every prompt so `tau provider login` or
                // `/provider-auth` takes effect without restarting Tau.
                let mut auth_store = tau_provider::storage::load().unwrap_or_default();
                let backend = prompt
                    .model
                    .as_deref()
                    .and_then(|m| resolve_backend(m, &model_registry, &mut auth_store));

                match backend {
                    Some(BackendConfig::ChatCompletions(cfg)) => {
                        handle_chat_completions(
                            &session_prompt_id,
                            &cfg,
                            &prompt,
                            &mut writer,
                            &mut retry_ctx,
                        )?;
                    }
                    Some(BackendConfig::Responses(cfg)) => {
                        handle_responses(
                            &session_prompt_id,
                            &cfg,
                            &prompt,
                            &mut writer,
                            &mut retry_ctx,
                        )?;
                    }
                    None => {
                        let msg = match &prompt.model {
                            Some(m) => format!("cannot resolve model config for: {m}"),
                            None => "no model specified".to_owned(),
                        };
                        writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id,
                                text: Some(msg),
                                tool_calls: Vec::new(),
                                input_tokens: None,
                                cached_tokens: None,
                                thinking: None,
                                originator: prompt.originator.clone(),
                            },
                        )))?;
                        writer.flush()?;
                    }
                }
            }
            Frame::Message(Message::Disconnect(_)) => return Ok(()),
            _ => {}
        }
        if let Some(id) = log_id {
            writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
            writer.flush()?;
        }
    }
}

/// What the retry loop needs from the agent's main event pump:
/// access to the channel of incoming events (so a long backoff sleep
/// can wake on disconnect / queued prompts) and a deferred buffer
/// for events that arrive mid-sleep but belong to a later main-loop
/// iteration.
struct RetryContext<'a> {
    frame_rx: &'a Receiver<Frame>,
    deferred: &'a mut VecDeque<Frame>,
}

/// Outcome of an interruptible sleep.
enum SleepOutcome {
    /// Slept the full duration.
    Elapsed,
    /// Woken by a `LifecycleDisconnect` (or reader EOF). The caller
    /// should stop retrying and finalize the prompt.
    Aborted,
}

impl<'a> RetryContext<'a> {
    /// Sleep for up to `delay`, but wake early if the harness sends a
    /// `LifecycleDisconnect` (or the reader thread exits). Any other
    /// events that arrive mid-sleep are stashed onto the deferred
    /// buffer so the main loop processes them after the current
    /// prompt finishes.
    fn sleep_or_abort(&mut self, delay: Duration) -> SleepOutcome {
        let deadline = Instant::now() + delay;
        loop {
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return SleepOutcome::Elapsed;
            };
            match self.frame_rx.recv_timeout(remaining) {
                Err(RecvTimeoutError::Timeout) => return SleepOutcome::Elapsed,
                Err(RecvTimeoutError::Disconnected) => return SleepOutcome::Aborted,
                Ok(frame) => {
                    let abort = matches!(
                        frame,
                        Frame::Message(Message::Disconnect(_))
                            | Frame::Event(Event::UiCancelPrompt(_))
                    );
                    self.deferred.push_back(frame);
                    if abort {
                        return SleepOutcome::Aborted;
                    }
                    // Otherwise keep waiting out the rest of the backoff.
                }
            }
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
    auth_store: &mut tau_provider::storage::AuthStore,
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
            use tau_provider::storage::{Credentials, ProviderKind};
            let (access_token, account_id) = match auth_store.providers.get(provider_name)? {
                Credentials::Oauth {
                    access_token,
                    refresh_token,
                    expires_at_ms,
                    account_id,
                    ..
                } => {
                    let mut access_token = access_token.clone();
                    let mut account_id = account_id.clone();
                    if oauth_token_should_refresh(&access_token, *expires_at_ms) {
                        if let Ok(tokens) = tau_provider::oauth::openai_codex_refresh(refresh_token)
                        {
                            access_token = tokens.access_token.clone();
                            account_id = tokens.account_id.clone();
                            auth_store.providers.insert(
                                provider_name.to_owned(),
                                Credentials::Oauth {
                                    provider_kind: ProviderKind::OpenaiCodex,
                                    access_token: tokens.access_token,
                                    refresh_token: tokens.refresh_token,
                                    expires_at_ms: tokens.expires_at_ms,
                                    account_id: tokens.account_id,
                                },
                            );
                            let _ = tau_provider::storage::save(auth_store);
                        }
                    }
                    (access_token, account_id)
                }
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
                supports_llama_cpp_cache: provider.compat.supports_llama_cpp_cache,
            }))
        }
        "api-key" | "none" => {
            chat_completions_backend(provider_name, provider, auth_store, model_id)
        }
        _ => chat_completions_backend(provider_name, provider, auth_store, model_id),
    }
}

fn oauth_token_should_refresh(access_token: &str, expires_at_ms: u64) -> bool {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64;

    if let Some(issued_at_ms) = jwt_issued_at_ms(access_token) {
        let lifetime_ms = expires_at_ms.saturating_sub(issued_at_ms);
        let refresh_at_ms = issued_at_ms.saturating_add(lifetime_ms / 2);
        if refresh_at_ms <= now_ms {
            return true;
        }
    }

    // Fallback for tokens without an `iat` claim: refresh a little early to avoid
    // expiry during a long streaming response.
    expires_at_ms <= now_ms + Duration::from_secs(5 * 60).as_millis() as u64
}

fn jwt_issued_at_ms(jwt: &str) -> Option<u64> {
    let payload = jwt.split('.').nth(1)?;
    let payload = tau_provider::oauth::base64_url_safe_no_pad_decode(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims.get("iat")?.as_u64().map(|secs| secs * 1000)
}

fn chat_completions_backend(
    provider_name: &str,
    provider: &ProviderConfig,
    auth_store: &mut tau_provider::storage::AuthStore,
    model_id: &str,
) -> Option<BackendConfig> {
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
        supports_llama_cpp_cache: provider.compat.supports_llama_cpp_cache,
    }))
}

fn prompt_cache_key(provider: &ProviderConfig, base_url: &str, model_id: &str) -> Option<String> {
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

/// Maximum number of retry attempts before giving up on a transient
/// LLM error. Combined with [`llm_retry_schedule`]'s fibonacci shape
/// (min 10s), this caps total wait time at roughly 9 minutes.
const LLM_MAX_RETRIES: usize = 8;

/// Build a fibonacci backoff schedule for retrying transient LLM
/// errors. Roughly: 10s, 10s, 20s, 30s, 50s, 80s, 130s, 210s — eight
/// retries, ~9 minutes total wait before we give up. Jittered to
/// avoid lockstep retries from many agents hitting a recovering
/// upstream simultaneously.
fn llm_retry_schedule() -> backon::FibonacciBackoff {
    backon::FibonacciBuilder::default()
        .with_min_delay(Duration::from_secs(10))
        .with_max_times(LLM_MAX_RETRIES)
        .with_jitter()
        .build()
}

/// Retry an LLM streaming call with fibonacci backoff for transient
/// errors.
///
/// `call` is re-invoked from scratch each attempt — if a previous
/// attempt streamed partial text via its `on_update`, the next
/// attempt's updates will overwrite it on the way through. Between
/// attempts we paint a transient retry banner over the response
/// block (via `AgentResponseUpdated.text`) so the user sees what's
/// happening; the banner is replaced by real content as soon as the
/// next attempt's first delta arrives.
///
/// The backoff sleep is interruptible: a `LifecycleDisconnect`
/// arriving mid-sleep aborts the retry and the call returns the
/// last error.
fn with_llm_retry<F, W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    writer: &mut FrameWriter<BufWriter<W>>,
    retry_ctx: &mut RetryContext<'_>,
    mut call: F,
) -> Result<openai::StreamState, openai::OpenAiError>
where
    F: FnMut(&mut FrameWriter<BufWriter<W>>) -> Result<openai::StreamState, openai::OpenAiError>,
{
    let mut backoff = llm_retry_schedule();
    let max_attempts = LLM_MAX_RETRIES;
    let mut attempt = 0_usize;
    loop {
        let error = match call(writer) {
            Ok(state) => return Ok(state),
            Err(e) => e,
        };
        let Some(retry_after) = error.retry_after() else {
            return Err(error);
        };
        let Some(backoff_delay) = backoff.next() else {
            return Err(error);
        };
        let delay = retry_after.max(backoff_delay);
        attempt += 1;
        tracing::warn!(
            target: LOG_TARGET,
            session_prompt_id = %session_prompt_id,
            "LLM error, retrying in {delay:?} (attempt {attempt}/{max_attempts}): {error}",
        );
        emit_retry_banner(
            session_prompt_id,
            originator,
            writer,
            &error,
            delay,
            attempt,
            max_attempts,
        );
        if matches!(retry_ctx.sleep_or_abort(delay), SleepOutcome::Aborted) {
            tracing::info!(
                target: LOG_TARGET,
                session_prompt_id = %session_prompt_id,
                "retry aborted by disconnect/cancel",
            );
            return Err(error);
        }
    }
}

/// Paint a transient banner into the assistant response block so the
/// user can see we're waiting on an upstream retry. Best-effort:
/// write/flush failures are dropped, matching how the streaming
/// `on_update` handles them.
fn emit_retry_banner<W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    writer: &mut FrameWriter<BufWriter<W>>,
    error: &openai::OpenAiError,
    delay: Duration,
    attempt: usize,
    max_attempts: usize,
) {
    let banner = format!(
        "⏳ provider error — retrying in {}s (attempt {}/{})\n\n> {}",
        delay.as_secs(),
        attempt,
        max_attempts,
        error,
    );
    let _ = writer.write_frame(&Frame::Event(Event::AgentResponseUpdated(
        AgentResponseUpdated {
            session_prompt_id: session_prompt_id.into(),
            text: banner,
            thinking: None,
            originator: originator.clone(),
        },
    )));
    let _ = writer.flush();
}

fn handle_chat_completions<W: Write>(
    session_prompt_id: &str,
    config: &openai::OpenAiConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<BufWriter<W>>,
    retry_ctx: &mut RetryContext<'_>,
) -> Result<(), Box<dyn Error>> {
    let request = openai::PromptPayload {
        system_prompt: &prompt.system_prompt,
        messages: &prompt.messages,
        tools: &prompt.tools,
        effort: prompt.effort,
        thinking_summary: prompt.thinking_summary,
    };

    let originator = prompt.originator.clone();
    let result = with_llm_retry(
        session_prompt_id,
        &originator,
        writer,
        retry_ctx,
        |writer| {
            openai::chat_completion_stream(config, &request, |text_so_far, thinking_so_far| {
                let _ = writer.write_frame(&Frame::Event(Event::AgentResponseUpdated(
                    AgentResponseUpdated {
                        session_prompt_id: session_prompt_id.into(),
                        text: text_so_far.to_owned(),
                        thinking: thinking_so_far.map(str::to_owned),
                        originator: originator.clone(),
                    },
                )));
                let _ = writer.flush();
            })
        },
    );
    match result {
        Ok(state) => finish_stream(session_prompt_id, &prompt.originator, state, writer)?,
        Err(error) => finish_error(session_prompt_id, &prompt.originator, error, writer)?,
    }
    Ok(())
}

fn handle_responses<W: Write>(
    session_prompt_id: &str,
    config: &responses::ResponsesConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<BufWriter<W>>,
    retry_ctx: &mut RetryContext<'_>,
) -> Result<(), Box<dyn Error>> {
    let request = openai::PromptPayload {
        system_prompt: &prompt.system_prompt,
        messages: &prompt.messages,
        tools: &prompt.tools,
        effort: prompt.effort,
        thinking_summary: prompt.thinking_summary,
    };

    let originator = prompt.originator.clone();
    let result = with_llm_retry(
        session_prompt_id,
        &originator,
        writer,
        retry_ctx,
        |writer| {
            responses::responses_stream(config, &request, |text_so_far, thinking_so_far| {
                let _ = writer.write_frame(&Frame::Event(Event::AgentResponseUpdated(
                    AgentResponseUpdated {
                        session_prompt_id: session_prompt_id.into(),
                        text: text_so_far.to_owned(),
                        thinking: thinking_so_far.map(str::to_owned),
                        originator: originator.clone(),
                    },
                )));
                let _ = writer.flush();
            })
        },
    );
    match result {
        Ok(state) => finish_stream(session_prompt_id, &prompt.originator, state, writer)?,
        Err(error) => finish_error(session_prompt_id, &prompt.originator, error, writer)?,
    }
    Ok(())
}

fn finish_stream<W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    state: openai::StreamState,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let text_empty = state.text.is_empty();
    let text_content = state.text.clone();
    let input_tokens = state.input_tokens;
    let cached_tokens = state.cached_tokens;
    tracing::debug!(
        target: LOG_TARGET,
        session_prompt_id,
        input_tokens,
        cached_tokens,
        "agent response token usage"
    );
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
    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
        AgentResponseFinished {
            session_prompt_id: session_prompt_id.into(),
            text,
            tool_calls,
            input_tokens,
            cached_tokens,
            thinking,
            originator: originator.clone(),
        },
    )))?;
    writer.flush()?;
    Ok(())
}

fn finish_error<W: Write>(
    session_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    error: openai::OpenAiError,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
        AgentResponseFinished {
            session_prompt_id: session_prompt_id.into(),
            text: Some(format!("LLM error: {error}")),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            originator: originator.clone(),
        },
    )))?;
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

    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent-echo".into(),
        client_kind: ClientKind::Agent,
    })))?;
    writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SESSION_PROMPT_CREATED),
            EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
        ],
    })))?;
    writer.write_frame(&Frame::Message(Message::Ready(Ready {
        message: Some("echo agent ready".to_owned()),
    })))?;
    writer.flush()?;

    let mut next_call = 1_u64;

    loop {
        let Some(frame) = reader.read_frame()? else {
            return Ok(());
        };
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::SessionPromptCreated(prompt)) => {
                let spid = prompt.session_prompt_id.clone();
                writer.write_frame(&Frame::Event(Event::AgentPromptSubmitted(
                    AgentPromptSubmitted {
                        session_prompt_id: spid.clone(),
                        originator: prompt.originator.clone(),
                    },
                )))?;

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
                    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                        AgentResponseFinished {
                            session_prompt_id: spid,
                            text: Some(text),
                            tool_calls: Vec::new(),
                            input_tokens: None,
                            cached_tokens: None,
                            thinking: None,
                            originator: prompt.originator.clone(),
                        },
                    )))?;
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

                    writer.write_frame(&Frame::Event(Event::AgentResponseFinished(
                        AgentResponseFinished {
                            session_prompt_id: spid,
                            text: None,
                            tool_calls: vec![tool_call],
                            input_tokens: None,
                            cached_tokens: None,
                            thinking: None,
                            originator: prompt.originator.clone(),
                        },
                    )))?;
                }
                writer.flush()?;
            }
            Frame::Message(Message::Disconnect(_)) => return Ok(()),
            _ => {}
        }
        if let Some(id) = log_id {
            writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
            writer.flush()?;
        }
    }
}

#[cfg(test)]
mod tests;
