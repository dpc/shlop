//! CLI entrypoint for `tau provider` subcommands.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dialoguer::{Confirm, Input, Select};

use crate::oauth;
use crate::storage::{self, Credentials, ProviderKind};

/// Run the provider CLI with the given subcommand arguments.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let subcommand = args.first().map(String::as_str).unwrap_or("help");

    match subcommand {
        "add" => cmd_add()?,
        "remove" => cmd_remove(args.get(1).map(String::as_str))?,
        "list" => cmd_list()?,
        "login" => cmd_login(args.get(1).map(String::as_str))?,
        "list-models" => cmd_list_models(args.get(1).map(String::as_str))?,
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("unknown subcommand: {other}");
            print_help();
            return Err(format!("unknown subcommand: {other}").into());
        }
    }
    Ok(())
}

fn print_help() {
    eprintln!("Usage: tau provider <subcommand>");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!("  add                 Add a new provider (interactive wizard)");
    eprintln!("  remove [name]       Remove a provider from models.json5 and auth.json");
    eprintln!("  list                List configured providers");
    eprintln!("  login [name]        Log in / refresh OAuth token for a provider");
    eprintln!("  list-models [name]  List models available from a provider");
}

// ---------------------------------------------------------------------------
// tau provider add
// ---------------------------------------------------------------------------

fn cmd_add() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Pick provider kind.
    let kinds = ProviderKind::all();
    let kind_names: Vec<&str> = kinds.iter().map(ProviderKind::display_name).collect();

    let selection = Select::new()
        .with_prompt("Provider type")
        .items(&kind_names)
        .default(0)
        .interact()?;
    let kind = kinds[selection].clone();

    // 2. Pick a name for this instance.
    let default_name = match &kind {
        ProviderKind::Ollama => "local",
        ProviderKind::Openai => "openai",
        ProviderKind::OpenaiCodex => "openai-codex",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::GithubCopilot => "github-copilot",
    };

    let name: String = Input::new()
        .with_prompt("Name for this provider")
        .default(default_name.to_string())
        .interact_text()?;

    // 3. Kind-specific setup.
    let creds = match &kind {
        ProviderKind::Ollama => {
            let base_url: String = Input::new()
                .with_prompt("Base URL")
                .default("http://localhost:11434".to_string())
                .interact_text()?;
            Credentials::None {
                provider_kind: kind.clone(),
                base_url: Some(base_url),
            }
        }

        ProviderKind::Openai => {
            let api_key: String = Input::new().with_prompt("API key").interact_text()?;
            Credentials::ApiKey {
                provider_kind: kind.clone(),
                api_key,
            }
        }

        ProviderKind::Anthropic => {
            let api_key: String = Input::new().with_prompt("API key").interact_text()?;
            Credentials::ApiKey {
                provider_kind: kind.clone(),
                api_key,
            }
        }

        ProviderKind::OpenaiCodex => {
            eprintln!("\nStarting OpenAI login flow...");
            run_openai_codex_login(&kind)?
        }

        ProviderKind::GithubCopilot => {
            eprintln!("\nStarting GitHub Copilot login flow...");
            run_github_copilot_login(&kind)?
        }
    };

    // 4. Save to auth.json.
    let mut store = storage::load()?;
    store.providers.insert(name.clone(), creds);
    storage::save(&store)?;

    if let Some(path) = storage::auth_path() {
        eprintln!("\nCredentials saved to: {}", path.display());
    }

    // 5. Update or print models.json5 snippet.
    let snippet = build_provider_entry(&kind);
    update_or_print_models_json5(&name, &snippet)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// tau provider remove
// ---------------------------------------------------------------------------

