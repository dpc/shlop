//! Thread-safe append-only in-memory event log used by client follower
//! threads for replay + live delivery.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};

use tau_proto::{ConnectionId, Event};

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
/// Consumers track their own position and call [`EventLog::get_next_from`] or
/// [`EventLog::wait_next_from`] in a loop. The log does not track subscribers.
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
    /// [`EventLog::wait_next_from`].
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
