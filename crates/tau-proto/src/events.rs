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
    AgentTokenUsage, CborValue, DiffSummary, ExtensionName, ModelId, SessionId, SessionPromptId,
    SkillName, ToolCallId, ToolName, ToolNameMaybe,
};

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

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
    pub const HARNESS_ROLES_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "roles_available");
    pub const HARNESS_MODEL_SELECTED: Self =
        Self::from_static(EventCategory::Harness, "model_selected");
    pub const HARNESS_CONTEXT_USAGE_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "context_usage_changed");
    pub const HARNESS_EFFORT_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "effort_changed");
    pub const HARNESS_SERVICE_TIER_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "service_tier_changed");
    pub const HARNESS_EFFORTS_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "efforts_available");
    pub const HARNESS_VERBOSITY_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "verbosity_changed");
    pub const HARNESS_VERBOSITIES_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "verbosities_available");
    pub const HARNESS_THINKING_SUMMARY_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "thinking_summary_changed");
    pub const HARNESS_THINKING_SUMMARIES_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "thinking_summaries_available");

    pub const UI_PROMPT_SUBMITTED: Self = Self::from_static(EventCategory::Ui, "prompt_submitted");
    pub const UI_MODEL_SELECT: Self = Self::from_static(EventCategory::Ui, "model_select");
    pub const UI_ROLE_SELECT: Self = Self::from_static(EventCategory::Ui, "role_select");
    pub const UI_ROLE_UPDATE: Self = Self::from_static(EventCategory::Ui, "role_update");
    pub const UI_SET_EFFORT: Self = Self::from_static(EventCategory::Ui, "set_effort");
    pub const UI_SET_SERVICE_TIER: Self = Self::from_static(EventCategory::Ui, "set_service_tier");
    pub const UI_SET_VERBOSITY: Self = Self::from_static(EventCategory::Ui, "set_verbosity");
    pub const UI_SET_THINKING_SUMMARY: Self =
        Self::from_static(EventCategory::Ui, "set_thinking_summary");
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
    pub const SESSION_PROMPT_PREWARM_REQUESTED: Self =
        Self::from_static(EventCategory::Session, "prompt_prewarm_requested");
    pub const SESSION_USER_MESSAGE_INJECTED: Self =
        Self::from_static(EventCategory::Session, "user_message_injected");

    pub const AGENT_PROMPT_SUBMITTED: Self =
        Self::from_static(EventCategory::Agent, "prompt_submitted");
    pub const AGENT_RESPONSE_UPDATED: Self =
        Self::from_static(EventCategory::Agent, "response_updated");
    pub const AGENT_RESPONSE_FINISHED: Self =
        Self::from_static(EventCategory::Agent, "response_finished");
    pub const AGENT_CACHE_MISS_DIAGNOSTIC: Self =
        Self::from_static(EventCategory::Agent, "cache_miss_diagnostic");
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

/// The harness announces role names with resolved descriptions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRoleInfo {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRolesAvailable {
    pub roles: Vec<HarnessRoleInfo>,
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
    /// Currently selected agent role, when the model was reached via a
    /// role rather than a direct model pick. `None` when the user
    /// selected a model directly, or when no role/model is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
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

    /// True for the default level (`Off`). Used by `ModelParams`'
    /// `#[serde(skip_serializing_if)]` so untouched values stay out
    /// of the wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Off)
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

/// Optional upstream service tier. `Fast` enables Fast mode on providers
/// that expose it; `Flex` is an explicit lower-priority service tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    Fast,
    Flex,
}

impl ServiceTier {
    /// Config/event spelling used by Codex (`fast` / `flex`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Flex => "flex",
        }
    }

    /// OpenAI wire spelling used by Codex requests (`priority` / `flex`).
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Fast => "priority",
            Self::Flex => "flex",
        }
    }
}

/// The harness announces the current service tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessServiceTierChanged {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

/// Output verbosity hint sent to providers that support it (OpenAI
/// GPT-5 family: `verbosity` on Chat Completions, `text.verbosity` on
/// Responses). Providers that don't advertise `supportsVerbosity`
/// silently ignore the field.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Verbosity {
    Low = 0,
    #[default]
    Medium = 1,
    High = 2,
}

