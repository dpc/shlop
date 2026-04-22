//! Harness daemon: manages extensions, routing, session state, and
//! serves socket clients.
//!
//! Each connection has a reader thread and a writer thread.  All
//! reader threads feed one shared `mpsc::channel`.  The harness event
//! loop blocks on `rx.recv()` and dispatches instantly.  The bus
//! delivers outgoing events by sending to per-connection writer
//! channels (non-blocking).  Writer threads drain their channel and
//! write to the stream; on channel close they run the shutdown
//! sequence for that connection.

pub mod runtime_dir;

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tau_config::{Config, ExtensionConfig};
use tau_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    DefaultSubscriptionPolicy, EventBus, EventLog, PolicyStore, RouteError, SessionEntry,
    SessionStore, SessionStoreError, ToolActivityOutcome, ToolActivityRecord, ToolRegistry,
    ToolRouteError,
};
use tau_proto::{
    AgentResponseFinished, AgentToolCall, CborValue, ClientKind, ContentBlock, ConversationMessage,
    ConversationRole, DecodeError, Event, EventReader, EventSelector, EventWriter,
    HarnessModelSelected, HarnessModelsAvailable, LifecycleDisconnect, LifecycleHello,
    LifecycleSubscribe, ModelId, PROTOCOL_VERSION, ProgressUpdate, SessionId, SessionPromptCreated,
    SessionPromptId, SessionPromptQueued, ToolCallId, ToolDefinition, ToolError, ToolName,
    ToolProgress, ToolRegister, ToolRequest, ToolResult, UiPromptSubmitted,
};
use tau_socket::{SocketPeer, SocketTransportError};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Serve-loop options for daemon mode.
#[derive(Clone, Debug, Default, Eq, PartialEq, bon::Builder)]
pub struct ServeOptions {
    pub max_clients: Option<usize>,
    pub policy_store_path: Option<PathBuf>,
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
}

/// One completed user interaction with optional progress updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionOutcome {
    pub lifecycle_messages: Vec<String>,
    pub progress_messages: Vec<String>,
    pub response: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by the harness.
#[derive(Debug)]
pub enum HarnessError {
    Io(io::Error),
    ProtocolDecode(DecodeError),
    ProtocolEncode(tau_proto::EncodeError),
    SessionStore(SessionStoreError),
    SocketTransport(SocketTransportError),
    Route(RouteError),
    ToolRoute(ToolRouteError),
    StartupTimeout,
    ResponseTimeout,
    ThreadJoin(String),
    Participant(String),
    NoAgentConfigured,
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::ProtocolDecode(source) => write!(f, "protocol decode error: {source}"),
            Self::ProtocolEncode(source) => write!(f, "protocol encode error: {source}"),
            Self::SessionStore(source) => write!(f, "session store error: {source}"),
            Self::SocketTransport(source) => write!(f, "socket transport error: {source}"),
            Self::Route(source) => write!(f, "routing error: {source}"),
            Self::ToolRoute(source) => write!(f, "tool routing error: {source}"),
            Self::StartupTimeout => f.write_str("timed out waiting for extensions to start"),
            Self::ResponseTimeout => f.write_str("timed out waiting for agent response"),
            Self::ThreadJoin(name) => write!(f, "failed to join {name} thread cleanly"),
            Self::Participant(message) => write!(f, "participant error: {message}"),
            Self::NoAgentConfigured => {
                f.write_str("no extension with role \"agent\" in configuration")
            }
        }
    }
}

impl std::error::Error for HarnessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::ProtocolDecode(source) => Some(source),
            Self::ProtocolEncode(source) => Some(source),
            Self::SessionStore(source) => Some(source),
            Self::SocketTransport(source) => Some(source),
            Self::Route(source) => Some(source),
            Self::ToolRoute(source) => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for HarnessError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}
impl From<DecodeError> for HarnessError {
    fn from(source: DecodeError) -> Self {
        Self::ProtocolDecode(source)
    }
}
impl From<SessionStoreError> for HarnessError {
    fn from(source: SessionStoreError) -> Self {
        Self::SessionStore(source)
    }
}
impl From<SocketTransportError> for HarnessError {
    fn from(source: SocketTransportError) -> Self {
        Self::SocketTransport(source)
    }
}
impl From<RouteError> for HarnessError {
    fn from(source: RouteError) -> Self {
        Self::Route(source)
    }
}
impl From<ToolRouteError> for HarnessError {
    fn from(source: ToolRouteError) -> Self {
        Self::ToolRoute(source)
    }
}

// ---------------------------------------------------------------------------
// Internal event type — all reader threads feed this into one channel
// ---------------------------------------------------------------------------

enum HarnessEvent {
    /// Decoded event from any connection (extension or client).
    FromConnection {
        connection_id: tau_proto::ConnectionId,
        event: Event,
    },
    /// A connection's reader hit EOF or decode error.
    Disconnected {
        connection_id: tau_proto::ConnectionId,
    },
    /// Socket listener accepted a new client.
    NewClient(UnixStream),
}

// ---------------------------------------------------------------------------
// Connection sink — sends to the per-connection writer channel
// ---------------------------------------------------------------------------

struct ChannelSink {
    tx: Sender<Event>,
}

impl ConnectionSink for ChannelSink {
    fn send(&mut self, event: tau_core::RoutedEvent) -> Result<(), ConnectionSendError> {
        self.tx
            .send(event.event)
            .map_err(|_| ConnectionSendError::new("writer closed"))
    }
}

// ---------------------------------------------------------------------------
// Reader thread — one per connection, sends to the shared harness channel
// ---------------------------------------------------------------------------

