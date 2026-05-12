use tempfile::TempDir;

use super::*;

#[test]
fn default_cli_settings_have_logo_enabled() {
    let s = CliSettings::default();
    assert!(s.greeting);
    assert!(s.show_logo);
    assert!(s.bar_cursor);
    assert_eq!(s.prompt_symbol, "◯");
    assert_eq!(s.submitted_prompt_symbol, "⬤");
}

#[test]
fn default_harness_settings_have_no_model() {
    let s = HarnessSettings::default();
    assert!(s.default_model.is_none());
    assert!(s.default_efforts.is_empty());
    assert_eq!(s.session_retention_days, 60);
}

#[test]
fn zero_session_retention_disables_cleanup() {
    let settings = HarnessSettings {
        session_retention_days: 0,
        ..HarnessSettings::default()
    };

    assert_eq!(settings.session_retention(), None);
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
    assert_eq!(s.prompt_symbol, "◯"); // default
    assert_eq!(s.submitted_prompt_symbol, "⬤"); // default
    assert_eq!(
        s.bind.get("C-o"),
        Some(&CliBindingAction {
            action: "shell-prompt-edit".to_owned(),
            command: "$TAU_EDITOR \"$TAU_PROMPT_PATH\"".to_owned(),
            trim: false,
        })
    );
}

#[test]
fn cli_settings_load_bindings() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.json5"),
        r#"{ bind: { "C-f": { action: "shell-prompt-insert", command: "fzf" } } }"#,
    )
    .expect("write");

    let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
    assert_eq!(
        s.bind.get("C-f"),
        Some(&CliBindingAction {
            action: "shell-prompt-insert".to_owned(),
            command: "fzf".to_owned(),
            trim: false,
        })
    );
}

#[test]
fn load_cli_settings_merges_builtin_bindings_with_user_overrides() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.json5"),
        r#"{ bind: { "C-f": { action: "shell-prompt-edit", command: "pick", trim: true } } }"#,
    )
    .expect("write");
    let dirs = TauDirs {
        config_dir: Some(dir.to_owned()),
        state_dir: None,
    };

    let s = load_cli_settings_in(&dirs).expect("load");
    assert_eq!(
        s.bind.get("C-f"),
        Some(&CliBindingAction {
            action: "shell-prompt-edit".to_owned(),
            command: "pick".to_owned(),
            trim: true,
        })
    );
    assert_eq!(
        s.bind.get("C-r").map(|binding| binding.action.as_str()),
        Some("shell-prompt-insert")
    );
    assert_eq!(
        s.bind.get("C-o").map(|binding| binding.action.as_str()),
        Some("shell-prompt-edit")
    );
}

#[test]
fn load_cli_settings_in_surfaces_scalar_field_overrides() {
    // Regression guard against re-introducing the manual field-by-field
    // copy in `load_cli_settings_in`. Any field with `#[serde(default)]`
    // on `CliSettings` should reach the caller without per-field
    // plumbing.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.json5"),
        r#"{ greeting: false, show_logo: false, bar_cursor: false, prompt_symbol: "λ", submitted_prompt_symbol: "✓" }"#,
    )
    .expect("write");
    let dirs = TauDirs {
        config_dir: Some(dir.to_owned()),
        state_dir: None,
    };

    let s = load_cli_settings_in(&dirs).expect("load");
    assert!(!s.greeting);
    assert!(!s.show_logo);
    assert!(!s.bar_cursor);
    assert_eq!(s.prompt_symbol, "λ");
    assert_eq!(s.submitted_prompt_symbol, "✓");
}

#[test]
fn cli_state_defaults_when_file_missing() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    let state = CliState::load(&dirs);
    assert_eq!(state, CliState::default());
    assert!(!state.show_diff);
    assert!(state.show_thinking);
    assert!(state.show_cache_stats);
}

#[test]
fn cli_state_round_trip_through_save_and_load() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    let original = CliState {
        show_diff: true,
        show_thinking: false,
        show_cache_stats: false,
        show_token_stats: true,
    };
    original.save(&dirs);
    assert!(td.path().join("cli.json").exists());
    let reloaded = CliState::load(&dirs);
    assert_eq!(reloaded, original);
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
    assert_eq!(s.prompt_symbol, "◯"); // default
    assert_eq!(s.submitted_prompt_symbol, "⬤"); // default
}

