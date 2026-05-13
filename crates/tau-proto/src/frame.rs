//! Wire envelope: a `Frame` is either a [`Message`] (control-plane
//! point-to-point) or an [`Event`] (bus-broadcast fact).
//!
//! `Frame` is an untagged enum: serde tries [`Message`] first (matches
//! when the inner map carries a `"message"` discriminator); otherwise
//! it falls through to [`Event`] (which uses an `"event"` discriminator).
//!
//! All transports — stdio, Unix sockets, in-memory channels — read and
//! write `Frame`. Higher layers split: the bus only routes `Event`s,
//! while messages are dispatched directly to the harness's connection
//! state machine.

use serde::{Deserialize, Serialize};

use crate::Event;
use crate::messages::{LogEventId, Message};

/// Top-level wire envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
// Keep `Frame` allocation-free: it is the central wire envelope and callers
// routinely construct, match, and move `Frame::Event` values directly.
#[allow(clippy::large_enum_variant)]
pub enum Frame {
    Message(Message),
    Event(Event),
}

impl Frame {
    /// If this frame is a [`Message::LogEvent`], peel it: return the
    /// log id and a fresh [`Frame::Event`] carrying the inner bus
    /// fact. Otherwise return `(None, self)` unchanged.
    ///
    /// Receivers that want at-least-once semantics ack the returned
    /// id after processing the inner event.
    #[must_use]
    pub fn peel_log(self) -> (Option<LogEventId>, Self) {
        match self {
            Self::Message(Message::LogEvent(env)) => (Some(env.id), Self::Event(*env.event)),
            other => (None, other),
        }
    }
}

impl From<Event> for Frame {
    fn from(event: Event) -> Self {
        Self::Event(event)
    }
}

impl From<Message> for Frame {
    fn from(message: Message) -> Self {
        Self::Message(message)
    }
}