impl Verbosity {
    /// Cycles to the next level (wraps `High → Low`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Low,
        }
    }

    /// Cycle in canonical order through levels that are in `allowed`.
    /// Falls back to plain [`Verbosity::next`] when `allowed` is empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        for _ in 0..3 {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

    /// Short label for status display (`low` / `medium` / `high`).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Low),
            1 => Some(Self::Medium),
            2 => Some(Self::High),
            _ => None,
        }
    }

    /// Wire string for OpenAI's `verbosity` / `text.verbosity` field.
    /// All variants map to a non-empty string — there is no "off"
    /// sentinel — so callers gate the field on a provider-level
    /// `supports_verbosity` flag, not on the value itself.
    #[must_use]
    pub const fn as_openai_wire(self) -> &'static str {
        self.as_str()
    }

    /// True for the default level. Used by `#[serde(skip_serializing_if)]`
    /// on `ModelParams` so untouched values stay out of the wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Medium)
    }
}

impl std::str::FromStr for Verbosity {
    type Err = ParseVerbosityError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(ParseVerbosityError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when a verbosity string is not one of the well-known
/// levels (`low`, `medium`, `high`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseVerbosityError {
    input: String,
}

impl ParseVerbosityError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseVerbosityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown verbosity level `{}`; expected low/medium/high",
            self.input
        )
    }
}

impl std::error::Error for ParseVerbosityError {}

impl fmt::Display for Verbosity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces the current verbosity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessVerbosityChanged {
    pub level: Verbosity,
}

/// The harness announces which verbosity levels are valid for the
/// currently-selected model. Updated on startup and on every model
/// switch. Empty list means no model is selected; a single-element
/// `[Medium]` list means the provider doesn't support the knob.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessVerbositiesAvailable {
    pub levels: Vec<Verbosity>,
}

/// Whether to ask the provider for a human-readable summary of its
/// reasoning, and at what verbosity. Currently only the OpenAI
/// Responses API exposes this surface (`reasoning.summary`). Auto by
/// default for providers that advertise `supportsReasoningSummary`;
/// `Off` everywhere else.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ThinkingSummary {
    #[default]
    Off = 0,
    Auto = 1,
    Concise = 2,
    Detailed = 3,
}

impl ThinkingSummary {
    /// Cycles to the next level (wraps `Detailed → Off`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Auto,
            Self::Auto => Self::Concise,
            Self::Concise => Self::Detailed,
            Self::Detailed => Self::Off,
        }
    }

    /// Cycle in canonical order through levels that are in `allowed`.
    /// Falls back to plain [`ThinkingSummary::next`] when `allowed` is
    /// empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        for _ in 0..4 {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

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

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Auto),
            2 => Some(Self::Concise),
            3 => Some(Self::Detailed),
            _ => None,
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

    /// True for the default level.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Off)
    }
}

impl std::str::FromStr for ThinkingSummary {
    type Err = ParseThinkingSummaryError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            "concise" => Ok(Self::Concise),
            "detailed" => Ok(Self::Detailed),
            other => Err(ParseThinkingSummaryError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when a thinking-summary string is not one of the
/// well-known modes (`off`, `auto`, `concise`, `detailed`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseThinkingSummaryError {
    input: String,
}

impl ParseThinkingSummaryError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseThinkingSummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown thinking summary `{}`; expected off/auto/concise/detailed",
            self.input
        )
    }
}

impl std::error::Error for ParseThinkingSummaryError {}

impl std::fmt::Display for ThinkingSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces the current thinking-summary mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessThinkingSummaryChanged {
    pub level: ThinkingSummary,
}

/// The harness announces which thinking-summary modes are valid for
/// the currently-selected model. Empty list means no model is
/// selected; `[Off]` means the provider doesn't expose summaries.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessThinkingSummariesAvailable {
    pub levels: Vec<ThinkingSummary>,
}

/// Per-prompt model knobs the harness selects, persists, and stamps
/// onto every [`SessionPromptCreated`]. Bundling these together lets
/// providers and backends thread one struct through instead of a
/// growing list of fields. Each component independently falls back to
/// "omit the field" when its [`Verbosity::is_default`] / `is_default`
/// helper says it's still at the default.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelParams {
    #[serde(default, skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
    #[serde(default, skip_serializing_if = "Verbosity::is_default")]
    pub verbosity: Verbosity,
    #[serde(default, skip_serializing_if = "ThinkingSummary::is_default")]
    pub thinking_summary: ThinkingSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
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

/// Per-prompt knob telling the provider whether the model is allowed
/// to call tools on this turn. Stamped onto every
/// [`SessionPromptCreated`]; the harness sets [`Self::None`] for
/// non-tool extension-side queries (e.g. `std-notifications`' idle
/// summary) so the cache prefix (tools + system_prompt) stays
/// byte-identical to the parent conv's while still preventing the
/// summarizer from accidentally calling `write` / `edit` / `delegate`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call tools (provider default).
    #[default]
    Auto,
    /// The model must produce a text answer this turn; tools are
    /// still declared in the request (so cache prefix matches), but
    /// the provider rejects tool-call output.
    None,
}