fn spawn_reader_thread(
    connection_id: tau_proto::ConnectionId,
    stream: impl io::Read + Send + 'static,
    tx: Sender<HarnessEvent>,
) {
    thread::spawn(move || {
        let mut reader = EventReader::new(BufReader::new(stream));
        loop {
            match reader.read_event() {
                Ok(Some(event)) => {
                    if tx
                        .send(HarnessEvent::FromConnection {
                            connection_id: connection_id.clone(),
                            event,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) | Err(_) => {
                    let _ = tx.send(HarnessEvent::Disconnected {
                        connection_id: connection_id.clone(),
                    });
                    return;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Writer thread — one per connection, drains channel and writes to stream
// ---------------------------------------------------------------------------

/// What the writer thread should do when its channel closes.
enum WriterShutdown {
    /// Just close the stream (socket clients, in-process peers).
    CloseStream,
    /// Supervised child: send disconnect, close stdin, wait/signal.
    KillChild(Child),
}

fn spawn_writer_thread(
    writer: impl Write + Send + 'static,
    shutdown: WriterShutdown,
) -> Sender<Event> {
    let (tx, rx) = mpsc::channel::<Event>();
    thread::spawn(move || {
        let mut w = EventWriter::new(BufWriter::new(writer));

        // Drain events until the channel closes.
        while let Ok(event) = rx.recv() {
            if w.write_event(&event).is_err() {
                return;
            }
            if w.flush().is_err() {
                return;
            }
        }

        // Channel closed — run shutdown sequence.
        match shutdown {
            WriterShutdown::CloseStream => {
                // Drop the writer → closes the stream.
            }
            WriterShutdown::KillChild(mut child) => {
                // Best-effort disconnect message.
                let _ = w.write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                    reason: Some("shutdown".to_owned()),
                }));
                let _ = w.flush();
                // Drop the writer → closes stdin → extension sees EOF.
                drop(w);

                // Wait for graceful exit, then escalate.
                let started = Instant::now();
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) => {}
                        Err(_) => return,
                    }
                    if SHUTDOWN_GRACE <= started.elapsed() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    });
    tx
}

// ---------------------------------------------------------------------------
// Extension tracking
// ---------------------------------------------------------------------------

/// Tracks whose turn it is in the agent interaction loop.
enum TurnState {
    /// Waiting for user input (or queued prompt dispatch).
    Idle,
    /// Waiting for tool extensions to finish per-session setup
    /// (announce skills + AGENTS.md) after a `SessionStarted` broadcast,
    /// before any user prompt for that session can be dispatched.
    InitializingSession {
        session_id: SessionId,
        waiting_on: std::collections::HashSet<tau_proto::ConnectionId>,
    },
    /// Agent is processing a prompt; we are waiting for its response.
    AgentThinking { _session_id: SessionId },
    /// Agent requested tool calls; waiting for all results before
    /// sending the next prompt.
    ToolsRunning {
        session_id: SessionId,
        remaining_calls: Vec<ToolCallId>,
    },
}

impl TurnState {
    fn is_idle(&self) -> bool {
        matches!(self, TurnState::Idle)
    }
}

/// Outcome of `submit_user_prompt`: either the prompt was handed off to
/// the agent immediately, or it was placed on `pending_prompts` and will
/// be dispatched once the harness is ready (model selected, agent idle,
/// extensions ready, session initialized).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptSubmission {
    Dispatched,
    Queued,
}

/// Lifecycle phase of a configured extension. Drives the
/// `extensions_all_ready()` gate that keeps user prompts queued until
/// every desired extension has finished its handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExtensionState {
    /// Process spawned (or in-process thread started); no
    /// `LifecycleHello` seen yet.
    Spawning,
    /// `LifecycleHello` received; waiting for the extension to finish
    /// announcing tools/skills and emit `LifecycleReady`.
    Handshaking,
    /// `LifecycleReady` received; the extension is fully online.
    Ready,
    /// The connection dropped after at least reaching `Spawning`.
    /// Fresh prompts continue with the remaining live providers.
    Disconnected,
}

struct ExtensionEntry {
    name: String,
    instance_id: tau_proto::ExtensionInstanceId,
    connection_id: tau_proto::ConnectionId,
    /// PID of supervised child process, or current process for in-process.
    pid: Option<u32>,
    /// In-process extension thread handle (for join on shutdown).
    in_process_thread: Option<JoinHandle<Result<(), String>>>,
    /// Current lifecycle state. See `extensions_all_ready` for how this
    /// gates dispatch.
    state: ExtensionState,
    /// Highest `LogEventId` the extension has acknowledged. Cumulative —
    /// any id `<= last_acked` is considered processed. Used by future
    /// reconnect/replay machinery; today it's tracked but not yet
    /// consumed.
    last_acked: tau_proto::LogEventId,
}

// ---------------------------------------------------------------------------
// Event debug log
// ---------------------------------------------------------------------------

/// Append-only JSON event log for debugging.
struct DebugEventLog {
    path: PathBuf,
    file: std::fs::File,
}

impl DebugEventLog {
    fn open(dir: &Path) -> Result<Self, HarnessError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("events.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self { path, file })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn log_harness_event(&mut self, harness_event: &HarnessEvent) {
        use std::io::Write;
        let entry = match harness_event {
            HarnessEvent::FromConnection {
                connection_id,
                event,
            } => {
                let event_json = serde_json::to_value(event).unwrap_or_default();
                serde_json::json!({
                    "type": "from_connection",
                    "source": connection_id,
                    "event_name": event.name().as_str(),
                    "event": event_json,
                })
            }
            HarnessEvent::Disconnected { connection_id } => {
                serde_json::json!({
                    "type": "disconnected",
                    "source": connection_id,
                })
            }
            HarnessEvent::NewClient(_) => {
                serde_json::json!({ "type": "new_client" })
            }
        };
        let _ = serde_json::to_writer(&mut self.file, &entry);
        let _ = self.file.write_all(b"\n");
        let _ = self.file.flush();
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A skill discovered by an extension.
struct DiscoveredSkill {
    source_id: tau_proto::ConnectionId,
    description: String,
    file_path: std::path::PathBuf,
    add_to_prompt: bool,
}

/// One AGENTS.md file discovered by an extension.
struct DiscoveredAgentsFile {
    source_id: tau_proto::ConnectionId,
    file_path: PathBuf,
    content: String,
}

/// Connection ID used for harness-owned tools (e.g. the `skill` tool).
const HARNESS_CONNECTION_ID: &str = "__harness__";

struct Harness {
    tx: Sender<HarnessEvent>,
    rx: Receiver<HarnessEvent>,
    bus: EventBus,
    registry: ToolRegistry,
    store: SessionStore,
    pending_request_sessions: VecDeque<SessionId>,
    pending_tool_sessions: std::collections::HashMap<ToolCallId, SessionId>,
    event_log: std::sync::Arc<EventLog>,
    /// Writer channels for socket clients, keyed by connection ID.
    /// Used to start follower threads for log-based replay + delivery.
    client_writers: std::collections::HashMap<tau_proto::ConnectionId, Sender<Event>>,
    lifecycle_messages: Vec<String>,
    extensions: Vec<ExtensionEntry>,
    agent_connection_id: tau_proto::ConnectionId,
    _next_instance_counter: u64,
    next_session_prompt_id: u64,
    /// Maps session_prompt_id → session_id for in-flight prompts.
    prompt_sessions: std::collections::HashMap<SessionPromptId, SessionId>,
    /// Whose turn it is in the agent interaction loop.
    turn_state: TurnState,
    /// Queued user prompts waiting for the current turn to finish.
    /// Each entry is (session_id, text) and is persisted only when it
    /// is actually dispatched to the agent.
    //
    // Future: add a steering queue for mid-turn injection. Steering
    // messages would be injected after tool-call turns complete but
    // before the next LLM call, allowing the user to redirect the
    // agent while it's working. See PI_PROMPT_QUEUEING.md for Pi's
    // two-tier (steering + follow-up) design.
    /// (session_id, text) — text is persisted when dispatched.
    pending_prompts: VecDeque<(SessionId, String)>,
    /// Append-only event debug log.
    debug_log: Option<DebugEventLog>,
    /// All available models as `"provider/model_id"` strings.
    available_models: Vec<ModelId>,
    /// Currently selected model as `"provider/model_id"`.
    selected_model: ModelId,
    /// Skills discovered by extensions, keyed by name.
    discovered_skills: std::collections::HashMap<String, DiscoveredSkill>,
    /// AGENTS.md files discovered by extensions, in delivery order.
    discovered_agents_files: Vec<DiscoveredAgentsFile>,
    /// Sessions whose AGENTS/skill discovery has completed.
    initialized_sessions: std::collections::HashSet<SessionId>,
    /// Session prompt IDs that have already been completed by the agent.
    /// Used to dedupe duplicate `AgentResponseFinished` events that can
    /// arise under at-least-once delivery (e.g. an agent that reconnects
    /// after a crash and replays its last prompt).
    completed_prompts: std::collections::HashSet<SessionPromptId>,
    /// Tool invocations from the current agent turn that have not been
    /// dispatched yet. Drained in FIFO order by
    /// `drain_pending_tool_invocations` whenever the in-flight set
    /// allows the next call through. Cleared out implicitly: a turn
    /// only completes once this is empty and `in_flight_tool_kinds` is
    /// empty.
    pending_tool_invocations: VecDeque<(SessionId, AgentToolCall, tau_proto::ToolSideEffects)>,
    /// Kind of every tool call currently dispatched but not yet
    /// completed (no `ToolResult`/`ToolError` received). Keyed by
    /// `call_id`. Used by the dispatch state machine to decide whether
    /// the next queued invocation can proceed: a `Pure` call may go
    /// whenever no `Mutating` is in flight; a `Mutating` call may go
    /// only when this set is empty.
    in_flight_tool_kinds: std::collections::HashMap<ToolCallId, tau_proto::ToolSideEffects>,
    /// Directory layout (config + state) the harness reads and writes.
    dirs: tau_config::settings::TauDirs,
}

type AgentRunner = fn(UnixStream, UnixStream) -> Result<(), String>;

fn default_agent_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
    tau_agent::run(r, w).map_err(|e| e.to_string())
}

impl Harness {
    /// Creates a harness with in-process extensions (agent, fs, shell).
    fn new(
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
    ) -> Result<Self, HarnessError> {
        Self::new_with_agent(
            store_path,
            policy_store_path,
            dirs,
            default_agent_runner,
            false,
            None,
        )
    }

    fn new_with_agent(
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        agent_runner: AgentRunner,
        include_echo: bool,
        debug_log_dir: Option<&Path>,
    ) -> Result<Self, HarnessError> {
        let (tx, rx) = mpsc::channel();
        let mut bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(PolicyStore::open(policy_store_path.into())?),
        ));
        let store = SessionStore::open(store_path)?;

        let own_pid = std::process::id();
        let mut _next_instance_counter: u64 = 0;

        let mut extensions = Vec::new();
        // Agent
        let (conn_id, thread) =
            spawn_in_process("agent", ClientKind::Agent, agent_runner, &mut bus, &tx)?;
        let agent_connection_id = conn_id.clone();
        let iid = tau_proto::ExtensionInstanceId::new(_next_instance_counter);
        _next_instance_counter += 1;
        extensions.push(ExtensionEntry {
            name: "agent".to_owned(),
            instance_id: iid,
            connection_id: conn_id,
            pid: Some(own_pid),
            in_process_thread: Some(thread),
            state: ExtensionState::Spawning,
            last_acked: tau_proto::LogEventId::default(),
        });

        // Filesystem and shell tools
        let (conn_id, thread) = spawn_in_process(
            "tools",
            ClientKind::Tool,
            move |r, w| tau_ext_fs::run(r, w, include_echo).map_err(|e| e.to_string()),
            &mut bus,
            &tx,
        )?;
        let iid = tau_proto::ExtensionInstanceId::new(_next_instance_counter);
        _next_instance_counter += 1;
        extensions.push(ExtensionEntry {
            name: "tools".to_owned(),
            instance_id: iid,
            connection_id: conn_id,
            pid: Some(own_pid),
            in_process_thread: Some(thread),
            state: ExtensionState::Spawning,
            last_acked: tau_proto::LogEventId::default(),
        });

        let (available_models, selected_model) = load_model_list(&dirs);

        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            store,
            pending_request_sessions: VecDeque::new(),
            pending_tool_sessions: std::collections::HashMap::new(),
            event_log: EventLog::new(),
            client_writers: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            agent_connection_id,
            extensions,
            _next_instance_counter,
            next_session_prompt_id: 0,
            prompt_sessions: std::collections::HashMap::new(),
            turn_state: TurnState::Idle,
            pending_prompts: VecDeque::new(),
            debug_log: None,
            available_models,
            selected_model,
            discovered_skills: std::collections::HashMap::new(),
            discovered_agents_files: Vec::new(),
            initialized_sessions: std::collections::HashSet::new(),
            completed_prompts: std::collections::HashSet::new(),
            pending_tool_invocations: VecDeque::new(),
            in_flight_tool_kinds: std::collections::HashMap::new(),
            dirs,
        };

        if let Some(dir) = debug_log_dir {
            let _path = harness.enable_debug_log(dir)?;
        }

        for i in 0..harness.extensions.len() {
            let name = harness.extensions[i].name.clone();
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_extensions_ready()?;
        harness.register_harness_tools();
        harness.check_config_exists();
        Ok(harness)
    }

    /// Creates a harness from configuration, spawning real child processes.
    fn from_config(
        config: &Config,
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        debug_log_dir: Option<&Path>,
    ) -> Result<Self, HarnessError> {
        let (tx, rx) = mpsc::channel();
        let mut bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(PolicyStore::open(policy_store_path.into())?),
        ));
        let store = SessionStore::open(store_path)?;

        let mut extensions = Vec::new();
        let mut _next_instance_counter: u64 = 0;
        let mut agent_connection_id = None;

        for ext_config in &config.extensions {
            let kind = match ext_config.role.as_deref() {
                Some("agent") => ClientKind::Agent,
                _ => ClientKind::Tool,
            };

            let (conn_id, child_pid) = spawn_supervised(ext_config, kind.clone(), &mut bus, &tx)?;

            if kind == ClientKind::Agent {
                agent_connection_id = Some(conn_id.clone());
            }
            let iid = tau_proto::ExtensionInstanceId::new(_next_instance_counter);
            _next_instance_counter += 1;
            extensions.push(ExtensionEntry {
                name: ext_config.name.clone(),
                instance_id: iid,
                connection_id: conn_id,
                pid: Some(child_pid),
                in_process_thread: None,
                state: ExtensionState::Spawning,
                last_acked: tau_proto::LogEventId::default(),
            });
        }

        let agent_connection_id = agent_connection_id.ok_or(HarnessError::NoAgentConfigured)?;

        let (available_models, selected_model) = load_model_list(&dirs);

        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            store,
            pending_request_sessions: VecDeque::new(),
            pending_tool_sessions: std::collections::HashMap::new(),
            event_log: EventLog::new(),
            client_writers: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            agent_connection_id,
            extensions,
            _next_instance_counter,
            next_session_prompt_id: 0,
            prompt_sessions: std::collections::HashMap::new(),
            turn_state: TurnState::Idle,
            pending_prompts: VecDeque::new(),
            debug_log: None,
            available_models,
            selected_model,
            discovered_skills: std::collections::HashMap::new(),
            discovered_agents_files: Vec::new(),
            initialized_sessions: std::collections::HashSet::new(),
            completed_prompts: std::collections::HashSet::new(),
            pending_tool_invocations: VecDeque::new(),
            in_flight_tool_kinds: std::collections::HashMap::new(),
            dirs,
        };

        if let Some(dir) = debug_log_dir {
            let _path = harness.enable_debug_log(dir)?;
        }

        for i in 0..harness.extensions.len() {
            let name = harness.extensions[i].name.clone();
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_extensions_ready()?;
        harness.register_harness_tools();
        harness.check_config_exists();
        Ok(harness)
    }

    fn log_event(&mut self, harness_event: &HarnessEvent) {
        if let Some(log) = &mut self.debug_log {
            log.log_harness_event(harness_event);
        }
    }

    /// Publishes an event to both the event bus and the event log.
    fn publish_event(&mut self, source: Option<&str>, event: Event) {
        let seq = self
            .event_log
            .append(source.map(tau_proto::ConnectionId::from), event.clone());
        // Wrap in a `LogEvent` envelope so subscribers get the id and
        // can ack after processing. Receivers that don't care (UIs)
        // call `peel_log()` and discard the id.
        let log_event = Event::LogEvent(tau_proto::LogEvent {
            id: tau_proto::LogEventId::new(seq),
            event: Box::new(event),
        });
        let _ = self.bus.publish_from(source, log_event);
    }

    fn enable_debug_log(&mut self, dir: &Path) -> Result<PathBuf, HarnessError> {
        let log = DebugEventLog::open(dir)?;
        let path = log.path().to_path_buf();
        self.debug_log = Some(log);
        Ok(path)
    }

    // -----------------------------------------------------------------------
    // Startup
    // -----------------------------------------------------------------------

    /// Drives the event loop until every configured extension reaches
    /// `ExtensionState::Ready`. Replaces the old `wait_for_startup(n)`:
    /// state transitions are tracked per-extension so the same predicate
    /// can also gate runtime dispatch in `dispatch_blocked`.
    fn wait_for_extensions_ready(&mut self) -> Result<(), HarnessError> {
        if self.extensions_all_ready() {
            return Ok(());
        }
        let started_at = Instant::now();
        while !self.extensions_all_ready() {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let event = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&event);
            match event {
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    self.handle_extension_event(&connection_id, event)?;
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let name = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| connection_id.to_string());
                    self.handle_disconnect(&connection_id);
                    return Err(HarnessError::Participant(format!(
                        "{name} disconnected during startup"
                    )));
                }
                HarnessEvent::NewClient(_) => {}
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Main event loop (daemon mode)
    // -----------------------------------------------------------------------

    fn run_event_loop(&mut self, max_clients: Option<usize>) -> Result<(), HarnessError> {
        let mut served_clients = 0_usize;
        loop {
            if max_clients.is_some_and(|max| served_clients >= max) {
                break;
            }
            let Ok(event) = self.rx.recv() else { break };
            self.log_event(&event);
            match event {
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    let origin = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.origin.clone());
                    match origin {
                        Some(ConnectionOrigin::Socket) => {
                            let keep = self.handle_client_event(&connection_id, event)?;
                            if !keep {
                                let _ = self.bus.disconnect(&connection_id);
                                served_clients += 1;
                            }
                        }
                        Some(_) => self.handle_extension_event(&connection_id, event)?,
                        None => {} // already disconnected
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let is_agent = connection_id == self.agent_connection_id;
                    let was_socket = self
                        .bus
                        .connection(&connection_id)
                        .is_some_and(|m| m.origin == ConnectionOrigin::Socket);
                    self.handle_disconnect(&connection_id);
                    if was_socket {
                        served_clients += 1;
                    }
                    if is_agent {
                        return Err(HarnessError::Participant("agent disconnected".to_owned()));
                    }
                }
                HarnessEvent::NewClient(stream) => {
                    self.accept_client(stream)?;
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Client acceptance
    // -----------------------------------------------------------------------

    fn accept_client(&mut self, stream: UnixStream) -> Result<(), HarnessError> {
        let write_stream = stream.try_clone()?;
        let writer_tx = spawn_writer_thread(write_stream, WriterShutdown::CloseStream);
        let writer_tx_for_follower = writer_tx.clone();
        let conn_id = self.bus.connect(Connection::new(
            ConnectionMetadata {
                id: tau_proto::ConnectionId::default(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin: ConnectionOrigin::Socket,
            },
            Box::new(ChannelSink { tx: writer_tx }),
        ));
        self.client_writers
            .insert(conn_id.clone(), writer_tx_for_follower);
        spawn_reader_thread(conn_id, stream, self.tx.clone());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    fn handle_extension_event(
        &mut self,
        source_id: &str,
        event: Event,
    ) -> Result<(), HarnessError> {
        match event {
            Event::Ack(ack) => {
                // Cumulative ack: advance the cursor if it moves
                // forward, ignore otherwise (duplicates, late acks).
                if let Some(entry) = self
                    .extensions
                    .iter_mut()
                    .find(|e| e.connection_id.as_str() == source_id)
                {
                    if ack.up_to.get() > entry.last_acked.get() {
                        entry.last_acked = ack.up_to;
                    }
                }
            }
            Event::LifecycleHello(hello) => {
                self.set_extension_state(source_id, ExtensionState::Handshaking);
                self.publish_event(Some(source_id), Event::LifecycleHello(hello));
            }
            Event::LifecycleSubscribe(subscribe) => {
                self.bus
                    .set_subscriptions(source_id, subscribe.selectors.clone())?;
                self.publish_event(Some(source_id), Event::LifecycleSubscribe(subscribe));
            }
            Event::LifecycleReady(ready) => {
                self.emit_extension_ready(source_id);
                self.publish_event(Some(source_id), Event::LifecycleReady(ready));
                self.set_extension_state(source_id, ExtensionState::Ready);
                self.try_advance_queue();
            }
            Event::ToolRegister(ToolRegister { tool }) => {
                let _ = self.registry.register(source_id, tool);
            }
            Event::ToolRequest(request) => {
                self.persist_tool_request(&request)?;
                match self
                    .registry
                    .route_tool_request(&mut self.bus, source_id, request.clone())
                {
                    Ok(_) => {}
                    Err(ToolRouteError::NoProvider { tool_name }) => {
                        let error = ToolError {
                            call_id: request.call_id,
                            tool_name,
                            message: "no live provider available".to_owned(),
                            details: None,
                        };
                        self.persist_tool_error(&error)?;
                        self.publish_event(None, Event::ToolError(error));
                    }
                    Err(error) => return Err(HarnessError::ToolRoute(error)),
                }
            }
            Event::ToolResult(result) => {
                if self.pending_tool_sessions.contains_key(&result.call_id) {
                    self.persist_tool_result(&result)?;
                    let call_id = result.call_id.to_string();
                    self.publish_event(Some(source_id), Event::ToolResult(result));
                    self.on_tool_call_complete(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool result for call_id={}",
                        result.call_id
                    ));
                }
            }
            Event::ToolError(error) => {
                if self.pending_tool_sessions.contains_key(&error.call_id) {
                    self.persist_tool_error(&error)?;
                    let call_id = error.call_id.to_string();
                    self.publish_event(Some(source_id), Event::ToolError(error));
                    self.on_tool_call_complete(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool error for call_id={}",
                        error.call_id
                    ));
                }
            }
            Event::ToolProgress(progress) => {
                self.publish_event(Some(source_id), Event::ToolProgress(progress));
            }
            Event::ExtSkillAvailable(ref skill) => {
                self.discovered_skills.insert(
                    skill.name.clone(),
                    DiscoveredSkill {
                        source_id: source_id.into(),
                        description: skill.description.clone(),
                        file_path: std::path::PathBuf::from(&skill.file_path),
                        add_to_prompt: skill.add_to_prompt,
                    },
                );
                self.publish_event(Some(source_id), event);
            }
            Event::ExtAgentsMdAvailable(ref agents) => {
                let file_path = PathBuf::from(&agents.file_path);
                if let Some(existing) = self.discovered_agents_files.iter_mut().find(|existing| {
                    existing.source_id == source_id && existing.file_path == file_path
                }) {
                    existing.content = agents.content.clone();
                } else {
                    self.discovered_agents_files.push(DiscoveredAgentsFile {
                        source_id: source_id.into(),
                        file_path,
                        content: agents.content.clone(),
                    });
                }
                self.publish_event(Some(source_id), event);
            }
            Event::ExtensionContextReady(ready) => {
                self.publish_event(Some(source_id), Event::ExtensionContextReady(ready.clone()));
                self.handle_extension_context_ready(source_id, ready)?;
            }
            Event::AgentPromptSubmitted(_) | Event::AgentResponseUpdated(_) => {
                self.publish_event(Some(source_id), event);
            }
            Event::AgentResponseFinished(response) => {
                self.publish_event(None, Event::AgentResponseFinished(response.clone()));
                self.handle_agent_response_finished(response)?;
            }
            other => {
                self.publish_event(Some(source_id), other);
            }
        }
        Ok(())
    }

    fn handle_client_event(&mut self, client_id: &str, event: Event) -> Result<bool, HarnessError> {
        match event {
            Event::LifecycleHello(hello) => {
                self.publish_event(Some(client_id), Event::LifecycleHello(hello));
                Ok(true)
            }
            Event::LifecycleSubscribe(subscribe) => {
                // Policy check via the bus.
                match self
                    .bus
                    .set_subscriptions(client_id, subscribe.selectors.clone())
                {
                    Ok(()) => {
                        let selectors_for_replay = subscribe.selectors.clone();
                        self.publish_event(Some(client_id), Event::LifecycleSubscribe(subscribe));
                        self.replay_harness_info(client_id, &selectors_for_replay);
                        Ok(true)
                    }
                    Err(RouteError::SubscriptionDenied { reason, .. }) => {
                        let _ = self.bus.send_to(
                            client_id,
                            None,
                            Event::LifecycleDisconnect(LifecycleDisconnect {
                                reason: Some(format!("subscription denied: {reason}")),
                            }),
                        );
                        Ok(false)
                    }
                    Err(other) => Err(HarnessError::Route(other)),
                }
            }
            Event::UiModelSelect(select) => {
                if self.available_models.contains(&select.model) {
                    let was_empty = self.selected_model.is_empty();
                    self.selected_model = select.model.clone();
                    save_last_selected_model(&self.dirs, &self.selected_model);
                    self.publish_event(
                        None,
                        Event::HarnessModelSelected(HarnessModelSelected {
                            model: self.selected_model.clone(),
                        }),
                    );
                    // If we just went from no-model to having one,
                    // drain queued prompts.
                    if was_empty && self.turn_state.is_idle() {
                        self.try_advance_queue();
                    }
                } else {
                    self.publish_event(
                        None,
                        Event::HarnessInfo(tau_proto::HarnessInfo {
                            message: format!("unknown model: {}", select.model),
                        }),
                    );
                }
                Ok(true)
            }
            Event::UiPromptSubmitted(prompt) => {
                self.publish_event(Some(client_id), Event::UiPromptSubmitted(prompt.clone()));

                let submission =
                    self.submit_user_prompt(prompt.session_id.clone(), prompt.text.clone())?;
                if submission == PromptSubmission::Queued {
                    self.publish_event(
                        None,
                        Event::SessionPromptQueued(SessionPromptQueued {
                            session_id: prompt.session_id.clone(),
                            text: prompt.text.clone(),
                        }),
                    );
                    if self.selected_model.is_empty() {
                        self.emit_info("no model selected — use /model to pick one");
                    }
                }
                Ok(true)
            }
            Event::LifecycleDisconnect(_) => Ok(false),
            other => {
                self.publish_event(Some(client_id), other);
                Ok(true)
            }
        }
    }

    fn handle_disconnect(&mut self, connection_id: &str) {
        self.remove_discovered_context(connection_id);
        self.maybe_complete_session_init_for_disconnect(connection_id);
        self.set_extension_state(connection_id, ExtensionState::Disconnected);
        let Some(meta) = self.bus.disconnect(connection_id) else {
            return;
        };
        if meta.origin == ConnectionOrigin::Supervised || meta.origin == ConnectionOrigin::InMemory
        {
            let _ = self.registry.unregister_connection(connection_id);
            self.emit_extension_exited(&meta.name);
        }
    }

    // -----------------------------------------------------------------------
    // Persistence helpers
    // -----------------------------------------------------------------------

    fn persist_tool_request(&mut self, request: &ToolRequest) -> Result<(), HarnessError> {
        let session_id = self
            .pending_request_sessions
            .pop_front()
            .unwrap_or_else(|| "default".into());
        self.pending_tool_sessions
            .insert(request.call_id.clone(), session_id.clone());
        self.store.append_tool_activity(
            session_id.into_string(),
            ToolActivityRecord {
                call_id: request.call_id.clone(),
                tool_name: request.tool_name.clone(),
                outcome: ToolActivityOutcome::Requested {
                    arguments: request.arguments.clone(),
                },
            },
        )?;
        Ok(())
    }

    fn persist_tool_result(&mut self, result: &ToolResult) -> Result<(), HarnessError> {
        let session_id = self
            .pending_tool_sessions
            .remove(result.call_id.as_str())
            .unwrap_or_else(|| "default".into());
        self.store.append_tool_activity(
            session_id.into_string(),
            ToolActivityRecord {
                call_id: result.call_id.clone(),
                tool_name: result.tool_name.clone(),
                outcome: ToolActivityOutcome::Result {
                    result: result.result.clone(),
                },
            },
        )?;
        Ok(())
    }

    fn persist_tool_error(&mut self, error: &ToolError) -> Result<(), HarnessError> {
        let session_id = self
            .pending_tool_sessions
            .remove(error.call_id.as_str())
            .unwrap_or_else(|| "default".into());
        self.store.append_tool_activity(
            session_id.into_string(),
            ToolActivityRecord {
                call_id: error.call_id.clone(),
                tool_name: error.tool_name.clone(),
                outcome: ToolActivityOutcome::Error {
                    message: error.message.clone(),
                    details: error.details.clone(),
                },
            },
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Lifecycle helpers
    // -----------------------------------------------------------------------

    fn find_extension_by_name(&self, name: &str) -> Option<&ExtensionEntry> {
        self.extensions.iter().find(|e| e.name == name)
    }

    fn find_extension_by_connection(&self, connection_id: &str) -> Option<&ExtensionEntry> {
        self.extensions
            .iter()
            .find(|e| e.connection_id == connection_id)
    }

    fn emit_extension_starting(&mut self, extension_name: &str) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.lifecycle_messages
            .push(format!("extension {extension_name} starting"));
        self.publish_event(
            Some("harness"),
            Event::ExtensionStarting(tau_proto::ExtensionStarting {
                instance_id: iid,
                extension_name: extension_name.into(),
                pid,
            }),
        );
    }

    fn emit_extension_ready(&mut self, connection_id: &str) {
        let Some(ext) = self.find_extension_by_connection(connection_id) else {
            return;
        };
        let name = ext.name.clone();
        let iid = ext.instance_id;
        let pid = ext.pid;
        self.lifecycle_messages
            .push(format!("extension {name} ready"));
        self.publish_event(
            Some("harness"),
            Event::ExtensionReady(tau_proto::ExtensionReady {
                instance_id: iid,
                extension_name: name.into(),
                pid,
            }),
        );
    }

    fn emit_extension_exited(&mut self, extension_name: &str) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.lifecycle_messages
            .push(format!("extension {extension_name} exited"));
        self.publish_event(
            Some("harness"),
            Event::ExtensionExited(tau_proto::ExtensionExited {
                instance_id: iid,
                extension_name: extension_name.into(),
                pid,
                exit_code: None,
                signal: None,
            }),
        );
    }

    fn check_config_exists(&mut self) {
        if let Some(dir) = tau_config::settings::config_dir() {
            if !dir.join("harness.json5").exists() {
                self.emit_info("no config found; run `tau init` to create sample config files");
            }
        }
    }

    fn emit_info(&mut self, message: &str) {
        self.publish_event(
            Some("harness"),
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: message.to_owned(),
            }),
        );
    }

    /// Replays harness info and extension lifecycle events from the log
    /// to a late-joining client.
    fn replay_harness_info(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let mut cursor = 0;
        while let Some(entry) = self.event_log.get_next_from(cursor) {
            cursor = entry.seq + 1;
            let dominated = matches!(
                entry.event,
                Event::HarnessInfo(_)
                    | Event::ExtensionStarting(_)
                    | Event::ExtensionReady(_)
                    | Event::ExtensionExited(_)
            );
            if dominated && selector_matches_event(selectors, &entry.event) {
                let _ = self
                    .bus
                    .send_to(client_id, entry.source.as_deref(), entry.event);
            }
        }

        // Send current model state to the new client.
        let models_event = Event::HarnessModelsAvailable(HarnessModelsAvailable {
            models: self.available_models.clone(),
        });
        if selector_matches_event(selectors, &models_event) {
            let _ = self.bus.send_to(client_id, None, models_event);
        }
        let selected_event = Event::HarnessModelSelected(HarnessModelSelected {
            model: self.selected_model.clone(),
        });
        if selector_matches_event(selectors, &selected_event) {
            let _ = self.bus.send_to(client_id, None, selected_event);
        }
    }

    fn remove_discovered_context(&mut self, source_id: &str) {
        self.discovered_skills
            .retain(|_, skill| skill.source_id != source_id);
        self.discovered_agents_files
            .retain(|file| file.source_id != source_id);
    }

    fn session_init_provider_ids(&self) -> std::collections::HashSet<tau_proto::ConnectionId> {
        let event = Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "probe".into(),
        });
        self.bus
            .connections()
            .into_iter()
            .filter(|connection| {
                connection.kind == ClientKind::Tool
                    && connection.origin != ConnectionOrigin::Socket
                    && self
                        .bus
                        .subscriptions(connection.id.as_str())
                        .is_some_and(|selectors| selector_matches_event(selectors, &event))
            })
            .map(|connection| connection.id)
            .collect()
    }

    fn dispatch_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), HarnessError> {
        self.store
            .append_user_message(session_id.as_str(), text.clone())?;
        self.turn_state = TurnState::AgentThinking {
            _session_id: session_id.clone(),
        };
        self.send_prompt_to_agent(&session_id);
        Ok(())
    }

    fn session_initialized(&self, session_id: &SessionId) -> bool {
        self.initialized_sessions.contains(session_id)
    }

    /// Queue a prompt when it cannot be sent directly yet, or dispatch
    /// it immediately when the session is initialized and the harness is
    /// ready to talk to the agent.
    fn submit_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HarnessError> {
        if self.dispatch_blocked() || !self.session_initialized(&session_id) {
            self.pending_prompts.push_back((session_id, text));
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }

        self.dispatch_user_prompt(session_id, text)?;
        Ok(PromptSubmission::Dispatched)
    }

    /// Broadcasts `SessionStarted` for `session_id` and enters
    /// `InitializingSession` until every subscribed tool extension has
    /// acknowledged with `ExtensionContextReady` (or all of them have
    /// disconnected). When the wait set drains, AGENTS.md content is
    /// injected into the session log and any queued user prompts are
    /// dispatched.
    fn start_session_init(&mut self, session_id: SessionId) {
        let waiting_on = self.session_init_provider_ids();
        if waiting_on.is_empty() {
            if let Err(error) = self.complete_session_init(session_id) {
                self.emit_info(&format!("failed to initialize session: {error}"));
                self.turn_state = TurnState::Idle;
            }
            return;
        }

        for source_id in &waiting_on {
            self.remove_discovered_context(source_id.as_str());
        }

        self.turn_state = TurnState::InitializingSession {
            session_id: session_id.clone(),
            waiting_on,
        };
        self.publish_event(
            None,
            Event::SessionStarted(tau_proto::SessionStarted { session_id }),
        );
    }

    fn handle_extension_context_ready(
        &mut self,
        source_id: &str,
        ready: tau_proto::ExtensionContextReady,
    ) -> Result<(), HarnessError> {
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                waiting_on,
            } if *session_id == ready.session_id => {
                waiting_on.remove(source_id);
                waiting_on.is_empty().then(|| session_id.clone())
            }
            _ => None,
        };

        if let Some(session_id) = completed_session {
            self.complete_session_init(session_id)?;
        }

        Ok(())
    }

    fn maybe_complete_session_init_for_disconnect(&mut self, connection_id: &str) {
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                waiting_on,
            } => {
                let removed = waiting_on.remove(connection_id);
                if removed && waiting_on.is_empty() {
                    Some(session_id.clone())
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(session_id) = completed_session {
            if let Err(error) = self.complete_session_init(session_id) {
                self.emit_info(&format!("failed to initialize session: {error}"));
                self.turn_state = TurnState::Idle;
            }
        }
    }

    fn complete_session_init(&mut self, session_id: SessionId) -> Result<(), HarnessError> {
        self.ensure_agents_context_inserted(session_id.as_str())?;
        self.initialized_sessions.insert(session_id);
        self.turn_state = TurnState::Idle;
        self.try_advance_queue();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Agent prompt assembly
    // -----------------------------------------------------------------------

    fn ensure_agents_context_inserted(&mut self, session_id: &str) -> Result<(), HarnessError> {
        if self.discovered_agents_files.is_empty() {
            return Ok(());
        }

        let text = render_agents_context_message(self.discovered_agents_files.iter());
        self.store
            .append_user_message(session_id.to_owned(), text)
            .map_err(HarnessError::from)?;

        Ok(())
    }

    fn send_prompt_to_agent(&mut self, session_id: &str) -> SessionPromptId {
        let tree = self.store.session(session_id);
        let messages = tree.map(assemble_conversation).unwrap_or_default();
        let tools = self.gather_tool_definitions();
        let session_prompt_id: SessionPromptId =
            format!("sp-{}", self.next_session_prompt_id).into();
        self.next_session_prompt_id += 1;
        self.prompt_sessions
            .insert(session_prompt_id.clone(), session_id.into());

        // Publish SessionPromptCreated — both the agent and UI see it.
        let model = if self.selected_model.is_empty() {
            None
        } else {
            Some(self.selected_model.clone())
        };
        let event = Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: session_prompt_id.clone(),
            session_id: session_id.into(),
            system_prompt: build_system_prompt(&tools, &self.discovered_skills),
            messages,
            tools,
            model,
        });
        self.publish_event(None, event);

        session_prompt_id
    }

    fn gather_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.registry
            .all_tools()
            .into_iter()
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: spec.description.clone(),
                parameters: spec.parameters.clone(),
            })
            .collect()
    }

    fn handle_agent_response_finished(
        &mut self,
        response: AgentResponseFinished,
    ) -> Result<(), HarnessError> {
        // Dedupe: under at-least-once delivery the agent may resend a
        // finished-response after a reconnect. The first delivery removed
        // the entry from `prompt_sessions`; later ones must be ignored
        // rather than fall through to the "default" session fallback,
        // which would silently misroute the duplicate.
        let Some(session_id) = self
            .prompt_sessions
            .remove(response.session_prompt_id.as_str())
        else {
            self.emit_info(&format!(
                "discarding duplicate agent response for session_prompt_id={}",
                response.session_prompt_id
            ));
            return Ok(());
        };
        self.completed_prompts
            .insert(response.session_prompt_id.clone());

        // Persist agent text if present.
        if let Some(ref text) = response.text {
            self.store
                .append_agent_message(&*session_id, text.clone())?;
        }

        if !response.tool_calls.is_empty() {
            // Tool calls to execute — agent stays busy. After all
            // tools complete, maybe_complete_agent_turn will send
            // a new prompt with the results.
            //
            // Future: check the steering queue here and inject any
            // steering messages into the next prompt alongside the
            // tool results, allowing the user to redirect the agent
            // mid-turn.
            let call_ids: Vec<ToolCallId> = response
                .tool_calls
                .iter()
                .map(|c| c.id.clone().into())
                .collect();
            self.turn_state = TurnState::ToolsRunning {
                session_id: session_id.clone(),
                remaining_calls: call_ids,
            };
            // Classify each call and enqueue in the order the agent
            // emitted them. Dispatch is done by `drain_pending_tool_invocations`,
            // which respects the pure-vs-mutating ordering rule.
            for call in &response.tool_calls {
                let kind = self.resolve_tool_kind(&call.name);
                self.pending_tool_invocations
                    .push_back((session_id.clone(), call.clone(), kind));
            }
            self.drain_pending_tool_invocations()?;
        } else {
            // No tool calls — turn is done. Dispatch next queued
            // prompt if any, otherwise mark agent as idle.
            self.dispatch_next_or_idle(&session_id);
        }

        Ok(())
    }

    /// Advances the front of the prompt queue when possible.
    ///
    /// Session initialization happens before prompt dispatch, so a fresh
    /// `chat-*` session can discover AGENTS.md and skills before the
    /// agent sees the first user message.
    fn try_advance_queue(&mut self) {
        if !self.turn_state.is_idle() || !self.extensions_all_ready() {
            return;
        }

        let Some((session_id, _)) = self.pending_prompts.front() else {
            return;
        };
        let session_id = session_id.clone();

        if !self.session_initialized(&session_id) {
            self.start_session_init(session_id);
            return;
        }

        if self.selected_model.is_empty() {
            return;
        }

        if let Some((session_id, text)) = self.pending_prompts.pop_front() {
            if let Err(error) = self.dispatch_user_prompt(session_id, text) {
                self.emit_info(&format!("failed to dispatch queued prompt: {error}"));
                self.turn_state = TurnState::Idle;
            }
        }
    }

    /// True when a fresh user prompt should *not* be sent to the agent.
    ///
    /// Three conditions can block dispatch:
    /// - no model selected (handled by the existing /model UI flow);
    /// - the agent is mid-turn (`turn_state != Idle`);
    /// - some configured extension is not in `ExtensionState::Ready`.
    ///
    /// In-flight turns are *not* affected — only fresh dispatch.
    fn dispatch_blocked(&self) -> bool {
        self.selected_model.is_empty() || !self.turn_state.is_idle() || !self.extensions_all_ready()
    }

    /// True iff every configured extension has either reached `Ready`
    /// or dropped permanently.
    ///
    /// `Disconnected` counts as "no longer blocking": a dead extension
    /// will never reach `Ready` on its own (auto-respawn is not wired
    /// up), and we don't want it to wedge fresh prompt dispatch.
    /// Session initialization for a still-live session with a dead
    /// provider still completes correctly — `handle_disconnect`
    /// removes the entry from the `waiting_on` set.
    fn extensions_all_ready(&self) -> bool {
        self.extensions.iter().all(|e| {
            matches!(
                e.state,
                ExtensionState::Ready | ExtensionState::Disconnected
            )
        })
    }

    /// Update an extension's lifecycle state, looked up by connection id.
    /// No-op if no entry matches (e.g. for socket clients).
    fn set_extension_state(&mut self, connection_id: &str, new_state: ExtensionState) {
        if let Some(entry) = self
            .extensions
            .iter_mut()
            .find(|e| e.connection_id.as_str() == connection_id)
        {
            entry.state = new_state;
        }
    }

    /// Dispatches the next queued prompt or marks the agent as idle.
    fn dispatch_next_or_idle(&mut self, _completed_session_id: &str) {
        self.turn_state = TurnState::Idle;
        self.try_advance_queue();
    }

    /// Returns the side-effect class of a tool name.
    ///
    /// Falls back to `Mutating` for unknown tools so an unregistered
    /// name does not accidentally parallelize.
    fn resolve_tool_kind(&self, name: &str) -> tau_proto::ToolSideEffects {
        self.registry
            .resolve_provider(name)
            .map(|provider| provider.tool.side_effects)
            .unwrap_or(tau_proto::ToolSideEffects::Mutating)
    }

    /// Whether any currently in-flight tool call is `Mutating`.
    fn has_mutating_in_flight(&self) -> bool {
        self.in_flight_tool_kinds
            .values()
            .any(|kind| matches!(kind, tau_proto::ToolSideEffects::Mutating))
    }

    /// State-machine drain: dispatch queued tool invocations in FIFO
    /// order while the in-flight set allows them through.
    ///
    /// Rule:
    /// - `Pure` head may dispatch when no `Mutating` is in-flight.
    /// - `Mutating` head may dispatch when the in-flight set is empty.
    ///
    /// Because the queue is FIFO and new calls are only enqueued from
    /// `handle_agent_response_finished` (one agent turn at a time),
    /// this gives the agent a sequential read-after-write view even
    /// though individual `Pure` calls still run concurrently.
    ///
    /// Call this after enqueuing new work or after any in-flight call
    /// completes.
    fn drain_pending_tool_invocations(&mut self) -> Result<(), HarnessError> {
        while let Some((_, _, kind)) = self.pending_tool_invocations.front() {
            let compatible = match *kind {
                tau_proto::ToolSideEffects::Pure => !self.has_mutating_in_flight(),
                tau_proto::ToolSideEffects::Mutating => self.in_flight_tool_kinds.is_empty(),
            };
            if !compatible {
                break;
            }
            let (session_id, call, kind) = self
                .pending_tool_invocations
                .pop_front()
                .expect("front just peeked");
            let call_id: ToolCallId = call.id.clone().into();
            self.in_flight_tool_kinds.insert(call_id.clone(), kind);
            // If dispatch fails synchronously, roll back the in-flight
            // entry so a retry or clean-up is not wedged on a phantom
            // slot.
            if let Err(error) = self.execute_agent_tool_call(&session_id, &call) {
                self.in_flight_tool_kinds.remove(&call_id);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Hook called whenever a tool call has finished (result, error,
    /// synthetic NoProvider error, or inline skill completion). Removes
    /// it from the in-flight set, drains any freshly-eligible queued
    /// calls, and then checks whether the turn is done.
    fn on_tool_call_complete(&mut self, call_id: &str) {
        let owned: ToolCallId = call_id.to_owned().into();
        self.in_flight_tool_kinds.remove(&owned);
        if let Err(error) = self.drain_pending_tool_invocations() {
            self.emit_info(&format!("queued tool dispatch failed: {error}"));
        }
        self.maybe_complete_agent_turn(call_id);
    }

    fn maybe_complete_agent_turn(&mut self, completed_call_id: &str) {
        let should_send = if let TurnState::ToolsRunning {
            remaining_calls, ..
        } = &mut self.turn_state
        {
            remaining_calls.retain(|id| id != completed_call_id);
            remaining_calls.is_empty()
        } else {
            false
        };
        if should_send {
            let session_id = if let TurnState::ToolsRunning { session_id, .. } = &self.turn_state {
                session_id.clone()
            } else {
                unreachable!("just checked")
            };
            self.turn_state = TurnState::AgentThinking {
                _session_id: session_id.clone(),
            };
            self.send_prompt_to_agent(&session_id);
        }
    }

    fn execute_agent_tool_call(
        &mut self,
        session_id: &str,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        // Handle harness-owned tools directly.
        if call.name == "skill" {
            return self.handle_skill_tool_call(session_id, call);
        }

        let call_id: ToolCallId = call.id.clone().into();

        // Persist the request.
        self.store.append_tool_activity(
            session_id,
            ToolActivityRecord {
                call_id: call_id.clone(),
                tool_name: call.name.clone().into(),
                outcome: ToolActivityOutcome::Requested {
                    arguments: call.arguments.clone(),
                },
            },
        )?;

        // Route to tool provider.
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: call.name.clone().into(),
            arguments: call.arguments.clone(),
        };

        // Track which session this call belongs to.
        self.pending_tool_sessions
            .insert(call_id.clone(), session_id.into());

        match self
            .registry
            .route_tool_request(&mut self.bus, &self.agent_connection_id, request)
        {
            Ok(_) => {}
            Err(ToolRouteError::NoProvider { tool_name }) => {
                let error = ToolError {
                    call_id: call_id.clone(),
                    tool_name,
                    message: "no live provider available".to_owned(),
                    details: None,
                };
                self.persist_tool_error(&error)?;
                // Mark this call as completed so the turn can proceed.
                self.on_tool_call_complete(&call.id);
            }
            Err(error) => return Err(HarnessError::ToolRoute(error)),
        }

        Ok(())
    }

    /// Register harness-owned tools (e.g. `skill`).
    fn register_harness_tools(&mut self) {
        let _ = self.registry.register(
            HARNESS_CONNECTION_ID,
            tau_proto::ToolSpec {
                name: "skill".into(),
                description: Some(
                    "Load a skill's full content by name. Use this when a task \
                     matches an available skill's description."
                        .to_owned(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Name of the skill to load"
                        }
                    },
                    "required": ["name"]
                })),
                side_effects: tau_proto::ToolSideEffects::Pure,
            },
        );
    }

    /// Handle the harness-owned `skill` tool call inline.
    fn handle_skill_tool_call(
        &mut self,
        session_id: &str,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone().into();
        let tool_name: ToolName = "skill".into();

        // Persist the request and track the session mapping.
        self.store.append_tool_activity(
            session_id,
            ToolActivityRecord {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                outcome: ToolActivityOutcome::Requested {
                    arguments: call.arguments.clone(),
                },
            },
        )?;
        self.pending_tool_sessions
            .insert(call_id.clone(), session_id.into());

        // Extract the skill name from arguments.
        let skill_name = cbor_map_text(&call.arguments, "name");

        let result_event = match skill_name {
            Some(name) => match self.discovered_skills.get(name) {
                Some(skill) => match std::fs::read_to_string(&skill.file_path) {
                    Ok(content) => {
                        let body = tau_skills::strip_frontmatter(&content);
                        Event::ToolResult(tau_proto::ToolResult {
                            call_id: call_id.clone(),
                            tool_name: tool_name.clone(),
                            result: CborValue::Map(vec![
                                (
                                    CborValue::Text("name".to_owned()),
                                    CborValue::Text(name.to_owned()),
                                ),
                                (
                                    CborValue::Text("content".to_owned()),
                                    CborValue::Text(body.to_owned()),
                                ),
                            ]),
                        })
                    }
                    Err(e) => Event::ToolError(tau_proto::ToolError {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        message: format!("failed to read skill file: {e}"),
                        details: None,
                    }),
                },
                None => Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    message: format!("unknown skill: {name}"),
                    details: None,
                }),
            },
            None => Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: "missing required argument: name".to_owned(),
                details: None,
            }),
        };

        // Persist, publish, and complete the tool call.
        match &result_event {
            Event::ToolResult(r) => self.persist_tool_result(r)?,
            Event::ToolError(e) => self.persist_tool_error(e)?,
            _ => {}
        }
        self.publish_event(None, result_event);
        self.on_tool_call_complete(&call.id);

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn send_user_message(
        &mut self,
        session_id: &str,
        text: &str,
        _source_id: Option<&str>,
    ) -> Result<InteractionOutcome, HarnessError> {
        // Synchronous test entrypoint: dispatch directly without going
        // through `submit_user_prompt`'s queue. The embedded test harness
        // has no model configured (nothing to select from) and no UI to
        // drain a queued prompt, so the queued-until-model path would
        // deadlock. AGENTS.md session init is exercised separately in
        // unit tests via `submit_user_prompt` / manual turn-state setup.
        self.dispatch_user_prompt(session_id.into(), text.to_owned())?;

        let started_at = Instant::now();
        let mut progress_messages = Vec::new();
        loop {
            let remaining = RESPONSE_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let event = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::ResponseTimeout)?;
            self.log_event(&event);
            match event {
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    if let Event::ToolProgress(ref progress) = event {
                        progress_messages.push(format_tool_progress(progress));
                    }
                    let is_final = matches!(
                        &event,
                        Event::AgentResponseFinished(r) if r.tool_calls.is_empty()
                    );
                    let final_text = if let Event::AgentResponseFinished(ref r) = event {
                        r.text.clone()
                    } else {
                        None
                    };
                    self.handle_extension_event(&connection_id, event)?;
                    if is_final {
                        return Ok(InteractionOutcome {
                            lifecycle_messages: Vec::new(),
                            progress_messages,
                            response: final_text.unwrap_or_default(),
                        });
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let is_agent = connection_id == self.agent_connection_id;
                    self.handle_disconnect(&connection_id);
                    if is_agent {
                        return Err(HarnessError::Participant("agent disconnected".to_owned()));
                    }
                }
                HarnessEvent::NewClient(_) => {}
            }
        }
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    fn shutdown(&mut self) -> Result<(), HarnessError> {
        // Disconnect all extensions from the bus.  Dropping the
        // ChannelSink closes the writer channel, which triggers each
        // writer thread's shutdown sequence (send disconnect, close
        // stdin, wait/kill child).
        for ext in &self.extensions {
            let _ = self.bus.disconnect(&ext.connection_id);
        }

        // Join in-process extension threads.
        for i in 0..self.extensions.len() {
            if let Some(handle) = self.extensions[i].in_process_thread.take() {
                let name = self.extensions[i].name.clone();
                let result = handle.join().map_err(|_| HarnessError::ThreadJoin(name))?;
                result.map_err(HarnessError::Participant)?;
            }
            let name = self.extensions[i].name.clone();
            self.emit_extension_exited(&name);
        }
        Ok(())
    }

    #[cfg(test)]
    fn extension_connection_id(&self, name: &str) -> Option<&str> {
        self.extensions
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.connection_id.as_str())
    }
}

