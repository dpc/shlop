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

// ---------------------------------------------------------------------------
// CLI settings
// ---------------------------------------------------------------------------

/// CLI display settings loaded from `cli.json5`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
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
    /// Key bindings for prompt-local shell actions.
    pub bind: HashMap<String, CliBindingAction>,
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
    /// Action name, e.g. `shell-prompt-insert` or `shell-prompt-edit`.
    pub action: String,
    /// Shell command to execute.
    pub command: String,
    /// Whether to trim command stdout before insertion.
    pub trim: bool,
}

impl Default for CliBindingAction {
    fn default() -> Self {
        Self {
            action: "shell-prompt-insert".to_owned(),
            command: String::new(),
            trim: false,
        }
    }
}

impl Default for CliSettings {
    fn default() -> Self {
        Self {
            greeting: true,
            show_logo: true,
            bar_cursor: true,
            prompt_symbol: "◯".to_string(),
            submitted_prompt_symbol: "⬤".to_string(),
            bind: default_cli_bindings(),
        }
    }
}

fn default_cli_bindings() -> HashMap<String, CliBindingAction> {
    HashMap::from([
        (
            "C-f".to_owned(),
            CliBindingAction {
                action: "shell-prompt-insert".to_owned(),
                command: "rg --files --hidden --glob '!.git' | fzf --height=100%".to_owned(),
                trim: true,
            },
        ),
        (
            "C-r".to_owned(),
            CliBindingAction {
                action: "shell-prompt-insert".to_owned(),
                command: r#"RG_PREFIX='rg --line-number --column --no-heading --color=always --smart-case'; fzf --height=100% --ansi --disabled --bind "change:reload:$RG_PREFIX {q} || true" --delimiter : --preview 'bat --color=always --style=numbers --highlight-line {2} -- {1} 2>/dev/null || awk -v line={2} '\''line - 4 <= NR && NR <= line + 4 { printf "%6d  %s\n", NR, $0 }'\'' -- {1}' --preview-window '+{2}/2' | cut -d: -f1"#.to_owned(),
                trim: true,
            },
        ),
        (
            "C-k".to_owned(),
            CliBindingAction {
                action: "prompt-previous".to_owned(),
                command: String::new(),
                trim: false,
            },
        ),
        (
            "C-Up".to_owned(),
            CliBindingAction {
                action: "prompt-previous".to_owned(),
                command: String::new(),
                trim: false,
            },
        ),
        (
            "C-j".to_owned(),
            CliBindingAction {
                action: "prompt-next".to_owned(),
                command: String::new(),
                trim: false,
            },
        ),
        (
            "C-Down".to_owned(),
            CliBindingAction {
                action: "prompt-next".to_owned(),
                command: String::new(),
                trim: false,
            },
        ),
        (
            "C-o".to_owned(),
            CliBindingAction {
                action: "shell-prompt-edit".to_owned(),
                command: "${VISUAL:-${EDITOR:-}} \"$TAU_PROMPT_PATH\"".to_owned(),
                trim: false,
            },
        ),
        (
            "C-g".to_owned(),
            CliBindingAction {
                action: "shell-prompt-edit".to_owned(),
                command: "${VISUAL:-${EDITOR:-}} \"$TAU_PROMPT_PATH\"".to_owned(),
                trim: false,
            },
        ),
    ])
}

// ---------------------------------------------------------------------------
// CLI runtime state
// ---------------------------------------------------------------------------

/// Mutable CLI state persisted across runs at
/// `<state_dir>/cli.json`. Distinct from `CliSettings` (config) —
/// this file is written by the CLI itself in response to slash
/// commands like `/show-diff`, `/show-thinking`, and
/// `/show-cache-stats`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CliState {
    /// Whether to render file-mutation diffs in their full expanded
    /// form (vs the compact `+N/-M` chip). Toggled by `/show-diff`.
    pub show_diff: bool,
    /// Whether to render the agent's reasoning summary (the
    /// `agent.thinking` block). Toggled by `/show-thinking`.
    pub show_thinking: bool,
    /// Whether to render provider prompt-cache hit stats in the model
    /// status bar. Toggled by `/show-cache-stats`.
    pub show_cache_stats: bool,
}

impl Default for CliState {
    fn default() -> Self {
        Self {
            show_diff: false,
            show_thinking: true,
            show_cache_stats: true,
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
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
        std::fs::write(path, text)
    }
}

// ---------------------------------------------------------------------------
// Harness settings
// ---------------------------------------------------------------------------

/// Harness/agent settings loaded from `harness.json5`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct HarnessSettings {
    /// Default model provider/model to use (e.g.
    /// "anthropic/claude-sonnet-4-20250514").
    pub default_model: Option<String>,

    /// Default effort per model (`provider/model` -> level).
    pub default_efforts: HashMap<String, tau_proto::Effort>,

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

impl Default for HarnessSettings {
    fn default() -> Self {
        Self {
            default_model: None,
            default_efforts: HashMap::new(),
            session_retention_days: 60,
            extensions: HashMap::new(),
        }
    }
}

impl HarnessSettings {
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
    /// to `[<current-exe>, "ext", <name>]`; for new entries
    /// this must be set explicitly.
    pub command: Option<Vec<String>>,

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
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ModelRegistry {
    /// Named providers, keyed by provider name.
    pub providers: HashMap<String, ProviderConfig>,
}

/// One LLM provider configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderConfig {
    /// Base URL for the API endpoint.
    pub base_url: Option<String>,
    /// API protocol: "anthropic", "openai-completions", etc.
    pub api: Option<String>,
    /// Authentication method: "api-key" (default when `apiKey` is set),
    /// "openai-codex", "github-copilot", or "none". Kept as a raw
    /// `Option<String>` so that the typed view from
    /// [`ProviderConfig::auth_type`] can localize unknown values to the
    /// offending provider entry rather than failing whole-file load.
    pub auth: Option<String>,
    /// API key or environment variable name. Prefix with `!` for
    /// shell command execution (Pi convention).
    pub api_key: Option<String>,
    /// Extra HTTP headers (key → value or env var name).
    pub headers: Option<HashMap<String, String>>,
    /// Optional provider-side prompt cache retention policy.
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    /// Compatibility flags for non-standard providers.
    pub compat: ProviderCompat,
    /// Models available from this provider.
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
#[derive(Clone, Debug, Deserialize)]
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
        }
    }
}

/// Provider-side prompt cache retention policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
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
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// Model identifier (e.g. "claude-sonnet-4-20250514").
    pub id: String,
    /// Optional display name.
    pub name: Option<String>,
    /// Max output tokens override.
    pub max_output_tokens: Option<u64>,
    /// Total context window size, in tokens.
    pub context_window: Option<u64>,
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
/// Scalar fields follow `#[serde(default)]` on [`CliSettings`] — anything the
/// user omits gets the built-in default automatically. The one special case
/// is `bind`: when the user writes a `bind: { … }` table, the built-in
/// key bindings are merged underneath so unmentioned keys stay bound to
/// their defaults instead of being dropped.
pub fn load_cli_settings_in(dirs: &TauDirs) -> Result<CliSettings, SettingsError> {
    let Some(ref dir) = dirs.config_dir else {
        return Ok(CliSettings::default());
    };
    let mut settings: CliSettings = load_json5_layered(dir, "cli")?;
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
    let Some(ref dir) = dirs.config_dir else {
        return Ok(HarnessSettings::default());
    };
    load_json5_layered(dir, "harness")
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

#[cfg(test)]
mod tests;