impl ToolChoice {
    /// True for the default value. Used by `#[serde(skip_serializing_if)]`
    /// on [`SessionPromptCreated`] so untouched values stay out of the
    /// wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Auto)
    }
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
    /// Optional UI display descriptor populated by the tool. When
    /// present, lets the renderer paint a uniform tool block without
    /// inspecting `result`'s tool-specific shape. `None` for older
    /// logs and tools that haven't migrated yet — the renderer falls
    /// back to a minimal generic block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
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
    /// See [`ToolResult::display`]. On error, the descriptor's
    /// `status` is typically [`ToolDisplayStatus::Error`] and
    /// `status_text` carries a short error label for the chip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
    /// Echo of the originating [`ToolRequest::originator`]; see
    /// [`ToolResult::originator`].
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// UI display descriptor for one finished tool call.
///
/// Populated by the tool side (in-tree dispatchers or out-of-tree
/// extensions) and rendered uniformly by the CLI without inspecting
/// the tool's specific result shape. Carries everything the chip line
/// needs (args label, info chips, stats, status word) plus an
/// optional rich payload to render in a block below.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolDisplay {
    /// Short label rendered alongside the tool name (e.g.
    /// `"src/main.rs"`, `"\"foo\" in src"`, `"git status"`). Empty
    /// when the tool has nothing useful to surface beyond its name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    /// Compact `(NM, NL, NkB)`-style stats. Each field is optional
    /// so the renderer can omit a slot rather than emit `(0M, 1L)`.
    #[serde(default, skip_serializing_if = "ToolDisplayStats::is_empty")]
    pub stats: ToolDisplayStats,
    /// Labelled counter chips (current / optional total) rendered
    /// between stats and `info_chips`. Used for tools that surface
    /// progress data: `ctx: 6%/200k`, `tools: 3`, `bytes: 12/200`,
    /// etc. The unit hint picks the rendering shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress_counters: Vec<ProgressCounter>,
    /// Free-form info chips beyond the stats slot (e.g. `"(2
    /// suggestions)"`, `"(3 entries)"`). Rendered between counters
    /// and status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub info_chips: Vec<String>,
    /// Severity of the trailing status chip. Picks its themed color.
    pub status: ToolDisplayStatus,
    /// Short status word/message rendered as the last chip (e.g.
    /// `"ok"`, `"ok: no matches"`, `"err: regex parse error: …"`).
    pub status_text: String,
    /// Optional rich content rendered in a block below the chip row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<ToolDisplayPayload>,
}

/// One labelled counter rendered as an info chip. Shape depends on
/// `unit` and which of `current` / `total` are populated:
/// - `Count`: `N` (current only) or `N/M` (both).
/// - `Percent`: `N%` (current only) or `N%/M` (both — `M` is e.g. a context
///   window size, formatted by [`format_token_count`]).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProgressCounter {
    /// Human-readable prefix shown before the value (e.g. `"ctx"`,
    /// `"tools"`). Renders as `"label: value"`. `None` for an
    /// unlabelled chip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// What `current` and `total` represent. Picks the rendering.
    pub unit: ProgressUnit,
    /// Current value. `None` is rendered as `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u64>,
    /// Optional denominator. For `Count`, the cumulative total; for
    /// `Percent`, the underlying span (e.g. context window size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit {
    /// Raw integers. Renders as `N` or `N/M`. Default if the sender
    /// doesn't specify.
    #[default]
    Count,
    /// `current` is a percent 0..=100. Renders as `N%` or
    /// `N%/format_token_count(total)`.
    Percent,
}

/// Volume metrics. Each is optional because a given tool typically
/// reports only some of them — `read` has lines/bytes but no matches;
/// `grep` has all three; `ls` has none (uses [`ToolDisplay::info_chips`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ToolDisplayStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl ToolDisplayStats {
    pub fn is_empty(&self) -> bool {
        self.matches.is_none() && self.lines.is_none() && self.bytes.is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDisplayStatus {
    #[default]
    Success,
    Warning,
    Error,
    /// The tool is still running. Used by progress events. The
    /// renderer trades the trailing status chip for a `…` indicator.
    InProgress,
}

/// Rich content rendered below the chip row. Closed for now — extend
/// as new tool kinds need it. Tools that don't produce a rich payload
/// (most of them) leave [`ToolDisplay::payload`] as `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDisplayPayload {
    /// Structured file diff. The renderer derives the `+N -M` chip
    /// from the summary's `added`/`removed` and renders the hunks
    /// below the chip row.
    Diff(DiffSummary),
    /// Plain text rendered below the chip row. Used when the inline
    /// args label would be too noisy (e.g. multi-line shell commands).
    Text { text: String },
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// UI display descriptor for the running delegate block. The
    /// harness fills this in from the fields above so the renderer
    /// can paint the progress generically without inspecting them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
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

/// The user requests switching to an agent role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRoleSelect {
    pub role: String,
}