// ---------------------------------------------------------------------------
// Extension spawning
// ---------------------------------------------------------------------------

fn spawn_in_process<F>(
    name: &str,
    kind: ClientKind,
    run: F,
    bus: &mut EventBus,
    tx: &Sender<HarnessEvent>,
) -> Result<(tau_proto::ConnectionId, JoinHandle<Result<(), String>>), HarnessError>
where
    F: FnOnce(UnixStream, UnixStream) -> Result<(), String> + Send + 'static,
{
    // Two unidirectional pairs so dropping one end cleanly EOFs the
    // other — no shared clones keeping the socket alive.
    let (ext_read, harness_write) = UnixStream::pair()?; // harness → extension
    let (harness_read, ext_write) = UnixStream::pair()?; // extension → harness

    let writer_tx = spawn_writer_thread(harness_write, WriterShutdown::CloseStream);
    let conn_id = bus.connect(Connection::new(
        ConnectionMetadata {
            id: tau_proto::ConnectionId::default(),
            name: name.to_owned(),
            kind,
            origin: ConnectionOrigin::Supervised,
        },
        Box::new(ChannelSink { tx: writer_tx }),
    ));

    spawn_reader_thread(conn_id.clone(), harness_read, tx.clone());

    let thread = thread::spawn(move || run(ext_read, ext_write));
    Ok((conn_id, thread))
}

