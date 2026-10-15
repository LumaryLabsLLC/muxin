//! Your GitHub, beyond runners: who you are, your repositories, what needs you
//! (review requests, assigned issues, your open pull requests), and each
//! repository's issues, pull requests and Actions runs. Read-only, through the
//! same ETag-cached client as runner status.

use chrono::{DateTime, Utc};
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use super::Client;
use crate::config::GitHubSettings;

#[derive(Debug, Clone, Serialize)]
pub struct Viewer {
    pub login: String,
    pub name: Option<String>,
    pub html_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    /// "owner/name".
    pub full_name: String,
    pub description: Option<String>,
    pub private: bool,
    pub fork: bool,
    pub archived: bool,
    pub default_branch: String,
    pub language: Option<String>,
    pub stars: u32,
    /// Open issues and pull requests together, as GitHub counts them.
    pub open_issues: u32,
    pub pushed_at: Option<DateTime<Utc>>,
    pub html_url: String,
    /// Owned by an organization (else by a person: you, or someone who shared it with you).
    pub owner_is_org: bool,
    /// Starred in Muxen (not GitHub's stars).
    pub favorite: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Label {
    pub name: String,
    /// "rrggbb".
    pub color: String,
}

/// An issue or a pull request, as listed.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub pull_request: bool,
    pub draft: bool,
    /// "open" or "closed".
    pub state: String,
    pub author: String,
    pub assignees: Vec<String>,
    pub labels: Vec<Label>,
    pub comments: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub html_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Checks {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    /// Names of the failed checks.
    pub failing: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Review {
    pub author: String,
    /// "APPROVED", "CHANGES_REQUESTED", "COMMENTED", ...
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PullInfo {
    pub head: String,
    /// Where the head branch lives ("owner/name"): a fork, or the repository itself.
    pub head_repo: Option<String>,
    pub base: String,
    pub mergeable: Option<bool>,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
    pub checks: Checks,
    pub reviews: Vec<Review>,
}

/// One issue or pull request with its description and conversation.
#[derive(Debug, Clone, Serialize)]
pub struct Detail {
    pub item: Item,
    pub body: String,
    pub comments: Vec<Comment>,
    pub pull: Option<PullInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub name: String,
    /// "queued", "in_progress", "completed".
    pub status: String,
    /// "success", "failure", "cancelled", ... once completed.
    pub conclusion: Option<String>,
    pub branch: String,
    pub event: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub html_url: String,
}

/// What needs you across GitHub.
#[derive(Debug, Clone, Serialize)]
pub struct Inbox {
    pub viewer: Viewer,
    pub review_requests: Vec<Item>,
    pub assigned: Vec<Item>,
    pub authored: Vec<Item>,
}

/// A repository's open issues and pull requests.
#[derive(Debug, Clone, Serialize)]
pub struct RepoItems {
    pub issues: Vec<Item>,
    pub pulls: Vec<Item>,
}

// GitHub's JSON, as much of it as we read.

#[derive(Deserialize)]
struct RawUser {
    login: String,
    name: Option<String>,
    html_url: Option<String>,
    #[serde(default)]
    id: u64,
}

#[derive(Deserialize)]
struct RawRepo {
    full_name: String,
    description: Option<String>,
    private: bool,
    #[serde(default)]
    fork: bool,
    #[serde(default)]
    archived: bool,
    default_branch: Option<String>,
    language: Option<String>,
    #[serde(default)]
    stargazers_count: u32,
    #[serde(default)]
    open_issues_count: u32,
    pushed_at: Option<DateTime<Utc>>,
    html_url: String,
    owner: Option<RawOwner>,
}

#[derive(Deserialize)]
struct RawOwner {
    #[serde(rename = "type", default)]
    kind: String,
}

#[derive(Deserialize)]
struct RawLabel {
    name: String,
    #[serde(default)]
    color: String,
}

#[derive(Deserialize)]
struct RawLogin {
    login: String,
}

#[derive(Deserialize)]
struct RawIssue {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    user: Option<RawLogin>,
    #[serde(default)]
    assignees: Vec<RawLogin>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    comments: u32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    html_url: String,
    pull_request: Option<serde_json::Value>,
    body: Option<String>,
    repository_url: Option<String>,
}

#[derive(Deserialize)]
struct RawSearch {
    items: Vec<RawIssue>,
}

#[derive(Deserialize)]
struct RawComment {
    user: Option<RawLogin>,
    body: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct RawRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
    /// Gone when a fork was deleted.
    repo: Option<RawRefRepo>,
}

#[derive(Deserialize)]
struct RawRefRepo {
    full_name: String,
}

#[derive(Deserialize)]
struct RawPull {
    head: RawRef,
    base: RawRef,
    mergeable: Option<bool>,
    #[serde(default)]
    additions: u32,
    #[serde(default)]
    deletions: u32,
    #[serde(default)]
    changed_files: u32,
}

/// A pull request in a list, as `branch_pull` finds it.
#[derive(Deserialize)]
struct RawPullItem {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    merged_at: Option<String>,
    html_url: String,
    head: RawRef,
}

/// A branch's pull request: "open", "draft", "merged" or "closed", and its checks.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BranchPull {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
    pub checks: Checks,
}

/// Check runs, counted: passed, failed (by name) and still going.
fn checks_of(runs: Vec<RawCheckRun>) -> Checks {
    let mut checks = Checks::default();
    for run in runs {
        checks.total += 1;
        match (run.status.as_str(), run.conclusion.as_deref()) {
            ("completed", Some("success" | "neutral" | "skipped")) => checks.passed += 1,
            ("completed", _) => {
                checks.failed += 1;
                checks.failing.push(run.name);
            }
            _ => checks.pending += 1,
        }
    }
    checks
}

/// A value for a URL's query: the characters that would end or change it, escaped.
fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => out.push(c),
            c => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct RawReview {
    user: Option<RawLogin>,
    state: String,
}

#[derive(Deserialize)]
struct RawCheckRuns {
    check_runs: Vec<RawCheckRun>,
}

#[derive(Deserialize)]
struct RawCheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct RawRuns {
    workflow_runs: Vec<RawRun>,
}

#[derive(Deserialize)]
struct RawRun {
    name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    head_branch: Option<String>,
    event: String,
    display_title: Option<String>,
    created_at: DateTime<Utc>,
    html_url: String,
}

fn item(repo: &str, raw: RawIssue) -> Item {
    // Search results say which repository through its API URL.
    let repo = raw
        .repository_url
        .as_deref()
        .and_then(|u| u.split("/repos/").nth(1))
        .map(str::to_string)
        .unwrap_or_else(|| repo.to_string());
    Item {
        repo,
        number: raw.number,
        title: raw.title,
        pull_request: raw.pull_request.is_some(),
        draft: raw.draft,
        state: raw.state,
        author: raw.user.map(|u| u.login).unwrap_or_default(),
        assignees: raw.assignees.into_iter().map(|u| u.login).collect(),
        labels: raw.labels.into_iter().map(|l| Label { name: l.name, color: l.color }).collect(),
        comments: raw.comments,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        html_url: raw.html_url,
    }
}

fn repo(raw: RawRepo) -> Repo {
    Repo {
        full_name: raw.full_name,
        description: raw.description.filter(|d| !d.is_empty()),
        private: raw.private,
        fork: raw.fork,
        archived: raw.archived,
        default_branch: raw.default_branch.unwrap_or_else(|| "main".into()),
        language: raw.language,
        stars: raw.stargazers_count,
        open_issues: raw.open_issues_count,
        pushed_at: raw.pushed_at,
        html_url: raw.html_url,
        owner_is_org: raw.owner.is_some_and(|o| o.kind == "Organization"),
        favorite: false,
    }
}

/// "owner/name" from a GitHub remote or URL (https, ssh, scp-style, including
/// ssh host aliases like `github.com-work`), or plain "owner/name".
pub fn repo_name(input: &str) -> Option<String> {
    let s = input.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = if let Some((_, rest)) = s.split_once("://") {
        // https://github.com/o/r, ssh://git@github.com/o/r
        let (host, path) = rest.split_once('/')?;
        if !host.contains("github.com") {
            return None;
        }
        path
    } else if let Some((host, path)) = s.split_once(':') {
        // git@github.com:o/r, git@github.com-work:o/r
        if !host.contains("github.com") {
            return None;
        }
        path
    } else {
        s.strip_prefix("github.com/").unwrap_or(s)
    };
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let (owner, name) = (parts.next()?, parts.next()?);
    let ok = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    (ok(owner) && ok(name)).then(|| format!("{owner}/{name}"))
}

/// "owner/name#123", or a GitHub issue/PR URL, as (repo, number).
pub fn issue_ref(input: &str) -> Option<(String, u64)> {
    let s = input.trim();
    if let Some((repo, num)) = s.rsplit_once('#') {
        return Some((repo_name(repo)?, num.parse().ok()?));
    }
    let tail = s.split("github.com/").nth(1)?;
    let parts: Vec<&str> = tail.split('/').collect();
    match parts.as_slice() {
        [owner, name, "issues" | "pull", num, ..] => Some((format!("{owner}/{name}"), num.parse().ok()?)),
        _ => None,
    }
}

impl Client {
    /// A name and private email to commit as `login`: their profile name and
    /// GitHub's noreply address (commits still count as theirs).
    pub async fn commit_identity(&self, settings: &GitHubSettings, login: &str) -> Result<(String, String), String> {
        self.configure(settings);
        let u: RawUser = self.get(&format!("/users/{login}")).await.map_err(|e| e.message)?;
        let name = u.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| u.login.clone());
        Ok((name, format!("{}+{}@users.noreply.github.com", u.id, u.login)))
    }

    pub async fn viewer(&self, settings: &GitHubSettings) -> Result<Viewer, String> {
        self.configure(settings);
        let u: RawUser = self.get("/user").await.map_err(|e| e.message)?;
        Ok(Viewer { html_url: u.html_url.unwrap_or_else(|| format!("https://github.com/{}", u.login)), login: u.login, name: u.name })
    }

    /// Repositories you own, collaborate on, or reach through an org, most recently pushed first.
    pub async fn my_repos(&self, settings: &GitHubSettings) -> Result<Vec<Repo>, String> {
        self.configure(settings);
        let pages: Vec<Vec<RawRepo>> = self
            .get_all("/user/repos?per_page=100&sort=pushed&affiliation=owner,collaborator,organization_member", 4)
            .await
            .map_err(|e| e.message)?;
        Ok(pages.into_iter().flatten().map(repo).collect())
    }

    pub async fn repo(&self, settings: &GitHubSettings, full_name: &str) -> Result<Repo, String> {
        self.configure(settings);
        let raw: RawRepo = self.get(&format!("/repos/{full_name}")).await.map_err(|e| e.message)?;
        Ok(repo(raw))
    }

    /// Review requests, issues assigned to you, and your open pull requests.
    pub async fn inbox(&self, settings: &GitHubSettings) -> Result<Inbox, String> {
        self.configure(settings);
        let viewer = self.viewer(settings).await?;
        let search = |q: &'static str| async move {
            let path = format!("/search/issues?q={}&sort=updated&order=desc&per_page=40", q.replace(' ', "+"));
            self.get::<RawSearch>(&path).await.map(|r| r.items.into_iter().map(|i| item("", i)).collect::<Vec<_>>())
        };
        let (reviews, assigned, authored) = futures::join!(
            search("is:open is:pr review-requested:@me archived:false"),
            search("is:open assignee:@me archived:false"),
            search("is:open is:pr author:@me archived:false"),
        );
        Ok(Inbox {
            viewer,
            review_requests: reviews.map_err(|e| e.message)?,
            assigned: assigned.map_err(|e| e.message)?,
            authored: authored.map_err(|e| e.message)?,
        })
    }

