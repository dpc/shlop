//! Public entry points: blocking `run_*` daemons, the embedded
//! single-message helpers, and the small types passed to/from them.

use std::os::unix::net::UnixListener;
#[cfg(any(test, feature = "echo-agent"))]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use tau_proto::{
    ClientKind, Disconnect, Event, EventSelector, Frame, Hello, Message, PROTOCOL_VERSION,
    Subscribe, UiPromptSubmitted,
};
use tau_socket::SocketPeer;

use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::format::{format_extension_event, format_tool_progress};
use crate::harness::{Harness, default_agent_runner};
use crate::runtime_dir;
use crate::settings::{Config, resolve_config};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLaunchStatus {
    New,
    Resumed,
}

impl SessionLaunchStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resumed => "resumed",
        }
    }
}

impl From<SessionLaunchStatus> for tau_proto::SessionDirStatus {
    fn from(status: SessionLaunchStatus) -> Self {
        match status {
            SessionLaunchStatus::New => Self::New,
            SessionLaunchStatus::Resumed => Self::Resumed,
        }
    }
}

/// Serve-loop options for daemon mode.
#[derive(Clone, Debug, Eq, PartialEq, bon::Builder)]
pub struct ServeOptions {
    /// Hard cap on total served clients before the serve loop exits.
    /// Used mainly in tests to bound a run. `None` = unbounded.
    pub max_clients: Option<usize>,
    /// When set, the daemon exits as soon as the last attached UI
    /// socket disconnects. When clear, the daemon keeps running with
    /// no attached UIs — a later `tau --attach` can pick up the
    /// session. The `ui.detach_request` event flips this at runtime.
    ///
    /// Default `false`: daemon is long-lived unless explicitly told
    /// otherwise.
    #[builder(default)]
    pub exit_on_disconnect: bool,
    /// Session lifecycle status announced to UI clients for the eager
    /// session.
    #[builder(default = SessionLaunchStatus::New)]
    pub session_status: SessionLaunchStatus,
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            max_clients: None,
            exit_on_disconnect: false,
            session_status: SessionLaunchStatus::New,
            dirs: None,
        }
    }
}

/// One completed user interaction with optional progress updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionOutcome {
    pub lifecycle_messages: Vec<String>,
    pub progress_messages: Vec<String>,
    pub response: String,
}

/// Options for a one-shot embedded run.
#[derive(Clone, Debug, Default, Eq, PartialEq, bon::Builder)]
pub struct EmbeddedOptions {
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
}

pub(crate) fn bind_listener(path: &Path) -> Result<UnixListener, HarnessError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path).map_err(HarnessError::from)
}

/// Runs one embedded interaction and returns progress plus the final
/// agent response.
pub fn run_embedded_message_with_trace(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    run_embedded_message_impl(
        state_dir,
        session_id,
        message,
        default_agent_runner,
        EmbeddedOptions::default(),
    )
}

/// Runs one embedded interaction and returns the final agent response.
pub fn run_embedded_message(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, HarnessError> {
    Ok(run_embedded_message_with_trace(state_dir, session_id, message)?.response)
}

/// Like [`run_embedded_message_with_trace`] but lets the caller override
/// directory layout and other options.
pub fn run_embedded_message_with_options(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
    options: EmbeddedOptions,
) -> Result<InteractionOutcome, HarnessError> {
    run_embedded_message_impl(
        state_dir,
        session_id,
        message,
        default_agent_runner,
        options,
    )
}

/// Like [`run_embedded_message_with_trace`] but uses the echo agent for
/// testing.
#[cfg(any(test, feature = "echo-agent"))]
pub fn run_embedded_message_with_echo(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_agent::run_echo(r, w).map_err(|e| e.to_string())
    }
    run_embedded_message_impl(
        state_dir,
        session_id,
        message,
        echo_runner,
        EmbeddedOptions::default(),
    )
}

fn run_embedded_message_impl(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
    agent_runner: crate::harness::AgentRunner,
    options: EmbeddedOptions,
) -> Result<InteractionOutcome, HarnessError> {
    let state_dir = state_dir.into();
    let dirs = options.dirs.unwrap_or_default();
    let mut harness = Harness::new_with_agent(state_dir, dirs, agent_runner, session_id)?;
    let mut outcome = harness.send_user_message(session_id, message, None)?;
    harness.shutdown()?;
    outcome.lifecycle_messages = harness.lifecycle_messages.clone();
    Ok(outcome)
}