fn spawn_supervised(
    config: &ExtensionConfig,
    kind: ClientKind,
    bus: &mut EventBus,
    tx: &Sender<HarnessEvent>,
) -> Result<(tau_proto::ConnectionId, u32), HarnessError> {
    let mut child = Command::new(&config.command)
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(HarnessError::Io)?;

    let child_pid = child.id();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Participant("missing stdin".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Participant("missing stdout".to_owned()))?;

    let writer_tx = spawn_writer_thread(stdin, WriterShutdown::KillChild(child));
    let conn_id = bus.connect(Connection::new(
        ConnectionMetadata {
            id: tau_proto::ConnectionId::default(),
            name: config.name.clone(),
            kind,
            origin: ConnectionOrigin::Supervised,
        },
        Box::new(ChannelSink { tx: writer_tx }),
    ));

    spawn_reader_thread(conn_id.clone(), stdout, tx.clone());

    Ok((conn_id, child_pid))
}

/// Load model registry and harness settings, build the flat model list
/// and determine the initially selected model.
///
/// Priority: default_model from harness.json5 → last used from state →
/// first available → empty (no model).
fn load_model_list(dirs: &tau_config::settings::TauDirs) -> (Vec<ModelId>, ModelId) {
    let model_registry = tau_config::settings::load_models_in(dirs).unwrap_or_default();
    let harness_settings = tau_config::settings::load_harness_settings_in(dirs).unwrap_or_default();
    let mut available: Vec<ModelId> = Vec::new();
    for (provider_name, provider_cfg) in &model_registry.providers {
        for model in &provider_cfg.models {
            available.push(format!("{provider_name}/{}", model.id).into());
        }
    }
    available.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let selected = harness_settings
        .default_model
        .filter(|m| available.iter().any(|a| **a == **m))
        .map(ModelId::from)
        .or_else(|| {
            load_last_selected_model(dirs)
                .filter(|m| available.iter().any(|a| **a == **m))
                .map(ModelId::from)
        })
        .or_else(|| available.first().cloned())
        .unwrap_or_default();
    (available, selected)
}

