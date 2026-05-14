//! User settings loaded from `~/.config/tau/` with `.d/` directory
//! overrides. Three config files:
//!
//! - `cli.json5` — CLI display preferences
//! - `harness.json5` — harness/agent settings (default model, etc.)
//! - `models.json5` — LLM provider and model registry
//!
//! Uses the `config` crate for layered JSON5 loading.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tau_proto::{ModelId, ModelName, ProviderName};

// ---------------------------------------------------------------------------
// Built-in configs
//
// Tau ships its baseline `cli.json5`, `cli-bindings.json5` and
// `harness.json5` as ordinary source files under
// `crates/tau-config/config/`, embedded via `include_str!`. They are
// layered underneath the user's own files at load time (see
// `load_json5_layered_with_builtin`) so user partial overrides keep
// working without the public `CliSettings` / `HarnessSettings` types
// having to carry a `#[serde(default)]` and a synthesized `Default`
// impl that secretly parses a file.
// ---------------------------------------------------------------------------

const BUILT_IN_CLI_JSON5: &str = include_str!("../config/built-in.cli.json5");
const BUILT_IN_CLI_BINDINGS_JSON5: &str = include_str!("../config/built-in.cli-bindings.json5");
const BUILT_IN_HARNESS_JSON5: &str = include_str!("../config/built-in.harness.json5");

fn parse_built_in<T: for<'de> Deserialize<'de>>(name: &str, text: &str) -> T {
    json5::from_str(text).unwrap_or_else(|err| {
        panic!("tau ships with malformed {name}: {err}\nthis is a bug; please report it")
    })
}

// ---------------------------------------------------------------------------
// CLI settings
// ---------------------------------------------------------------------------

/// CLI display settings loaded from `cli.json5`.
///
/// Has no `Default` impl on purpose — the baseline lives in
/// `config/built-in.cli.json5` and is layered in by the loader. Use
/// [`CliSettings::built_in`] when you need a fresh, populated value
/// in a test or fallback.
#[derive(Clone, Debug, Deserialize)]
pub struct CliSettings {
    /// Show a greeting message on startup.
    pub greeting: bool,
    /// Show the tau ASCII logo on startup.
    pub show_logo: bool,
    /// Use a bar-shaped cursor in the CLI. When false, use a steady
    /// block cursor instead.
    pub bar_cursor: bool,
    /// Symbol shown before the input prompt.
    pub prompt_symbol: String,
    /// Symbol shown before submitted prompts in the transcript.
    pub submitted_prompt_symbol: String,
    /// Key bindings for prompt-local shell actions. Defaults to an
    /// empty map at the serde layer; the loader merges
    /// `built-in.cli-bindings.json5` underneath the user's bindings.
    #[serde(default)]
    pub bind: HashMap<String, CliBindingAction>,
}

impl CliSettings {
    /// The fully-populated baseline that ships with tau, parsed from
    /// the embedded `built-in.cli.json5` plus `built-in.cli-bindings.json5`.
    pub fn built_in() -> Self {
        let mut s: Self = parse_built_in("built-in.cli.json5", BUILT_IN_CLI_JSON5);
        s.bind = default_cli_bindings();
        s
    }
}

/// Shell command configured for a CLI key binding.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum CliShellCommand {
    Command(String),
    Options { command: String, trim: bool },
}

impl CliShellCommand {
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self::Options {
            command: command.into(),
            trim: false,
        }
    }

    #[must_use]
    pub fn new_trimmed(command: impl Into<String>) -> Self {
        Self::Options {
            command: command.into(),
            trim: true,
        }
    }

    #[must_use]
    pub fn command(&self) -> &str {
        match self {
            Self::Command(command) | Self::Options { command, .. } => command,
        }
    }

    #[must_use]
    pub fn trim(&self) -> bool {
        match self {
            Self::Command(_) => false,
            Self::Options { trim, .. } => *trim,
        }
    }
}

/// CLI key binding action.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CliBindingAction {
    /// Action name, e.g. `shell-prompt-insert`, `shell-prompt-edit`,
    /// `fast-toggle`, or `role-cycle`.
    pub action: String,
    /// Shell command to execute. `None` for actions that don't shell
    /// out (e.g. `prompt-previous`, `prompt-next`, `fast-toggle`,
    /// `role-cycle`).
    pub command: Option<String>,
    /// Whether to trim command stdout before insertion.
    pub trim: bool,
}