/// The user changes or deletes an agent role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRoleUpdate {
    pub role: String,
    pub action: UiRoleUpdateAction,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UiRoleUpdateAction {
    Delete,
    Set { setting: String, value: String },
}

/// The user requests a effort change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetEffort {
    pub level: Effort,
}

/// The user requests a service-tier change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetServiceTier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

/// The user requests a verbosity change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetVerbosity {
    pub level: Verbosity,
}

/// The user requests a thinking-summary mode change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetThinkingSummary {
    pub level: ThinkingSummary,
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

/// Stop advancing an in-flight prompt at the next harness boundary.
///
/// Originally tied to the user typing `/cancel`, now also published
/// by the harness itself to preempt non-tool extension side
/// conversations when a user prompt arrives. The optional
/// [`Self::session_prompt_id`] disambiguates the two cases:
///
/// - `None` — broadcast cancel (the legacy `/cancel` semantics). The harness
///   clears the default conversation; the agent aborts whatever prompt it's
///   currently retry-sleeping on.
/// - `Some(spid)` — targeted cancel. The agent only aborts if the in-flight
///   prompt's spid matches; otherwise the frame is left in the retry-loop's
///   deferred buffer so the wrong prompt isn't collateral damage. The agent
///   serializes prompt processing internally, so a cancel published while the
///   spid in question is still queued (not yet dequeued from the agent's frame
///   channel) is harmless — it just falls through with no in-flight match.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiCancelPrompt {
    pub session_id: SessionId,
    /// Optional target. See struct doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_prompt_id: Option<SessionPromptId>,
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

/// Reference to a message prefix carried by an earlier prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptMessagePrefix {
    /// Prompt whose materialized messages contain the prefix.
    pub base_session_prompt_id: SessionPromptId,
    /// Number of leading messages to copy from the base prompt.
    pub message_count: usize,
}

/// Reference to a system prompt carried by an earlier prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptSystemPromptRef {
    /// Prompt whose materialized system prompt contains the full text.
    pub base_session_prompt_id: SessionPromptId,
}

/// Reference to tool definitions carried by an earlier prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptToolsRef {
    /// Prompt whose materialized tools contain the full tool list.
    pub base_session_prompt_id: SessionPromptId,
}

/// The harness persisted a user prompt and assigned it an ID.
/// Also carries the assembled conversation context for the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptCreated {
    pub session_prompt_id: SessionPromptId,
    pub session_id: SessionId,
    /// System prompt, or empty when [`Self::system_prompt_ref`] is set.
    pub system_prompt: String,
    /// Optional reference to a full system prompt from an earlier prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_ref: Option<PromptSystemPromptRef>,
    /// Conversation messages, or only the suffix when
    /// [`Self::message_prefix`] is set.
    pub messages: Vec<ConversationMessage>,
    /// Optional reference to leading messages from an earlier prompt.
    /// When set, handlers materialize the full message list as:
    /// `base.messages[..message_count] + messages`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_prefix: Option<PromptMessagePrefix>,
    /// Tool definitions, or empty when [`Self::tools_ref`] is set.
    pub tools: Vec<ToolDefinition>,
    /// Optional reference to full tool definitions from an earlier prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_ref: Option<PromptToolsRef>,
    /// Currently selected model as `"provider/model_id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Per-prompt model knobs (reasoning effort, output verbosity,
    /// thinking-summary mode). The harness stamps in its current
    /// selection on every prompt; backends pass each field through
    /// only when the provider advertises support for it.
    #[serde(default)]
    pub model_params: ModelParams,
    /// Whether tool calls are allowed on this turn. Defaults to
    /// `Auto`; the harness flips to `None` for non-tool extension
    /// side queries (e.g. idle-summary) so they cannot trigger
    /// destructive tools. Backends emit this as `tool_choice: "none"`
    /// on the upstream request body.
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// Who asked for this prompt. Defaults to [`PromptOriginator::User`]
    /// for backward compatibility with old persisted events.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// When `true`, the backend uses the **user's** `prompt_cache_key`
    /// bucket for this turn even though [`Self::originator`] is an
    /// extension. The harness sets this for non-fan-out side queries
    /// (notably `std-notifications`' idle-summary) so a single side
    /// turn can hit the user's already-warm prefix cache. Delegate
    /// sub-agents leave it `false` because parallel fan-out on a
    /// shared key would exceed OpenAI's 15 RPM-per-bucket guideline
    /// and degrade routing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub share_user_cache_key: bool,
    /// Echo of [`UiPromptSubmitted::ctx_id`] when this prompt was
    /// initiated by a UI submission. Tool-result follow-up
    /// `SessionPromptCreated` events for the same chain do not
    /// inherit it — only the first one does — so a correlator should
    /// capture the resulting [`Self::session_prompt_id`] and track
    /// the rest of the chain by spid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
    /// Hint for backends that support stateful chaining (currently the
    /// OpenAI Codex Responses API): the most recent
    /// [`AgentResponseFinished::response_id`] from this conversation
    /// and the index in `messages` where the new turn's content
    /// begins. Backends that don't support stateful chaining ignore
    /// this and replay the full `messages` slice; the Responses
    /// backend slices `messages[message_index..]` and sets
    /// `previous_response_id` + `store: true` on the upstream call.
    /// `None` when no chain has been established yet, or when an
    /// edit / model switch / error invalidated the prior chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response: Option<PreviousResponseRef>,
}