fn cmd_remove(name_arg: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let models = tau_config::settings::load_models()?;
    let mut store = storage::load()?;

    let name = match name_arg {
        Some(n) => n.to_string(),
        None => {
            let mut names: Vec<&str> = models.providers.keys().map(String::as_str).collect();
            names.extend(store.providers.keys().map(String::as_str));
            names.sort();
            names.dedup();

            if names.is_empty() {
                eprintln!("No providers to remove.");
                return Ok(());
            }

            let sel = Select::new()
                .with_prompt("Which provider to remove?")
                .items(&names)
                .default(0)
                .interact()?;
            names[sel].to_string()
        }
    };

    let mut removed_anything = false;

    // Remove from auth.json.
    if store.providers.remove(&name).is_some() {
        storage::save(&store)?;
        eprintln!("Removed credentials for '{name}' from auth.json.");
        removed_anything = true;
    }

    // Remove from models.json5.
    if let Some(path) = models_json5_path() {
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let mut root: serde_json::Value = json5::from_str(&text)?;

            let had_it = root
                .as_object_mut()
                .and_then(|o| o.get_mut("providers"))
                .and_then(|p| p.as_object_mut())
                .map(|providers| providers.remove(&name).is_some())
                .unwrap_or(false);

            if had_it {
                let json = serde_json::to_string_pretty(&root)?;
                atomic_write_following_symlink(&path, &json)?;
                eprintln!("Removed '{name}' from models.json5.");
                removed_anything = true;
            }
        }
    }

    if !removed_anything {
        eprintln!("Provider '{name}' not found.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// tau provider list
// ---------------------------------------------------------------------------

fn cmd_list() -> Result<(), Box<dyn std::error::Error>> {
    use comfy_table::{ContentArrangement, Table};

    let models = tau_config::settings::load_models()?;
    let store = storage::load()?;

    if models.providers.is_empty() && store.providers.is_empty() {
        eprintln!("No providers configured. Use `tau provider add` to add one.");
        return Ok(());
    }

    // Collect all provider names from both sources.
    let mut names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for k in models.providers.keys() {
        names.insert(k.as_str());
    }
    for k in store.providers.keys() {
        names.insert(k.as_str());
    }

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.load_preset(comfy_table::presets::NOTHING);
    table.set_header(["Name", "API", "Auth", "Models"]);

    for name in &names {
        let model_info = models.providers.get(*name);
        let auth_info = store.providers.get(*name);

        let auth_type = model_info.and_then(|p| p.auth.as_deref()).unwrap_or(
            if model_info.is_some_and(|p| p.api_key.is_some()) {
                "api-key"
            } else {
                "none"
            },
        );

        let auth_status = match (auth_type, auth_info) {
            (_, Some(Credentials::Oauth { expires_at_ms, .. })) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if now_ms < *expires_at_ms {
                    "logged in".to_string()
                } else {
                    "expired".to_string()
                }
            }
            (a, _) if is_oauth_auth(Some(a)) => match auth_info {
                Some(_) => "logged in".to_string(),
                None => "not logged in".to_string(),
            },
            (a, _) => a.to_string(),
        };

        let api = model_info.and_then(|p| p.api.as_deref()).unwrap_or("-");

        let model_count = model_info.map_or(0, |p| p.models.len());

        table.add_row([*name, api, &auth_status, &model_count.to_string()]);
    }

    println!("{table}");
    Ok(())
}

// ---------------------------------------------------------------------------
// tau provider login
// ---------------------------------------------------------------------------

fn cmd_login(name_arg: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let models = tau_config::settings::load_models()?;
    let mut store = storage::load()?;

    // Collect providers that use OAuth (determined by `auth` field in
    // models.json5).
    let mut oauth_names: Vec<String> = models
        .providers
        .iter()
        .filter(|(_, cfg)| is_oauth_auth(cfg.auth.as_deref()))
        .map(|(name, _)| name.clone())
        .collect();
    oauth_names.sort();

    let name = match name_arg {
        Some(n) => n.to_string(),
        None => {
            if oauth_names.is_empty() {
                eprintln!("No OAuth providers in models.json5.");
                eprintln!("Use `tau provider add` to add one with OAuth auth.");
                return Ok(());
            }

            let items: Vec<&str> = oauth_names.iter().map(String::as_str).collect();
            let sel = Select::new()
                .with_prompt("Which provider to log in to?")
                .items(&items)
                .default(0)
                .interact()?;
            items[sel].to_string()
        }
    };

    let provider_cfg = models
        .providers
        .get(&name)
        .ok_or_else(|| format!("provider '{name}' not found in models.json5"))?;

    let auth = provider_cfg.auth.as_deref().unwrap_or("api-key");
    let kind = auth_to_provider_kind(auth)?;

    let new_creds = match &kind {
        ProviderKind::OpenaiCodex => run_openai_codex_login(&kind)?,
        ProviderKind::GithubCopilot => run_github_copilot_login(&kind)?,
        _ => {
            eprintln!("Provider '{name}' (auth={auth}) does not use OAuth login.");
            return Ok(());
        }
    };

    store.providers.insert(name.clone(), new_creds);
    storage::save(&store)?;
    eprintln!("Login refreshed for '{name}'.");
    Ok(())
}

/// Does this `auth` value represent an OAuth flow?
fn is_oauth_auth(auth: Option<&str>) -> bool {
    matches!(auth, Some("openai-codex" | "github-copilot"))
}

/// Map an `auth` string from models.json5 to a `ProviderKind`.
fn auth_to_provider_kind(auth: &str) -> Result<ProviderKind, Box<dyn std::error::Error>> {
    match auth {
        "none" => Ok(ProviderKind::Ollama),
        "api-key" => Ok(ProviderKind::Openai),
        "openai-codex" => Ok(ProviderKind::OpenaiCodex),
        "github-copilot" => Ok(ProviderKind::GithubCopilot),
        other => Err(format!("unknown auth type: {other}").into()),
    }
}

// ---------------------------------------------------------------------------
// tau provider list-models
// ---------------------------------------------------------------------------

fn cmd_list_models(name_arg: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let models = tau_config::settings::load_models()?;

    let name = match name_arg {
        Some(n) => n.to_string(),
        None => {
            let mut names: Vec<&str> = models.providers.keys().map(String::as_str).collect();
            names.sort();
            if names.is_empty() {
                eprintln!("No providers configured. Use `tau provider add` first.");
                return Ok(());
            }
            let sel = Select::new()
                .with_prompt("Which provider?")
                .items(&names)
                .default(0)
                .interact()?;
            names[sel].to_string()
        }
    };

    let provider_cfg = models
        .providers
        .get(&name)
        .ok_or_else(|| format!("provider '{name}' not found in models.json5"))?;

    if provider_cfg.models.is_empty() {
        eprintln!("No models configured for '{name}' in models.json5.");
    } else {
        for m in &provider_cfg.models {
            println!("{}", m.id);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// OAuth flow runners
// ---------------------------------------------------------------------------

fn run_openai_codex_login(kind: &ProviderKind) -> Result<Credentials, Box<dyn std::error::Error>> {
    let (auth_url, expected_state, verifier) = oauth::openai_codex_auth_url();

    eprintln!("\nOpen this URL in your browser:\n");
    eprintln!("{auth_url}");
    // OSC 8 hyperlink for terminals that support it.
    eprintln!("\x1b]8;;{auth_url}\x1b\\Or click here.\x1b]8;;\x1b\\");
    eprintln!();
    eprintln!("After logging in, you'll be redirected to a page that won't load.");
    eprintln!("Copy the full URL from your browser's address bar and paste it here:\n");

    io::stdout().flush()?;
    let redirect_input: String = Input::new().with_prompt("Redirect URL").interact_text()?;

    let (code, state) = oauth::parse_redirect_url(&redirect_input)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    if state != expected_state {
        return Err("state mismatch — possible CSRF attack or stale URL".into());
    }

    eprintln!("Exchanging code for tokens...");
    let tokens = oauth::openai_codex_exchange(&code, &verifier)?;

    eprintln!("Login successful!");
    Ok(Credentials::Oauth {
        provider_kind: kind.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: tokens.expires_at_ms,
        account_id: tokens.account_id,
    })
}

fn run_github_copilot_login(
    kind: &ProviderKind,
) -> Result<Credentials, Box<dyn std::error::Error>> {
    let device = oauth::github_device_code_start()?;

    eprintln!("\nGo to: {}", device.verification_uri);
    eprintln!("Enter code: {}\n", device.user_code);
    eprintln!("Waiting for authorization...");

    let github_token = oauth::github_device_code_poll(&device.device_code, device.interval)?;

    eprintln!("GitHub authorized. Fetching Copilot token...");
    let tokens = oauth::github_copilot_token(&github_token)?;

    eprintln!("Login successful!");
    Ok(Credentials::Oauth {
        provider_kind: kind.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: tokens.expires_at_ms,
        account_id: tokens.account_id,
    })
}

// ---------------------------------------------------------------------------
// models.json5 update
// ---------------------------------------------------------------------------

/// Build a `serde_json::Value` for the new provider entry.
fn build_provider_entry(kind: &ProviderKind) -> serde_json::Value {
    match kind {
        ProviderKind::Ollama => serde_json::json!({
            "baseUrl": "http://localhost:11434/v1",
            "auth": "none",
            "api": "openai-completions",
            "models": [{ "id": "llama3:70b", "contextWindow": 8192 }],
        }),
        ProviderKind::Openai => serde_json::json!({
            "auth": "api-key",
            "api": "openai-chat",
            "models": [
                { "id": "gpt-5.5", "contextWindow": 200000 },
                { "id": "gpt-5.5-mini", "contextWindow": 200000 },
                { "id": "o3-mini", "contextWindow": 200000 },
            ],
        }),
        ProviderKind::OpenaiCodex => serde_json::json!({
            "auth": "openai-codex",
            "api": "openai-chat",
            "models": [
                { "id": "gpt-5.5", "contextWindow": 200000 },
                { "id": "gpt-5.5-mini", "contextWindow": 200000 },
                { "id": "o3-mini", "contextWindow": 200000 },
            ],
        }),
        ProviderKind::Anthropic => serde_json::json!({
            "baseUrl": "https://api.anthropic.com/v1",
            "auth": "api-key",
            "api": "anthropic",
            "models": [
                { "id": "claude-opus-4-20250514", "contextWindow": 200000 },
                { "id": "claude-sonnet-4-20250514", "contextWindow": 200000 },
            ],
        }),
        ProviderKind::GithubCopilot => serde_json::json!({
            "auth": "github-copilot",
            "api": "openai-chat",
            "models": [
                { "id": "claude-sonnet-4.6", "contextWindow": 200000 },
                { "id": "gpt-5.5", "contextWindow": 200000 },
                { "id": "gemini-3-pro", "contextWindow": 1000000 },
            ],
        }),
    }
}

/// Path to `~/.config/tau/models.json5`.
fn models_json5_path() -> Option<PathBuf> {
    tau_config::settings::config_dir().map(|d| d.join("models.json5"))
}

/// Offer to overwrite models.json5 with the new provider added, or
/// print just the new section for the user to paste manually.
fn update_or_print_models_json5(
    name: &str,
    entry: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = models_json5_path();
    let can_write = path
        .as_ref()
        .is_some_and(|p| p.exists() || p.parent().is_some_and(|d| d.is_dir()));

    if can_write {
        let update = Confirm::new()
            .with_prompt("Update models.json5? (warning: comments will not be preserved)")
            .default(true)
            .interact()?;

        if update {
            let path = path.expect("checked above");
            match write_provider_to_models_json5(&path, name, entry) {
                Ok(()) => {
                    eprintln!("Updated: {}", path.display());
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("Failed to update {}: {e}", path.display());
                    eprintln!("Falling back to printing the entry instead.");
                }
            }
        }
    }

    print_provider_entry(name, entry);
    Ok(())
}

/// Print the provider entry for the user to paste into models.json5.
fn print_provider_entry(name: &str, entry: &serde_json::Value) {
    let inner = serde_json::to_string_pretty(entry).unwrap_or_default();
    eprintln!("\n--- Add this inside \"providers\" in ~/.config/tau/models.json5 ---\n");
    eprintln!("\"{name}\": {inner}");
}

/// Read existing models.json5, insert the provider, and atomically
/// replace the file via write-new + rename.
pub(crate) fn write_provider_to_models_json5(
    path: &Path,
    name: &str,
    entry: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut root: serde_json::Value = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        json5::from_str(&text)?
    } else {
        serde_json::json!({ "providers": {} })
    };

    let providers = root
        .as_object_mut()
        .ok_or("models.json5 root is not an object")?
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}));

    providers
        .as_object_mut()
        .ok_or("providers is not an object")?
        .insert(name.to_string(), entry.clone());

    let json = serde_json::to_string_pretty(&root)?;

    atomic_write_following_symlink(path, &json)?;

    Ok(())
}