impl Default for CliBindingAction {
    fn default() -> Self {
        Self {
            action: "shell-prompt-insert".to_owned(),
            command: None,
            trim: false,
        }
    }
}

/// Parse the embedded `built-in.cli-bindings.json5`. Called from
/// [`CliSettings::built_in`] and from [`load_cli_settings_in`] (the
/// latter overlays user bindings on top of this baseline so users
/// don't lose unmentioned keys when they customize a single chord).
pub(crate) fn default_cli_bindings() -> HashMap<String, CliBindingAction> {
    parse_built_in("built-in.cli-bindings.json5", BUILT_IN_CLI_BINDINGS_JSON5)
}

// ---------------------------------------------------------------------------
// CLI runtime state
// ---------------------------------------------------------------------------

/// Mutable CLI state persisted across runs at
/// `<state_dir>/cli.json`. Distinct from `CliSettings` (config) —
/// this file is written by the CLI itself in response to
/// `/set <name> <value>` commands.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CliState {
    /// Whether to render file-mutation diffs in their full expanded
    /// form (vs the compact `+N/-M` chip). Controlled by
    /// `/set show-diff <true|false>`.
    pub show_diff: bool,
    /// Whether to render the agent's reasoning summary (the
    /// `agent.thinking` block). Controlled by
    /// `/set show-thinking <true|false>`.
    pub show_thinking: bool,
    /// Whether to render per-turn token usage stats below agent
    /// responses. Controlled by `/set show-token-stats <true|false>`.
    pub show_token_stats: bool,
    /// How tool calls are rendered in the transcript. Controlled by
    /// `/set show-tools <off|summarize-turn|summarize-prompt|compact|full>`.
    pub show_tools: ShowTools,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ShowTools {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "summarize-turn")]
    SummarizeTurn,
    #[serde(rename = "summarize-prompt")]
    SummarizePrompt,
    #[serde(rename = "compact")]
    Compact,
    #[serde(rename = "full", alias = "on")]
    #[default]
    Full,
}

impl ShowTools {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::SummarizeTurn => "summarize-turn",
            Self::SummarizePrompt => "summarize-prompt",
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "summarize-turn" => Some(Self::SummarizeTurn),
            "summarize-prompt" => Some(Self::SummarizePrompt),
            "compact" => Some(Self::Compact),
            "full" | "on" => Some(Self::Full),
            _ => None,
        }
    }
}

impl Default for CliState {
    fn default() -> Self {
        Self {
            show_diff: false,
            show_thinking: true,
            show_token_stats: false,
            show_tools: ShowTools::Full,
        }
    }
}

