//! Core event bus, routing, and connection abstractions.
//!
//! This crate keeps transport details outside the routing layer. Stdio, Unix
//! socket, and in-memory test clients can all plug into the same bus through a
//! small [`ConnectionSink`] interface.

mod bus;
mod connection;
mod event_log;
mod memory;
mod policy;
mod session;
mod session_store;
mod tool_registry;

#[cfg(test)]
mod tests;

pub use bus::EventBus;
pub use connection::{
    AllowAll, Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError,
    ConnectionSink, DeliveryFailure, RouteError, RouteReport, RoutedEvent, VisibilityFilter,
};
pub use event_log::{EventLog, EventSeq, LogEntry};
pub use memory::{MemoryInbox, memory_connection};
pub use policy::{
    DefaultSubscriptionPolicy, PolicyStore, SubscriptionApproval, SubscriptionPolicy,
    SubscriptionPolicyError,
};
pub use session::{
    NodeId, PersistedSessionEvent, SessionEntry, SessionMeta, SessionNode, SessionTree,
    ToolActivityOutcome, ToolActivityRecord,
};
pub use session_store::{SessionStore, SessionStoreError, list_session_metas};
pub use tool_registry::{
    RegisterToolReport, ToolProvider, ToolRegistry, ToolRegistryWarning, ToolRouteError,
    ToolRouteReport,
};