    /// Open issues and pull requests, most recently updated first.
    pub async fn repo_items(&self, settings: &GitHubSettings, full_name: &str) -> Result<RepoItems, String> {
        self.configure(settings);
        let pages: Vec<Vec<RawIssue>> = self
            .get_all(&format!("/repos/{full_name}/issues?state=open&sort=updated&per_page=100"), 3)
            .await
            .map_err(|e| e.message)?;
        let (pulls, issues): (Vec<Item>, Vec<Item>) = pages.into_iter().flatten().map(|i| item(full_name, i)).partition(|i| i.pull_request);
        Ok(RepoItems { issues, pulls })
    }

    /// An issue or pull request with its conversation (and for pull requests,
    /// branches, size, checks and reviews).
    pub async fn detail(&self, settings: &GitHubSettings, full_name: &str, number: u64) -> Result<Detail, String> {
        self.configure(settings);
        let raw: RawIssue = self.get(&format!("/repos/{full_name}/issues/{number}")).await.map_err(|e| e.message)?;
        let body = raw.body.clone().unwrap_or_default();
        let is_pr = raw.pull_request.is_some();
        let comments_path = format!("/repos/{full_name}/issues/{number}/comments?per_page=100");
        let comments: Vec<RawComment> = self.get(&comments_path).await.unwrap_or_default();
        let comments = comments
            .into_iter()
            .map(|c| Comment { author: c.user.map(|u| u.login).unwrap_or_default(), body: c.body.unwrap_or_default(), created_at: c.created_at })
            .collect();
        let pull = if is_pr { Some(self.pull_info(full_name, number).await?) } else { None };
        Ok(Detail { item: item(full_name, raw), body, comments, pull })
    }