impl CliState {
    /// Load the persisted CLI state. Missing / malformed file → defaults.
    #[must_use]
    pub fn load(dirs: &TauDirs) -> Self {
        let Some(dir) = dirs.state_dir.as_ref() else {
            return Self::default();
        };
        let path = dir.join("cli.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Persist current state. Best-effort: a slash command never fails
    /// because the user's state dir is read-only, but failures are
    /// logged on stderr so a silently-resetting state dir is visible
    /// to the user.
    pub fn save(&self, dirs: &TauDirs) {
        let Some(dir) = dirs.state_dir.as_ref() else {
            return;
        };
        if let Err(error) = self.save_inner(dir) {
            eprintln!(
                "tau: failed to persist CLI state to {}: {error}",
                dir.join("cli.json").display()
            );
        }
    }

    fn save_inner(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("cli.json");
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

// ---------------------------------------------------------------------------
// Harness settings
// ---------------------------------------------------------------------------

/// Harness/agent settings loaded from `harness.json5`.
///
/// Has no `Default` impl on purpose — the baseline lives in
/// `config/built-in.harness.json5` and is layered in by the loader.
/// Use [`HarnessSettings::built_in`] when you need a fresh,
/// populated value in a test or fallback.
#[derive(Clone, Debug, Deserialize)]
pub struct HarnessSettings {
    /// Default model to use (e.g.
    /// `"anthropic/claude-sonnet-4-20250514"`).
    pub default_model: Option<ModelId>,

    /// Default per-prompt model parameters (effort, verbosity,
    /// thinking-summary), keyed by model id. Each entry's fields are
    /// independently optional — fields omitted from a per-model entry
    /// fall back to the harness's per-param middle/auto default.
    pub default_params: HashMap<ModelId, tau_proto::ModelParams>,

    /// Number of days to keep inactive session state directories.
    /// Set to `0` to disable session cleanup.
    pub session_retention_days: u64,

    /// Extension table, keyed by name. Built-in entries (`core-agent`,
    /// `core-shell`) come pre-baked at the harness level; anything the
    /// user writes here overrides those per-field, or adds a new
    /// extension.
    ///
    /// Example `harness.json5`:
    /// ```json5
    /// {
    ///   extensions: {
    ///     // disable the built-in shell extension
    ///     "core-shell": { enable: false },
    ///     // run the agent through ssh on a remote box
    ///     "core-agent": { prefix: ["ssh", "user@host"] },
    ///     // a third-party extension
    ///     mything: { command: ["/usr/local/bin/my-tau-ext"] },
    ///   },
    /// }
    /// ```
    pub extensions: HashMap<String, ExtensionEntry>,
}

impl HarnessSettings {
    /// The fully-populated baseline that ships with tau, parsed from
    /// the embedded `built-in.harness.json5`.
    pub fn built_in() -> Self {
        parse_built_in("built-in.harness.json5", BUILT_IN_HARNESS_JSON5)
    }

    #[must_use]
    pub fn session_retention(&self) -> Option<Duration> {
        if self.session_retention_days == 0 {
            return None;
        }
        Some(Duration::from_secs(
            self.session_retention_days.saturating_mul(24 * 60 * 60),
        ))
    }
}

/// One entry in the harness's `extensions` map.
///
/// All fields are optional on the wire so users can override just the
/// fields they care about for built-in extensions; the harness merges
/// these with built-in defaults at startup. `None` on any field means
/// "the user did not say anything" — distinct from an empty value the
/// user set on purpose.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionEntry {
    /// argv prefix prepended before `command`. Useful for wrappers
    /// that don't change the inner command, e.g.
    /// `["ssh", "user@host"]` to run remotely or
    /// `["bwrap", "--ro-bind", "/", "/", "--"]` to sandbox.
    pub prefix: Option<Vec<String>>,

    /// argv of the extension itself. `command[0]` is the executable;
    /// the rest are arguments. For built-in extensions this defaults
    /// to `[<current-exe>]`; for new entries this must be set
    /// explicitly. Tau-piggybacking entries can omit `command` and
    /// use `suffix` to pick the subcommand on the running tau binary.
    pub command: Option<Vec<String>>,

    /// argv suffix appended after `command`. Symmetric to `prefix`.
    /// Built-in extensions use this to spell their subcommand (e.g.
    /// `["ext", "agent"]`) so the `command` slot stays as the tau
    /// binary path.
    pub suffix: Option<Vec<String>>,

    /// Whether to run this extension. Defaults to the built-in's
    /// `enable` (or `true` for user-added entries). Set to `false`
    /// to keep the entry in config but skip spawning.
    pub enable: Option<bool>,

    /// Role tag. Exactly one extension must have `role: "agent"`.
    /// Built-in `agent` defaults to that; everything else is treated
    /// as a tool extension.
    pub role: Option<String>,

    /// Free-form configuration object handed to the extension at
    /// startup via `LifecycleConfigure`. The harness does not
    /// interpret it — the extension defines and validates its own
    /// schema. Absent on the wire means "merge nothing in", so the
    /// built-in's default config object is used unchanged.
    pub config: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Model registry
// ---------------------------------------------------------------------------

/// Top-level model configuration (mirrors Pi's models.json).
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ModelRegistry {
    /// Named providers, keyed by [`ProviderName`].
    pub providers: HashMap<ProviderName, ProviderConfig>,
    /// Named agent roles. Each role is a partial set of model settings;
    /// missing fields inherit from `default`, then from hardcoded fallbacks.
    #[serde(rename = "defaultRoles", default = "default_agent_roles")]
    pub default_roles: HashMap<String, AgentRole>,
}

fn default_agent_roles() -> HashMap<String, AgentRole> {
    let mut default_roles = HashMap::new();
    default_roles.insert("default".to_owned(), AgentRole::default());
    default_roles.insert("smart".to_owned(), AgentRole::default());
    default_roles.insert(
        "deep".to_owned(),
        AgentRole {
            effort: Some(tau_proto::Effort::XHigh),
            verbosity: Some(tau_proto::Verbosity::High),
            thinking_summary: Some(tau_proto::ThinkingSummary::Detailed),
            ..AgentRole::default()
        },
    );
    default_roles.insert(
        "rush".to_owned(),
        AgentRole {
            effort: Some(tau_proto::Effort::Low),
            verbosity: Some(tau_proto::Verbosity::Low),
            thinking_summary: Some(tau_proto::ThinkingSummary::Off),
            ..AgentRole::default()
        },
    );
    default_roles
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self {
            providers: HashMap::new(),
            default_roles: default_agent_roles(),
        }
    }
}

/// Partial agent-role settings loaded from `models.json5` and persisted
/// to state. `None` means "inherit" for every field.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentRole {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<tau_proto::Effort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<tau_proto::Verbosity>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "thinkingSummary")]
    pub thinking_summary: Option<tau_proto::ThinkingSummary>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "fastMode")]
    pub fast_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "serviceTier")]
    pub service_tier: Option<tau_proto::ServiceTier>,
}

