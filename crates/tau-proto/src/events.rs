//! Protocol event types and payloads.
//!
//! All event definitions live here so `grep events.rs` finds them.
//!
//! Events are facts — each component broadcasts what happened.
//! There are no requests or responses, only announcements.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{
    CborValue, ExtensionName, ModelId, SessionId, SessionPromptId, ToolCallId, ToolName,
    ToolNameMaybe,
};

// ---------------------------------------------------------------------------
// Event names
// ---------------------------------------------------------------------------

/// Known dotted event names for the protocol surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum EventName {
    // Lifecycle
    #[serde(rename = "lifecycle.hello")]
    LifecycleHello,
    #[serde(rename = "lifecycle.subscribe")]
    LifecycleSubscribe,
    #[serde(rename = "lifecycle.ready")]
    LifecycleReady,
    #[serde(rename = "lifecycle.disconnect")]
    LifecycleDisconnect,

    // Tools
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

    // Extension supervision
    #[serde(rename = "extension.starting")]
    ExtensionStarting,
    #[serde(rename = "extension.ready")]
    ExtensionReady,
    #[serde(rename = "extension.exited")]
    ExtensionExited,
    #[serde(rename = "extension.restarting")]
    ExtensionRestarting,
    #[serde(rename = "extension.skill_available")]
    ExtSkillAvailable,
    #[serde(rename = "extension.agents_md_available")]
    ExtAgentsMdAvailable,
    #[serde(rename = "extension.context_ready")]
    ExtensionContextReady,

    // Harness informational messages
    #[serde(rename = "harness.info")]
    HarnessInfo,
    #[serde(rename = "harness.models_available")]
    HarnessModelsAvailable,
    #[serde(rename = "harness.model_selected")]
    HarnessModelSelected,

    // UI events — facts from the UI
    #[serde(rename = "ui.prompt_submitted")]
    UiPromptSubmitted,
    #[serde(rename = "ui.model_select")]
    UiModelSelect,

    // Session events — facts from the harness session tracker
    #[serde(rename = "session.prompt_queued")]
    SessionPromptQueued,
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "session.prompt_created")]
    SessionPromptCreated,

    // Agent events — facts from the agent backend
    #[serde(rename = "agent.prompt_submitted")]
    AgentPromptSubmitted,
    #[serde(rename = "agent.response_updated")]
    AgentResponseUpdated,
    #[serde(rename = "agent.response_finished")]
    AgentResponseFinished,

    // Wire-level transport (at-least-once delivery)
    #[serde(rename = "wire.log_event")]
    LogEvent,
    #[serde(rename = "wire.ack")]
    Ack,
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
            Self::ExtensionStarting => "extension.starting",
            Self::ExtensionReady => "extension.ready",
            Self::ExtensionExited => "extension.exited",
            Self::ExtensionRestarting => "extension.restarting",
            Self::ExtSkillAvailable => "extension.skill_available",
            Self::ExtAgentsMdAvailable => "extension.agents_md_available",
            Self::ExtensionContextReady => "extension.context_ready",
            Self::HarnessInfo => "harness.info",
            Self::HarnessModelsAvailable => "harness.models_available",
            Self::HarnessModelSelected => "harness.model_selected",
            Self::UiPromptSubmitted => "ui.prompt_submitted",
            Self::UiModelSelect => "ui.model_select",
            Self::SessionPromptQueued => "session.prompt_queued",
            Self::SessionStarted => "session.started",
            Self::SessionPromptCreated => "session.prompt_created",
            Self::AgentPromptSubmitted => "agent.prompt_submitted",
            Self::AgentResponseUpdated => "agent.response_updated",
            Self::AgentResponseFinished => "agent.response_finished",
            Self::LogEvent => "wire.log_event",
            Self::Ack => "wire.ack",
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
            "extension.starting" => Ok(Self::ExtensionStarting),
            "extension.ready" => Ok(Self::ExtensionReady),
            "extension.exited" => Ok(Self::ExtensionExited),
            "extension.restarting" => Ok(Self::ExtensionRestarting),
            "extension.skill_available" => Ok(Self::ExtSkillAvailable),
            "extension.agents_md_available" => Ok(Self::ExtAgentsMdAvailable),
            "extension.context_ready" => Ok(Self::ExtensionContextReady),
            "harness.info" => Ok(Self::HarnessInfo),
            "harness.models_available" => Ok(Self::HarnessModelsAvailable),
            "harness.model_selected" => Ok(Self::HarnessModelSelected),
            "ui.prompt_submitted" => Ok(Self::UiPromptSubmitted),
            "ui.model_select" => Ok(Self::UiModelSelect),
            "session.prompt_queued" => Ok(Self::SessionPromptQueued),
            "session.started" => Ok(Self::SessionStarted),
            "session.prompt_created" => Ok(Self::SessionPromptCreated),
            "agent.prompt_submitted" => Ok(Self::AgentPromptSubmitted),
            "agent.response_updated" => Ok(Self::AgentResponseUpdated),
            "agent.response_finished" => Ok(Self::AgentResponseFinished),
            "wire.log_event" => Ok(Self::LogEvent),
            "wire.ack" => Ok(Self::Ack),
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
// Harness informational messages
// ---------------------------------------------------------------------------

