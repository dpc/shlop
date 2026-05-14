//! Model-registry helpers: loading the available model list, computing
//! valid effort/verbosity/thinking-summary levels per model, persisting
//! the user's selection, and gauging context-window usage.

use std::collections::HashMap;

use tau_config::settings::AgentRole;
use tau_proto::{ModelId, ModelParams};

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
    pub selected_role: Option<String>,
    pub roles: HashMap<String, AgentRole>,
    pub role_overrides: HashMap<String, AgentRole>,
    pub model_registry: tau_config::settings::ModelRegistry,
    pub harness_settings: tau_config::settings::HarnessSettings,
    pub harness_settings_error: Option<tau_config::settings::SettingsError>,
    pub models_error: Option<tau_config::settings::SettingsError>,
}

/// Load model registry and harness settings, build the flat model list
/// and determine the initially selected role/model.
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
    let role_overrides = load_role_overrides(dirs);
    let mut roles = model_registry.default_roles.clone();
    for (name, role) in &role_overrides {
        roles.insert(name.clone(), role.clone());
    }
    let selected_role = load_last_selected_role(dirs)
        .filter(|role| roles.contains_key(role))
        .or_else(|| roles.contains_key("default").then(|| "default".to_owned()))
        .or_else(|| roles.keys().next().cloned());
    let selected = selected_role
        .as_ref()
        .and_then(|role| model_for_role(&roles, role, &available))
        .or_else(|| {
            harness_settings
                .default_model
                .as_ref()
                .filter(|m| available.contains(m))
                .cloned()
        })
        .or_else(|| load_last_selected_model(dirs).filter(|m| available.contains(m)))
        .or_else(|| available.first().cloned());
    LoadedModelList {
        available,
        selected,
        selected_role,
        roles,
        role_overrides,
        model_registry,
        harness_settings,
        harness_settings_error,
        models_error,
    }
}

pub(crate) fn model_for_role(
    roles: &HashMap<String, AgentRole>,
    role: &str,
    available: &[ModelId],
) -> Option<ModelId> {
    let model = roles
        .get(role)
        .and_then(|r| r.model.clone())
        .or_else(|| {
            (role != "default")
                .then(|| roles.get("default")?.model.clone())
                .flatten()
        })
        .or_else(|| available.first().cloned())?;
    available.contains(&model).then_some(model)
}

pub(crate) fn selected_params_for_role(
    registry: &tau_config::settings::ModelRegistry,
    roles: &HashMap<String, AgentRole>,
    role: &str,
    model: &ModelId,
) -> ModelParams {
    let allowed_effort = efforts_for_model(registry, model);
    let allowed_verbosity = verbosities_for_model(registry, model);
    let allowed_thinking = thinking_summaries_for_model(registry, model);
    let current = roles.get(role);
    let default = (role != "default").then(|| roles.get("default")).flatten();
    let effort = current
        .and_then(|r| r.effort)
        .or_else(|| default.and_then(|r| r.effort))
        .unwrap_or_else(|| middle_effort(&allowed_effort));
    let verbosity = current
        .and_then(|r| r.verbosity)
        .or_else(|| default.and_then(|r| r.verbosity))
        .unwrap_or_else(|| default_verbosity(&allowed_verbosity));
    let thinking_summary = current
        .and_then(|r| r.thinking_summary)
        .or_else(|| default.and_then(|r| r.thinking_summary))
        .unwrap_or_else(|| default_thinking_summary(&allowed_thinking));
    let service_tier = current
        .and_then(|r| r.fast_mode)
        .or_else(|| default.and_then(|r| r.fast_mode))
        .map(|enabled| enabled.then_some(tau_proto::ServiceTier::Fast))
        .unwrap_or_else(|| {
            current
                .and_then(|r| r.service_tier)
                .or_else(|| default.and_then(|r| r.service_tier))
        });

    ModelParams {
        effort: clamp_effort(effort, &allowed_effort),
        verbosity: clamp_verbosity(verbosity, &allowed_verbosity),
        thinking_summary: clamp_thinking_summary(thinking_summary, &allowed_thinking),
        service_tier,
    }
}

