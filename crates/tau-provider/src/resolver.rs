use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tau_config::settings::{ModelRegistry, PromptCacheRetention, ProviderConfig};

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
    pub model_id: String,
    pub supports_reasoning_effort: bool,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    pub supports_llama_cpp_cache: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedResponses {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    pub account_id: Option<String>,
    pub supports_reasoning_effort: bool,
    pub supports_reasoning_summary: bool,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<PromptCacheRetention>,
}

pub fn resolve(model: &str, models: &ModelRegistry) -> Option<ResolvedBackend> {
    let mut auth_store = storage::load().unwrap_or_default();
    resolve_with_auth_store(model, models, &mut auth_store)
}

pub fn resolve_with_auth_store(
    model: &str,
    models: &ModelRegistry,
    auth_store: &mut storage::AuthStore,
) -> Option<ResolvedBackend> {
    let (provider_name, model_id) = model.split_once('/')?;
    let provider = models.providers.get(provider_name)?;
    let auth_type = provider
        .auth
        .as_deref()
        .unwrap_or(if provider.api_key.is_some() {
            "api-key"
        } else {
            "none"
        });

    match auth_type {
        "openai-codex" => responses_backend(provider_name, provider, auth_store, model_id),
        "github-copilot" => copilot_backend(provider_name, provider, auth_store, model_id),
        "api-key" | "none" => {
            chat_completions_backend(provider_name, provider, auth_store, model_id)
        }
        other => {
            tracing::warn!(
                provider = provider_name,
                auth = other,
                "unknown `auth` value in models.json5; not resolving"
            );
            None
        }
    }
}

fn responses_backend(
    provider_name: &str,
    provider: &ProviderConfig,
    auth_store: &mut storage::AuthStore,
    model_id: &str,
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
                    auth_store
                        .providers
                        .insert(provider_name.to_owned(), creds.clone());
                    if let Err(error) = storage::save_provider(provider_name, creds) {
                        tracing::warn!(
                            provider = provider_name,
                            "failed to save refreshed credentials: {error}"
                        );
                    }
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
        model_id: model_id.to_owned(),
        account_id,
        supports_reasoning_effort: provider.compat.supports_reasoning_effort,
        supports_reasoning_summary: supports_reasoning_summary(provider, base_url),
        prompt_cache_key: prompt_cache_key(provider, base_url, model_id),
        prompt_cache_retention: prompt_cache_retention(provider, base_url),
    }))
}

fn copilot_backend(
    provider_name: &str,
    provider: &ProviderConfig,
    auth_store: &mut storage::AuthStore,
    model_id: &str,
) -> Option<ResolvedBackend> {
    let access_token = match auth_store.providers.get(provider_name)? {
        Credentials::Oauth { access_token, .. } => access_token.clone(),
        _ => return None,
    };
    let base_url = extract_copilot_base_url(&access_token)
        .unwrap_or_else(|| "https://api.individual.githubcopilot.com".to_owned());
    Some(ResolvedBackend::ChatCompletions(ResolvedChatCompletions {
        prompt_cache_key: prompt_cache_key(provider, &base_url, model_id),
        prompt_cache_retention: prompt_cache_retention(provider, &base_url),
        base_url,
        api_key: access_token,
        model_id: model_id.to_owned(),
        supports_reasoning_effort: provider.compat.supports_reasoning_effort,
        supports_llama_cpp_cache: provider.compat.supports_llama_cpp_cache,
    }))
}

fn chat_completions_backend(
    provider_name: &str,
    provider: &ProviderConfig,
    auth_store: &mut storage::AuthStore,
    model_id: &str,
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
        prompt_cache_retention: prompt_cache_retention(provider, &base_url),
        base_url,
        api_key: provider.api_key.clone().unwrap_or_default(),
        model_id: model_id.to_owned(),
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
) -> Option<PromptCacheRetention> {
    if supports_prompt_cache_retention(provider, base_url) {
        provider.prompt_cache_retention
    } else {
        None
    }
}

fn supports_prompt_cache_key(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_prompt_cache_key || is_builtin_openai_prompt_cache_api(base_url)
}

fn supports_reasoning_summary(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_reasoning_summary || is_builtin_openai_prompt_cache_api(base_url)
}

fn supports_prompt_cache_retention(provider: &ProviderConfig, base_url: &str) -> bool {
    provider.compat.supports_prompt_cache_retention || is_builtin_openai_prompt_cache_api(base_url)
}

fn is_builtin_openai_prompt_cache_api(base_url: &str) -> bool {
    matches!(
        base_url.trim_end_matches('/'),
        "https://api.openai.com/v1" | "https://chatgpt.com/backend-api"
    )
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
