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

use std::fmt;
use std::io::{Cursor, Read, Write};
use std::str::FromStr;

pub use ciborium::value::Value as CborValue;
use serde::{Deserialize, Serialize};

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

/// Known dotted event names for the initial protocol surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum EventName {
    #[serde(rename = "lifecycle.hello")]
    LifecycleHello,
    #[serde(rename = "lifecycle.subscribe")]
    LifecycleSubscribe,
    #[serde(rename = "lifecycle.ready")]
    LifecycleReady,
    #[serde(rename = "lifecycle.disconnect")]
    LifecycleDisconnect,
    #[serde(rename = "tool.register")]
    ToolRegister,
    #[serde(rename = "tool.unregister")]
    ToolUnregister,
    #[serde(rename = "tool.request")]
    ToolRequest,
    #[serde(rename = "tool.invoke")]
    ToolInvoke,
    #[serde(rename = "tool.result")]
    ToolResult,
    #[serde(rename = "tool.error")]
    ToolError,
    #[serde(rename = "tool.progress")]
    ToolProgress,
    #[serde(rename = "tool.cancel")]
    ToolCancel,
    #[serde(rename = "tool.cancelled")]
    ToolCancelled,
    #[serde(rename = "message.user")]
    MessageUser,
    #[serde(rename = "message.agent")]
    MessageAgent,
    #[serde(rename = "extension.starting")]
    ExtensionStarting,
    #[serde(rename = "extension.ready")]
    ExtensionReady,
    #[serde(rename = "extension.exited")]
    ExtensionExited,
    #[serde(rename = "extension.restarting")]
    ExtensionRestarting,
}

impl EventName {
    /// Returns the dotted wire-format name for this event.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LifecycleHello => "lifecycle.hello",
            Self::LifecycleSubscribe => "lifecycle.subscribe",
            Self::LifecycleReady => "lifecycle.ready",
            Self::LifecycleDisconnect => "lifecycle.disconnect",
            Self::ToolRegister => "tool.register",
            Self::ToolUnregister => "tool.unregister",
            Self::ToolRequest => "tool.request",
            Self::ToolInvoke => "tool.invoke",
            Self::ToolResult => "tool.result",
            Self::ToolError => "tool.error",
            Self::ToolProgress => "tool.progress",
            Self::ToolCancel => "tool.cancel",
            Self::ToolCancelled => "tool.cancelled",
            Self::MessageUser => "message.user",
            Self::MessageAgent => "message.agent",
            Self::ExtensionStarting => "extension.starting",
            Self::ExtensionReady => "extension.ready",
            Self::ExtensionExited => "extension.exited",
            Self::ExtensionRestarting => "extension.restarting",
        }
    }
}

impl fmt::Display for EventName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventName {
    type Err = ParseEventNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "lifecycle.hello" => Ok(Self::LifecycleHello),
            "lifecycle.subscribe" => Ok(Self::LifecycleSubscribe),
            "lifecycle.ready" => Ok(Self::LifecycleReady),
            "lifecycle.disconnect" => Ok(Self::LifecycleDisconnect),
            "tool.register" => Ok(Self::ToolRegister),
            "tool.unregister" => Ok(Self::ToolUnregister),
            "tool.request" => Ok(Self::ToolRequest),
            "tool.invoke" => Ok(Self::ToolInvoke),
            "tool.result" => Ok(Self::ToolResult),
            "tool.error" => Ok(Self::ToolError),
            "tool.progress" => Ok(Self::ToolProgress),
            "tool.cancel" => Ok(Self::ToolCancel),
            "tool.cancelled" => Ok(Self::ToolCancelled),
            "message.user" => Ok(Self::MessageUser),
            "message.agent" => Ok(Self::MessageAgent),
            "extension.starting" => Ok(Self::ExtensionStarting),
            "extension.ready" => Ok(Self::ExtensionReady),
            "extension.exited" => Ok(Self::ExtensionExited),
            "extension.restarting" => Ok(Self::ExtensionRestarting),
            _ => Err(ParseEventNameError {
                invalid_name: value.to_owned(),
            }),
        }
    }
}