/// Load the last-selected model from `<state_dir>/harness-state.json`.
fn load_last_selected_model(dirs: &tau_config::settings::TauDirs) -> Option<String> {
    let path = dirs.state_dir.as_ref()?.join("harness-state.json");
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json["last_selected_model"].as_str().map(String::from)
}

/// Persist the last-selected model to `<state_dir>/harness-state.json`.
fn save_last_selected_model(dirs: &tau_config::settings::TauDirs, model: &str) {
    let Some(dir) = dirs.state_dir.as_ref() else {
        return;
    };
    let path = dir.join("harness-state.json");
    let _ = std::fs::create_dir_all(dir);
    let json = serde_json::json!({ "last_selected_model": model });
    let _ = serde_json::to_string_pretty(&json)
        .ok()
        .and_then(|s| std::fs::write(&path, s).ok());
}

/// Builds the system prompt from available tools, skills, and environment.
fn build_system_prompt(
    tools: &[ToolDefinition],
    skills: &std::collections::HashMap<String, DiscoveredSkill>,
) -> String {
    let mut prompt = String::from(
        "You are an expert coding assistant operating inside tau, \
         a coding agent harness. You help users by reading files, \
         executing commands, editing code, and writing new files.\n\n",
    );

    // Available tools section.
    if !tools.is_empty() {
        prompt.push_str("Available tools:\n");
        for tool in tools {
            let desc = tool.description.as_deref().unwrap_or("(no description)");
            prompt.push_str(&format!("- {}: {desc}\n", tool.name));
        }
        prompt.push('\n');
    }

    // Guidelines.
    prompt.push_str(
        "Guidelines:\n\
         - Be concise in your responses.\n\
         - Show file paths clearly when working with files.\n\
         - When asked to read a file, use the read tool.\n\
         - When asked to run a command, use the bash tool.\n",
    );

    // Available skills section.
    let prompt_skills: Vec<_> = skills.iter().filter(|(_, s)| s.add_to_prompt).collect();
    if !prompt_skills.is_empty() {
        prompt.push_str(
            "\nThe following skills provide specialized instructions for specific tasks.\n\
             Use the skill tool to load a skill when the task matches its description.\n\n\
             <available_skills>\n",
        );
        for (name, skill) in &prompt_skills {
            prompt.push_str(&format!(
                "  <skill>\n    <name>{name}</name>\n    \
                 <description>{}</description>\n  </skill>\n",
                skill.description
            ));
        }
        prompt.push_str("</available_skills>\n");
    }

    // Date and CWD.
    let now = chrono_free_date();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".to_owned());
    prompt.push_str(&format!("\nCurrent date: {now}\n"));
    prompt.push_str(&format!("Current working directory: {cwd}\n"));

    prompt
}

fn render_agents_context_message<'a>(
    files: impl IntoIterator<Item = &'a DiscoveredAgentsFile>,
) -> String {
    let mut text = String::from(
        "# AGENTS.md instructions\n\n\
The following instructions were loaded from AGENTS.md files.\n\
More specific files usually override broader ones.\n\n",
    );

    for file in files {
        text.push_str(&format!(
            "<AGENTS_FILE path=\"{}\">\n",
            file.file_path.display()
        ));
        text.push_str(&file.content);
        if !file.content.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("</AGENTS_FILE>\n\n");
    }

    text
}

/// Returns the current date as YYYY-MM-DD without chrono.
fn chrono_free_date() -> String {
    // Use UNIX timestamp to derive date.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    // Simple days-since-epoch to Y-M-D (good enough, no leap second edge cases).
    let mut y = 1970_i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for md in &month_days {
        if remaining < *md {
            break;
        }
        remaining -= md;
        m += 1;
    }
    format!("{y}-{:02}-{:02}", m + 1, remaining + 1)
}

