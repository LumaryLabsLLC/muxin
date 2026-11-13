//! Signing in with ChatGPT, so the chat's own brain (brain/responses.rs) can
//! answer on the user's ChatGPT plan: no API key, and no other program.
//!
//! This is OpenAI's "Sign in with ChatGPT" for open-source apps that run on
//! the user's computer (developers.openai.com/siwc). Muxin is a public
//! client: no secret ships in it.
//!
//! - The first sign-in registers this copy of Muxin for the account: the
//!   browser is sent off with `client_id=dynamic_agent_client`, and comes
//!   back with a client id of Muxin's own for that account, which is kept
//!   and used from then on (signing in again doesn't register another).
//! - Each sign-in proves itself with PKCE, and OpenAI sends the browser back
//!   to a port on this computer.
//! - What comes back is checked before any of it is kept: the identity
//!   token's signature and claims (jwt.rs), and that the user allowed plan
//!   use, which signing in alone doesn't mean.
//! - Access tokens last an hour and are renewed by themselves; each renewal
//!   replaces the token it's renewed with, so one renewal runs at a time,
//!   across every Muxin on this computer.
//!
//! One account at a time. Tokens never go in a URL or a log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::jwt;
use crate::oauth::{self, Loopback};

/// Where the user reviews and limits what apps use of their plan.
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
/// The permission that lets requests use the plan. Signing in alone doesn't grant it.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// What OpenAI shows the user as this app's name, the same on every computer.
const APP_NAME: &str = "Muxin";
/// What a first sign-in is sent with instead of a client id of Muxin's own.
const REGISTER: &str = "dynamic_agent_client";
/// Where the browser comes back. Only the port may ever differ between sign-ins.
const CALLBACK_PATH: &str = "/auth/callback";
const FIRST_PORT: u16 = 1455;
/// Renewed this long before it runs out.
const RENEW_BEFORE: i64 = 300;
/// What the token endpoint says when the sign-in itself is over.
const SIGN_IN_AGAIN: [&str; 6] = ["invalid_grant", "invalid_refresh_token", "token_expired", "refresh_token_expired", "refresh_token_invalidated", "refresh_token_reused"];

/// OpenAI's addresses (others in tests).
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    /// The issuer's description of itself: where its keys are, where to end a sign-in.
    pub discovery: String,
    /// What the tokens are for, and where requests go (`/responses`, `/models`).
    pub api: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            issuer: "https://auth.openai.com".into(),
            authorize: "https://auth.openai.com/api/accounts/authorize".into(),
            token: "https://auth.openai.com/api/accounts/oauth/token".into(),
            discovery: "https://auth.openai.com/.well-known/openid-configuration".into(),
            api: "https://api.openai.com/v1".into(),
        }
    }
}

/// What's kept of the sign-in.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Record {
    /// This computer's copy of Muxin, to OpenAI: made once, before the first
    /// sign-in, and never changed. It identifies nothing about the user.
    host_id: String,
    /// Muxin's client id for this account, from its first sign-in. Kept
    /// through signing out, so signing in again doesn't register another.
    client_id: String,
    /// The account, as OpenAI names it.
    subject: String,
    email: String,
    /// Kept to tell OpenAI which account a later sign-in is for.
    id_token: String,
    access_token: String,
    /// Works once: each renewal returns the next.
    refresh_token: String,
    /// Unix seconds.
    expires_at: i64,
    scopes: Vec<String>,
    /// The user was told, once, that requests here use their plan.
    welcomed: bool,
}

impl Record {
    fn signed_in(&self) -> bool {
        !self.refresh_token.is_empty() && !self.client_id.is_empty()
    }

    fn plan(&self) -> bool {
        self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }

    fn account(&self) -> Account {
        Account { email: self.email.clone(), plan: self.plan(), welcomed: self.welcomed }
    }

    fn forget_tokens(&mut self) {
        self.id_token.clear();
        self.access_token.clear();
        self.refresh_token.clear();
        self.expires_at = 0;
        self.scopes.clear();
    }
}

