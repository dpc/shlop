//! Shared protocol types and CBOR stream codec helpers.
//!
//! The wire format is a sequence of self-delimiting CBOR items. Each item is a
//! small map with two keys:
//!
//! - `event`: a dotted event name such as `tool.invoke`
//! - `payload`: the typed payload for that event
//!
//! The codec helpers in this crate work with any [`std::io::Read`] or
//! [`std::io::Write`], so the same protocol layer can be reused for stdio,
//! Unix sockets, tests, or in-memory transports.
//!
//! All event definitions live in [`events`] and are re-exported at the
//! crate root.

mod events;

use std::io::{Cursor, Read, Write};

pub use ciborium::value::Value as CborValue;
pub use events::*;

/// First protocol version implemented by this crate.
pub const PROTOCOL_VERSION: u32 = 1;

/// Convenience alias for session identifiers.
pub type SessionId = String;

/// Convenience alias for tool names.
pub type ToolName = String;

/// Convenience alias for tool call identifiers.
pub type ToolCallId = String;

/// Convenience alias for connection identifiers.
pub type ConnectionId = String;

/// Convenience alias for extension identifiers.
pub type ExtensionName = String;

/// CBOR serialization error used by [`encode_event`] and [`EventWriter`].
pub type EncodeError = ciborium::ser::Error<std::io::Error>;

/// CBOR deserialization error used by [`decode_event`] and [`EventReader`].
pub type DecodeError = ciborium::de::Error<std::io::Error>;

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

/// Encodes one event as a self-delimiting CBOR item.
pub fn encode_event<W>(writer: W, event: &Event) -> Result<(), EncodeError>
where
    W: Write,
{
    ciborium::into_writer(event, writer)
}

/// Decodes one event from a self-delimiting CBOR item.
pub fn decode_event<R>(reader: R) -> Result<Event, DecodeError>
where
    R: Read,
{
    ciborium::from_reader(reader)
}

/// Encodes one event into an owned byte buffer.
pub fn encode_event_to_vec(event: &Event) -> Result<Vec<u8>, EncodeError> {
    let mut bytes = Vec::new();
    encode_event(&mut bytes, event)?;
    Ok(bytes)
}

/// Decodes one event from a byte slice.
pub fn decode_event_from_slice(bytes: &[u8]) -> Result<Event, DecodeError> {
    decode_event(Cursor::new(bytes))
}

/// Stateful writer for a stream of protocol events.
#[derive(Debug)]
pub struct EventWriter<W> {
    inner: W,
}

