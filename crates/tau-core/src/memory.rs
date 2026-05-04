//! Snapshot-friendly in-memory connection adapter for tests and in-process
//! integrations.

use std::cell::RefCell;
use std::rc::Rc;

use tau_proto::{ClientKind, ConnectionId};

use crate::connection::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    RoutedEvent,
};

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
pub(crate) struct MemorySink {
    pub(crate) inbox: MemoryInbox,
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
