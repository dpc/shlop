//! Core event bus, routing, and connection abstractions.
//!
//! This crate keeps transport details outside the routing layer. Stdio, Unix
//! socket, and in-memory test clients can all plug into the same bus through a
//! small [`ConnectionSink`] interface.

use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use tau_proto::{
    ClientKind, ConnectionId, Event, EventSelector, LifecycleSubscribe, ToolName, ToolRequest,
    ToolSpec,
};

/// The origin class of one live connection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionOrigin {
    Supervised,
    Socket,
    InMemory,
}

/// Immutable metadata describing one live connection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectionMetadata {
    pub id: ConnectionId,
    pub name: String,
    pub kind: ClientKind,
    pub origin: ConnectionOrigin,
}

/// One protocol event routed through the internal bus.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutedEvent {
    pub source_id: Option<ConnectionId>,
    pub event: Event,
}

impl RoutedEvent {
    /// Creates a routed event with an optional source connection.
    #[must_use]
    pub fn new(source_id: Option<ConnectionId>, event: Event) -> Self {
        Self { source_id, event }
    }
}

/// A sink that accepts routed events for one live connection.
pub trait ConnectionSink {
    fn send(&mut self, event: RoutedEvent) -> Result<(), ConnectionSendError>;
}

/// A per-connection visibility hook.
pub trait VisibilityFilter {
    fn allows(&self, event: &RoutedEvent) -> bool;
}

impl<F> VisibilityFilter for F
where
    F: Fn(&RoutedEvent) -> bool + 'static,
{
    fn allows(&self, event: &RoutedEvent) -> bool {
        self(event)
    }
}

/// Visibility filter that allows all routed events.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl VisibilityFilter for AllowAll {
    fn allows(&self, _event: &RoutedEvent) -> bool {
        true
    }
}

/// A transport-agnostic connection registered with the bus.
pub struct Connection {
    metadata: ConnectionMetadata,
    sink: Box<dyn ConnectionSink>,
    visibility_filter: Box<dyn VisibilityFilter>,
}

impl Connection {
    /// Creates a connection with an allow-all visibility filter.
    #[must_use]
    pub fn new(metadata: ConnectionMetadata, sink: Box<dyn ConnectionSink>) -> Self {
        Self {
            metadata,
            sink,
            visibility_filter: Box::new(AllowAll),
        }
    }

    /// Installs a custom visibility filter for this connection.
    #[must_use]
    pub fn with_visibility_filter(mut self, filter: Box<dyn VisibilityFilter>) -> Self {
        self.visibility_filter = filter;
        self
    }

    /// Returns immutable metadata for the connection.
    #[must_use]
    pub fn metadata(&self) -> &ConnectionMetadata {
        &self.metadata
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SubscriptionSet {
    selectors: Vec<EventSelector>,
}

impl SubscriptionSet {
    fn replace(&mut self, selectors: Vec<EventSelector>) {
        self.selectors = selectors;
    }

    fn matches(&self, event: &Event) -> bool {
        self.selectors
            .iter()
            .any(|selector| selector_matches(selector, event))
    }

    fn selectors(&self) -> &[EventSelector] {
        &self.selectors
    }
}

fn selector_matches(selector: &EventSelector, event: &Event) -> bool {
    match selector {
        EventSelector::Exact(name) => *name == event.name(),
        EventSelector::Prefix(prefix) => event.name().as_str().starts_with(prefix),
    }
}

struct ConnectionEntry {
    metadata: ConnectionMetadata,
    sink: Box<dyn ConnectionSink>,
    visibility_filter: Box<dyn VisibilityFilter>,
    subscriptions: SubscriptionSet,
}

/// Summary of one routing operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RouteReport {
    pub delivered_to: Vec<ConnectionId>,
    pub blocked_by_filter: Vec<ConnectionId>,
    pub skipped_by_subscription: Vec<ConnectionId>,
    pub failed_deliveries: Vec<DeliveryFailure>,
}

/// One failed sink delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryFailure {
    pub connection_id: ConnectionId,
    pub error: ConnectionSendError,
}

/// Error returned when the bus cannot route as requested.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteError {
    UnknownConnection {
        connection_id: ConnectionId,
    },
    SubscriptionDenied {
        connection_id: ConnectionId,
        reason: String,
    },
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownConnection { connection_id } => {
                write!(f, "unknown connection: {connection_id}")
            }
            Self::SubscriptionDenied {
                connection_id,
                reason,
            } => write!(f, "subscription denied for {connection_id}: {reason}"),
        }
    }
}

impl Error for RouteError {}

/// Error returned by connection sinks when a delivery fails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionSendError {
    message: String,
}

impl ConnectionSendError {
    /// Creates a new send error with a human-readable message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConnectionSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ConnectionSendError {}

/// Persisted approval for one subscription request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionApproval {
    pub connection_name: String,
    pub connection_origin: ConnectionOrigin,
    pub selectors: Vec<EventSelector>,
}

/// File-backed store of approved subscription sets.
#[derive(Debug)]
pub struct PolicyStore {
    path: PathBuf,
    approvals: Vec<SubscriptionApproval>,
}

impl PolicyStore {
    /// Opens a policy store, loading any existing approvals.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| {
                SessionStoreError::CreateParentDirectory {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
        }

        let approvals = if path.exists() {
            let bytes = fs::read(&path).map_err(|source| SessionStoreError::Read {
                path: path.clone(),
                source,
            })?;
            if bytes.is_empty() {
                Vec::new()
            } else {
                ciborium::from_reader(bytes.as_slice()).map_err(|source| {
                    SessionStoreError::Decode {
                        path: path.clone(),
                        source,
                    }
                })?
            }
        } else {
            Vec::new()
        };