/// An informational message from the harness displayed to the user.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub message: String,
}

/// The harness announces all available models as `provider/model` strings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessModelsAvailable {
    /// Each entry is `"provider_name/model_id"`.
    pub models: Vec<ModelId>,
}

/// The harness announces which model is currently selected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessModelSelected {
    /// `"provider_name/model_id"`, or empty if none.
    pub model: ModelId,
}

// ---------------------------------------------------------------------------
// Tool events
// ---------------------------------------------------------------------------

/// Tool metadata used during registration and invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// Side-effect class used by the harness dispatch state machine to
    /// serialize mutating calls with respect to pure ones. Unknown /
    /// unset declarations default to `Mutating` so extensions that
    /// haven't been updated don't silently lose ordering.
    #[serde(default)]
    pub side_effects: ToolSideEffects,
}

/// Whether a tool observably mutates state. Purely informational for
/// the agent; enforced by the harness's tool dispatch queue.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSideEffects {
    /// Read-only / commutative with other tool calls. Multiple `Pure`
    /// calls can run concurrently and can be interleaved freely.
    Pure,
    /// May mutate externally observable state (filesystem, network,
    /// processes, shared session data, …). Serialized against every
    /// other in-flight tool call — the next tool does not dispatch
    /// until this one's result has been received. Default so that
    /// tools which don't explicitly opt in to `Pure` are treated
    /// conservatively.
    #[default]
    Mutating,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRegister {
    pub tool: ToolSpec,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolUnregister {
    pub tool_name: ToolName,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: CborValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInvoke {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: CborValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub result: CborValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProgressUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolProgress {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressUpdate>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancel {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancelled {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
}

// ---------------------------------------------------------------------------
// Extension supervision events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionStarting {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionReady {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionExited {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionRestarting {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// An extension discovered a skill and is advertising it to the harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtSkillAvailable {
    pub name: String,
    pub description: String,
    /// Absolute path to the skill file so the harness can read it.
    pub file_path: String,
    /// When true the harness should include this skill in the system prompt.
    pub add_to_prompt: bool,
}

/// An extension discovered one AGENTS.md file and is advertising it to the
/// harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentsMdAvailable {
    /// Absolute path to the AGENTS.md file.
    pub file_path: String,
    /// Full file contents, sent eagerly so the harness can inject them
    /// without an extra tool round trip.
    pub content: String,
}

/// An extension finished broadcasting refreshed prompt context for one session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionContextReady {
    pub session_id: SessionId,
}

// ---------------------------------------------------------------------------
// Wire transport — at-least-once delivery for event-log entries
// ---------------------------------------------------------------------------

/// Monotonic id assigned by the harness when an event is appended to its
/// event log. Receivers acknowledge processing by returning the same id
/// in [`Ack::up_to`].
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct LogEventId(pub u64);

impl LogEventId {
    #[must_use]
    pub fn new(v: u64) -> Self {
        Self(v)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LogEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A bus event delivered through the harness's event log. Receivers
/// must process the inner event and then send an [`Ack`] referencing
/// `id` (or any later id, since acks are cumulative).
///
/// `event` is boxed because `Event` is recursive through this variant.
/// It is never another `LogEvent` or `Ack` — only "real" payload
/// events (e.g., `SessionStarted`, `ExtensionReady`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEvent {
    pub id: LogEventId,
    pub event: Box<Event>,
}

/// Receiver → sender acknowledgement that all log events with id
/// `<= up_to` have been processed. Cumulative — newer acks supersede
/// older ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub up_to: LogEventId,
}

// ---------------------------------------------------------------------------
// UI events — facts from the user interface
// ---------------------------------------------------------------------------

/// The user submitted a prompt in the UI.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptSubmitted {
    pub session_id: SessionId,
    pub text: String,
}

/// The user requests switching to a different model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiModelSelect {
    /// `"provider_name/model_id"`.
    pub model: ModelId,
}

