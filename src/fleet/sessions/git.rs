//! What sessions' work changed: for each session's folder, its git branch,
//! the files, lines and commits since the default branch, and the branch's
//! pull request on GitHub with its checks. The machine's agent reads git
//! (`git_info`); GitHub is asked at most every couple of minutes per branch.
//!
//! Looked at when a session stops working (it may have changed files), when
//! one starts, and at least every minute while sessions run.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use super::{MachineSessions, agent};
use crate::fleet::Engine;
use crate::github::hub::{BranchPull, repo_name};

/// At least this often while sessions run.
const LOOK_EVERY: Duration = Duration::from_secs(60);
/// A branch's pull request is asked of GitHub at most this often.
const PULL_EVERY: Duration = Duration::from_secs(120);

/// What the work in a session's folder changed.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GitStatus {
    /// The checkout's top folder.
    pub root: String,
    pub branch: String,
    /// What the branch is compared with: the default branch ("origin/main").
    pub base: Option<String>,
    /// Commits not pushed to, and not pulled from, its upstream branch.
    pub ahead: i64,
    pub behind: i64,
    /// Files with uncommitted changes, new ones included.
    pub uncommitted: u32,
    /// Since it left the base: files, lines added and removed (uncommitted
    /// changes included), and commits.
    pub files: u32,
    pub added: u32,
    pub removed: u32,
    pub commits: u32,
    /// A git worktree of another checkout.
    pub worktree: bool,
    /// "owner/name" when it's on GitHub.
    pub repo: Option<String>,
    /// The branch's pull request, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull: Option<BranchPull>,
}

impl GitStatus {
    /// On the base branch itself ("main" when the base is "origin/main"), or no branch.
    pub fn on_base(&self) -> bool {
        self.branch.is_empty() || self.branch == "(detached)" || self.base.as_deref().and_then(|b| b.split_once('/')).is_some_and(|(_, b)| b == self.branch)
    }

    /// In a few words, for the CLI: "fix-login +12 −3, 2 uncommitted, #14 open".
    pub fn short(&self) -> String {
        let mut out = if self.branch.is_empty() { "no branch".to_string() } else { self.branch.clone() };
        if self.added > 0 || self.removed > 0 {
            out += &format!(" +{} −{}", self.added, self.removed);
        }
        if self.uncommitted > 0 {
            out += &format!(", {} uncommitted", self.uncommitted);
        }
        if self.ahead > 0 {
            out += &format!(", {} unpushed", self.ahead);
        }
        if let Some(p) = &self.pull {
            out += &format!(", #{} {}", p.number, p.state);
        }
        out
    }
}

/// The agent's answer for one folder, or None outside a git checkout.
pub fn parse(v: &Value) -> Option<(String, GitStatus)> {
    if v["git"].as_bool() != Some(true) {
        return None;
    }
    let n = |k: &str| v[k].as_u64().unwrap_or(0) as u32;
    let path = v["path"].as_str()?.to_string();
    let status = GitStatus {
        root: v["root"].as_str().unwrap_or_default().to_string(),
        branch: v["branch"].as_str().unwrap_or_default().to_string(),
        base: v["base"].as_str().map(str::to_string),
        ahead: v["ahead"].as_i64().unwrap_or(0),
        behind: v["behind"].as_i64().unwrap_or(0),
        uncommitted: n("dirty"),
        files: n("files"),
        added: n("added"),
        removed: n("removed"),
        commits: n("commits"),
        worktree: v["worktree"].as_bool().unwrap_or(false),
        // Only a remote on GitHub (a folder's path would read as "owner/name").
        repo: v["remote"].as_str().filter(|r| r.contains("github.com")).and_then(repo_name),
        pull: None,
    };
    Some((path, status))
}

/// One look at a time per machine.
fn looking() -> &'static Mutex<HashSet<String>> {
    static LOOKING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    LOOKING.get_or_init(Default::default)
}

impl Engine {
    /// After a look at a machine's sessions: looks at what their work changed
    /// when one went quiet or started, or the last look was a while ago.
    pub(crate) fn maybe_look_at_git(self: &Arc<Self>, machine_id: &str, before: Option<&MachineSessions>, now: &MachineSessions) {
        let was = |id: &str| before.and_then(|b| b.sessions.iter().find(|s| s.id == id));
        let quieted = now.sessions.iter().any(|s| !s.busy && was(&s.id).is_some_and(|p| p.busy));
        let started = now.sessions.iter().any(|s| was(&s.id).is_none());
        let stale = self.git_looked_at(machine_id).is_none_or(|t| t.elapsed() > LOOK_EVERY);
        if !(quieted || started || stale) || now.sessions.is_empty() {
            return;
        }
        if !looking().lock().unwrap().insert(machine_id.to_string()) {
            return;
        }
        let (me, id) = (self.clone(), machine_id.to_string());
        tokio::spawn(async move {
            if let Err(e) = me.look_at_git(&id).await {
                tracing::debug!("git on {id}: {e}");
            }
            looking().lock().unwrap().remove(&id);
        });
    }

