//! Minimal CLI runtime for embedded and daemon-attached use.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use shlop_config::{Config, ExtensionConfig};
use shlop_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    DefaultSubscriptionPolicy, EventBus, PolicyStore, RouteError, SessionEntry, SessionStore,
    SessionStoreError, ToolActivityOutcome, ToolActivityRecord, ToolRegistry, ToolRouteError,
};
use shlop_proto::{
    ChatMessage, ClientKind, DecodeError, Event, EventName, EventReader, EventSelector,
    EventWriter, LifecycleDisconnect, LifecycleHello, LifecycleSubscribe, PROTOCOL_VERSION,
    ProgressUpdate, ToolError, ToolProgress, ToolRegister, ToolRequest, ToolResult,
};
use shlop_socket::{SocketListener, SocketPeer, SocketTransportError};
use shlop_supervisor::{ExtensionCommand, SupervisionError, SupervisedChild};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Errors returned by the minimal CLI runtime.
#[derive(Debug)]
pub enum CliError {
    Io(io::Error),
    ProtocolDecode(DecodeError),
    ProtocolEncode(shlop_proto::EncodeError),
    SessionStore(SessionStoreError),
    SocketTransport(SocketTransportError),
    Supervision(SupervisionError),
    Route(RouteError),
    ToolRoute(ToolRouteError),
    StartupTimeout,
    ResponseTimeout,
    ThreadJoin(String),
    Participant(String),
    NoAgentConfigured,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::ProtocolDecode(source) => write!(f, "protocol decode error: {source}"),
            Self::ProtocolEncode(source) => write!(f, "protocol encode error: {source}"),
            Self::SessionStore(source) => write!(f, "session store error: {source}"),
            Self::SocketTransport(source) => write!(f, "socket transport error: {source}"),
            Self::Supervision(source) => write!(f, "supervision error: {source}"),
            Self::Route(source) => write!(f, "routing error: {source}"),
            Self::ToolRoute(source) => write!(f, "tool routing error: {source}"),
            Self::StartupTimeout => {
                f.write_str("timed out waiting for local participants to start")
            }
            Self::ResponseTimeout => f.write_str("timed out waiting for agent response"),
            Self::ThreadJoin(name) => write!(f, "failed to join {name} thread cleanly"),
            Self::Participant(message) => write!(f, "participant error: {message}"),
            Self::NoAgentConfigured => {
                f.write_str("no extension with role \"agent\" in configuration")
            }
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::ProtocolDecode(source) => Some(source),
            Self::ProtocolEncode(source) => Some(source),
            Self::SessionStore(source) => Some(source),
            Self::SocketTransport(source) => Some(source),
            Self::Supervision(source) => Some(source),
            Self::Route(source) => Some(source),
            Self::ToolRoute(source) => Some(source),
            Self::StartupTimeout
            | Self::ResponseTimeout
            | Self::ThreadJoin(_)
            | Self::Participant(_)
            | Self::NoAgentConfigured => None,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<DecodeError> for CliError {
    fn from(source: DecodeError) -> Self {
        Self::ProtocolDecode(source)
    }
}

impl From<SessionStoreError> for CliError {
    fn from(source: SessionStoreError) -> Self {
        Self::SessionStore(source)
    }
}

impl From<SocketTransportError> for CliError {
    fn from(source: SocketTransportError) -> Self {
        Self::SocketTransport(source)
    }
}

impl From<SupervisionError> for CliError {
    fn from(source: SupervisionError) -> Self {
        Self::Supervision(source)
    }
}

impl From<RouteError> for CliError {
    fn from(source: RouteError) -> Self {
        Self::Route(source)
    }
}

impl From<ToolRouteError> for CliError {
    fn from(source: ToolRouteError) -> Self {
        Self::ToolRoute(source)
    }
}

// ---------------------------------------------------------------------------
// Transport types
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum TransportPoll {
    Event(Event),
    Timeout,
    Disconnected,
}

#[derive(Debug)]
struct LocalPeer {
    writer: EventWriter<BufWriter<UnixStream>>,
    incoming: Receiver<Result<Event, DecodeError>>,
}

impl LocalPeer {
    fn new(stream: UnixStream) -> Result<Self, CliError> {
        let writer_stream = stream.try_clone()?;
        let incoming = spawn_reader(stream);
        Ok(Self {
            writer: EventWriter::new(BufWriter::new(writer_stream)),
            incoming,
        })
    }

    fn send(&mut self, event: &Event) -> Result<(), CliError> {
        self.writer
            .write_event(event)
            .map_err(CliError::ProtocolEncode)?;
        self.writer.flush()?;
        Ok(())
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<TransportPoll, CliError> {
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(event)) => Ok(TransportPoll::Event(event)),
            Ok(Err(error)) if is_unexpected_eof(&error) => Ok(TransportPoll::Disconnected),
            Ok(Err(error)) => Err(CliError::ProtocolDecode(error)),
            Err(RecvTimeoutError::Timeout) => Ok(TransportPoll::Timeout),
            Err(RecvTimeoutError::Disconnected) => Ok(TransportPoll::Disconnected),
        }
    }
}

#[derive(Debug)]
struct LocalPeerSink {
    peer: Rc<RefCell<LocalPeer>>,
}

impl ConnectionSink for LocalPeerSink {
    fn send(&mut self, event: shlop_core::RoutedEvent) -> Result<(), ConnectionSendError> {
        self.peer
            .borrow_mut()
            .send(&event.event)
            .map_err(|error| ConnectionSendError::new(error.to_string()))
    }
}

struct SocketPeerSink {
    peer: Rc<RefCell<SocketPeer>>,
}

