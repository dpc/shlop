use tau_config::settings::{self, PromptCacheRetention, ProviderConfig};

use super::*;

#[test]
fn public_openai_api_enables_prompt_cache_support() {
    let provider = ProviderConfig::default();

    assert!(supports_prompt_cache_key(
        &provider,
        "https://api.openai.com/v1"
    ));
    assert!(supports_prompt_cache_retention(
        &provider,
        "https://api.openai.com/v1/"
    ));
}

#[test]
fn codex_backend_enables_prompt_cache_key_but_not_retention() {
    let provider = ProviderConfig::default();

    assert!(supports_prompt_cache_key(
        &provider,
        "https://chatgpt.com/backend-api"
    ));
    // chatgpt.com/backend-api 400s on `prompt_cache_retention` —
    // only the public REST API accepts it.
    assert!(!supports_prompt_cache_retention(
        &provider,
        "https://chatgpt.com/backend-api/"
    ));
}

#[test]
fn provider_flags_enable_prompt_cache_support_for_non_openai_backends() {
    let provider = ProviderConfig {
        compat: settings::ProviderCompat {
            supports_prompt_cache_key: true,
            supports_prompt_cache_retention: true,
            ..settings::ProviderCompat::default()
        },
        ..ProviderConfig::default()
    };

    assert!(supports_prompt_cache_key(
        &provider,
        "https://example.com/v1"
    ));
    assert!(supports_prompt_cache_retention(
        &provider,
        "https://example.com/v1"
    ));
}

#[test]
fn public_openai_api_defaults_retention_to_24h_on_supported_models() {
    let provider = ProviderConfig::default();

    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1", "gpt-5.5"),
        Some(PromptCacheRetention::Extended24h)
    );
    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1/", "gpt-5.5-pro"),
        Some(PromptCacheRetention::Extended24h)
    );
}

#[test]
fn codex_backend_skips_retention_default_even_on_supported_models() {
    let provider = ProviderConfig::default();

    // Regression: defaulting `prompt_cache_retention` to 24h on the
    // Codex Responses backend caused HTTP 400 — the routing there
    // doesn't accept the param, even on gpt-5.5+.
    assert_eq!(
        prompt_cache_retention(&provider, "https://chatgpt.com/backend-api", "gpt-5.5"),
        None
    );
    assert_eq!(
        prompt_cache_retention(&provider, "https://chatgpt.com/backend-api/", "gpt-5.5-pro"),
        None
    );
}

#[test]
fn builtin_openai_skips_retention_default_on_older_models() {
    let provider = ProviderConfig::default();

    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1", "gpt-5.4"),
        None
    );
    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1", "gpt-4o"),
        None
    );
}

#[test]
fn explicit_provider_retention_wins_over_model_default() {
    let provider = ProviderConfig {
        prompt_cache_retention: Some(PromptCacheRetention::InMemory),
        ..ProviderConfig::default()
    };

    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1", "gpt-5.5"),
        Some(PromptCacheRetention::InMemory)
    );
}

#[test]
fn non_builtin_backend_skips_retention_default() {
    let provider = ProviderConfig {
        compat: settings::ProviderCompat {
            supports_prompt_cache_retention: true,
            ..settings::ProviderCompat::default()
        },
        ..ProviderConfig::default()
    };

    assert_eq!(
        prompt_cache_retention(&provider, "https://example.com/v1", "gpt-5.5"),
        None
    );
}
