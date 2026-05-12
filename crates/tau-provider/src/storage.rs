//! Auth credential storage.
//!
//! Credentials live as one file per provider under
//! `~/.local/state/tau/auth.d/<name>.json`. A pre-existing whole-file
//! `auth.json` from older Tau versions is still read on load for
//! backwards compatibility, but all *writes* go through the per-file
//! layout so concurrent updates to different providers cannot collide.

use std::collections::HashMap;
use std::path::PathBuf;
use std::{fs, io};

use serde::{Deserialize, Serialize};
use tau_config::atomic::atomic_write_following_symlink;
use tau_proto::ProviderName;

/// Returns the auth state directory.
///
/// Prefers `XDG_STATE_HOME/tau` (`~/.local/state/tau` on Linux).
/// Falls back to `data_local_dir/tau` on platforms where `state_dir`
/// is not available (macOS, Windows).
fn state_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("tau"))
}

/// Returns the path to legacy `auth.json` (read for backwards compat,
/// never written to by the current code).
pub fn auth_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("auth.json"))
}

/// Returns the per-provider auth directory `auth.d/`.
pub fn auth_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join("auth.d"))
}

/// Returns the file path that backs the named provider's credentials.
///
/// Filename safety is guaranteed by [`ProviderName`]'s constructor —
/// the type itself rejects names that aren't safe to embed in a path,
/// so this helper just joins.
pub fn provider_auth_path(provider_name: &ProviderName) -> io::Result<PathBuf> {
    let dir = auth_dir().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cannot determine data directory")
    })?;
    Ok(dir.join(format!("{provider_name}.json")))
}

/// The kind of provider (determines which OAuth flow or auth method).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Local Ollama/llama.cpp — no auth needed.
    Ollama,
    /// OpenAI direct API key access.
    Openai,
    /// OpenAI via ChatGPT subscription (OAuth).
    OpenaiCodex,
    /// Anthropic direct API key access.
    Anthropic,
    /// GitHub Copilot subscription (device code OAuth).
    GithubCopilot,
}

impl ProviderKind {
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Ollama => "Ollama (local)",
            Self::Openai => "OpenAI (API key)",
            Self::OpenaiCodex => "OpenAI Codex (ChatGPT subscription)",
            Self::Anthropic => "Anthropic (API key)",
            Self::GithubCopilot => "GitHub Copilot (subscription)",
        }
    }

    pub fn requires_oauth(&self) -> bool {
        matches!(self, Self::OpenaiCodex | Self::GithubCopilot)
    }

    pub fn all() -> &'static [ProviderKind] {
        &[
            Self::Ollama,
            Self::Openai,
            Self::OpenaiCodex,
            Self::Anthropic,
            Self::GithubCopilot,
        ]
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Credentials for a single provider instance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credentials {
    /// No authentication needed (e.g. local Ollama).
    None {
        provider_kind: ProviderKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
    /// Direct API key.
    ApiKey {
        provider_kind: ProviderKind,
        api_key: String,
    },
    /// OAuth token pair with expiration.
    Oauth {
        provider_kind: ProviderKind,
        access_token: String,
        refresh_token: String,
        /// Milliseconds since epoch when `access_token` expires.
        expires_at_ms: u64,
        /// Provider-specific account identifier (e.g. OpenAI account ID).
        #[serde(skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
    },
}

impl Credentials {
    pub fn provider_kind(&self) -> &ProviderKind {
        match self {
            Self::None { provider_kind, .. }
            | Self::ApiKey { provider_kind, .. }
            | Self::Oauth { provider_kind, .. } => provider_kind,
        }
    }
}

/// In-memory snapshot of all configured credentials.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuthStore {
    pub providers: HashMap<ProviderName, Credentials>,
}

/// Load all credentials from disk.
///
/// Reads (in order): legacy `auth.json` if present, then each
/// `auth.d/*.json` file. Per-file entries override the legacy file on
/// duplicate provider names. Missing files yield an empty store.
/// Files whose stem fails [`ProviderName`] validation are skipped with
/// a warning rather than aborting the whole load.
pub fn load() -> io::Result<AuthStore> {
    let mut providers: HashMap<ProviderName, Credentials> = HashMap::new();

    if let Some(legacy_path) = auth_path() {
        if legacy_path.exists() {
            let text = fs::read_to_string(&legacy_path)?;
            let legacy: AuthStore = serde_json::from_str(&text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            providers.extend(legacy.providers);
        }
    }

    if let Some(dir) = auth_dir() {
        if dir.is_dir() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let provider = match ProviderName::try_new(stem.to_owned()) {
                    Ok(p) => p,
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            "skipping auth file with invalid provider name: {error}"
                        );
                        continue;
                    }
                };
                let text = fs::read_to_string(&path)?;
                let creds: Credentials = serde_json::from_str(&text)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                providers.insert(provider, creds);
            }
        }
    }

    Ok(AuthStore { providers })
}

/// Atomically save one provider's credentials to `auth.d/<name>.json`.
///
/// This is the only durability primitive for credentials. Unlike a
/// whole-store write, it does not read or rewrite other providers, so a
/// concurrent `tau provider login` against a different provider can run
/// safely in parallel.
pub fn save_provider(provider_name: &ProviderName, credentials: &Credentials) -> io::Result<()> {
    let path = provider_auth_path(provider_name)?;
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no parent for provider auth path")
    })?;
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }

    let json = serde_json::to_string_pretty(credentials)?;

    #[cfg(unix)]
    let default_permissions = {
        use std::os::unix::fs::PermissionsExt;
        Some(fs::Permissions::from_mode(0o600))
    };
    #[cfg(not(unix))]
    let default_permissions = None;

    atomic_write_following_symlink(&path, json.as_bytes(), default_permissions)
}

/// Remove a provider's credentials from disk.
///
/// Removes `auth.d/<name>.json` if present, and also strips the entry
/// from legacy `auth.json` if that file still exists. Returns true if
/// any state on disk changed.
pub fn delete_provider(provider_name: &ProviderName) -> io::Result<bool> {
    let mut changed = false;

    let path = provider_auth_path(provider_name)?;
    match fs::remove_file(&path) {
        Ok(()) => {
            changed = true;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    if let Some(legacy_path) = auth_path() {
        if legacy_path.exists() {
            let text = fs::read_to_string(&legacy_path)?;
            let mut store: AuthStore = serde_json::from_str(&text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if store.providers.remove(provider_name).is_some() {
                let json = serde_json::to_string_pretty(&store)?;
                #[cfg(unix)]
                let default_permissions = {
                    use std::os::unix::fs::PermissionsExt;
                    Some(fs::Permissions::from_mode(0o600))
                };
                #[cfg(not(unix))]
                let default_permissions = None;
                atomic_write_following_symlink(&legacy_path, json.as_bytes(), default_permissions)?;
                changed = true;
            }
        }
    }

    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_accepts_typical_names() {
        for name in [
            "local",
            "openai",
            "openai-codex",
            "github-copilot",
            "my.provider_2",
            "a",
        ] {
            assert!(
                ProviderName::try_new(name.to_owned()).is_ok(),
                "expected '{name}' to be accepted"
            );
        }
    }

    #[test]
    fn provider_name_rejects_unsafe_inputs() {
        for name in [
            "",
            ".hidden",
            "-leading-dash",
            "has space",
            "has/slash",
            "has\\backslash",
            "..",
            "../escape",
        ] {
            assert!(
                ProviderName::try_new(name.to_owned()).is_err(),
                "expected '{name}' to be rejected"
            );
        }
    }
}
