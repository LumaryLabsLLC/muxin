//! GitHub REST API: runner status for orgs/repos, plus queued and running jobs
//! from recently active repos. Requests are conditional (ETag), so polling an
//! unchanged endpoint returns 304 and doesn't count against the rate limit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use futures::stream::{self, StreamExt};
use reqwest::header::{ACCEPT, AUTHORIZATION, ETAG, IF_NONE_MATCH, LINK, USER_AGENT};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::GitHubSettings;

pub mod hub;

const CONCURRENCY: usize = 6;
const ORG_REPOS_TTL: Duration = Duration::from_secs(300);
const RELEASE_TTL: Duration = Duration::from_secs(6 * 3600);

// ---------------------------------------------------------------------------
// Results handed to the fleet engine

#[derive(Debug, Clone, Default)]
pub struct GitHubData {
    pub synced_at: Option<DateTime<Utc>>,
    pub token_source: String,
    pub login: Option<String>,
    /// A problem that stopped the whole sync (e.g. no token).
    pub error: Option<String>,
    pub scopes: Vec<ScopeData>,
    pub jobs: Vec<JobData>,
    pub scanned_repos: Vec<String>,
    pub rate: Option<Rate>,
    pub latest_runner_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScopeData {
    pub kind: ScopeKind,
    pub name: String,
    pub runners: Vec<RunnerData>,
    pub synced_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Org,
    Repo,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Org => "org",
            ScopeKind::Repo => "repo",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunnerData {
    pub id: u64,
    pub name: String,
    pub os: String,
    pub status: String,
    pub busy: bool,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct JobData {
    pub id: u64,
    pub run_id: u64,
    pub name: String,
    pub workflow: String,
    pub repo: String,
    pub branch: Option<String>,
    pub event: Option<String>,
    pub actor: Option<String>,
    pub url: String,
    pub status: String,
    pub labels: Vec<String>,
    pub runner_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Rate {
    pub limit: u32,
    pub remaining: u32,
    pub reset_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ScopeTest {
    pub kind: ScopeKind,
    pub name: String,
    pub runners: usize,
    pub error: Option<String>,
    pub hint: Option<String>,
}

// ---------------------------------------------------------------------------
// API shapes

#[derive(Deserialize)]
struct ApiRunners {
    runners: Vec<ApiRunner>,
}

#[derive(Deserialize)]
struct ApiRunner {
    id: u64,
    name: String,
    #[serde(default)]
    os: String,
    status: String,
    #[serde(default)]
    busy: bool,
    #[serde(default)]
    labels: Vec<ApiLabel>,
}

#[derive(Deserialize)]
struct ApiLabel {
    name: String,
}

#[derive(Deserialize)]
struct ApiRuns {
    workflow_runs: Vec<ApiRun>,
}

#[derive(Deserialize)]
struct ApiRun {
    id: u64,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    actor: Option<ApiUser>,
}

#[derive(Deserialize)]
struct ApiUser {
    login: String,
}

#[derive(Deserialize)]
struct ApiJobs {
    jobs: Vec<ApiJob>,
}

#[derive(Deserialize)]
struct ApiJob {
    id: u64,
    run_id: u64,
    name: String,
    status: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    runner_name: Option<String>,
    created_at: DateTime<Utc>,
    #[serde(default)]
    started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    workflow_name: Option<String>,
    #[serde(default)]
    head_branch: Option<String>,
}

#[derive(Deserialize)]
struct ApiRepo {
    full_name: String,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
}

/// An actions/runner release, with the SHA-256 GitHub published for each package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerRelease {
    /// e.g. "2.329.0"
    pub version: String,
    /// (platform, lowercase hex), e.g. ("linux-x64", "194f…"). The platform is the
    /// package name's middle: actions-runner-<platform>-<version>.tar.gz or .zip.
    pub sha256: Vec<(String, String)>,
}

/// The checksums in a release's notes, one line per package:
/// `<!-- BEGIN SHA linux-x64 -->HEX<!-- END SHA linux-x64 -->`.
fn runner_checksums(body: &str) -> Vec<(String, String)> {
    const BEGIN: &str = "<!-- BEGIN SHA ";
    let mut out: Vec<(String, String)> = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find(BEGIN) {
        rest = &rest[at + BEGIN.len()..];
        let Some((platform, after)) = rest.split_once(" -->") else { break };
        let platform = platform.trim();
        let end = format!("<!-- END SHA {platform} -->");
        let Some((hex, _)) = after.split_once(&end) else { continue };
        let hex = hex.trim().to_ascii_lowercase();
        let platform_ok = !platform.is_empty() && platform.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        let hex_ok = hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit());
        if platform_ok && hex_ok && !out.iter().any(|(p, _)| p == platform) {
            out.push((platform.to_string(), hex));
        }
    }
    out
}

#[derive(Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    message: String,
}

// ---------------------------------------------------------------------------
// Client

#[derive(Debug)]
pub struct ApiError {
    pub status: Option<StatusCode>,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Clone)]
struct Token {
    value: String,
    source: String,
    /// Logins change (a new account, more permissions); look again after a while.
    fetched: Instant,
}

/// How long a token from the GitHub CLI is reused before asking it again.
const TOKEN_TTL: Duration = Duration::from_secs(60);

struct Cached {
    etag: String,
    body: std::sync::Arc<[u8]>,
    next: Option<String>,
}

/// An account the GitHub CLI is signed in to.
#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub login: String,
    /// The CLI's own active account (what `gh` uses in a terminal).
    pub gh_active: bool,
    /// False when its login has expired or been revoked.
    pub ok: bool,
    pub scopes: String,
}