#[test]
fn cli_settings_can_customize_prompt_symbols() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.json5"),
        r#"{ prompt_symbol: "λ", submitted_prompt_symbol: "✓" }"#,
    )
    .expect("write");

    let s: CliSettings = load_json5_layered(dir, "cli").expect("load");
    assert_eq!(s.prompt_symbol, "λ");
    assert_eq!(s.submitted_prompt_symbol, "✓");
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
    assert_eq!(s.session_retention_days, 60);
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
                        promptCacheRetention: "24h",
                        compat: {
                            supportsPromptCacheKey: true,
                            supportsPromptCacheRetention: true,
                            supportsLlamaCppCache: true,
                        },
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
    assert_eq!(
        local.prompt_cache_retention,
        Some(PromptCacheRetention::Extended24h)
    );
    assert!(local.compat.supports_prompt_cache_key);
    assert!(local.compat.supports_prompt_cache_retention);
    assert!(local.compat.supports_llama_cpp_cache);
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
        include_str!("../../../../config/cli.json5"),
    )
    .expect("write cli");
    std::fs::write(
        dir.join("harness.json5"),
        include_str!("../../../../config/harness.json5"),
    )
    .expect("write harness");
    std::fs::write(
        dir.join("models.json5"),
        include_str!("../../../../config/models.json5"),
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

#[test]
fn add_provider_in_writes_typed_entry() {
    let tmp = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: Some(tmp.path().to_path_buf()),
        state_dir: None,
    };

    let mut provider = ProviderConfig {
        api: Some("openai-chat".to_owned()),
        auth: Some("api-key".to_owned()),
        ..Default::default()
    };
    provider.models.push(ModelConfig {
        id: "gpt-x".to_owned(),
        name: None,
        max_output_tokens: None,
        context_window: Some(123_456),
        supports_xhigh: None,
        reasoning_efforts: None,
    });

    let path = add_provider_in(&dirs, "openai", &provider).expect("add provider");
    assert!(path.ends_with("models.json5"));

    let registry = load_models_in(&dirs).expect("reload");
    let written = registry
        .providers
        .get("openai")
        .expect("openai entry present");
    assert_eq!(written.auth.as_deref(), Some("api-key"));
    assert_eq!(written.api.as_deref(), Some("openai-chat"));
    assert_eq!(written.models.len(), 1);
    assert_eq!(written.models[0].id, "gpt-x");
    assert_eq!(written.models[0].context_window, Some(123_456));
}

#[test]
fn add_provider_in_preserves_other_entries_and_unknown_fields() {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("models.json5");
    std::fs::write(
        &path,
        r#"{
            // user comment that WILL be lost (documented behavior)
            "extraTopLevelField": 42,
            "providers": {
                "keep": {
                    "auth": "none",
                    "api": "openai-completions",
                    "models": [],
                    "extraProviderField": "preserved"
                }
            }
        }"#,
    )
    .expect("seed file");

    let dirs = TauDirs {
        config_dir: Some(tmp.path().to_path_buf()),
        state_dir: None,
    };
    let new_provider = ProviderConfig {
        auth: Some("api-key".to_owned()),
        api: Some("openai-chat".to_owned()),
        ..Default::default()
    };
    add_provider_in(&dirs, "added", &new_provider).expect("add");

    let text = std::fs::read_to_string(&path).expect("read");
    let root: serde_json::Value = json5::from_str(&text).expect("parse");

    assert_eq!(root["extraTopLevelField"], 42);
    assert_eq!(root["providers"]["keep"]["extraProviderField"], "preserved");
    assert_eq!(root["providers"]["added"]["auth"], "api-key");
}

/// The xhigh whitelist drives the default for models that don't set
/// `supportsXhigh` explicitly. Lock the curated set so a future tweak
/// can't silently demote (or promote) a model that was working before.
#[test]
fn xhigh_whitelist_covers_known_openai_families() {
    // Full-size GPT-5 frontier models support xhigh.
    assert!(is_known_xhigh_model_id("gpt-5.5"));
    assert!(is_known_xhigh_model_id("gpt-5.5-2026-04-15"));
    assert!(is_known_xhigh_model_id("gpt-5.4"));
    assert!(is_known_xhigh_model_id("gpt-5.4-pro"));
    assert!(is_known_xhigh_model_id("gpt-5.3-codex"));
    assert!(is_known_xhigh_model_id("gpt-5.3-codex-spark"));
    assert!(is_known_xhigh_model_id("gpt-5.2"));
    assert!(is_known_xhigh_model_id("gpt-5.1-codex-max"));

    // mini / nano variants top out at `high`.
    assert!(!is_known_xhigh_model_id("gpt-5.5-mini"));
    assert!(!is_known_xhigh_model_id("gpt-5.4-mini"));
    assert!(!is_known_xhigh_model_id("gpt-5.4-nano"));
    assert!(!is_known_xhigh_model_id("gpt-5.2-mini"));

    // Older / unrelated families.
    assert!(!is_known_xhigh_model_id("o3-mini"));
    assert!(!is_known_xhigh_model_id("gpt-4.1"));
    assert!(!is_known_xhigh_model_id("claude-sonnet-4.6"));
    assert!(!is_known_xhigh_model_id("llama3.2:latest"));
    assert!(!is_known_xhigh_model_id(""));
}