/// Runs a foreground daemon that accepts socket clients.
///
/// `eager_session_id` is the session the harness pre-warms (AGENTS.md +
/// skill discovery) and where `events.jsonl` lands. Subsequent prompts for
/// other session ids lazy-init.
pub fn run_daemon(
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener = bind_listener(&socket_path)?;
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness = Harness::new(state_dir, dirs, eager_session_id)?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Like [`run_daemon`] but uses the echo agent for testing. Also enables
/// the shell extension's `echo` tool so echo-agent–driven tool calls
/// resolve.
#[cfg(any(test, feature = "echo-agent"))]
pub fn run_daemon_with_echo(
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_agent::run_echo(r, w).map_err(|e| e.to_string())
    }
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener = bind_listener(&socket_path)?;
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness = Harness::new_with_agent(state_dir, dirs, echo_runner, eager_session_id)?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Runs a foreground daemon using extensions from configuration.
pub fn run_daemon_with_config(
    config: &Config,
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener = bind_listener(&socket_path)?;
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness = Harness::from_config(config, state_dir, dirs, eager_session_id)?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Sends one user message to a running daemon and returns progress
/// plus the final response.
///
/// Stamps the outgoing `UiPromptSubmitted` with a unique `ctx_id` and
/// uses the matching `SessionPromptCreated` to capture the
/// `session_prompt_id` the harness allocated for this submission.
/// Without this, opening a fresh socket against a daemon that has
/// served a previous prompt would replay that prompt's terminal
/// `AgentResponseFinished` to the new subscriber and the helper
/// would return the historical response instead of waiting for the
/// live one.
pub fn send_daemon_message_with_trace(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    let mut peer = SocketPeer::connect(socket_path)?;
    peer.send(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-cli".into(),
        client_kind: ClientKind::Ui,
    })))?;
    peer.send(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Prefix("agent.".to_owned()),
            EventSelector::Prefix("session.".to_owned()),
            EventSelector::Prefix("tool.".to_owned()),
            EventSelector::Prefix("extension.".to_owned()),
            EventSelector::Prefix("harness.".to_owned()),
        ],
    })))?;
    let ctx_id = next_ctx_id();
    peer.send(&Frame::Event(Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: session_id.into(),
        text: message.to_owned(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: Some(ctx_id.clone()),
    })))?;

    let started_at = Instant::now();
    let mut lifecycle_messages = Vec::new();
    let mut progress_messages = Vec::new();
    // Counter parsed out of the `SessionPromptCreated` whose `ctx_id`
    // matches our submit. The terminal `AgentResponseFinished` has a
    // spid counter `>= our_spid_counter` (equal when no tool calls,
    // higher when tool-result follow-ups bump the counter).
    let mut our_spid_counter: Option<u64> = None;
    loop {
        if RESPONSE_TIMEOUT <= started_at.elapsed() {
            return Err(HarnessError::ResponseTimeout);
        }
        if let Some(frame) = peer.recv_timeout(RESPONSE_TIMEOUT)? {
            // UI clients don't ack — they just consume the inner event.
            let (_log_id, frame) = frame.peel_log();
            match frame {
                Frame::Event(Event::ToolProgress(p)) => {
                    progress_messages.push(format_tool_progress(&p))
                }
                Frame::Event(Event::HarnessInfo(ref info)) => {
                    lifecycle_messages.push(info.message.clone());
                }
                Frame::Event(
                    ref event @ (Event::ExtensionStarting(_)
                    | Event::ExtensionReady(_)
                    | Event::ExtensionExited(_)
                    | Event::ExtensionRestarting(_)),
                ) => {
                    lifecycle_messages.push(format_extension_event(event));
                }
                Frame::Event(Event::SessionPromptCreated(prompt))
                    if prompt.ctx_id.as_deref() == Some(ctx_id.as_str()) =>
                {
                    our_spid_counter = parse_spid_counter(prompt.session_prompt_id.as_ref());
                }
                Frame::Event(Event::AgentResponseFinished(finished))
                    if finished.tool_calls.is_empty()
                        && our_spid_counter.is_some_and(|ours| {
                            parse_spid_counter(finished.session_prompt_id.as_ref())
                                .is_some_and(|c| ours <= c)
                        }) =>
                {
                    peer.send(&Frame::Message(Message::Disconnect(Disconnect {
                        reason: Some("done".to_owned()),
                    })))?;
                    return Ok(InteractionOutcome {
                        lifecycle_messages,
                        progress_messages,
                        response: finished.text.unwrap_or_default(),
                    });
                }
                Frame::Message(Message::Disconnect(d)) => {
                    return Err(HarnessError::Participant(
                        d.reason.unwrap_or_else(|| "daemon disconnected".to_owned()),
                    ));
                }
                _ => {}
            }
        }
    }
}