/// One LLM provider configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderConfig {
    /// Base URL for the API endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// API protocol: "anthropic", "openai-completions", etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Authentication method: "api-key" (default when `apiKey` is set),
    /// "openai-codex", "github-copilot", or "none". Kept as a raw
    /// `Option<String>` so that the typed view from
    /// [`ProviderConfig::auth_type`] can localize unknown values to the
    /// offending provider entry rather than failing whole-file load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// API key or environment variable name. Prefix with `!` for
    /// shell command execution (Pi convention).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Extra HTTP headers (key → value or env var name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// Optional provider-side prompt cache retention policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    /// Compatibility flags for non-standard providers.
    #[serde(skip_serializing_if = "ProviderCompat::is_default")]
    pub compat: ProviderCompat,
    /// Models available from this provider.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelConfig>,
}

/// Authentication method for a [`ProviderConfig`]. Single source of truth
/// for the `auth` taxonomy — exhaustive `match`es against this enum should
/// replace string comparisons against the raw `auth` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthType {
    /// No authentication needed (local Ollama / llama.cpp).
    None,
    /// Direct API-key authentication.
    ApiKey,
    /// OpenAI Codex / ChatGPT subscription (auth-code + PKCE OAuth).
    OpenaiCodex,
    /// GitHub Copilot subscription (device-code OAuth).
    GithubCopilot,
}

impl AuthType {
    /// Wire-format string matching the `auth` field in `models.json5`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ApiKey => "api-key",
            Self::OpenaiCodex => "openai-codex",
            Self::GithubCopilot => "github-copilot",
        }
    }

    /// Returns true if this auth type requires an OAuth login flow.
    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::OpenaiCodex | Self::GithubCopilot)
    }
}

impl std::fmt::Display for AuthType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ProviderConfig {
    /// Resolve the typed [`AuthType`] for this provider.
    ///
    /// `auth` takes precedence; if absent, infers `ApiKey` when an
    /// `apiKey` is configured and `None` otherwise. Unknown `auth`
    /// strings are returned as `Err(s)` so the caller can surface
    /// per-provider config errors without aborting the whole file.
    pub fn auth_type(&self) -> Result<AuthType, &str> {
        match self.auth.as_deref() {
            None if self.api_key.is_some() => Ok(AuthType::ApiKey),
            None => Ok(AuthType::None),
            Some("none") => Ok(AuthType::None),
            Some("api-key") => Ok(AuthType::ApiKey),
            Some("openai-codex") => Ok(AuthType::OpenaiCodex),
            Some("github-copilot") => Ok(AuthType::GithubCopilot),
            Some(other) => Err(other),
        }
    }
}

