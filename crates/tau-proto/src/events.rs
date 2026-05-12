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
    AgentTokenUsage, CborValue, ExtensionName, ModelId, SessionId, SessionPromptId, SkillName,
    ToolCallId, ToolName, ToolNameMaybe,
};

// ---------------------------------------------------------------------------
// Event names
// ---------------------------------------------------------------------------

/// First segment of a dotted event name.
///
/// The well-known categories are enumerated so that subscription
/// policies, routing, and other category-level logic can branch on a
/// closed set. Unknown categories — e.g. from a future extension that
/// invents its own family — round-trip through [`EventCategory::Other`]
/// without losing fidelity.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum EventCategory {
    Tool,
    Extension,
    Harness,
    Ui,
    Shell,
    Session,
    Agent,
    /// Terminal-output side effects directed at the UI: escape
    /// sequences the UI should write straight through to its
    /// terminal (notifications, OSC user-vars, etc.).
    Term,
    /// Any category we don't recognize, kept verbatim.
    Other(String),
}

impl EventCategory {
    /// The wire string for this category (the part before the first
    /// dot in the dotted name).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Tool => "tool",
            Self::Extension => "extension",
            Self::Harness => "harness",
            Self::Ui => "ui",
            Self::Shell => "shell",
            Self::Session => "session",
            Self::Agent => "agent",
            Self::Term => "term",
            Self::Other(s) => s.as_str(),
        }
    }

    /// Parse a category string. Always succeeds; unknown strings
    /// become [`EventCategory::Other`].
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            "tool" => Self::Tool,
            "extension" => Self::Extension,
            "harness" => Self::Harness,
            "ui" => Self::Ui,
            "shell" => Self::Shell,
            "session" => Self::Session,
            "agent" => Self::Agent,
            "term" => Self::Term,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl fmt::Display for EventCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Second segment of a dotted event name.
///
/// Open-ended: the wire format permits arbitrary identifiers after
/// the first dot, so this is just a string newtype rather than a
/// closed enum. Borrowed `&'static str` for the well-known constants
/// declared on [`EventName`]; owned `String` for anything decoded
/// from the wire or constructed at runtime.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct EventCall(std::borrow::Cow<'static, str>);

impl EventCall {
    pub const fn from_static(s: &'static str) -> Self {
        Self(std::borrow::Cow::Borrowed(s))
    }

