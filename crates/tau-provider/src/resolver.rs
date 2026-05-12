use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tau_config::settings::{
    AuthType, ModelRegistry, PromptCacheRetention, ProviderConfig,
    is_known_24h_prompt_cache_model_id,
};
use tau_proto::{ModelId, ModelName, ProviderName};

use crate::storage::{self, Credentials, ProviderKind};

#[derive(Clone, Debug)]
pub enum ResolvedBackend {
    ChatCompletions(ResolvedChatCompletions),
    Responses(ResolvedResponses),
}

#[derive(Clone, Debug)]
pub struct ResolvedChatCompletions {
    pub base_url: String,
    pub api_key: String,
    pub model_id: ModelName,
    pub supports_reasoning_effort: bool,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    pub supports_llama_cpp_cache: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedResponses {
    pub base_url: String,
    pub api_key: String,
    pub model_id: ModelName,
    pub account_id: Option<String>,
    pub supports_reasoning_effort: bool,
    pub supports_reasoning_summary: bool,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<PromptCacheRetention>,
}

/// Resolve a [`ModelId`] against the configured provider registry and
/// the caller-supplied auth store.
///
/// The caller threads `auth_store` so that any OAuth refresh performed
/// during resolution is observable on subsequent calls without a disk
/// reload. Refreshes are also persisted to disk via
/// [`storage::save_provider`].
pub fn resolve(
    model: &ModelId,
    models: &ModelRegistry,
    auth_store: &mut storage::AuthStore,
) -> Option<ResolvedBackend> {
    let provider = models.providers.get(&model.provider)?;
    let auth_type = match provider.auth_type() {
        Ok(t) => t,
        Err(other) => {
            tracing::warn!(
                provider = %model.provider,
                auth = other,
                "unknown `auth` value in models.json5; not resolving"
            );
            return None;
        }
    };

    match auth_type {
        AuthType::OpenaiCodex => {
            responses_backend(&model.provider, provider, auth_store, &model.model)
        }
        AuthType::GithubCopilot => {
            copilot_backend(&model.provider, provider, auth_store, &model.model)
        }
        AuthType::ApiKey | AuthType::None => {
            chat_completions_backend(&model.provider, provider, auth_store, &model.model)
        }
    }
}

fn responses_backend(
    provider_name: &ProviderName,
    provider: &ProviderConfig,
    auth_store: &mut storage::AuthStore,
    model_id: &ModelName,
) -> Option<ResolvedBackend> {
    let (access_token, account_id) = match auth_store.providers.get(provider_name)? {
        Credentials::Oauth {
            access_token,
            refresh_token,
            expires_at_ms,
            account_id,
            ..
        } => {
            let mut access_token = access_token.clone();
            let mut account_id = account_id.clone();
            if oauth_token_should_refresh(&access_token, *expires_at_ms) {
                if let Ok(tokens) = crate::oauth::openai_codex_refresh(refresh_token) {
                    access_token = tokens.access_token.clone();
                    account_id = tokens.account_id.clone();
                    let creds = Credentials::Oauth {
                        provider_kind: ProviderKind::OpenaiCodex,
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                        expires_at_ms: tokens.expires_at_ms,
                        account_id: tokens.account_id,
                    };
                    if let Err(error) = storage::save_provider(provider_name, &creds) {
                        tracing::warn!(
                            provider = %provider_name,
                            "failed to save refreshed credentials: {error}"
                        );
                    }
                    auth_store.providers.insert(provider_name.clone(), creds);
                }
            }
            (access_token, account_id)
        }
        _ => return None,
    };
    let base_url = "https://chatgpt.com/backend-api";
    Some(ResolvedBackend::Responses(ResolvedResponses {
        base_url: base_url.to_owned(),
        api_key: access_token,
        model_id: model_id.clone(),
        account_id,
        supports_reasoning_effort: provider.compat.supports_reasoning_effort,
        supports_reasoning_summary: supports_reasoning_summary(provider, base_url),
        prompt_cache_key: prompt_cache_key(provider, base_url, model_id),
        prompt_cache_retention: prompt_cache_retention(provider, base_url, model_id),
    }))
}

fn copilot_backend(
    provider_name: &ProviderName,
    provider: &ProviderConfig,
    auth_store: &mut storage::AuthStore,
    model_id: &ModelName,
) -> Option<ResolvedBackend> {
    let access_token = match auth_store.providers.get(provider_name)? {
        Credentials::Oauth { access_token, .. } => access_token.clone(),
        _ => return None,
    };
    let base_url = extract_copilot_base_url(&access_token)
        .unwrap_or_else(|| "https://api.individual.githubcopilot.com".to_owned());
    Some(ResolvedBackend::ChatCompletions(ResolvedChatCompletions {
        prompt_cache_key: prompt_cache_key(provider, &base_url, model_id),
        prompt_cache_retention: prompt_cache_retention(provider, &base_url, model_id),
        base_url,
        api_key: access_token,
        model_id: model_id.clone(),
        supports_reasoning_effort: provider.compat.supports_reasoning_effort,
        supports_llama_cpp_cache: provider.compat.supports_llama_cpp_cache,
    }))
}

fn chat_completions_backend(
    provider_name: &ProviderName,
    provider: &ProviderConfig,
    auth_store: &mut storage::AuthStore,
    model_id: &ModelName,
) -> Option<ResolvedBackend> {
    let base_url =
        provider
            .base_url
            .clone()
            .or_else(|| match auth_store.providers.get(provider_name)? {
                Credentials::ApiKey { .. } => Some("https://api.openai.com/v1".to_owned()),
                _ => None,
            })?;
    Some(ResolvedBackend::ChatCompletions(ResolvedChatCompletions {
        prompt_cache_key: prompt_cache_key(provider, &base_url, model_id),
        prompt_cache_retention: prompt_cache_retention(provider, &base_url, model_id),
        base_url,
        api_key: provider.api_key.clone().unwrap_or_default(),
        model_id: model_id.clone(),
        supports_reasoning_effort: provider.compat.supports_reasoning_effort,
        supports_llama_cpp_cache: provider.compat.supports_llama_cpp_cache,
    }))
}

fn oauth_token_should_refresh(access_token: &str, expires_at_ms: u64) -> bool {
    let now_ms = now_ms();
    if let Some(issued_at_ms) = jwt_issued_at_ms(access_token) {
        let lifetime_ms = expires_at_ms.saturating_sub(issued_at_ms);
        let refresh_at_ms = issued_at_ms.saturating_add(lifetime_ms / 2);
        if refresh_at_ms <= now_ms {
            return true;
        }
    }
    expires_at_ms <= now_ms.saturating_add(duration_millis_u64(Duration::from_secs(5 * 60)))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn jwt_issued_at_ms(jwt: &str) -> Option<u64> {
    let payload = jwt.split('.').nth(1)?;
    let payload = crate::oauth::base64_url_safe_no_pad_decode(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims.get("iat")?.as_u64().map(|secs| secs * 1000)
}

fn prompt_cache_key(provider: &ProviderConfig, base_url: &str, model_id: &str) -> Option<String> {
    if !supports_prompt_cache_key(provider, base_url) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    Some(prompt_cache_key_for(base_url, model_id, &cwd))
}

/// Derive the per-(provider, model, workspace) cache key the OpenAI
/// guide expects. The key is hashed with the prompt prefix on the
/// server side and used to bias routing so semantically-related
/// requests land on the same machine and reuse its cached KV state.
///
/// Granularity rationale: `(base_url, model_id, cwd)` is the
/// coarsest grouping that still pins a session to one cache. A
/// single workspace usually has one active conversation at a time,
/// well under OpenAI's documented ~15 RPM ceiling per (prefix,
/// machine) — go any coarser and bursts overflow to multiple
/// machines, defeating the point.
fn prompt_cache_key_for(base_url: &str, model_id: &str, cwd: &std::path::Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_url.as_bytes());
    hasher.update(b"\0");
    hasher.update(model_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(cwd.to_string_lossy().as_bytes());
    format!("tau-{:x}", hasher.finalize())
}

fn prompt_cache_retention(
    provider: &ProviderConfig,
    base_url: &str,
    model_id: &str,
) -> Option<PromptCacheRetention> {
    if !supports_prompt_cache_retention(provider, base_url) {
        return None;
    }
    if let Some(explicit) = provider.prompt_cache_retention {
        return Some(explicit);
    }
    // Default to 24 h retention on the OpenAI public API when the
    // model is known to support the param. Same-price extension of
    // the in-memory TTL (5–10 min idle, max 1 h) to 24 h, so coffee
    // breaks don't evict the working prefix.
    if is_builtin_openai_public_api_endpoint(base_url)
        && is_known_24h_prompt_cache_model_id(model_id)
    {
        return Some(PromptCacheRetention::Extended24h);
    }
    None
}

fn supports_prompt_cache_key(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_prompt_cache_key || is_builtin_openai_endpoint(base_url)
}

fn supports_reasoning_summary(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_reasoning_summary || is_builtin_openai_endpoint(base_url)
}

fn supports_prompt_cache_retention(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_prompt_cache_retention
        || is_builtin_openai_public_api_endpoint(base_url)
}

/// True for the two OpenAI-operated endpoints that ship with the
/// prompt-cache-key and reasoning-summary features. User-configured
/// proxies and re-hosters in front of these URLs do NOT count — they
/// must opt in explicitly via `ProviderCompat`.
///
/// Note: `prompt_cache_retention` is NOT in this set — the ChatGPT
/// Codex Responses backend at `chatgpt.com/backend-api` rejects it as
/// an unknown parameter. Use [`is_builtin_openai_public_api_endpoint`]
/// for retention gating.
fn is_builtin_openai_endpoint(base_url: &str) -> bool {
    matches!(
        base_url.trim_end_matches('/'),
        "https://api.openai.com/v1" | "https://chatgpt.com/backend-api"
    )
}

/// True for the OpenAI public REST API only. The ChatGPT
/// Codex Responses backend (`chatgpt.com/backend-api`) is excluded
/// because, despite sharing most compat features with the public
/// API, it rejects `prompt_cache_retention` as an unknown parameter.
fn is_builtin_openai_public_api_endpoint(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == "https://api.openai.com/v1"
}

fn extract_copilot_base_url(token: &str) -> Option<String> {
    for part in token.split(';') {
        if let Some(ep) = part.strip_prefix("proxy-ep=") {
            let url = format!("https://{ep}");
            if url::Url::parse(&url).is_ok() {
                return Some(url);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