impl ConnectionSink for SocketPeerSink {
    fn send(&mut self, event: shlop_core::RoutedEvent) -> Result<(), ConnectionSendError> {
        self.peer
            .borrow_mut()
            .send(&event.event)
            .map_err(|error| ConnectionSendError::new(error.to_string()))
    }
}

struct SupervisedChildSink {
    child: Rc<RefCell<SupervisedChild>>,
}

impl ConnectionSink for SupervisedChildSink {
    fn send(&mut self, event: shlop_core::RoutedEvent) -> Result<(), ConnectionSendError> {
        self.child
            .borrow_mut()
            .send(&event.event)
            .map_err(|error| ConnectionSendError::new(error.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Extension handle — transport-agnostic wrapper for one extension connection
// ---------------------------------------------------------------------------

/// Transport backend for one extension connection.
enum ExtensionTransport {
    /// In-process via `UnixStream` pair (used by tests and in-process spawning).
    InProcess {
        peer: Rc<RefCell<LocalPeer>>,
        thread: Option<JoinHandle<Result<(), String>>>,
    },
    /// Real child process via stdio.
    Supervised {
        child: Rc<RefCell<SupervisedChild>>,
    },
}

/// One live extension managed by the harness.
struct ExtensionHandle {
    name: String,
    kind: ClientKind,
    connection_id: String,
    connected: bool,
    transport: ExtensionTransport,
}

impl ExtensionHandle {
    fn poll(&self, timeout: Duration) -> Result<TransportPoll, CliError> {
        match &self.transport {
            ExtensionTransport::InProcess { peer, .. } => peer.borrow_mut().recv_timeout(timeout),
            ExtensionTransport::Supervised { child } => {
                let result = child
                    .borrow_mut()
                    .recv_timeout(timeout)
                    .map_err(|e| CliError::Participant(e.to_string()))?;
                match result {
                    Some(event) => Ok(TransportPoll::Event(event)),
                    None => {
                        // Distinguish timeout from child exit.
                        if child
                            .borrow_mut()
                            .try_wait()
                            .map_err(|e| CliError::Participant(e.to_string()))?
                            .is_some()
                        {
                            Ok(TransportPoll::Disconnected)
                        } else {
                            Ok(TransportPoll::Timeout)
                        }
                    }
                }
            }
        }
    }

    fn make_sink(&self) -> Box<dyn ConnectionSink> {
        match &self.transport {
            ExtensionTransport::InProcess { peer, .. } => {
                Box::new(LocalPeerSink { peer: peer.clone() })
            }
            ExtensionTransport::Supervised { child } => {
                Box::new(SupervisedChildSink {
                    child: child.clone(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Harness — the core runtime that manages extensions, routing, and state
// ---------------------------------------------------------------------------

struct Harness {
    bus: EventBus,
    registry: ToolRegistry,
    store: SessionStore,
    pending_request_sessions: VecDeque<String>,
    pending_tool_sessions: std::collections::HashMap<String, String>,
    extension_statuses: std::collections::HashMap<String, Event>,
    lifecycle_messages: Vec<String>,
    extensions: Vec<ExtensionHandle>,
    agent_index: usize,
}

impl Harness {
    /// Creates a harness using in-process extensions (agent, fs, shell).
    fn new(
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
    ) -> Result<Self, CliError> {
        let agent = spawn_local_participant("agent", ClientKind::Agent, |reader, writer| {
            shlop_agent::run(reader, writer).map_err(|error| error.to_string())
        })?;
        let filesystem_tool =
            spawn_local_participant("filesystem-tool", ClientKind::Tool, |reader, writer| {
                shlop_ext_fs::run(reader, writer).map_err(|error| error.to_string())
            })?;
        let shell_tool =
            spawn_local_participant("shell-tool", ClientKind::Tool, |reader, writer| {
                shlop_ext_shell::run(reader, writer).map_err(|error| error.to_string())
            })?;
        Self::init(vec![agent, filesystem_tool, shell_tool], store_path, policy_store_path)
    }

    /// Creates a harness from loaded configuration, spawning real child
    /// processes.
    fn from_config(
        config: &Config,
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
    ) -> Result<Self, CliError> {
        let extensions = config
            .extensions
            .iter()
            .map(spawn_supervised_extension)
            .collect::<Result<Vec<_>, _>>()?;
        Self::init(extensions, store_path, policy_store_path)
    }

    fn init(
        mut extensions: Vec<ExtensionHandle>,
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
    ) -> Result<Self, CliError> {
        let mut bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(PolicyStore::open(policy_store_path.into())?),
        ));
        let store = SessionStore::open(store_path)?;

        let agent_index = extensions
            .iter()
            .position(|ext| ext.kind == ClientKind::Agent)
            .ok_or(CliError::NoAgentConfigured)?;

        for ext in &mut extensions {
            let connection_id = bus.connect(Connection::new(
                ConnectionMetadata {
                    id: String::new(),
                    name: ext.name.clone(),
                    kind: ext.kind.clone(),
                    origin: ConnectionOrigin::Supervised,
                },
                ext.make_sink(),
            ));
            ext.connection_id = connection_id;
            ext.connected = true;
        }

        let mut harness = Self {
            bus,
            registry: ToolRegistry::new(),
            store,
            pending_request_sessions: VecDeque::new(),
            pending_tool_sessions: std::collections::HashMap::new(),
            extension_statuses: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            extensions,
            agent_index,
        };

        for i in 0..harness.extensions.len() {
            let name = harness.extensions[i].name.clone();
            harness.emit_extension_starting(&name);
        }
        harness.wait_for_startup()?;
        Ok(harness)
    }

    fn wait_for_startup(&mut self) -> Result<(), CliError> {
        let started_at = Instant::now();
        let total = self.extensions.len();
        let mut ready_count = 0;

        while ready_count < total {
            if STARTUP_TIMEOUT <= started_at.elapsed() {
                return Err(CliError::StartupTimeout);
            }
            for i in 0..self.extensions.len() {
                if !self.extensions[i].connected {
                    continue;
                }
                match self.extensions[i].poll(POLL_INTERVAL)? {
                    TransportPoll::Event(event) => {
                        if matches!(event, Event::LifecycleReady(_)) {
                            ready_count += 1;
                        }
                        let conn_id = self.extensions[i].connection_id.clone();
                        let _ =
                            self.handle_participant_event(&conn_id, event, &mut Vec::new())?;
                    }
                    TransportPoll::Disconnected => {
                        let name = self.extensions[i].name.clone();
                        self.handle_extension_disconnect(i);
                        return Err(CliError::Participant(format!(
                            "{name} disconnected during startup"
                        )));
                    }
                    TransportPoll::Timeout => {}
                }
            }
        }
        Ok(())
    }

    fn send_user_message(
        &mut self,
        session_id: &str,
        text: &str,
        source_id: Option<&str>,
    ) -> Result<InteractionOutcome, CliError> {
        self.store
            .append_user_message(session_id.to_owned(), text.to_owned())?;
        self.pending_request_sessions
            .push_back(session_id.to_owned());
        let _ = self.bus.publish_from(
            source_id,
            Event::MessageUser(ChatMessage {
                session_id: Some(session_id.to_owned()),
                text: text.to_owned(),
            }),
        );
        self.wait_for_agent_response()
    }

    fn wait_for_agent_response(&mut self) -> Result<InteractionOutcome, CliError> {
        let started_at = Instant::now();
        let mut progress_messages = Vec::new();
        loop {
            if RESPONSE_TIMEOUT <= started_at.elapsed() {
                return Err(CliError::ResponseTimeout);
            }
            if let Some(response) = self.poll_participants_once(&mut progress_messages)? {
                return Ok(InteractionOutcome {
                    lifecycle_messages: Vec::new(),
                    progress_messages,
                    response: response.text,
                });
            }
        }
    }

    fn poll_participants_once(
        &mut self,
        progress_messages: &mut Vec<String>,
    ) -> Result<Option<ChatMessage>, CliError> {
        for i in 0..self.extensions.len() {
            if !self.extensions[i].connected {
                continue;
            }
            match self.extensions[i].poll(POLL_INTERVAL)? {
                TransportPoll::Event(event) => {
                    let conn_id = self.extensions[i].connection_id.clone();
                    if let Some(message) =
                        self.handle_participant_event(&conn_id, event, progress_messages)?
                    {
                        return Ok(Some(message));
                    }
                }
                TransportPoll::Disconnected => {
                    let is_agent = i == self.agent_index;
                    let name = self.extensions[i].name.clone();
                    self.handle_extension_disconnect(i);
                    if is_agent {
                        return Err(CliError::Participant(format!("{name} disconnected")));
                    }
                }
                TransportPoll::Timeout => {}
            }
        }
        Ok(None)
    }

    fn handle_participant_event(
        &mut self,
        source_id: &str,
        event: Event,
        progress_messages: &mut Vec<String>,
    ) -> Result<Option<ChatMessage>, CliError> {
        match event {
            Event::LifecycleHello(hello) => {
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::LifecycleHello(hello));
                Ok(None)
            }
            Event::LifecycleSubscribe(subscribe) => {
                self.bus
                    .set_subscriptions(source_id, subscribe.selectors.clone())?;
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::LifecycleSubscribe(subscribe));
                Ok(None)
            }
            Event::LifecycleReady(ready) => {
                self.emit_extension_ready(source_id);
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::LifecycleReady(ready));
                Ok(None)
            }
            Event::ToolRegister(ToolRegister { tool }) => {
                let _ = self.registry.register(source_id, tool);
                Ok(None)
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
                    Err(error) => return Err(CliError::ToolRoute(error)),
                }
                Ok(None)
            }
            Event::ToolResult(result) => {
                self.persist_tool_result(&result)?;
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::ToolResult(result));
                Ok(None)
            }
            Event::ToolError(error) => {
                self.persist_tool_error(&error)?;
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::ToolError(error));
                Ok(None)
            }
            Event::ToolProgress(progress) => {
                progress_messages.push(format_tool_progress(&progress));
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::ToolProgress(progress));
                Ok(None)
            }
            Event::MessageAgent(message) => {
                if let Some(session_id) = &message.session_id {
                    self.store
                        .append_agent_message(session_id.clone(), message.text.clone())?;
                }
                let _ = self
                    .bus
                    .publish_from(Some(source_id), Event::MessageAgent(message.clone()));
                Ok(Some(message))
            }
            other => {
                let _ = self.bus.publish_from(Some(source_id), other);
                Ok(None)
            }
        }
    }

    fn persist_tool_request(&mut self, request: &ToolRequest) -> Result<(), CliError> {
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

    fn persist_tool_result(&mut self, result: &ToolResult) -> Result<(), CliError> {
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

    fn persist_tool_error(&mut self, error: &ToolError) -> Result<(), CliError> {
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

    fn handle_client_event(&mut self, client_id: &str, event: Event) -> Result<bool, CliError> {
        match event {
            Event::LifecycleHello(hello) => {
                let _ = self
                    .bus
                    .publish_from(Some(client_id), Event::LifecycleHello(hello));
                Ok(true)
            }
            Event::LifecycleSubscribe(subscribe) => {
                self.bus
                    .set_subscriptions(client_id, subscribe.selectors.clone())?;
                let replay_extension_statuses = wants_extension_events(&subscribe.selectors);
                let _ = self
                    .bus
                    .publish_from(Some(client_id), Event::LifecycleSubscribe(subscribe));
                if replay_extension_statuses {
                    self.replay_extension_statuses(client_id)?;
                }
                Ok(true)
            }
            Event::MessageUser(message) => {
                let session_id = message
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "default".to_owned());
                let _ = self.send_user_message(&session_id, &message.text, Some(client_id))?;
                Ok(true)
            }
            Event::LifecycleDisconnect(_) => Ok(false),
            other => {
                let _ = self.bus.publish_from(Some(client_id), other);
                Ok(true)
            }
        }
    }

    fn attach_socket_client(&mut self, peer: Rc<RefCell<SocketPeer>>) -> String {
        self.bus.connect(Connection::new(
            ConnectionMetadata {
                id: String::new(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin: ConnectionOrigin::Socket,
            },
            Box::new(SocketPeerSink { peer }),
        ))
    }

    fn shutdown(&mut self) -> Result<(), CliError> {
        for ext in &self.extensions {
            let _ = self.bus.send_to(
                &ext.connection_id,
                None,
                Event::LifecycleDisconnect(LifecycleDisconnect {
                    reason: Some("shutdown".to_owned()),
                }),
            );
        }

        for i in 0..self.extensions.len() {
            match &mut self.extensions[i].transport {
                ExtensionTransport::InProcess { thread, .. } => {
                    if let Some(handle) = thread.take() {
                        let name = self.extensions[i].name.clone();
                        let result = handle
                            .join()
                            .map_err(|_| CliError::ThreadJoin(name))?;
                        result.map_err(CliError::Participant)?;
                    }
                }
                ExtensionTransport::Supervised { child } => {
                    let _ = child.borrow_mut().wait_for_exit(Duration::from_secs(5));
                }
            }
            let name = self.extensions[i].name.clone();
            self.emit_extension_exited(&name);
        }
        Ok(())
    }

    fn emit_extension_starting(&mut self, extension_name: &str) {
        let event = Event::ExtensionStarting(shlop_proto::ExtensionStarting {
            extension_name: extension_name.to_owned(),
            argv: Vec::new(),
        });
        self.extension_statuses
            .insert(extension_name.to_owned(), event.clone());
        self.lifecycle_messages.push(format_extension_event(&event));
        let _ = self.bus.publish(event);
    }

    fn handle_extension_disconnect(&mut self, index: usize) {
        let ext = &mut self.extensions[index];
        ext.connected = false;
        let conn_id = ext.connection_id.clone();
        let name = ext.name.clone();
        let _ = self.bus.disconnect(&conn_id);
        let _ = self.registry.unregister_connection(&conn_id);
        self.emit_extension_exited(&name);
    }

    fn emit_extension_ready(&mut self, connection_id: &str) {
        let Some(connection) = self.bus.connection(connection_id).cloned() else {
            return;
        };
        let extension_name = connection.name;
        let event = Event::ExtensionReady(shlop_proto::ExtensionReady {
            extension_name: extension_name.clone(),
            connection_id: Some(connection.id),
        });
        self.extension_statuses
            .insert(extension_name, event.clone());
        self.lifecycle_messages.push(format_extension_event(&event));
        let _ = self.bus.publish(event);
    }

    fn emit_extension_exited(&mut self, extension_name: &str) {
        let event = Event::ExtensionExited(shlop_proto::ExtensionExited {
            extension_name: extension_name.to_owned(),
            exit_code: None,
            signal: None,
        });
        self.extension_statuses
            .insert(extension_name.to_owned(), event.clone());
        self.lifecycle_messages.push(format_extension_event(&event));
        let _ = self.bus.publish(event);
    }

    fn replay_extension_statuses(&mut self, client_id: &str) -> Result<(), CliError> {
        let mut extension_names = self.extension_statuses.keys().cloned().collect::<Vec<_>>();
        extension_names.sort();
        for extension_name in extension_names {
            if let Some(event) = self.extension_statuses.get(&extension_name).cloned() {
                let _ = self.bus.send_to(client_id, None, event)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn find_extension(&self, name: &str) -> Option<&ExtensionHandle> {
        self.extensions.iter().find(|ext| ext.name == name)
    }
}

// ---------------------------------------------------------------------------
// Extension spawning helpers
// ---------------------------------------------------------------------------

fn spawn_local_participant<F>(
    name: &str,
    kind: ClientKind,
    run: F,
) -> Result<ExtensionHandle, CliError>
where
    F: FnOnce(UnixStream, UnixStream) -> Result<(), String> + Send + 'static,
{
    let (runtime_stream, core_stream) = UnixStream::pair()?;
    let reader_stream = runtime_stream.try_clone()?;
    let peer = Rc::new(RefCell::new(LocalPeer::new(core_stream)?));
    let thread = thread::spawn(move || run(reader_stream, runtime_stream));
    Ok(ExtensionHandle {
        name: name.to_owned(),
        kind,
        connection_id: String::new(),
        connected: false,
        transport: ExtensionTransport::InProcess {
            peer,
            thread: Some(thread),
        },
    })
}

fn spawn_supervised_extension(config: &ExtensionConfig) -> Result<ExtensionHandle, CliError> {
    let kind = match config.role.as_deref() {
        Some("agent") => ClientKind::Agent,
        _ => ClientKind::Tool,
    };
    let command = ExtensionCommand {
        name: config.name.clone(),
        program: PathBuf::from(&config.command),
        args: config.args.clone(),
    };
    let child = SupervisedChild::spawn(command)?;
    Ok(ExtensionHandle {
        name: config.name.clone(),
        kind,
        connection_id: String::new(),
        connected: false,
        transport: ExtensionTransport::Supervised {
            child: Rc::new(RefCell::new(child)),
        },
    })
}

fn spawn_reader(stream: UnixStream) -> Receiver<Result<Event, DecodeError>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = EventReader::new(BufReader::new(stream));
        loop {
            match reader.read_event() {
                Ok(event) => {
                    if sender.send(Ok(event)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
    });
    receiver
}

fn is_unexpected_eof(error: &DecodeError) -> bool {
    match error {
        DecodeError::Io(source) => source.kind() == io::ErrorKind::UnexpectedEof,
        _ => false,
    }
}

fn wants_extension_events(selectors: &[EventSelector]) -> bool {
    selectors.iter().any(|selector| match selector {
        EventSelector::Exact(name) => name.as_str().starts_with("extension."),
        EventSelector::Prefix(prefix) => prefix.starts_with("extension."),
    })
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn format_tool_progress(progress: &ToolProgress) -> String {
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

fn format_extension_event(event: &Event) -> String {
    match event {
        Event::ExtensionStarting(starting) => {
            format!("extension {} starting", starting.extension_name)
        }
        Event::ExtensionReady(ready) => {
            format!("extension {} ready", ready.extension_name)
        }
        Event::ExtensionExited(exited) => {
            format!("extension {} exited", exited.extension_name)
        }
        Event::ExtensionRestarting(restarting) => {
            format!("extension {} restarting", restarting.extension_name)
        }
        _ => event.name().to_string(),
    }
}

fn format_session_entry(entry: &SessionEntry) -> String {
    match entry {
        SessionEntry::UserMessage { text } => format!("user: {text}"),
        SessionEntry::AgentMessage { text } => format!("agent: {text}"),
        SessionEntry::ToolActivity(activity) => format_tool_activity(activity),
    }
}

fn format_tool_activity(activity: &ToolActivityRecord) -> String {
    match &activity.outcome {
        ToolActivityOutcome::Requested { .. } => {
            format!("tool.request {} ({})", activity.tool_name, activity.call_id)
        }
        ToolActivityOutcome::Result { result } => {
            format!(
                "tool.result {} ({}) -> {result:?}",
                activity.tool_name, activity.call_id
            )
        }
        ToolActivityOutcome::Error { message, .. } => {
            format!(
                "tool.error {} ({}) -> {}",
                activity.tool_name, activity.call_id, message
            )
        }
    }
}

fn latest_agent_preview(session: &shlop_core::SessionSnapshot) -> Option<String> {
    session.entries.iter().rev().find_map(|entry| match entry {
        SessionEntry::AgentMessage { text } => Some(text.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Public API — in-process (test-friendly) path
// ---------------------------------------------------------------------------

/// Runs one embedded interaction and returns progress plus the final agent
/// response.
pub fn run_embedded_message_with_trace(
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, CliError> {
    let session_store_path = session_store_path.into();
    let mut harness = Harness::new(
        session_store_path.clone(),
        default_policy_store_path_from_session_store(&session_store_path),
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
) -> Result<String, CliError> {
    Ok(run_embedded_message_with_trace(session_store_path, session_id, message)?.response)
}

/// Runs the foreground daemon loop that accepts later socket clients.
pub fn run_daemon(
    socket_path: impl Into<PathBuf>,
    session_store_path: impl Into<PathBuf>,
    options: ServeOptions,
) -> Result<(), CliError> {
    let socket_path = socket_path.into();
    let session_store_path = session_store_path.into();
    let listener = SocketListener::bind(&socket_path)?;
    let policy_store_path = options
        .policy_store_path
        .clone()
        .unwrap_or_else(|| default_policy_store_path_from_session_store(&session_store_path));
    let mut harness = Harness::new(session_store_path, policy_store_path)?;
    serve_daemon_loop(&mut harness, &listener, &options)
}

// ---------------------------------------------------------------------------
// Public API — config-driven (real subprocess) path
// ---------------------------------------------------------------------------

/// Runs one embedded interaction using extensions from configuration.
pub fn run_embedded_message_with_config(
    config: &Config,
    session_store_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, CliError> {
    let session_store_path = session_store_path.into();
    let mut harness = Harness::from_config(
        config,
        session_store_path.clone(),
        default_policy_store_path_from_session_store(&session_store_path),
    )?;
    let mut outcome = harness.send_user_message(session_id, message, None)?;
    harness.shutdown()?;
    outcome.lifecycle_messages = harness.lifecycle_messages;
    Ok(outcome)
}

/// Runs the foreground daemon loop using extensions from configuration.
pub fn run_daemon_with_config(
    config: &Config,
    socket_path: impl Into<PathBuf>,
    session_store_path: impl Into<PathBuf>,
    options: ServeOptions,
) -> Result<(), CliError> {
    let socket_path = socket_path.into();
    let session_store_path = session_store_path.into();
    let listener = SocketListener::bind(&socket_path)?;
    let policy_store_path = options
        .policy_store_path
        .clone()
        .unwrap_or_else(|| default_policy_store_path_from_session_store(&session_store_path));
    let mut harness = Harness::from_config(config, session_store_path, policy_store_path)?;
    serve_daemon_loop(&mut harness, &listener, &options)
}

fn serve_daemon_loop(
    harness: &mut Harness,
    listener: &SocketListener,
    options: &ServeOptions,
) -> Result<(), CliError> {
    let mut handled_clients = 0_usize;

    loop {
        if options
            .max_clients
            .is_some_and(|max_clients| max_clients < handled_clients + 1)
        {
            break;
        }

        let peer = Rc::new(RefCell::new(listener.accept()?));
        let client_id = harness.attach_socket_client(peer.clone());

        let mut keep_client = true;
        while keep_client {
            let client_event = { peer.borrow_mut().recv_timeout(POLL_INTERVAL)? };
            if let Some(event) = client_event {
                match harness.handle_client_event(&client_id, event) {
                    Ok(new_keep_client) => keep_client = new_keep_client,
                    Err(CliError::Route(RouteError::SubscriptionDenied { reason, .. })) => {
                        peer.borrow_mut().send(&Event::LifecycleDisconnect(
                            LifecycleDisconnect {
                                reason: Some(format!("subscription denied: {reason}")),
                            },
                        ))?;
                        keep_client = false;
                    }
                    Err(error) => return Err(error),
                }
            }
            let _ = harness.poll_participants_once(&mut Vec::new())?;
        }

        let _ = harness.bus.disconnect(&client_id);
        handled_clients += 1;
    }

    harness.shutdown()
}

/// Sends one user message to a running daemon and returns progress plus the
/// final response.
pub fn send_daemon_message_with_trace(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, CliError> {
    let mut peer = SocketPeer::connect(socket_path)?;
    peer.send(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "shlop-cli".to_owned(),
        client_kind: ClientKind::Ui,
    }))?;
    peer.send(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::MessageAgent),
            EventSelector::Exact(EventName::ToolProgress),
            EventSelector::Prefix("extension.".to_owned()),
        ],
    }))?;
    peer.send(&Event::MessageUser(ChatMessage {
        session_id: Some(session_id.to_owned()),
        text: message.to_owned(),
    }))?;

    let started_at = Instant::now();
    let mut lifecycle_messages = Vec::new();
    let mut progress_messages = Vec::new();
    loop {
        if RESPONSE_TIMEOUT <= started_at.elapsed() {
            return Err(CliError::ResponseTimeout);
        }
        if let Some(event) = peer.recv_timeout(POLL_INTERVAL)? {
            match event {
                Event::ToolProgress(progress) => {
                    progress_messages.push(format_tool_progress(&progress))
                }
                Event::ExtensionStarting(_)
                | Event::ExtensionReady(_)
                | Event::ExtensionExited(_)
                | Event::ExtensionRestarting(_) => {
                    lifecycle_messages.push(format_extension_event(&event))
                }
                Event::MessageAgent(message) => {
                    peer.send(&Event::LifecycleDisconnect(LifecycleDisconnect {
                        reason: Some("done".to_owned()),
                    }))?;
                    return Ok(InteractionOutcome {
                        lifecycle_messages,
                        progress_messages,
                        response: message.text,
                    });
                }
                Event::LifecycleDisconnect(disconnect) => {
                    return Err(CliError::Participant(
                        disconnect
                            .reason
                            .unwrap_or_else(|| "daemon disconnected".to_owned()),
                    ));
                }
                _ => {}
            }
        }
    }
}

/// Sends one user message to a running daemon and returns the final response.
pub fn send_daemon_message(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, CliError> {
    Ok(send_daemon_message_with_trace(socket_path, session_id, message)?.response)
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Returns the default session-store path used by the CLI.
#[must_use]
pub fn default_session_store_path() -> PathBuf {
    PathBuf::from(".shlop").join("sessions.cbor")
}

/// Returns the default policy-store path used by the CLI.
#[must_use]
pub fn default_policy_store_path() -> PathBuf {
    PathBuf::from(".shlop").join("policy.cbor")
}

fn default_policy_store_path_from_session_store(session_store_path: &Path) -> PathBuf {
    session_store_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("policy.cbor")
}

/// Returns the default daemon socket path used by the CLI.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    PathBuf::from(".shlop").join("daemon.sock")
}

/// Returns the default session ID used by the CLI.
#[must_use]
pub fn default_session_id() -> &'static str {
    "default"
}

// ---------------------------------------------------------------------------
// Inspection helpers
// ---------------------------------------------------------------------------

/// Opens the session store for inspection or tests.
pub fn open_session_store(path: impl AsRef<Path>) -> Result<SessionStore, CliError> {
    SessionStore::open(path.as_ref()).map_err(CliError::from)
}

/// Formats one session as printable lines for CLI output.
pub fn session_lines(path: impl AsRef<Path>, session_id: &str) -> Result<Vec<String>, CliError> {
    let store = open_session_store(path)?;
    let Some(session) = store.session(session_id) else {
        return Ok(vec![format!("session {session_id} not found")]);
    };

    Ok(session
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| format!("{}: {}", index + 1, format_session_entry(entry)))
        .collect())
}

/// Formats all known sessions as one-line summaries for CLI output.
pub fn session_list_lines(path: impl AsRef<Path>) -> Result<Vec<String>, CliError> {
    let store = open_session_store(path)?;
    let mut sessions = store.sessions();
    sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    if sessions.is_empty() {
        return Ok(vec!["no sessions".to_owned()]);
    }
    Ok(sessions
        .into_iter()
        .map(|session| {
            format!(
                "{} ({} entries){}",
                session.session_id,
                session.entries.len(),
                latest_agent_preview(session)
                    .map(|preview| format!(": {preview}"))
                    .unwrap_or_default()
            )
        })
        .collect())
}

/// Opens the policy store for inspection or tests.
pub fn open_policy_store(path: impl AsRef<Path>) -> Result<PolicyStore, CliError> {
    PolicyStore::open(path.as_ref()).map_err(CliError::from)
}

/// Formats persisted policy approvals as printable lines for CLI output.
pub fn policy_lines(path: impl AsRef<Path>) -> Result<Vec<String>, CliError> {
    let store = open_policy_store(path)?;
    let mut approvals = store.approvals().to_vec();
    approvals.sort_by(|left, right| left.connection_name.cmp(&right.connection_name));
    if approvals.is_empty() {
        return Ok(vec!["no policy approvals".to_owned()]);
    }
    Ok(approvals
        .into_iter()
        .map(|approval| {
            let selectors = approval
                .selectors
                .iter()
                .map(|selector| match selector {
                    EventSelector::Exact(name) => name.as_str().to_owned(),
                    EventSelector::Prefix(prefix) => format!("{prefix}*"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{} [{:?}] -> {}",
                approval.connection_name, approval.connection_origin, selectors
            )
        })
        .collect())
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
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("sessions.cbor");

        let response = run_embedded_message(&store_path, "session-1", "hello")
            .expect("embedded interaction should succeed");
        assert!(response.contains("demo.echo returned"));

        let store = open_session_store(&store_path).expect("store should reopen");
        let session = store.session("session-1").expect("session should exist");
        assert_eq!(session.entries.len(), 4);
    }

    #[test]
    fn daemon_mode_accepts_later_clients_and_continues_same_session() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let socket_path = tempdir.path().join("daemon.sock");
        let store_path = tempdir.path().join("sessions.cbor");

        let server = thread::spawn({
            let socket_path = socket_path.clone();
            let store_path = store_path.clone();
            move || {
                run_daemon(
                    socket_path,
                    store_path,
                    ServeOptions {
                        max_clients: Some(2),
                        policy_store_path: None,
                    },
                )
            }
        });

        let started_at = Instant::now();
        while !socket_path.exists() {
            if Duration::from_secs(2) <= started_at.elapsed() {
                panic!("daemon socket was not created in time");
            }
            thread::sleep(Duration::from_millis(10));
        }

        let first = send_daemon_message(&socket_path, "session-1", "hello")
            .expect("first client message should succeed");
        let second = send_daemon_message(&socket_path, "session-1", "again")
            .expect("second client message should succeed");
        assert!(first.contains("demo.echo returned"));
        assert!(second.contains("demo.echo returned"));

        server
            .join()
            .expect("server thread should finish")
            .expect("daemon should exit cleanly");

        let store = open_session_store(&store_path).expect("store should reopen");
        let session = store.session("session-1").expect("session should exist");
        assert_eq!(session.entries.len(), 8);
    }

    #[test]
    fn embedded_mode_can_read_files() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("sessions.cbor");
        let file_path = tempdir.path().join("note.txt");
        std::fs::write(&file_path, "hello from disk").expect("fixture file should be written");

        let response = run_embedded_message(
            &store_path,
            "session-1",
            &format!("read {}", file_path.display()),
        )
        .expect("embedded file read should succeed");
        assert!(response.contains("fs.read"));
        assert!(response.contains("hello from disk"));
    }

    #[test]
    fn embedded_mode_can_run_shell_commands() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("sessions.cbor");

        let response = run_embedded_message(&store_path, "session-1", "shell printf hi")
            .expect("embedded shell command should succeed");
        assert!(response.contains("shell.exec status 0"));
        assert!(response.contains("stdout:\nhi"));
    }

    #[test]
    fn unavailable_tool_is_reported_without_crashing_harness() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("sessions.cbor");
        let policy_path = tempdir.path().join("policy.cbor");
        let mut harness = Harness::new(&store_path, &policy_path).expect("harness should start");

        let shell_conn_id = harness
            .find_extension("shell-tool")
            .expect("shell-tool should exist")
            .connection_id
            .clone();
        let removed_tools = harness.registry.unregister_connection(&shell_conn_id);
        assert!(
            removed_tools
                .iter()
                .any(|tool_name| tool_name == "shell.exec")
        );

        let outcome = harness
            .send_user_message("session-1", "shell printf hi", None)
            .expect("interaction should succeed with synthetic error");
        assert!(
            outcome
                .response
                .contains("shell.exec failed: no live provider available")
        );

        harness.shutdown().expect("harness should shut down");
    }

    #[test]
    fn disconnected_tool_is_removed_and_reported_cleanly() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("sessions.cbor");
        let policy_path = tempdir.path().join("policy.cbor");
        let mut harness = Harness::new(&store_path, &policy_path).expect("harness should start");

        let shell_conn_id = harness
            .find_extension("shell-tool")
            .expect("shell-tool should exist")
            .connection_id
            .clone();
        harness
            .bus
            .send_to(
                &shell_conn_id,
                None,
                Event::LifecycleDisconnect(LifecycleDisconnect {
                    reason: Some("test shutdown".to_owned()),
                }),
            )
            .expect("disconnect should route");
        for _ in 0..10 {
            let _ = harness.poll_participants_once(&mut Vec::new());
            if !harness
                .find_extension("shell-tool")
                .expect("shell-tool should exist")
                .connected
            {
                break;
            }
        }

        assert!(
            !harness
                .find_extension("shell-tool")
                .expect("shell-tool should exist")
                .connected
        );
        assert!(harness.registry.providers_for("shell.exec").is_empty());
        assert!(
            harness
                .lifecycle_messages
                .iter()
                .any(|message| message == "extension shell-tool exited")
        );

        let outcome = harness
            .send_user_message("session-1", "shell printf hi", None)
            .expect("interaction should succeed with synthetic error");
        assert!(outcome.response.contains("no live provider available"));

        harness.shutdown().expect("harness should shut down");
    }

    #[test]
    fn traced_embedded_interaction_reports_shell_progress() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("sessions.cbor");

        let outcome = run_embedded_message_with_trace(&store_path, "session-1", "shell printf hi")
            .expect("embedded shell command should succeed");
        assert_eq!(
            outcome.progress_messages,
            vec!["shell.exec: running shell command".to_owned()]
        );
        assert!(outcome.response.contains("shell.exec status 0"));
    }

    #[test]
    fn traced_daemon_interaction_reports_shell_progress() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let socket_path = tempdir.path().join("daemon.sock");
        let store_path = tempdir.path().join("sessions.cbor");

        let server = thread::spawn({
            let socket_path = socket_path.clone();
            let store_path = store_path.clone();
            move || {
                run_daemon(
                    socket_path,
                    store_path,
                    ServeOptions {
                        max_clients: Some(1),
                        policy_store_path: None,
                    },
                )
            }
        });

        let started_at = Instant::now();
        while !socket_path.exists() {
            if Duration::from_secs(2) <= started_at.elapsed() {
                panic!("daemon socket was not created in time");
            }
            thread::sleep(Duration::from_millis(10));
        }

        let outcome = send_daemon_message_with_trace(&socket_path, "session-1", "shell printf hi")
            .expect("daemon shell command should succeed");
        assert!(
            outcome
                .lifecycle_messages
                .iter()
                .any(|message| message == "extension agent ready")
        );
        assert!(
            outcome
                .lifecycle_messages
                .iter()
                .any(|message| message == "extension filesystem-tool ready")
        );
        assert!(
            outcome
                .lifecycle_messages
                .iter()
                .any(|message| message == "extension shell-tool ready")
        );
        assert_eq!(
            outcome.progress_messages,
            vec!["shell.exec: running shell command".to_owned()]
        );
        assert!(outcome.response.contains("shell.exec status 0"));

        server
            .join()
            .expect("server thread should finish")
            .expect("daemon should exit cleanly");
    }

    #[test]
    fn traced_embedded_interaction_reports_lifecycle_messages() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("sessions.cbor");

        let outcome = run_embedded_message_with_trace(&store_path, "session-1", "hello")
            .expect("embedded interaction should succeed");
        assert!(
            outcome
                .lifecycle_messages
                .iter()
                .any(|message| message == "extension agent starting")
        );
        assert!(
            outcome
                .lifecycle_messages
                .iter()
                .any(|message| message == "extension agent ready")
        );
        assert!(
            outcome
                .lifecycle_messages
                .iter()
                .any(|message| message == "extension agent exited")
        );
    }

    #[test]
    fn session_and_policy_lines_are_printable() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let socket_path = tempdir.path().join("daemon.sock");
        let store_path = tempdir.path().join("sessions.cbor");
        let policy_path = tempdir.path().join("policy.cbor");

        let server = thread::spawn({
            let socket_path = socket_path.clone();
            let store_path = store_path.clone();
            let policy_path = policy_path.clone();
            move || {
                run_daemon(
                    socket_path,
                    store_path,
                    ServeOptions {
                        max_clients: Some(1),
                        policy_store_path: Some(policy_path),
                    },
                )
            }
        });

        let started_at = Instant::now();
        while !socket_path.exists() {
            if Duration::from_secs(2) <= started_at.elapsed() {
                panic!("daemon socket was not created in time");
            }
            thread::sleep(Duration::from_millis(10));
        }

        let _ = send_daemon_message_with_trace(&socket_path, "session-1", "hello")
            .expect("daemon interaction should succeed");
        server
            .join()
            .expect("server thread should finish")
            .expect("daemon should exit cleanly");

        let session_output =
            session_lines(&store_path, "session-1").expect("session lines should load");
        assert!(
            session_output
                .iter()
                .any(|line| line.contains("user: hello"))
        );
        assert!(
            session_output
                .iter()
                .any(|line| line.contains("tool.request demo.echo"))
        );

        let session_list_output =
            session_list_lines(&store_path).expect("session list should load");
        assert!(
            session_list_output
                .iter()
                .any(|line| line.contains("session-1 (4 entries)"))
        );

        let policy_output = policy_lines(&policy_path).expect("policy lines should load");
        assert!(policy_output.iter().any(|line| line.contains("socket-ui")));
    }

    #[test]
    fn empty_session_and_policy_views_are_friendly() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("sessions.cbor");
        let policy_path = tempdir.path().join("policy.cbor");

        assert_eq!(
            session_list_lines(&store_path).expect("session list should load"),
            vec!["no sessions".to_owned()]
        );
        assert_eq!(
            policy_lines(&policy_path).expect("policy lines should load"),
            vec!["no policy approvals".to_owned()]
        );
        assert_eq!(
            session_lines(&store_path, "missing").expect("session lines should load"),
            vec!["session missing not found".to_owned()]
        );
    }

    #[test]
    fn daemon_disconnect_reason_is_reported_to_cli_clients() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let socket_path = tempdir.path().join("daemon.sock");
        let listener = SocketListener::bind(&socket_path).expect("listener should bind");

        let server = thread::spawn(move || {
            let mut peer = listener.accept().expect("client should connect");
            let _ = peer
                .recv_timeout(Duration::from_secs(1))
                .expect("hello should decode")
                .expect("hello should arrive");
            let _ = peer
                .recv_timeout(Duration::from_secs(1))
                .expect("subscribe should decode")
                .expect("subscribe should arrive");
            let _ = peer
                .recv_timeout(Duration::from_secs(1))
                .expect("message should decode")
                .expect("message should arrive");
            peer.send(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: Some("test disconnect".to_owned()),
            }))
            .expect("disconnect should send");
        });

        let error = send_daemon_message_with_trace(&socket_path, "session-1", "hello")
            .expect_err("client should receive disconnect error");
        assert!(matches!(error, CliError::Participant(reason) if reason == "test disconnect"));
        server.join().expect("server thread should finish");
    }
}