        Ok(Self { path, approvals })
    }

    /// Returns true when the exact approval is already present.
    #[must_use]
    pub fn contains(&self, approval: &SubscriptionApproval) -> bool {
        self.approvals.iter().any(|existing| existing == approval)
    }

    /// Records one approval and persists it if it is new.
    pub fn record(&mut self, approval: SubscriptionApproval) -> Result<(), SessionStoreError> {
        if self.contains(&approval) {
            return Ok(());
        }
        self.approvals.push(approval);

        let mut encoded = Vec::new();
        ciborium::into_writer(&self.approvals, &mut encoded).map_err(|source| {
            SessionStoreError::Encode {
                path: self.path.clone(),
                source,
            }
        })?;
        fs::write(&self.path, encoded).map_err(|source| SessionStoreError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// Returns all persisted approvals.
    #[must_use]
    pub fn approvals(&self) -> &[SubscriptionApproval] {
        &self.approvals
    }
}

/// Policy error returned when a subscription request is rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPolicyError {
    reason: String,
}

impl SubscriptionPolicyError {
    /// Creates a new policy error.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Returns the rejection reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for SubscriptionPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl Error for SubscriptionPolicyError {}

/// Subscription-time policy hook.
pub trait SubscriptionPolicy {
    fn evaluate(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError>;
}

/// Default MVP subscription policy.
#[derive(Debug, Default)]
pub struct DefaultSubscriptionPolicy {
    store: Option<RefCell<PolicyStore>>,
}

impl DefaultSubscriptionPolicy {
    /// Creates the default in-memory-only policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the default policy backed by one approval store.
    pub fn with_store(store: PolicyStore) -> Self {
        Self {
            store: Some(RefCell::new(store)),
        }
    }

    fn record_approval(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if connection.origin != ConnectionOrigin::Socket {
            return Ok(());
        }

        store
            .borrow_mut()
            .record(SubscriptionApproval {
                connection_name: connection.name.clone(),
                connection_origin: connection.origin.clone(),
                selectors: selectors.to_vec(),
            })
            .map_err(|error| SubscriptionPolicyError::new(error.to_string()))
    }
}

impl SubscriptionPolicy for DefaultSubscriptionPolicy {
    fn evaluate(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError> {
        if connection.origin == ConnectionOrigin::Socket {
            for selector in selectors {
                let allowed = match selector {
                    EventSelector::Exact(name) => {
                        let name = name.as_str();
                        name.starts_with("message.")
                            || name.starts_with("tool.")
                            || name.starts_with("extension.")
                    }
                    EventSelector::Prefix(prefix) => {
                        prefix.starts_with("message.")
                            || prefix.starts_with("tool.")
                            || prefix.starts_with("extension.")
                    }
                };
                if !allowed {
                    return Err(SubscriptionPolicyError::new(
                        "socket clients may only subscribe to message.*, tool.*, or extension.*",
                    ));
                }
            }
        }

        self.record_approval(connection, selectors)
    }
}

/// Internal event bus and subscription registry.
pub struct EventBus {
    next_connection_id: u64,
    connections: HashMap<ConnectionId, ConnectionEntry>,
    subscription_policy: Box<dyn SubscriptionPolicy>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self {
            next_connection_id: 0,
            connections: HashMap::new(),
            subscription_policy: Box::new(DefaultSubscriptionPolicy::new()),
        }
    }
}

impl EventBus {
    /// Creates an empty event bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty event bus with an explicit subscription policy.
    #[must_use]
    pub fn with_subscription_policy(policy: Box<dyn SubscriptionPolicy>) -> Self {
        Self {
            next_connection_id: 0,
            connections: HashMap::new(),
            subscription_policy: policy,
        }
    }

    /// Registers a connection and returns its assigned connection ID.
    pub fn connect(&mut self, connection: Connection) -> ConnectionId {
        let connection_id = if connection.metadata.id.is_empty() {
            self.allocate_connection_id()
        } else {
            connection.metadata.id.clone()
        };

        let metadata = ConnectionMetadata {
            id: connection_id.clone(),
            name: connection.metadata.name,
            kind: connection.metadata.kind,
            origin: connection.metadata.origin,
        };

        let entry = ConnectionEntry {
            metadata,
            sink: connection.sink,
            visibility_filter: connection.visibility_filter,
            subscriptions: SubscriptionSet::default(),
        };

        self.connections.insert(connection_id.clone(), entry);
        connection_id
    }

    /// Removes a connection from the bus and returns its metadata if present.
    pub fn disconnect(&mut self, connection_id: &str) -> Option<ConnectionMetadata> {
        self.connections
            .remove(connection_id)
            .map(|entry| entry.metadata)
    }

    /// Returns immutable metadata for one connection.
    #[must_use]
    pub fn connection(&self, connection_id: &str) -> Option<&ConnectionMetadata> {
        self.connections
            .get(connection_id)
            .map(|entry| &entry.metadata)
    }

    /// Returns a snapshot of all connected clients.
    #[must_use]
    pub fn connections(&self) -> Vec<ConnectionMetadata> {
        self.connections
            .values()
            .map(|entry| entry.metadata.clone())
            .collect()
    }

    /// Replaces the subscription selectors for one connection.
    pub fn set_subscriptions(
        &mut self,
        connection_id: &str,
        selectors: Vec<EventSelector>,
    ) -> Result<(), RouteError> {
        let metadata = self
            .connections
            .get(connection_id)
            .map(|entry| entry.metadata.clone())
            .ok_or_else(|| RouteError::UnknownConnection {
                connection_id: connection_id.to_owned(),
            })?;
        self.subscription_policy
            .evaluate(&metadata, &selectors)
            .map_err(|error| RouteError::SubscriptionDenied {
                connection_id: connection_id.to_owned(),
                reason: error.reason().to_owned(),
            })?;
        let entry = self.connections.get_mut(connection_id).ok_or_else(|| {
            RouteError::UnknownConnection {
                connection_id: connection_id.to_owned(),
            }
        })?;
        entry.subscriptions.replace(selectors);
        Ok(())
    }

    /// Returns the active subscription selectors for one connection.
    #[must_use]
    pub fn subscriptions(&self, connection_id: &str) -> Option<&[EventSelector]> {
        self.connections
            .get(connection_id)
            .map(|entry| entry.subscriptions.selectors())
    }