    async fn pull_info(&self, full_name: &str, number: u64) -> Result<PullInfo, String> {
        let p: RawPull = self.get(&format!("/repos/{full_name}/pulls/{number}")).await.map_err(|e| e.message)?;
        let reviews_path = format!("/repos/{full_name}/pulls/{number}/reviews?per_page=100");
        let checks_path = format!("/repos/{full_name}/commits/{}/check-runs?per_page=100", p.head.sha);
        let (reviews, runs) = futures::join!(self.get::<Vec<RawReview>>(&reviews_path), self.get::<RawCheckRuns>(&checks_path));
        // The latest review per person is what counts.
        let mut latest: Vec<Review> = Vec::new();
        for r in reviews.unwrap_or_default() {
            let author = r.user.map(|u| u.login).unwrap_or_default();
            if r.state == "COMMENTED" && latest.iter().any(|x| x.author == author) {
                continue;
            }
            latest.retain(|x| x.author != author);
            latest.push(Review { author, state: r.state });
        }
        let checks = checks_of(runs.map(|r| r.check_runs).unwrap_or_default());
        Ok(PullInfo {
            head_repo: p.head.repo.map(|r| r.full_name),
            head: p.head.name,
            base: p.base.name,
            mergeable: p.mergeable,
            additions: p.additions,
            deletions: p.deletions,
            changed_files: p.changed_files,
            checks,
            reviews: latest,
        })
    }