/// Error returned when parsing an unknown event name string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseEventNameError {
    invalid_name: String,
}

impl ParseEventNameError {
    /// Returns the unknown event name that failed to parse.
    #[must_use]
    pub fn invalid_name(&self) -> &str {
        &self.invalid_name
    }
}

impl fmt::Display for ParseEventNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown event name: {}", self.invalid_name)
    }
}

impl std::error::Error for ParseEventNameError {}

/// The type of participant speaking the protocol.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Agent,
    Tool,
    Ui,
    Core,
    External,
}

/// A subscription selector used by `lifecycle.subscribe`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum EventSelector {
    Exact(EventName),
    Prefix(String),
}

/// Announcement sent by a participant after connecting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LifecycleHello {
    pub protocol_version: u32,
    pub client_name: String,
    pub client_kind: ClientKind,
}

/// Subscription request describing which events a participant wants to receive.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LifecycleSubscribe {
    pub selectors: Vec<EventSelector>,
}

/// Readiness notification emitted after startup or handshake completes.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LifecycleReady {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Disconnect notification with an optional human-readable reason.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LifecycleDisconnect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Tool metadata used during registration and invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Registers one live tool provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolRegister {
    pub tool: ToolSpec,
}

/// Removes one live tool provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolUnregister {
    pub tool_name: ToolName,
}

/// Requests a tool invocation from the harness.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: CborValue,
}

/// Directs a selected provider to run one tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInvoke {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: CborValue,
}

/// Reports a successful tool result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub result: CborValue,
}

/// Reports a failed tool result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
}

/// Reports progress from an in-flight tool invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProgressUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Emits progress for an in-flight tool invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolProgress {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressUpdate>,
}

/// Requests cancellation of a running tool invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancel {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
}

/// Reports that a running tool invocation was cancelled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancelled {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
}

/// Common message payload used by user and agent messages.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub text: String,
}

/// Supervision event emitted before the harness starts an extension.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionStarting {
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
}

/// Supervision event emitted when an extension becomes ready.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionReady {
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<ConnectionId>,
}

/// Supervision event emitted when an extension exits.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionExited {
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

/// Supervision event emitted before the harness retries an extension.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionRestarting {
    pub extension_name: ExtensionName,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Top-level event envelope used on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
pub enum Event {
    #[serde(rename = "lifecycle.hello")]
    LifecycleHello(LifecycleHello),
    #[serde(rename = "lifecycle.subscribe")]
    LifecycleSubscribe(LifecycleSubscribe),
    #[serde(rename = "lifecycle.ready")]
    LifecycleReady(LifecycleReady),
    #[serde(rename = "lifecycle.disconnect")]
    LifecycleDisconnect(LifecycleDisconnect),
    #[serde(rename = "tool.register")]
    ToolRegister(ToolRegister),
    #[serde(rename = "tool.unregister")]
    ToolUnregister(ToolUnregister),
    #[serde(rename = "tool.request")]
    ToolRequest(ToolRequest),
    #[serde(rename = "tool.invoke")]
    ToolInvoke(ToolInvoke),
    #[serde(rename = "tool.result")]
    ToolResult(ToolResult),
    #[serde(rename = "tool.error")]
    ToolError(ToolError),
    #[serde(rename = "tool.progress")]
    ToolProgress(ToolProgress),
    #[serde(rename = "tool.cancel")]
    ToolCancel(ToolCancel),
    #[serde(rename = "tool.cancelled")]
    ToolCancelled(ToolCancelled),
    #[serde(rename = "message.user")]
    MessageUser(ChatMessage),
    #[serde(rename = "message.agent")]
    MessageAgent(ChatMessage),
    #[serde(rename = "extension.starting")]
    ExtensionStarting(ExtensionStarting),
    #[serde(rename = "extension.ready")]
    ExtensionReady(ExtensionReady),
    #[serde(rename = "extension.exited")]
    ExtensionExited(ExtensionExited),
    #[serde(rename = "extension.restarting")]
    ExtensionRestarting(ExtensionRestarting),
}