    /// Reads git in the folders of a machine's sessions (and its workspaces),
    /// finds each branch's pull request, and publishes it all.
    pub async fn look_at_git(self: &Arc<Self>, machine_id: &str) -> Result<(), String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        let mut folders: Vec<String> = Vec::new();
        let sessions = self.sessions_of(&spec.id).map(|f| f.sessions).unwrap_or_default();
        let snapshot = self.current();
        let workspaces = snapshot.workspaces.iter().filter(|w| w.machine_id == spec.id && w.status == "ready");
        for f in sessions.iter().filter(|s| s.status == "running").map(|s| s.cwd.clone()).chain(workspaces.map(|w| w.path.clone())) {
            if !f.is_empty() && !folders.contains(&f) {
                folders.push(f);
            }
        }
        if folders.is_empty() {
            self.set_git(&spec.id, Vec::new());
            self.set_git_looked(&spec.id);
            return Ok(());
        }
        let found: Vec<(String, GitStatus)> = match self.demo_world() {
            Some(world) => world.git(&spec.id, &folders),
            None => {
                agent::ensure(&spec, probe.as_ref()).await?;
                // When it fails (an agent from before git_info), don't ask again at once.
                let reply = agent::ctl(&spec, &json!({ "op": "git_info", "paths": folders })).await.inspect_err(|_| self.set_git_looked(&spec.id))?;
                reply["folders"].as_array().into_iter().flatten().filter_map(parse).collect()
            }
        };
        let mut out = Vec::with_capacity(found.len());
        for (path, mut status) in found {
            if status.pull.is_none()
                && !status.on_base()
                && let Some(repo) = status.repo.clone()
            {
                status.pull = self.branch_pull(&repo, &status.branch).await;
            }
            out.push((path, status));
        }
        self.set_git(&spec.id, out);
        self.set_git_looked(&spec.id);
        Ok(())
    }

    /// A branch's pull request, from GitHub at most every couple of minutes.
    async fn branch_pull(&self, repo: &str, branch: &str) -> Option<BranchPull> {
        let key = (repo.to_lowercase(), branch.to_string());
        if let Some(cached) = self.cached_pull(&key, PULL_EVERY) {
            return cached;
        }
        let found = self.github().branch_pull(&self.settings().github, repo, branch).await;
        let pull = match found {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("pull request for {repo} {branch}: {e}");
                None
            }
        };
        self.cache_pull(key, pull.clone());
        pull
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_agents_answer() {
        let v = json!({
            "path": "~/code/api", "git": true, "root": "/home/me/code/api", "remote": "git@github.com:acme/api.git",
            "branch": "fix-login", "base": "origin/main", "ahead": 2, "behind": 0, "dirty": 3, "untracked": 1,
            "files": 4, "added": 120, "removed": 40, "commits": 2, "worktree": true,
        });
        let (path, s) = parse(&v).unwrap();
        assert_eq!(path, "~/code/api");
        assert_eq!((s.branch.as_str(), s.repo.as_deref(), s.uncommitted, s.files, s.added, s.removed, s.commits), ("fix-login", Some("acme/api"), 3, 4, 120, 40, 2));
        assert!(s.worktree && !s.on_base());
        assert!(parse(&json!({ "path": "~", "git": false })).is_none());
        let local = json!({ "path": "/x", "git": true, "remote": "/srv/git/acme/api.git" });
        assert_eq!(parse(&local).unwrap().1.repo, None);
    }

    #[test]
    fn knows_the_base_branch() {
        let s = |branch: &str| GitStatus { branch: branch.into(), base: Some("origin/main".into()), ..Default::default() };
        assert!(s("main").on_base() && s("(detached)").on_base() && s("").on_base());
        assert!(!s("fix").on_base());
    }

    #[test]
    fn says_it_in_a_few_words() {
        let mut s = GitStatus { branch: "fix".into(), added: 12, removed: 3, uncommitted: 2, ahead: 1, ..Default::default() };
        assert_eq!(s.short(), "fix +12 −3, 2 uncommitted, 1 unpushed");
        s.pull = Some(BranchPull { number: 14, title: "Fix".into(), state: "merged".into(), url: String::new(), checks: Default::default() });
        assert!(s.short().ends_with(", #14 merged"));
        assert_eq!(GitStatus::default().short(), "no branch");
    }
}
