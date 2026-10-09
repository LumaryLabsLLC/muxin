//! How Muxen signs in to DigitalOcean, as the user chose in Settings › Cloud:
//!
//! - "token": a personal access token the user pasted, kept in the Keychain.
//! - "doctl": the token DigitalOcean's CLI saved when the user ran `doctl auth init`.
//! - "oauth": the browser sign-in (OAuth), with its tokens kept in the Keychain.
//!   Muxen is a public OAuth client: no secret ships in the app. Each sign-in
//!   proves itself with PKCE, and DigitalOcean sends the browser back to a
//!   local port. Access tokens last an hour and refresh by themselves; each
//!   refresh token works once.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::secrets;

pub const TOKEN: &str = "digitalocean-token";
const OAUTH: &str = "digitalocean-oauth";

/// Muxen's OAuth client at DigitalOcean: registered by Lumary Labs on
/// 2026-10-09 as a public client ("Muxen", https://muxin.dev, loopback
/// callbacks at /digitalocean), so it's no secret. `MUXEN_DO_CLIENT_ID` at build
/// time overrides it.
const CLIENT_ID: &str = "df4228c4a87c9e8d950e0a519d5f41796c0131356c188e5dcabe2e152d59f7b7";

/// The callback path; DigitalOcean matches loopback callbacks on any port.
const CALLBACK_PATH: &str = "/digitalocean";

/// What Muxen asks to do in the account, and nothing more.
const SCOPES: &str = "account:read regions:read sizes:read ssh_key:read ssh_key:create droplet:create droplet:read droplet:update droplet:delete actions:read";

fn client_id() -> Option<&'static str> {
    Some(option_env!("MUXEN_DO_CLIENT_ID").unwrap_or(CLIENT_ID)).filter(|id| !id.is_empty())
}

/// Whether this build can sign in with the browser.
pub fn oauth_available() -> bool {
    client_id().is_some()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OAuthTokens {
    access_token: String,
    refresh_token: String,
    /// Unix seconds.
    expires_at: i64,
}

/// A token for the API, by the chosen method.
pub async fn token(method: &str, dir: &Path) -> Result<String, String> {
    match method {
        "doctl" => doctl_token().ok_or_else(|| "doctl isn't signed in on this computer. Run `doctl auth init`, or pick another way in Settings › Cloud.".into()),
        "oauth" => oauth_token(dir).await,
        _ => secrets::get(dir, TOKEN).ok_or_else(|| "Connect DigitalOcean first: paste an API token in Settings › Cloud.".into()),
    }
}

/// Whether the chosen method has a sign-in to use (without asking DigitalOcean).
pub fn signed_in(method: &str, dir: &Path) -> bool {
    match method {
        "doctl" => doctl_token().is_some(),
        "oauth" => secrets::get(dir, OAUTH).is_some(),
        _ => secrets::get(dir, TOKEN).is_some(),
    }
}

/// Forgets the sign-in of a method (doctl's own login stays doctl's).
pub fn sign_out(method: &str, dir: &Path) {
    match method {
        "oauth" => secrets::delete(dir, OAUTH),
        "token" => secrets::delete(dir, TOKEN),
        _ => {}
    }
}

pub fn save_token(dir: &Path, token: &str) -> Result<(), String> {
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err("That doesn't look like a DigitalOcean token.".into());
    }
    secrets::set(dir, TOKEN, token)
}

// ---------------------------------------------------------------------------
// doctl