/// The signed-in account, for showing.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub email: String,
    /// The user allowed requests to use their plan.
    pub plan: bool,
    /// They've been told so once already.
    pub welcomed: bool,
}

/// Where the sign-in is kept: the Keychain on a Mac; elsewhere a file only
/// the user can read, next to config.toml (it's too large for Windows'
/// Credential Manager).
#[derive(Clone, Debug)]
pub enum Vault {
    Keychain(PathBuf),
    File(PathBuf),
}

const KEYCHAIN_NAME: &str = "chatgpt-account";

impl Vault {
    /// The sign-in kept for the Muxin whose folder is `dir`.
    ///
    /// `MUXIN_CHATGPT_VAULT` names a file to keep it in instead, for checking
    /// a build that isn't signed: macOS asks the user each time such a build
    /// reads the Keychain, and waits for the answer before anything else.
    pub fn of(dir: &Path) -> Vault {
        if let Some(path) = std::env::var_os("MUXIN_CHATGPT_VAULT").filter(|p| !p.is_empty()) {
            return Vault::File(path.into());
        }
        if cfg!(target_os = "macos") { Vault::Keychain(dir.to_path_buf()) } else { Vault::File(dir.join("chatgpt.json")) }
    }

    fn load(&self) -> Record {
        let text = match self {
            Vault::Keychain(dir) => crate::cloud::secrets::get(dir, KEYCHAIN_NAME),
            Vault::File(path) => std::fs::read_to_string(path).ok(),
        };
        text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self, record: &Record) -> Result<(), String> {
        let text = serde_json::to_string(record).map_err(|e| e.to_string())?;
        match self {
            Vault::Keychain(dir) => crate::cloud::secrets::set(dir, KEYCHAIN_NAME, &text),
            Vault::File(path) => {
                if let Some(folder) = path.parent() {
                    let _ = std::fs::create_dir_all(folder);
                }
                crate::config::write_atomic(path, text.as_bytes()).map_err(|e| format!("Couldn't save the sign-in: {e:#}"))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                Ok(())
            }
        }
    }

