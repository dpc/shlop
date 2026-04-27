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

use serde::Deserialize;

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
}

impl Default for CliSettings {
    fn default() -> Self {
        Self {
            greeting: true,
            show_logo: true,
            bar_cursor: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Harness settings
// ---------------------------------------------------------------------------

/// Harness/agent settings loaded from `harness.json5`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct HarnessSettings {
    /// Default model provider/model to use (e.g.
    /// "anthropic/claude-sonnet-4-20250514").
    pub default_model: Option<String>,

    /// Default effort per model (`provider/model` -> level).
    pub default_efforts: HashMap<String, tau_proto::Effort>,

    /// Extension table, keyed by name. Built-in entries (`agent`,
    /// `shell`) come pre-baked at the harness level; anything the
    /// user writes here overrides those per-field, or adds a new
    /// extension.
    ///
    /// Example `harness.json5`:
    /// ```json5
    /// {
    ///   extensions: {
    ///     // disable the built-in shell extension
    ///     shell: { enable: false },
    ///     // run the agent through ssh on a remote box
    ///     agent: { prefix: ["ssh", "user@host"] },
    ///     // a third-party extension
    ///     mything: { command: ["/usr/local/bin/my-tau-ext"] },
    ///   },
    /// }
    /// ```
    pub extensions: HashMap<String, ExtensionEntry>,
}

/// One entry in the harness's `extensions` map.
///
/// All fields are optional in the on-disk form so users can override
/// just the fields they care about for built-in extensions; the
/// harness merges these with built-in defaults at startup.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionEntry {
    /// argv prefix prepended before `command`. Useful for wrappers
    /// that don't change the inner command, e.g.
    /// `["ssh", "user@host"]` to run remotely or
    /// `["bwrap", "--ro-bind", "/", "/", "--"]` to sandbox.
    pub prefix: Vec<String>,

    /// argv of the extension itself. `command[0]` is the executable;
    /// the rest are arguments. For built-in extensions this defaults
    /// to `[<current-exe>, "ext", <name>]`; for new entries
    /// this must be set explicitly.
    pub command: Vec<String>,

    /// Whether to run this extension. Defaults to `true`. Set to
    /// `false` to keep the entry in config but skip spawning.
    pub enable: bool,

    /// Role tag. Exactly one extension must have `role: "agent"`.
    /// Built-in `agent` defaults to that; everything else is treated
    /// as a tool extension.
    pub role: Option<String>,

    /// Free-form configuration object handed to the extension at
    /// startup via `LifecycleConfigure`. The harness does not
    /// interpret it — the extension defines and validates its own
    /// schema. Defaults to an empty object so extensions can rely
    /// on always seeing a value.
    pub config: serde_json::Value,
}

impl Default for ExtensionEntry {
    fn default() -> Self {
        // `enable: true` so a user writing
        // `extensions: { foo: { command: [...] } }` doesn't need to
        // also write `enable: true` for the entry to actually run.
        Self {
            prefix: Vec::new(),
            command: Vec::new(),
            enable: true,
            role: None,
            config: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// Built-in extension shipped with `tau`. Used by
/// [`HarnessSettings::resolve_extensions`] to seed the table before
/// applying user overrides.
pub struct BuiltinExtension {
    pub name: &'static str,
    pub command: Vec<String>,
    pub role: Option<&'static str>,
    pub enable: bool,
    /// Built-in default config for this extension, merged below any
    /// user-provided `config: { … }` object in `harness.json5`.
    pub config: serde_json::Value,
}

impl HarnessSettings {
    /// Merge user-provided `extensions` entries on top of the
    /// supplied built-in extensions and produce a flat list of
    /// [`crate::ExtensionConfig`]s ready for the harness to spawn.
    ///
    /// Per-key merging:
    /// - Field-level overlay for built-in keys: any field the user set replaces
    ///   the built-in's value; unset fields keep the built-in's defaults.
    /// - User keys not in the built-in list are added as-is. They must specify
    ///   a non-empty `command`.
    /// - Entries with `enable: false` are dropped.
    ///
    /// Returns `Err` for entries that end up with an empty `command`
    /// after the merge — only possible for user-added unknown keys.
    pub fn resolve_extensions(
        &self,
        builtins: Vec<BuiltinExtension>,
    ) -> Result<Vec<crate::ExtensionConfig>, ResolveExtensionsError> {
        // Pass 1: seed an indexed map with built-ins, in order.
        let mut order: Vec<String> = builtins.iter().map(|b| b.name.to_owned()).collect();
        let mut entries: HashMap<String, ResolvedExtension> = builtins
            .into_iter()
            .map(|b| {
                (
                    b.name.to_owned(),
                    ResolvedExtension {
                        prefix: Vec::new(),
                        command: b.command,
                        enable: b.enable,
                        role: b.role.map(str::to_owned),
                        config: b.config,
                    },
                )
            })
            .collect();

        // Pass 2: overlay user entries. Sort user keys deterministically.
        let mut user_keys: Vec<&String> = self.extensions.keys().collect();
        user_keys.sort();
        for name in user_keys {
            let user = &self.extensions[name];
            match entries.get_mut(name) {
                Some(existing) => {
                    if !user.prefix.is_empty() {
                        existing.prefix = user.prefix.clone();
                    }
                    if !user.command.is_empty() {
                        existing.command = user.command.clone();
                    }
                    existing.enable = user.enable;
                    if user.role.is_some() {
                        existing.role = user.role.clone();
                    }
                    // Config: user object overlays builtin object
                    // field-by-field; non-object user values replace
                    // the builtin entirely. This lets a user override
                    // `idle_seconds` without re-stating the rest of
                    // the builtin defaults.
                    existing.config = merge_json(existing.config.take(), user.config.clone());
                }
                None => {
                    if user.command.is_empty() {
                        return Err(ResolveExtensionsError::EmptyCommand(name.clone()));
                    }
                    order.push(name.clone());
                    entries.insert(
                        name.clone(),
                        ResolvedExtension {
                            prefix: user.prefix.clone(),
                            command: user.command.clone(),
                            enable: user.enable,
                            role: user.role.clone(),
                            config: user.config.clone(),
                        },
                    );
                }
            }
        }

        // Pass 3: produce ExtensionConfigs in declared order, dropping
        // disabled entries. argv = prefix ++ command; argv[0] is the
        // executable, rest are args.
        let mut out = Vec::new();
        for name in order {
            let entry = entries.remove(&name).expect("seeded above");
            if !entry.enable {
                continue;
            }
            let mut argv = entry.prefix;
            argv.extend(entry.command);
            let (program, args) = match argv.split_first() {
                Some((first, rest)) => (first.clone(), rest.to_vec()),
                None => return Err(ResolveExtensionsError::EmptyCommand(name)),
            };
            out.push(crate::ExtensionConfig {
                name,
                command: program,
                args,
                role: entry.role,
                config: entry.config,
            });
        }
        Ok(out)
    }
}

#[derive(Debug)]
struct ResolvedExtension {
    prefix: Vec<String>,
    command: Vec<String>,
    enable: bool,
    role: Option<String>,
    config: serde_json::Value,
}

/// Merge `over` on top of `base` for extension config objects.
///
/// When both are JSON objects, keys are merged shallowly:
/// `over`'s keys win, `base`'s keys are kept where `over` doesn't
/// mention them. For any other shape (one side isn't an object),
/// `over` replaces `base` outright if it isn't `Null`. This is the
/// minimum needed to let a user override one field of a builtin's
/// config without restating the rest.
fn merge_json(base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
    match (base, over) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                b.insert(k, v);
            }
            serde_json::Value::Object(b)
        }
        (base, serde_json::Value::Null) => base,
        (_, over) => over,
    }
}

/// Error returned by [`HarnessSettings::resolve_extensions`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveExtensionsError {
    /// A user-added extension entry has no `command` (and therefore
    /// no executable to spawn).
    EmptyCommand(String),
}