/// Compatibility flags for providers that don't support all features.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderCompat {
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_prefill: bool,
    pub supports_prompt_cache_key: bool,
    pub supports_prompt_cache_retention: bool,
    /// llama.cpp-compatible Chat Completions extension: accepts
    /// `cache_prompt` requests and returns `tokens_cached` /
    /// `tokens_evaluated` response stats.
    pub supports_llama_cpp_cache: bool,
    /// Provider's API accepts `reasoning.summary` and streams
    /// `response.reasoning_summary_text.*` events. Currently only
    /// the OpenAI Responses API surface.
    pub supports_reasoning_summary: bool,
    /// Provider's API accepts an output-verbosity hint (`verbosity`
    /// on Chat Completions, `text.verbosity` on the Responses API).
    /// Currently OpenAI's GPT-5 family. Off by default so we don't
    /// emit the field to providers that reject unknown arguments.
    pub supports_verbosity: bool,
    /// Provider's API accepts the `phase` field on assistant
    /// `message` items in the Responses API input (`commentary` /
    /// `final_answer`). Currently OpenAI Codex on `gpt-5.3-codex`
    /// and later. Off by default — emitting the field to a provider
    /// that rejects unknown arguments breaks the call. The Codex
    /// Responses endpoint auto-enables this at resolver time, so
    /// users don't need to flip it on for the built-in OAuth flow.
    pub supports_phase: bool,
    /// Provider's endpoint accepts `include:
    /// ["reasoning.encrypted_content"]` on the request and fills in
    /// the `encrypted_content` blob on each `reasoning` output item
    /// the model emits. Off by default — emitting the opt-in to a
    /// provider that rejects unknown arguments breaks the call. The
    /// built-in Codex Responses endpoint auto-enables this for every
    /// model at resolver time, so OAuth users never need to flip it
    /// on; flip it on here for self-hosted or proxy backends that
    /// expose the same surface.
    ///
    /// No per-model gate: this is a server capability, not a model
    /// capability — models that don't emit reasoning simply omit
    /// `encrypted_content`, and the agent skips capturing items
    /// without the blob (so a non-reasoning model on a Codex-shaped
    /// endpoint costs nothing). Companion to
    /// [`Self::supports_phase`], but resolved independently because
    /// `phase` *is* model-gated (older Codex generations reject the
    /// field).
    pub supports_encrypted_reasoning: bool,
    /// Provider exposes the Responses API over a persistent
    /// WebSocket transport instead of (or in addition to) HTTP+SSE.
    /// When on, the agent caches per-conversation WS connections
    /// across prompts so the connection-local `previous_response_id`
    /// cache stays warm — the change buys ~40% on tool-heavy
    /// turns. Off by default — custom OpenAI-compatible endpoints
    /// generally do not implement WS mode. Auto-enabled for the
    /// built-in OpenAI Codex endpoint at resolver time, so users
    /// don't need to flip it on for the OAuth flow.
    pub supports_websocket: bool,
}

impl Default for ProviderCompat {
    fn default() -> Self {
        Self {
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_prefill: true,
            supports_prompt_cache_key: false,
            supports_prompt_cache_retention: false,
            supports_llama_cpp_cache: false,
            supports_reasoning_summary: false,
            supports_verbosity: false,
            supports_phase: false,
            supports_encrypted_reasoning: false,
            supports_websocket: false,
        }
    }
}

impl ProviderCompat {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Provider-side prompt cache retention policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum PromptCacheRetention {
    #[serde(rename = "in_memory")]
    InMemory,
    #[serde(rename = "24h")]
    Extended24h,
}

impl PromptCacheRetention {
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::InMemory => "in_memory",
            Self::Extended24h => "24h",
        }
    }
}

