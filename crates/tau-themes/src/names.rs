//! Canonical style name constants.
//!
//! These match the keys in the built-in `tau.json5` theme.

// -- User input --
pub const USER_PROMPT: &str = "user.prompt";
pub const USER_PROMPT_QUEUED: &str = "user.prompt.queued";

// -- Agent responses --
pub const AGENT_RESPONSE: &str = "agent.response";
pub const AGENT_PENDING: &str = "agent.pending";

// -- Tool execution --
//
// Tool-call blocks are composed of three spans: the tool name at the
// start, its arguments in the middle, and a status suffix at the end.
// Each span has its own style so a theme can paint them differently.
pub const TOOL_NAME: &str = "tool.name";
pub const TOOL_ARGS: &str = "tool.args";
pub const TOOL_STATUS_SUCCESS: &str = "tool.status.success";
pub const TOOL_STATUS_ERROR: &str = "tool.status.error";
pub const TOOL_STATUS_INFO: &str = "tool.status.info";
pub const TOOL_PROGRESS: &str = "tool.progress";

// -- Extensions --
pub const EXTENSION_LIFECYCLE: &str = "extension.lifecycle";

// -- System --
pub const SYSTEM_INFO: &str = "system.info";
pub const SYSTEM_DISCONNECT: &str = "system.disconnect";

// -- Model status --
pub const MODEL_STATUS: &str = "model.status";

// -- Completion menu --
pub const COMPLETION_LABEL: &str = "completion.label";
pub const COMPLETION_DESC: &str = "completion.desc";
pub const COMPLETION_SELECTED: &str = "completion.selected";

// -- Prompt --
pub const PROMPT_MARKER: &str = "prompt.marker";

// -- Banner --
pub const BANNER_ACCENT: &str = "banner.accent";