impl<W> EventWriter<W> {
    /// Wraps an arbitrary writer.
    #[must_use]
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Returns the wrapped writer.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W> EventWriter<W>
where
    W: Write,
{
    /// Writes one protocol event to the stream.
    pub fn write_event(&mut self, event: &Event) -> Result<(), EncodeError> {
        encode_event(&mut self.inner, event)
    }

    /// Flushes the wrapped writer.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Stateful reader for a stream of protocol events.
#[derive(Debug)]
pub struct EventReader<R> {
    inner: R,
}

impl<R> EventReader<R> {
    /// Wraps an arbitrary reader.
    #[must_use]
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Returns the wrapped reader.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R> EventReader<R>
where
    R: Read,
{
    /// Reads one protocol event from the stream.
    ///
    /// Returns `Ok(None)` on clean end-of-stream (EOF at a message
    /// boundary). Returns `Err` only for actual corruption or
    /// truncated data.
    pub fn read_event(&mut self) -> Result<Option<Event>, DecodeError> {
        let mut first = [0_u8; 1];
        match self.inner.read_exact(&mut first) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(DecodeError::Io(e)),
        }
        let chained = Cursor::new(first).chain(&mut self.inner);
        ciborium::from_reader(chained).map(Some)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn representative_events() -> Vec<Event> {
        vec![
            Event::LifecycleHello(LifecycleHello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "agent".to_owned(),
                client_kind: ClientKind::Agent,
            }),
            Event::LifecycleSubscribe(LifecycleSubscribe {
                selectors: vec![
                    EventSelector::Exact(EventName::MessageUser),
                    EventSelector::Prefix("tool.".to_owned()),
                ],
            }),
            Event::LifecycleReady(LifecycleReady {
                message: Some("ready".to_owned()),
            }),
            Event::ToolRegister(ToolRegister {
                tool: ToolSpec {
                    name: "demo.echo".to_owned(),
                    description: Some("Echo a payload".to_owned()),
                },
            }),
            Event::ToolRequest(ToolRequest {
                call_id: "call-1".to_owned(),
                tool_name: "demo.echo".to_owned(),
                arguments: CborValue::Text("hello".to_owned()),
            }),
            Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".to_owned(),
                tool_name: "demo.echo".to_owned(),
                arguments: CborValue::Text("hello".to_owned()),
            }),
            Event::ToolResult(ToolResult {
                call_id: "call-1".to_owned(),
                tool_name: "demo.echo".to_owned(),
                result: CborValue::Text("hello".to_owned()),
            }),
            Event::ToolError(ToolError {
                call_id: "call-1".to_owned(),
                tool_name: "missing.tool".to_owned(),
                message: "no live provider".to_owned(),
                details: None,
            }),
            Event::ToolProgress(ToolProgress {
                call_id: "call-1".to_owned(),
                tool_name: "shell.exec".to_owned(),
                message: Some("running".to_owned()),
                progress: Some(ProgressUpdate {
                    current: Some(1),
                    total: Some(10),
                }),
            }),
            Event::MessageUser(ChatMessage {
                session_id: Some("session-1".to_owned()),
                text: "hello".to_owned(),
            }),
            Event::MessageAgent(ChatMessage {
                session_id: Some("session-1".to_owned()),
                text: "hi there".to_owned(),
            }),
            Event::ExtensionStarting(ExtensionStarting {
                extension_name: "fs".to_owned(),
                argv: vec!["tau-ext-fs".to_owned()],
            }),
            Event::ExtensionReady(ExtensionReady {
                extension_name: "fs".to_owned(),
                connection_id: Some("conn-1".to_owned()),
            }),
            Event::ExtensionExited(ExtensionExited {
                extension_name: "fs".to_owned(),
                exit_code: Some(0),
                signal: None,
            }),
            Event::ExtensionRestarting(ExtensionRestarting {
                extension_name: "fs".to_owned(),
                attempt: 2,
                reason: Some("hot reload".to_owned()),
            }),
            Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: Some("shutdown".to_owned()),
            }),
            Event::AgentPromptRequest(AgentPromptRequest {
                turn_id: "turn-1".to_owned(),
                session_id: "s1".to_owned(),
                system_prompt: "You are helpful.".to_owned(),
                messages: vec![ConversationMessage {
                    role: ConversationRole::User,
                    content: vec![ContentBlock::Text {
                        text: "hello".to_owned(),
                    }],
                }],
                tools: vec![ToolDefinition {
                    name: "fs.read".to_owned(),
                    description: Some("Read a file".to_owned()),
                }],
            }),
            Event::AgentPromptResponse(AgentPromptResponse {
                turn_id: "turn-1".to_owned(),
                session_id: "s1".to_owned(),
                text: Some("Hi there".to_owned()),
                tool_calls: Vec::new(),
            }),
        ]
    }

    #[test]
    fn event_name_round_trips_from_string() {
        for event in representative_events() {
            let name = event.name();
            assert_eq!(name.as_str().parse::<EventName>(), Ok(name));
        }
    }

    #[test]
    fn representative_events_round_trip_through_cbor() {
        for event in representative_events() {
            let encoded = encode_event_to_vec(&event).expect("event should encode");
            let decoded = decode_event_from_slice(&encoded).expect("event should decode");
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn multiple_events_can_share_one_stream() {
        let events = representative_events();
        let mut writer = EventWriter::new(Vec::new());
        for event in &events {
            writer.write_event(event).expect("event should encode");
        }
        writer.flush().expect("stream should flush");

        let bytes = writer.into_inner();
        let mut reader = EventReader::new(std::io::Cursor::new(bytes));
        let mut decoded = Vec::new();
        for _ in 0..events.len() {
            decoded.push(
                reader
                    .read_event()
                    .expect("read should succeed")
                    .expect("event should arrive"),
            );
        }

        assert_eq!(decoded, events);
    }
}