/// One model available from a provider.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// Model identifier (e.g. `"claude-sonnet-4-20250514"`).
    pub id: ModelName,
    /// Optional display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Max output tokens override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Total context window size, in tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Whether this model accepts `reasoning_effort=xhigh`. `None`
    /// (the default) means "fall back to the built-in whitelist for
    /// well-known OpenAI model IDs" — see [`is_known_xhigh_model_id`].
    /// Set explicitly in `models.json5` (`supportsXhigh: true|false`)
    /// to override. Ignored when `reasoning_efforts` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_xhigh: Option<bool>,
    /// Full per-model override of the reasoning-effort levels this
    /// model accepts. When set, this list replaces both the
    /// canonical default set (`[off, minimal, low, medium, high]`)
    /// and the `supports_xhigh` flag — use it for escape-hatch cases
    /// where Tau's built-in detection is wrong or out of date, or
    /// for asymmetric models like `gpt-5.4-pro` which accept only
    /// `[medium, high, xhigh]`. The list also takes precedence over
    /// the provider-level `supportsReasoningEffort` flag.
    /// `None` keeps the default behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<tau_proto::Effort>>,
    /// Per-model override of the provider-level `supportsVerbosity`
    /// flag. `None` (the default) defers to the provider flag.
    /// Ignored when `verbosities` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_verbosity: Option<bool>,
    /// Full per-model override of the verbosity levels this model
    /// accepts. When set, this list replaces both the canonical
    /// `[low, medium, high]` set and the `supports_verbosity` /
    /// `supportsVerbosity` flags. `None` keeps the default behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosities: Option<Vec<tau_proto::Verbosity>>,
}

impl ModelConfig {
    /// Effective xhigh support: explicit `supports_xhigh` wins,
    /// otherwise consult the built-in whitelist of known OpenAI
    /// model IDs. Not consulted when `reasoning_efforts` is set —
    /// that field is an authoritative override.
    #[must_use]
    pub fn supports_xhigh(&self) -> bool {
        self.supports_xhigh
            .unwrap_or_else(|| is_known_xhigh_model_id(&self.id))
    }
}

/// Returns `true` for OpenAI model IDs known to accept
/// `prompt_cache_retention="24h"` on the public API. Per OpenAI's
/// prompt-caching guide, extended retention is offered only on
/// gpt-5.5 and forward; sending the param to older models is
/// rejected as an unknown argument, so we whitelist conservatively
/// and let unknown models fall back to the default in-memory cache.
#[must_use]
pub fn is_known_24h_prompt_cache_model_id(id: &str) -> bool {
    const PREFIXES: &[&str] = &["gpt-5.5"];
    PREFIXES.iter().any(|p| id.starts_with(p))
}

/// Returns `true` for OpenAI model IDs known to accept
/// `reasoning_effort=xhigh` on the public API as of 2026-05.
///
/// Curated from OpenAI's model documentation:
/// - `gpt-5.5` family (excluding mini/nano)
/// - `gpt-5.4` and `gpt-5.4-pro` (excluding mini/nano)
/// - `gpt-5.3-codex` family
/// - `gpt-5.2` (excluding mini/nano) — introduced xhigh
/// - `gpt-5.1-codex-max`
///
/// Matches by prefix so dated/aliased variants (e.g.
/// `gpt-5.5-2026-04-15`) pick up the same setting as their base ID.
/// `mini` and `nano` variants are excluded — they top out at `high`.
#[must_use]
pub fn is_known_xhigh_model_id(id: &str) -> bool {
    if id.contains("mini") || id.contains("nano") {
        return false;
    }
    const PREFIXES: &[&str] = &[
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.3-codex",
        "gpt-5.2",
        "gpt-5.1-codex-max",
    ];
    PREFIXES.iter().any(|p| id.starts_with(p))
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Errors from settings/model loading.
#[derive(Debug)]
pub enum SettingsError {
    Config(config::ConfigError),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(source) => write!(f, "settings error: {source}"),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
        }
    }
}

impl From<config::ConfigError> for SettingsError {
    fn from(source: config::ConfigError) -> Self {
        Self::Config(source)
    }
}

/// Returns the default tau config directory (`~/.config/tau`).
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tau"))
}

/// Returns the default tau state directory (`~/.local/state/tau`).
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("tau"))
}

/// Returns the per-session storage root inside `state_dir`. Each
/// session lives in its own directory at
/// `<state_dir>/sessions/<session_id>/`; grouping them under a
/// dedicated subdirectory keeps the state dir's top level reserved
/// for tau-wide scalar state (`policy.cbor`, `cli.json`, …).
#[must_use]
pub fn sessions_dir_of(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions")
}

/// Returns the default tau per-session storage root
/// (`~/.local/state/tau/sessions`).
#[must_use]
pub fn sessions_dir() -> Option<PathBuf> {
    state_dir().map(|d| sessions_dir_of(&d))
}

