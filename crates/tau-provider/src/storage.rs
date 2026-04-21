//! Auth credential storage at `~/.local/state/tau/auth.json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::{fs, io};

use serde::{Deserialize, Serialize};

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

/// Returns the path to `auth.json`.
pub fn auth_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("auth.json"))
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

/// Top-level auth.json structure.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuthStore {
    /// Named provider instances.
    pub providers: HashMap<String, Credentials>,
}

/// Load auth store from disk. Returns default (empty) if file doesn't exist.
pub fn load() -> io::Result<AuthStore> {
    let Some(path) = auth_path() else {
        return Ok(AuthStore::default());
    };
    if !path.exists() {
        return Ok(AuthStore::default());
    }
    let contents = fs::read_to_string(&path)?;
    serde_json::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Save auth store to disk with secure permissions.
pub fn save(store: &AuthStore) -> io::Result<()> {
    let Some(path) = auth_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "cannot determine data directory",
        ));
    };

    // Ensure parent dir exists with 0700.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }

    let json = serde_json::to_string_pretty(store)?;
    fs::write(&path, &json)?;

    // Set file to 0600.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}