impl fmt::Display for ResolveExtensionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommand(name) => write!(
                f,
                "extension {name:?} has no `command` set; user-added entries must specify the executable",
            ),
        }
    }
}

impl std::error::Error for ResolveExtensionsError {}

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
#[serde(default)]
pub struct ProviderConfig {
    /// Base URL for the API endpoint.
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    /// API protocol: "anthropic", "openai-completions", etc.
    pub api: Option<String>,
    /// Authentication method: "api-key" (default when `apiKey` is set),
    /// "openai-codex", "github-copilot", or "none".
    pub auth: Option<String>,
    /// API key or environment variable name. Prefix with `!` for
    /// shell command execution (Pi convention).
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    /// Extra HTTP headers (key → value or env var name).
    pub headers: Option<HashMap<String, String>>,
    /// Compatibility flags for non-standard providers.
    #[serde(default)]
    pub compat: ProviderCompat,
    /// Models available from this provider.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// Compatibility flags for providers that don't support all features.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ProviderCompat {
    #[serde(rename = "supportsDeveloperRole")]
    pub supports_developer_role: bool,
    #[serde(rename = "supportsReasoningEffort")]
    pub supports_reasoning_effort: bool,
    #[serde(rename = "supportsPrefill")]
    pub supports_prefill: bool,
}