    pub fn new(s: impl Into<String>) -> Self {
        Self(std::borrow::Cow::Owned(s.into()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&'static str> for EventCall {
    fn from(s: &'static str) -> Self {
        Self::from_static(s)
    }
}

impl From<String> for EventCall {
    fn from(s: String) -> Self {
        Self(std::borrow::Cow::Owned(s))
    }
}

/// A dotted event name, split into a category and a call.
///
/// Wire format is `"<category>.<call>"`; serde and `Display` use that
/// form. The well-known protocol events are exposed as `pub const`
/// values directly on this type (`EventName::TOOL_REGISTER`, etc.) so
/// match-arm-style call sites keep their compactness while gaining
/// a typed `category` to branch on.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct EventName {
    pub category: EventCategory,
    pub call: EventCall,
}

impl EventName {
    pub const fn from_static(category: EventCategory, call: &'static str) -> Self {
        Self {
            category,
            call: EventCall::from_static(call),
        }
    }

    pub fn new(category: EventCategory, call: impl Into<EventCall>) -> Self {
        Self {
            category,
            call: call.into(),
        }
    }

    /// True iff the dotted form `"<category>.<call>"` starts with
    /// `prefix`. Avoids allocating; handles category-only and
    /// across-the-dot prefixes correctly.
    #[must_use]
    pub fn matches_prefix(&self, prefix: &str) -> bool {
        let cat = self.category.as_str();
        if prefix.len() <= cat.len() {
            return cat.starts_with(prefix);
        }
        // prefix extends past the category — it must include the dot.
        if !prefix.starts_with(cat) {
            return false;
        }
        match prefix.as_bytes().get(cat.len()) {
            Some(&b'.') => self.call.as_str().starts_with(&prefix[cat.len() + 1..]),
            _ => false,
        }
    }

    // -- Well-known event names ----------------------------------------

    pub const TOOL_REGISTER: Self = Self::from_static(EventCategory::Tool, "register");
    pub const TOOL_UNREGISTER: Self = Self::from_static(EventCategory::Tool, "unregister");
    pub const TOOL_REQUEST: Self = Self::from_static(EventCategory::Tool, "request");
    pub const TOOL_INVOKE: Self = Self::from_static(EventCategory::Tool, "invoke");
    pub const TOOL_RESULT: Self = Self::from_static(EventCategory::Tool, "result");
    pub const TOOL_ERROR: Self = Self::from_static(EventCategory::Tool, "error");
    pub const TOOL_PROGRESS: Self = Self::from_static(EventCategory::Tool, "progress");
    pub const TOOL_CANCEL: Self = Self::from_static(EventCategory::Tool, "cancel");
    pub const TOOL_CANCELLED: Self = Self::from_static(EventCategory::Tool, "cancelled");
    pub const TOOL_DELEGATE_PROGRESS: Self =
        Self::from_static(EventCategory::Tool, "delegate_progress");

    pub const EXTENSION_STARTING: Self = Self::from_static(EventCategory::Extension, "starting");
    pub const EXTENSION_READY: Self = Self::from_static(EventCategory::Extension, "ready");
    pub const EXTENSION_EXITED: Self = Self::from_static(EventCategory::Extension, "exited");
    pub const EXTENSION_RESTARTING: Self =
        Self::from_static(EventCategory::Extension, "restarting");
    pub const EXTENSION_SKILL_AVAILABLE: Self =
        Self::from_static(EventCategory::Extension, "skill_available");
    pub const EXTENSION_AGENTS_MD_AVAILABLE: Self =
        Self::from_static(EventCategory::Extension, "agents_md_available");
    pub const EXTENSION_CONTEXT_READY: Self =
        Self::from_static(EventCategory::Extension, "context_ready");
    pub const EXTENSION_AGENT_QUERY: Self =
        Self::from_static(EventCategory::Extension, "agent_query");
    pub const EXTENSION_AGENT_QUERY_RESULT: Self =
        Self::from_static(EventCategory::Extension, "agent_query_result");

    pub const HARNESS_INFO: Self = Self::from_static(EventCategory::Harness, "info");
    pub const HARNESS_SESSION_DIR: Self = Self::from_static(EventCategory::Harness, "session_dir");
    pub const HARNESS_UI_DIR: Self = Self::from_static(EventCategory::Harness, "ui_dir");
    pub const HARNESS_MODELS_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "models_available");
    pub const HARNESS_MODEL_SELECTED: Self =
        Self::from_static(EventCategory::Harness, "model_selected");
    pub const HARNESS_CONTEXT_USAGE_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "context_usage_changed");
    pub const HARNESS_EFFORT_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "effort_changed");
    pub const HARNESS_EFFORTS_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "efforts_available");

    pub const UI_PROMPT_SUBMITTED: Self = Self::from_static(EventCategory::Ui, "prompt_submitted");
    pub const UI_MODEL_SELECT: Self = Self::from_static(EventCategory::Ui, "model_select");
    pub const UI_SET_EFFORT: Self = Self::from_static(EventCategory::Ui, "set_effort");
    pub const UI_DETACH_REQUEST: Self = Self::from_static(EventCategory::Ui, "detach_request");
    pub const UI_SHELL_COMMAND: Self = Self::from_static(EventCategory::Ui, "shell_command");
    pub const UI_SWITCH_SESSION: Self = Self::from_static(EventCategory::Ui, "switch_session");
    pub const UI_TREE_REQUEST: Self = Self::from_static(EventCategory::Ui, "tree_request");
    pub const UI_NAVIGATE_TREE: Self = Self::from_static(EventCategory::Ui, "navigate_tree");
    pub const UI_PROMPT_DRAFT: Self = Self::from_static(EventCategory::Ui, "prompt_draft");
    pub const UI_CANCEL_PROMPT: Self = Self::from_static(EventCategory::Ui, "cancel_prompt");

    pub const TERM_OSC1337_SET_USER_VAR: Self =
        Self::from_static(EventCategory::Term, "osc1337_set_user_var");

    pub const SHELL_COMMAND_PROGRESS: Self =
        Self::from_static(EventCategory::Shell, "command_progress");
    pub const SHELL_COMMAND_FINISHED: Self =
        Self::from_static(EventCategory::Shell, "command_finished");

    pub const SESSION_PROMPT_QUEUED: Self =
        Self::from_static(EventCategory::Session, "prompt_queued");
    pub const SESSION_PROMPT_STEERED: Self =
        Self::from_static(EventCategory::Session, "prompt_steered");
    pub const SESSION_STARTED: Self = Self::from_static(EventCategory::Session, "started");
    pub const SESSION_SHUTDOWN: Self = Self::from_static(EventCategory::Session, "shutdown");
    pub const SESSION_PROMPT_CREATED: Self =
        Self::from_static(EventCategory::Session, "prompt_created");
    pub const SESSION_USER_MESSAGE_INJECTED: Self =
        Self::from_static(EventCategory::Session, "user_message_injected");

    pub const AGENT_PROMPT_SUBMITTED: Self =
        Self::from_static(EventCategory::Agent, "prompt_submitted");
    pub const AGENT_RESPONSE_UPDATED: Self =
        Self::from_static(EventCategory::Agent, "response_updated");
    pub const AGENT_RESPONSE_FINISHED: Self =
        Self::from_static(EventCategory::Agent, "response_finished");
}

impl fmt::Display for EventName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.category, self.call)
    }
}

impl FromStr for EventName {
    type Err = ParseEventNameError;

    /// Always succeeds for well-formed `"a.b"` input. Unknown
    /// categories survive as [`EventCategory::Other`]; unknown
    /// `call` segments survive as owned strings. Errors only on
    /// missing-dot input.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((cat, call)) = value.split_once('.') else {
            return Err(ParseEventNameError {
                invalid_name: value.to_owned(),
            });
        };
        Ok(Self {
            category: EventCategory::from_wire(cat),
            call: EventCall::new(call),
        })
    }
}

