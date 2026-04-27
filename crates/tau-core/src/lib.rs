//! Core event bus, routing, and connection abstractions.
//!
//! This crate keeps transport details outside the routing layer. Stdio, Unix
//! socket, and in-memory test clients can all plug into the same bus through a
//! small [`ConnectionSink`] interface.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_proto::{
    ClientKind, ConnectionId, Event, EventSelector, LifecycleSubscribe, SessionId, ToolCallId,
    ToolName, ToolRequest, ToolSpec,
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
    // For event-log deliveries, match against the inner event's name —
    // subscribers like to subscribe to "session.started", not "wire.log_event".
    let target_name = match event {
        Event::LogEvent(env) => env.event.name(),
        _ => event.name(),
    };
    match selector {
        EventSelector::Exact(name) => *name == target_name,
        EventSelector::Prefix(prefix) => target_name.matches_prefix(prefix),
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
            // Closed list of categories a socket client is allowed to
            // subscribe to. The unknown `EventCategory::Other` and the
            // wire-level `Wire` family are rejected outright — UI
            // clients should not see at-least-once envelope plumbing.
            fn category_allowed(category: &tau_proto::EventCategory) -> bool {
                use tau_proto::EventCategory as C;
                matches!(
                    category,
                    C::Tool
                        | C::Extension
                        | C::Agent
                        | C::Session
                        | C::Ui
                        | C::Harness
                        | C::Shell
                        | C::Term
                )
            }
            for selector in selectors {
                let allowed = match selector {
                    EventSelector::Exact(name) => category_allowed(&name.category),
                    EventSelector::Prefix(prefix) => {
                        // The category portion of the prefix must
                        // resolve to a known, allowed category.
                        let category_str = prefix.split_once('.').map(|(c, _)| c).unwrap_or(prefix);
                        let category = tau_proto::EventCategory::from_wire(category_str);
                        category_allowed(&category)
                    }
                };
                if !allowed {
                    return Err(SubscriptionPolicyError::new(
                        "socket clients may only subscribe to allowed event families",
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
                connection_id: connection_id.into(),
            })?;
        self.subscription_policy
            .evaluate(&metadata, &selectors)
            .map_err(|error| RouteError::SubscriptionDenied {
                connection_id: connection_id.into(),
                reason: error.reason().to_owned(),
            })?;
        let entry = self.connections.get_mut(connection_id).ok_or_else(|| {
            RouteError::UnknownConnection {
                connection_id: connection_id.into(),
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

        let routed_event = RoutedEvent::new(source_id.map(ConnectionId::from), event);
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
        let routed_event = RoutedEvent::new(source_id.map(ConnectionId::from), event);
        let entry =
            self.connections
                .get_mut(target_id)
                .ok_or_else(|| RouteError::UnknownConnection {
                    connection_id: target_id.into(),
                })?;

        let mut report = RouteReport::default();
        if !entry.visibility_filter.allows(&routed_event) {
            report.blocked_by_filter.push(target_id.into());
            return Ok(report);
        }

        match entry.sink.send(routed_event) {
            Ok(()) => report.delivered_to.push(target_id.into()),
            Err(error) => report.failed_deliveries.push(DeliveryFailure {
                connection_id: target_id.into(),
                error,
            }),
        }

        Ok(report)
    }

    fn allocate_connection_id(&mut self) -> ConnectionId {
        self.next_connection_id += 1;
        format!("conn-{}", self.next_connection_id).into()
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
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, Default, PartialEq)]
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
                connection_id: connection_id.into(),
                tool,
            });
        }

        let connection_tools = self
            .tools_by_connection
            .entry(connection_id.into())
            .or_default();
        if !connection_tools.contains(&tool_name) {
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

    /// Returns all unique tool names currently registered.
    #[must_use]
    pub fn all_tool_names(&self) -> Vec<&ToolName> {
        self.providers_by_tool.keys().collect()
    }

    /// Returns all unique tool specs, one per tool name (first provider wins).
    #[must_use]
    pub fn all_tools(&self) -> Vec<&ToolSpec> {
        self.providers_by_tool
            .values()
            .filter_map(|providers| providers.first().map(|p| &p.tool))
            .collect()
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
    pub call_id: ToolCallId,
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

/// Unique identifier for a node in the session tree.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct NodeId(pub u64);

/// One node in the session tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionNode {
    pub id: NodeId,
    pub parent_id: Option<NodeId>,
    pub entry: SessionEntry,
}

/// Tree-structured session history with branching.
///
/// Each entry is a node with a unique ID and parent pointer. The
/// `head` tracks the current position. Branching = moving head to an
/// earlier node; the next append creates a new branch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionTree {
    session_id: SessionId,
    nodes: Vec<SessionNode>,
    head: Option<NodeId>,
}

impl SessionTree {
    /// Returns the session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the current head node ID, if any.
    #[must_use]
    pub fn head(&self) -> Option<NodeId> {
        self.head
    }

    /// Returns a node by ID.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&SessionNode> {
        self.nodes.get(id.0 as usize)
    }

    /// Returns all nodes.
    #[must_use]
    pub fn nodes(&self) -> &[SessionNode] {
        &self.nodes
    }

    /// Returns the entries along the current branch (root to head).
    #[must_use]
    pub fn current_branch(&self) -> Vec<&SessionEntry> {
        let mut path = Vec::new();
        let mut current = self.head;
        while let Some(id) = current {
            if let Some(node) = self.nodes.get(id.0 as usize) {
                path.push(&node.entry);
                current = node.parent_id;
            } else {
                break;
            }
        }
        path.reverse();
        path
    }

    /// Returns the direct children of a node.
    #[must_use]
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| n.parent_id == Some(id))
            .map(|n| n.id)
            .collect()
    }

    fn append_node(&mut self, entry: SessionEntry) -> NodeId {
        let id = NodeId(self.nodes.len() as u64);
        self.nodes.push(SessionNode {
            id,
            parent_id: self.head,
            entry,
        });
        self.head = Some(id);
        id
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum PersistedSessionRecord {
    Node {
        id: NodeId,
        parent_id: Option<NodeId>,
        entry: SessionEntry,
    },
    SetHead {
        node_id: NodeId,
    },
}

/// Per-session sidecar metadata at `<state_dir>/<session_id>/meta.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Working directory at the time of session creation.
    pub cwd: Option<PathBuf>,
    /// Unix epoch seconds when the session was first created.
    pub created_at: u64,
    /// Unix epoch seconds of the most recent append.
    pub last_touched: u64,
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
    /// Another process holds the exclusive lock on this session.
    Locked {
        path: PathBuf,
        holder: String,
    },
    InvalidSessionDir {
        path: PathBuf,
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
            Self::Locked { path, holder } => write!(
                f,
                "session lock at {} held by another process ({})",
                path.display(),
                holder.trim()
            ),
            Self::InvalidSessionDir { path } => write!(
                f,
                "invalid session directory name (non-utf8): {}",
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
            Self::Locked { .. } => None,
            Self::InvalidSessionDir { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Event log
// ---------------------------------------------------------------------------

/// Monotonically increasing sequence number for log entries.
pub type EventSeq = u64;

/// One entry in the event log.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub seq: EventSeq,
    pub source: Option<ConnectionId>,
    pub event: Event,
}

struct EventLogInner {
    entries: BTreeMap<EventSeq, LogEntry>,
    next_seq: EventSeq,
}

/// Thread-safe append-only event log.
///
/// Consumers track their own position and call [`get_next_from`] or
/// [`wait_next_from`] in a loop. The log does not track subscribers.
pub struct EventLog {
    inner: Mutex<EventLogInner>,
    condvar: Condvar,
}

impl EventLog {
    /// Creates an empty event log.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(EventLogInner {
                entries: BTreeMap::new(),
                next_seq: 0,
            }),
            condvar: Condvar::new(),
        })
    }

    /// Appends an event and wakes any threads blocked in
    /// [`wait_next_from`].
    pub fn append(&self, source: Option<ConnectionId>, event: Event) -> EventSeq {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.entries.insert(seq, LogEntry { seq, source, event });
        self.condvar.notify_all();
        seq
    }

    /// Returns the first entry with seq >= `from`, or `None` if no such
    /// entry exists yet.
    pub fn get_next_from(&self, from: EventSeq) -> Option<LogEntry> {
        let inner = self.inner.lock().expect("event log mutex poisoned");
        inner
            .entries
            .range(from..)
            .next()
            .map(|(_, entry)| entry.clone())
    }

    /// Blocks until an entry with seq >= `from` exists, then returns it.
    pub fn wait_next_from(&self, from: EventSeq) -> LogEntry {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        loop {
            if let Some((_, entry)) = inner.entries.range(from..).next() {
                return entry.clone();
            }
            inner = self.condvar.wait(inner).expect("event log mutex poisoned");
        }
    }

    /// Returns the sequence number that the next appended entry will
    /// receive.
    pub fn next_seq(&self) -> EventSeq {
        self.inner
            .lock()
            .expect("event log mutex poisoned")
            .next_seq
    }

    /// Removes all entries with seq < `min_seq`.
    pub fn prune_below(&self, min_seq: EventSeq) {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        inner.entries = inner.entries.split_off(&min_seq);
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self {
            inner: Mutex::new(EventLogInner {
                entries: BTreeMap::new(),
                next_seq: 0,
            }),
            condvar: Condvar::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Session store
// ---------------------------------------------------------------------------

/// Append-only persistence for tree-structured session history.
///
/// Each session lives in its own directory under `state_dir`:
///
/// ```text
/// <state_dir>/<session_id>/
///   log.cbor      # length-prefixed PersistedSessionRecord stream
///   meta.json     # SessionMeta sidecar (cwd, created_at, last_touched)
///   lock          # exclusively flock'd while this store has the session loaded for write
/// ```
///
/// Existing session dirs are eagerly loaded into memory at `open()`; their
/// flocks are taken lazily on first write so read-only consumers (e.g.
/// inspection commands) don't contend with a running daemon.
#[derive(Debug)]
pub struct SessionStore {
    state_dir: PathBuf,
    sessions: HashMap<SessionId, SessionTree>,
    /// Held flocks per session, acquired lazily on first write. Released when
    /// this store is dropped (the OS releases the flock when the file
    /// handle closes).
    locks: HashMap<SessionId, File>,
}

impl SessionStore {
    /// Opens the session store rooted at `state_dir`, eagerly loading every
    /// session subdirectory found there.
    pub fn open(state_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let state_dir = state_dir.into();
        fs::create_dir_all(&state_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: state_dir.clone(),
                source,
            }
        })?;

        let mut sessions = HashMap::new();
        for entry in fs::read_dir(&state_dir).map_err(|source| SessionStoreError::Read {
            path: state_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| SessionStoreError::Read {
                path: state_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let log_path = path.join("log.cbor");
            if !log_path.exists() {
                continue;
            }
            let session_id_str = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| SessionStoreError::InvalidSessionDir { path: path.clone() })?;
            let sid: SessionId = session_id_str.into();
            let tree = load_session_log(&log_path, &sid)?;
            sessions.insert(sid, tree);
        }

        Ok(Self {
            state_dir,
            sessions,
            locks: HashMap::new(),
        })
    }

    /// Returns the path to one session's directory (created lazily on write).
    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.state_dir.join(session_id)
    }

    /// Acquires an exclusive flock on the session's `lock` file if not already
    /// held.
    fn ensure_locked(&mut self, session_id: &str) -> Result<(), SessionStoreError> {
        let sid: SessionId = session_id.into();
        if self.locks.contains_key(&sid) {
            return Ok(());
        }
        let session_dir = self.session_dir(session_id);
        fs::create_dir_all(&session_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: session_dir.clone(),
                source,
            }
        })?;
        let lock_path = session_dir.join("lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| SessionStoreError::Open {
                path: lock_path.clone(),
                source,
            })?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            return Err(SessionStoreError::Locked {
                path: lock_path,
                holder,
            });
        }
        // Replace lock contents with our PID + start time.
        file.set_len(0).map_err(|source| SessionStoreError::Write {
            path: lock_path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|source| SessionStoreError::Write {
                path: lock_path.clone(),
                source,
            })?;
        let pid = std::process::id();
        let now = unix_now();
        writeln!(&mut file, "pid={pid} start={now}").map_err(|source| {
            SessionStoreError::Write {
                path: lock_path.clone(),
                source,
            }
        })?;
        self.locks.insert(sid, file);
        Ok(())
    }

    /// Appends an entry at the current head, returns the new node ID.
    pub fn append(
        &mut self,
        session_id: &str,
        entry: SessionEntry,
    ) -> Result<NodeId, SessionStoreError> {
        self.ensure_locked(session_id)?;
        let sid: SessionId = session_id.into();
        let tree = self
            .sessions
            .entry(sid.clone())
            .or_insert_with(|| SessionTree {
                session_id: sid.clone(),
                nodes: Vec::new(),
                head: None,
            });
        let parent_id = tree.head;
        let id = tree.append_node(entry.clone());
        let record = PersistedSessionRecord::Node {
            id,
            parent_id,
            entry,
        };
        let session_dir = self.session_dir(session_id);
        append_record(&session_dir.join("log.cbor"), &record)?;
        touch_meta(&session_dir.join("meta.json"))?;
        Ok(id)
    }

    /// Moves the head pointer to an existing node (branch switch).
    pub fn set_head(&mut self, session_id: &str, node_id: NodeId) -> Result<(), SessionStoreError> {
        self.ensure_locked(session_id)?;
        let session_dir = self.session_dir(session_id);
        let tree = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionStoreError::Open {
                path: session_dir.clone(),
                source: io::Error::new(io::ErrorKind::NotFound, "session not found"),
            })?;
        tree.head = Some(node_id);
        let record = PersistedSessionRecord::SetHead { node_id };
        append_record(&session_dir.join("log.cbor"), &record)?;
        touch_meta(&session_dir.join("meta.json"))
    }

    /// Appends one user message to a session.
    pub fn append_user_message(
        &mut self,
        session_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<NodeId, SessionStoreError> {
        self.append(
            &session_id.into(),
            SessionEntry::UserMessage { text: text.into() },
        )
    }

    /// Appends one agent message to a session.
    pub fn append_agent_message(
        &mut self,
        session_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<NodeId, SessionStoreError> {
        self.append(
            &session_id.into(),
            SessionEntry::AgentMessage { text: text.into() },
        )
    }

    /// Appends one tool activity record to a session.
    pub fn append_tool_activity(
        &mut self,
        session_id: impl Into<String>,
        activity: ToolActivityRecord,
    ) -> Result<NodeId, SessionStoreError> {
        self.append(&session_id.into(), SessionEntry::ToolActivity(activity))
    }

    /// Returns the state dir this store is rooted at.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns one session tree if it exists.
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<&SessionTree> {
        self.sessions.get(session_id)
    }

    /// Returns all known sessions.
    #[must_use]
    pub fn sessions(&self) -> Vec<&SessionTree> {
        self.sessions.values().collect()
    }

    /// Records initial cwd metadata for a session if not already present.
    /// Idempotent: subsequent calls only update `last_touched` via
    /// [`touch_meta`].
    pub fn record_session_meta(
        &mut self,
        session_id: &str,
        cwd: Option<PathBuf>,
    ) -> Result<(), SessionStoreError> {
        self.ensure_locked(session_id)?;
        let path = self.session_dir(session_id).join("meta.json");
        let now = unix_now();
        let mut meta = read_meta(&path).unwrap_or_default();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        if meta.cwd.is_none() {
            meta.cwd = cwd;
        }
        meta.last_touched = now;
        write_meta(&path, &meta)
    }
}

/// Lists session metadata across `state_dir` without taking any flocks.
///
/// Sessions whose `meta.json` is missing or malformed are skipped silently;
/// the goal is best-effort discovery for `-r` resumption, not strict listing.
pub fn list_session_metas(state_dir: &Path) -> io::Result<Vec<(SessionId, SessionMeta)>> {
    let mut out = Vec::new();
    if !state_dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(state_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let meta_path = path.join("meta.json");
        let Ok(meta) = read_meta(&meta_path) else {
            continue;
        };
        out.push((SessionId::from(name), meta));
    }
    Ok(out)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_meta(path: &Path) -> io::Result<SessionMeta> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn write_meta(path: &Path, meta: &SessionMeta) -> Result<(), SessionStoreError> {
    let bytes = serde_json::to_vec_pretty(meta).map_err(|e| SessionStoreError::Write {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })?;
    fs::write(path, bytes).map_err(|source| SessionStoreError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Updates `last_touched` on the session's meta sidecar (creating it with
/// `created_at = now` if absent).
fn touch_meta(path: &Path) -> Result<(), SessionStoreError> {
    let now = unix_now();
    let mut meta = read_meta(path).unwrap_or_default();
    if meta.created_at == 0 {
        meta.created_at = now;
    }
    meta.last_touched = now;
    write_meta(path, &meta)
}

fn load_session_log(
    log_path: &Path,
    session_id: &SessionId,
) -> Result<SessionTree, SessionStoreError> {
    let mut tree = SessionTree {
        session_id: session_id.clone(),
        nodes: Vec::new(),
        head: None,
    };
    let mut file = File::open(log_path).map_err(|source| SessionStoreError::Open {
        path: log_path.to_path_buf(),
        source,
    })?;
    loop {
        let mut length_bytes = [0_u8; 8];
        match file.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(tree),
            Err(source) => {
                return Err(SessionStoreError::Read {
                    path: log_path.to_path_buf(),
                    source,
                });
            }
        }

        let record_length = u64::from_le_bytes(length_bytes) as usize;
        let mut record_bytes = vec![0_u8; record_length];
        file.read_exact(&mut record_bytes)
            .map_err(|source| SessionStoreError::Read {
                path: log_path.to_path_buf(),
                source,
            })?;

        let record: PersistedSessionRecord = ciborium::from_reader(record_bytes.as_slice())
            .map_err(|source| SessionStoreError::Decode {
                path: log_path.to_path_buf(),
                source,
            })?;

        match record {
            PersistedSessionRecord::Node {
                id,
                parent_id,
                entry,
            } => {
                debug_assert!(id.0 == tree.nodes.len() as u64);
                tree.nodes.push(SessionNode {
                    id,
                    parent_id,
                    entry,
                });
                tree.head = Some(id);
            }
            PersistedSessionRecord::SetHead { node_id } => {
                tree.head = Some(node_id);
            }
        }
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
            id: ConnectionId::default(),
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
        AgentResponseFinished, CborValue, EventName, EventReader, EventWriter, LifecycleSubscribe,
        SessionPromptCreated, ToolRegister, ToolSideEffects, UiPromptSubmitted,
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
                id: ConnectionId::default(),
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
            vec![EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)],
        )
        .expect("agent subscriptions should be stored");
        bus.set_subscriptions(&ui_id, vec![EventSelector::Prefix("tool.".to_owned())])
            .expect("ui subscriptions should be stored");

        let report = bus.publish(Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
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
            vec![EventSelector::Exact(EventName::LIFECYCLE_SUBSCRIBE)],
        )
        .expect("initial subscriptions should be stored");

        let report = bus.publish_from(
            Some(&connection_id),
            Event::LifecycleSubscribe(LifecycleSubscribe {
                selectors: vec![EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)],
            }),
        );

        assert_eq!(report.delivered_to, Vec::<ConnectionId>::new());
        assert_eq!(
            bus.subscriptions(&connection_id),
            Some([EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)].as_slice())
        );
        assert!(inbox.snapshot().is_empty());
    }

    #[test]
    fn directed_events_ignore_subscriptions_but_still_use_visibility_filters() {
        let mut bus = EventBus::new();

        let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
        let filtered_connection =
            ui_connection.with_visibility_filter(Box::new(|event: &RoutedEvent| {
                event.event.name() == EventName::TOOL_INVOKE
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
                Event::AgentResponseFinished(AgentResponseFinished {
                    session_prompt_id: "sp-1".into(),
                    text: Some("hidden".to_owned()),
                    tool_calls: Vec::new(),
                    input_tokens: None,
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
                    call_id: "call-1".into(),
                    tool_name: "echo".into(),
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

        bus.set_subscriptions(&agent_id, vec![EventSelector::Prefix("agent.".to_owned())])
            .expect("agent subscriptions should be stored");
        bus.set_subscriptions(&tool_id, vec![EventSelector::Prefix("tool.".to_owned())])
            .expect("tool subscriptions should be stored");

        let first_report = bus.publish(Event::ToolResult(tau_proto::ToolResult {
            call_id: "call-1".into(),
            tool_name: "echo".into(),
            result: CborValue::Text("done".to_owned()),
        }));
        assert_eq!(first_report.delivered_to, vec![tool_id.clone()]);

        let second_report = bus.publish(Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-1".into(),
            text: Some("done".to_owned()),
            tool_calls: Vec::new(),
            input_tokens: None,
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
                name: "echo".into(),
                description: Some("Echo a payload".to_owned()),
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        );
        assert!(register_report.warnings.is_empty());

        let route_report = registry
            .route_tool_request(
                &mut bus,
                &agent_id,
                ToolRequest {
                    call_id: "call-1".into(),
                    tool_name: "echo".into(),
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
                call_id: "call-1".into(),
                tool_name: "echo".into(),
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
                name: "echo".into(),
                description: Some("Echo".to_owned()),
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        );
        assert!(first_report.warnings.is_empty());

        let second_report = registry.register(
            "conn-b",
            ToolSpec {
                name: "echo".into(),
                description: Some("Echo from another provider".to_owned()),
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        );
        assert_eq!(second_report.warnings.len(), 1);
        assert_eq!(
            second_report.warnings[0],
            ToolRegistryWarning::DuplicateRegistration {
                tool_name: "echo".into(),
                existing_provider_ids: vec!["conn-a".into()],
            }
        );

        let providers = registry.providers_for("echo");
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
                name: "echo".into(),
                description: None,
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        );
        registry.register(
            &second_id,
            ToolSpec {
                name: "echo".into(),
                description: None,
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        );
        registry.register(
            &first_id,
            ToolSpec {
                name: "demo_upper".into(),
                description: None,
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        );

        let disconnected = bus.disconnect(&first_id);
        assert!(disconnected.is_some());
        let removed_tools = registry.unregister_connection(&first_id);
        assert_eq!(removed_tools.len(), 2);
        assert!(removed_tools.iter().any(|tool_name| tool_name == "echo"));
        assert!(
            removed_tools
                .iter()
                .any(|tool_name| tool_name == "demo_upper")
        );

        let echo_providers = registry.providers_for("echo");
        assert_eq!(echo_providers.len(), 1);
        assert_eq!(echo_providers[0].connection_id, second_id);
        assert!(registry.providers_for("demo_upper").is_empty());
    }

    #[test]
    fn register_events_map_cleanly_to_registry_state() {
        let mut registry = ToolRegistry::new();

        let report = registry.register(
            "conn-tool",
            ToolRegister {
                tool: ToolSpec {
                    name: "echo".into(),
                    description: Some("Echo".to_owned()),
                    parameters: None,
                    side_effects: ToolSideEffects::Pure,
                },
            }
            .tool,
        );

        assert!(report.warnings.is_empty());
        assert_eq!(registry.providers_for("echo").len(), 1);
        assert!(registry.unregister("conn-tool", "echo"));
        assert!(registry.providers_for("echo").is_empty());
    }

    #[test]
    fn session_tree_persists_across_reopen() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("state");

        let mut store = SessionStore::open(&store_path).expect("store should open");
        let id0 = store
            .append_user_message("session-1", "hello")
            .expect("user message should persist");
        let id1 = store
            .append_agent_message("session-1", "hi there")
            .expect("agent message should persist");

        assert_eq!(id0, NodeId(0));
        assert_eq!(id1, NodeId(1));

        let reopened = SessionStore::open(&store_path).expect("store should reopen");
        let tree = reopened
            .session("session-1")
            .expect("session should reload");
        assert_eq!(tree.head(), Some(NodeId(1)));
        assert_eq!(
            tree.current_branch(),
            vec![
                &SessionEntry::UserMessage {
                    text: "hello".to_owned(),
                },
                &SessionEntry::AgentMessage {
                    text: "hi there".to_owned(),
                },
            ]
        );
        // Verify tree structure.
        assert!(tree.node(NodeId(0)).expect("node 0").parent_id.is_none());
        assert_eq!(
            tree.node(NodeId(1)).expect("node 1").parent_id,
            Some(NodeId(0))
        );
    }

    #[test]
    fn session_tree_supports_branching() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("state");

        let mut store = SessionStore::open(&store_path).expect("store should open");
        let _ = store.append_user_message("s1", "hello").expect("ok");
        let _ = store.append_agent_message("s1", "hi").expect("ok");
        // Branch: go back to node 0 and append a different message.
        store
            .set_head("s1", NodeId(0))
            .expect("set_head should work");
        let _ = store.append_user_message("s1", "goodbye").expect("ok");

        let tree = store.session("s1").expect("session should exist");
        assert_eq!(tree.head(), Some(NodeId(2)));
        // Current branch: hello → goodbye (skipping "hi").
        assert_eq!(
            tree.current_branch(),
            vec![
                &SessionEntry::UserMessage {
                    text: "hello".to_owned(),
                },
                &SessionEntry::UserMessage {
                    text: "goodbye".to_owned(),
                },
            ]
        );
        // Node 0 has two children (branching point).
        let mut children = tree.children(NodeId(0));
        children.sort_by_key(|id| id.0);
        assert_eq!(children, vec![NodeId(1), NodeId(2)]);

        // Verify persistence across reopen.
        let reopened = SessionStore::open(&store_path).expect("reopen");
        let tree2 = reopened.session("s1").expect("session");
        assert_eq!(tree2.head(), Some(NodeId(2)));
        assert_eq!(tree2.current_branch().len(), 2);
    }

    #[test]
    fn session_tree_associates_tool_activity() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("state");

        let mut store = SessionStore::open(&store_path).expect("store should open");
        store
            .append_user_message("session-1", "read a file")
            .expect("user message should persist");
        store
            .append_tool_activity(
                "session-1",
                ToolActivityRecord {
                    call_id: "call-1".into(),
                    tool_name: "read".into(),
                    outcome: ToolActivityOutcome::Result {
                        result: CborValue::Text("README".to_owned()),
                    },
                },
            )
            .expect("tool activity should persist");

        let reopened = SessionStore::open(&store_path).expect("store should reopen");
        let tree = reopened
            .session("session-1")
            .expect("session should reload");
        let branch = tree.current_branch();
        assert_eq!(branch.len(), 2);
        assert_eq!(
            *branch[1],
            SessionEntry::ToolActivity(ToolActivityRecord {
                call_id: "call-1".into(),
                tool_name: "read".into(),
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
                id: ConnectionId::default(),
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
                reason: "socket clients may only subscribe to allowed event families".to_owned(),
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
                id: ConnectionId::default(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin: ConnectionOrigin::Socket,
            },
            Box::new(MemorySink { inbox }),
        );
        let connection_id = bus.connect(connection);

        bus.set_subscriptions(
            &connection_id,
            vec![EventSelector::Prefix("tool.".to_owned())],
        )
        .expect("allowed socket subscription should persist");

        let reopened = PolicyStore::open(&policy_path).expect("policy store should reopen");
        assert_eq!(
            reopened.approvals(),
            [SubscriptionApproval {
                connection_name: "socket-ui".to_owned(),
                connection_origin: ConnectionOrigin::Socket,
                selectors: vec![EventSelector::Prefix("tool.".to_owned())],
            }]
            .as_slice()
        );
    }

    #[test]
    fn deterministic_agent_and_tool_complete_one_vertical_slice() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let store_path = tempdir.path().join("state");
        let _store = SessionStore::open(&store_path).expect("store should open");
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
            tau_ext_shell::run(tool_reader, tool_runtime_stream, true)
                .expect("tool extension should run successfully");
        });

        let (agent_connection, mut agent_reader) =
            stream_connection("agent", ClientKind::Agent, agent_harness_stream);
        let (tool_connection, mut tool_reader) =
            stream_connection("tool", ClientKind::Tool, tool_harness_stream);
        let agent_id = bus.connect(agent_connection);
        let tool_id = bus.connect(tool_connection);

        let (ui_connection, _ui_inbox) = memory_connection("ui", ClientKind::Ui);
        let ui_id = bus.connect(ui_connection);
        bus.set_subscriptions(
            &ui_id,
            vec![EventSelector::Exact(EventName::AGENT_RESPONSE_FINISHED)],
        )
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

        let tool_hello = tool_reader
            .read_event()
            .expect("read")
            .expect("tool hello should arrive");
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
        assert!(registered_tool_names.iter().any(|name| name == "echo"));
        assert!(registered_tool_names.iter().any(|name| name == "read"));

        // Send a SessionPromptCreated to the agent (new protocol).
        use tau_proto::{ContentBlock, ConversationMessage, ConversationRole, ToolDefinition};

        let prompt = SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "session-1".into(),
            system_prompt: "You are helpful.".to_owned(),
            messages: vec![ConversationMessage {
                role: ConversationRole::User,
                content: vec![ContentBlock::Text {
                    text: "hello".to_owned(),
                }],
            }],
            tools: vec![ToolDefinition {
                name: "echo".into(),
                description: None,
                parameters: None,
            }],
            model: None,
            effort: tau_proto::Effort::Off,
        };
        let _ = bus.send_to(&agent_id, None, Event::SessionPromptCreated(prompt));

        // Without a model, the agent should report an error.
        let response = loop {
            let ev = agent_reader
                .read_event()
                .expect("read")
                .expect("agent event should arrive");
            if let Event::AgentResponseFinished(r) = ev {
                break r;
            }
        };
        assert!(response.text.as_deref().unwrap_or("").contains("no model"));
        assert!(response.tool_calls.is_empty());

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

    // -----------------------------------------------------------------------
    // EventLog tests
    // -----------------------------------------------------------------------

    #[test]
    fn event_log_append_and_get() {
        let log = EventLog::new();
        let seq = log.append(
            Some("conn-1".into()),
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: "hello".to_owned(),

                level: tau_proto::HarnessInfoLevel::Normal,
            }),
        );
        assert_eq!(seq, 0);
        assert_eq!(log.next_seq(), 1);

        let entry = log.get_next_from(0).expect("entry should exist");
        assert_eq!(entry.seq, 0);
        assert_eq!(entry.source, Some("conn-1".into()));

        assert!(log.get_next_from(1).is_none());
    }

    #[test]
    fn event_log_get_next_from_skips_earlier() {
        let log = EventLog::new();
        log.append(
            None,
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: "a".to_owned(),

                level: tau_proto::HarnessInfoLevel::Normal,
            }),
        );
        log.append(
            None,
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: "b".to_owned(),

                level: tau_proto::HarnessInfoLevel::Normal,
            }),
        );
        log.append(
            None,
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: "c".to_owned(),

                level: tau_proto::HarnessInfoLevel::Normal,
            }),
        );

        let entry = log.get_next_from(1).expect("entry should exist");
        assert_eq!(entry.seq, 1);
        let Event::HarnessInfo(info) = &entry.event else {
            panic!("expected HarnessInfo");
        };
        assert_eq!(info.message, "b");
    }

    #[test]
    fn event_log_wait_next_from_blocks_then_returns() {
        let log = EventLog::new();
        let log2 = Arc::clone(&log);

        let handle = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(20));
            log2.append(
                None,
                Event::HarnessInfo(tau_proto::HarnessInfo {
                    message: "delayed".to_owned(),

                    level: tau_proto::HarnessInfoLevel::Normal,
                }),
            );
        });

        let entry = log.wait_next_from(0);
        assert_eq!(entry.seq, 0);
        handle.join().expect("append thread");
    }

    #[test]
    fn event_log_wait_next_from_returns_immediately_if_available() {
        let log = EventLog::new();
        log.append(
            None,
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: "already here".to_owned(),

                level: tau_proto::HarnessInfoLevel::Normal,
            }),
        );

        let entry = log.wait_next_from(0);
        assert_eq!(entry.seq, 0);
    }

    #[test]
    fn event_log_prune_below_removes_old_entries() {
        let log = EventLog::new();
        for i in 0..5 {
            log.append(
                None,
                Event::HarnessInfo(tau_proto::HarnessInfo {
                    message: format!("msg-{i}"),

                    level: tau_proto::HarnessInfoLevel::Normal,
                }),
            );
        }
        assert_eq!(log.next_seq(), 5);

        log.prune_below(3);

        assert!(log.get_next_from(0).is_some());
        // The first available entry should be seq 3.
        let entry = log.get_next_from(0).expect("entry after prune");
        assert_eq!(entry.seq, 3);

        // Entries 0, 1, 2 are gone.
        assert!(log.get_next_from(2).map(|e| e.seq) == Some(3));
    }

    #[test]
    fn event_log_multiple_waiters_wake_on_append() {
        let log = EventLog::new();
        let mut handles = Vec::new();
        for _ in 0..3 {
            let log = Arc::clone(&log);
            handles.push(thread::spawn(move || {
                let entry = log.wait_next_from(0);
                entry.seq
            }));
        }

        thread::sleep(std::time::Duration::from_millis(20));
        log.append(
            None,
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: "wake all".to_owned(),

                level: tau_proto::HarnessInfoLevel::Normal,
            }),
        );

        for h in handles {
            assert_eq!(h.join().expect("waiter thread"), 0);
        }
    }
}