/// Gets the token of another machine's GitHub CLI login: (machine id, host).
pub type TokenFetcher = Arc<dyn Fn(String, String) -> futures::future::BoxFuture<'static, Option<String>> + Send + Sync>;

pub struct Client {
    http: reqwest::Client,
    api_url: Mutex<String>,
    account: Mutex<String>,
    account_machine: Mutex<String>,
    fetcher: Mutex<Option<TokenFetcher>>,
    token: Mutex<Option<Token>>,
    cache: Mutex<HashMap<String, Cached>>,
    rate: Mutex<Option<Rate>>,
    /// The token's OAuth scopes, from GitHub's last answer (None: unknown, e.g. a fine-grained token).
    scopes: Mutex<Option<Vec<String>>>,
    login: Mutex<Option<String>>,
    org_repos: Mutex<HashMap<String, (Instant, Vec<String>)>>,
    release: Mutex<Option<(Instant, RunnerRelease)>>,
}

impl Client {
    pub fn new() -> Client {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(8))
            .build()
            .expect("building HTTP client");
        Client {
            http,
            api_url: Mutex::new("https://api.github.com".into()),
            account: Mutex::new(String::new()),
            account_machine: Mutex::new(String::new()),
            fetcher: Mutex::new(None),
            token: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
            rate: Mutex::new(None),
            scopes: Mutex::new(None),
            login: Mutex::new(None),
            org_repos: Mutex::new(HashMap::new()),
            release: Mutex::new(None),
        }
    }

    /// Points the client at the configured server and account. A different
    /// server or account means different credentials and cached responses.
    fn configure(&self, settings: &GitHubSettings) {
        let mut url = self.api_url.lock().unwrap();
        let mut account = self.account.lock().unwrap();
        let mut machine = self.account_machine.lock().unwrap();
        if *url != settings.api_url || *account != settings.account || *machine != settings.account_machine {
            *url = settings.api_url.clone();
            *account = settings.account.clone();
            *machine = settings.account_machine.clone();
            *self.token.lock().unwrap() = None;
            *self.login.lock().unwrap() = None;
            self.cache.lock().unwrap().clear();
            self.org_repos.lock().unwrap().clear();
        }
    }

    /// GitHub host for `gh auth token --hostname`.
    fn gh_hostname(&self) -> String {
        let api = self.api_url.lock().unwrap().clone();
        let host = api.split("://").nth(1).unwrap_or(&api).split('/').next().unwrap_or("github.com");
        if host == "api.github.com" { "github.com".into() } else { host.to_string() }
    }

    async fn token(&self) -> Option<Token> {
        if let Some(t) = self.token.lock().unwrap().clone().filter(|t| t.fetched.elapsed() < TOKEN_TTL) {
            return Some(t);
        }
        let account = self.account.lock().unwrap().clone();
        let machine = self.account_machine.lock().unwrap().clone();
        let api = self.api_url.lock().unwrap().clone();
        let found = match env_token(&api, |var| std::env::var(var).ok()) {
            Some(t) => Some(t),
            None if machine.is_empty() => gh_token(&self.gh_hostname(), &account).await,
            None => {
                let fetch = self.fetcher.lock().unwrap().clone();
                match fetch {
                    Some(f) => f(machine, self.gh_hostname()).await.map(|value| Token { value, source: "gh".into(), fetched: Instant::now() }),
                    None => None,
                }
            }
        };
        *self.token.lock().unwrap() = found.clone();
        found
    }

    /// How to get a token when another machine's login is chosen.
    pub fn set_token_fetcher(&self, f: TokenFetcher) {
        *self.fetcher.lock().unwrap() = Some(f);
    }

    /// The token's OAuth scopes as GitHub last reported them, if it's an OAuth token.
    pub fn scopes(&self) -> Option<Vec<String>> {
        self.scopes.lock().unwrap().clone()
    }

    /// Asks for the login again on the next request (after `gh auth refresh`).
    pub fn reload_login(&self) {
        *self.token.lock().unwrap() = None;
        *self.scopes.lock().unwrap() = None;
    }

    /// Organizations you're a member of.
    pub async fn my_orgs(&self, settings: &GitHubSettings) -> Result<Vec<String>, String> {
        #[derive(Deserialize)]
        struct Org {
            login: String,
        }
        self.configure(settings);
        let pages: Vec<Vec<Org>> = self.get_all("/user/orgs?per_page=100", 2).await.map_err(|e| e.message)?;
        Ok(pages.into_iter().flatten().map(|o| o.login).collect())
    }

    fn forget_token(&self) {
        *self.token.lock().unwrap() = None;
    }

    pub fn rate(&self) -> Option<Rate> {
        *self.rate.lock().unwrap()
    }

    /// GET a JSON endpoint, following `Link: rel="next"` up to `max_pages`.
    async fn get_all<T: DeserializeOwned>(&self, path: &str, max_pages: usize) -> Result<Vec<T>, ApiError> {
        let mut out = Vec::new();
        let mut url = self.url(path);
        for _ in 0..max_pages {
            let (page, next) = self.get_page::<T>(&url, true).await?;
            out.push(page);
            match next {
                Some(n) => url = n,
                None => break,
            }
        }
        Ok(out)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        Ok(self.get_page(&self.url(path), true).await?.0)
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") { path.to_string() } else { format!("{}{}", self.api_url.lock().unwrap(), path) }
    }

    fn get_page<'a, T: DeserializeOwned>(
        &'a self,
        url: &'a str,
        retry_auth: bool,
    ) -> futures::future::BoxFuture<'a, Result<(T, Option<String>), ApiError>> {
        Box::pin(async move {
            let Some(token) = self.token().await else {
                return Err(ApiError { status: None, message: "No GitHub token".into() });
            };
            let cached_etag = self.cache.lock().unwrap().get(url).map(|c| c.etag.clone());
            let mut req = self
                .http
                .get(url)
                .header(AUTHORIZATION, format!("Bearer {}", token.value))
                .header(ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header(USER_AGENT, concat!("muxen/", env!("CARGO_PKG_VERSION")));
            if let Some(etag) = &cached_etag {
                req = req.header(IF_NONE_MATCH, etag);
            }
            let resp = req.send().await.map_err(|e| ApiError {
                status: None,
                message: format!("Couldn't reach GitHub: {}", root_cause(&e)),
            })?;
            self.record_rate(resp.headers());
            let status = resp.status();

            if status == StatusCode::NOT_MODIFIED {
                let cache = self.cache.lock().unwrap();
                if let Some(c) = cache.get(url) {
                    let value = serde_json::from_slice(&c.body).map_err(|e| decode_error(url, e))?;
                    return Ok((value, c.next.clone()));
                }
            }
            let rate_limited = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == "0");
            if retry_auth && refused(status, rate_limited) {
                // The login may have changed since we read it (`gh auth refresh`
                // granting a permission, another account): look again, and retry
                // once if it did.
                self.forget_token();
                if self.token().await.is_some_and(|fresh| status == StatusCode::UNAUTHORIZED || fresh.value != token.value) {
                    return self.get_page(url, false).await;
                }
            }
            if !status.is_success() {
                let body = resp.bytes().await.unwrap_or_default();
                let message = serde_json::from_slice::<ApiErrorBody>(&body).map(|b| b.message).unwrap_or_default();
                let message = if rate_limited {
                    "GitHub API rate limit reached".to_string()
                } else if message.is_empty() {
                    format!("GitHub returned {status}")
                } else {
                    message
                };
                return Err(ApiError { status: Some(status), message });
            }

            let etag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
            let next = resp.headers().get(LINK).and_then(|v| v.to_str().ok()).and_then(next_link);
            let body: std::sync::Arc<[u8]> = resp
                .bytes()
                .await
                .map_err(|e| ApiError { status: None, message: format!("Reading GitHub response: {e}") })?
                .to_vec()
                .into();
            let value = serde_json::from_slice(&body).map_err(|e| decode_error(url, e))?;
            if let Some(etag) = etag {
                self.cache.lock().unwrap().insert(url.to_string(), Cached { etag, body, next: next.clone() });
            }
            Ok((value, next))
        })
    }

    fn record_rate(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(s) = headers.get("x-oauth-scopes").and_then(|v| v.to_str().ok()) {
            *self.scopes.lock().unwrap() = Some(s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect());
        }
        let num = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        if let (Some(limit), Some(remaining), Some(reset)) =
            (num("x-ratelimit-limit"), num("x-ratelimit-remaining"), num("x-ratelimit-reset"))
        {
            // Only the core REST bucket matters here.
            let resource = headers.get("x-ratelimit-resource").and_then(|v| v.to_str().ok()).unwrap_or("core");
            if resource == "core" {
                *self.rate.lock().unwrap() = Some(Rate {
                    limit: limit as u32,
                    remaining: remaining as u32,
                    reset_at: Utc.timestamp_opt(reset, 0).single().unwrap_or_else(Utc::now),
                });
            }
        }
    }

    async fn login(&self) -> Option<String> {
        if let Some(l) = self.login.lock().unwrap().clone() {
            return Some(l);
        }
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        let user: User = self.get("/user").await.ok()?;
        *self.login.lock().unwrap() = Some(user.login.clone());
        Some(user.login)
    }

    async fn runners(&self, kind: ScopeKind, name: &str) -> Result<Vec<RunnerData>, ApiError> {
        let path = match kind {
            ScopeKind::Org => format!("/orgs/{name}/actions/runners?per_page=100"),
            ScopeKind::Repo => format!("/repos/{name}/actions/runners?per_page=100"),
        };
        let pages: Vec<ApiRunners> = self.get_all(&path, 10).await?;
        Ok(pages
            .into_iter()
            .flat_map(|p| p.runners)
            .map(|r| RunnerData {
                id: r.id,
                name: r.name,
                os: r.os,
                status: r.status,
                busy: r.busy,
                labels: r.labels.into_iter().map(|l| l.name).collect(),
            })
            .collect())
    }

    /// The most recently pushed, non-archived repos of an org (cached for a few minutes).
    async fn org_repos(&self, org: &str, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let key = format!("{org}:{limit}");
        if let Some((at, repos)) = self.org_repos.lock().unwrap().get(&key) {
            if at.elapsed() < ORG_REPOS_TTL {
                return repos.clone();
            }
        }
        let per_page = (limit + 5).min(100);
        let path = format!("/orgs/{org}/repos?sort=pushed&direction=desc&per_page={per_page}");
        let repos: Vec<String> = match self.get::<Vec<ApiRepo>>(&path).await {
            Ok(list) => list.into_iter().filter(|r| !r.archived && !r.disabled).map(|r| r.full_name).take(limit).collect(),
            Err(e) => {
                tracing::debug!("listing repos for {org}: {e}");
                Vec::new()
            }
        };
        self.org_repos.lock().unwrap().insert(key, (Instant::now(), repos.clone()));
        repos
    }

    /// Queued and in-progress jobs for one repo.
    async fn active_jobs(&self, repo: &str) -> Vec<JobData> {
        let mut runs = Vec::new();
        for status in ["queued", "in_progress"] {
            match self.get::<ApiRuns>(&format!("/repos/{repo}/actions/runs?status={status}&per_page=30")).await {
                Ok(r) => runs.extend(r.workflow_runs),
                Err(e) => tracing::debug!("listing {status} runs for {repo}: {e}"),
            }
        }
        runs.sort_by_key(|r| r.id);
        runs.dedup_by_key(|r| r.id);

        let per_run = stream::iter(runs)
            .map(|run| async move {
                let jobs = self
                    .get::<ApiJobs>(&format!("/repos/{repo}/actions/runs/{}/jobs?per_page=100", run.id))
                    .await
                    .map(|j| j.jobs)
                    .unwrap_or_default();
                jobs.into_iter()
                    .filter(|j| matches!(j.status.as_str(), "queued" | "in_progress" | "waiting" | "pending"))
                    .map(|j| JobData {
                        id: j.id,
                        run_id: j.run_id,
                        name: j.name,
                        workflow: j.workflow_name.unwrap_or_default(),
                        repo: repo.to_string(),
                        branch: j.head_branch,
                        event: run.event.clone(),
                        actor: run.actor.as_ref().map(|a| a.login.clone()),
                        url: j.html_url.unwrap_or_else(|| format!("https://github.com/{repo}/actions/runs/{}", j.run_id)),
                        status: if j.status == "pending" { "queued".into() } else { j.status },
                        labels: j.labels,
                        runner_name: j.runner_name.filter(|n| !n.is_empty()),
                        created_at: j.created_at,
                        started_at: j.started_at,
                    })
                    .collect::<Vec<_>>()
            })
            .buffer_unordered(CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        per_run.into_iter().flatten().collect()
    }

    async fn latest_runner_version(&self) -> Option<String> {
        self.latest_release().await.map(|r| r.version)
    }

    async fn latest_release(&self) -> Option<RunnerRelease> {
        if let Some((at, r)) = self.release.lock().unwrap().clone() {
            if at.elapsed() < RELEASE_TTL {
                return Some(r);
            }
        }
        let rel: ApiRelease = self.get("/repos/actions/runner/releases/latest").await.ok()?;
        let r = RunnerRelease {
            version: rel.tag_name.trim_start_matches('v').to_string(),
            sha256: runner_checksums(rel.body.as_deref().unwrap_or_default()),
        };
        *self.release.lock().unwrap() = Some((Instant::now(), r.clone()));
        Some(r)
    }

    /// One full pass: runners for every configured scope, then active jobs.
    pub async fn sync(&self, settings: &GitHubSettings) -> GitHubData {
        self.configure(settings);
        let mut data = GitHubData::default();
        let Some(token) = self.token().await else {
            data.error = Some("No GitHub token found. Run `gh auth login` or set GITHUB_TOKEN.".into());
            return data;
        };
        data.token_source = token.source;
        data.login = self.login().await;

        let scopes: Vec<(ScopeKind, String)> = settings
            .orgs
            .iter()
            .map(|o| (ScopeKind::Org, o.clone()))
            .chain(settings.repos.iter().map(|r| (ScopeKind::Repo, r.clone())))
            .collect();

        data.scopes = stream::iter(scopes)
            .map(|(kind, name)| async move {
                let result = self.runners(kind, &name).await;
                let (runners, error, hint) = match result {
                    Ok(r) => (r, None, None),
                    Err(e) => {
                        let (msg, hint) = explain(kind, &name, &e, &data_token_source(self));
                        (Vec::new(), Some(msg), hint)
                    }
                };
                ScopeData {
                    kind,
                    synced_at: error.is_none().then(Utc::now),
                    name,
                    runners,
                    error,
                    hint,
                }
            })
            .buffered(CONCURRENCY)
            .collect()
            .await;

        // Repos to scan for jobs: the configured ones plus each org's most recently pushed repos.
        let mut repos: Vec<String> = settings.repos.clone();
        for org in &settings.orgs {
            for r in self.org_repos(org, settings.scan_repos as usize).await {
                if !repos.iter().any(|x| x.eq_ignore_ascii_case(&r)) {
                    repos.push(r);
                }
            }
        }
        data.jobs = stream::iter(repos.clone())
            .map(|repo| async move { self.active_jobs(&repo).await })
            .buffer_unordered(CONCURRENCY)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();
        data.scanned_repos = repos;
        data.latest_runner_version = self.latest_runner_version().await;
        data.rate = self.rate();
        data.synced_at = Some(Utc::now());
        if !data.scopes.is_empty() && data.scopes.iter().all(|s| s.error.is_some()) {
            data.error = data.scopes.first().and_then(|s| s.error.clone());
        }
        data
    }

    /// Validates an org or repo before it's added in Settings.
    pub async fn test_scope(&self, settings: &GitHubSettings, scope: &str) -> ScopeTest {
        self.configure(settings);
        let name = crate::config::strip_github_url(scope);
        let kind = if name.contains('/') { ScopeKind::Repo } else { ScopeKind::Org };
        if self.token().await.is_none() {
            return ScopeTest {
                kind,
                name,
                runners: 0,
                error: Some("No GitHub token found.".into()),
                hint: Some("gh auth login".into()),
            };
        }
        match self.runners(kind, &name).await {
            Ok(r) => ScopeTest { kind, name, runners: r.len(), error: None, hint: None },
            Err(e) => {
                let (msg, hint) = explain(kind, &name, &e, &data_token_source(self));
                ScopeTest { kind, name, runners: 0, error: Some(msg), hint }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Writes, used by runner provisioning. Never cached.

    /// A short-lived token for `config.sh` (registering new runners). The scripts
    /// hand it over in ACTIONS_RUNNER_INPUT_TOKEN, never on a command line.
    pub async fn registration_token(&self, settings: &GitHubSettings, scope: &str) -> Result<String, String> {
        let (kind, name) = parse_scope(scope);
        let doing = format!("Registering runners with {name}");
        self.scoped_token(settings, kind, &name, "registration-token", &doing).await
    }

    /// A short-lived token for `config.sh remove` (deregistering a runner), handed
    /// over the same way.
    pub async fn remove_token(&self, settings: &GitHubSettings, scope: &str) -> Result<String, String> {
        let (kind, name) = parse_scope(scope);
        let doing = format!("Removing runners from {name}");
        self.scoped_token(settings, kind, &name, "remove-token", &doing).await
    }

    /// Deletes a runner registration directly, for runners whose machine is gone.
    pub async fn delete_runner(&self, settings: &GitHubSettings, scope: &str, runner_id: u64) -> Result<(), String> {
        self.configure(settings);
        let (kind, name) = parse_scope(scope);
        let path = format!("{}/actions/runners/{runner_id}", scope_path(kind, &name));
        match self.send(reqwest::Method::DELETE, &path).await {
            Ok(_) => Ok(()),
            Err(e) => Err(with_hint(explain_doing(kind, &name, &e, &data_token_source(self), &format!("Removing runners from {name}")))),
        }
    }

    /// Cancels a workflow run: every job in it that hasn't finished.
    pub async fn cancel_run(&self, settings: &GitHubSettings, repo: &str, run_id: u64) -> Result<(), String> {
        self.configure(settings);
        let path = format!("/repos/{repo}/actions/runs/{run_id}/cancel");
        match self.send(reqwest::Method::POST, &path).await {
            Ok(_) => Ok(()),
            Err(e) => Err(with_hint(explain_doing(ScopeKind::Repo, repo, &e, &data_token_source(self), &format!("Cancelling a run in {repo}")))),
        }
    }

    /// The newest actions/runner release (e.g. 2.329.0) and its packages' SHA-256.
    pub async fn latest_runner_release(&self, settings: &GitHubSettings) -> Option<RunnerRelease> {
        self.configure(settings);
        self.latest_release().await
    }

    /// The URL runners are registered against, e.g. https://github.com/acme.
    pub fn web_url(&self, settings: &GitHubSettings, scope: &str) -> String {
        self.configure(settings);
        let (_, name) = parse_scope(scope);
        format!("https://{}/{name}", self.gh_hostname())
    }

    async fn scoped_token(&self, settings: &GitHubSettings, kind: ScopeKind, name: &str, which: &str, doing: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct TokenBody {
            token: String,
        }
        self.configure(settings);
        if self.token().await.is_none() {
            return Err("No GitHub token found. Run `gh auth login` or set GITHUB_TOKEN.".into());
        }
        let path = format!("{}/actions/runners/{which}", scope_path(kind, name));
        match self.send(reqwest::Method::POST, &path).await {
            Ok(body) => serde_json::from_slice::<TokenBody>(&body)
                .map(|b| b.token)
                .map_err(|e| format!("Unexpected response from GitHub: {e}")),
            Err(e) => Err(with_hint(explain_doing(kind, name, &e, &data_token_source(self), doing))),
        }
    }

    /// An uncached request; retries once with a fresh token on 401.
    async fn send(&self, method: reqwest::Method, path: &str) -> Result<Vec<u8>, ApiError> {
        let url = self.url(path);
        for attempt in 0..2 {
            let Some(token) = self.token().await else {
                return Err(ApiError { status: None, message: "No GitHub token".into() });
            };
            let resp = self
                .http
                .request(method.clone(), &url)
                .header(AUTHORIZATION, format!("Bearer {}", token.value))
                .header(ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header(USER_AGENT, concat!("muxen/", env!("CARGO_PKG_VERSION")))
                .send()
                .await
                .map_err(|e| ApiError { status: None, message: format!("Couldn't reach GitHub: {}", root_cause(&e)) })?;
            self.record_rate(resp.headers());
            let status = resp.status();
            let rate_limited = resp.headers().get("x-ratelimit-remaining").and_then(|v| v.to_str().ok()).is_some_and(|v| v == "0");
            if attempt == 0 && refused(status, rate_limited) {
                // As in get_page: a changed login gets one more try.
                self.forget_token();
                if self.token().await.is_some_and(|fresh| status == StatusCode::UNAUTHORIZED || fresh.value != token.value) {
                    continue;
                }
            }
            let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            if status.is_success() {
                return Ok(body);
            }
            let message = serde_json::from_slice::<ApiErrorBody>(&body).map(|b| b.message).unwrap_or_default();
            let message = if message.is_empty() { format!("GitHub returned {status}") } else { message };
            return Err(ApiError { status: Some(status), message });
        }
        Err(ApiError { status: Some(StatusCode::UNAUTHORIZED), message: "GitHub rejected the token.".into() })
    }

    /// The accounts the GitHub CLI is signed in to on this server.
    pub async fn accounts(&self, settings: &GitHubSettings) -> Result<Vec<Account>, String> {
        #[derive(Deserialize)]
        struct Status {
            hosts: HashMap<String, Vec<RawAccount>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawAccount {
            login: String,
            #[serde(default)]
            active: bool,
            #[serde(default)]
            state: String,
            #[serde(default)]
            scopes: Option<String>,
        }
        self.configure(settings);
        let host = self.gh_hostname();
        let out = tokio::process::Command::new(gh_program())
            .args(["auth", "status", "--json", "hosts", "--hostname", &host])
            .output()
            .await
            .map_err(|e| format!("Couldn't run the GitHub CLI (gh): {e}"))?;
        // It exits 1 when an account has a problem, but still lists them all.
        let status: Status = serde_json::from_slice(&out.stdout)
            .map_err(|_| "The GitHub CLI (gh) couldn't list its accounts. It needs gh 2.40 or newer.".to_string())?;
        Ok(status
            .hosts
            .into_values()
            .flatten()
            .map(|a| Account { ok: a.state.is_empty() || a.state == "success", login: a.login, gh_active: a.active, scopes: a.scopes.unwrap_or_default() })
            .collect())
    }

    pub async fn token_source(&self) -> String {
        self.token().await.map(|t| t.source).unwrap_or_default()
    }

    /// The GitHub web host (e.g. "github.com") and the user's token, for cloning
    /// private repositories into agent workspaces. The caller must keep the token
    /// out of argv and logs.
    pub async fn git_credentials(&self, settings: &GitHubSettings) -> Option<(String, String)> {
        self.configure(settings);
        let token = self.token().await?;
        Some((self.gh_hostname(), token.value))
    }
}

/// GitHub turning the login away (not a rate limit): bad token, missing permission, or hidden.
fn refused(status: StatusCode, rate_limited: bool) -> bool {
    status == StatusCode::UNAUTHORIZED || (!rate_limited && (status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND))
}

fn data_token_source(client: &Client) -> String {
    client.token.lock().unwrap().as_ref().map(|t| t.source.clone()).unwrap_or_default()
}

/// Turns API failures into a sentence and, where possible, the command that fixes it.
fn explain(kind: ScopeKind, name: &str, e: &ApiError, token_source: &str) -> (String, Option<String>) {
    explain_doing(kind, name, e, token_source, &format!("Reading {name}'s runners"))
}

/// Like `explain`, for a specific action ("Registering runners with acme").
fn explain_doing(kind: ScopeKind, name: &str, e: &ApiError, token_source: &str, doing: &str) -> (String, Option<String>) {
    let via_gh = token_source == "gh";
    match (kind, e.status.map(|s| s.as_u16())) {
        (_, Some(401)) => (
            "GitHub rejected the token.".into(),
            Some(if via_gh { "gh auth login".into() } else { "Check GITHUB_TOKEN".into() }),
        ),
        (ScopeKind::Org, Some(403)) if !e.message.contains("rate limit") => (
            format!("{doing} needs an org admin token with the admin:org scope."),
            via_gh.then(|| "gh auth refresh -h github.com -s admin:org".into()),
        ),
        (ScopeKind::Org, Some(404)) => (format!("Organization {name} wasn't found, or you aren't an admin of it."), None),
        (ScopeKind::Repo, Some(403 | 404)) if !e.message.contains("rate limit") => {
            (format!("{name} wasn't found, or you don't have admin access to it."), None)
        }
        _ => (e.message.clone(), None),
    }
}

fn with_hint((msg, hint): (String, Option<String>)) -> String {
    match hint {
        Some(h) => format!("{msg} Run `{h}`."),
        None => msg,
    }
}

/// "acme", "org:acme" -> (Org, "acme"); "acme/api", "repo:acme/api" -> (Repo, "acme/api")
pub fn parse_scope(scope: &str) -> (ScopeKind, String) {
    let s = scope.trim();
    let s = s.strip_prefix("org:").or_else(|| s.strip_prefix("repo:")).unwrap_or(s);
    let name = crate::config::strip_github_url(s);
    let kind = if name.contains('/') { ScopeKind::Repo } else { ScopeKind::Org };
    (kind, name)
}

fn scope_path(kind: ScopeKind, name: &str) -> String {
    match kind {
        ScopeKind::Org => format!("/orgs/{name}"),
        ScopeKind::Repo => format!("/repos/{name}"),
    }
}

/// A token from the environment (`var` reads it), which wins over any login.
/// By gh's rule: `MUXEN_GITHUB_TOKEN`, `GITHUB_TOKEN` and `GH_TOKEN` are
/// github.com's, so they go to api.github.com only; any other server gets
/// `GH_ENTERPRISE_TOKEN` or `GITHUB_ENTERPRISE_TOKEN`. None over plain http.
fn env_token(api_url: &str, var: impl Fn(&str) -> Option<String>) -> Option<Token> {
    let url = reqwest::Url::parse(api_url).ok().filter(|u| u.scheme() == "https")?;
    let names: &[&str] = if url.host_str() == Some("api.github.com") {
        &["MUXEN_GITHUB_TOKEN", "OPENRUNNER_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"]
    } else {
        &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
    };
    names.iter().find_map(|name| {
        let v = var(name)?;
        (!v.trim().is_empty()).then(|| Token { value: v.trim().to_string(), source: name.to_string(), fetched: Instant::now() })
    })
}

/// `gh auth token` here, for `account`, else the GitHub CLI's active account.
async fn gh_token(hostname: &str, account: &str) -> Option<Token> {
    let mut cmd = tokio::process::Command::new(gh_program());
    cmd.args(["auth", "token", "--hostname", hostname]).kill_on_drop(true);
    if !account.is_empty() {
        cmd.args(["--user", account]);
    }
    // A locked keyring can make gh wait indefinitely.
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await.ok()?.ok()?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !value.is_empty()).then(|| Token { value, source: "gh".into(), fetched: Instant::now() })
}

/// `gh` from PATH, or from where installers usually put it. Apps started from
/// Finder, the Dock or a desktop launcher get a minimal PATH without Homebrew.
fn gh_program() -> std::path::PathBuf {
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join(if cfg!(windows) { "gh.exe" } else { "gh" }).is_file()))
        .unwrap_or(false);
    if on_path {
        return "gh".into();
    }
    let home = dirs::home_dir().unwrap_or_default();
    let candidates = [
        std::path::PathBuf::from("/opt/homebrew/bin/gh"),
        "/usr/local/bin/gh".into(),
        "/home/linuxbrew/.linuxbrew/bin/gh".into(),
        "/usr/bin/gh".into(),
        "/snap/bin/gh".into(),
        home.join(".local/bin/gh"),
        home.join("bin/gh"),
        r"C:\Program Files\GitHub CLI\gh.exe".into(),
    ];
    candidates.into_iter().find(|p| p.is_file()).unwrap_or_else(|| "gh".into())
}

fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| url.trim().trim_start_matches('<').trim_end_matches('>').to_string())
    })
}