impl Default for ProviderCompat {
    fn default() -> Self {
        Self {
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_prefill: true,
        }
    }
}

/// One model available from a provider.
#[derive(Clone, Debug, Deserialize)]
pub struct ModelConfig {
    /// Model identifier (e.g. "claude-sonnet-4-20250514").
    pub id: String,
    /// Optional display name.
    pub name: Option<String>,
    /// Max output tokens override.
    #[serde(rename = "maxOutputTokens")]
    pub max_output_tokens: Option<u64>,
    /// Total context window size, in tokens.
    #[serde(rename = "contextWindow")]
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
    /// Where to read/write runtime state like `harness-state.json`.
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
pub fn load_cli_settings_in(dirs: &TauDirs) -> Result<CliSettings, SettingsError> {
    let Some(ref dir) = dirs.config_dir else {
        return Ok(CliSettings::default());
    };
    load_json5_layered(dir, "cli")
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

    // Base file is optional, but parse errors must surface.
    // We guard on exists() and use required(true) so a missing file
    // is fine but a malformed one is an error.
    if base_path.exists() {
        builder = builder.add_source(
            config::File::from(base_path)
                .format(config::FileFormat::Json5)
                .required(true),
        );
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
        }
    }

    let config = builder.build()?;

    // If no sources were added, return default.
    if config.cache.kind == config::ValueKind::Nil {
        return Ok(T::default());
    }

    config.try_deserialize().map_err(SettingsError::from)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn default_cli_settings_have_logo_enabled() {
        let s = CliSettings::default();
        assert!(s.greeting);
        assert!(s.show_logo);
        assert!(s.bar_cursor);
    }

    #[test]
    fn default_harness_settings_have_no_model() {
        let s = HarnessSettings::default();
        assert!(s.default_model.is_none());
        assert!(s.default_efforts.is_empty());
    }