    /// Broadcasts one event to subscribed and visible clients.
    pub fn publish(&mut self, event: Event) -> RouteReport {
        self.publish_from(None, event)
    }

    /// Broadcasts one event from a specific source connection.
    pub fn publish_from(&mut self, source_id: Option<&str>, event: Event) -> RouteReport {
        if let Some(source_id) = source_id {
            self.maybe_update_subscriptions(source_id, &event);
        }

        let routed_event = RoutedEvent::new(source_id.map(ToOwned::to_owned), event);
        let mut report = RouteReport::default();

        for (connection_id, entry) in &mut self.connections {
            if !entry.subscriptions.matches(&routed_event.event) {
                report.skipped_by_subscription.push(connection_id.clone());
                continue;
            }
            if !entry.visibility_filter.allows(&routed_event) {
                report.blocked_by_filter.push(connection_id.clone());
                continue;
            }

            match entry.sink.send(routed_event.clone()) {
                Ok(()) => report.delivered_to.push(connection_id.clone()),
                Err(error) => report.failed_deliveries.push(DeliveryFailure {
                    connection_id: connection_id.clone(),
                    error,
                }),
            }
        }

        report
    }

    /// Sends one directed event to a specific connection.
    pub fn send_to(
        &mut self,
        target_id: &str,
        source_id: Option<&str>,
        event: Event,
    ) -> Result<RouteReport, RouteError> {
        let routed_event = RoutedEvent::new(source_id.map(ToOwned::to_owned), event);
        let entry =
            self.connections
                .get_mut(target_id)
                .ok_or_else(|| RouteError::UnknownConnection {
                    connection_id: target_id.to_owned(),
                })?;

        let mut report = RouteReport::default();
        if !entry.visibility_filter.allows(&routed_event) {
            report.blocked_by_filter.push(target_id.to_owned());
            return Ok(report);
        }

        match entry.sink.send(routed_event) {
            Ok(()) => report.delivered_to.push(target_id.to_owned()),
            Err(error) => report.failed_deliveries.push(DeliveryFailure {
                connection_id: target_id.to_owned(),
                error,
            }),
        }

        Ok(report)
    }

    fn allocate_connection_id(&mut self) -> ConnectionId {
        self.next_connection_id += 1;
        format!("conn-{}", self.next_connection_id)
    }

    fn maybe_update_subscriptions(&mut self, source_id: &str, event: &Event) {
        let Event::LifecycleSubscribe(LifecycleSubscribe { selectors }) = event else {
            return;
        };

        if let Some(entry) = self.connections.get_mut(source_id) {
            entry.subscriptions.replace(selectors.clone());
        }
    }
}

/// One live provider registered for a tool name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolProvider {
    pub connection_id: ConnectionId,
    pub tool: ToolSpec,
}

/// Warning emitted by the tool registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRegistryWarning {
    DuplicateRegistration {
        tool_name: ToolName,
        existing_provider_ids: Vec<ConnectionId>,
    },
}

/// Summary of one registration call.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegisterToolReport {
    pub warnings: Vec<ToolRegistryWarning>,
}

/// Error returned when a tool request cannot be routed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRouteError {
    NoProvider { tool_name: ToolName },
    Route(RouteError),
}

impl fmt::Display for ToolRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider { tool_name } => write!(f, "no live provider for tool: {tool_name}"),
            Self::Route(error) => write!(f, "failed to route tool request: {error}"),
        }
    }
}

impl Error for ToolRouteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NoProvider { .. } => None,
            Self::Route(error) => Some(error),
        }
    }
}

/// Summary of one `tool.request` routing decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRouteReport {
    pub provider_connection_id: ConnectionId,
    pub route_report: RouteReport,
}

/// Live tool registration state keyed by connection and tool name.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolRegistry {
    providers_by_tool: HashMap<ToolName, Vec<ToolProvider>>,
    tools_by_connection: HashMap<ConnectionId, Vec<ToolName>>,
}

impl ToolRegistry {
    /// Creates an empty tool registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one tool for a live provider connection.
    pub fn register(&mut self, connection_id: &str, tool: ToolSpec) -> RegisterToolReport {
        let tool_name = tool.name.clone();
        let providers = self.providers_by_tool.entry(tool_name.clone()).or_default();

        let existing_provider_ids = providers
            .iter()
            .map(|provider| provider.connection_id.clone())
            .collect::<Vec<_>>();
        let mut report = RegisterToolReport::default();
        if !existing_provider_ids.is_empty() {
            report
                .warnings
                .push(ToolRegistryWarning::DuplicateRegistration {
                    tool_name: tool_name.clone(),
                    existing_provider_ids,
                });
        }

        if let Some(existing_provider) = providers
            .iter_mut()
            .find(|provider| provider.connection_id == connection_id)
        {
            existing_provider.tool = tool;
        } else {
            providers.push(ToolProvider {
                connection_id: connection_id.to_owned(),
                tool,
            });
        }

        let connection_tools = self
            .tools_by_connection
            .entry(connection_id.to_owned())
            .or_default();
        if !connection_tools
            .iter()
            .any(|existing| *existing == tool_name)
        {
            connection_tools.push(tool_name);
        }

        report
    }

    /// Unregisters one tool from one provider connection.
    pub fn unregister(&mut self, connection_id: &str, tool_name: &str) -> bool {
        let mut removed = false;

        if let Some(providers) = self.providers_by_tool.get_mut(tool_name) {
            let initial_len = providers.len();
            providers.retain(|provider| provider.connection_id != connection_id);
            removed = providers.len() != initial_len;
            if providers.is_empty() {
                self.providers_by_tool.remove(tool_name);
            }
        }

        if removed {
            self.remove_tool_from_connection(connection_id, tool_name);
        }

        removed
    }

