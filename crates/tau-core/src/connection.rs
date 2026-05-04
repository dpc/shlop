//! Transport-agnostic connection abstractions used by the [`crate::bus`].
//!
//! Stdio, Unix socket, and in-memory test clients all plug into the same
//! routing layer through the [`ConnectionSink`] trait.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use tau_proto::{ClientKind, ConnectionId, Event};

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
    pub(crate) metadata: ConnectionMetadata,
    pub(crate) sink: Box<dyn ConnectionSink>,
    pub(crate) visibility_filter: Box<dyn VisibilityFilter>,
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
