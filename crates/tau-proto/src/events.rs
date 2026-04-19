//! Protocol event types and payloads.
//!
//! All event definitions live here so `grep events.rs` finds them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{CborValue, ConnectionId, ExtensionName, SessionId, ToolCallId, ToolName};

// ---------------------------------------------------------------------------
// Event names
// ---------------------------------------------------------------------------

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
    #[serde(rename = "agent.prompt_request")]
    AgentPromptRequest,
    #[serde(rename = "agent.prompt_response")]
    AgentPromptResponse,
    #[serde(rename = "agent.response_start")]
    AgentResponseStart,
    #[serde(rename = "agent.response_update")]
    AgentResponseUpdate,
    #[serde(rename = "agent.response_end")]
    AgentResponseEnd,
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
            Self::AgentPromptRequest => "agent.prompt_request",
            Self::AgentPromptResponse => "agent.prompt_response",
            Self::AgentResponseStart => "agent.response_start",
            Self::AgentResponseUpdate => "agent.response_update",
            Self::AgentResponseEnd => "agent.response_end",
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
            "agent.prompt_request" => Ok(Self::AgentPromptRequest),
            "agent.prompt_response" => Ok(Self::AgentPromptResponse),
            "agent.response_start" => Ok(Self::AgentResponseStart),
            "agent.response_update" => Ok(Self::AgentResponseUpdate),
            "agent.response_end" => Ok(Self::AgentResponseEnd),
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

// ---------------------------------------------------------------------------
// Lifecycle events
// ---------------------------------------------------------------------------

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

/// Subscription request describing which events a participant wants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LifecycleSubscribe {
    pub selectors: Vec<EventSelector>,
}

/// Readiness notification emitted after startup or handshake.
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

// ---------------------------------------------------------------------------
// Tool events
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Chat messages
// ---------------------------------------------------------------------------

/// Common message payload used by user and agent messages.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Agent prompt protocol
// ---------------------------------------------------------------------------

/// Role of a participant in the conversation history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
}

/// One block of content within a conversation message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: CborValue,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// One message in the conversation history sent to the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub content: Vec<ContentBlock>,
}

/// A tool definition available for the agent to use.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Fully assembled prompt sent by the harness to the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptRequest {
    pub turn_id: String,
    pub session_id: String,
    pub system_prompt: String,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolDefinition>,
}

/// One tool call requested by the agent in its response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentToolCall {
    pub id: String,
    pub name: String,
    pub arguments: CborValue,
}

/// Agent's response to a prompt request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptResponse {
    pub turn_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AgentToolCall>,
}

// ---------------------------------------------------------------------------
// Agent streaming events
// ---------------------------------------------------------------------------

/// Signals the start of a streaming agent response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentResponseStart {
    pub turn_id: String,
    pub session_id: String,
}

/// Carries the full accumulated text so far during streaming.
/// Each update replaces the previous content (not a delta).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentResponseUpdate {
    pub turn_id: String,
    pub session_id: String,
    pub text: String,
}

/// Signals the end of a streaming response. Carries the final
/// complete response including any tool calls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentResponseEnd {
    pub turn_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AgentToolCall>,
}

// ---------------------------------------------------------------------------
// Extension supervision events
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Top-level event envelope
// ---------------------------------------------------------------------------

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
    #[serde(rename = "agent.prompt_request")]
    AgentPromptRequest(AgentPromptRequest),
    #[serde(rename = "agent.prompt_response")]
    AgentPromptResponse(AgentPromptResponse),
    #[serde(rename = "agent.response_start")]
    AgentResponseStart(AgentResponseStart),
    #[serde(rename = "agent.response_update")]
    AgentResponseUpdate(AgentResponseUpdate),
    #[serde(rename = "agent.response_end")]
    AgentResponseEnd(AgentResponseEnd),
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
            Self::AgentPromptRequest(_) => EventName::AgentPromptRequest,
            Self::AgentPromptResponse(_) => EventName::AgentPromptResponse,
            Self::AgentResponseStart(_) => EventName::AgentResponseStart,
            Self::AgentResponseUpdate(_) => EventName::AgentResponseUpdate,
            Self::AgentResponseEnd(_) => EventName::AgentResponseEnd,
        }
    }
}
