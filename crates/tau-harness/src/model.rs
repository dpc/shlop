//! Model-registry helpers: loading the available model list, computing
//! valid effort levels per model, persisting the user's selection, and
//! gauging context-window usage.

use tau_proto::ModelId;

use crate::settings::{load_harness_settings_or_warn, load_models_or_warn};

/// Loaded model list plus the inputs used to build it. The two
/// `*_error` fields hold the parse error (if any) from the
/// corresponding config file — the harness emits them as
/// `Important` `HarnessInfo` once it can publish events, so a
/// malformed config doesn't silently fall back to defaults.
pub(crate) struct LoadedModelList {
    pub available: Vec<ModelId>,
    /// The model the harness will start in, if any. `None` means no
    /// providers / models are configured at all.
    pub selected: Option<ModelId>,
    pub model_registry: tau_config::settings::ModelRegistry,
    pub harness_settings: tau_config::settings::HarnessSettings,
    pub harness_settings_error: Option<tau_config::settings::SettingsError>,
    pub models_error: Option<tau_config::settings::SettingsError>,
}

/// Load model registry and harness settings, build the flat model list
/// and determine the initially selected model.
///
/// Priority: default_model from harness.json5 → last used from state →
/// first available → `None` (no model).
pub(crate) fn load_model_list(dirs: &tau_config::settings::TauDirs) -> LoadedModelList {
    let (model_registry, models_error) = load_models_or_warn(dirs);
    let (harness_settings, harness_settings_error) = load_harness_settings_or_warn(dirs);
    let mut available: Vec<ModelId> = Vec::new();
    for (provider_name, provider_cfg) in &model_registry.providers {
        for model in &provider_cfg.models {
            available.push(ModelId::new(provider_name.clone(), model.id.clone()));
        }
    }
    available.sort();
    let selected = harness_settings
        .default_model
        .as_ref()
        .filter(|m| available.contains(m))
        .cloned()
        .or_else(|| load_last_selected_model(dirs).filter(|m| available.contains(m)))
        .or_else(|| available.first().cloned());
    LoadedModelList {
        available,
        selected,
        model_registry,
        harness_settings,
        harness_settings_error,
        models_error,
    }
}

/// Returns the efforts valid for `model`.
///
/// Resolution order:
/// 1. Empty list when the model's provider isn't in the registry.
/// 2. Per-model `reasoningEfforts` (escape hatch): an authoritative list that
///    replaces both the canonical default set and the provider-level
///    `supportsReasoningEffort` flag.
/// 3. `[Off]` when the provider has `supportsReasoningEffort: false`.
/// 4. Otherwise the canonical `[Off, Minimal, Low, Medium, High]` set, plus
///    `XHigh` when the model opts in via per-model `supportsXhigh` or
///    [`tau_config::settings::is_known_xhigh_model_id`].
pub(crate) fn efforts_for_model(
    registry: &tau_config::settings::ModelRegistry,
    model: &ModelId,
) -> Vec<tau_proto::Effort> {
    use tau_proto::Effort as L;
    let Some(provider) = registry.providers.get(&model.provider) else {
        return Vec::new();
    };
    let model_cfg = provider.models.iter().find(|m| m.id == model.model);
    if let Some(custom) = model_cfg.and_then(|m| m.reasoning_efforts.as_ref()) {
        // Authoritative override — preserve user-specified order
        // but drop duplicates so the cycle helper doesn't loop.
        let mut seen = std::collections::BTreeSet::new();
        return custom
            .iter()
            .copied()
            .filter(|level| seen.insert(*level))
            .collect();
    }
    if !provider.compat.supports_reasoning_effort {
        return vec![L::Off];
    }
    let mut levels = vec![L::Off, L::Minimal, L::Low, L::Medium, L::High];
    if model_cfg.is_some_and(tau_config::settings::ModelConfig::supports_xhigh) {
        levels.push(L::XHigh);
    }
    levels
}

pub(crate) fn model_context_window(
    registry: &tau_config::settings::ModelRegistry,
    model: &ModelId,
) -> Option<u64> {
    let provider = registry.providers.get(&model.provider)?;
    provider
        .models
        .iter()
        .find(|candidate| candidate.id == model.model)
        .and_then(|candidate| candidate.context_window)
}

pub(crate) fn context_percent_used(input_tokens: u64, context_window: u64) -> u8 {
    if context_window == 0 {
        return 0;
    }
    let percent = input_tokens.saturating_mul(100) / context_window;
    percent.min(100) as u8
}

