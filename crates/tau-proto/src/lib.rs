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

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
            pub fn into_string(self) -> String { self.0 }
            pub fn is_empty(&self) -> bool { self.0.is_empty() }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str { &self.0 }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_owned()) }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool { self.0 == other }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool { self.0 == *other }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool { self.0 == *other }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str { &self.0 }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }
    };
}

string_newtype!(/// Session identifier.
    SessionId);
// ToolName is defined manually below with validation.
string_newtype!(/// Tool call identifier.
    ToolCallId);
string_newtype!(/// Connection identifier.
    ConnectionId);
string_newtype!(/// Unique identifier for one prompt within a session.
    SessionPromptId);
string_newtype!(/// Extension name.
    ExtensionName);
string_newtype!(/// Qualified model identifier (e.g. `"openai/gpt-4o"`).
    ModelId);
string_newtype!(/// Provider name (e.g. `"openai"`, `"anthropic"`).
    ProviderName);

// ---------------------------------------------------------------------------
// ToolName (validated newtype)
// ---------------------------------------------------------------------------

/// Tool name: must be non-empty and contain only ASCII alphanumerics or
/// underscores (`[a-zA-Z0-9_]+`).
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    /// Create a new `ToolName`, panicking if the name is invalid.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(Self::is_valid(&s), "invalid tool name: {s:?}");
        Self(s)
    }

    /// Try to create a `ToolName`, returning `None` if invalid.
    pub fn try_new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        Self::is_valid(&s).then(|| Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn into_string(self) -> String {
        self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn is_valid(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }
}

impl Default for ToolName {
    fn default() -> Self {
        // Default is used by serde/ciborium for missing fields; an empty
        // ToolName will fail validation at the boundary where it matters.
        Self(String::new())
    }
}

impl std::ops::Deref for ToolName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ToolName {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for ToolName {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl PartialEq<str> for ToolName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ToolName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for ToolName {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl std::borrow::Borrow<str> for ToolName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if Self::is_valid(&s) {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom(format!(
                "invalid tool name: {s:?}"
            )))
        }
    }
}

/// Unique identifier for one extension instance (monotonic counter).
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ExtensionInstanceId(u64);

impl ExtensionInstanceId {
    pub fn new(v: u64) -> Self {
        Self(v)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ExtensionInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for ExtensionInstanceId {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

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
                    EventSelector::Exact(EventName::UiPromptSubmitted),
                    EventSelector::Prefix("tool.".to_owned()),
                ],
            }),
            Event::LifecycleReady(LifecycleReady {
                message: Some("ready".to_owned()),
            }),
            Event::ToolRegister(ToolRegister {
                tool: ToolSpec {
                    name: "echo".into(),
                    description: Some("Echo a payload".to_owned()),
                    parameters: None,
                    side_effects: ToolSideEffects::Pure,
                },
            }),
            Event::ToolRequest(ToolRequest {
                call_id: "call-1".into(),
                tool_name: "echo".into(),
                arguments: CborValue::Text("hello".to_owned()),
            }),
            Event::ToolInvoke(ToolInvoke {
                call_id: "call-1".into(),
                tool_name: "echo".into(),
                arguments: CborValue::Text("hello".to_owned()),
            }),
            Event::ToolResult(ToolResult {
                call_id: "call-1".into(),
                tool_name: "echo".into(),
                result: CborValue::Text("hello".to_owned()),
            }),
            Event::ToolError(ToolError {
                call_id: "call-1".into(),
                tool_name: "missing_tool".into(),
                message: "no live provider".to_owned(),
                details: None,
            }),
            Event::ToolProgress(ToolProgress {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
                message: Some("running".to_owned()),
                progress: Some(ProgressUpdate {
                    current: Some(1),
                    total: Some(10),
                }),
            }),
            Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: "hello".to_owned(),
            }),
            Event::SessionStarted(SessionStarted {
                session_id: "s1".into(),
            }),
            Event::SessionPromptCreated(SessionPromptCreated {
                session_prompt_id: "sp-1".into(),
                session_id: "s1".into(),
                system_prompt: "You are helpful.".to_owned(),
                messages: vec![ConversationMessage {
                    role: ConversationRole::User,
                    content: vec![ContentBlock::Text {
                        text: "hello".to_owned(),
                    }],
                }],
                tools: vec![ToolDefinition {
                    name: "read".to_owned(),
                    description: Some("Read a file".to_owned()),
                    parameters: None,
                }],
                model: None,
            }),
            Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-1".into(),
                text: Some("Hi there".to_owned()),
                tool_calls: Vec::new(),
            }),
            Event::ExtensionStarting(ExtensionStarting {
                instance_id: 1.into(),
                extension_name: "fs".into(),
                pid: Some(1234),
            }),
            Event::ExtensionReady(ExtensionReady {
                instance_id: 1.into(),
                extension_name: "fs".into(),
                pid: Some(1234),
            }),
            Event::ExtensionExited(ExtensionExited {
                instance_id: 1.into(),
                extension_name: "fs".into(),
                pid: Some(1234),
                exit_code: Some(0),
                signal: None,
            }),
            Event::ExtensionRestarting(ExtensionRestarting {
                instance_id: 1.into(),
                extension_name: "fs".into(),
                pid: Some(1234),
                attempt: 2,
                reason: Some("hot reload".to_owned()),
            }),
            Event::ExtSkillAvailable(ExtSkillAvailable {
                name: "brave-search".to_owned(),
                description: "Web search via Brave API".to_owned(),
                file_path: "/home/user/.agents/skills/brave-search/SKILL.md".to_owned(),
                add_to_prompt: true,
            }),
            Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
                file_path: "/home/user/src/project/AGENTS.md".to_owned(),
                content: "# Project instructions\n- Run tests".to_owned(),
            }),
            Event::ExtensionContextReady(ExtensionContextReady {
                session_id: "s1".into(),
            }),
            Event::LogEvent(LogEvent {
                id: LogEventId::new(42),
                event: Box::new(Event::SessionStarted(SessionStarted {
                    session_id: "s1".into(),
                })),
            }),
            Event::Ack(Ack {
                up_to: LogEventId::new(42),
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

    #[test]
    fn tool_name_accepts_valid_names() {
        assert!(ToolName::try_new("read").is_some());
        assert!(ToolName::try_new("shell").is_some());
        assert!(ToolName::try_new("my_tool_2").is_some());
        assert!(ToolName::try_new("Echo").is_some());
    }

    #[test]
    fn tool_name_rejects_invalid_names() {
        assert!(ToolName::try_new("").is_none());
        assert!(ToolName::try_new("fs.read").is_none());
        assert!(ToolName::try_new("my tool").is_none());
        assert!(ToolName::try_new("a-b").is_none());
        assert!(ToolName::try_new("tool/name").is_none());
    }

    #[test]
    #[should_panic(expected = "invalid tool name")]
    fn tool_name_new_panics_on_invalid() {
        let _ = ToolName::new("bad.name");
    }
}