/// Best-effort provider-side prompt-cache prewarm request.
///
/// Carries the same stable prefix fields as the first real
/// [`SessionPromptCreated`] but intentionally has no
/// [`SessionPromptId`], no user task prompt, and no
/// `previous_response_id`. Agents that support a non-generating
/// upstream call may send it; all others no-op.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptPrewarmRequested {
    pub session_id: SessionId,
    pub system_prompt: String,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolDefinition>,
    /// Currently selected model as `"provider/model_id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Per-prompt model knobs, matching the first real prompt.
    #[serde(default)]
    pub model_params: ModelParams,
    /// Whether tool calls are allowed on the warmed prefix.
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// Prewarm only warms the interactive user's cache bucket.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Preserve the first real user prompt's cache-key derivation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub share_user_cache_key: bool,
}

/// Reference to a prior turn's response, used to enable stateful
/// chaining on backends that support it. See
/// [`SessionPromptCreated::previous_response`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreviousResponseRef {
    /// `response.id` returned by the provider on the most recent
    /// successful turn for this conversation.
    pub id: String,
    /// Index in [`SessionPromptCreated::messages`] where messages
    /// added since the prior response begin. Backends slicing for a
    /// delta call use `messages[message_index..]`.
    pub message_index: usize,
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
    /// Pre-rendered live-header descriptor stamped by the harness
    /// before publishing so UIs can render the running block (e.g.
    /// `grep "foo" in src …`) without per-tool string knowledge.
    /// `None` on the wire from the agent driver; the harness fills
    /// it in by inspecting `name` + `arguments` once the call has
    /// been accepted. Subscribers that don't render UI ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
}