// ---------------------------------------------------------------------------
// Session events — facts from the harness session tracker
// ---------------------------------------------------------------------------

/// The harness queued a user prompt because the agent is busy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptQueued {
    pub session_id: SessionId,
    pub text: String,
}

/// The harness created a new session. Extensions that subscribe react by
/// performing per-session setup (e.g. discovering AGENTS.md) and signal
/// completion with `ExtensionContextReady`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub session_id: SessionId,
}

/// The harness persisted a user prompt and assigned it an ID.
/// Also carries the assembled conversation context for the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptCreated {
    pub session_prompt_id: SessionPromptId,
    pub session_id: SessionId,
    pub system_prompt: String,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolDefinition>,
    /// Currently selected model as `"provider/model_id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
}

// ---------------------------------------------------------------------------
// Agent events — facts from the agent backend
// ---------------------------------------------------------------------------

/// The agent accepted a prompt and began processing it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptSubmitted {
    pub session_prompt_id: SessionPromptId,
}

/// The agent has new accumulated response text for a prompt.
/// Each update carries the full text so far (replace, not delta).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentResponseUpdated {
    pub session_prompt_id: SessionPromptId,
    pub text: String,
}

/// One tool call the agent wants to make.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentToolCall {
    pub id: String,
    /// Model-produced name. Kept as [`ToolNameMaybe`] so that a
    /// single hallucinated / malformed name doesn't fail decode of
    /// the entire batch; the harness matches on the variant at
    /// dispatch time and rejects `Invalid` with a synthetic
    /// `ToolError` while letting sibling calls run.
    pub name: ToolNameMaybe,
    pub arguments: CborValue,
}

/// The agent finished processing a prompt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentResponseFinished {
    pub session_prompt_id: SessionPromptId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AgentToolCall>,
}

// ---------------------------------------------------------------------------
// Conversation types (used in SessionPromptCreated)
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
        /// Same untrusted-LLM-output contract as
        /// `AgentToolCall::name`. See [`ToolNameMaybe`].
        name: ToolNameMaybe,
        input: CborValue,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// One message in the conversation history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub content: Vec<ContentBlock>,
}

/// A tool definition available for the agent to use.
///
/// This is outbound (harness → LLM in the prompt), so the harness
/// controls the string and we enforce the `ToolName` invariant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Top-level event envelope
// ---------------------------------------------------------------------------

/// Top-level event envelope used on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
pub enum Event {
    // Lifecycle
    #[serde(rename = "lifecycle.hello")]
    LifecycleHello(LifecycleHello),
    #[serde(rename = "lifecycle.subscribe")]
    LifecycleSubscribe(LifecycleSubscribe),
    #[serde(rename = "lifecycle.ready")]
    LifecycleReady(LifecycleReady),
    #[serde(rename = "lifecycle.disconnect")]
    LifecycleDisconnect(LifecycleDisconnect),

    // Tools
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

