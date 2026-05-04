//! [`EventBus`]: routes protocol events between connections, gates
//! subscriptions through a [`SubscriptionPolicy`], and tracks per-connection
//! subscription state.

use std::collections::HashMap;

use tau_proto::{ConnectionId, Event, EventSelector, LifecycleSubscribe};

use crate::connection::{
    Connection, ConnectionMetadata, ConnectionSink, DeliveryFailure, RouteError, RouteReport,
    RoutedEvent, VisibilityFilter,
};
use crate::policy::{DefaultSubscriptionPolicy, SubscriptionPolicy};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SubscriptionSet {
    selectors: Vec<EventSelector>,
}

impl SubscriptionSet {
    pub(crate) fn replace(&mut self, selectors: Vec<EventSelector>) {
        self.selectors = selectors;
    }

    pub(crate) fn matches(&self, event: &Event) -> bool {
        self.selectors
            .iter()
            .any(|selector| selector_matches(selector, event))
    }

    pub(crate) fn selectors(&self) -> &[EventSelector] {
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

pub(crate) struct ConnectionEntry {
    pub(crate) metadata: ConnectionMetadata,
    pub(crate) sink: Box<dyn ConnectionSink>,
    pub(crate) visibility_filter: Box<dyn VisibilityFilter>,
    pub(crate) subscriptions: SubscriptionSet,
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