/// The agent finished processing a prompt.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    /// Output tokens produced by the final request, if the provider
    /// reported them. Includes reasoning tokens on backends that
    /// generate hidden chain-of-thought (o-series, GPT-5), which both
    /// the OpenAI Responses and Chat Completions APIs roll into the
    /// top-level output count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Final accumulated provider-supplied reasoning summary, if the
    /// provider exposed one. Persisted with the assistant turn but
    /// never replayed into later prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Session-scoped token usage snapshot after this response
    /// completed. Filled in by the harness (which knows the qualified
    /// `provider/model` id and the running session totals) before the
    /// event is re-published; agents emit `None` here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<AgentTokenUsage>,
    /// Which LLM backend handled this turn. Recorded once per turn
    /// (instead of in a trace line) so offline inspection of the
    /// event log can correlate cache-miss / retry patterns with the
    /// backend that produced them — e.g. distinguishing OpenAI
    /// public-API behavior from the ChatGPT Codex Responses backend.
    /// `None` for turns that never reached a backend (e.g. an
    /// agent-side resolution failure or the in-process echo agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<AgentBackend>,
    /// Provider-supplied `response.id` for this turn, when the
    /// backend exposed one. Used by the harness to thread a
    /// `previous_response_id` into the next `SessionPromptCreated`
    /// so the upstream call can run in stateful-chain mode (smaller
    /// request body, server-side reasoning continuity). `None` for
    /// backends that don't expose response ids (Chat Completions),
    /// for error turns, and for turns the harness shouldn't chain
    /// from (the agent withholds the id when it judged the response
    /// unfit for chaining — e.g. after a fallback retry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Provider-supplied assistant-phase label captured for this
    /// turn. `Some(_)` only when the backend supports the Codex
    /// `phase` field on assistant `message` items (see
    /// [`MessagePhase`]) and the model emitted one. Persisted with
    /// the turn so later prompts can echo it back, preventing the
    /// "early stopping" behavior the OpenAI deployment checklist
    /// warns about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
    /// Provider-supplied reasoning output items, in stream order. Each
    /// entry is the raw JSON of one `reasoning` item — including its
    /// `id` and `encrypted_content` — captured when the backend ran
    /// with `include: ["reasoning.encrypted_content"]` on the request
    /// (currently the Codex Responses backend, gated on its
    /// `supports_encrypted_reasoning` flag). The harness persists
    /// these blobs and replays them verbatim on later full-transcript
    /// turns so the model's reasoning continuity survives a broken
    /// chain — same role as Pi's `thinkingSignature` blob.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_items: Vec<String>,
    /// Per-turn delta of the agent's Codex WS pool counters. `Some(_)`
    /// only for Responses-backend turns where the WS path was
    /// attempted (i.e. `cfg.supports_websocket` and the per-session
    /// sticky-disable flag was off). `None` for Chat Completions and
    /// for Responses sessions that have been permanently flipped to
    /// HTTP+SSE. Lets offline analysis attribute a low
    /// `cached_tokens` to a chain-strip event (the Codex chain cache
    /// is connection-local; a fresh socket or a silent reconnect
    /// drops the in-request `previous_response_id`, collapsing
    /// `cached_tokens` to the static system+tools baseline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_pool_delta: Option<WsPoolDelta>,
}

/// Per-turn delta of the agent's Codex WebSocket pool counters. All
/// three counters are monotonic-since-process-start in the agent;
/// the harness records the *delta* incurred by a single turn so
/// offline analysis can attribute cache misses to WS-layer events.
///
/// A non-zero `silent_reconnects` or `chain_strips_on_fresh` on a
/// turn is the definitive signature of why that turn's
/// `previous_response_id` was stripped on the wire — and therefore
/// why its `cached_tokens` dropped to the static system+tools
/// baseline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsPoolDelta {
    /// Fresh sockets opened this turn. Counts every reason: cold
    /// pool, server-age purge, bearer rotation, silent-reconnect
    /// recovery.
    pub upgrades: u32,
    /// Cached sockets that died mid-turn and triggered the silent
    /// reopen-and-replay-without-chain-id recovery this turn.
    pub silent_reconnects: u32,
    /// Times the fresh-socket path stripped `previous_response_id`
    /// from the outgoing request this turn because the new socket's
    /// chain cache was empty by definition.
    pub chain_strips_on_fresh: u32,
}

/// Diagnostic emitted when a chained prompt reports unexpectedly low
/// provider cache reuse. The harness derives it from the original
/// [`SessionPromptCreated`] plus final [`AgentResponseFinished`]
/// token usage so offline analysis can distinguish provider/cache-key
/// misses from obvious WS chain-strip misses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentCacheMissDiagnostic {
    pub session_prompt_id: SessionPromptId,
    /// Currently selected model as `"provider/model_id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    pub previous_response_id: String,
    pub previous_response_message_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_prefix_count: Option<usize>,
    #[serde(default)]
    pub originator: PromptOriginator,
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// Wire `prompt_cache_key` if known to the component emitting the
    /// diagnostic. The harness currently lacks provider config, so it
    /// leaves this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_pool_delta: Option<WsPoolDelta>,
    /// Hex blake3 fingerprint of the provider-visible request fields
    /// Tau expects to remain stable across a chain.
    pub request_body_fingerprint: String,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub previous_input_tokens: u64,
    pub cacheable_input_tokens: u64,
    pub corrected_cache_efficiency: f32,
}

/// Identifies the LLM backend that handled an
/// [`AgentResponseFinished`].
///
/// Kind discriminates the provider API shape (Chat Completions vs.
/// Responses), and `base_url` pins down the specific endpoint —
/// `https://api.openai.com/v1` and `https://chatgpt.com/backend-api`
/// share the Responses kind but have very different cache /
/// rate-limit behavior, so the base URL is what an offline analysis
/// needs to tell them apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBackend {
    pub kind: AgentBackendKind,
    pub base_url: String,
    /// Wire transport the turn was sent over. Defaults to
    /// `HttpSse` for backwards compatibility with sessions recorded
    /// before this field existed.
    #[serde(default)]
    pub transport: AgentBackendTransport,
}