fn decode_error(url: &str, e: serde_json::Error) -> ApiError {
    ApiError { status: None, message: format!("Unexpected response from {url}: {e}") }
}

fn root_cause(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

/// Labels GitHub uses for its own hosted runners. A job asking only for these
/// isn't waiting for one of our machines.
pub fn is_github_hosted_label(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    ["ubuntu-", "windows-", "macos-"].iter().any(|p| l.starts_with(p))
}

/// "https://github.com/acme" -> "acme", "https://github.com/acme/api" -> "acme/api"
pub fn scope_from_runner_url(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://").or_else(|| url.trim().strip_prefix("http://"))?;
    let (_, path) = rest.split_once('/')?;
    let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        ["enterprises", ..] => None,
        [org] => Some(org.to_string()),
        [owner, repo] => Some(format!("{owner}/{repo}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_tokens_go_only_to_their_own_server() {
        let env = |name: &str| ["GH_TOKEN", "GH_ENTERPRISE_TOKEN"].contains(&name).then(|| format!("{name}-value"));
        let source = |api: &str| env_token(api, env).map(|t| t.source);
        assert_eq!(source("https://api.github.com").as_deref(), Some("GH_TOKEN"));
        assert_eq!(source("https://ghe.example.com/api/v3").as_deref(), Some("GH_ENTERPRISE_TOKEN"));
        // github.com's token goes nowhere else, whatever the URL looks like.
        let github_only = |name: &str| (name == "GITHUB_TOKEN").then(|| "secret".to_string());
        for api in ["https://ghe.example.com/api/v3", "https://api.github.com.evil.example", "https://api.github.com@evil.example", "http://api.github.com"] {
            assert!(env_token(api, github_only).is_none(), "{api}");
        }
    }

    #[test]
    fn parses_link_header() {
        let h = r#"<https://api.github.com/orgs/acme/actions/runners?page=2>; rel="next", <https://api.github.com/orgs/acme/actions/runners?page=5>; rel="last""#;
        assert_eq!(next_link(h).as_deref(), Some("https://api.github.com/orgs/acme/actions/runners?page=2"));
        assert_eq!(next_link(r#"<x>; rel="prev""#), None);
    }

    #[test]
    fn scopes_from_urls() {
        assert_eq!(scope_from_runner_url("https://github.com/acme").as_deref(), Some("acme"));
        assert_eq!(scope_from_runner_url("https://github.com/acme/api/").as_deref(), Some("acme/api"));
        assert_eq!(scope_from_runner_url("https://github.com/enterprises/big"), None);
    }

    #[test]
    fn parses_scopes() {
        assert_eq!(parse_scope("org:acme"), (ScopeKind::Org, "acme".into()));
        assert_eq!(parse_scope("repo:acme/api"), (ScopeKind::Repo, "acme/api".into()));
        assert_eq!(parse_scope(" https://github.com/acme/api "), (ScopeKind::Repo, "acme/api".into()));
        assert_eq!(scope_path(ScopeKind::Org, "acme"), "/orgs/acme");
    }

    #[test]
    fn explains_registration_errors() {
        let e = ApiError { status: Some(StatusCode::FORBIDDEN), message: "Resource not accessible".into() };
        let msg = with_hint(explain_doing(ScopeKind::Org, "acme", &e, "gh", "Registering runners with acme"));
        assert_eq!(
            msg,
            "Registering runners with acme needs an org admin token with the admin:org scope. Run `gh auth refresh -h github.com -s admin:org`."
        );
    }

    /// The end of actions/runner v2.338.0's release notes (they use CRLF).
    const RUNNER_NOTES: &str = "## Using your self hosted runner\r\n\r\nSee the docs.\r\n\r\n\
## SHA-256 Checksums\r\n\r\n\
The SHA-256 checksums for the packages included in this build are shown below:\r\n\r\n\
- actions-runner-win-x64-2.338.0.zip <!-- BEGIN SHA win-x64 -->f48e0750a21812bca5f82de5f7f5aeae71abee647fab5a582f1742d07eba455f<!-- END SHA win-x64 -->\r\n\
- actions-runner-win-arm64-2.338.0.zip <!-- BEGIN SHA win-arm64 -->a57a4d29c0dcf02210a16b01906ed76cb8091b26d728c41232736c0f627a557e<!-- END SHA win-arm64 -->\r\n\
- actions-runner-osx-x64-2.338.0.tar.gz <!-- BEGIN SHA osx-x64 -->dea7a58796ce215fc424a8b27c0fdd9b813fa5fc03d109a4a1d38131d6bfa1a3<!-- END SHA osx-x64 -->\r\n\
- actions-runner-osx-arm64-2.338.0.tar.gz <!-- BEGIN SHA osx-arm64 -->df4cebda25c86a886ed204e49fee63f5c2e7cec5f447b5c98440a826bbdf9df2<!-- END SHA osx-arm64 -->\r\n\
- actions-runner-linux-x64-2.338.0.tar.gz <!-- BEGIN SHA linux-x64 -->af4b794c1bc41d73d40535e3fe092a39f9679cd8d965954c2aca25a05ca41d32<!-- END SHA linux-x64 -->\r\n\
- actions-runner-linux-arm64-2.338.0.tar.gz <!-- BEGIN SHA linux-arm64 -->628b4a7258487b80c1d3c221095a7ded349f5ae0175fd3dfab708d07428f041b<!-- END SHA linux-arm64 -->\r\n\
- actions-runner-linux-arm-2.338.0.tar.gz <!-- BEGIN SHA linux-arm -->1d6a7688d2aca1a1fecd4df5d473e4aa4226c1f1785ff915cf84049665a3483f<!-- END SHA linux-arm -->\n";

    #[test]
    fn reads_runner_checksums_from_release_notes() {
        let sums = runner_checksums(RUNNER_NOTES);
        let sha = |platform: &str| sums.iter().find(|(p, _)| p == platform).map(|(_, h)| h.as_str());
        // Every package install.sh (osx/linux: x64, arm64, arm) and install.ps1 (win: x64, arm64) picks.
        assert_eq!(sha("osx-x64"), Some("dea7a58796ce215fc424a8b27c0fdd9b813fa5fc03d109a4a1d38131d6bfa1a3"));
        assert_eq!(sha("osx-arm64"), Some("df4cebda25c86a886ed204e49fee63f5c2e7cec5f447b5c98440a826bbdf9df2"));
        assert_eq!(sha("linux-x64"), Some("af4b794c1bc41d73d40535e3fe092a39f9679cd8d965954c2aca25a05ca41d32"));
        assert_eq!(sha("linux-arm64"), Some("628b4a7258487b80c1d3c221095a7ded349f5ae0175fd3dfab708d07428f041b"));
        assert_eq!(sha("linux-arm"), Some("1d6a7688d2aca1a1fecd4df5d473e4aa4226c1f1785ff915cf84049665a3483f"));
        assert_eq!(sha("win-x64"), Some("f48e0750a21812bca5f82de5f7f5aeae71abee647fab5a582f1742d07eba455f"));
        assert_eq!(sha("win-arm64"), Some("a57a4d29c0dcf02210a16b01906ed76cb8091b26d728c41232736c0f627a557e"));
        assert_eq!(sums.len(), 7);
    }

    #[test]
    fn skips_release_notes_without_good_checksums() {
        // Notes from before the markers, or with none at all: nothing to check against.
        assert!(runner_checksums("## Features\r\n- Faster\r\n").is_empty());
        assert!(runner_checksums("").is_empty());
        let odd = "<!-- BEGIN SHA linux-x64 -->not-hex<!-- END SHA linux-x64 -->\
<!-- BEGIN SHA osx-x64 -->DEA7A58796CE215FC424A8B27C0FDD9B813FA5FC03D109A4A1D38131D6BFA1A3<!-- END SHA osx-x64 -->\
<!-- BEGIN SHA osx-x64 -->0000000000000000000000000000000000000000000000000000000000000000<!-- END SHA osx-x64 -->\
<!-- BEGIN SHA win x64 -->f48e0750a21812bca5f82de5f7f5aeae71abee647fab5a582f1742d07eba455f<!-- END SHA win x64 -->\
<!-- BEGIN SHA linux-arm -->1d6a7688d2aca1a1fecd4df5d473e4aa4226c1f1785ff915cf84049665a3483f<!-- END SHA linux-arm64 -->\
<!-- BEGIN SHA win-arm64 -->a57a4d29";
        // Short or non-hex values, names with spaces and mismatched ends are left out;
        // hex is lowercased, and the first value for a platform wins.
        assert_eq!(runner_checksums(odd), vec![("osx-x64".to_string(), "dea7a58796ce215fc424a8b27c0fdd9b813fa5fc03d109a4a1d38131d6bfa1a3".to_string())]);
    }

    #[test]
    fn explains_org_permission_errors() {
        let e = ApiError { status: Some(StatusCode::FORBIDDEN), message: "Must have admin rights".into() };
        let (msg, hint) = explain(ScopeKind::Org, "acme", &e, "gh");
        assert!(msg.contains("admin:org"));
        assert_eq!(hint.as_deref(), Some("gh auth refresh -h github.com -s admin:org"));
    }
}