pub(crate) fn clamp_effort(
    requested: tau_proto::Effort,
    allowed: &[tau_proto::Effort],
) -> tau_proto::Effort {
    use tau_proto::Effort as L;
    if allowed.contains(&requested) {
        return requested;
    }
    // Graceful degradation for `xhigh` on models that don't expose
    // it: fall back to `high` rather than all the way to `off`, so
    // `/effort xhigh` on (say) `gpt-5.4-mini` still produces a
    // sensible reasoning level instead of silently disabling
    // reasoning. Mirrors Pi's behaviour.
    if requested == L::XHigh && allowed.contains(&L::High) {
        return L::High;
    }
    if allowed.contains(&L::Off) {
        return L::Off;
    }
    allowed.first().copied().unwrap_or(L::Off)
}

fn parse_effort(value: &str) -> Option<tau_proto::Effort> {
    value.parse().ok()
}

fn load_last_efforts(
    dirs: &tau_config::settings::TauDirs,
) -> std::collections::HashMap<ModelId, tau_proto::Effort> {
    let Some(path) = dirs.state_dir.as_ref().map(|d| d.join("harness.json5")) else {
        return std::collections::HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return std::collections::HashMap::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return std::collections::HashMap::new();
    };

    let mut levels = std::collections::HashMap::new();
    if let Some(map) = json["last_efforts"].as_object() {
        for (model, level) in map {
            let Ok(model) = model.parse::<ModelId>() else {
                // Skip entries persisted with a malformed id rather
                // than failing the whole load — the on-disk state file
                // is best-effort UX, not a contract.
                continue;
            };
            let Some(level) = level.as_str().and_then(parse_effort) else {
                continue;
            };
            levels.insert(model, level);
        }
    }

    levels
}

pub(crate) fn selected_effort_for_model(
    dirs: &tau_config::settings::TauDirs,
    harness_settings: &tau_config::settings::HarnessSettings,
    registry: &tau_config::settings::ModelRegistry,
    model: &ModelId,
) -> tau_proto::Effort {
    let allowed = efforts_for_model(registry, model);
    let requested = harness_settings
        .default_efforts
        .get(model)
        .copied()
        .or_else(|| load_last_efforts(dirs).remove(model))
        .unwrap_or_else(|| middle_effort(&allowed));
    clamp_effort(requested, &allowed)
}

/// Pick the middle element of `allowed`, or `Off` for an empty list.
/// First-time users (no `default_efforts` entry, no persisted last
/// effort) get a sensible reasoning level instead of always landing on
/// `Off` — for the standard `[Off, Minimal, Low, Medium, High]` list
/// that's `Low`. Returns `Off` for `[Off]`-only providers and the
/// empty case.
pub(crate) fn middle_effort(allowed: &[tau_proto::Effort]) -> tau_proto::Effort {
    if allowed.is_empty() {
        return tau_proto::Effort::Off;
    }
    allowed[allowed.len() / 2]
}

/// Load the last-selected model from `<state_dir>/harness.json5`.
/// Returns `None` if the file is missing, malformed, or the saved id
/// no longer parses as a `provider/model`.
fn load_last_selected_model(dirs: &tau_config::settings::TauDirs) -> Option<ModelId> {
    let path = dirs.state_dir.as_ref()?.join("harness.json5");
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json["last_selected_model"].as_str()?.parse().ok()
}

/// Persist model + effort to `<state_dir>/harness.json5`. `model: None`
/// records that no model is currently selected.
pub(crate) fn save_harness_state(
    dirs: &tau_config::settings::TauDirs,
    model: Option<&ModelId>,
    effort: tau_proto::Effort,
) {
    let Some(dir) = dirs.state_dir.as_ref() else {
        return;
    };
    let path = dir.join("harness.json5");
    let _ = std::fs::create_dir_all(dir);
    let mut last_efforts = load_last_efforts(dirs);
    if let Some(model) = model {
        last_efforts.insert(model.clone(), effort);
    }
    let effort_json = last_efforts
        .into_iter()
        .map(|(model, level)| {
            (
                model.to_string(),
                serde_json::Value::String(level.as_str().to_owned()),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let json = serde_json::json!({
        "last_selected_model": model.map(ModelId::to_string).unwrap_or_default(),
        "last_efforts": effort_json,
    });
    let _ = serde_json::to_string_pretty(&json)
        .ok()
        .and_then(|s| std::fs::write(&path, s).ok());
}