/// Atomically write `contents` by creating a randomized sibling temporary file
/// and renaming it over the destination. If `path` is a symlink, replace the
/// symlink target instead of unlinking the symlink itself.
fn atomic_write_following_symlink(path: &Path, contents: &str) -> std::io::Result<()> {
    let destination = symlink_target_or_path(path)?;

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut temp_path = destination.clone();
    loop {
        let suffix: u64 = rand::random();
        let file_name = destination
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("models.json5");
        temp_path.set_file_name(format!(".{file_name}.{suffix:016x}.tmp"));

        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(mut file) => {
                file.write_all(contents.as_bytes())?;
                file.sync_all()?;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    if let Err(error) = std::fs::rename(&temp_path, &destination) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }

    Ok(())
}

fn symlink_target_or_path(path: &Path) -> std::io::Result<PathBuf> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path.to_path_buf()),
        Err(error) => return Err(error),
    };

    if !metadata.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }

    let target = std::fs::read_link(path)?;
    if target.is_absolute() {
        Ok(target)
    } else if let Some(parent) = path.parent() {
        Ok(parent.join(target))
    } else {
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn write_provider_to_models_json5_preserves_symlink() {
        let temp_dir = tempfile::tempdir().unwrap();
        let target = temp_dir.path().join("target.json5");
        let link = temp_dir.path().join("models.json5");
        std::fs::write(&target, r#"{ providers: {} }"#).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_provider_to_models_json5(
            &link,
            "openai",
            &serde_json::json!({
                "auth": "api-key",
                "api": "openai-chat",
                "models": [],
            }),
        )
        .unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let updated = std::fs::read_to_string(target).unwrap();
        assert!(updated.contains("openai"));
    }
}