    #[test]
    fn cli_settings_load_from_json5_file() {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(dir.join("cli.json5"), r#"{ greeting: false }"#).expect("write");

        let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
        assert!(!s.greeting);
        assert!(s.show_logo); // default
        assert!(s.bar_cursor); // default
    }

    #[test]
    fn cli_settings_can_disable_bar_cursor() {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(dir.join("cli.json5"), r#"{ bar_cursor: false }"#).expect("write");

        let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
        assert!(!s.bar_cursor);
        assert!(s.greeting); // default
        assert!(s.show_logo); // default
    }

    #[test]
    fn harness_settings_load_from_json5_file() {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(
            dir.join("harness.json5"),
            r#"{
                default_model: "anthropic/claude-sonnet-4-20250514",
                default_efforts: {
                    "anthropic/claude-sonnet-4-20250514": "high",
                },
            }"#,
        )
        .expect("write");

        let s: HarnessSettings = load_json5_layered(dir, "harness").expect("load");
        assert_eq!(
            s.default_model.as_deref(),
            Some("anthropic/claude-sonnet-4-20250514")
        );
        assert_eq!(
            s.default_efforts
                .get("anthropic/claude-sonnet-4-20250514")
                .copied(),
            Some(tau_proto::Effort::High)
        );
    }

    fn builtins() -> Vec<BuiltinExtension> {
        vec![
            BuiltinExtension {
                name: "agent",
                command: vec!["tau".into(), "ext".into(), "agent".into()],
                role: Some("agent"),
                enable: true,
                config: serde_json::json!({}),
            },
            BuiltinExtension {
                name: "shell",
                command: vec!["tau".into(), "ext".into(), "ext-shell".into()],
                role: Some("tool"),
                enable: true,
                config: serde_json::json!({}),
            },
            BuiltinExtension {
                name: "test_dummy",
                command: vec!["tau".into(), "ext".into(), "ext-test-dummy".into()],
                role: Some("tool"),
                enable: false,
                config: serde_json::json!({}),
            },
            BuiltinExtension {
                name: "dpc_notifications",
                command: vec!["tau".into(), "ext".into(), "ext-dpc-notifications".into()],
                role: Some("tool"),
                enable: false,
                config: serde_json::json!({ "idle_seconds": 60 }),
            },
        ]
    }

    #[test]
    fn resolve_extensions_returns_builtins_when_user_config_empty() {
        let s = HarnessSettings::default();
        let resolved = s.resolve_extensions(builtins()).expect("resolve");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "agent");
        assert_eq!(resolved[0].command, "tau");
        assert_eq!(resolved[0].args, vec!["ext", "agent"]);
        assert_eq!(resolved[0].role.as_deref(), Some("agent"));
        assert_eq!(resolved[1].name, "shell");
    }

    #[test]
    fn resolve_extensions_builtin_can_start_disabled() {
        let s = HarnessSettings::default();
        let resolved = s.resolve_extensions(builtins()).expect("resolve");
        assert!(resolved.iter().all(|e| e.name != "test_dummy"));
    }