/// Explicit `supportsXhigh` in models.json5 must win over the
/// built-in whitelist in either direction.
#[test]
fn model_supports_xhigh_explicit_override_wins() {
    let base = ModelConfig {
        id: "gpt-5.5".to_owned(),
        name: None,
        max_output_tokens: None,
        context_window: None,
        supports_xhigh: None,
        reasoning_efforts: None,
    };
    assert!(base.supports_xhigh(), "whitelist default for gpt-5.5");

    let forced_off = ModelConfig {
        supports_xhigh: Some(false),
        ..base.clone()
    };
    assert!(
        !forced_off.supports_xhigh(),
        "explicit `false` overrides the whitelist"
    );

    let forced_on = ModelConfig {
        id: "exotic-local-model".to_owned(),
        supports_xhigh: Some(true),
        ..base
    };
    assert!(
        forced_on.supports_xhigh(),
        "explicit `true` overrides the (absent) whitelist entry"
    );
}

/// `reasoningEfforts` parses as a list of effort levels using the
/// canonical snake_case wire form shared with `default_efforts`.
#[test]
fn models_json5_reasoning_efforts_override_parses() {
    use tau_proto::Effort;
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("models.json5"),
        r#"{
            providers: {
                openai: {
                    api: "openai-chat",
                    auth: "api-key",
                    apiKey: "test",
                    models: [
                        // gpt-5.4-pro accepts medium/high/xhigh only
                        // (no `off` or `low`/`minimal`) — the user
                        // pins exactly what the API will accept.
                        {
                            id: "gpt-5.4-pro",
                            reasoningEfforts: ["medium", "high", "xhigh"],
                        },
                        // No override: keep the defaults.
                        { id: "gpt-5.5" },
                    ],
                },
            },
        }"#,
    )
    .expect("write");

    let m: ModelRegistry = load_json5_layered(dir, "models").expect("load");
    let models = &m.providers["openai"].models;
    let pro = models.iter().find(|m| m.id == "gpt-5.4-pro").expect("pro");
    assert_eq!(
        pro.reasoning_efforts.as_deref(),
        Some(&[Effort::Medium, Effort::High, Effort::XHigh][..])
    );
    let v5 = models.iter().find(|m| m.id == "gpt-5.5").expect("v5");
    assert_eq!(v5.reasoning_efforts, None);
}

/// `supportsXhigh: true` should round-trip through json5.
#[test]
fn models_json5_supports_xhigh_field_parses() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("models.json5"),
        r#"{
            providers: {
                openai: {
                    api: "openai-chat",
                    auth: "api-key",
                    apiKey: "test",
                    models: [
                        { id: "gpt-5.4" },
                        { id: "weird-custom", supportsXhigh: true },
                        { id: "gpt-5.5", supportsXhigh: false },
                    ],
                },
            },
        }"#,
    )
    .expect("write");

    let m: ModelRegistry = load_json5_layered(dir, "models").expect("load");
    let models = &m.providers["openai"].models;
    let by_id = |id: &str| models.iter().find(|m| m.id == id).expect(id);
    assert_eq!(by_id("gpt-5.4").supports_xhigh, None);
    assert!(by_id("gpt-5.4").supports_xhigh(), "whitelist default");
    assert_eq!(by_id("weird-custom").supports_xhigh, Some(true));
    assert!(by_id("weird-custom").supports_xhigh());
    assert_eq!(by_id("gpt-5.5").supports_xhigh, Some(false));
    assert!(!by_id("gpt-5.5").supports_xhigh(), "explicit opt-out wins");
}

#[test]
fn remove_provider_in_returns_false_when_absent() {
    let tmp = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: Some(tmp.path().to_path_buf()),
        state_dir: None,
    };
    assert!(!remove_provider_in(&dirs, "missing").expect("ok"));

    let provider = ProviderConfig {
        auth: Some("none".to_owned()),
        ..Default::default()
    };
    add_provider_in(&dirs, "p1", &provider).expect("add");
    assert!(remove_provider_in(&dirs, "p1").expect("remove"));
    assert!(!remove_provider_in(&dirs, "p1").expect("removed twice"));
}