/// Overridable directory layout for tau. Use the defaults (`Self::default()`)
/// for normal user runs or construct explicit paths for tests and custom
/// installations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TauDirs {
    /// Where to look for `cli.json5`, `harness.json5`, `models.json5`, etc.
    pub config_dir: Option<PathBuf>,
    /// Where to read/write runtime state like `harness.json5`.
    pub state_dir: Option<PathBuf>,
}

impl Default for TauDirs {
    fn default() -> Self {
        Self {
            config_dir: config_dir(),
            state_dir: state_dir(),
        }
    }
}

/// Loads CLI settings from `cli.json5` with `cli.d/*.json5` overrides.
pub fn load_cli_settings() -> Result<CliSettings, SettingsError> {
    load_cli_settings_in(&TauDirs::default())
}

/// Like [`load_cli_settings`] but reads from an explicit directory layout.
///
/// The embedded `built-in.cli.json5` is layered underneath the user's
/// own `cli.json5` (and any `cli.d/*.json5` drop-ins), so the user
/// can write a partial file and unmentioned fields fall back to the
/// shipped defaults. The `bind` map is merged per-key on top so a
/// user customizing one chord doesn't lose the others.
pub fn load_cli_settings_in(dirs: &TauDirs) -> Result<CliSettings, SettingsError> {
    let mut settings: CliSettings =
        load_json5_layered_with_builtin(BUILT_IN_CLI_JSON5, dirs.config_dir.as_deref(), "cli")?;
    let mut bindings = default_cli_bindings();
    bindings.extend(settings.bind);
    settings.bind = bindings;
    Ok(settings)
}

/// Loads harness settings from `harness.json5` with `harness.d/*.json5`
/// overrides.
pub fn load_harness_settings() -> Result<HarnessSettings, SettingsError> {
    load_harness_settings_in(&TauDirs::default())
}

/// Like [`load_harness_settings`] but reads from an explicit directory layout.
pub fn load_harness_settings_in(dirs: &TauDirs) -> Result<HarnessSettings, SettingsError> {
    load_json5_layered_with_builtin(
        BUILT_IN_HARNESS_JSON5,
        dirs.config_dir.as_deref(),
        "harness",
    )
}

/// Loads the model registry from `models.json5` with `models.d/*.json5`
/// overrides.
pub fn load_models() -> Result<ModelRegistry, SettingsError> {
    load_models_in(&TauDirs::default())
}

/// Like [`load_models`] but reads from an explicit directory layout.
pub fn load_models_in(dirs: &TauDirs) -> Result<ModelRegistry, SettingsError> {
    let Some(ref dir) = dirs.config_dir else {
        return Ok(ModelRegistry::default());
    };
    load_json5_layered(dir, "models")
}

/// Like [`load_json5_layered`] but also stacks an embedded built-in
/// json5 string underneath the user's files. `T` therefore doesn't
/// need a `Default` impl — the built-in layer always supplies every
/// required field.
fn load_json5_layered_with_builtin<T: for<'de> Deserialize<'de>>(
    built_in_text: &'static str,
    dir: Option<&Path>,
    name: &str,
) -> Result<T, SettingsError> {
    let mut builder = config::Config::builder().add_source(
        config::File::from_str(built_in_text, config::FileFormat::Json5).required(true),
    );

    if let Some(dir) = dir {
        let base_path = dir.join(format!("{name}.json5"));
        if base_path.exists() {
            builder = builder.add_source(
                config::File::from(base_path)
                    .format(config::FileFormat::Json5)
                    .required(true),
            );
        }

        let drop_dir = dir.join(format!("{name}.d"));
        if drop_dir.is_dir() {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(&drop_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|ext| ext == "json5"))
                .collect();
            paths.sort();
            for path in paths {
                builder = builder.add_source(
                    config::File::from(path)
                        .format(config::FileFormat::Json5)
                        .required(true),
                );
            }
        }
    }

    builder
        .build()?
        .try_deserialize()
        .map_err(SettingsError::from)
}