    /// Unregisters all tools owned by one disconnected provider connection.
    pub fn unregister_connection(&mut self, connection_id: &str) -> Vec<ToolName> {
        let Some(tool_names) = self.tools_by_connection.remove(connection_id) else {
            return Vec::new();
        };

        for tool_name in &tool_names {
            if let Some(providers) = self.providers_by_tool.get_mut(tool_name) {
                providers.retain(|provider| provider.connection_id != connection_id);
                if providers.is_empty() {
                    self.providers_by_tool.remove(tool_name);
                }
            }
        }

        tool_names
    }

    /// Returns all currently live providers for a tool name.
    #[must_use]
    pub fn providers_for(&self, tool_name: &str) -> Vec<ToolProvider> {
        self.providers_by_tool
            .get(tool_name)
            .cloned()
            .unwrap_or_default()
    }

    /// Picks one currently live provider for a tool name.
    #[must_use]
    pub fn resolve_provider(&self, tool_name: &str) -> Option<&ToolProvider> {
        self.providers_by_tool
            .get(tool_name)
            .and_then(|providers| providers.first())
    }

    /// Routes a `tool.request` to one live provider as a directed
    /// `tool.invoke`.
    pub fn route_tool_request(
        &self,
        bus: &mut EventBus,
        requester_id: &str,
        request: ToolRequest,
    ) -> Result<ToolRouteReport, ToolRouteError> {
        let provider_connection_id = self
            .resolve_provider(&request.tool_name)
            .map(|provider| provider.connection_id.clone())
            .ok_or_else(|| ToolRouteError::NoProvider {
                tool_name: request.tool_name.clone(),
            })?;

        let route_report = bus
            .send_to(
                &provider_connection_id,
                Some(requester_id),
                Event::ToolInvoke(tau_proto::ToolInvoke {
                    call_id: request.call_id,
                    tool_name: request.tool_name,
                    arguments: request.arguments,
                }),
            )
            .map_err(ToolRouteError::Route)?;

        Ok(ToolRouteReport {
            provider_connection_id,
            route_report,
        })
    }

    fn remove_tool_from_connection(&mut self, connection_id: &str, tool_name: &str) {
        if let Some(tool_names) = self.tools_by_connection.get_mut(connection_id) {
            tool_names.retain(|name| name != tool_name);
            if tool_names.is_empty() {
                self.tools_by_connection.remove(connection_id);
            }
        }
    }
}

/// One persisted chat or tool activity entry belonging to a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SessionEntry {
    UserMessage { text: String },
    AgentMessage { text: String },
    ToolActivity(ToolActivityRecord),
}

/// One persisted tool activity record associated with a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolActivityRecord {
    pub call_id: String,
    pub tool_name: ToolName,
    pub outcome: ToolActivityOutcome,
}

/// The persisted outcome of one tool activity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToolActivityOutcome {
    Requested {
        arguments: tau_proto::CborValue,
    },
    Result {
        result: tau_proto::CborValue,
    },
    Error {
        message: String,
        details: Option<tau_proto::CborValue>,
    },
}

/// In-memory snapshot of one session and its persisted entries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub entries: Vec<SessionEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct PersistedSessionRecord {
    session_id: String,
    entry: SessionEntry,
}

/// Errors returned by the append-only session store.
#[derive(Debug)]
pub enum SessionStoreError {
    CreateParentDirectory {
        path: PathBuf,
        source: io::Error,
    },
    Open {
        path: PathBuf,
        source: io::Error,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Decode {
        path: PathBuf,
        source: tau_proto::DecodeError,
    },
    Encode {
        path: PathBuf,
        source: tau_proto::EncodeError,
    },
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateParentDirectory { path, source } => write!(
                f,
                "failed to create parent directory for session store {}: {source}",
                path.display()
            ),
            Self::Open { path, source } => {
                write!(
                    f,
                    "failed to open session store {}: {source}",
                    path.display()
                )
            }
            Self::Read { path, source } => {
                write!(
                    f,
                    "failed to read session store {}: {source}",
                    path.display()
                )
            }
            Self::Write { path, source } => {
                write!(
                    f,
                    "failed to write session store {}: {source}",
                    path.display()
                )
            }
            Self::Decode { path, source } => write!(
                f,
                "failed to decode session store record from {}: {source}",
                path.display()
            ),
            Self::Encode { path, source } => write!(
                f,
                "failed to encode session store record for {}: {source}",
                path.display()
            ),
        }
    }
}

impl Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDirectory { source, .. } => Some(source),
            Self::Open { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::Write { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Encode { source, .. } => Some(source),
        }
    }
}

/// Append-only persistence for minimal session history and tool activity.
#[derive(Debug)]
pub struct SessionStore {
    path: PathBuf,
    sessions: HashMap<String, SessionSnapshot>,
}

impl SessionStore {
    /// Opens one session store, loading any existing records into memory.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| {
                SessionStoreError::CreateParentDirectory {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
        }

        let mut sessions = HashMap::new();
        if path.exists() {
            let mut file = File::open(&path).map_err(|source| SessionStoreError::Open {
                path: path.clone(),
                source,
            })?;
            load_records(&path, &mut file, &mut sessions)?;
        }