/// Generates a unique correlation id for one daemon-helper submission.
/// The pid + atomic counter combination is unique within the test
/// process; the bytes never need to be sortable or persisted.
fn next_ctx_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "tau-daemon-helper-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Parses the `sp-N` counter the harness assigns when allocating a
/// new `SessionPromptId`. Returns `None` if the format ever changes —
/// callers treat that as "can't correlate" rather than panicking.
fn parse_spid_counter(spid: &str) -> Option<u64> {
    spid.strip_prefix("sp-").and_then(|s| s.parse().ok())
}

/// Sends one user message to a running daemon and returns the final
/// response.
pub fn send_daemon_message(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, HarnessError> {
    Ok(send_daemon_message_with_trace(socket_path, session_id, message)?.response)
}

/// Runs the harness daemon with runtime directory management.
pub fn run_harness_daemon(
    project_root: &Path,
    config: &Config,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let startup_started_at = Instant::now();
    tracing::debug!(target: "tau_harness::startup", project_root = %project_root.display(), eager_session_id, "starting harness daemon");
    let daemon_dir = runtime_dir::prepare_daemon_dir(project_root)?;
    tracing::debug!(target: "tau_harness::startup", daemon_dir = %daemon_dir.path().display(), elapsed_ms = startup_started_at.elapsed().as_millis(), "prepared daemon dir");
    let listener = bind_listener(&daemon_dir.socket_path())?;
    tracing::debug!(target: "tau_harness::startup", socket_path = %daemon_dir.socket_path().display(), elapsed_ms = startup_started_at.elapsed().as_millis(), "bound daemon socket");

    let state_dir = crate::dirs::default_state_dir();
    let dirs = options.dirs.clone().unwrap_or_default();
    tracing::debug!(target: "tau_harness::startup", state_dir = %state_dir.display(), elapsed_ms = startup_started_at.elapsed().as_millis(), "constructing harness");
    let mut harness = Harness::from_config(config, &state_dir, dirs, eager_session_id)?;
    tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "harness constructed");
    harness.publish_event(
        None,
        Event::HarnessSessionDir(tau_proto::HarnessSessionDir {
            session_id: eager_session_id.to_owned().into(),
            path: state_dir.join(eager_session_id),
            status: options.session_status.into(),
        }),
    );

    // Write marker AFTER extensions are ready.
    tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "writing daemon ready markers");
    daemon_dir.write_marker()?;
    daemon_dir.write_pid()?;
    daemon_dir.write_session_id(eager_session_id)?;
    tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "daemon ready markers written");

    // Signal the parent CLI (if it passed us a pipe fd via
    // `TAU_READY_FD`) that the socket is bound and discoverable. The
    // parent is blocked on `read()` until this byte arrives, so the
    // wakeup latency is whatever it takes the kernel to deliver one
    // byte — not the 10ms granularity of a poll loop.
    runtime_dir::signal_ready_to_parent();

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    daemon_dir.cleanup();
    result
}

/// Entrypoint for `tau ext harness`.
pub fn run_component() -> Result<(), Box<dyn std::error::Error>> {
    let startup_started_at = Instant::now();
    tracing::debug!(target: "tau_harness::startup", "harness component starting");
    // Make TAU_VERSION/TAU_BUILD/TAU_LAST_MODIFIED visible to anything
    // we spawn (shell extension, sub-agents) by reading our own
    // `built` snapshot — saves the parent CLI from having to forward
    // these via env vars on every daemon launch.
    crate::version::export_to_env();
    let project_root = std::env::current_dir()?;
    tracing::debug!(target: "tau_harness::startup", project_root = %project_root.display(), elapsed_ms = startup_started_at.elapsed().as_millis(), "resolved project root");
    let config = resolve_config(None)?;
    tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "resolved config");
    // The CLI passes the minted/resumed session id via the harness's
    // SESSION_ID env var when spawning a daemon. Fallback to
    // `default_session_id()` covers a bare `tau ext harness`
    // launched without a CLI in front of it.
    let eager_session_id = std::env::var("TAU_SESSION_ID")
        .unwrap_or_else(|_| crate::dirs::default_session_id().to_owned());
    let session_status = match std::env::var("TAU_SESSION_STATUS").as_deref() {
        Ok("resumed") => crate::daemon::SessionLaunchStatus::Resumed,
        _ => crate::daemon::SessionLaunchStatus::New,
    };
    run_harness_daemon(
        &project_root,
        &config,
        &eager_session_id,
        // Exit once the spawning UI leaves. A UI that wants the
        // daemon to outlive it sends `ui.detach_request`, which
        // flips this to `false` at runtime.
        ServeOptions {
            exit_on_disconnect: true,
            session_status,
            ..Default::default()
        },
    )
    .map_err(Into::into)
}
