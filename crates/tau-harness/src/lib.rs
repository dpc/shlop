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
    DefaultSubscriptionPolicy, EventBus, PolicyStore, RouteError, SessionEntry, SessionStore,
    SessionStoreError, ToolActivityOutcome, ToolActivityRecord, ToolRegistry, ToolRouteError,
};
use tau_proto::{
    AgentResponseFinished, AgentToolCall, ClientKind, ContentBlock, ConversationMessage,
    ConversationRole, DecodeError, Event, EventReader, EventSelector, EventWriter,
    LifecycleDisconnect, LifecycleHello, LifecycleSubscribe, PROTOCOL_VERSION, ProgressUpdate,
    SessionPromptCreated, SessionPromptQueued, ToolDefinition, ToolError, ToolProgress,
    ToolRegister, ToolRequest, ToolResult, UiPromptSubmitted,
};
use tau_socket::{SocketPeer, SocketTransportError};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Serve-loop options for daemon mode.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServeOptions {
    pub max_clients: Option<usize>,
    pub policy_store_path: Option<PathBuf>,
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
    FromConnection { connection_id: String, event: Event },
    /// A connection's reader hit EOF or decode error.
    Disconnected { connection_id: String },
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
    connection_id: String,
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

/// Tracks an in-progress agent turn waiting for tool results.
struct PendingAgentTurn {
    session_id: String,
    remaining_calls: Vec<String>,
}

struct ExtensionEntry {
    name: String,
    connection_id: String,
    /// In-process extension thread handle (for join on shutdown).
    in_process_thread: Option<JoinHandle<Result<(), String>>>,
}

// ---------------------------------------------------------------------------
// Event debug log
// ---------------------------------------------------------------------------

/// Append-only JSON event log for debugging.
struct EventLog {
    path: PathBuf,
    file: std::fs::File,
}

impl EventLog {
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
                    "connection_id": connection_id,
                    "event_name": event.name().as_str(),
                    "event": event_json,
                })
            }
            HarnessEvent::Disconnected { connection_id } => {
                serde_json::json!({
                    "type": "disconnected",
                    "connection_id": connection_id,
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

struct Harness {
    tx: Sender<HarnessEvent>,
    rx: Receiver<HarnessEvent>,
    bus: EventBus,
    registry: ToolRegistry,
    store: SessionStore,
    pending_request_sessions: VecDeque<String>,
    pending_tool_sessions: std::collections::HashMap<String, String>,
    extension_statuses: std::collections::HashMap<String, Event>,
    lifecycle_messages: Vec<String>,
    extensions: Vec<ExtensionEntry>,
    agent_connection_id: String,
    next_session_prompt_id: u64,
    /// Maps session_prompt_id → session_id for in-flight prompts.
    prompt_sessions: std::collections::HashMap<String, String>,
    /// Pending agent turn: session_id + remaining tool call IDs.
    pending_agent_turn: Option<PendingAgentTurn>,
    /// True while the agent is processing a prompt (between
    /// SessionPromptCreated and the final AgentResponseFinished
    /// with no tool calls).
    agent_busy: bool,
    /// Queued user prompts waiting for the current turn to finish.
    /// Each entry is (session_id, text) — already persisted in the
    /// session tree, just waiting for dispatch.
    //
    // Future: add a steering queue for mid-turn injection. Steering
    // messages would be injected after tool-call turns complete but
    // before the next LLM call, allowing the user to redirect the
    // agent while it's working. See PI_PROMPT_QUEUEING.md for Pi's
    // two-tier (steering + follow-up) design.
    /// (session_id, text) — text is persisted when dispatched.
    pending_prompts: VecDeque<(String, String)>,
    /// Append-only event debug log.
    event_log: Option<EventLog>,
}

impl Harness {
    /// Creates a harness with in-process extensions (agent, fs, shell).
    fn new(
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
    ) -> Result<Self, HarnessError> {
        let (tx, rx) = mpsc::channel();
        let mut bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(PolicyStore::open(policy_store_path.into())?),
        ));
        let store = SessionStore::open(store_path)?;

        let mut extensions = Vec::new();
        // Agent
        let (conn_id, thread) = spawn_in_process(
            "agent",
            ClientKind::Agent,
            |r, w| tau_agent::run(r, w).map_err(|e| e.to_string()),
            &mut bus,
            &tx,
        )?;
        let agent_connection_id = conn_id.clone();
        extensions.push(ExtensionEntry {
            name: "agent".to_owned(),
            connection_id: conn_id,
            in_process_thread: Some(thread),
        });

        // Filesystem and shell tools
        let (conn_id, thread) = spawn_in_process(
            "tools",
            ClientKind::Tool,
            |r, w| tau_ext_fs::run(r, w).map_err(|e| e.to_string()),
            &mut bus,
            &tx,
        )?;
        extensions.push(ExtensionEntry {
            name: "tools".to_owned(),
            connection_id: conn_id,
            in_process_thread: Some(thread),
        });

        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            store,
            pending_request_sessions: VecDeque::new(),
            pending_tool_sessions: std::collections::HashMap::new(),
            extension_statuses: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            agent_connection_id,
            extensions,
            next_session_prompt_id: 0,
            prompt_sessions: std::collections::HashMap::new(),
            pending_agent_turn: None,
            agent_busy: false,
            pending_prompts: VecDeque::new(),
            event_log: None,
        };

        let n = harness.extensions.len();
        for i in 0..n {
            let name = harness.extensions[i].name.clone();
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_startup(n)?;
        Ok(harness)
    }

    /// Creates a harness from configuration, spawning real child processes.
    fn from_config(
        config: &Config,
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
    ) -> Result<Self, HarnessError> {
        let (tx, rx) = mpsc::channel();
        let mut bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(PolicyStore::open(policy_store_path.into())?),
        ));
        let store = SessionStore::open(store_path)?;

        let mut extensions = Vec::new();
        let mut agent_connection_id = None;

        for ext_config in &config.extensions {
            let kind = match ext_config.role.as_deref() {
                Some("agent") => ClientKind::Agent,
                _ => ClientKind::Tool,
            };

            let conn_id = spawn_supervised(ext_config, kind.clone(), &mut bus, &tx)?;

            if kind == ClientKind::Agent {
                agent_connection_id = Some(conn_id.clone());
            }
            extensions.push(ExtensionEntry {
                name: ext_config.name.clone(),
                connection_id: conn_id,
                in_process_thread: None,
            });
        }

        let agent_connection_id = agent_connection_id.ok_or(HarnessError::NoAgentConfigured)?;

        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            store,
            pending_request_sessions: VecDeque::new(),
            pending_tool_sessions: std::collections::HashMap::new(),
            extension_statuses: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            agent_connection_id,
            extensions,
            next_session_prompt_id: 0,
            prompt_sessions: std::collections::HashMap::new(),
            pending_agent_turn: None,
            agent_busy: false,
            pending_prompts: VecDeque::new(),
            event_log: None,
        };

        let n = harness.extensions.len();
        for i in 0..n {
            let name = harness.extensions[i].name.clone();
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_startup(n)?;
        Ok(harness)
    }

    fn log_event(&mut self, harness_event: &HarnessEvent) {
        if let Some(log) = &mut self.event_log {
            log.log_harness_event(harness_event);
        }
    }

    fn enable_event_log(&mut self, dir: &Path) -> Result<PathBuf, HarnessError> {
        let log = EventLog::open(dir)?;
        let path = log.path().to_path_buf();
        self.event_log = Some(log);
        Ok(path)
    }

    // -----------------------------------------------------------------------
    // Startup
    // -----------------------------------------------------------------------

    fn wait_for_startup(&mut self, total: usize) -> Result<(), HarnessError> {
        let mut ready_count = 0;
        let started_at = Instant::now();
        while ready_count < total {
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
                    if matches!(&event, Event::LifecycleReady(_)) {
                        ready_count += 1;
                    }
                    self.handle_extension_event(&connection_id, event)?;
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let name = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| connection_id.clone());
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
        let conn_id = self.bus.connect(Connection::new(
            ConnectionMetadata {
                id: String::new(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin: ConnectionOrigin::Socket,
            },
            Box::new(ChannelSink { tx: writer_tx }),
        ));
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
            Event::LifecycleHello(hello) => {
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::LifecycleHello(hello));
            }
            Event::LifecycleSubscribe(subscribe) => {
                self.bus
                    .set_subscriptions(source_id, subscribe.selectors.clone())?;
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::LifecycleSubscribe(subscribe));
            }
            Event::LifecycleReady(ready) => {
                self.emit_extension_ready(source_id);
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::LifecycleReady(ready));
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
                        let _ = self.bus.publish(Event::ToolError(error));
                    }
                    Err(error) => return Err(HarnessError::ToolRoute(error)),
                }
            }
            Event::ToolResult(result) => {
                self.persist_tool_result(&result)?;
                let call_id = result.call_id.clone();
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::ToolResult(result));
                self.maybe_complete_agent_turn(&call_id);
            }
            Event::ToolError(error) => {
                self.persist_tool_error(&error)?;
                let call_id = error.call_id.clone();
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::ToolError(error));
                self.maybe_complete_agent_turn(&call_id);
            }
            Event::ToolProgress(progress) => {
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::ToolProgress(progress));
            }
            Event::AgentPromptSubmitted(_) | Event::AgentResponseUpdated(_) => {
                // Forward agent events to all subscribers (UI).
                let _ = self.bus.publish_from(Some(source_id), event);
            }
            Event::AgentResponseFinished(response) => {
                // Forward to subscribers first, then handle internally.
                let _ = self
                    .bus
                    .publish(Event::AgentResponseFinished(response.clone()));
                self.handle_agent_response_finished(response)?;
            }
            other => {
                let _ = self.bus.publish_from(Some(source_id), other);
            }
        }
        Ok(())
    }

    fn handle_client_event(&mut self, client_id: &str, event: Event) -> Result<bool, HarnessError> {
        match event {
            Event::LifecycleHello(hello) => {
                let _ = self
                    .bus
                    .publish_from(Some(client_id), Event::LifecycleHello(hello));
                Ok(true)
            }
            Event::LifecycleSubscribe(subscribe) => {
                match self
                    .bus
                    .set_subscriptions(client_id, subscribe.selectors.clone())
                {
                    Ok(()) => {
                        let replay = wants_extension_events(&subscribe.selectors);
                        let _ = self
                            .bus
                            .publish_from(Some(client_id), Event::LifecycleSubscribe(subscribe));
                        if replay {
                            self.replay_extension_statuses(client_id)?;
                        }
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
            Event::UiPromptSubmitted(prompt) => {
                // Publish the fact so the UI echoes the user message.
                let _ = self
                    .bus
                    .publish_from(Some(client_id), Event::UiPromptSubmitted(prompt.clone()));

                if self.agent_busy {
                    // Queue for later — do NOT persist yet. If we
                    // persist now, the user message appears in the
                    // session tree before the current response,
                    // breaking role alternation for the LLM API.
                    // The text is persisted when dispatched.
                    //
                    // Future: support "steering" mode where the
                    // message is injected mid-turn (after the current
                    // tool calls finish but before the next LLM call)
                    // rather than waiting for the full turn to end.
                    self.pending_prompts
                        .push_back((prompt.session_id.clone(), prompt.text.clone()));
                    let _ = self
                        .bus
                        .publish(Event::SessionPromptQueued(SessionPromptQueued {
                            session_id: prompt.session_id.clone(),
                            text: prompt.text.clone(),
                        }));
                } else {
                    // Persist and dispatch immediately.
                    self.store
                        .append_user_message(&prompt.session_id, prompt.text.clone())?;
                    self.agent_busy = true;
                    self.send_prompt_to_agent(&prompt.session_id);
                }
                Ok(true)
            }
            Event::LifecycleDisconnect(_) => Ok(false),
            other => {
                let _ = self.bus.publish_from(Some(client_id), other);
                Ok(true)
            }
        }
    }

    fn handle_disconnect(&mut self, connection_id: &str) {
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
            .unwrap_or_else(|| "default".to_owned());
        self.pending_tool_sessions
            .insert(request.call_id.clone(), session_id.clone());
        self.store.append_tool_activity(
            session_id,
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
            .remove(&result.call_id)
            .unwrap_or_else(|| "default".to_owned());
        self.store.append_tool_activity(
            session_id,
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
            .remove(&error.call_id)
            .unwrap_or_else(|| "default".to_owned());
        self.store.append_tool_activity(
            session_id,
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

    fn emit_extension_starting(&mut self, extension_name: &str) {
        let event = Event::ExtensionStarting(tau_proto::ExtensionStarting {
            extension_name: extension_name.to_owned(),
            argv: Vec::new(),
        });
        self.extension_statuses
            .insert(extension_name.to_owned(), event.clone());
        self.lifecycle_messages.push(format_extension_event(&event));
        let _ = self.bus.publish(event);
    }

    fn emit_extension_ready(&mut self, connection_id: &str) {
        let Some(connection) = self.bus.connection(connection_id).cloned() else {
            return;
        };
        let event = Event::ExtensionReady(tau_proto::ExtensionReady {
            extension_name: connection.name.clone(),
            connection_id: Some(connection.id),
        });
        self.extension_statuses
            .insert(connection.name, event.clone());
        self.lifecycle_messages.push(format_extension_event(&event));
        let _ = self.bus.publish(event);
    }

    fn emit_extension_exited(&mut self, extension_name: &str) {
        let event = Event::ExtensionExited(tau_proto::ExtensionExited {
            extension_name: extension_name.to_owned(),
            exit_code: None,
            signal: None,
        });
        self.extension_statuses
            .insert(extension_name.to_owned(), event.clone());
        self.lifecycle_messages.push(format_extension_event(&event));
        let _ = self.bus.publish(event);
    }

    fn replay_extension_statuses(&mut self, client_id: &str) -> Result<(), HarnessError> {
        let mut names = self.extension_statuses.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in names {
            if let Some(event) = self.extension_statuses.get(&name).cloned() {
                let _ = self.bus.send_to(client_id, None, event)?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Agent prompt assembly
    // -----------------------------------------------------------------------

    fn send_prompt_to_agent(&mut self, session_id: &str) -> String {
        let tree = self.store.session(session_id);
        let messages = tree.map(assemble_conversation).unwrap_or_default();
        let tools = self.gather_tool_definitions();
        let session_prompt_id = format!("sp-{}", self.next_session_prompt_id);
        self.next_session_prompt_id += 1;
        self.prompt_sessions
            .insert(session_prompt_id.clone(), session_id.to_owned());

        // Publish SessionPromptCreated — both the agent and UI see it.
        let event = Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: session_prompt_id.clone(),
            session_id: session_id.to_owned(),
            system_prompt: build_system_prompt(&tools),
            messages,
            tools,
        });
        // Log this event since it's published via the bus, not the
        // harness channel (so log_event in the event loop misses it).
        if let Some(log) = &mut self.event_log {
            log.log_harness_event(&HarnessEvent::FromConnection {
                connection_id: "harness".to_owned(),
                event: event.clone(),
            });
        }
        let _ = self.bus.publish(event);

        session_prompt_id
    }

    fn gather_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.registry
            .all_tools()
            .into_iter()
            .map(|spec| ToolDefinition {
                name: spec.name.clone(),
                description: spec.description.clone(),
            })
            .collect()
    }

    fn handle_agent_response_finished(
        &mut self,
        response: AgentResponseFinished,
    ) -> Result<(), HarnessError> {
        let session_id = self
            .prompt_sessions
            .remove(&response.session_prompt_id)
            .unwrap_or_else(|| "default".to_owned());

        // Persist agent text if present.
        if let Some(ref text) = response.text {
            self.store.append_agent_message(&session_id, text.clone())?;
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
            let call_ids: Vec<String> = response.tool_calls.iter().map(|c| c.id.clone()).collect();
            self.pending_agent_turn = Some(PendingAgentTurn {
                session_id: session_id.clone(),
                remaining_calls: call_ids,
            });
            for call in &response.tool_calls {
                self.execute_agent_tool_call(&session_id, call)?;
            }
        } else {
            // No tool calls — turn is done. Dispatch next queued
            // prompt if any, otherwise mark agent as idle.
            self.dispatch_next_or_idle(&session_id);
        }

        Ok(())
    }

    /// Dispatches the next queued prompt or marks the agent as idle.
    fn dispatch_next_or_idle(&mut self, _completed_session_id: &str) {
        if let Some((session_id, text)) = self.pending_prompts.pop_front() {
            // Persist now — the user message is placed after the
            // response from the just-finished turn, maintaining
            // correct role alternation.
            let _ = self.store.append_user_message(&session_id, text);
            self.send_prompt_to_agent(&session_id);
            // agent_busy stays true
        } else {
            self.agent_busy = false;
        }
    }

    fn maybe_complete_agent_turn(&mut self, completed_call_id: &str) {
        let should_send = if let Some(turn) = &mut self.pending_agent_turn {
            turn.remaining_calls.retain(|id| id != completed_call_id);
            turn.remaining_calls.is_empty()
        } else {
            false
        };
        if should_send {
            let session_id = self
                .pending_agent_turn
                .take()
                .expect("just checked")
                .session_id;
            self.send_prompt_to_agent(&session_id);
        }
    }

    fn execute_agent_tool_call(
        &mut self,
        session_id: &str,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        // Persist the request.
        self.store.append_tool_activity(
            session_id,
            ToolActivityRecord {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                outcome: ToolActivityOutcome::Requested {
                    arguments: call.arguments.clone(),
                },
            },
        )?;

        // Route to tool provider.
        let request = ToolRequest {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
        };

        // Track which session this call belongs to.
        self.pending_tool_sessions
            .insert(call.id.clone(), session_id.to_owned());

        match self
            .registry
            .route_tool_request(&mut self.bus, &self.agent_connection_id, request)
        {
            Ok(_) => {}
            Err(ToolRouteError::NoProvider { tool_name }) => {
                let error = ToolError {
                    call_id: call.id.clone(),
                    tool_name,
                    message: "no live provider available".to_owned(),
                    details: None,
                };
                self.persist_tool_error(&error)?;
                // Mark this call as completed so the turn can proceed.
                self.maybe_complete_agent_turn(&call.id);
            }
            Err(error) => return Err(HarnessError::ToolRoute(error)),
        }

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
        self.store
            .append_user_message(session_id.to_owned(), text.to_owned())?;
        self.agent_busy = true;
        self.send_prompt_to_agent(session_id);

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
) -> Result<(String, JoinHandle<Result<(), String>>), HarnessError>
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
            id: String::new(),
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
) -> Result<String, HarnessError> {
    let mut child = Command::new(&config.command)
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(HarnessError::Io)?;

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
            id: String::new(),
            name: config.name.clone(),
            kind,
            origin: ConnectionOrigin::Supervised,
        },
        Box::new(ChannelSink { tx: writer_tx }),
    ));

    spawn_reader_thread(conn_id.clone(), stdout, tx.clone());

    Ok(conn_id)
}

/// Builds the system prompt from available tools and environment.
fn build_system_prompt(tools: &[ToolDefinition]) -> String {
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
         - When asked to read a file, use the fs.read tool.\n\
         - When asked to run a command, use the shell.exec tool.\n",
    );

    // Date and CWD.
    let now = chrono_free_date();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".to_owned());
    prompt.push_str(&format!("\nCurrent date: {now}\n"));
    prompt.push_str(&format!("Current working directory: {cwd}\n"));

    prompt
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
                            id: activity.call_id.clone(),
                            name: activity.tool_name.clone(),
                            input: arguments.clone(),
                        });
                    }
                }
                ToolActivityOutcome::Result { result } => {
                    messages.push(ConversationMessage {
                        role: ConversationRole::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: activity.call_id.clone(),
                            content: cbor_to_text(result),
                            is_error: false,
                        }],
                    });
                }
                ToolActivityOutcome::Error { message, .. } => {
                    messages.push(ConversationMessage {
                        role: ConversationRole::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: activity.call_id.clone(),
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

fn wants_extension_events(selectors: &[EventSelector]) -> bool {
    selectors.iter().any(|selector| match selector {
        EventSelector::Exact(name) => name.as_str().starts_with("extension."),
        EventSelector::Prefix(prefix) => prefix.starts_with("extension."),
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
    let mut text = progress.tool_name.clone();
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

/// Runs one embedded interaction and returns progress plus the final
/// agent response.
pub fn run_embedded_message_with_trace(
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    let session_store_path = session_store_path.into();
    let mut harness = Harness::new(
        session_store_path.clone(),
        default_policy_store_path_from(&session_store_path),
    )?;
    let mut outcome = harness.send_user_message(session_id, message, None)?;
    harness.shutdown()?;
    outcome.lifecycle_messages = harness.lifecycle_messages;
    Ok(outcome)
}

/// Runs one embedded interaction and returns the final agent response.
pub fn run_embedded_message(
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, HarnessError> {
    Ok(run_embedded_message_with_trace(session_store_path, session_id, message)?.response)
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
    let mut harness = Harness::new(session_store_path, policy_store_path)?;

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
    let mut harness = Harness::from_config(config, session_store_path, policy_store_path)?;

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
        ],
    }))?;
    peer.send(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: session_id.to_owned(),
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
            match event {
                Event::ToolProgress(p) => progress_messages.push(format_tool_progress(&p)),
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

    let mut harness = Harness::from_config(config, &session_store_path, &policy_store_path)?;

    // Enable event debug log.
    let log_dir = project_root.join(".tau");
    match harness.enable_event_log(&log_dir) {
        Ok(path) => eprintln!("event log: {}", path.display()),
        Err(e) => eprintln!("warning: could not create event log: {e}"),
    }

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

    #[test]
    fn embedded_mode_returns_agent_response_and_persists_history() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let r = run_embedded_message(&sp, "s1", "hello").expect("should succeed");
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
    fn daemon_mode_accepts_later_clients() {
        let td = TempDir::new().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let sp = td.path().join("sessions.cbor");

        let server = thread::spawn({
            let sock = sock.clone();
            let sp = sp.clone();
            move || {
                run_daemon(
                    sock,
                    sp,
                    ServeOptions {
                        max_clients: Some(2),
                        policy_store_path: None,
                    },
                )
            }
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
        let r = run_embedded_message(&sp, "s1", &format!("read {}", fp.display()))
            .expect("should succeed");
        assert!(!r.is_empty(), "fs.read response should not be empty");
        assert!(r.contains("hello from disk"));
    }

    #[test]
    fn embedded_mode_can_run_shell_commands() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let r = run_embedded_message(&sp, "s1", "shell printf hi").expect("should succeed");
        assert!(!r.is_empty(), "shell response should not be empty");
    }

    #[test]
    fn unavailable_tool_is_reported_without_crashing() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let pp = td.path().join("policy.cbor");
        let mut h = Harness::new(&sp, &pp).expect("start");

        let conn_id = h
            .extension_connection_id("tools")
            .expect("tools")
            .to_owned();
        let removed = h.registry.unregister_connection(&conn_id);
        assert!(removed.iter().any(|t| t == "shell.exec"));

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
        let mut h = Harness::new(&sp, &pp).expect("start");

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
        assert!(h.registry.providers_for("shell.exec").is_empty());
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
        let o = run_embedded_message_with_trace(&sp, "s1", "shell printf hi").expect("ok");
        assert_eq!(
            o.progress_messages,
            vec!["shell.exec: running shell command"]
        );
        assert!(!o.response.is_empty(), "shell response should not be empty");
    }

    #[test]
    fn traced_daemon_reports_shell_progress() {
        let td = TempDir::new().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let sp = td.path().join("sessions.cbor");

        let server = thread::spawn({
            let sock = sock.clone();
            let sp = sp.clone();
            move || {
                run_daemon(
                    sock,
                    sp,
                    ServeOptions {
                        max_clients: Some(1),
                        policy_store_path: None,
                    },
                )
            }
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
                .any(|m| m == "extension filesystem-tool ready")
        );
        assert!(
            o.lifecycle_messages
                .iter()
                .any(|m| m == "extension shell-tool ready")
        );
        assert_eq!(
            o.progress_messages,
            vec!["shell.exec: running shell command"]
        );
        assert!(!o.response.is_empty(), "shell response should not be empty");
        server.join().expect("join").expect("clean exit");
    }

    #[test]
    fn traced_embedded_reports_lifecycle() {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("sessions.cbor");
        let o = run_embedded_message_with_trace(&sp, "s1", "hello").expect("ok");
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
                    ServeOptions {
                        max_clients: Some(1),
                        policy_store_path: Some(pp),
                    },
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
        assert!(sl.iter().any(|l| l.contains("tool.request demo.echo")));
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
}