        Ok(Self { path, sessions })
    }

    /// Appends one user message to a session, creating the session if needed.
    pub fn append_user_message(
        &mut self,
        session_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<(), SessionStoreError> {
        self.append_entry(
            session_id.into(),
            SessionEntry::UserMessage { text: text.into() },
        )
    }

    /// Appends one agent message to a session, creating the session if needed.
    pub fn append_agent_message(
        &mut self,
        session_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<(), SessionStoreError> {
        self.append_entry(
            session_id.into(),
            SessionEntry::AgentMessage { text: text.into() },
        )
    }

    /// Appends one tool activity record to a session, creating the session if
    /// needed.
    pub fn append_tool_activity(
        &mut self,
        session_id: impl Into<String>,
        activity: ToolActivityRecord,
    ) -> Result<(), SessionStoreError> {
        self.append_entry(session_id.into(), SessionEntry::ToolActivity(activity))
    }

    /// Returns one session snapshot if it exists.
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<&SessionSnapshot> {
        self.sessions.get(session_id)
    }

    /// Returns all known sessions.
    #[must_use]
    pub fn sessions(&self) -> Vec<&SessionSnapshot> {
        self.sessions.values().collect()
    }

    fn append_entry(
        &mut self,
        session_id: String,
        entry: SessionEntry,
    ) -> Result<(), SessionStoreError> {
        let record = PersistedSessionRecord {
            session_id: session_id.clone(),
            entry: entry.clone(),
        };
        append_record(&self.path, &record)?;
        self.sessions
            .entry(session_id.clone())
            .or_insert_with(|| SessionSnapshot {
                session_id,
                entries: Vec::new(),
            })
            .entries
            .push(entry);
        Ok(())
    }
}

fn load_records(
    path: &Path,
    reader: &mut File,
    sessions: &mut HashMap<String, SessionSnapshot>,
) -> Result<(), SessionStoreError> {
    loop {
        let mut length_bytes = [0_u8; 8];
        match reader.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(source) => {
                return Err(SessionStoreError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }

        let record_length = u64::from_le_bytes(length_bytes) as usize;
        let mut record_bytes = vec![0_u8; record_length];
        reader
            .read_exact(&mut record_bytes)
            .map_err(|source| SessionStoreError::Read {
                path: path.to_path_buf(),
                source,
            })?;

        let record: PersistedSessionRecord = ciborium::from_reader(record_bytes.as_slice())
            .map_err(|source| SessionStoreError::Decode {
                path: path.to_path_buf(),
                source,
            })?;
        sessions
            .entry(record.session_id.clone())
            .or_insert_with(|| SessionSnapshot {
                session_id: record.session_id.clone(),
                entries: Vec::new(),
            })
            .entries
            .push(record.entry);
    }
}

fn append_record(path: &Path, record: &PersistedSessionRecord) -> Result<(), SessionStoreError> {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).map_err(|source| SessionStoreError::Encode {
        path: path.to_path_buf(),
        source,
    })?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| SessionStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&encoded)
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.flush().map_err(|source| SessionStoreError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Snapshot-friendly in-memory client inbox for tests and in-process adapters.
#[derive(Clone, Debug, Default)]
pub struct MemoryInbox {
    events: Rc<RefCell<Vec<RoutedEvent>>>,
}

impl MemoryInbox {
    /// Returns a snapshot of all delivered events.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RoutedEvent> {
        self.events.borrow().clone()
    }

    /// Removes and returns all delivered events.
    #[must_use]
    pub fn drain(&self) -> Vec<RoutedEvent> {
        self.events.borrow_mut().drain(..).collect()
    }
}

#[derive(Debug)]
struct MemorySink {
    inbox: MemoryInbox,
}

impl ConnectionSink for MemorySink {
    fn send(&mut self, event: RoutedEvent) -> Result<(), ConnectionSendError> {
        self.inbox.events.borrow_mut().push(event);
        Ok(())
    }
}

/// Creates a transport-agnostic in-memory connection pair for tests.
#[must_use]
pub fn memory_connection(name: impl Into<String>, kind: ClientKind) -> (Connection, MemoryInbox) {
    let inbox = MemoryInbox::default();
    let connection = Connection::new(
        ConnectionMetadata {
            id: String::new(),
            name: name.into(),
            kind,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(MemorySink {
            inbox: inbox.clone(),
        }),
    );
    (connection, inbox)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{BufReader, BufWriter};
    use std::os::unix::net::UnixStream;
    use std::rc::Rc;
    use std::thread;

    use tau_proto::{
        CborValue, ChatMessage, EventName, EventReader, EventWriter, LifecycleSubscribe,
        ToolRegister,
    };
    use tempfile::TempDir;

    use super::*;

    struct StreamSink {
        writer: Rc<RefCell<EventWriter<BufWriter<UnixStream>>>>,
    }

    impl ConnectionSink for StreamSink {
        fn send(&mut self, event: RoutedEvent) -> Result<(), ConnectionSendError> {
            let mut writer = self.writer.borrow_mut();
            writer
                .write_event(&event.event)
                .map_err(|error| ConnectionSendError::new(error.to_string()))?;
            writer
                .flush()
                .map_err(|error| ConnectionSendError::new(error.to_string()))
        }
    }

    fn stream_connection(
        name: &str,
        kind: ClientKind,
        stream: UnixStream,
    ) -> (Connection, EventReader<BufReader<UnixStream>>) {
        let writer_stream = stream
            .try_clone()
            .expect("stream clone for writer should succeed");
        let connection = Connection::new(
            ConnectionMetadata {
                id: String::new(),
                name: name.to_owned(),
                kind,
                origin: ConnectionOrigin::InMemory,
            },
            Box::new(StreamSink {
                writer: Rc::new(RefCell::new(EventWriter::new(BufWriter::new(
                    writer_stream,
                )))),
            }),
        );
        let reader = EventReader::new(BufReader::new(stream));
        (connection, reader)
    }

    #[test]
    fn subscribed_clients_only_receive_matching_events() {
        let mut bus = EventBus::new();

        let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
        let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
        let agent_id = bus.connect(agent_connection);
        let ui_id = bus.connect(ui_connection);

        bus.set_subscriptions(
            &agent_id,
            vec![EventSelector::Exact(EventName::MessageUser)],
        )
        .expect("agent subscriptions should be stored");
        bus.set_subscriptions(&ui_id, vec![EventSelector::Prefix("tool.".to_owned())])
            .expect("ui subscriptions should be stored");

        let report = bus.publish(Event::MessageUser(ChatMessage {
            session_id: Some("session-1".to_owned()),
            text: "hello".to_owned(),
        }));

        assert_eq!(report.delivered_to, vec![agent_id.clone()]);
        assert_eq!(report.skipped_by_subscription, vec![ui_id.clone()]);
        assert_eq!(agent_inbox.snapshot().len(), 1);
        assert!(ui_inbox.snapshot().is_empty());
    }

    #[test]
    fn publish_from_updates_subscription_registry_from_lifecycle_subscribe() {
        let mut bus = EventBus::new();

        let (connection, inbox) = memory_connection("agent", ClientKind::Agent);
        let connection_id = bus.connect(connection);
        bus.set_subscriptions(
            &connection_id,
            vec![EventSelector::Exact(EventName::LifecycleSubscribe)],
        )
        .expect("initial subscriptions should be stored");

        let report = bus.publish_from(
            Some(&connection_id),
            Event::LifecycleSubscribe(LifecycleSubscribe {
                selectors: vec![EventSelector::Exact(EventName::MessageUser)],
            }),
        );

        assert_eq!(report.delivered_to, Vec::<ConnectionId>::new());
        assert_eq!(
            bus.subscriptions(&connection_id),
            Some([EventSelector::Exact(EventName::MessageUser)].as_slice())
        );
        assert!(inbox.snapshot().is_empty());
    }

    #[test]
    fn directed_events_ignore_subscriptions_but_still_use_visibility_filters() {
        let mut bus = EventBus::new();

        let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
        let filtered_connection =
            ui_connection.with_visibility_filter(Box::new(|event: &RoutedEvent| {
                event.event.name() == EventName::ToolInvoke
            }));
        let ui_id = bus.connect(filtered_connection);

        let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
        let tool_id = bus.connect(tool_connection);
        bus.set_subscriptions(&tool_id, Vec::new())
            .expect("tool subscriptions should be stored");

        let blocked = bus
            .send_to(
                &ui_id,
                Some(&tool_id),
                Event::MessageAgent(ChatMessage {
                    session_id: None,
                    text: "hidden".to_owned(),
                }),
            )
            .expect("directed route should succeed");
        assert_eq!(blocked.blocked_by_filter, vec![ui_id.clone()]);
        assert!(ui_inbox.snapshot().is_empty());

        let delivered = bus
            .send_to(
                &ui_id,
                Some(&tool_id),
                Event::ToolInvoke(tau_proto::ToolInvoke {
                    call_id: "call-1".to_owned(),
                    tool_name: "demo.echo".to_owned(),
                    arguments: CborValue::Null,
                }),
            )
            .expect("directed route should succeed");
        assert_eq!(delivered.delivered_to, vec![ui_id.clone()]);
        assert_eq!(ui_inbox.snapshot().len(), 1);
        assert!(tool_inbox.snapshot().is_empty());
    }

    #[test]
    fn connection_abstraction_is_transport_independent_for_in_memory_clients() {
        let mut bus = EventBus::new();

        let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
        let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
        let agent_id = bus.connect(agent_connection);
        let tool_id = bus.connect(tool_connection);

        bus.set_subscriptions(
            &agent_id,
            vec![EventSelector::Prefix("message.".to_owned())],
        )
        .expect("agent subscriptions should be stored");
        bus.set_subscriptions(&tool_id, vec![EventSelector::Prefix("tool.".to_owned())])
            .expect("tool subscriptions should be stored");

        let first_report = bus.publish(Event::ToolResult(tau_proto::ToolResult {
            call_id: "call-1".to_owned(),
            tool_name: "demo.echo".to_owned(),
            result: CborValue::Text("done".to_owned()),
        }));
        assert_eq!(first_report.delivered_to, vec![tool_id.clone()]);

        let second_report = bus.publish(Event::MessageAgent(ChatMessage {
            session_id: Some("session-1".to_owned()),
            text: "done".to_owned(),
        }));
        assert_eq!(second_report.delivered_to, vec![agent_id.clone()]);

        assert_eq!(tool_inbox.snapshot().len(), 1);
        assert_eq!(agent_inbox.snapshot().len(), 1);
    }

    #[test]
    fn provider_can_register_tool_and_receive_invocations() {
        let mut bus = EventBus::new();
        let mut registry = ToolRegistry::new();

        let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
        let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
        let agent_id = bus.connect(agent_connection);
        let tool_id = bus.connect(tool_connection);

        let register_report = registry.register(
            &tool_id,
            ToolSpec {
                name: "demo.echo".to_owned(),
                description: Some("Echo a payload".to_owned()),
            },
        );
        assert!(register_report.warnings.is_empty());

        let route_report = registry
            .route_tool_request(
                &mut bus,
                &agent_id,
                ToolRequest {
                    call_id: "call-1".to_owned(),
                    tool_name: "demo.echo".to_owned(),
                    arguments: CborValue::Text("hello".to_owned()),
                },
            )
            .expect("tool request should route");

        assert_eq!(route_report.provider_connection_id, tool_id.clone());
        assert_eq!(
            route_report.route_report.delivered_to,
            vec![tool_id.clone()]
        );
        assert!(agent_inbox.snapshot().is_empty());

        let delivered_events = tool_inbox.snapshot();
        assert_eq!(delivered_events.len(), 1);
        assert_eq!(delivered_events[0].source_id, Some(agent_id));
        assert_eq!(
            delivered_events[0].event,
            Event::ToolInvoke(tau_proto::ToolInvoke {
                call_id: "call-1".to_owned(),
                tool_name: "demo.echo".to_owned(),
                arguments: CborValue::Text("hello".to_owned()),
            })
        );
    }

    #[test]
    fn duplicate_tool_registrations_warn_but_remain_available() {
        let mut registry = ToolRegistry::new();

        let first_report = registry.register(
            "conn-a",
            ToolSpec {
                name: "demo.echo".to_owned(),
                description: Some("Echo".to_owned()),
            },
        );
        assert!(first_report.warnings.is_empty());

        let second_report = registry.register(
            "conn-b",
            ToolSpec {
                name: "demo.echo".to_owned(),
                description: Some("Echo from another provider".to_owned()),
            },
        );
        assert_eq!(second_report.warnings.len(), 1);
        assert_eq!(
            second_report.warnings[0],
            ToolRegistryWarning::DuplicateRegistration {
                tool_name: "demo.echo".to_owned(),
                existing_provider_ids: vec!["conn-a".to_owned()],
            }
        );

        let providers = registry.providers_for("demo.echo");
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].connection_id, "conn-a");
        assert_eq!(providers[1].connection_id, "conn-b");
    }

    #[test]
    fn disconnect_cleanup_removes_stale_tool_providers() {
        let mut bus = EventBus::new();
        let mut registry = ToolRegistry::new();

        let (first_connection, _first_inbox) = memory_connection("tool-a", ClientKind::Tool);
        let (second_connection, _second_inbox) = memory_connection("tool-b", ClientKind::Tool);
        let first_id = bus.connect(first_connection);
        let second_id = bus.connect(second_connection);

        registry.register(
            &first_id,
            ToolSpec {
                name: "demo.echo".to_owned(),
                description: None,
            },
        );
        registry.register(
            &second_id,
            ToolSpec {
                name: "demo.echo".to_owned(),
                description: None,
            },
        );
        registry.register(
            &first_id,
            ToolSpec {
                name: "demo.upper".to_owned(),
                description: None,
            },
        );

        let disconnected = bus.disconnect(&first_id);
        assert!(disconnected.is_some());
        let removed_tools = registry.unregister_connection(&first_id);
        assert_eq!(removed_tools.len(), 2);
        assert!(
            removed_tools
                .iter()
                .any(|tool_name| tool_name == "demo.echo")
        );
        assert!(
            removed_tools
                .iter()
                .any(|tool_name| tool_name == "demo.upper")
        );

        let echo_providers = registry.providers_for("demo.echo");
        assert_eq!(echo_providers.len(), 1);
        assert_eq!(echo_providers[0].connection_id, second_id);
        assert!(registry.providers_for("demo.upper").is_empty());
    }

    #[test]
    fn register_events_map_cleanly_to_registry_state() {
        let mut registry = ToolRegistry::new();

        let report = registry.register(
            "conn-tool",
            ToolRegister {
                tool: ToolSpec {
                    name: "demo.echo".to_owned(),
                    description: Some("Echo".to_owned()),
                },
            }
            .tool,
        );

        assert!(report.warnings.is_empty());
        assert_eq!(registry.providers_for("demo.echo").len(), 1);
        assert!(registry.unregister("conn-tool", "demo.echo"));
        assert!(registry.providers_for("demo.echo").is_empty());
    }

    #[test]
    fn session_store_persists_message_history_across_reopen() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("session-store.bin");

        let mut store = SessionStore::open(&store_path).expect("store should open");
        store
            .append_user_message("session-1", "hello")
            .expect("user message should persist");
        store
            .append_agent_message("session-1", "hi there")
            .expect("agent message should persist");

        let reopened = SessionStore::open(&store_path).expect("store should reopen");
        let session = reopened
            .session("session-1")
            .expect("session should reload");
        assert_eq!(
            session.entries,
            vec![
                SessionEntry::UserMessage {
                    text: "hello".to_owned(),
                },
                SessionEntry::AgentMessage {
                    text: "hi there".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn session_store_associates_tool_activity_with_a_session() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("session-store.bin");

        let mut store = SessionStore::open(&store_path).expect("store should open");
        store
            .append_user_message("session-1", "read a file")
            .expect("user message should persist");
        store
            .append_tool_activity(
                "session-1",
                ToolActivityRecord {
                    call_id: "call-1".to_owned(),
                    tool_name: "fs.read".to_owned(),
                    outcome: ToolActivityOutcome::Result {
                        result: CborValue::Text("README".to_owned()),
                    },
                },
            )
            .expect("tool activity should persist");

        let reopened = SessionStore::open(&store_path).expect("store should reopen");
        let session = reopened
            .session("session-1")
            .expect("session should reload");
        assert_eq!(session.entries.len(), 2);
        assert_eq!(
            session.entries[1],
            SessionEntry::ToolActivity(ToolActivityRecord {
                call_id: "call-1".to_owned(),
                tool_name: "fs.read".to_owned(),
                outcome: ToolActivityOutcome::Result {
                    result: CborValue::Text("README".to_owned()),
                },
            })
        );
    }

    #[test]
    fn socket_clients_are_denied_forbidden_subscriptions() {
        let mut bus = EventBus::new();
        let inbox = MemoryInbox::default();
        let connection = Connection::new(
            ConnectionMetadata {
                id: String::new(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin: ConnectionOrigin::Socket,
            },
            Box::new(MemorySink { inbox }),
        );
        let connection_id = bus.connect(connection);

        let error = bus
            .set_subscriptions(
                &connection_id,
                vec![EventSelector::Prefix("lifecycle.".to_owned())],
            )
            .expect_err("socket lifecycle subscription should be denied");
        assert_eq!(
            error,
            RouteError::SubscriptionDenied {
                connection_id,
                reason: "socket clients may only subscribe to message.*, tool.*, or extension.*"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn policy_store_persists_allowed_socket_subscriptions() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let policy_path = tempdir.path().join("policy.cbor");
        let store = PolicyStore::open(&policy_path).expect("policy store should open");
        let mut bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(store),
        ));
        let inbox = MemoryInbox::default();
        let connection = Connection::new(
            ConnectionMetadata {
                id: String::new(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin: ConnectionOrigin::Socket,
            },
            Box::new(MemorySink { inbox }),
        );
        let connection_id = bus.connect(connection);

        bus.set_subscriptions(
            &connection_id,
            vec![EventSelector::Prefix("message.".to_owned())],
        )
        .expect("allowed socket subscription should persist");

        let reopened = PolicyStore::open(&policy_path).expect("policy store should reopen");
        assert_eq!(
            reopened.approvals(),
            [SubscriptionApproval {
                connection_name: "socket-ui".to_owned(),
                connection_origin: ConnectionOrigin::Socket,
                selectors: vec![EventSelector::Prefix("message.".to_owned())],
            }]
            .as_slice()
        );
    }

    #[test]
    fn deterministic_agent_and_tool_complete_one_vertical_slice() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("session-store.bin");
        let mut store = SessionStore::open(&store_path).expect("store should open");
        let mut bus = EventBus::new();
        let mut registry = ToolRegistry::new();

        let (agent_runtime_stream, agent_harness_stream) =
            UnixStream::pair().expect("agent stream pair should open");
        let (tool_runtime_stream, tool_harness_stream) =
            UnixStream::pair().expect("tool stream pair should open");

        let agent_thread = thread::spawn(move || {
            let agent_reader = agent_runtime_stream
                .try_clone()
                .expect("agent reader clone should succeed");
            tau_agent::run(agent_reader, agent_runtime_stream)
                .expect("agent should run successfully");
        });
        let tool_thread = thread::spawn(move || {
            let tool_reader = tool_runtime_stream
                .try_clone()
                .expect("tool reader clone should succeed");
            tau_ext_fs::run(tool_reader, tool_runtime_stream)
                .expect("tool extension should run successfully");
        });

        let (agent_connection, mut agent_reader) =
            stream_connection("agent", ClientKind::Agent, agent_harness_stream);
        let (tool_connection, mut tool_reader) =
            stream_connection("tool", ClientKind::Tool, tool_harness_stream);
        let agent_id = bus.connect(agent_connection);
        let tool_id = bus.connect(tool_connection);

        let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
        let ui_id = bus.connect(ui_connection);
        bus.set_subscriptions(&ui_id, vec![EventSelector::Exact(EventName::MessageAgent)])
            .expect("ui subscription should be stored");

        let agent_hello = agent_reader
            .read_event()
            .expect("read")
            .expect("agent hello should arrive");
        assert!(matches!(agent_hello, Event::LifecycleHello(_)));
        let _ = bus.publish_from(Some(&agent_id), agent_hello);
        let agent_subscribe = agent_reader
            .read_event()
            .expect("read")
            .expect("agent subscribe should arrive");
        let _ = bus.publish_from(Some(&agent_id), agent_subscribe);
        let agent_ready = agent_reader
            .read_event()
            .expect("read")
            .expect("agent ready should arrive");
        assert!(matches!(agent_ready, Event::LifecycleReady(_)));

        let tool_hello = tool_reader.read_event().expect("read").expect("tool hello should arrive");
        assert!(matches!(tool_hello, Event::LifecycleHello(_)));
        let _ = bus.publish_from(Some(&tool_id), tool_hello);
        let tool_subscribe = tool_reader
            .read_event()
            .expect("read")
            .expect("tool subscribe should arrive");
        let _ = bus.publish_from(Some(&tool_id), tool_subscribe);
        let mut registered_tool_names = Vec::new();
        loop {
            let startup_event = tool_reader
                .read_event()
                .expect("read")
                .expect("tool startup event should arrive");
            match startup_event {
                Event::ToolRegister(tool_register) => {
                    let register_report = registry.register(&tool_id, tool_register.tool.clone());
                    assert!(register_report.warnings.is_empty());
                    registered_tool_names.push(tool_register.tool.name);
                }
                Event::LifecycleReady(_) => break,
                _ => panic!("unexpected tool startup event"),
            }
        }
        assert!(registered_tool_names.iter().any(|name| name == "demo.echo"));
        assert!(registered_tool_names.iter().any(|name| name == "fs.read"));

        store
            .append_user_message("session-1", "hello")
            .expect("user message should persist");
        let published = bus.publish_from(
            Some(&ui_id),
            Event::MessageUser(ChatMessage {
                session_id: Some("session-1".to_owned()),
                text: "hello".to_owned(),
            }),
        );
        assert_eq!(published.delivered_to, vec![agent_id.clone()]);

        let request = agent_reader
            .read_event()
            .expect("read")
            .expect("tool request should arrive");
        let Event::ToolRequest(request) = request else {
            panic!("expected tool request from agent");
        };
        store
            .append_tool_activity(
                "session-1",
                ToolActivityRecord {
                    call_id: request.call_id.clone(),
                    tool_name: request.tool_name.clone(),
                    outcome: ToolActivityOutcome::Requested {
                        arguments: request.arguments.clone(),
                    },
                },
            )
            .expect("tool request should persist");
        let routed = registry
            .route_tool_request(&mut bus, &agent_id, request)
            .expect("tool request should route");
        assert_eq!(routed.provider_connection_id, tool_id.clone());

        let result = tool_reader.read_event().expect("read").expect("tool result should arrive");
        let Event::ToolResult(result) = result else {
            panic!("expected tool result from tool extension");
        };
        store
            .append_tool_activity(
                "session-1",
                ToolActivityRecord {
                    call_id: result.call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    outcome: ToolActivityOutcome::Result {
                        result: result.result.clone(),
                    },
                },
            )
            .expect("tool result should persist");
        let _ = bus.publish_from(Some(&tool_id), Event::ToolResult(result));

        let response = agent_reader
            .read_event()
            .expect("read")
            .expect("agent response should arrive");
        let Event::MessageAgent(response_message) = response else {
            panic!("expected agent message");
        };
        store
            .append_agent_message("session-1", response_message.text.clone())
            .expect("agent message should persist");
        let delivered = bus.publish_from(Some(&agent_id), Event::MessageAgent(response_message));
        assert_eq!(delivered.delivered_to, vec![ui_id.clone()]);
        assert_eq!(ui_inbox.snapshot().len(), 1);

        let reopened = SessionStore::open(&store_path).expect("store should reopen");
        let session = reopened
            .session("session-1")
            .expect("session should reload");
        assert_eq!(session.entries.len(), 4);

        bus.send_to(
            &agent_id,
            Some(&ui_id),
            Event::LifecycleDisconnect(tau_proto::LifecycleDisconnect {
                reason: Some("test complete".to_owned()),
            }),
        )
        .expect("agent disconnect should route");
        bus.send_to(
            &tool_id,
            Some(&ui_id),
            Event::LifecycleDisconnect(tau_proto::LifecycleDisconnect {
                reason: Some("test complete".to_owned()),
            }),
        )
        .expect("tool disconnect should route");

        agent_thread.join().expect("agent thread should finish");
        tool_thread.join().expect("tool thread should finish");
    }
}