/// Converts a session tree's current branch into LLM conversation
/// messages.
fn assemble_conversation(tree: &tau_core::SessionTree) -> Vec<ConversationMessage> {
    let mut messages: Vec<ConversationMessage> = Vec::new();

    for entry in tree.current_branch() {
        match entry {
            SessionEntry::UserMessage { text } => {
                messages.push(ConversationMessage {
                    role: ConversationRole::User,
                    content: vec![ContentBlock::Text { text: text.clone() }],
                });
            }
            SessionEntry::AgentMessage { text } => {
                messages.push(ConversationMessage {
                    role: ConversationRole::Assistant,
                    content: vec![ContentBlock::Text { text: text.clone() }],
                });
            }
            SessionEntry::ToolActivity(activity) => match &activity.outcome {
                ToolActivityOutcome::Requested { arguments } => {
                    // Tool use goes into the preceding assistant message.
                    // If there's no assistant message yet, create one.
                    let needs_new = messages
                        .last()
                        .is_none_or(|m| m.role != ConversationRole::Assistant);
                    if needs_new {
                        messages.push(ConversationMessage {
                            role: ConversationRole::Assistant,
                            content: Vec::new(),
                        });
                    }
                    if let Some(last) = messages.last_mut() {
                        last.content.push(ContentBlock::ToolUse {
                            id: activity.call_id.to_string(),
                            name: activity.tool_name.to_string(),
                            input: arguments.clone(),
                        });
                    }
                }
                ToolActivityOutcome::Result { result } => {
                    messages.push(ConversationMessage {
                        role: ConversationRole::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: activity.call_id.to_string(),
                            content: cbor_to_text(result),
                            is_error: false,
                        }],
                    });
                }
                ToolActivityOutcome::Error { message, .. } => {
                    messages.push(ConversationMessage {
                        role: ConversationRole::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: activity.call_id.to_string(),
                            content: message.clone(),
                            is_error: true,
                        }],
                    });
                }
            },
        }
    }

    messages
}

/// Extract a string value from a CBOR map by key.
fn cbor_map_text<'a>(map: &'a CborValue, key: &str) -> Option<&'a str> {
    match map {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Text(v)) if k == key => Some(v.as_str()),
            _ => None,
        }),
        _ => None,
    }
}

/// Converts a CBOR value to human-readable text for tool results.
fn cbor_to_text(v: &tau_proto::CborValue) -> String {
    use tau_proto::CborValue;
    match v {
        CborValue::Null => String::new(),
        CborValue::Bool(b) => b.to_string(),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            n.to_string()
        }
        CborValue::Float(f) => f.to_string(),
        CborValue::Text(s) => s.clone(),
        CborValue::Bytes(b) => format!("<{} bytes>", b.len()),
        CborValue::Array(arr) => arr.iter().map(cbor_to_text).collect::<Vec<_>>().join("\n"),
        CborValue::Map(entries) => {
            // For maps, extract text values cleanly.
            let mut parts = Vec::new();
            for (k, val) in entries {
                let key = match k {
                    CborValue::Text(s) => s.clone(),
                    other => cbor_to_text(other),
                };
                let value = cbor_to_text(val);
                if value.contains('\n') {
                    parts.push(format!("{key}:\n{value}"));
                } else {
                    parts.push(format!("{key}: {value}"));
                }
            }
            parts.join("\n")
        }
        CborValue::Tag(_, inner) => cbor_to_text(inner),
        _ => String::new(),
    }
}

fn selector_matches_event(selectors: &[EventSelector], event: &Event) -> bool {
    // Match against the inner event for log deliveries (see the
    // matching helper in tau-core for the same reasoning).
    let target_name = match event {
        Event::LogEvent(env) => env.event.name(),
        _ => event.name(),
    };
    let name = target_name.as_str();
    selectors.iter().any(|selector| match selector {
        EventSelector::Exact(expected) => *expected == target_name,
        EventSelector::Prefix(prefix) => name.starts_with(prefix),
    })
}

fn bind_listener(path: &Path) -> Result<UnixListener, HarnessError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path).map_err(HarnessError::from)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Formats a tool progress event for display.
#[must_use]
pub fn format_tool_progress(progress: &ToolProgress) -> String {
    let mut text = progress.tool_name.to_string();
    if let Some(message) = &progress.message {
        text.push_str(": ");
        text.push_str(message);
    }
    if let Some(ProgressUpdate {
        current: Some(current),
        total: Some(total),
    }) = &progress.progress
    {
        text.push_str(&format!(" ({current}/{total})"));
    }
    text
}

/// Formats an extension lifecycle event for display.
#[must_use]
pub fn format_extension_event(event: &Event) -> String {
    match event {
        Event::ExtensionStarting(s) => format!("extension {} starting", s.extension_name),
        Event::ExtensionReady(r) => format!("extension {} ready", r.extension_name),
        Event::ExtensionExited(e) => format!("extension {} exited", e.extension_name),
        Event::ExtensionRestarting(r) => format!("extension {} restarting", r.extension_name),
        _ => event.name().to_string(),
    }
}

fn format_session_entry(entry: &SessionEntry) -> String {
    match entry {
        SessionEntry::UserMessage { text } => format!("user: {text}"),
        SessionEntry::AgentMessage { text } => format!("agent: {text}"),
        SessionEntry::ToolActivity(a) => match &a.outcome {
            ToolActivityOutcome::Requested { .. } => {
                format!("tool.request {} ({})", a.tool_name, a.call_id)
            }
            ToolActivityOutcome::Result { result } => {
                let text = cbor_to_text(result);
                let preview = if text.len() > 80 {
                    format!("{}...", &text[..80])
                } else {
                    text
                };
                format!("tool.result {} ({}) -> {preview}", a.tool_name, a.call_id)
            }
            ToolActivityOutcome::Error { message, .. } => {
                format!("tool.error {} ({}) -> {message}", a.tool_name, a.call_id)
            }
        },
    }
}

fn latest_agent_preview(session: &tau_core::SessionTree) -> Option<String> {
    session
        .current_branch()
        .into_iter()
        .rev()
        .find_map(|e| match e {
            SessionEntry::AgentMessage { text } => Some(text.clone()),
            _ => None,
        })
}

// ---------------------------------------------------------------------------
// Public API — default config
// ---------------------------------------------------------------------------

/// Returns a default configuration that spawns built-in components via
/// `tau component <name>`.
#[must_use]
pub fn default_config() -> Config {
    use tau_config::{Config, CoreConfig, CoreMode, ExtensionConfig};

    let tau_binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "tau".to_owned());

    Config {
        core: CoreConfig {
            mode: CoreMode::Embedded,
        },
        extensions: vec![
            ExtensionConfig {
                name: "agent".to_owned(),
                command: tau_binary.clone(),
                args: vec!["component".to_owned(), "agent".to_owned()],
                role: Some("agent".to_owned()),
            },
            ExtensionConfig {
                name: "tools".to_owned(),
                command: tau_binary,
                args: vec!["component".to_owned(), "ext-fs".to_owned()],
                role: Some("tool".to_owned()),
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Public API — in-process (test-only)
// ---------------------------------------------------------------------------

/// Options for a one-shot embedded run.
#[derive(Clone, Debug, Default, Eq, PartialEq, bon::Builder)]
pub struct EmbeddedOptions {
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
}

/// Runs one embedded interaction and returns progress plus the final
/// agent response.
pub fn run_embedded_message_with_trace(
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    run_embedded_message_impl(
        session_store_path,
        session_id,
        message,
        default_agent_runner,
        EmbeddedOptions::default(),
    )
}

/// Runs one embedded interaction and returns the final agent response.
pub fn run_embedded_message(
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, HarnessError> {
    Ok(run_embedded_message_with_trace(session_store_path, session_id, message)?.response)
}

/// Like [`run_embedded_message_with_trace`] but lets the caller override
/// directory layout and other options.
pub fn run_embedded_message_with_options(
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
    options: EmbeddedOptions,
) -> Result<InteractionOutcome, HarnessError> {
    run_embedded_message_impl(
        session_store_path,
        session_id,
        message,
        default_agent_runner,
        options,
    )
}

/// Like [`run_embedded_message_with_trace`] but uses the echo agent for
/// testing.
pub fn run_embedded_message_with_echo(
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_agent::run_echo(r, w).map_err(|e| e.to_string())
    }
    run_embedded_message_impl(
        session_store_path,
        session_id,
        message,
        echo_runner,
        EmbeddedOptions::default(),
    )
}

fn run_embedded_message_impl(
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
    agent_runner: AgentRunner,
    options: EmbeddedOptions,
) -> Result<InteractionOutcome, HarnessError> {
    let session_store_path = session_store_path.into();
    let dirs = options.dirs.unwrap_or_default();
    let mut harness = Harness::new_with_agent(
        session_store_path.clone(),
        default_policy_store_path_from(&session_store_path),
        dirs,
        agent_runner,
        true,
        None,
    )?;
    let mut outcome = harness.send_user_message(session_id, message, None)?;
    harness.shutdown()?;
    outcome.lifecycle_messages = harness.lifecycle_messages;
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Public API — daemon
// ---------------------------------------------------------------------------

/// Runs a foreground daemon that accepts socket clients.
pub fn run_daemon(
    socket_path: impl Into<PathBuf>,
    session_store_path: impl Into<PathBuf>,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let socket_path = socket_path.into();
    let session_store_path = session_store_path.into();
    let listener = bind_listener(&socket_path)?;
    let policy_store_path = options
        .policy_store_path
        .clone()
        .unwrap_or_else(|| default_policy_store_path_from(&session_store_path));
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness = Harness::new(session_store_path, policy_store_path, dirs)?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients);
    let _ = harness.shutdown();
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Runs a foreground daemon using extensions from configuration.
pub fn run_daemon_with_config(
    config: &Config,
    socket_path: impl Into<PathBuf>,
    session_store_path: impl Into<PathBuf>,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let socket_path = socket_path.into();
    let session_store_path = session_store_path.into();
    let listener = bind_listener(&socket_path)?;
    let policy_store_path = options
        .policy_store_path
        .clone()
        .unwrap_or_else(|| default_policy_store_path_from(&session_store_path));
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness =
        Harness::from_config(config, session_store_path, policy_store_path, dirs, None)?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients);
    let _ = harness.shutdown();
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Sends one user message to a running daemon and returns progress
/// plus the final response.
pub fn send_daemon_message_with_trace(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    let mut peer = SocketPeer::connect(socket_path)?;
    peer.send(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-cli".to_owned(),
        client_kind: ClientKind::Ui,
    }))?;
    peer.send(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Prefix("agent.".to_owned()),
            EventSelector::Prefix("session.".to_owned()),
            EventSelector::Prefix("tool.".to_owned()),
            EventSelector::Prefix("extension.".to_owned()),
            EventSelector::Prefix("harness.".to_owned()),
        ],
    }))?;
    peer.send(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: session_id.into(),
        text: message.to_owned(),
    }))?;

    let started_at = Instant::now();
    let mut lifecycle_messages = Vec::new();
    let mut progress_messages = Vec::new();
    loop {
        if RESPONSE_TIMEOUT <= started_at.elapsed() {
            return Err(HarnessError::ResponseTimeout);
        }
        if let Some(event) = peer.recv_timeout(RESPONSE_TIMEOUT)? {
            // UI clients don't ack — they just consume the inner event.
            let (_log_id, event) = event.peel_log();
            match event {
                Event::ToolProgress(p) => progress_messages.push(format_tool_progress(&p)),
                Event::HarnessInfo(ref info) => {
                    lifecycle_messages.push(info.message.clone());
                }
                Event::ExtensionStarting(_)
                | Event::ExtensionReady(_)
                | Event::ExtensionExited(_)
                | Event::ExtensionRestarting(_) => {
                    lifecycle_messages.push(format_extension_event(&event));
                }
                Event::AgentResponseFinished(finished) if finished.tool_calls.is_empty() => {
                    peer.send(&Event::LifecycleDisconnect(LifecycleDisconnect {
                        reason: Some("done".to_owned()),
                    }))?;
                    return Ok(InteractionOutcome {
                        lifecycle_messages,
                        progress_messages,
                        response: finished.text.unwrap_or_default(),
                    });
                }
                Event::LifecycleDisconnect(d) => {
                    return Err(HarnessError::Participant(
                        d.reason.unwrap_or_else(|| "daemon disconnected".to_owned()),
                    ));
                }
                _ => {}
            }
        }
    }
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

// ---------------------------------------------------------------------------
// Public API — harness daemon with runtime directory
// ---------------------------------------------------------------------------

/// Runs the harness daemon with runtime directory management.
pub fn run_harness_daemon(
    project_root: &Path,
    config: &Config,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    let daemon_dir = runtime_dir::prepare_daemon_dir(project_root)?;
    let listener = bind_listener(&daemon_dir.socket_path())?;

    let session_store_path = project_root.join(".tau").join("sessions.cbor");
    let policy_store_path = options
        .policy_store_path
        .clone()
        .unwrap_or_else(|| project_root.join(".tau").join("policy.cbor"));

    let dirs = options.dirs.clone().unwrap_or_default();
    let log_dir = project_root.join(".tau");
    let mut harness = Harness::from_config(
        config,
        &session_store_path,
        &policy_store_path,
        dirs,
        Some(&log_dir),
    )?;
    harness.emit_info(&format!(
        "event log: {}",
        log_dir.join("events.jsonl").display()
    ));

    // Write marker AFTER extensions are ready.
    daemon_dir.write_marker()?;
    daemon_dir.write_pid()?;

    let tx = harness.tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                return;
            }
        }
    });

    let result = harness.run_event_loop(options.max_clients);
    let _ = harness.shutdown();
    daemon_dir.cleanup();
    result
}