pub(crate) fn describe_role(
    registry: &tau_config::settings::ModelRegistry,
    roles: &HashMap<String, AgentRole>,
    role: &str,
    available: &[ModelId],
) -> String {
    let Some(model) = model_for_role(roles, role, available) else {
        return "no model".to_owned();
    };
    let params = selected_params_for_role(registry, roles, role, &model);
    let fast = if matches!(params.service_tier, Some(tau_proto::ServiceTier::Fast)) {
        ", fast"
    } else {
        ""
    };
    format!(
        "model={}, effort={}, verbosity={}, thinking-summary={}{}",
        model, params.effort, params.verbosity, params.thinking_summary, fast
    )
}

pub(crate) fn role_infos(
    registry: &tau_config::settings::ModelRegistry,
    roles: &HashMap<String, AgentRole>,
    available: &[ModelId],
) -> Vec<tau_proto::HarnessRoleInfo> {
    let mut out: Vec<_> = roles
        .keys()
        .map(|name| tau_proto::HarnessRoleInfo {
            name: name.clone(),
            description: describe_role(registry, roles, name, available),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
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

/// Returns the verbosity levels valid for `model`.
///
/// Resolution order:
/// 1. Empty list when the model's provider isn't in the registry.
/// 2. Per-model `verbosities` (escape hatch).
/// 3. `[Medium]` when neither the per-model `supportsVerbosity` nor the
///    provider-level `supportsVerbosity` flag is true — the medium-only
///    "pinned" set is harmless to send and keeps the status bar rendering
///    uniform.
/// 4. Otherwise the canonical `[Low, Medium, High]` set.
pub(crate) fn verbosities_for_model(
    registry: &tau_config::settings::ModelRegistry,
    model: &ModelId,
) -> Vec<tau_proto::Verbosity> {
    use tau_proto::Verbosity as V;
    let Some(provider) = registry.providers.get(&model.provider) else {
        return Vec::new();
    };
    let model_cfg = provider.models.iter().find(|m| m.id == model.model);
    if let Some(custom) = model_cfg.and_then(|m| m.verbosities.as_ref()) {
        let mut seen = std::collections::BTreeSet::new();
        return custom
            .iter()
            .copied()
            .filter(|level| seen.insert(*level))
            .collect();
    }
    let supports = model_cfg
        .and_then(|m| m.supports_verbosity)
        .unwrap_or(provider.compat.supports_verbosity);
    if !supports {
        return vec![V::Medium];
    }
    vec![V::Low, V::Medium, V::High]
}

/// Returns the thinking-summary modes valid for `model`. `[Off]` when
/// the provider doesn't expose `reasoning.summary`; otherwise the full
/// `[Off, Auto, Concise, Detailed]` set.
pub(crate) fn thinking_summaries_for_model(
    registry: &tau_config::settings::ModelRegistry,
    model: &ModelId,
) -> Vec<tau_proto::ThinkingSummary> {
    use tau_proto::ThinkingSummary as T;
    let Some(provider) = registry.providers.get(&model.provider) else {
        return Vec::new();
    };
    if !provider.compat.supports_reasoning_summary {
        return vec![T::Off];
    }
    vec![T::Off, T::Auto, T::Concise, T::Detailed]
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

pub(crate) fn clamp_verbosity(
    requested: tau_proto::Verbosity,
    allowed: &[tau_proto::Verbosity],
) -> tau_proto::Verbosity {
    use tau_proto::Verbosity as V;
    if allowed.contains(&requested) {
        return requested;
    }
    if allowed.contains(&V::Medium) {
        return V::Medium;
    }
    allowed.first().copied().unwrap_or(V::Medium)
}

pub(crate) fn clamp_thinking_summary(
    requested: tau_proto::ThinkingSummary,
    allowed: &[tau_proto::ThinkingSummary],
) -> tau_proto::ThinkingSummary {
    use tau_proto::ThinkingSummary as T;
    if allowed.contains(&requested) {
        return requested;
    }
    if allowed.contains(&T::Off) {
        return T::Off;
    }
    allowed.first().copied().unwrap_or(T::Off)
}

fn load_state_json(
    dirs: &tau_config::settings::TauDirs,
) -> serde_json::Map<String, serde_json::Value> {
    let Some(path) = dirs.state_dir.as_ref().map(|d| d.join("harness.json5")) else {
        return serde_json::Map::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return serde_json::Map::new();
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn load_last_params(dirs: &tau_config::settings::TauDirs) -> HashMap<ModelId, ModelParams> {
    let json = load_state_json(dirs);
    let mut out = HashMap::new();
    if let Some(map) = json
        .get("last_params")
        .and_then(serde_json::Value::as_object)
    {
        for (model, entry) in map {
            let Ok(model) = model.parse::<ModelId>() else {
                // Skip entries persisted with a malformed id rather
                // than failing the whole load — the on-disk state file
                // is best-effort UX, not a contract.
                continue;
            };
            let Ok(params) = serde_json::from_value::<ModelParams>(entry.clone()) else {
                continue;
            };
            out.insert(model, params);
        }
    }

    out
}

/// Resolve the [`ModelParams`] to use for `model` on startup or after
/// a model switch.
///
/// Each field is selected independently:
/// 1. `default_params[model]` entry in `harness.json5`, if any;
/// 2. otherwise the persisted `last_params[model]` from state;
/// 3. otherwise the per-field middle / Auto fallback.
///
/// Each field is then clamped against the allowed set for `model`, so
/// stale persistence after a model switch can't ship a value the new
/// model doesn't accept.
pub(crate) fn selected_params_for_model(
    dirs: &tau_config::settings::TauDirs,
    harness_settings: &tau_config::settings::HarnessSettings,
    registry: &tau_config::settings::ModelRegistry,
    model: &ModelId,
) -> ModelParams {
    let allowed_effort = efforts_for_model(registry, model);
    let allowed_verbosity = verbosities_for_model(registry, model);
    let allowed_thinking = thinking_summaries_for_model(registry, model);
    let default_entry = harness_settings.default_params.get(model).copied();
    let last = load_last_params(dirs).remove(model);

    let effort = default_entry
        .map(|p| p.effort)
        .or(last.map(|p| p.effort))
        .unwrap_or_else(|| middle_effort(&allowed_effort));
    let verbosity = default_entry
        .map(|p| p.verbosity)
        .or(last.map(|p| p.verbosity))
        .unwrap_or_else(|| default_verbosity(&allowed_verbosity));
    let thinking_summary = default_entry
        .map(|p| p.thinking_summary)
        .or(last.map(|p| p.thinking_summary))
        .unwrap_or_else(|| default_thinking_summary(&allowed_thinking));
    let service_tier = default_entry
        .map(|p| p.service_tier)
        .or(last.map(|p| p.service_tier))
        .flatten();

    ModelParams {
        effort: clamp_effort(effort, &allowed_effort),
        verbosity: clamp_verbosity(verbosity, &allowed_verbosity),
        thinking_summary: clamp_thinking_summary(thinking_summary, &allowed_thinking),
        service_tier,
    }
}

/// Pick the middle element of `allowed`, or `Off` for an empty list.
/// First-time users (no `default_params` entry, no persisted last
/// params) get a sensible reasoning level instead of always landing on
/// `Off` — for the standard `[Off, Minimal, Low, Medium, High]` list
/// that's `Low`. Returns `Off` for `[Off]`-only providers and the
/// empty case.
pub(crate) fn middle_effort(allowed: &[tau_proto::Effort]) -> tau_proto::Effort {
    if allowed.is_empty() {
        return tau_proto::Effort::Off;
    }
    allowed[allowed.len() / 2]
}

/// Default verbosity when no persisted preference exists. Picks
/// `Low` whenever it's allowed (matching the quiet defaults used by
/// other coding agents), otherwise falls back to the first allowed
/// entry.
pub(crate) fn default_verbosity(allowed: &[tau_proto::Verbosity]) -> tau_proto::Verbosity {
    use tau_proto::Verbosity as V;
    if allowed.contains(&V::Low) {
        return V::Low;
    }
    allowed.first().copied().unwrap_or(V::Low)
}

/// Default thinking-summary mode when no persisted preference exists.
/// `Auto` for providers that support summaries; `Off` everywhere else.
pub(crate) fn default_thinking_summary(
    allowed: &[tau_proto::ThinkingSummary],
) -> tau_proto::ThinkingSummary {
    use tau_proto::ThinkingSummary as T;
    if allowed.contains(&T::Auto) {
        return T::Auto;
    }
    if allowed.contains(&T::Off) {
        return T::Off;
    }
    allowed.first().copied().unwrap_or(T::Off)
}

/// Load the last-selected model from `<state_dir>/harness.json5`.
/// Returns `None` if the file is missing, malformed, or the saved id
/// no longer parses as a `provider/model`.
fn load_last_selected_model(dirs: &tau_config::settings::TauDirs) -> Option<ModelId> {
    let json = load_state_json(dirs);
    json.get("last_selected_model")?.as_str()?.parse().ok()
}

fn load_last_selected_role(dirs: &tau_config::settings::TauDirs) -> Option<String> {
    let json = load_state_json(dirs);
    let role = json.get("last_selected_role")?.as_str()?.to_owned();
    (!role.is_empty()).then_some(role)
}

fn load_role_overrides(dirs: &tau_config::settings::TauDirs) -> HashMap<String, AgentRole> {
    let json = load_state_json(dirs);
    let mut out = HashMap::new();
    if let Some(map) = json
        .get("role_overrides")
        .and_then(serde_json::Value::as_object)
    {
        for (name, entry) in map {
            if let Ok(role) = serde_json::from_value::<AgentRole>(entry.clone()) {
                out.insert(name.clone(), role);
            }
        }
    }
    out
}

pub(crate) fn save_role_overrides(
    dirs: &tau_config::settings::TauDirs,
    selected_role: Option<&str>,
    roles: &HashMap<String, AgentRole>,
) {
    let Some(dir) = dirs.state_dir.as_ref() else {
        return;
    };
    let path = dir.join("harness.json5");
    let _ = std::fs::create_dir_all(dir);
    let mut json = load_state_json(dirs);
    json.insert(
        "last_selected_role".to_owned(),
        serde_json::Value::String(selected_role.unwrap_or_default().to_owned()),
    );
    let overrides = roles
        .iter()
        .map(|(name, role)| {
            (
                name.clone(),
                serde_json::to_value(role).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    json.insert(
        "role_overrides".to_owned(),
        serde_json::Value::Object(overrides),
    );
    let _ = serde_json::to_string_pretty(&serde_json::Value::Object(json))
        .ok()
        .and_then(|s| std::fs::write(&path, s).ok());
}

/// Persist model + params to `<state_dir>/harness.json5`. `model: None`
/// records that no model is currently selected.
pub(crate) fn save_harness_state(
    dirs: &tau_config::settings::TauDirs,
    model: Option<&ModelId>,
    params: ModelParams,
) {
    let Some(dir) = dirs.state_dir.as_ref() else {
        return;
    };
    let path = dir.join("harness.json5");
    let _ = std::fs::create_dir_all(dir);
    let mut last_params = load_last_params(dirs);
    if let Some(model) = model {
        last_params.insert(model.clone(), params);
    }
    let params_json = last_params
        .into_iter()
        .map(|(model, params)| {
            (
                model.to_string(),
                serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let mut json = load_state_json(dirs);
    json.insert(
        "last_selected_model".to_owned(),
        serde_json::Value::String(model.map(ModelId::to_string).unwrap_or_default()),
    );
    json.insert(
        "last_params".to_owned(),
        serde_json::Value::Object(params_json),
    );
    let _ = serde_json::to_string_pretty(&serde_json::Value::Object(json))
        .ok()
        .and_then(|s| std::fs::write(&path, s).ok());
}
