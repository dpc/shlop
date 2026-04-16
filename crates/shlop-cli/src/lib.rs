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

use shlop_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    DefaultSubscriptionPolicy, EventBus, PolicyStore, RouteError, SessionStore, SessionStoreError,
    ToolActivityOutcome, ToolActivityRecord, ToolRegistry, ToolRouteError,
};
use shlop_proto::{
    ChatMessage, ClientKind, DecodeError, Event, EventName, EventReader, EventSelector,
    EventWriter, LifecycleDisconnect, LifecycleHello, LifecycleSubscribe, PROTOCOL_VERSION,
    ProgressUpdate, ToolError, ToolProgress, ToolRegister, ToolRequest, ToolResult,
};
use shlop_socket::{SocketListener, SocketPeer, SocketTransportError};

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
    Route(RouteError),
    ToolRoute(ToolRouteError),
    StartupTimeout,
    ResponseTimeout,
    ThreadJoin(&'static str),
    Participant(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::ProtocolDecode(source) => write!(f, "protocol decode error: {source}"),
            Self::ProtocolEncode(source) => write!(f, "protocol encode error: {source}"),
            Self::SessionStore(source) => write!(f, "session store error: {source}"),
            Self::SocketTransport(source) => write!(f, "socket transport error: {source}"),
            Self::Route(source) => write!(f, "routing error: {source}"),
            Self::ToolRoute(source) => write!(f, "tool routing error: {source}"),
            Self::StartupTimeout => {
                f.write_str("timed out waiting for local participants to start")
            }
            Self::ResponseTimeout => f.write_str("timed out waiting for agent response"),
            Self::ThreadJoin(name) => write!(f, "failed to join {name} thread cleanly"),
            Self::Participant(message) => write!(f, "participant error: {message}"),
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
            Self::Route(source) => Some(source),
            Self::ToolRoute(source) => Some(source),
            Self::StartupTimeout => None,
            Self::ResponseTimeout => None,
            Self::ThreadJoin(_) => None,
            Self::Participant(_) => None,
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

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Event>, CliError> {
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(event)) => Ok(Some(event)),
            Ok(Err(error)) if is_unexpected_eof(&error) => Ok(None),
            Ok(Err(error)) => Err(CliError::ProtocolDecode(error)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Ok(None),
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

#[derive(Debug)]
struct ParticipantHandle {
    connection_id: String,
    peer: Rc<RefCell<LocalPeer>>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

struct Harness {
    bus: EventBus,
    registry: ToolRegistry,
    store: SessionStore,
    pending_request_sessions: VecDeque<String>,
    pending_tool_sessions: std::collections::HashMap<String, String>,
    agent: ParticipantHandle,
    filesystem_tool: ParticipantHandle,
    shell_tool: ParticipantHandle,
}

impl Harness {
    fn new(
        store_path: impl Into<PathBuf>,
        policy_store_path: impl Into<PathBuf>,
    ) -> Result<Self, CliError> {
        let mut bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(PolicyStore::open(policy_store_path.into())?),
        ));
        let store = SessionStore::open(store_path)?;

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

        let agent_id = bus.connect(Connection::new(
            ConnectionMetadata {
                id: String::new(),
                name: "agent".to_owned(),
                kind: ClientKind::Agent,
                origin: ConnectionOrigin::Supervised,
            },
            Box::new(LocalPeerSink {
                peer: agent.peer.clone(),
            }),
        ));
        let filesystem_tool_id = bus.connect(Connection::new(
            ConnectionMetadata {
                id: String::new(),
                name: "filesystem-tool".to_owned(),
                kind: ClientKind::Tool,
                origin: ConnectionOrigin::Supervised,
            },
            Box::new(LocalPeerSink {
                peer: filesystem_tool.peer.clone(),
            }),
        ));
        let shell_tool_id = bus.connect(Connection::new(
            ConnectionMetadata {
                id: String::new(),
                name: "shell-tool".to_owned(),
                kind: ClientKind::Tool,
                origin: ConnectionOrigin::Supervised,
            },
            Box::new(LocalPeerSink {
                peer: shell_tool.peer.clone(),
            }),
        ));

        let mut harness = Self {
            bus,
            registry: ToolRegistry::new(),
            store,
            pending_request_sessions: VecDeque::new(),
            pending_tool_sessions: std::collections::HashMap::new(),
            agent: ParticipantHandle {
                connection_id: agent_id,
                ..agent
            },
            filesystem_tool: ParticipantHandle {
                connection_id: filesystem_tool_id,
                ..filesystem_tool
            },
            shell_tool: ParticipantHandle {
                connection_id: shell_tool_id,
                ..shell_tool
            },
        };
        harness.wait_for_startup()?;
        Ok(harness)
    }

    fn wait_for_startup(&mut self) -> Result<(), CliError> {
        let started_at = Instant::now();
        let mut agent_ready = false;
        let mut filesystem_ready = false;
        let mut shell_ready = false;
        let mut echo_registered = false;
        let mut fs_registered = false;
        let mut shell_registered = false;

        while !(agent_ready
            && filesystem_ready
            && shell_ready
            && echo_registered
            && fs_registered
            && shell_registered)
        {
            if STARTUP_TIMEOUT <= started_at.elapsed() {
                return Err(CliError::StartupTimeout);
            }

            let agent_event = { self.agent.peer.borrow_mut().recv_timeout(POLL_INTERVAL)? };
            if let Some(event) = agent_event {
                if matches!(event, Event::LifecycleReady(_)) {
                    agent_ready = true;
                }
                let agent_id = self.agent.connection_id.clone();
                let _ = self.handle_participant_event(&agent_id, event, &mut Vec::new())?;
            }

            let filesystem_event = {
                self.filesystem_tool
                    .peer
                    .borrow_mut()
                    .recv_timeout(POLL_INTERVAL)?
            };
            if let Some(event) = filesystem_event {
                if matches!(event, Event::LifecycleReady(_)) {
                    filesystem_ready = true;
                }
                if let Event::ToolRegister(register) = &event {
                    echo_registered |= register.tool.name == shlop_ext_fs::DEMO_ECHO_TOOL_NAME;
                    fs_registered |= register.tool.name == shlop_ext_fs::FS_READ_TOOL_NAME;
                }
                let filesystem_tool_id = self.filesystem_tool.connection_id.clone();
                let _ =
                    self.handle_participant_event(&filesystem_tool_id, event, &mut Vec::new())?;
            }

            let shell_event = {
                self.shell_tool
                    .peer
                    .borrow_mut()
                    .recv_timeout(POLL_INTERVAL)?
            };
            if let Some(event) = shell_event {
                if matches!(event, Event::LifecycleReady(_)) {
                    shell_ready = true;
                }
                if let Event::ToolRegister(register) = &event {
                    shell_registered |= register.tool.name == shlop_ext_shell::SHELL_EXEC_TOOL_NAME;
                }
                let shell_tool_id = self.shell_tool.connection_id.clone();
                let _ = self.handle_participant_event(&shell_tool_id, event, &mut Vec::new())?;
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
        let agent_event = { self.agent.peer.borrow_mut().recv_timeout(POLL_INTERVAL)? };
        if let Some(event) = agent_event {
            let agent_id = self.agent.connection_id.clone();
            if let Some(message) =
                self.handle_participant_event(&agent_id, event, progress_messages)?
            {
                return Ok(Some(message));
            }
        }
        let filesystem_event = {
            self.filesystem_tool
                .peer
                .borrow_mut()
                .recv_timeout(POLL_INTERVAL)?
        };
        if let Some(event) = filesystem_event {
            let filesystem_tool_id = self.filesystem_tool.connection_id.clone();
            if let Some(message) =
                self.handle_participant_event(&filesystem_tool_id, event, progress_messages)?
            {
                return Ok(Some(message));
            }
        }
        let shell_event = {
            self.shell_tool
                .peer
                .borrow_mut()
                .recv_timeout(POLL_INTERVAL)?
        };
        if let Some(event) = shell_event {
            let shell_tool_id = self.shell_tool.connection_id.clone();
            if let Some(message) =
                self.handle_participant_event(&shell_tool_id, event, progress_messages)?
            {
                return Ok(Some(message));
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
                let _ = self
                    .registry
                    .route_tool_request(&mut self.bus, source_id, request)?;
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
                let _ = self
                    .bus
                    .publish_from(Some(client_id), Event::LifecycleSubscribe(subscribe));
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
        for connection_id in [
            &self.agent.connection_id,
            &self.filesystem_tool.connection_id,
            &self.shell_tool.connection_id,
        ] {
            let _ = self.bus.send_to(
                connection_id,
                None,
                Event::LifecycleDisconnect(LifecycleDisconnect {
                    reason: Some("shutdown".to_owned()),
                }),
            );
        }

        if let Some(handle) = self.agent.thread.take() {
            let result = handle.join().map_err(|_| CliError::ThreadJoin("agent"))?;
            result.map_err(CliError::Participant)?;
        }
        if let Some(handle) = self.filesystem_tool.thread.take() {
            let result = handle
                .join()
                .map_err(|_| CliError::ThreadJoin("filesystem-tool"))?;
            result.map_err(CliError::Participant)?;
        }
        if let Some(handle) = self.shell_tool.thread.take() {
            let result = handle
                .join()
                .map_err(|_| CliError::ThreadJoin("shell-tool"))?;
            result.map_err(CliError::Participant)?;
        }
        Ok(())
    }
}

fn spawn_local_participant<F>(
    _name: &str,
    _kind: ClientKind,
    run: F,
) -> Result<ParticipantHandle, CliError>
where
    F: FnOnce(UnixStream, UnixStream) -> Result<(), String> + Send + 'static,
{
    let (runtime_stream, core_stream) = UnixStream::pair()?;
    let reader_stream = runtime_stream.try_clone()?;
    let peer = Rc::new(RefCell::new(LocalPeer::new(core_stream)?));
    let thread = thread::spawn(move || run(reader_stream, runtime_stream));
    Ok(ParticipantHandle {
        connection_id: String::new(),
        peer,
        thread: Some(thread),
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
    let outcome = harness.send_user_message(session_id, message, None)?;
    harness.shutdown()?;
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
        ],
    }))?;
    peer.send(&Event::MessageUser(ChatMessage {
        session_id: Some(session_id.to_owned()),
        text: message.to_owned(),
    }))?;

    let started_at = Instant::now();
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
                Event::MessageAgent(message) => {
                    peer.send(&Event::LifecycleDisconnect(LifecycleDisconnect {
                        reason: Some("done".to_owned()),
                    }))?;
                    return Ok(InteractionOutcome {
                        progress_messages,
                        response: message.text,
                    });
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

/// Opens the session store for inspection or tests.
pub fn open_session_store(path: impl AsRef<Path>) -> Result<SessionStore, CliError> {
    SessionStore::open(path.as_ref()).map_err(CliError::from)
}

/// Opens the policy store for inspection or tests.
pub fn open_policy_store(path: impl AsRef<Path>) -> Result<PolicyStore, CliError> {
    PolicyStore::open(path.as_ref()).map_err(CliError::from)
}

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
}