/// Entrypoint for `tau component harness`.
pub fn run_component() -> Result<(), Box<dyn std::error::Error>> {
    let project_root = std::env::current_dir()?;
    let config = resolve_config(None)?;
    run_harness_daemon(
        &project_root,
        &config,
        ServeOptions {
            max_clients: Some(1),
            ..Default::default()
        },
    )
    .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

#[must_use]
pub fn default_session_store_path() -> PathBuf {
    PathBuf::from(".tau").join("sessions.cbor")
}

#[must_use]
pub fn default_policy_store_path() -> PathBuf {
    PathBuf::from(".tau").join("policy.cbor")
}

fn default_policy_store_path_from(session_store_path: &Path) -> PathBuf {
    session_store_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("policy.cbor")
}

#[must_use]
pub fn default_session_id() -> &'static str {
    "default"
}

// ---------------------------------------------------------------------------
// Inspection helpers
// ---------------------------------------------------------------------------

pub fn open_session_store(path: impl AsRef<Path>) -> Result<SessionStore, HarnessError> {
    SessionStore::open(path.as_ref()).map_err(HarnessError::from)
}

pub fn session_lines(
    path: impl AsRef<Path>,
    session_id: &str,
) -> Result<Vec<String>, HarnessError> {
    let store = open_session_store(path)?;
    let Some(tree) = store.session(session_id) else {
        return Ok(vec![format!("session {session_id} not found")]);
    };
    Ok(tree
        .current_branch()
        .into_iter()
        .enumerate()
        .map(|(i, e)| format!("{}: {}", i + 1, format_session_entry(e)))
        .collect())
}

pub fn session_list_lines(path: impl AsRef<Path>) -> Result<Vec<String>, HarnessError> {
    let store = open_session_store(path)?;
    let mut sessions = store.sessions();
    sessions.sort_by(|a, b| a.session_id().cmp(b.session_id()));
    if sessions.is_empty() {
        return Ok(vec!["no sessions".to_owned()]);
    }
    Ok(sessions
        .into_iter()
        .map(|s| {
            let branch = s.current_branch();
            format!(
                "{} ({} entries){}",
                s.session_id(),
                branch.len(),
                latest_agent_preview(s)
                    .map(|p| format!(": {p}"))
                    .unwrap_or_default()
            )
        })
        .collect())
}

pub fn open_policy_store(path: impl AsRef<Path>) -> Result<PolicyStore, HarnessError> {
    PolicyStore::open(path.as_ref()).map_err(HarnessError::from)
}