    // Extension supervision
    #[serde(rename = "extension.starting")]
    ExtensionStarting(ExtensionStarting),
    #[serde(rename = "extension.ready")]
    ExtensionReady(ExtensionReady),
    #[serde(rename = "extension.exited")]
    ExtensionExited(ExtensionExited),
    #[serde(rename = "extension.restarting")]
    ExtensionRestarting(ExtensionRestarting),
    #[serde(rename = "extension.skill_available")]
    ExtSkillAvailable(ExtSkillAvailable),
    #[serde(rename = "extension.agents_md_available")]
    ExtAgentsMdAvailable(ExtAgentsMdAvailable),
    #[serde(rename = "extension.context_ready")]
    ExtensionContextReady(ExtensionContextReady),

    // Harness info
    #[serde(rename = "harness.info")]
    HarnessInfo(HarnessInfo),
    #[serde(rename = "harness.models_available")]
    HarnessModelsAvailable(HarnessModelsAvailable),
    #[serde(rename = "harness.model_selected")]
    HarnessModelSelected(HarnessModelSelected),

    // UI
    #[serde(rename = "ui.prompt_submitted")]
    UiPromptSubmitted(UiPromptSubmitted),
    #[serde(rename = "ui.model_select")]
    UiModelSelect(UiModelSelect),

    // Session
    #[serde(rename = "session.prompt_queued")]
    SessionPromptQueued(SessionPromptQueued),
    #[serde(rename = "session.started")]
    SessionStarted(SessionStarted),
    #[serde(rename = "session.prompt_created")]
    SessionPromptCreated(SessionPromptCreated),

    // Agent
    #[serde(rename = "agent.prompt_submitted")]
    AgentPromptSubmitted(AgentPromptSubmitted),
    #[serde(rename = "agent.response_updated")]
    AgentResponseUpdated(AgentResponseUpdated),
    #[serde(rename = "agent.response_finished")]
    AgentResponseFinished(AgentResponseFinished),

    // Wire transport
    #[serde(rename = "wire.log_event")]
    LogEvent(LogEvent),
    #[serde(rename = "wire.ack")]
    Ack(Ack),
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
            Self::ExtensionStarting(_) => EventName::ExtensionStarting,
            Self::ExtensionReady(_) => EventName::ExtensionReady,
            Self::ExtensionExited(_) => EventName::ExtensionExited,
            Self::ExtensionRestarting(_) => EventName::ExtensionRestarting,
            Self::ExtSkillAvailable(_) => EventName::ExtSkillAvailable,
            Self::ExtAgentsMdAvailable(_) => EventName::ExtAgentsMdAvailable,
            Self::ExtensionContextReady(_) => EventName::ExtensionContextReady,
            Self::HarnessInfo(_) => EventName::HarnessInfo,
            Self::HarnessModelsAvailable(_) => EventName::HarnessModelsAvailable,
            Self::HarnessModelSelected(_) => EventName::HarnessModelSelected,
            Self::UiPromptSubmitted(_) => EventName::UiPromptSubmitted,
            Self::UiModelSelect(_) => EventName::UiModelSelect,
            Self::SessionPromptQueued(_) => EventName::SessionPromptQueued,
            Self::SessionStarted(_) => EventName::SessionStarted,
            Self::SessionPromptCreated(_) => EventName::SessionPromptCreated,
            Self::AgentPromptSubmitted(_) => EventName::AgentPromptSubmitted,
            Self::AgentResponseUpdated(_) => EventName::AgentResponseUpdated,
            Self::AgentResponseFinished(_) => EventName::AgentResponseFinished,
            Self::LogEvent(_) => EventName::LogEvent,
            Self::Ack(_) => EventName::Ack,
        }
    }

    /// Peels off a `LogEvent` envelope, returning `(Some(id), inner)`
    /// for log-delivered events and `(None, self)` for direct ones.
    /// Receivers that want at-least-once semantics ack the returned id
    /// after processing the inner event.
    #[must_use]
    pub fn peel_log(self) -> (Option<LogEventId>, Self) {
        match self {
            Self::LogEvent(env) => (Some(env.id), *env.event),
            other => (None, other),
        }
    }
}
