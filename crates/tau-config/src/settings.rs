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
}

impl Default for CliSettings {
    fn default() -> Self {
        Self {
            greeting: true,
            show_logo: true,
        }
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
}

impl Default for HarnessSettings {
    fn default() -> Self {
        Self {
            default_model: None,
        }
    }
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
#[serde(default)]
pub struct ProviderConfig {
    /// Base URL for the API endpoint.
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    /// API protocol: "anthropic", "openai-completions", etc.
    pub api: Option<String>,
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

/// Loads CLI settings from `~/.config/tau/cli.json5` with
/// `cli.d/*.json5` overrides.
pub fn load_cli_settings() -> Result<CliSettings, SettingsError> {
    let Some(dir) = config_dir() else {
        return Ok(CliSettings::default());
    };
    load_json5_layered(&dir, "cli")
}

/// Loads harness settings from `~/.config/tau/harness.json5` with
/// `harness.d/*.json5` overrides.
pub fn load_harness_settings() -> Result<HarnessSettings, SettingsError> {
    let Some(dir) = config_dir() else {
        return Ok(HarnessSettings::default());
    };
    load_json5_layered(&dir, "harness")
}

/// Loads the model registry from `~/.config/tau/models.json5` with
/// `models.d/*.json5` overrides.
pub fn load_models() -> Result<ModelRegistry, SettingsError> {
    let Some(dir) = config_dir() else {
        return Ok(ModelRegistry::default());
    };
    load_json5_layered(&dir, "models")
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

    // Base file (optional).
    if base_path.exists() {
        builder = builder.add_source(
            config::File::from(base_path)
                .format(config::FileFormat::Json5)
                .required(false),
        );
    }

    // Drop-in directory (optional, sorted).
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
                    .required(false),
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
    }

    #[test]
    fn default_harness_settings_have_no_model() {
        let s = HarnessSettings::default();
        assert!(s.default_model.is_none());
    }

    #[test]
    fn cli_settings_load_from_json5_file() {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(dir.join("cli.json5"), r#"{ greeting: false }"#).expect("write");

        let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
        assert!(!s.greeting);
        assert!(s.show_logo); // default
    }

    #[test]
    fn harness_settings_load_from_json5_file() {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(
            dir.join("harness.json5"),
            r#"{ default_model: "anthropic/claude-sonnet-4-20250514" }"#,
        )
        .expect("write");

        let s: HarnessSettings = load_json5_layered(dir, "harness").expect("load");
        assert_eq!(
            s.default_model.as_deref(),
            Some("anthropic/claude-sonnet-4-20250514")
        );
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