impl Event {
    /// Returns the dotted event name carried by this envelope.
    #[must_use]
    pub const fn name(&self) -> EventName {
        match self {
            Self::LifecycleHello(_) => EventName::LifecycleHello,
            Self::LifecycleSubscribe(_) => EventName::LifecycleSubscribe,
            Self::LifecycleReady(_) => EventName::LifecycleReady,
            Self::LifecycleDisconnect(_) => EventName::LifecycleDisconnect,
            Self::ToolRegister(_) => EventName::ToolRegister,
            Self::ToolUnregister(_) => EventName::ToolUnregister,
            Self::ToolRequest(_) => EventName::ToolRequest,
            Self::ToolInvoke(_) => EventName::ToolInvoke,
            Self::ToolResult(_) => EventName::ToolResult,
            Self::ToolError(_) => EventName::ToolError,
            Self::ToolProgress(_) => EventName::ToolProgress,
            Self::ToolCancel(_) => EventName::ToolCancel,
            Self::ToolCancelled(_) => EventName::ToolCancelled,
            Self::MessageUser(_) => EventName::MessageUser,
            Self::MessageAgent(_) => EventName::MessageAgent,
            Self::ExtensionStarting(_) => EventName::ExtensionStarting,
            Self::ExtensionReady(_) => EventName::ExtensionReady,
            Self::ExtensionExited(_) => EventName::ExtensionExited,
            Self::ExtensionRestarting(_) => EventName::ExtensionRestarting,
        }
    }
}

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
    pub fn read_event(&mut self) -> Result<Event, DecodeError> {
        decode_event(&mut self.inner)
    }
}

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
            Event::ToolRegister(ToolRegister {
                tool: ToolSpec {
                    name: "fs.read".to_owned(),
                    description: Some("Read a file from disk".to_owned()),
                },
            }),
            Event::ToolRequest(ToolRequest {
                call_id: "call-1".to_owned(),
                tool_name: "fs.read".to_owned(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("README.md".to_owned()),
                )]),
            }),
            Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".to_owned(),
                tool_name: "fs.read".to_owned(),
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("README.md".to_owned()),
                )]),
            }),
            Event::ToolResult(ToolResult {
                call_id: "call-1".to_owned(),
                tool_name: "fs.read".to_owned(),
                result: CborValue::Text("file contents".to_owned()),
            }),
            Event::ToolError(ToolError {
                call_id: "call-2".to_owned(),
                tool_name: "fs.read".to_owned(),
                message: "file not found".to_owned(),
                details: Some(CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("missing.txt".to_owned()),
                )])),
            }),
            Event::ToolProgress(ToolProgress {
                call_id: "call-3".to_owned(),
                tool_name: "shell.exec".to_owned(),
                message: Some("running".to_owned()),
                progress: Some(ProgressUpdate {
                    current: Some(1),
                    total: Some(3),
                }),
            }),
            Event::ToolCancelled(ToolCancelled {
                call_id: "call-4".to_owned(),
                tool_name: "shell.exec".to_owned(),
            }),
            Event::MessageUser(ChatMessage {
                session_id: Some("session-1".to_owned()),
                text: "Read README.md".to_owned(),
            }),
            Event::MessageAgent(ChatMessage {
                session_id: Some("session-1".to_owned()),
                text: "Here is the file.".to_owned(),
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
        let mut reader = EventReader::new(Cursor::new(bytes));
        let mut decoded = Vec::new();
        for _ in 0..events.len() {
            decoded.push(reader.read_event().expect("event should decode"));
        }

        assert_eq!(decoded, events);
    }
}