fn doctl_config() -> Option<PathBuf> {
    let candidates = [
        dirs::config_dir().map(|d| d.join("doctl").join("config.yaml")),
        dirs::home_dir().map(|h| h.join(".config").join("doctl").join("config.yaml")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

/// The token of doctl's current context, from its config.yaml.
fn doctl_token() -> Option<String> {
    if let Ok(t) = std::env::var("DIGITALOCEAN_ACCESS_TOKEN") {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    let text = std::fs::read_to_string(doctl_config()?).ok()?;
    doctl_token_in(&text)
}

fn doctl_token_in(text: &str) -> Option<String> {
    let top = |key: &str| {
        text.lines().find_map(|l| l.strip_prefix(&format!("{key}:")).map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())).filter(|v| !v.is_empty())
    };
    let context = top("context").unwrap_or_else(|| "default".into());
    if context == "default" {
        return top("access-token");
    }
    // auth-contexts:
    //   work: dop_v1_...
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("auth-contexts:") {
            inside = true;
            continue;
        }
        if inside {
            if !line.starts_with(' ') && !line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.trim().split_once(':') {
                if name.trim() == context {
                    return Some(value.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// OAuth

fn saved_tokens(dir: &Path) -> Result<OAuthTokens, String> {
    let saved = secrets::get(dir, OAUTH).ok_or("Sign in with DigitalOcean first (Settings › Cloud).")?;
    serde_json::from_str(&saved).map_err(|_| "The DigitalOcean sign-in is damaged; sign in again.".to_string())
}

async fn oauth_token(dir: &Path) -> Result<String, String> {
    let fresh_enough = |t: &OAuthTokens| t.expires_at - 120 > chrono::Utc::now().timestamp();
    let tokens = saved_tokens(dir)?;
    if fresh_enough(&tokens) {
        return Ok(tokens.access_token);
    }
    // A refresh token works once: one refresh at a time, and whoever waited
    // uses the tokens the first one saved.
    static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = REFRESH.lock().await;
    let tokens = saved_tokens(dir)?;
    if fresh_enough(&tokens) {
        return Ok(tokens.access_token);
    }
    let id = client_id().ok_or("This build of Muxen can't sign in with DigitalOcean.")?;
    let fresh = exchange(&[("grant_type", "refresh_token"), ("refresh_token", &tokens.refresh_token), ("client_id", id)])
        .await
        .map_err(|_| "The DigitalOcean sign-in expired. Sign in again in Settings › Cloud.".to_string())?;
    secrets::set(dir, OAUTH, &serde_json::to_string(&fresh).unwrap_or_default())?;
    Ok(fresh.access_token)
}

/// PKCE: a random verifier, and its SHA-256 (base64url) for the authorization request.
fn pkce() -> (String, String) {
    let verifier = crate::util::random_token(64);
    let challenge = challenge_for(&verifier);
    (verifier, challenge)
}

fn challenge_for(verifier: &str) -> String {
    use base64::Engine as _;
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

async fn exchange(params: &[(&str, &str)]) -> Result<OAuthTokens, String> {
    #[derive(Deserialize)]
    struct Answer {
        access_token: String,
        #[serde(default)]
        refresh_token: String,
        #[serde(default)]
        expires_in: i64,
    }
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let url = reqwest::Url::parse_with_params("https://cloud.digitalocean.com/v1/oauth/token", params).map_err(|e| e.to_string())?;
    let resp = http.post(url).send().await.map_err(|e| format!("Couldn't reach DigitalOcean: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("DigitalOcean didn't accept the sign-in ({}). Sign in again.", resp.status()));
    }
    let a: Answer = resp.json().await.map_err(|e| format!("Unexpected answer from DigitalOcean: {e}"))?;
    Ok(OAuthTokens { access_token: a.access_token, refresh_token: a.refresh_token, expires_at: chrono::Utc::now().timestamp() + a.expires_in.max(60) })
}

/// The browser sign-in: opens DigitalOcean's consent page (`open`), waits up
/// to five minutes for it to send the browser back here, and keeps the tokens.
pub async fn oauth_sign_in(dir: &Path, open: impl FnOnce(&str)) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let id = client_id().ok_or("This build of Muxen can't sign in with DigitalOcean yet. Use an API token or doctl.")?;
    // Any free port: DigitalOcean matches loopback callbacks on any port.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| format!("Couldn't wait for DigitalOcean's answer: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let callback = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let state = crate::util::random_token(24);
    let (verifier, challenge) = pkce();
    let url = reqwest::Url::parse_with_params(
        "https://cloud.digitalocean.com/v1/oauth/authorize",
        &[
            ("client_id", id),
            ("redirect_uri", callback.as_str()),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ],
    )
    .map_err(|e| e.to_string())?;
    open(url.as_str());
    let accept = async {
        loop {
            let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = request.split_whitespace().nth(1).unwrap_or("").to_string();
            let parsed = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok();
            let query = |k: &str| parsed.as_ref().and_then(|u| u.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.to_string()));
            if parsed.as_ref().is_none_or(|u| u.path() != CALLBACK_PATH) {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n").await;
                continue;
            }
            let (ok, body) = match (query("code"), query("state")) {
                (Some(code), Some(s)) if s == state => (Ok(code), "Signed in. You can close this tab and go back to Muxen."),
                _ => (Err(query("error_description").unwrap_or_else(|| "The sign-in was cancelled.".into())), "The sign-in didn't finish. Go back to Muxen and try again."),
            };
            let page = format!("<!doctype html><meta charset=utf-8><title>Muxen</title><body style=\"font:15px -apple-system,system-ui;padding:40px\">{body}</body>");
            let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}", page.len()).as_bytes()).await;
            return ok;
        }
    };
    let code = tokio::time::timeout(Duration::from_secs(300), accept).await.map_err(|_| "The sign-in timed out.".to_string())??;
    let tokens = exchange(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("client_id", id),
        ("code_verifier", &verifier),
        ("redirect_uri", &callback),
    ])
    .await?;
    secrets::set(dir, OAUTH, &serde_json::to_string(&tokens).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc() {
        // RFC 7636, appendix B.
        assert_eq!(challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let (v, c) = pkce();
        assert_eq!(v.len(), 64);
        assert_eq!(c, challenge_for(&v));
    }

    #[test]
    fn reads_doctl_contexts() {
        assert_eq!(doctl_token_in("access-token: dop_v1_a\ncontext: default\n").as_deref(), Some("dop_v1_a"));
        let multi = "access-token: dop_v1_a\nauth-contexts:\n  work: dop_v1_w\n  home: dop_v1_h\ncontext: home\n";
        assert_eq!(doctl_token_in(multi).as_deref(), Some("dop_v1_h"));
        assert_eq!(doctl_token_in("context: default\n"), None);
    }
}