/// The provider API shape an [`AgentBackend`] talks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    ChatCompletions,
    Responses,
}

/// Transport the agent used to deliver one turn. `HttpSse` covers
/// both the Chat Completions path and the HTTP+SSE Responses path
/// (kind discriminates which API); `Websocket` is the Codex
/// Responses persistent-WS path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendTransport {
    /// One-shot HTTP request with Server-Sent Events streaming.
    /// Default — covers Chat Completions and the HTTP+SSE Responses
    /// fallback.
    #[default]
    HttpSse,
    /// Persistent WebSocket. Only Codex Responses today.
    Websocket,
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
    /// One provider-supplied reasoning output item, stored as the raw
    /// JSON the API emitted (item id + `encrypted_content` + optional
    /// summary). The harness treats the payload as opaque and replays
    /// it verbatim as a top-level `input` item on later turns, which
    /// is how Codex `gpt-5.3-codex+` preserves model reasoning state
    /// across requests when the chain is broken (reconnect, fork,
    /// fingerprint mismatch). Same shape as Pi's `thinkingSignature`
    /// — opaque blob, forward-compatible against schema drift.
    ///
    /// Assistant role only; appears before any `Text`/`ToolUse` blocks
    /// from the same response.
    Reasoning {
        /// Raw JSON of the provider's `reasoning` output item. Tau
        /// never parses fields out of this — backends that don't
        /// understand the source provider's schema (e.g. Chat
        /// Completions replaying Codex history) simply drop it.
        item: String,
    },
}

/// Assistant-message phase label, mirroring the OpenAI Codex
/// `phase` field on assistant `message` items.
///
/// The Codex Responses API attaches one of these to each assistant
/// turn it produces (on models that support it, currently
/// `gpt-5.3-codex` and later). Resending the same value on later
/// turns lets the model distinguish intermediate progress from
/// completed work — the doc-recommended remedy for "early stopping"
/// in long, tool-heavy runs.
///
/// We capture the value off the SSE stream, persist it alongside the
/// assistant turn, and echo it back on every re-serialized history
/// replay. Older models that do not emit this field still receive
/// the `final_answer` default on assistant message items the harness
/// re-serializes, which is the explicit guidance in the deployment
/// checklist.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    /// Intermediate progress / preliminary notes.
    Commentary,
    /// Final completed response.
    FinalAnswer,
}

impl MessagePhase {
    /// Wire string accepted by the OpenAI Codex Responses API on
    /// assistant `message` items.
    #[must_use]
    pub const fn as_openai_wire(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
        }
    }
}