    /// The pull request for a branch of a repository (the newest, open or not),
    /// with its checks; None when the branch has none.
    pub async fn branch_pull(&self, settings: &GitHubSettings, full_name: &str, branch: &str) -> Result<Option<BranchPull>, String> {
        self.configure(settings);
        let owner = full_name.split('/').next().unwrap_or_default();
        let path = format!("/repos/{full_name}/pulls?state=all&per_page=1&head={owner}:{}", query_value(branch));
        let found: Vec<RawPullItem> = self.get(&path).await.map_err(|e| e.message)?;
        let Some(p) = found.into_iter().next() else { return Ok(None) };
        let runs: Option<RawCheckRuns> = self.get(&format!("/repos/{full_name}/commits/{}/check-runs?per_page=100", p.head.sha)).await.ok();
        let state = match (&p.merged_at, p.state.as_str(), p.draft) {
            (Some(_), _, _) => "merged",
            (None, "closed", _) => "closed",
            (None, _, true) => "draft",
            _ => "open",
        };
        Ok(Some(BranchPull { number: p.number, title: p.title, state: state.into(), url: p.html_url, checks: checks_of(runs.map(|r| r.check_runs).unwrap_or_default()) }))
    }

    /// Recent GitHub Actions runs.
    pub async fn repo_runs(&self, settings: &GitHubSettings, full_name: &str) -> Result<Vec<Run>, String> {
        self.configure(settings);
        let raw: RawRuns = self.get(&format!("/repos/{full_name}/actions/runs?per_page=20")).await.map_err(|e| e.message)?;
        Ok(raw
            .workflow_runs
            .into_iter()
            .map(|r| Run {
                name: r.name.unwrap_or_default(),
                status: r.status.unwrap_or_default(),
                conclusion: r.conclusion,
                branch: r.head_branch.unwrap_or_default(),
                event: r.event,
                title: r.display_title.unwrap_or_default(),
                created_at: r.created_at,
                html_url: r.html_url,
            })
            .collect())
    }

    /// Several repositories' metadata at once (for checkouts of repos not in your list).
    #[allow(dead_code)]
    pub async fn repos_by_name(&self, settings: &GitHubSettings, names: &[String]) -> Vec<Repo> {
        let all = join_all(names.iter().map(|n| self.repo(settings, n))).await;
        all.into_iter().filter_map(Result::ok).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_and_issue_refs() {
        assert_eq!(repo_name("git@github.com:LumaryLabsLLC/muxin.git").as_deref(), Some("LumaryLabsLLC/muxin"));
        assert_eq!(repo_name("https://github.com/zennotes/zennotes").as_deref(), Some("zennotes/zennotes"));
        assert_eq!(repo_name("acme/api").as_deref(), Some("acme/api"));
        assert_eq!(repo_name("https://gitlab.com/x/y"), None);
        assert_eq!(repo_name("git@github.com-contractorhub:ContractorHUB-dev/ContractorHUB.git").as_deref(), Some("ContractorHUB-dev/ContractorHUB"));
        assert_eq!(repo_name("ssh://git@ssh.github.com:443/acme/api.git").as_deref(), Some("acme/api"));
        assert_eq!(issue_ref("https://github.com/acme/api/issues/42"), Some(("acme/api".into(), 42)));
        assert_eq!(issue_ref("https://github.com/acme/api/pull/7/files"), Some(("acme/api".into(), 7)));
        assert_eq!(issue_ref("acme/api#12"), Some(("acme/api".into(), 12)));
    }
}
