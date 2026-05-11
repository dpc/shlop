//! CLI entrypoint for `tau provider` subcommands.

use std::io::{self, Write};
use std::path::PathBuf;

use dialoguer::{Confirm, Input};
use tau_cli_picker::{PickerItem, pick};
use tau_config::settings::{AuthType, ModelConfig, ProviderConfig};
use tau_provider::oauth;
use tau_provider::storage::{self, Credentials, ProviderKind};

const HELP_TEXT: &str = "\
Usage: tau provider <subcommand>

Subcommands:
  add                 Add a new provider (interactive wizard)
  remove [name]       Remove a provider from models.json5 and auth.json
  list                List configured providers
  login [name]        Log in / refresh OAuth token for a provider
  list-models [name]  List models available from a provider";

/// Run the provider CLI with the given subcommand arguments.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let subcommand = args.first().map(String::as_str).unwrap_or("help");

    match subcommand {
        "add" => cmd_add()?,
        "remove" => cmd_remove(args.get(1).map(String::as_str))?,
        "list" => cmd_list()?,
        "login" => cmd_login(args.get(1).map(String::as_str))?,
        "list-models" => cmd_list_models(args.get(1).map(String::as_str))?,
        "help" | "--help" | "-h" => println!("{HELP_TEXT}"),
        other => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("{HELP_TEXT}");
            return Err(format!("unknown subcommand: {other}").into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tau provider add
// ---------------------------------------------------------------------------

fn cmd_add() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Pick provider kind.
    let kinds = ProviderKind::all();
    let kind_names = kinds
        .iter()
        .map(|kind| PickerItem::enabled(kind.display_name()))
        .collect::<Vec<_>>();

    let selection = pick("Provider type", &kind_names)?;
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

            let items = names
                .iter()
                .map(|name| PickerItem::enabled(*name))
                .collect::<Vec<_>>();
            let sel = pick("Which provider to remove?", &items)?;
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

    if tau_config::settings::remove_provider(&name)? {
        eprintln!("Removed '{name}' from models.json5.");
        removed_anything = true;
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

        // Resolved AuthType (Ok) or the raw unknown string (Err); used both
        // to drive the OAuth-status branch and as the displayed value.
        let auth_type = model_info.map(|p| p.auth_type());

        let auth_status = match auth_info {
            Some(Credentials::Oauth { expires_at_ms, .. }) => {
                let now_ms: u64 = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                if now_ms < *expires_at_ms {
                    "logged in".to_string()
                } else {
                    "expired".to_string()
                }
            }
            _ if matches!(auth_type, Some(Ok(t)) if t.is_oauth()) => match auth_info {
                Some(_) => "logged in".to_string(),
                None => "not logged in".to_string(),
            },
            _ => match auth_type {
                Some(Ok(t)) => t.to_string(),
                Some(Err(raw)) => format!("?{raw}"),
                None => "-".to_string(),
            },
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

    let mut oauth_names: Vec<String> = models
        .providers
        .iter()
        .filter(|(_, cfg)| cfg.auth_type().is_ok_and(|t| t.is_oauth()))
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

            let items = oauth_names
                .iter()
                .map(|name| PickerItem::enabled(name.as_str()))
                .collect::<Vec<_>>();
            let sel = pick("Which provider to log in to?", &items)?;
            oauth_names[sel].to_string()
        }
    };

    let provider_cfg = models
        .providers
        .get(&name)
        .ok_or_else(|| format!("provider '{name}' not found in models.json5"))?;

    let auth_type = provider_cfg
        .auth_type()
        .map_err(|s| format!("unknown auth type for '{name}': {s}"))?;

    let new_creds = match auth_type {
        AuthType::OpenaiCodex => run_openai_codex_login(&ProviderKind::OpenaiCodex)?,
        AuthType::GithubCopilot => run_github_copilot_login(&ProviderKind::GithubCopilot)?,
        AuthType::ApiKey | AuthType::None => {
            eprintln!("Provider '{name}' (auth={auth_type}) does not use OAuth login.");
            return Ok(());
        }
    };

    store.providers.insert(name.clone(), new_creds);
    storage::save(&store)?;
    eprintln!("Login refreshed for '{name}'.");
    Ok(())
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
            let items = names
                .iter()
                .map(|name| PickerItem::enabled(*name))
                .collect::<Vec<_>>();
            let sel = pick("Which provider?", &items)?;
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

    let github_token =
        oauth::github_device_code_poll(&device.device_code, device.interval, device.expires_in)?;

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

/// Build a typed [`ProviderConfig`] with sensible defaults for the chosen
/// [`ProviderKind`]. The user is expected to edit the result afterwards;
/// the entries here are only starting points.
fn build_provider_entry(kind: &ProviderKind) -> ProviderConfig {
    fn model(id: &str, context_window: u64) -> ModelConfig {
        ModelConfig {
            id: id.to_owned(),
            name: None,
            max_output_tokens: None,
            context_window: Some(context_window),
        }
    }

    let mut config = ProviderConfig::default();
    match kind {
        ProviderKind::Ollama => {
            config.base_url = Some("http://localhost:11434/v1".to_owned());
            config.auth = Some(AuthType::None.as_str().to_owned());
            config.api = Some("openai-completions".to_owned());
            config.compat.supports_llama_cpp_cache = true;
            config.models = vec![model("llama3:70b", 8192)];
        }
        ProviderKind::Openai => {
            config.auth = Some(AuthType::ApiKey.as_str().to_owned());
            config.api = Some("openai-chat".to_owned());
            config.models = vec![
                model("gpt-5.5", 200_000),
                model("gpt-5.5-mini", 200_000),
                model("o3-mini", 200_000),
            ];
        }
        ProviderKind::OpenaiCodex => {
            config.auth = Some(AuthType::OpenaiCodex.as_str().to_owned());
            config.api = Some("openai-chat".to_owned());
            config.models = vec![
                model("gpt-5.5", 200_000),
                model("gpt-5.5-mini", 200_000),
                model("o3-mini", 200_000),
            ];
        }
        ProviderKind::Anthropic => {
            config.base_url = Some("https://api.anthropic.com/v1".to_owned());
            config.auth = Some(AuthType::ApiKey.as_str().to_owned());
            config.api = Some("anthropic".to_owned());
            config.models = vec![
                model("claude-opus-4-20250514", 200_000),
                model("claude-sonnet-4-20250514", 200_000),
            ];
        }
        ProviderKind::GithubCopilot => {
            config.auth = Some(AuthType::GithubCopilot.as_str().to_owned());
            config.api = Some("openai-chat".to_owned());
            config.models = vec![
                model("claude-sonnet-4.6", 200_000),
                model("gpt-5.5", 200_000),
                model("gemini-3-pro", 1_000_000),
            ];
        }
    }
    config
}

/// Path to `~/.config/tau/models.json5`.
fn models_json5_path() -> Option<PathBuf> {
    tau_config::settings::config_dir().map(|d| d.join("models.json5"))
}

/// Offer to overwrite models.json5 with the new provider added, or
/// print just the new section for the user to paste manually.
fn update_or_print_models_json5(
    name: &str,
    entry: &ProviderConfig,
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
            match tau_config::settings::add_provider(name, entry) {
                Ok(written_path) => {
                    eprintln!("Updated: {}", written_path.display());
                    return Ok(());
                }
                Err(e) => {
                    let displayed = path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<unknown>".to_owned());
                    eprintln!("Failed to update {displayed}: {e}");
                    eprintln!("Falling back to printing the entry instead.");
                }
            }
        }
    }

    print_provider_entry(name, entry);
    Ok(())
}

/// Print the provider entry for the user to paste into models.json5.
fn print_provider_entry(name: &str, entry: &ProviderConfig) {
    let inner = serde_json::to_string_pretty(entry).unwrap_or_default();
    eprintln!("\n--- Add this inside \"providers\" in ~/.config/tau/models.json5 ---\n");
    eprintln!("\"{name}\": {inner}");
}

#[cfg(test)]
mod tests;