pub fn policy_lines(path: impl AsRef<Path>) -> Result<Vec<String>, HarnessError> {
    let store = open_policy_store(path)?;
    let mut approvals = store.approvals().to_vec();
    approvals.sort_by(|a, b| a.connection_name.cmp(&b.connection_name));
    if approvals.is_empty() {
        return Ok(vec!["no policy approvals".to_owned()]);
    }
    Ok(approvals
        .into_iter()
        .map(|a| {
            let sels = a
                .selectors
                .iter()
                .map(|s| match s {
                    EventSelector::Exact(n) => n.as_str().to_owned(),
                    EventSelector::Prefix(p) => format!("{p}*"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{} [{:?}] -> {sels}",
                a.connection_name, a.connection_origin
            )
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

fn resolve_config(explicit_path: Option<&Path>) -> Result<Config, Box<dyn std::error::Error>> {
    use tau_config::LoadOptions;

    let options = match explicit_path {
        Some(path) => LoadOptions {
            user_config_path: Some(path.to_owned()),
            enable_project_config: false,
            project_config_path: None,
        },
        None => LoadOptions {
            user_config_path: None,
            enable_project_config: true,
            project_config_path: None,
        },
    };

    match tau_config::load(&options) {
        Ok(config) if !config.extensions.is_empty() => Ok(config),
        _ => Ok(default_config()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_agent::run_echo(r, w).map_err(|e| e.to_string())
    }

    fn echo_harness(
        sp: impl Into<PathBuf>,
        pp: impl Into<PathBuf>,
    ) -> Result<Harness, HarnessError> {
        Harness::new_with_agent(
            sp,
            pp,
            tau_config::settings::TauDirs::default(),
            echo_runner,
            true,
            None,
        )
    }

    #[test]
    fn embedded_mode_returns_agent_response_and_persists_history() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let r = run_embedded_message_with_echo(&sp, "s1", "hello")
            .expect("should succeed")
            .response;
        assert!(!r.is_empty(), "response should not be empty: {r:?}");
        let store = open_session_store(&sp).expect("reopen");
        let branch = store.session("s1").expect("session").current_branch();
        assert!(
            branch.len() >= 2,
            "should have user msg + agent response, got {}",
            branch.len()
        );
    }

    #[test]
    #[ignore = "needs echo agent wired into run_daemon"]
    fn daemon_mode_accepts_later_clients() {
        let td = TempDir::new().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let sp = td.path().join("sessions.cbor");

        let server = thread::spawn({
            let sock = sock.clone();
            let sp = sp.clone();
            move || run_daemon(sock, sp, ServeOptions::builder().max_clients(2).build())
        });

        let started = Instant::now();
        while !sock.exists() {
            assert!(started.elapsed() < Duration::from_secs(3), "socket timeout");
            thread::sleep(Duration::from_millis(10));
        }

        let r1 = send_daemon_message(&sock, "s1", "hello").expect("first");
        let r2 = send_daemon_message(&sock, "s1", "again").expect("second");
        assert!(!r1.is_empty(), "response should not be empty");
        assert!(!r2.is_empty(), "response should not be empty");

        server.join().expect("join").expect("daemon clean exit");
        let store = open_session_store(&sp).expect("reopen");
        assert_eq!(
            store.session("s1").expect("session").current_branch().len(),
            8
        );
    }

    #[test]
    fn embedded_mode_can_read_files() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let fp = td.path().join("note.txt");
        std::fs::write(&fp, "hello from disk").expect("write fixture");
        let r = run_embedded_message_with_echo(&sp, "s1", &format!("read {}", fp.display()))
            .expect("should succeed")
            .response;
        assert!(!r.is_empty(), "read response should not be empty");
        assert!(r.contains("hello from disk"));
    }

    #[test]
    fn embedded_mode_can_run_shell_commands() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let r = run_embedded_message_with_echo(&sp, "s1", "shell printf hi")
            .expect("should succeed")
            .response;
        assert!(!r.is_empty(), "shell response should not be empty");
    }

    #[test]
    fn unavailable_tool_is_reported_without_crashing() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        let mut h = echo_harness(&sp, &pp).expect("start");

        let conn_id = h
            .extension_connection_id("tools")
            .expect("tools")
            .to_owned();
        let removed = h.registry.unregister_connection(&conn_id);
        assert!(removed.iter().any(|t| t == "shell"));

        let outcome = h
            .send_user_message("s1", "shell printf hi", None)
            .expect("should succeed with error");
        assert!(outcome.response.contains("no live provider available"));
        h.shutdown().expect("shutdown");
    }

    #[test]
    fn disconnected_tool_is_removed_cleanly() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        let mut h = echo_harness(&sp, &pp).expect("start");

        let conn_id = h
            .extension_connection_id("tools")
            .expect("tools")
            .to_owned();

        // Send disconnect to the extension via the bus (through the
        // writer channel → writer thread → stream).
        let _ = h.bus.send_to(
            &conn_id,
            None,
            Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: Some("test".to_owned()),
            }),
        );

        // Drive event loop until the disconnect arrives.
        let started = Instant::now();
        loop {
            let event =
                h.rx.recv_timeout(Duration::from_secs(2))
                    .expect("should get disconnect");
            match event {
                HarnessEvent::Disconnected {
                    ref connection_id, ..
                } if *connection_id == conn_id => {
                    h.handle_disconnect(&conn_id);
                    break;
                }
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    let _ = h.handle_extension_event(&connection_id, event);
                }
                _ => {}
            }
            assert!(started.elapsed() < Duration::from_secs(2), "timeout");
        }

        assert!(h.bus.connection(&conn_id).is_none());
        assert!(h.registry.providers_for("shell").is_empty());
        assert!(
            h.lifecycle_messages
                .iter()
                .any(|m| m == "extension tools exited")
        );

        let outcome = h
            .send_user_message("s1", "shell printf hi", None)
            .expect("should succeed with error");
        assert!(outcome.response.contains("no live provider available"));
        h.shutdown().expect("shutdown");
    }

    #[test]
    fn traced_embedded_reports_shell_progress() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let o = run_embedded_message_with_echo(&sp, "s1", "shell printf hi").expect("ok");
        assert_eq!(o.progress_messages, vec!["shell: running shell command"]);
        assert!(!o.response.is_empty(), "shell response should not be empty");
    }

    #[test]
    #[ignore = "needs echo agent wired into run_daemon"]
    fn traced_daemon_reports_shell_progress() {
        let td = TempDir::new().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let sp = td.path().join("sessions.cbor");

        let server = thread::spawn({
            let sock = sock.clone();
            let sp = sp.clone();
            move || run_daemon(sock, sp, ServeOptions::builder().max_clients(1).build())
        });

        let started = Instant::now();
        while !sock.exists() {
            assert!(started.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(10));
        }

        let o = send_daemon_message_with_trace(&sock, "s1", "shell printf hi").expect("ok");
        assert!(
            o.lifecycle_messages
                .iter()
                .any(|m| m == "extension agent ready")
        );
        assert!(
            o.lifecycle_messages
                .iter()
                .any(|m| m == "extension tools ready")
        );
        assert_eq!(o.progress_messages, vec!["shell: running shell command"]);
        assert!(!o.response.is_empty(), "shell response should not be empty");
        server.join().expect("join").expect("clean exit");
    }

    #[test]
    fn traced_embedded_reports_lifecycle() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let o = run_embedded_message_with_echo(&sp, "s1", "hello").expect("ok");
        assert!(
            o.lifecycle_messages
                .iter()
                .any(|m| m == "extension agent starting")
        );
        assert!(
            o.lifecycle_messages
                .iter()
                .any(|m| m == "extension agent ready")
        );
        assert!(
            o.lifecycle_messages
                .iter()
                .any(|m| m == "extension agent exited")
        );
    }

    #[test]
    #[ignore = "needs echo agent wired into run_daemon"]
    fn session_and_policy_lines_are_printable() {
        let td = TempDir::new().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");

        let server = thread::spawn({
            let sock = sock.clone();
            let sp = sp.clone();
            let pp = pp.clone();
            move || {
                run_daemon(
                    sock,
                    sp,
                    ServeOptions::builder()
                        .max_clients(1)
                        .policy_store_path(pp)
                        .build(),
                )
            }
        });

        let started = Instant::now();
        while !sock.exists() {
            assert!(started.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(10));
        }

        let _ = send_daemon_message_with_trace(&sock, "s1", "hello").expect("ok");
        server.join().expect("join").expect("clean exit");

        let sl = session_lines(&sp, "s1").expect("lines");
        assert!(sl.iter().any(|l| l.contains("user: hello")));
        assert!(sl.iter().any(|l| l.contains("tool.request echo")));
        let sll = session_list_lines(&sp).expect("list");
        assert!(sll.iter().any(|l| l.contains("s1 (4 entries)")));
        let pl = policy_lines(&pp).expect("policy");
        assert!(pl.iter().any(|l| l.contains("socket-ui")));
    }

    #[test]
    fn empty_session_and_policy_views() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        assert_eq!(session_list_lines(&sp).expect("ok"), vec!["no sessions"]);
        assert_eq!(policy_lines(&pp).expect("ok"), vec!["no policy approvals"]);
        assert_eq!(
            session_lines(&sp, "x").expect("ok"),
            vec!["session x not found"]
        );
    }

    #[test]
    fn daemon_disconnect_reason_is_reported() {
        let td = TempDir::new().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let listener = bind_listener(&sock).expect("bind");

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let read_stream = stream.try_clone().expect("clone");
            let mut reader = EventReader::new(BufReader::new(read_stream));
            let mut writer = EventWriter::new(BufWriter::new(stream));
            let _ = reader.read_event(); // hello
            let _ = reader.read_event(); // subscribe
            let _ = reader.read_event(); // message
            writer
                .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                    reason: Some("test disconnect".to_owned()),
                }))
                .expect("write");
            writer.flush().expect("flush");
        });

        let err = send_daemon_message_with_trace(&sock, "s1", "hello")
            .expect_err("should get disconnect");
        assert!(matches!(&err, HarnessError::Participant(r) if r == "test disconnect"));
        server.join().expect("join");
    }

    // -- AGENTS.md --

    #[test]
    fn agents_context_is_injected_at_session_init() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        let mut h = echo_harness(&sp, &pp).expect("start");
        let tools_connection_id = h
            .extension_connection_id("tools")
            .expect("tools")
            .to_owned();

        h.discovered_agents_files.push(DiscoveredAgentsFile {
            source_id: tools_connection_id.clone().into(),
            file_path: PathBuf::from("/repo/AGENTS.md"),
            content: "# Root\n- root rule\n".to_owned(),
        });
        h.discovered_agents_files.push(DiscoveredAgentsFile {
            source_id: tools_connection_id.clone().into(),
            file_path: PathBuf::from("/repo/pkg/AGENTS.md"),
            content: "# Package\n- package rule\n".to_owned(),
        });
        h.turn_state = TurnState::InitializingSession {
            session_id: "s1".into(),
            waiting_on: [tools_connection_id.clone().into()].into_iter().collect(),
        };
        h.handle_extension_event(
            &tools_connection_id,
            Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
                session_id: "s1".into(),
            }),
        )
        .expect("ready");

        assert!(matches!(h.turn_state, TurnState::Idle));

        let branch = h.store.session("s1").expect("session").current_branch();
        let SessionEntry::UserMessage { text: injected } = branch[0] else {
            panic!("expected injected AGENTS.md user message");
        };
        assert!(injected.starts_with("# AGENTS.md instructions"));
        assert!(injected.contains("<AGENTS_FILE path=\"/repo/AGENTS.md\">"));
        assert!(injected.contains("<AGENTS_FILE path=\"/repo/pkg/AGENTS.md\">"));
        let root_pos = injected.find("root rule").expect("root rule");
        let pkg_pos = injected.find("package rule").expect("package rule");
        assert!(
            root_pos < pkg_pos,
            "broader file should appear before nested one"
        );

        h.shutdown().expect("shutdown");
    }

    #[test]
    fn first_prompt_initializes_custom_session_before_dispatch() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        let mut h = echo_harness(&sp, &pp).expect("start");
        let tools_connection_id = h
            .extension_connection_id("tools")
            .expect("tools")
            .to_owned();

        h.selected_model = "test/model".into();
        let submission = h
            .submit_user_prompt("chat-1".into(), "hello".to_owned())
            .expect("submit");
        assert_eq!(
            submission,
            PromptSubmission::Queued,
            "fresh session should initialize before dispatch"
        );
        assert!(h.pending_prompts.len() == 1, "prompt should remain queued");
        assert!(
            matches!(
                &h.turn_state,
                TurnState::InitializingSession { session_id, .. }
                    if session_id == "chat-1"
            ),
            "expected custom session init, got different turn state"
        );

        h.handle_extension_event(
            &tools_connection_id,
            Event::ExtAgentsMdAvailable(tau_proto::ExtAgentsMdAvailable {
                file_path: "/repo/AGENTS.md".to_owned(),
                content: "# Root\n- root rule\n".to_owned(),
            }),
        )
        .expect("agents");
        h.handle_extension_event(
            &tools_connection_id,
            Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
                session_id: "chat-1".into(),
            }),
        )
        .expect("ready");

        assert!(h.initialized_sessions.contains("chat-1"));
        assert!(
            matches!(&h.turn_state, TurnState::AgentThinking { .. }),
            "queued prompt should dispatch after init"
        );

        let branch = h.store.session("chat-1").expect("session").current_branch();
        let SessionEntry::UserMessage { text: injected } = &branch[0] else {
            panic!("expected injected AGENTS.md user message");
        };
        assert!(injected.starts_with("# AGENTS.md instructions"));
        let SessionEntry::UserMessage { text: prompt } = &branch[1] else {
            panic!("expected queued user prompt after AGENTS.md");
        };
        assert_eq!(prompt, "hello");

        h.shutdown().expect("shutdown");
    }

    // -- Tool dispatch state machine --

    #[test]
    fn pure_mutating_pure_serializes_through_dispatch_state_machine() {
        use tau_proto::ToolSideEffects::{Mutating, Pure};

        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        let mut h = echo_harness(&sp, &pp).expect("start");

        // Pre-seed turn state as if the agent had just been prompted
        // and is about to respond with tool calls.
        h.selected_model = "test/model".into();
        h.turn_state = TurnState::AgentThinking {
            _session_id: "s1".into(),
        };
        h.prompt_sessions.insert("sp-x".into(), "s1".into());

        // A `read` of a nonexistent path returns a ToolError (Pure);
        // `write` of a valid path creates the file and returns
        // ToolResult (Mutating). Either kind of response path is
        // handled identically by the state machine.
        let read_args = CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Text("/nonexistent/tau-test-path".to_owned()),
        )]);
        let write_args = CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(td.path().join("w.txt").display().to_string()),
            ),
            (
                CborValue::Text("content".to_owned()),
                CborValue::Text("hi".to_owned()),
            ),
        ]);
        let response = AgentResponseFinished {
            session_prompt_id: "sp-x".into(),
            text: None,
            tool_calls: vec![
                AgentToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: read_args.clone(),
                },
                AgentToolCall {
                    id: "c2".into(),
                    name: "write".into(),
                    arguments: write_args,
                },
                AgentToolCall {
                    id: "c3".into(),
                    name: "read".into(),
                    arguments: read_args,
                },
            ],
        };

        h.handle_agent_response_finished(response)
            .expect("finished");

        // Right after dispatch, only c1 (Pure) should be in-flight;
        // c2 (Mutating) and c3 (Pure behind the Mutating) must wait.
        let c1_id: ToolCallId = "c1".to_owned().into();
        let c2_id: ToolCallId = "c2".to_owned().into();
        let c3_id: ToolCallId = "c3".to_owned().into();
        assert_eq!(h.in_flight_tool_kinds.len(), 1);
        assert_eq!(h.in_flight_tool_kinds.get(&c1_id), Some(&Pure));
        assert_eq!(h.pending_tool_invocations.len(), 2);
        assert_eq!(h.pending_tool_invocations[0].1.id, "c2");
        assert_eq!(h.pending_tool_invocations[1].1.id, "c3");

        drive_harness_until_call_completes(&mut h, "c1");

        // After c1 completes the Mutating gate opens and c2 dispatches.
        // c3 must stay queued behind it.
        assert_eq!(h.in_flight_tool_kinds.len(), 1);
        assert_eq!(h.in_flight_tool_kinds.get(&c2_id), Some(&Mutating));
        assert_eq!(h.pending_tool_invocations.len(), 1);
        assert_eq!(h.pending_tool_invocations[0].1.id, "c3");

        drive_harness_until_call_completes(&mut h, "c2");

        // With the Mutating cleared, c3 finally dispatches.
        assert_eq!(h.in_flight_tool_kinds.len(), 1);
        assert_eq!(h.in_flight_tool_kinds.get(&c3_id), Some(&Pure));
        assert!(h.pending_tool_invocations.is_empty());

        drive_harness_until_call_completes(&mut h, "c3");
        assert!(h.in_flight_tool_kinds.is_empty());

        h.shutdown().expect("shutdown");
    }

    /// Pumps the harness event loop until the named tool call's result
    /// or error is received and handled. Panics on timeout.
    fn drive_harness_until_call_completes(h: &mut Harness, target_call_id: &str) {
        let started = Instant::now();
        loop {
            if started.elapsed() >= Duration::from_secs(3) {
                panic!("timed out waiting for {target_call_id} to complete");
            }
            let event =
                h.rx.recv_timeout(Duration::from_secs(1))
                    .expect("tool result should arrive");
            match event {
                HarnessEvent::FromConnection {
                    connection_id,
                    event,
                } => {
                    let is_target = match &event {
                        Event::ToolResult(r) => r.call_id.as_str() == target_call_id,
                        Event::ToolError(e) => e.call_id.as_str() == target_call_id,
                        _ => false,
                    };
                    h.handle_extension_event(&connection_id, event)
                        .expect("handle");
                    if is_target {
                        return;
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    h.handle_disconnect(&connection_id);
                }
                HarnessEvent::NewClient(_) => {}
            }
        }
    }

    // -- At-least-once delivery --

    #[test]
    fn extension_ack_advances_cursor() {
        // Verifies the at-least-once cursor: after the harness receives
        // an Ack from an extension, that extension's `last_acked` field
        // reflects the highest acked id.
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        let mut h = echo_harness(&sp, &pp).expect("start");
        let tools_id = h
            .extension_connection_id("tools")
            .expect("tools")
            .to_owned();

        h.handle_extension_event(
            &tools_id,
            Event::Ack(tau_proto::Ack {
                up_to: tau_proto::LogEventId::new(7),
            }),
        )
        .expect("ack");

        let tools = h
            .extensions
            .iter()
            .find(|e| e.connection_id.as_str() == tools_id)
            .expect("entry");
        assert_eq!(tools.last_acked, tau_proto::LogEventId::new(7));
        h.shutdown().expect("shutdown");
    }

    #[test]
    fn duplicate_ack_is_ignored() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        let mut h = echo_harness(&sp, &pp).expect("start");
        let tools_id = h
            .extension_connection_id("tools")
            .expect("tools")
            .to_owned();
        let before = h
            .extensions
            .iter()
            .find(|e| e.connection_id.as_str() == tools_id)
            .expect("entry")
            .last_acked;

        // Resending an old ack must not move the cursor backward and
        // must not bump it forward either.
        h.handle_extension_event(
            &tools_id,
            Event::Ack(tau_proto::Ack {
                up_to: tau_proto::LogEventId::new(0),
            }),
        )
        .expect("ack");

        let after = h
            .extensions
            .iter()
            .find(|e| e.connection_id.as_str() == tools_id)
            .expect("entry")
            .last_acked;
        assert_eq!(before, after, "stale ack should not change cursor");
        h.shutdown().expect("shutdown");
    }

    // -- Skills --

    #[test]
    fn build_system_prompt_includes_skills() {
        let mut skills = std::collections::HashMap::new();
        skills.insert(
            "brave-search".to_owned(),
            DiscoveredSkill {
                source_id: "skills".into(),
                description: "Web search via Brave API".to_owned(),
                file_path: PathBuf::from("/skills/brave-search/SKILL.md"),
                add_to_prompt: true,
            },
        );
        let prompt = build_system_prompt(&[], &skills);
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>brave-search</name>"));
        assert!(prompt.contains("Web search via Brave API"));
    }

    #[test]
    fn build_system_prompt_excludes_hidden_skills() {
        let mut skills = std::collections::HashMap::new();
        skills.insert(
            "hidden".to_owned(),
            DiscoveredSkill {
                source_id: "skills".into(),
                description: "Should not appear".to_owned(),
                file_path: PathBuf::from("/skills/hidden/SKILL.md"),
                add_to_prompt: false,
            },
        );
        let prompt = build_system_prompt(&[], &skills);
        assert!(!prompt.contains("<available_skills>"));
        assert!(!prompt.contains("hidden"));
    }

    #[test]
    fn skill_tool_reads_file_content() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");

        let skill_dir = td.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        let skill_file = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_file,
            "---\nname: my-skill\ndescription: A test skill\n---\n# Instructions\nDo the thing.",
        )
        .expect("write");

        let mut h = echo_harness(&sp, &pp).expect("start");

        // Manually insert a discovered skill.
        h.discovered_skills.insert(
            "my-skill".to_owned(),
            DiscoveredSkill {
                source_id: "skills".into(),
                description: "A test skill".to_owned(),
                file_path: skill_file,
                add_to_prompt: true,
            },
        );

        // Directly invoke the skill tool handler.
        h.store
            .append_user_message("s1", "load skill".to_owned())
            .expect("append");
        h.turn_state = TurnState::ToolsRunning {
            session_id: "s1".into(),
            remaining_calls: vec!["call-skill".into()],
        };
        let call = AgentToolCall {
            id: "call-skill".to_owned(),
            name: "skill".to_owned(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("name".to_owned()),
                CborValue::Text("my-skill".to_owned()),
            )]),
        };
        h.handle_skill_tool_call("s1", &call).expect("skill call");

        // Verify the tool result was persisted.
        let branch = h.store.session("s1").expect("session").current_branch();
        let has_skill_result = branch.iter().any(|entry| {
            matches!(
                entry,
                SessionEntry::ToolActivity(ToolActivityRecord {
                    outcome: ToolActivityOutcome::Result { .. },
                    ..
                })
            )
        });
        assert!(has_skill_result, "expected skill tool result in session");
    }

    #[test]
    fn skill_tool_returns_error_for_unknown_skill() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");

        let mut h = echo_harness(&sp, &pp).expect("start");
        h.store
            .append_user_message("s1", "load skill".to_owned())
            .expect("append");
        h.turn_state = TurnState::ToolsRunning {
            session_id: "s1".into(),
            remaining_calls: vec!["call-missing".into()],
        };
        let call = AgentToolCall {
            id: "call-missing".to_owned(),
            name: "skill".to_owned(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("name".to_owned()),
                CborValue::Text("nonexistent".to_owned()),
            )]),
        };
        h.handle_skill_tool_call("s1", &call).expect("skill call");

        // Verify a tool error was persisted.
        let branch = h.store.session("s1").expect("session").current_branch();
        let has_skill_error = branch.iter().any(|entry| {
            matches!(
                entry,
                SessionEntry::ToolActivity(ToolActivityRecord {
                    outcome: ToolActivityOutcome::Error { .. },
                    ..
                })
            )
        });
        assert!(has_skill_error, "expected skill tool error in session");
    }

    #[test]
    fn skill_tool_registered_in_tool_list() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");

        let h = echo_harness(&sp, &pp).expect("start");
        let defs = h.gather_tool_definitions();
        assert!(
            defs.iter().any(|d| d.name == "skill"),
            "skill tool should be registered; got: {:?}",
            defs.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn duplicate_tool_result_is_discarded() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");

        let mut h = echo_harness(&sp, &pp).expect("start");

        // Fabricate a tool result for a call_id that is not in pending_tool_sessions.
        let result = h.handle_extension_event(
            "fake-ext",
            Event::ToolResult(ToolResult {
                call_id: "orphan-call".into(),
                tool_name: "read".into(),
                result: tau_proto::CborValue::Text("stale data".to_owned()),
            }),
        );
        // Should not error — just emits a warning and discards.
        assert!(result.is_ok());
    }
}
