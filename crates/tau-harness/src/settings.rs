//! Loading and resolving harness/extension configuration on startup.

use tau_config::Config;

/// Load `harness.json5` and fall back to defaults on parse error,
/// after writing a warning to stderr. Without the warning a malformed
/// file silently disables every user-configured extension and the
/// only symptom is "my extension isn't running" with no clue why.
pub(crate) fn load_harness_settings_or_warn(
    dirs: &tau_config::settings::TauDirs,
) -> tau_config::settings::HarnessSettings {
    match tau_config::settings::load_harness_settings_in(dirs) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "tau: failed to load harness.json5: {error}\ntau: falling back to default harness settings — extensions and model selection from harness.json5 will be ignored"
            );
            tau_config::settings::HarnessSettings::default()
        }
    }
}

/// The set of extensions the harness ships with by default.
///
/// Each entry's `command` is `[<current-exe>, "ext", <name>]`, so a
/// fresh `tau` install with no `harness.json5` runs the in-binary
/// core-agent and core-shell extensions out of the box. Users can override
/// individual fields (or set `enable: false`) per entry in
/// `harness.json5` under `extensions: { name: { … } }`.
#[must_use]
pub fn builtin_extensions() -> Vec<tau_config::settings::BuiltinExtension> {
    use tau_config::settings::BuiltinExtension;

    let tau_binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "tau".to_owned());

    vec![
        BuiltinExtension {
            name: "core-agent",
            command: vec![tau_binary.clone(), "ext".to_owned(), "agent".to_owned()],
            role: Some("agent"),
            enable: true,
            config: serde_json::json!({}),
        },
        BuiltinExtension {
            name: "core-shell",
            command: vec![tau_binary.clone(), "ext".to_owned(), "ext-shell".to_owned()],
            role: Some("tool"),
            enable: true,
            config: serde_json::json!({}),
        },
        BuiltinExtension {
            name: "test-dummy",
            command: vec![
                tau_binary.clone(),
                "ext".to_owned(),
                "ext-test-dummy".to_owned(),
            ],
            role: Some("tool"),
            enable: false,
            config: serde_json::json!({}),
        },
        BuiltinExtension {
            name: "dpc-notifications",
            command: vec![
                tau_binary.clone(),
                "ext".to_owned(),
                "ext-dpc-notifications".to_owned(),
            ],
            role: Some("tool"),
            enable: false,
            config: serde_json::json!({ "idle_seconds": 60 }),
        },
        BuiltinExtension {
            name: "core-websearch-exa",
            command: vec![
                tau_binary,
                "ext".to_owned(),
                "ext-websearch-exa".to_owned(),
            ],
            role: Some("tool"),
            enable: true,
            config: serde_json::json!({}),
        },
    ]
}

#[must_use]
pub fn default_config() -> Config {
    use tau_config::{Config, CoreConfig, CoreMode};

    let extensions = tau_config::settings::HarnessSettings::default()
        .resolve_extensions(builtin_extensions())
        .expect("built-in extensions resolve cleanly");

    Config {
        core: CoreConfig {
            mode: CoreMode::Embedded,
        },
        extensions,
    }
}

pub(crate) fn resolve_config(
    _explicit_path: Option<&std::path::Path>,
) -> Result<Config, Box<dyn std::error::Error>> {
    use tau_config::{Config, CoreConfig, CoreMode};

    // Extensions live in `harness.json5` under `extensions: { ... }`.
    // We start from the built-in core-agent + tools defaults and apply the
    // user's overrides on top; a malformed harness.json5 falls back
    // to defaults rather than failing the whole startup, but we warn
    // on stderr so the user can see why their config is being
    // ignored.
    let settings = load_harness_settings_or_warn(&tau_config::settings::TauDirs::default());
    let extensions = settings.resolve_extensions(builtin_extensions())?;
    Ok(Config {
        core: CoreConfig {
            mode: CoreMode::Embedded,
        },
        extensions,
    })
}
