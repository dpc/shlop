//! OAuth flows: auth-code + PKCE (manual paste) and device-code (polling).

use std::collections::HashMap;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};
use url::Url;

// ---------------------------------------------------------------------------
// PKCE helpers
// ---------------------------------------------------------------------------

/// Generate a random code verifier (64 unreserved characters).
fn generate_code_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut buf = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter()
        .map(|b| CHARSET[(*b as usize) % CHARSET.len()] as char)
        .collect()
}

/// Generate a random state parameter (32 hex chars).
fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compute S256 code challenge from verifier.
fn code_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hash)
}

// ---------------------------------------------------------------------------
// OpenAI Codex (Auth Code + PKCE, manual paste)
// ---------------------------------------------------------------------------

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// Result of a successful OAuth token exchange.
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: u64,
    pub account_id: Option<String>,
}

/// Build the authorization URL for OpenAI Codex. Returns (url, state,
/// code_verifier) — the caller must present the URL to the user.
pub fn openai_codex_auth_url() -> (String, String, String) {
    let verifier = generate_code_verifier();
    let challenge = code_challenge(&verifier);
    let state = generate_state();

    let url = format!(
        "{OPENAI_AUTH_URL}?client_id={client_id}&redirect_uri={redirect}&response_type=code&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}&codex_cli_simplified_flow=true",
        client_id = OPENAI_CLIENT_ID,
        redirect = urlencoding(OPENAI_REDIRECT_URI),
        scope = urlencoding("openid profile email offline_access"),
    );

    (url, state, verifier)
}

/// Parse the redirect URL pasted by the user. Extracts `code` and
/// `state` query parameters.
pub fn parse_redirect_url(input: &str) -> Result<(String, String), String> {
    // User might paste the full URL or just the query part.
    let url = if input.starts_with("http") {
        Url::parse(input.trim()).map_err(|e| format!("invalid URL: {e}"))?
    } else {
        // Try prepending a dummy base.
        Url::parse(&format!("http://localhost{}", input.trim()))
            .map_err(|e| format!("invalid URL fragment: {e}"))?
    };

    let params: HashMap<_, _> = url.query_pairs().collect();
    let code = params
        .get("code")
        .ok_or("no 'code' parameter in URL")?
        .to_string();
    let state = params
        .get("state")
        .ok_or("no 'state' parameter in URL")?
        .to_string();

    Ok((code, state))
}

/// Exchange authorization code for tokens (OpenAI Codex).
pub fn openai_codex_exchange(code: &str, verifier: &str) -> Result<OAuthTokens, io::Error> {
    let body = format!(
        "grant_type=authorization_code&code={code}&code_verifier={verifier}&redirect_uri={redirect}&client_id={client_id}",
        redirect = urlencoding(OPENAI_REDIRECT_URI),
        client_id = OPENAI_CLIENT_ID,
    );

    let json = post_form(OPENAI_TOKEN_URL, &body)?;
    parse_openai_token_response(&json)
}

/// Refresh an OpenAI Codex access token using the refresh token.
pub fn openai_codex_refresh(refresh_token: &str) -> Result<OAuthTokens, io::Error> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}",
        client_id = OPENAI_CLIENT_ID,
    );

    let json = post_form(OPENAI_TOKEN_URL, &body)?;
    parse_openai_token_response(&json)
}

fn parse_openai_token_response(json: &serde_json::Value) -> Result<OAuthTokens, io::Error> {
    let access_token = json["access_token"]
        .as_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing access_token"))?
        .to_string();
    let refresh_token = json["refresh_token"]
        .as_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing refresh_token"))?
        .to_string();
    let expires_in = json["expires_in"]
        .as_u64()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing expires_in"))?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64;
    let expires_at_ms = now_ms + expires_in * 1000;

    // Try to extract account_id from JWT claims.
    let account_id = extract_openai_account_id(&access_token);

    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at_ms,
        account_id,
    })
}

/// Decode JWT payload (no verification) to extract OpenAI account ID.
fn extract_openai_account_id(jwt: &str) -> Option<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// GitHub Copilot (Device Code Flow)
// ---------------------------------------------------------------------------

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c887ecfe98";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Device code flow step 1 response.
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
}

/// Start the GitHub device code flow.
pub fn github_device_code_start() -> Result<DeviceCodeResponse, io::Error> {
    let body = format!("client_id={GITHUB_CLIENT_ID}&scope=read:user");

    let json = post_form_with_accept(GITHUB_DEVICE_CODE_URL, &body, "application/json")?;

    Ok(DeviceCodeResponse {
        device_code: json["device_code"].as_str().unwrap_or_default().to_string(),
        user_code: json["user_code"].as_str().unwrap_or_default().to_string(),
        verification_uri: json["verification_uri"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        interval: json["interval"].as_u64().unwrap_or(5),
    })
}

/// Poll for device code flow completion. Blocks until success or timeout.
pub fn github_device_code_poll(device_code: &str, interval: u64) -> Result<String, io::Error> {
    let mut wait = Duration::from_secs(interval);

    loop {
        std::thread::sleep(wait);

        let body = format!(
            "client_id={GITHUB_CLIENT_ID}&device_code={device_code}&grant_type=urn:ietf:params:oauth:grant-type:device_code"
        );

        let json = post_form_with_accept(GITHUB_TOKEN_URL, &body, "application/json")?;

        if let Some(token) = json["access_token"].as_str() {
            return Ok(token.to_string());
        }

        match json["error"].as_str() {
            Some("authorization_pending") => {} // keep polling
            Some("slow_down") => {
                wait = wait.mul_f32(1.4);
            }
            Some(err) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("device code flow failed: {err}"),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected response from GitHub",
                ));
            }
        }
    }
}

/// Exchange GitHub access token for a Copilot token.
pub fn github_copilot_token(github_token: &str) -> Result<OAuthTokens, io::Error> {
    let resp = ureq::get(GITHUB_COPILOT_TOKEN_URL)
        .set("Authorization", &format!("Bearer {github_token}"))
        .set("Accept", "application/json")
        .call()
        .map_err(|e| io::Error::other(e.to_string()))?;

    let json = read_json(resp)?;

    let token = json["token"]
        .as_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing token"))?
        .to_string();
    let expires_at = json["expires_at"]
        .as_u64()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing expires_at"))?;

    Ok(OAuthTokens {
        access_token: token,
        refresh_token: github_token.to_string(), // GitHub token is the "refresh" token
        expires_at_ms: expires_at * 1000,
        account_id: None,
    })
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// POST a form-encoded body and parse JSON response.
fn post_form(url: &str, body: &str) -> Result<serde_json::Value, io::Error> {
    let resp = ureq::post(url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(body)
        .map_err(|e| io::Error::other(e.to_string()))?;
    read_json(resp)
}

/// POST a form-encoded body with custom Accept header and parse JSON
/// response.
fn post_form_with_accept(
    url: &str,
    body: &str,
    accept: &str,
) -> Result<serde_json::Value, io::Error> {
    let resp = ureq::post(url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", accept)
        .send_string(body)
        .map_err(|e| io::Error::other(e.to_string()))?;
    read_json(resp)
}

/// Read a ureq response body as JSON.
fn read_json(resp: ureq::Response) -> Result<serde_json::Value, io::Error> {
    let text = resp
        .into_string()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