    /// Held while a token is renewed, so two Muxins don't both spend the
    /// same one. Waits up to a minute for another's renewal to finish.
    async fn lock(&self) -> Option<std::fs::File> {
        let path = match self {
            Vault::Keychain(dir) => dir.join("chatgpt.lock"),
            Vault::File(path) => path.with_extension("lock"),
        };
        if let Some(folder) = path.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
        for _ in 0..600 {
            if file.try_lock().is_ok() {
                return Some(file);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `urn:uuid:` and a random (version 4) UUID.
fn new_host_id() -> String {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("urn:uuid:{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

fn http() -> reqwest::Client {
    crate::util::http_client(|| reqwest::Client::builder().timeout(Duration::from_secs(30)))
}

/// What the token endpoint returned.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_in: i64,
    scope: String,
}

/// Why the token endpoint refused, or couldn't be reached.
#[derive(Debug)]
enum Refused {
    /// It answered no, with this code ("invalid_grant").
    Code(String),
    /// It couldn't be asked, or didn't answer as expected: nothing is known
    /// about the sign-in, so nothing kept is thrown away.
    Unknown(String),
}

/// A form-encoded POST to the token endpoint.
async fn token_request(at: &Endpoints, form: &[(&str, &str)]) -> Result<Tokens, Refused> {
    let body = reqwest::Url::parse_with_params("http://form/", form).ok().and_then(|u| u.query().map(str::to_string)).unwrap_or_default();
    let resp = http().post(&at.token).header("content-type", "application/x-www-form-urlencoded").body(body).send().await.map_err(|e| Refused::Unknown(format!("Couldn't reach ChatGPT's sign-in: {e}")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let answer: Value = serde_json::from_str(&text).unwrap_or_default();
        let code = answer["error"].as_str().or_else(|| answer["error"]["code"].as_str());
        return Err(match code {
            Some(code) if status.is_client_error() => Refused::Code(code.to_string()),
            _ => Refused::Unknown(format!("ChatGPT's sign-in answered {status}.")),
        });
    }
    serde_json::from_str(&text).map_err(|_| Refused::Unknown("ChatGPT's sign-in answered with something Muxin can't read.".into()))
}

/// The issuer's description of itself.
async fn discovery(at: &Endpoints) -> Result<Value, String> {
    let resp = http().get(&at.discovery).send().await.map_err(|e| format!("Couldn't reach ChatGPT's sign-in: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ChatGPT's sign-in answered {} when asked about itself.", resp.status()));
    }
    resp.json().await.map_err(|_| "ChatGPT's sign-in described itself in a way Muxin can't read.".to_string())
}

/// The signed-in account, or None.
pub fn current(vault: &Vault) -> Option<Account> {
    let record = vault.load();
    record.signed_in().then(|| record.account())
}

/// Remembers that the user was told their plan is in use here (they're told once).
pub fn welcomed(vault: &Vault) {
    let mut record = vault.load();
    if record.signed_in() && !record.welcomed {
        record.welcomed = true;
        let _ = vault.save(&record);
    }
}

/// Signs in: opens OpenAI's page in the browser (`open` is given its
/// address), waits up to five minutes for the browser to come back here,
/// checks what came back, and keeps it.
///
/// `Ok` with `plan: false` when the user signed in but didn't allow plan use.
pub async fn sign_in(vault: &Vault, at: &Endpoints, open: impl FnOnce(&str)) -> Result<Account, String> {
    let mut record = vault.load();
    if record.host_id.is_empty() {
        // Before the first sign-in, and from then on the same.
        record.host_id = new_host_id();
        vault.save(&record)?;
    }
    let returning = !record.client_id.is_empty();
    let loopback = Loopback::bind(&[FIRST_PORT], CALLBACK_PATH).await?;
    let redirect = loopback.redirect_uri.clone();
    let (state, nonce) = (crate::util::random_token(32), crate::util::random_token(32));
    let (verifier, challenge) = oauth::pkce();

    let mut params: Vec<(&str, &str)> = vec![("client_id", if returning { &record.client_id } else { REGISTER })];
    if !returning {
        params.push(("agent_name_hint", APP_NAME));
    }
    params.push(("ext_agent_host_id", &record.host_id));
    if returning && !record.id_token.is_empty() {
        // Which account this is for, so the user isn't asked to pick it again.
        params.push(("id_token_hint", &record.id_token));
    }
    if returning && !record.email.is_empty() {
        params.push(("login_hint", &record.email));
    }
    params.extend([
        ("response_type", "code"),
        ("redirect_uri", redirect.as_str()),
        ("scope", SCOPES),
        ("resource", at.api.as_str()),
        ("state", state.as_str()),
        ("nonce", nonce.as_str()),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge.as_str()),
    ]);
    let url = reqwest::Url::parse_with_params(&at.authorize, &params).map_err(|e| e.to_string())?;
    open(url.as_str());

    let expected = state.clone();
    let query = loopback.wait(Duration::from_secs(300), move |q| q.get("state") == Some(&expected) && q.contains_key("code") && !q.contains_key("error")).await?;
    // That it answers this attempt comes first: anything else in it is only then worth reading.
    if query.get("state") != Some(&state) {
        return Err("What came back wasn't the answer to this sign-in. Try again.".into());
    }
    if let Some(error) = query.get("error") {
        return Err(match error.as_str() {
            "access_denied" => "You didn't allow it, so nothing changed.".into(),
            other => format!("ChatGPT didn't finish the sign-in ({other})."),
        });
    }
    let code = query.get("code").filter(|c| !c.is_empty()).ok_or("ChatGPT's answer had no sign-in code. Try again.")?;
    let client_id = match (returning, query.get("client_id").filter(|id| !id.is_empty())) {
        (false, Some(id)) if id != REGISTER => id.clone(),
        (false, _) => return Err("ChatGPT didn't finish registering Muxin for your account. Try again.".into()),
        (true, Some(id)) if *id != record.client_id => return Err("ChatGPT answered for another registration than the one Muxin has. Nothing changed.".into()),
        (true, _) => record.client_id.clone(),
    };
    if !returning {
        // Kept before the code is exchanged: if that fails, the next attempt
        // goes on with this registration rather than making another.
        record.client_id = client_id.clone();
        vault.save(&record)?;
    }

    let form = [("grant_type", "authorization_code"), ("client_id", client_id.as_str()), ("code", code.as_str()), ("code_verifier", verifier.as_str()), ("redirect_uri", redirect.as_str()), ("resource", at.api.as_str())];
    let tokens = token_request(at, &form).await.map_err(|refused| match refused {
        Refused::Code(code) => format!("ChatGPT didn't accept the sign-in ({code}). Try again."),
        Refused::Unknown(why) => why,
    })?;
    if tokens.id_token.is_empty() || tokens.access_token.is_empty() {
        return Err("ChatGPT's answer was missing what a sign-in needs. Try again.".into());
    }
    let about = discovery(at).await?;
    let keys_at = about["jwks_uri"].as_str().ok_or("ChatGPT's sign-in didn't say where its keys are.")?;
    let keys: Value = http().get(keys_at).send().await.map_err(|e| format!("Couldn't get ChatGPT's keys: {e}"))?.json().await.map_err(|_| "ChatGPT's keys aren't readable.".to_string())?;
    let claims = jwt::verified(&tokens.id_token, &keys)?;
    let who = jwt::identity(&claims, &at.issuer, &client_id, &nonce, now())?;
    if returning && !record.subject.is_empty() && record.subject != who.subject {
        return Err("That's another ChatGPT account than the one Muxin is registered for, so nothing changed. To switch accounts: `muxin chat --sign-out --forget`, then sign in.".into());
    }

    record.subject = who.subject;
    record.email = who.email;
    record.id_token = tokens.id_token;
    record.access_token = tokens.access_token;
    record.refresh_token = tokens.refresh_token;
    record.expires_at = now() + tokens.expires_in.max(60);
    record.scopes = tokens.scope.split_whitespace().map(String::from).collect();
    vault.save(&record)?;
    Ok(record.account())
}

/// A token for a request on the user's plan, renewed first when it's about
/// to run out, or when it's `refused`: the token a request was just turned
/// away with.
pub async fn token(vault: &Vault, at: &Endpoints, refused: Option<&str>) -> Result<String, String> {
    let usable = |r: &Record| !r.access_token.is_empty() && Some(r.access_token.as_str()) != refused && r.expires_at - RENEW_BEFORE > now();
    let check = |r: &Record| {
        if !r.signed_in() {
            return Err("Sign in with ChatGPT first: `muxin chat --sign-in`.".to_string());
        }
        if !r.plan() {
            return Err("You're signed in with ChatGPT, but didn't allow Muxin to use your plan. Sign in again and allow it: `muxin chat --sign-in`.".to_string());
        }
        Ok(())
    };
    let record = vault.load();
    check(&record)?;
    if usable(&record) {
        return Ok(record.access_token);
    }
    // One renewal at a time, in this Muxin and across them: whoever waited
    // uses what the first one kept.
    static RENEWING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = RENEWING.lock().await;
    let _file = vault.lock().await;
    let mut record = vault.load();
    check(&record)?;
    if usable(&record) {
        return Ok(record.access_token);
    }
    let form = [("grant_type", "refresh_token"), ("client_id", record.client_id.as_str()), ("refresh_token", record.refresh_token.as_str()), ("resource", at.api.as_str())];
    match token_request(at, &form).await {
        Ok(tokens) if !tokens.access_token.is_empty() => {
            record.access_token = tokens.access_token;
            if !tokens.refresh_token.is_empty() {
                record.refresh_token = tokens.refresh_token;
            }
            if !tokens.id_token.is_empty() {
                record.id_token = tokens.id_token;
            }
            if !tokens.scope.is_empty() {
                record.scopes = tokens.scope.split_whitespace().map(String::from).collect();
            }
            record.expires_at = now() + tokens.expires_in.max(60);
            vault.save(&record)?;
            Ok(record.access_token)
        }
        Ok(_) => Err("ChatGPT's sign-in answered without a token. Try again.".into()),
        Err(Refused::Code(code)) if SIGN_IN_AGAIN.contains(&code.as_str()) => {
            // The sign-in is over. The registration stays, for the next one.
            record.forget_tokens();
            vault.save(&record)?;
            Err("Your ChatGPT sign-in has ended. Sign in again: `muxin chat --sign-in`.".into())
        }
        Err(Refused::Code(code)) => Err(format!("ChatGPT's sign-in refused Muxin ({code}). Nothing was changed.")),
        // Not reached, or not understood: the sign-in may be fine.
        Err(Refused::Unknown(why)) => Err(why),
    }
}

/// Signs out: ends the sign-in at OpenAI, then forgets its tokens here. The
/// registration is kept for signing in again, unless `forget` (to use
/// another account). `Ok(false)` when it's forgotten here but OpenAI
/// couldn't be told.
pub async fn sign_out(vault: &Vault, at: &Endpoints, forget: bool) -> Result<bool, String> {
    let mut record = vault.load();
    let mut told = true;
    if record.signed_in() {
        told = revoke(at, &record).await;
    }
    record.forget_tokens();
    if forget {
        record = Record { host_id: record.host_id, ..Record::default() };
    }
    vault.save(&record)?;
    Ok(told)
}

/// Tells OpenAI the sign-in is over. True when it said so (it does for a
/// token that had already ended, too).
async fn revoke(at: &Endpoints, record: &Record) -> bool {
    let Ok(about) = discovery(at).await else { return false };
    let Some(endpoint) = about["revocation_endpoint"].as_str() else { return false };
    let form = [("token", record.refresh_token.as_str()), ("token_type_hint", "refresh_token"), ("client_id", record.client_id.as_str())];
    let body = reqwest::Url::parse_with_params("http://form/", &form).ok().and_then(|u| u.query().map(str::to_string)).unwrap_or_default();
    for wait in [0, 1, 3] {
        tokio::time::sleep(Duration::from_secs(wait)).await;
        match http().post(endpoint).header("content-type", "application/x-www-form-urlencoded").body(body.clone()).send().await {
            Ok(resp) if resp.status().is_success() => return true,
            // It was asked and said no: asking again won't change that.
            Ok(resp) if resp.status().is_client_error() => return false,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::chat::jwt::tests::Signer;
    use crate::chat::testing::{self, Asked, Server};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    pub(crate) fn vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("muxin-account-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::File(dir.join("chatgpt.json"))
    }

    /// A stand-in for OpenAI's sign-in. `grant` is what the user allowed.
    pub(crate) struct FakeOpenAi {
        pub server: Server,
        pub at: Endpoints,
        /// The nonce of the sign-in under way, for the identity token.
        nonce: Arc<Mutex<String>>,
        /// What the token endpoint answers a renewal with: a status and a body.
        pub renewal: Arc<Mutex<(u16, Value)>>,
    }

    impl FakeOpenAi {
        pub(crate) async fn start(subject: &'static str, grant: &'static str) -> FakeOpenAi {
            let signer = Signer::new();
            let nonce = Arc::new(Mutex::new(String::new()));
            let renewal = Arc::new(Mutex::new((200, json!({"access_token": "access-2", "refresh_token": "refresh-2", "expires_in": 3600, "scope": grant}))));
            let own_url = Arc::new(Mutex::new(String::new()));
            let (n, r, u) = (nonce.clone(), renewal.clone(), own_url.clone());
            let server = Server::start(move |asked: &Asked| {
                let url = u.lock().unwrap().clone();
                match asked.path.as_str() {
                    "/discovery" => testing::json(200, json!({"issuer": url, "jwks_uri": format!("{url}/keys"), "revocation_endpoint": format!("{url}/revoke")})),
                    "/keys" => testing::json(200, signer.jwks()),
                    "/revoke" => (200, "text/plain", String::new()),
                    "/token" => {
                        let form = asked.form();
                        if form["grant_type"] == "refresh_token" {
                            let (status, body) = r.lock().unwrap().clone();
                            return testing::json(status, body);
                        }
                        let claims = json!({"iss": url, "sub": subject, "aud": [form["client_id"]], "nonce": *n.lock().unwrap(), "email": "alex@example.com", "iat": now(), "exp": now() + 3600});
                        testing::json(200, json!({"access_token": "access-1", "refresh_token": "refresh-1", "id_token": signer.token(&claims), "token_type": "Bearer", "expires_in": 3600, "scope": grant}))
                    }
                    _ => (404, "text/plain", String::new()),
                }
            })
            .await;
            *own_url.lock().unwrap() = server.url.clone();
            let at = Endpoints { issuer: server.url.clone(), authorize: format!("{}/authorize", server.url), token: format!("{}/token", server.url), discovery: format!("{}/discovery", server.url), api: format!("{}/v1", server.url) };
            FakeOpenAi { server, at, nonce, renewal }
        }

        /// What the browser would do: reads the address Muxin opened, and
        /// comes back to Muxin's port with `answer(query)` added to it.
        pub(crate) fn browser(&self, answer: impl Fn(&std::collections::HashMap<String, String>) -> Vec<(&'static str, String)> + Send + 'static) -> (impl FnOnce(&str), Arc<Mutex<std::collections::HashMap<String, String>>>) {
            let seen = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let (kept, nonce) = (seen.clone(), self.nonce.clone());
            let open = move |address: &str| {
                let url = reqwest::Url::parse(address).unwrap();
                let query: std::collections::HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                *nonce.lock().unwrap() = query["nonce"].clone();
                let back = reqwest::Url::parse_with_params(&query["redirect_uri"], answer(&query)).unwrap();
                *kept.lock().unwrap() = query;
                tokio::spawn(async move {
                    let _ = reqwest::get(back).await;
                });
            };
            (open, seen)
        }
    }

    const ALL: &str = "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";

    /// A vault as a sign-in leaves it: registered, allowed to use the plan, with a token good for an hour.
    pub(crate) fn signed_in(name: &str) -> Vault {
        let vault = vault(name);
        let record = Record {
            host_id: new_host_id(),
            client_id: "oaiapp_muxin1".into(),
            subject: "user-1".into(),
            email: "alex@example.com".into(),
            id_token: "kept.id.token".into(),
            access_token: "access-1".into(),
            refresh_token: "refresh-1".into(),
            expires_at: now() + 3600,
            scopes: ALL.split(' ').map(String::from).collect(),
            welcomed: false,
        };
        vault.save(&record).unwrap();
        vault
    }

    fn allowed(query: &std::collections::HashMap<String, String>) -> Vec<(&'static str, String)> {
        vec![("code", "code-1".into()), ("state", query["state"].clone()), ("client_id", "oaiapp_muxin1".into())]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_sign_in_registers_muxin_and_keeps_what_came_back() {
        let openai = FakeOpenAi::start("user-1", ALL).await;
        let vault = vault("first");
        let (open, sent) = openai.browser(allowed);
        let account = sign_in(&vault, &openai.at, open).await.unwrap();
        assert_eq!(account, Account { email: "alex@example.com".into(), plan: true, welcomed: false });

        // What the browser was sent off with: a first registration, by name, for this host.
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent["client_id"], "dynamic_agent_client");
        assert_eq!(sent["agent_name_hint"], "Muxin");
        assert!(sent["ext_agent_host_id"].starts_with("urn:uuid:") && sent["ext_agent_host_id"].len() == 45, "{}", sent["ext_agent_host_id"]);
        assert_eq!(sent["scope"], "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct");
        assert_eq!((sent["response_type"].as_str(), sent["code_challenge_method"].as_str()), ("code", "S256"));
        assert_eq!(sent["resource"], openai.at.api);
        assert!(sent["redirect_uri"].starts_with("http://127.0.0.1:") && sent["redirect_uri"].ends_with("/auth/callback"));
        assert!(!sent.contains_key("id_token_hint") && !sent.contains_key("login_hint"));

        // The code was exchanged with the client id that came back, and proved with PKCE.
        let exchange = openai.server.asked_at("/token")[0].form();
        assert_eq!((exchange["grant_type"].as_str(), exchange["client_id"].as_str(), exchange["code"].as_str()), ("authorization_code", "oaiapp_muxin1", "code-1"));
        assert_eq!(exchange["redirect_uri"], sent["redirect_uri"]);
        assert_eq!(oauth::challenge_for(&exchange["code_verifier"]), sent["code_challenge"]);
        assert!(!exchange.contains_key("client_secret"));

        // Kept, where only the user can read it; and a token is there to use.
        let record = vault.load();
        assert_eq!((record.client_id.as_str(), record.subject.as_str(), record.refresh_token.as_str()), ("oaiapp_muxin1", "user-1", "refresh-1"));
        assert_eq!(record.host_id, sent["ext_agent_host_id"]);
        #[cfg(unix)]
        if let Vault::File(path) = &vault {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(token(&vault, &openai.at, None).await.unwrap(), "access-1");
        assert_eq!(current(&vault), Some(account));
        welcomed(&vault);
        assert!(current(&vault).unwrap().welcomed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signing_in_again_reuses_the_registration_and_says_which_account() {
        let openai = FakeOpenAi::start("user-1", ALL).await;
        let vault = vault("again");
        let (open, first) = openai.browser(allowed);
        sign_in(&vault, &openai.at, open).await.unwrap();
        let host = first.lock().unwrap()["ext_agent_host_id"].clone();
        assert!(sign_out(&vault, &openai.at, false).await.unwrap());
        assert_eq!(current(&vault), None);
        // It was ended at OpenAI too, by the token that renews it.
        let revoked = openai.server.asked_at("/revoke")[0].form();
        assert_eq!((revoked["token"].as_str(), revoked["token_type_hint"].as_str(), revoked["client_id"].as_str()), ("refresh-1", "refresh_token", "oaiapp_muxin1"));
        assert!(token(&vault, &openai.at, None).await.unwrap_err().contains("Sign in with ChatGPT first"));

        // Again: Muxin's own client id, the same host, no name; the callback needn't repeat the client id.
        let (open, second) = openai.browser(|q| vec![("code", "code-2".into()), ("state", q["state"].clone())]);
        sign_in(&vault, &openai.at, open).await.unwrap();
        let second = second.lock().unwrap().clone();
        assert_eq!((second["client_id"].as_str(), second["ext_agent_host_id"].as_str()), ("oaiapp_muxin1", host.as_str()));
        assert_eq!(second["login_hint"], "alex@example.com");
        assert!(!second.contains_key("agent_name_hint"));
        // (Signing out forgot the identity token, so there was none to hint with.)
        assert!(!second.contains_key("id_token_hint"));

        // Signed in still: a third sign-in says which account with the token kept.
        let (open, third) = openai.browser(|q| vec![("code", "code-3".into()), ("state", q["state"].clone())]);
        sign_in(&vault, &openai.at, open).await.unwrap();
        assert!(third.lock().unwrap()["id_token_hint"].split('.').count() == 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_comes_back_wrong_changes_nothing() {
        let openai = FakeOpenAi::start("user-1", ALL).await;
        let vault = vault("wrong");
        let try_with = |answer: fn(&std::collections::HashMap<String, String>) -> Vec<(&'static str, String)>| {
            let (open, _) = openai.browser(answer);
            sign_in(&vault, &openai.at, open)
        };
        // Not the answer to this attempt.
        assert!(try_with(|_| vec![("code", "c".into()), ("state", "someone-elses".into()), ("client_id", "oaiapp_x".into())]).await.unwrap_err().contains("wasn't the answer"));
        // The user said no.
        assert!(try_with(|q| vec![("error", "access_denied".into()), ("state", q["state"].clone())]).await.unwrap_err().contains("didn't allow"));
        // A first sign-in that comes back without a client id of Muxin's own.
        assert!(try_with(|q| vec![("code", "c".into()), ("state", q["state"].clone())]).await.unwrap_err().contains("didn't finish registering"));
        assert!(try_with(|q| vec![("code", "c".into()), ("state", q["state"].clone()), ("client_id", "dynamic_agent_client".into())]).await.unwrap_err().contains("didn't finish registering"));
        // None of it was exchanged or kept; the host id was, from before the first attempt.
        assert!(openai.server.asked_at("/token").is_empty());
        let record = vault.load();
        assert!(record.client_id.is_empty() && !record.host_id.is_empty());
        assert_eq!(current(&vault), None);

        // Registered now. An answer for another registration, or another account, is refused.
        try_with(allowed).await.unwrap();
        assert!(try_with(|q| vec![("code", "c".into()), ("state", q["state"].clone()), ("client_id", "oaiapp_other".into())]).await.unwrap_err().contains("another registration"));
        let other = FakeOpenAi::start("user-2", ALL).await;
        let (open, _) = other.browser(|q| vec![("code", "c".into()), ("state", q["state"].clone())]);
        assert!(sign_in(&vault, &other.at, open).await.unwrap_err().contains("another ChatGPT account"));
        assert_eq!(vault.load().subject, "user-1");
        // Forgetting the registration is how to switch.
        sign_out(&vault, &openai.at, true).await.unwrap();
        let record = vault.load();
        assert!(record.client_id.is_empty() && record.subject.is_empty() && !record.host_id.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signing_in_without_allowing_plan_use_is_said_and_gives_no_token() {
        let openai = FakeOpenAi::start("user-1", "email openid profile").await;
        let vault = vault("noplan");
        let (open, _) = openai.browser(allowed);
        let account = sign_in(&vault, &openai.at, open).await.unwrap();
        assert!(!account.plan);
        assert!(token(&vault, &openai.at, None).await.unwrap_err().contains("didn't allow Muxin to use your plan"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_about_to_run_out_is_renewed_and_its_replacement_kept() {
        let openai = FakeOpenAi::start("user-1", ALL).await;
        let vault = vault("renew");
        let (open, _) = openai.browser(allowed);
        sign_in(&vault, &openai.at, open).await.unwrap();
        // Four minutes left: renewed before use.
        let mut record = vault.load();
        record.expires_at = now() + 240;
        vault.save(&record).unwrap();
        assert_eq!(token(&vault, &openai.at, None).await.unwrap(), "access-2");
        let renewal = openai.server.asked_at("/token")[1].form();
        assert_eq!((renewal["grant_type"].as_str(), renewal["client_id"].as_str(), renewal["refresh_token"].as_str()), ("refresh_token", "oaiapp_muxin1", "refresh-1"));
        assert_eq!(renewal["resource"], openai.at.api);
        assert!(!renewal.contains_key("scope"));
        assert_eq!(vault.load().refresh_token, "refresh-2");
        // Fresh now: used as it is, without asking.
        assert_eq!(token(&vault, &openai.at, None).await.unwrap(), "access-2");
        assert_eq!(openai.server.asked_at("/token").len(), 2);

        // OpenAI can't be reached properly: nothing is thrown away.
        *openai.renewal.lock().unwrap() = (503, json!({}));
        assert!(token(&vault, &openai.at, Some("access-2")).await.unwrap_err().contains("answered 503"));
        assert_eq!(vault.load().refresh_token, "refresh-2");
        // The sign-in is over: its tokens go, the registration stays.
        *openai.renewal.lock().unwrap() = (400, json!({"error": "refresh_token_reused"}));
        assert!(token(&vault, &openai.at, Some("access-2")).await.unwrap_err().contains("has ended"));
        let record = vault.load();
        assert!(record.refresh_token.is_empty() && record.access_token.is_empty());
        assert_eq!(record.client_id, "oaiapp_muxin1");
        assert_eq!(current(&vault), None);
    }

    #[test]
    fn a_host_id_is_a_random_uuid() {
        let (a, b) = (new_host_id(), new_host_id());
        assert_ne!(a, b);
        let uuid = a.strip_prefix("urn:uuid:").unwrap();
        let parts: Vec<&str> = uuid.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4') && matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')), "{uuid}");
    }
}