/// One message in the conversation history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub content: Vec<ContentBlock>,
    /// Assistant-only: the `phase` label the provider attached to
    /// this turn, when one was captured. Echoed back on outgoing
    /// requests so the model retains its read of "this prior turn
    /// was commentary vs. final" — the OpenAI deployment checklist
    /// flags missing phase on history as a cause of early stopping
    /// on `gpt-5.3-codex` and later.
    ///
    /// `None` for user messages and for assistant turns produced by
    /// providers that do not emit phase; the Responses backend
    /// substitutes `final_answer` on the wire in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
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
    #[serde(rename = "harness.roles_available")]
    HarnessRolesAvailable(HarnessRolesAvailable),
    #[serde(rename = "harness.model_selected")]
    HarnessModelSelected(HarnessModelSelected),
    #[serde(rename = "harness.context_usage_changed")]
    HarnessContextUsageChanged(HarnessContextUsageChanged),
    #[serde(rename = "harness.effort_changed")]
    HarnessEffortChanged(HarnessEffortChanged),
    #[serde(rename = "harness.service_tier_changed")]
    HarnessServiceTierChanged(HarnessServiceTierChanged),
    #[serde(rename = "harness.efforts_available")]
    HarnessEffortsAvailable(HarnessEffortsAvailable),
    #[serde(rename = "harness.verbosity_changed")]
    HarnessVerbosityChanged(HarnessVerbosityChanged),
    #[serde(rename = "harness.verbosities_available")]
    HarnessVerbositiesAvailable(HarnessVerbositiesAvailable),
    #[serde(rename = "harness.thinking_summary_changed")]
    HarnessThinkingSummaryChanged(HarnessThinkingSummaryChanged),
    #[serde(rename = "harness.thinking_summaries_available")]
    HarnessThinkingSummariesAvailable(HarnessThinkingSummariesAvailable),

    // UI
    #[serde(rename = "ui.prompt_submitted")]
    UiPromptSubmitted(UiPromptSubmitted),
    #[serde(rename = "ui.prompt_draft")]
    UiPromptDraft(UiPromptDraft),
    #[serde(rename = "ui.model_select")]
    UiModelSelect(UiModelSelect),
    #[serde(rename = "ui.role_select")]
    UiRoleSelect(UiRoleSelect),
    #[serde(rename = "ui.role_update")]
    UiRoleUpdate(UiRoleUpdate),
    #[serde(rename = "ui.set_effort")]
    UiSetEffort(UiSetEffort),
    #[serde(rename = "ui.set_service_tier")]
    UiSetServiceTier(UiSetServiceTier),
    #[serde(rename = "ui.set_verbosity")]
    UiSetVerbosity(UiSetVerbosity),
    #[serde(rename = "ui.set_thinking_summary")]
    UiSetThinkingSummary(UiSetThinkingSummary),
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
    #[serde(rename = "session.prompt_prewarm_requested")]
    SessionPromptPrewarmRequested(SessionPromptPrewarmRequested),
    #[serde(rename = "session.user_message_injected")]
    SessionUserMessageInjected(SessionUserMessageInjected),

    // Agent
    #[serde(rename = "agent.prompt_submitted")]
    AgentPromptSubmitted(AgentPromptSubmitted),
    #[serde(rename = "agent.response_updated")]
    AgentResponseUpdated(AgentResponseUpdated),
    #[serde(rename = "agent.response_finished")]
    AgentResponseFinished(AgentResponseFinished),
    #[serde(rename = "agent.cache_miss_diagnostic")]
    AgentCacheMissDiagnostic(AgentCacheMissDiagnostic),
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
            Self::HarnessRolesAvailable(_) => EventName::HARNESS_ROLES_AVAILABLE,
            Self::HarnessModelSelected(_) => EventName::HARNESS_MODEL_SELECTED,
            Self::HarnessContextUsageChanged(_) => EventName::HARNESS_CONTEXT_USAGE_CHANGED,
            Self::HarnessEffortChanged(_) => EventName::HARNESS_EFFORT_CHANGED,
            Self::HarnessServiceTierChanged(_) => EventName::HARNESS_SERVICE_TIER_CHANGED,
            Self::HarnessEffortsAvailable(_) => EventName::HARNESS_EFFORTS_AVAILABLE,
            Self::HarnessVerbosityChanged(_) => EventName::HARNESS_VERBOSITY_CHANGED,
            Self::HarnessVerbositiesAvailable(_) => EventName::HARNESS_VERBOSITIES_AVAILABLE,
            Self::HarnessThinkingSummaryChanged(_) => EventName::HARNESS_THINKING_SUMMARY_CHANGED,
            Self::HarnessThinkingSummariesAvailable(_) => {
                EventName::HARNESS_THINKING_SUMMARIES_AVAILABLE
            }
            Self::UiPromptSubmitted(_) => EventName::UI_PROMPT_SUBMITTED,
            Self::UiPromptDraft(_) => EventName::UI_PROMPT_DRAFT,
            Self::UiModelSelect(_) => EventName::UI_MODEL_SELECT,
            Self::UiRoleSelect(_) => EventName::UI_ROLE_SELECT,
            Self::UiRoleUpdate(_) => EventName::UI_ROLE_UPDATE,
            Self::UiSetEffort(_) => EventName::UI_SET_EFFORT,
            Self::UiSetServiceTier(_) => EventName::UI_SET_SERVICE_TIER,
            Self::UiSetVerbosity(_) => EventName::UI_SET_VERBOSITY,
            Self::UiSetThinkingSummary(_) => EventName::UI_SET_THINKING_SUMMARY,
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
            Self::SessionPromptPrewarmRequested(_) => EventName::SESSION_PROMPT_PREWARM_REQUESTED,
            Self::SessionUserMessageInjected(_) => EventName::SESSION_USER_MESSAGE_INJECTED,
            Self::AgentPromptSubmitted(_) => EventName::AGENT_PROMPT_SUBMITTED,
            Self::AgentResponseUpdated(_) => EventName::AGENT_RESPONSE_UPDATED,
            Self::AgentResponseFinished(_) => EventName::AGENT_RESPONSE_FINISHED,
            Self::AgentCacheMissDiagnostic(_) => EventName::AGENT_CACHE_MISS_DIAGNOSTIC,
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