impl Serialize for EventName {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EventName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Error returned when an event-name string lacks the required
/// `<category>.<call>` shape (no dot separator).
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
        write!(
            f,
            "malformed event name (expected 'category.call'): {}",
            self.invalid_name
        )
    }
}

impl std::error::Error for ParseEventNameError {}

// ---------------------------------------------------------------------------
// Protocol participant types and selectors
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

/// A subscription selector used by [`crate::Subscribe`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum EventSelector {
    Exact(EventName),
    Prefix(String),
}

/// Interception priority. Lower numeric values run first.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct InterceptionPriority(i64);

impl InterceptionPriority {
    #[must_use]
    pub const fn new(v: i64) -> Self {
        Self(v)
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Identifier of a node in the per-session tree. Lives on the wire
/// because tree-folding events stamp their `parent_node_id` so the
/// fold doesn't have to consult a shared write cursor.
///
/// Ids are valid only against the tree that produced them. The
/// in-memory `SessionTree` uses the underlying `u64` as a positional
/// index into its node vector and assigns ids by insertion order, so
/// the same numeric id can refer to different nodes across different
/// trees. Replaying the same persisted event log yields the same ids
/// only because the fold is deterministic; an id that originated in
/// one session (or in a sub-agent's tree) is meaningless in another.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct NodeId(u64);

impl NodeId {
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Harness informational messages
// ---------------------------------------------------------------------------

/// Severity of a harness informational message.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessInfoLevel {
    #[default]
    Normal,
    Important,
}

/// An informational message from the harness displayed to the user.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub message: String,
    #[serde(default)]
    pub level: HarnessInfoLevel,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDirStatus {
    #[default]
    New,
    Resumed,
}

impl SessionDirStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resumed => "resumed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessSessionDir {
    pub session_id: SessionId,
    pub path: std::path::PathBuf,
    pub status: SessionDirStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessUiDir {
    pub path: std::path::PathBuf,
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
    /// Selected model, or `None` when no model is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Total context window size, in tokens, if known for the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// Current context usage for the selected model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessContextUsageChanged {
    /// Input tokens consumed by the most recent agent response, if the
    /// provider reported it. `None` means usage has never been
    /// reported for the current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Cached input tokens consumed by the most recent agent response,
    /// if the provider reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Percentage of the context window currently used. `None` when
    /// the model's context window is unknown (no `contextWindow` in
    /// `models.json5` and the provider didn't expose one), so the UI
    /// can fall back to showing raw token count instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
}

/// Reasoning effort level. Maps to provider-specific reasoning
/// controls (OpenAI `reasoning.effort`, Anthropic
/// `thinking.budget_tokens`). `Off` disables it entirely.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Effort {
    #[default]
    Off = 0,
    Minimal = 1,
    Low = 2,
    Medium = 3,
    High = 4,
    /// `rename_all = "snake_case"` would emit `x_high` for this
    /// variant, but the canonical wire string is `xhigh` everywhere
    /// else (`/effort xhigh`, OpenAI's `reasoning_effort` field,
    /// `Display`, `FromStr`, `effort_wire`). Pin it explicitly so
    /// serde-driven config paths (`default_efforts`,
    /// `reasoningEfforts`) agree with the rest.
    #[serde(rename = "xhigh")]
    XHigh = 5,
}

impl Effort {
    /// Cycles to the next level (wraps `XHigh → Off`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::XHigh,
            Self::XHigh => Self::Off,
        }
    }

    /// Cycle in the canonical order, but only through levels that are
    /// in `allowed`. Used by Shift+Tab in the CLI so the user doesn't
    /// land on a level the current model doesn't support (e.g. xhigh
    /// on `gpt-5.4-mini`). Falls back to [`Effort::next`] when
    /// `allowed` is empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        // Bounded by `Effort` variant count — at most one full
        // wrap-around before we either land on an allowed level or
        // confirm none exist.
        for _ in 0..6 {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

    /// Short label for status display (`off` / `low` / `high` / etc).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    /// Numeric tag suitable for storing in an `AtomicU8`. Round-trips
    /// through [`Effort::from_u8`].
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Effort::as_u8`]. Returns `None` for unknown tags so
    /// callers can decide how to recover; the common case (loading from
    /// an atomic mirror) maps `None` to [`Effort::Off`].
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Minimal),
            2 => Some(Self::Low),
            3 => Some(Self::Medium),
            4 => Some(Self::High),
            5 => Some(Self::XHigh),
            _ => None,
        }
    }
}

impl std::str::FromStr for Effort {
    type Err = ParseEffortError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            other => Err(ParseEffortError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when an effort string is not one of the well-known
/// levels (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseEffortError {
    input: String,
}

impl ParseEffortError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseEffortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown effort level `{}`; expected off/minimal/low/medium/high/xhigh",
            self.input
        )
    }
}

impl std::error::Error for ParseEffortError {}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces the current effort.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessEffortChanged {
    pub level: Effort,
}

/// Whether to ask the provider for a human-readable summary of its
/// reasoning, and at what verbosity. Currently only the OpenAI
/// Responses API exposes this surface (`reasoning.summary`). Off by
/// default — summaries cost extra latency and tokens.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingSummary {
    #[default]
    Off,
    Auto,
    Concise,
    Detailed,
}

impl ThinkingSummary {
    /// Short label for status display.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Concise => "concise",
            Self::Detailed => "detailed",
        }
    }

    /// Wire string used by OpenAI's Responses `reasoning.summary`
    /// field, or `None` for the off mode where the field is omitted.
    #[must_use]
    pub const fn as_openai_wire(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Auto => Some("auto"),
            Self::Concise => Some("concise"),
            Self::Detailed => Some("detailed"),
        }
    }
}

impl std::str::FromStr for ThinkingSummary {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            "concise" => Ok(Self::Concise),
            "detailed" => Ok(Self::Detailed),
            other => Err(format!(
                "unknown thinking summary `{other}`; expected off/auto/concise/detailed"
            )),
        }
    }
}

impl std::fmt::Display for ThinkingSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces which efforts are valid for the
/// currently-selected model. Updated on startup and on every model
/// switch. Empty list means no effort applies (no model
/// selected, or the provider doesn't support reasoning).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessEffortsAvailable {
    pub levels: Vec<Effort>,
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
    /// Who started the prompt that produced this tool call. The
    /// harness stamps this from the call's owning conversation so
    /// subscribers can tell main-agent tool activity from sub-agent
    /// (delegate / extension-query) tool activity without having to
    /// map `call_id` back to a conversation themselves.
    #[serde(default)]
    pub originator: PromptOriginator,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInvoke {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: CborValue,
    /// Echo of [`ToolRequest::originator`]. Tools usually don't
    /// branch on it, but it's available for logging / progress
    /// tagging / policy decisions that depend on whether the call
    /// is for the main agent or a sub-agent.
    #[serde(default)]
    pub originator: PromptOriginator,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub result: CborValue,
    /// Echo of the originating [`ToolRequest::originator`]. Tool
    /// extensions usually pass [`PromptOriginator::User`] (the
    /// default); the harness re-stamps this with the call's owning
    /// conversation's originator before broadcasting, so subscribers
    /// see a faithful tag without every extension having to track
    /// it.
    #[serde(default)]
    pub originator: PromptOriginator,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
    /// Echo of the originating [`ToolRequest::originator`]; see
    /// [`ToolResult::originator`].
    #[serde(default)]
    pub originator: PromptOriginator,
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

/// Live snapshot of a sub-agent spawned by the `delegate` tool.
///
/// Emitted by the harness whenever the side conversation backing a
/// `delegate` invocation makes observable progress: a tool call starts
/// or finishes, or the sub-agent reports new context-token usage. The
/// CLI re-renders the running `delegate` tool block to surface this
/// to the user without persisting per-update history. Transient — not
/// folded into the durable session log.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DelegateProgress {
    /// The original parent `delegate` call — the tool block under
    /// which this update should appear.
    pub call_id: ToolCallId,
    /// Display name the parent agent provided for the sub-task.
    pub task_name: String,
    /// Most recent percent-of-context-window the sub-agent reported,
    /// when its model's window size is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_percent: Option<u8>,
    /// Most recent input-token count the sub-agent reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_input_tokens: Option<u64>,
    /// Sub-agent's model context window size, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_window: Option<u64>,
    /// Number of tool calls currently in flight in the sub-agent.
    pub tools_in_flight: u32,
    /// Cumulative number of tool calls the sub-agent has started
    /// during this delegation (including completed and in-flight).
    pub tools_total: u32,
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
    pub name: SkillName,
    pub description: String,
    /// Absolute path to the skill file so the harness can read it.
    pub file_path: std::path::PathBuf,
    /// When true the harness should include this skill in the system prompt.
    pub add_to_prompt: bool,
}

/// An extension discovered one AGENTS.md file and is advertising it to the
/// harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentsMdAvailable {
    /// Absolute path to the AGENTS.md file.
    pub file_path: std::path::PathBuf,
    /// Full file contents, sent eagerly so the harness can inject them
    /// without an extra tool round trip.
    pub content: String,
}

/// An extension finished broadcasting refreshed prompt context for one session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionContextReady {
    pub session_id: SessionId,
}

/// An extension's request for the harness to dispatch a side prompt
/// to the agent.
///
/// The harness spawns a fresh conversation off the user's current
/// branch, treats the side prompt like any other turn (LLM call,
/// optional tool calls, final response), then routes the agent's
/// final text back to the requesting extension as
/// [`ExtAgentQueryResult`] with the same `query_id`.
///
/// Side conversations are persisted as real branches in the session
/// tree but tagged via [`PromptOriginator::Extension`] so UIs can
/// filter them out.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentQuery {
    /// Extension-assigned correlation id, echoed back on the result.
    pub query_id: String,
    /// User-style instruction text. Appended to the current
    /// conversation's history as a `User` message before dispatch.
    pub instruction: String,
    /// `ToolCallId` of the tool invocation that triggered this query,
    /// when the extension is implementing a tool whose live progress
    /// the harness should attribute back to that call. Used by the
    /// `delegate` tool: the harness emits [`DelegateProgress`] under
    /// this id as the side conversation runs. Optional — non-tool
    /// extensions issuing queries leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Human-readable name for the delegated task, surfaced in the
    /// UI alongside [`DelegateProgress`]. Optional for the same reason
    /// `tool_call_id` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_name: Option<String>,
}

/// Reply to an [`ExtAgentQuery`], routed point-to-point back to the
/// extension that issued it. `text` is the agent's final answer
/// (empty when `error` is set).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentQueryResult {
    pub query_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Extension-defined event payload.
///
/// `name` is the dotted event name used for routing and subscription
/// matching. `payload` carries extension-owned CBOR data. When
/// `session_id` is set, the harness may include the event in that
/// session's durable event log according to the surrounding emit
/// metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomEvent {
    pub name: EventName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub payload: CborValue,
}

// ---------------------------------------------------------------------------
// UI events — facts from the user interface
// ---------------------------------------------------------------------------

/// The user submitted a prompt in the UI.
///
/// `originator` is normally [`PromptOriginator::User`] — the field
/// exists so the harness can re-use this event type when dispatching
/// side queries spawned by extensions: the appended user-style
/// instruction also flows as a `UiPromptSubmitted` (so it folds into
/// the session tree), but UIs and other extensions filter on
/// `originator.is_user()` to avoid rendering side conversations as
/// real user turns.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptSubmitted {
    pub session_id: SessionId,
    pub text: String,
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Free-form correlation tag chosen by the submitter and copied
    /// forward onto the first [`SessionPromptCreated`] the harness
    /// emits for this prompt. Lets a client (notably the test helper
    /// in `tau-harness::daemon`) match the response chain to the
    /// submission it made, without relying on event ordering or
    /// re-using a long-lived connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
}

/// A trailing-edge debounced snapshot of the in-progress prompt the
/// user is composing in the UI. Emitted at most once per second
/// while the user is typing; carries the full current contents of
/// the prompt buffer.
///
/// Always transient — never persisted to the per-session event log,
/// never folded into the session tree. Subscribers use it to detect
/// "user is alive" without polling: e.g. std-notifications resets
/// its idle deadline on every draft event so the desktop notification
/// doesn't fire while the user is mid-sentence.
///
/// Future consumers might use the text for autocomplete, draft
/// restoration on UI reconnect, or in-progress prompt sync across
/// multiple attached UIs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptDraft {
    pub session_id: SessionId,
    pub text: String,
}

/// The user requests switching to a different model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiModelSelect {
    /// `"provider_name/model_id"`.
    pub model: ModelId,
}

/// The UI is detaching and wants the daemon to stay alive after it
/// leaves, so a later `tau --attach` can pick up the same
/// session. The harness flips its `exit_on_disconnect` flag to
/// `false` on receipt.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiDetachRequest {}

/// The user requests a effort change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetEffort {
    pub level: Effort,
}

/// The user requests switching to a different session within the same
/// daemon. Harness emits `SessionShutdown` for the current session,
/// then `SessionStarted { reason: New | Resume }` for the new one,
/// and waits for extensions to acknowledge re-init.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSwitchSession {
    pub new_session_id: SessionId,
    /// `New` if the id was just minted, `Resume` if it points at an
    /// existing session on disk.
    pub reason: SessionStartReason,
}

/// The user typed `/tree`: render the session's branching tree (one
/// `harness.info` line per node) to the chat output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiTreeRequest {
    pub session_id: SessionId,
}

/// The user typed `/tree <id>`: move the session's head pointer to the
/// given node, so the next prompt branches off there.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiNavigateTree {
    pub session_id: SessionId,
    pub node_id: u64,
}

/// The user typed `/cancel`: stop advancing the current in-flight
/// prompt at the next harness boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiCancelPrompt {
    pub session_id: SessionId,
}

/// Which stream a [`ShellCommandProgress`] chunk came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStream {
    Stdout,
    Stderr,
}

/// The user submitted a `!`/`!!` shell command.
///
/// `include_in_context`: when `true` (from `!<cmd>`), the harness
/// injects a tagged user message containing the command and its
/// output into the session's conversation history on completion, so
/// the agent sees it on its next turn. When `false` (from `!!<cmd>`),
/// the result is UI-only and never reaches the model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiShellCommand {
    pub session_id: SessionId,
    pub command_id: crate::ShellCommandId,
    pub command: String,
    pub include_in_context: bool,
}

/// A chunk of output from a running user-initiated shell command.
/// Correlated to the request by `command_id`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellCommandProgress {
    pub command_id: crate::ShellCommandId,
    pub stream: ShellStream,
    pub chunk: String,
}

/// A user-initiated shell command completed (exited or was cancelled).
///
/// The extension echoes `command`, `session_id`, and
/// `include_in_context` back from the originating `UiShellCommand`
/// so the harness can act on the finished event without bookkeeping
/// a per-command_id map.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellCommandFinished {
    pub command_id: crate::ShellCommandId,
    pub session_id: SessionId,
    pub command: String,
    pub include_in_context: bool,
    /// Interleaved stdout + stderr (truncated), the same shape the
    /// `shell` tool returns.
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub cancelled: bool,
}

// ---------------------------------------------------------------------------
// Term events — terminal-output side effects directed at the UI
// ---------------------------------------------------------------------------

/// Ask the UI to write an iTerm2 OSC 1337 `SetUserVar` escape sequence
/// to its terminal. The terminal emulator interprets it as setting
/// the named user variable (visible from terminal multiplexers and
/// scripts watching status); the visible terminal output does not
/// change. Useful for surfacing notifications, build status, or any
/// other state to terminal-side tooling.
///
/// The UI base64-encodes `value` and emits the appropriate escape
/// sequence form (plain, or `\x1bPtmux;...\x1b\\` wrapped when running
/// inside `tmux`). Components without access to a terminal — or
/// running through a UI that ignores the event — are no-ops.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Osc1337SetUserVar {
    /// User-variable name. Must be printable ASCII without `=` or
    /// control characters; the UI does not validate this and passes
    /// it through verbatim.
    pub name: String,
    /// Value to associate with `name`. Arbitrary bytes are fine — the
    /// UI base64-encodes before transmission.
    pub value: String,
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

/// A previously queued user prompt that the harness folded into the
/// in-flight turn as a steering message — appended to the next
/// `SessionPromptCreated` for this conversation alongside tool results,
/// rather than waiting for the conversation to return to `Idle` and
/// kicking off a fresh turn.
///
/// Folds into the `SessionTree` the same way as `UiPromptSubmitted`
/// and `SessionUserMessageInjected`: appending one `UserMessage` entry
/// at the current head. UIs typically react by promoting their
/// "(queued)" rendering of this prompt to a regular user message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptSteered {
    pub session_id: SessionId,
    pub text: String,
}

/// Why a `SessionStarted` was published. Lets extensions distinguish
/// "first session of this harness's life" from "user switched to a new
/// session" (e.g. so they can clear caches).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartReason {
    /// The harness eagerly initialized this session at startup.
    Initial,
    /// The user requested a fresh session via `/session new`.
    New,
    /// The user resumed an existing session by id.
    Resume,
}

/// The harness created or switched to a session. Extensions that
/// subscribe react by performing per-session setup (e.g. discovering
/// AGENTS.md) and signal completion with `ExtensionContextReady`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub session_id: SessionId,
    #[serde(default = "default_session_start_reason")]
    pub reason: SessionStartReason,
}

fn default_session_start_reason() -> SessionStartReason {
    SessionStartReason::Initial
}

/// The harness is leaving the current session. Fired before
/// `SessionStarted` for the next one when the user switches sessions.
/// Extensions that hold per-session state subscribe to flush or drop it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionShutdown {
    pub session_id: SessionId,
}

/// A synthetic user message injected into the session by the harness
/// (not authored by the human user directly). Sources include
/// `!`-prefixed shell command output and the eager AGENTS.md context
/// preamble. Carries the fully-rendered text so session replay does
/// not need to re-run any harness-side formatter; the SessionTree
/// folder treats this event the same as `UiPromptSubmitted` —
/// appending one `UserMessage` entry at the current head.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionUserMessageInjected {
    pub session_id: SessionId,
    pub text: String,
}

/// Who initiated the prompt — the human user via the UI, or an
/// extension via [`ExtAgentQuery`].
///
/// The agent's only obligation is to copy the originator from the
/// incoming [`SessionPromptCreated`] onto its outgoing
/// [`AgentResponseFinished`]. The harness reads it on the way back
/// to decide whether the response is a normal turn (route to UI,
/// keep `default_conversation` advancing) or a side-query reply
/// (route an [`ExtAgentQueryResult`] to the requesting extension and
/// tear the conversation down).
///
/// UIs filter on `originator.is_user()` to ignore side conversations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptOriginator {
    /// Default — interactive prompt submitted through the UI.
    #[default]
    User,
    /// Side prompt requested by an extension via [`ExtAgentQuery`].
    Extension {
        name: ExtensionName,
        query_id: String,
    },
}

impl PromptOriginator {
    /// True iff this prompt is the user's interactive turn.
    #[must_use]
    pub const fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }
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
    /// Reasoning effort to request from the provider (if supported).
    #[serde(default)]
    pub effort: Effort,
    /// Whether to ask the provider for a visible reasoning summary,
    /// and at what verbosity. Sent to providers that advertise
    /// `supportsReasoningSummary`; ignored by everyone else.
    #[serde(default)]
    pub thinking_summary: ThinkingSummary,
    /// Who asked for this prompt. Defaults to [`PromptOriginator::User`]
    /// for backward compatibility with old persisted events.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Echo of [`UiPromptSubmitted::ctx_id`] when this prompt was
    /// initiated by a UI submission. Tool-result follow-up
    /// `SessionPromptCreated` events for the same chain do not
    /// inherit it — only the first one does — so a correlator should
    /// capture the resulting [`Self::session_prompt_id`] and track
    /// the rest of the chain by spid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Agent events — facts from the agent backend
// ---------------------------------------------------------------------------

/// The agent accepted a prompt and began processing it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptSubmitted {
    pub session_prompt_id: SessionPromptId,
    /// Echo of [`SessionPromptCreated::originator`]. UIs and other
    /// extensions filter on `originator.is_user()` so the agent
    /// starting a side conversation doesn't trigger user-facing
    /// effects like clearing an idle deadline.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// The agent has new accumulated response text for a prompt.
/// Each update carries the full text so far (replace, not delta).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentResponseUpdated {
    pub session_prompt_id: SessionPromptId,
    pub text: String,
    /// Accumulated provider-supplied reasoning summary so far, if the
    /// provider exposed one. Replace, not delta. Persisted with the
    /// final assistant turn but never replayed back into later
    /// prompts (see `assemble_conversation`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Echo of [`SessionPromptCreated::originator`]. UIs filter on
    /// `originator.is_user()` so the streaming text from a side
    /// conversation doesn't paint into the user's chat window.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// One tool call the agent wants to make.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentToolCall {
    pub id: ToolCallId,
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
    /// Echo of [`SessionPromptCreated::originator`]. The agent must
    /// copy this from the prompt; the harness routes the response
    /// based on it.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Input tokens consumed by the final request, if the provider
    /// reported usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Cached input tokens consumed by the final request, if the
    /// provider reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Final accumulated provider-supplied reasoning summary, if the
    /// provider exposed one. Persisted with the assistant turn but
    /// never replayed into later prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Session token usage snapshot after this response completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<AgentTokenUsage>,
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
        id: ToolCallId,
        /// Same untrusted-LLM-output contract as
        /// `AgentToolCall::name`. See [`ToolNameMaybe`].
        name: ToolNameMaybe,
        input: CborValue,
    },
    ToolResult {
        tool_use_id: ToolCallId,
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
    #[serde(rename = "tool.delegate_progress")]
    ToolDelegateProgress(DelegateProgress),

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
    #[serde(rename = "extension.agent_query")]
    ExtAgentQuery(ExtAgentQuery),
    #[serde(rename = "extension.agent_query_result")]
    ExtAgentQueryResult(ExtAgentQueryResult),
    #[serde(rename = "extension.event")]
    ExtensionEvent(CustomEvent),

    // Harness info
    #[serde(rename = "harness.info")]
    HarnessInfo(HarnessInfo),
    #[serde(rename = "harness.session_dir")]
    HarnessSessionDir(HarnessSessionDir),
    #[serde(rename = "harness.ui_dir")]
    HarnessUiDir(HarnessUiDir),
    #[serde(rename = "harness.models_available")]
    HarnessModelsAvailable(HarnessModelsAvailable),
    #[serde(rename = "harness.model_selected")]
    HarnessModelSelected(HarnessModelSelected),
    #[serde(rename = "harness.context_usage_changed")]
    HarnessContextUsageChanged(HarnessContextUsageChanged),
    #[serde(rename = "harness.effort_changed")]
    HarnessEffortChanged(HarnessEffortChanged),
    #[serde(rename = "harness.efforts_available")]
    HarnessEffortsAvailable(HarnessEffortsAvailable),

    // UI
    #[serde(rename = "ui.prompt_submitted")]
    UiPromptSubmitted(UiPromptSubmitted),
    #[serde(rename = "ui.prompt_draft")]
    UiPromptDraft(UiPromptDraft),
    #[serde(rename = "ui.model_select")]
    UiModelSelect(UiModelSelect),
    #[serde(rename = "ui.set_effort")]
    UiSetEffort(UiSetEffort),
    #[serde(rename = "ui.detach_request")]
    UiDetachRequest(UiDetachRequest),
    #[serde(rename = "ui.shell_command")]
    UiShellCommand(UiShellCommand),
    #[serde(rename = "ui.switch_session")]
    UiSwitchSession(UiSwitchSession),
    #[serde(rename = "ui.tree_request")]
    UiTreeRequest(UiTreeRequest),
    #[serde(rename = "ui.navigate_tree")]
    UiNavigateTree(UiNavigateTree),
    #[serde(rename = "ui.cancel_prompt")]
    UiCancelPrompt(UiCancelPrompt),

    // Term (terminal-output side effects)
    #[serde(rename = "term.osc1337_set_user_var")]
    Osc1337SetUserVar(Osc1337SetUserVar),

    // Shell (user-initiated)
    #[serde(rename = "shell.command_progress")]
    ShellCommandProgress(ShellCommandProgress),
    #[serde(rename = "shell.command_finished")]
    ShellCommandFinished(ShellCommandFinished),

    // Session
    #[serde(rename = "session.prompt_queued")]
    SessionPromptQueued(SessionPromptQueued),
    #[serde(rename = "session.prompt_steered")]
    SessionPromptSteered(SessionPromptSteered),
    #[serde(rename = "session.started")]
    SessionStarted(SessionStarted),
    #[serde(rename = "session.shutdown")]
    SessionShutdown(SessionShutdown),
    #[serde(rename = "session.prompt_created")]
    SessionPromptCreated(SessionPromptCreated),
    #[serde(rename = "session.user_message_injected")]
    SessionUserMessageInjected(SessionUserMessageInjected),

    // Agent
    #[serde(rename = "agent.prompt_submitted")]
    AgentPromptSubmitted(AgentPromptSubmitted),
    #[serde(rename = "agent.response_updated")]
    AgentResponseUpdated(AgentResponseUpdated),
    #[serde(rename = "agent.response_finished")]
    AgentResponseFinished(AgentResponseFinished),
}

impl Event {
    /// Returns the dotted event name carried by this envelope.
    #[must_use]
    pub fn name(&self) -> EventName {
        match self {
            Self::ToolRegister(_) => EventName::TOOL_REGISTER,
            Self::ToolUnregister(_) => EventName::TOOL_UNREGISTER,
            Self::ToolRequest(_) => EventName::TOOL_REQUEST,
            Self::ToolInvoke(_) => EventName::TOOL_INVOKE,
            Self::ToolResult(_) => EventName::TOOL_RESULT,
            Self::ToolError(_) => EventName::TOOL_ERROR,
            Self::ToolProgress(_) => EventName::TOOL_PROGRESS,
            Self::ToolCancel(_) => EventName::TOOL_CANCEL,
            Self::ToolCancelled(_) => EventName::TOOL_CANCELLED,
            Self::ToolDelegateProgress(_) => EventName::TOOL_DELEGATE_PROGRESS,
            Self::ExtensionStarting(_) => EventName::EXTENSION_STARTING,
            Self::ExtensionReady(_) => EventName::EXTENSION_READY,
            Self::ExtensionExited(_) => EventName::EXTENSION_EXITED,
            Self::ExtensionRestarting(_) => EventName::EXTENSION_RESTARTING,
            Self::ExtSkillAvailable(_) => EventName::EXTENSION_SKILL_AVAILABLE,
            Self::ExtAgentsMdAvailable(_) => EventName::EXTENSION_AGENTS_MD_AVAILABLE,
            Self::ExtensionContextReady(_) => EventName::EXTENSION_CONTEXT_READY,
            Self::ExtAgentQuery(_) => EventName::EXTENSION_AGENT_QUERY,
            Self::ExtAgentQueryResult(_) => EventName::EXTENSION_AGENT_QUERY_RESULT,
            Self::ExtensionEvent(event) => event.name.clone(),
            Self::HarnessInfo(_) => EventName::HARNESS_INFO,
            Self::HarnessSessionDir(_) => EventName::HARNESS_SESSION_DIR,
            Self::HarnessUiDir(_) => EventName::HARNESS_UI_DIR,
            Self::HarnessModelsAvailable(_) => EventName::HARNESS_MODELS_AVAILABLE,
            Self::HarnessModelSelected(_) => EventName::HARNESS_MODEL_SELECTED,
            Self::HarnessContextUsageChanged(_) => EventName::HARNESS_CONTEXT_USAGE_CHANGED,
            Self::HarnessEffortChanged(_) => EventName::HARNESS_EFFORT_CHANGED,
            Self::HarnessEffortsAvailable(_) => EventName::HARNESS_EFFORTS_AVAILABLE,
            Self::UiPromptSubmitted(_) => EventName::UI_PROMPT_SUBMITTED,
            Self::UiPromptDraft(_) => EventName::UI_PROMPT_DRAFT,
            Self::UiModelSelect(_) => EventName::UI_MODEL_SELECT,
            Self::UiSetEffort(_) => EventName::UI_SET_EFFORT,
            Self::UiDetachRequest(_) => EventName::UI_DETACH_REQUEST,
            Self::UiShellCommand(_) => EventName::UI_SHELL_COMMAND,
            Self::UiSwitchSession(_) => EventName::UI_SWITCH_SESSION,
            Self::UiTreeRequest(_) => EventName::UI_TREE_REQUEST,
            Self::UiNavigateTree(_) => EventName::UI_NAVIGATE_TREE,
            Self::UiCancelPrompt(_) => EventName::UI_CANCEL_PROMPT,
            Self::Osc1337SetUserVar(_) => EventName::TERM_OSC1337_SET_USER_VAR,
            Self::ShellCommandProgress(_) => EventName::SHELL_COMMAND_PROGRESS,
            Self::ShellCommandFinished(_) => EventName::SHELL_COMMAND_FINISHED,
            Self::SessionPromptQueued(_) => EventName::SESSION_PROMPT_QUEUED,
            Self::SessionPromptSteered(_) => EventName::SESSION_PROMPT_STEERED,
            Self::SessionStarted(_) => EventName::SESSION_STARTED,
            Self::SessionShutdown(_) => EventName::SESSION_SHUTDOWN,
            Self::SessionPromptCreated(_) => EventName::SESSION_PROMPT_CREATED,
            Self::SessionUserMessageInjected(_) => EventName::SESSION_USER_MESSAGE_INJECTED,
            Self::AgentPromptSubmitted(_) => EventName::AGENT_PROMPT_SUBMITTED,
            Self::AgentResponseUpdated(_) => EventName::AGENT_RESPONSE_UPDATED,
            Self::AgentResponseFinished(_) => EventName::AGENT_RESPONSE_FINISHED,
        }
    }

    /// Returns true for protocol events that historically behaved as
    /// transient when sent directly without an [`crate::Emit`] wrapper.
    #[must_use]
    pub const fn defaults_to_transient(&self) -> bool {
        matches!(
            self,
            Self::AgentResponseUpdated(_)
                | Self::ToolProgress(_)
                | Self::ToolDelegateProgress(_)
                | Self::ShellCommandProgress(_)
                | Self::UiPromptDraft(_)
        )
    }
}