/// Generic layered JSON5 loader: reads `{name}.json5` then all
/// `{name}.d/*.json5` files sorted alphabetically, merging each
/// layer on top.
fn load_json5_layered<T: for<'de> Deserialize<'de> + Default>(
    dir: &Path,
    name: &str,
) -> Result<T, SettingsError> {
    let base_path = dir.join(format!("{name}.json5"));
    let drop_dir = dir.join(format!("{name}.d"));

    let mut builder = config::Config::builder();
    let mut any_source = false;

    // Base file is optional, but parse errors must surface.
    // We guard on exists() and use required(true) so a missing file
    // is fine but a malformed one is an error.
    if base_path.exists() {
        builder = builder.add_source(
            config::File::from(base_path)
                .format(config::FileFormat::Json5)
                .required(true),
        );
        any_source = true;
    }

    // Drop-in files: same — optional to have, but must parse.
    if drop_dir.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&drop_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "json5"))
            .collect();
        paths.sort();
        for path in paths {
            builder = builder.add_source(
                config::File::from(path)
                    .format(config::FileFormat::Json5)
                    .required(true),
            );
            any_source = true;
        }
    }

    if !any_source {
        return Ok(T::default());
    }

    let config = builder.build()?;
    config.try_deserialize().map_err(SettingsError::from)
}

// ---------------------------------------------------------------------------
// Typed writes against `models.json5`
// ---------------------------------------------------------------------------

/// Add or update a provider entry in `~/.config/tau/models.json5`.
///
/// Reads the existing file (preserving unknown top-level keys and other
/// provider entries), inserts or replaces `providers[name]` with the
/// serialized `provider`, and writes atomically. Comments and trailing
/// commas in the source file are NOT preserved across the round-trip;
/// the caller is responsible for warning the user.
///
/// Returns the path of the file that was written.
pub fn add_provider(name: &ProviderName, provider: &ProviderConfig) -> std::io::Result<PathBuf> {
    add_provider_in(&TauDirs::default(), name, provider)
}

/// Like [`add_provider`] but writes against an explicit directory layout.
pub fn add_provider_in(
    dirs: &TauDirs,
    name: &ProviderName,
    provider: &ProviderConfig,
) -> std::io::Result<PathBuf> {
    let dir = dirs.config_dir.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no config directory available",
        )
    })?;
    let path = dir.join("models.json5");
    let mut root = read_models_root(&path)?;
    let entry = serde_json::to_value(provider).map_err(invalid_data)?;

    root.as_object_mut()
        .ok_or_else(|| invalid_data("models.json5 root is not an object"))?
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid_data("providers is not an object"))?
        .insert(name.as_str().to_owned(), entry);

    let json = serde_json::to_string_pretty(&root).map_err(invalid_data)?;
    crate::atomic::atomic_write_following_symlink(&path, json.as_bytes(), None)?;
    Ok(path)
}

/// Remove a provider entry from `~/.config/tau/models.json5`.
///
/// Returns `Ok(true)` if the provider was present and removed, `Ok(false)`
/// if the file or the named entry does not exist.
pub fn remove_provider(name: &ProviderName) -> std::io::Result<bool> {
    remove_provider_in(&TauDirs::default(), name)
}

/// Like [`remove_provider`] but operates against an explicit directory layout.
pub fn remove_provider_in(dirs: &TauDirs, name: &ProviderName) -> std::io::Result<bool> {
    let dir = dirs.config_dir.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no config directory available",
        )
    })?;
    let path = dir.join("models.json5");
    if !path.exists() {
        return Ok(false);
    }
    let mut root = read_models_root(&path)?;
    let removed = root
        .as_object_mut()
        .and_then(|o| o.get_mut("providers"))
        .and_then(|p| p.as_object_mut())
        .is_some_and(|providers| providers.remove(name.as_str()).is_some());
    if removed {
        let json = serde_json::to_string_pretty(&root).map_err(invalid_data)?;
        crate::atomic::atomic_write_following_symlink(&path, json.as_bytes(), None)?;
    }
    Ok(removed)
}

fn read_models_root(path: &Path) -> std::io::Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({ "providers": {} }));
    }
    let text = std::fs::read_to_string(path)?;
    json5::from_str(&text).map_err(invalid_data)
}

fn invalid_data<E: std::fmt::Display>(error: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests;