    #[test]
    fn resolve_extensions_disable_drops_entry() {
        let mut s = HarnessSettings::default();
        s.extensions.insert(
            "shell".into(),
            ExtensionEntry {
                enable: false,
                ..Default::default()
            },
        );
        let resolved = s.resolve_extensions(builtins()).expect("resolve");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "agent");
    }

    #[test]
    fn resolve_extensions_prefix_wraps_builtin_command() {
        let mut s = HarnessSettings::default();
        s.extensions.insert(
            "agent".into(),
            ExtensionEntry {
                prefix: vec!["ssh".into(), "user@host".into()],
                ..Default::default()
            },
        );
        let resolved = s.resolve_extensions(builtins()).expect("resolve");
        let agent = resolved.iter().find(|e| e.name == "agent").expect("agent");
        // argv[0] is the wrapper; original command moves into args.
        assert_eq!(agent.command, "ssh");
        assert_eq!(agent.args, vec!["user@host", "tau", "ext", "agent"]);
    }

    #[test]
    fn resolve_extensions_user_command_replaces_builtin_command() {
        let mut s = HarnessSettings::default();
        s.extensions.insert(
            "agent".into(),
            ExtensionEntry {
                command: vec!["/usr/local/bin/my-agent".into(), "--flag".into()],
                ..Default::default()
            },
        );
        let resolved = s.resolve_extensions(builtins()).expect("resolve");
        let agent = resolved.iter().find(|e| e.name == "agent").expect("agent");
        assert_eq!(agent.command, "/usr/local/bin/my-agent");
        assert_eq!(agent.args, vec!["--flag"]);
        // Role is preserved from the built-in default.
        assert_eq!(agent.role.as_deref(), Some("agent"));
    }

    #[test]
    fn resolve_extensions_adds_user_extension_keys() {
        let mut s = HarnessSettings::default();
        s.extensions.insert(
            "mything".into(),
            ExtensionEntry {
                command: vec!["/usr/local/bin/mything".into()],
                ..Default::default()
            },
        );
        let resolved = s.resolve_extensions(builtins()).expect("resolve");
        assert_eq!(resolved.len(), 3);
        let mything = resolved
            .iter()
            .find(|e| e.name == "mything")
            .expect("mything");
        assert_eq!(mything.command, "/usr/local/bin/mything");
        assert!(mything.role.is_none());
    }

    #[test]
    fn resolve_extensions_user_extension_without_command_errors() {
        let mut s = HarnessSettings::default();
        s.extensions.insert(
            "broken".into(),
            ExtensionEntry {
                ..Default::default()
            },
        );
        let err = s.resolve_extensions(builtins()).expect_err("must err");
        match err {
            ResolveExtensionsError::EmptyCommand(name) => assert_eq!(name, "broken"),
        }
    }

    #[test]
    fn resolve_extensions_loads_from_json5() {
        // End-to-end: a realistic harness.json5 round-trips through
        // load_json5_layered into the resolver.
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(
            dir.join("harness.json5"),
            r#"{
                extensions: {
                    shell: { enable: false },
                    test_dummy: { enable: true },
                    agent: { prefix: ["ssh", "host"] },
                    mything: { command: ["/bin/foo"] },
                },
            }"#,
        )
        .expect("write");

        let s: HarnessSettings = load_json5_layered(dir, "harness").expect("load");
        let resolved = s.resolve_extensions(builtins()).expect("resolve");
        let names: Vec<&str> = resolved.iter().map(|e| e.name.as_str()).collect();
        // shell dropped (disable). test_dummy enabled. agent kept
        // (prefix-wrapped). mything appended.
        assert_eq!(names, vec!["agent", "test_dummy", "mything"]);
        let agent = &resolved[0];
        assert_eq!(agent.command, "ssh");
        assert_eq!(agent.args, vec!["host", "tau", "ext", "agent"]);
    }

    #[test]
    fn drop_in_overrides_base() {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(dir.join("cli.json5"), r#"{ greeting: true }"#).expect("write");
        std::fs::create_dir(dir.join("cli.d")).expect("mkdir");
        std::fs::write(
            dir.join("cli.d").join("01-override.json5"),
            r#"{ greeting: false }"#,
        )
        .expect("write");

        let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
        assert!(!s.greeting);
    }

    #[test]
    fn models_load_with_providers() {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(
            dir.join("models.json5"),
            r#"{
                providers: {
                    local: {
                        baseUrl: "http://localhost:8080/v1",
                        api: "openai-completions",
                        apiKey: "test",
                        models: [{ id: "llama-3" }]
                    }
                }
            }"#,
        )
        .expect("write");

        let m: ModelRegistry = load_json5_layered(dir, "models").expect("load");
        assert_eq!(m.providers.len(), 1);
        let local = &m.providers["local"];
        assert_eq!(local.base_url.as_deref(), Some("http://localhost:8080/v1"));
        assert_eq!(local.models.len(), 1);
        assert_eq!(local.models[0].id, "llama-3");
    }

    #[test]
    fn missing_files_return_defaults() {
        let td = TempDir::new().expect("tempdir");
        let s: CliSettings = load_json5_layered(td.path(), "cli").expect("load");
        assert!(s.greeting);
        let h: HarnessSettings = load_json5_layered(td.path(), "harness").expect("load");
        assert!(h.default_model.is_none());
        assert!(h.default_efforts.is_empty());
        let m: ModelRegistry = load_json5_layered(td.path(), "models").expect("load");
        assert!(m.providers.is_empty());
    }

    #[test]
    fn sample_configs_deserialize() {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();

        std::fs::write(
            dir.join("cli.json5"),
            include_str!("../../../config/cli.json5"),
        )
        .expect("write cli");
        std::fs::write(
            dir.join("harness.json5"),
            include_str!("../../../config/harness.json5"),
        )
        .expect("write harness");
        std::fs::write(
            dir.join("models.json5"),
            include_str!("../../../config/models.json5"),
        )
        .expect("write models");

        let _cli: CliSettings = load_json5_layered(dir, "cli").expect("cli sample should parse");
        let _harness: HarnessSettings =
            load_json5_layered(dir, "harness").expect("harness sample should parse");
        let models: ModelRegistry =
            load_json5_layered(dir, "models").expect("models sample should parse");
        assert!(
            models.providers.contains_key("local"),
            "sample models should contain 'local' provider"
        );
    }
}
