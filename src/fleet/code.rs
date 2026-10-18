//! Code on the machines: git checkouts (found by each machine's agent in the
//! configured code folders), linked to their GitHub repositories, with fetch,
//! pull and push; and whether git there can commit and push (identity,
//! credentials), with one-step fixes.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};

use super::Engine;
use super::model::Checkout;
use super::sessions::agent;
use crate::github::hub::repo_name;
use crate::ops::remote::{ps_quote, sh_quote, shell_for};
use crate::probe::Shell;

impl Engine {
    /// Finds the git checkouts on a machine (through its agent) and publishes them.
    pub async fn refresh_checkouts(self: &Arc<Self>, machine_id: &str) -> Result<Vec<Checkout>, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if let Some(world) = self.demo_world() {
            let mut found = self.checkouts_of(&spec.id);
            if found.is_empty() {
                found = world.checkouts(&spec.id);
                self.set_checkouts(&spec.id, found.clone());
            }
            return Ok(found);
        }
        if !spec.is_checkable() {
            return Ok(Vec::new());
        }
        agent::ensure(&spec, probe.as_ref()).await?;
        let roots = self.settings().code_folders;
        let reply = agent::ctl(&spec, &json!({ "op": "repos", "roots": roots })).await?;
        let found: Vec<Checkout> = reply["repos"].as_array().into_iter().flatten().map(|r| checkout(&spec.id, r)).collect();
        self.set_checkouts(&spec.id, found.clone());
        Ok(found)
    }

    /// The token of a machine's GitHub CLI login, for using GitHub as that
    /// machine's account. It's the machine's active login; callers check whose.
    pub async fn machine_gh_token(self: &Arc<Self>, machine_id: &str, host: &str) -> Result<String, String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        // Where gh is, from the agent (non-interactive ssh often lacks Homebrew's PATH).
        let gh = match self.sessions_of(&spec.id).and_then(|s| s.gh) {
            Some(g) => g.path,
            None => self.refresh_sessions(&spec.id).await.ok().and_then(|s| s.gh).and_then(|g| g.path),
        };
        agent::gh_token(&spec, gh.as_deref().unwrap_or("gh"), host).await
    }

    /// Runs `git fetch`, `git pull --ff-only` or `git push` in a checkout, then re-reads it.
    pub async fn checkout_git(self: &Arc<Self>, machine_id: &str, path: &str, action: &str) -> Result<String, String> {
        if !matches!(action, "fetch" | "pull" | "push") {
            return Err(format!("\"{action}\" isn't a git action here; use fetch, pull or push."));
        }
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(format!("demo: would run git {action} in {path}"));
        }
        agent::ensure(&spec, probe.as_ref()).await?;
        let reply = agent::ctl(&spec, &json!({ "op": "git", "path": path, "action": action })).await;
        let _ = self.refresh_checkouts(&spec.id).await;
        reply.map(|r| r["output"].as_str().unwrap_or("").to_string())
    }
}

/// How git is set up on a machine, for committing and pushing to GitHub.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct GitSetup {
    /// git's version; None when git isn't installed.
    pub git: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    /// Credential helpers git would use for github.com ("gh", "osxkeychain", "store", ...).
    pub helpers: Vec<String>,
    /// git gets GitHub logins from the GitHub CLI (`gh auth setup-git`).
    pub uses_gh: bool,
    /// Private keys in ~/.ssh (for pushing over SSH).
    pub ssh_keys: Vec<String>,
}

const GIT_STATUS_SH: &str = r#"command -v git >/dev/null 2>&1 || { echo "git="; exit 0; }
echo "git=$(git --version 2>/dev/null | awk '{print $3}')"
# As git sees it outside a repository: system and global files, includes too.
cd "$HOME" || exit 0
echo "name=$(git config user.name 2>/dev/null)"
echo "email=$(git config user.email 2>/dev/null)"
git config --get-regexp '^credential\.' 2>/dev/null | while read -r k v; do echo "cred=$k $v"; done
for f in "$HOME"/.ssh/id_*; do case "$f" in *.pub) ;; *) [ -f "$f" ] && echo "key=${f##*/}" ;; esac; done
exit 0
"#;

/// The same, from PowerShell (Windows).
const GIT_STATUS_PS1: &str = r#"$g = Get-Command git -ErrorAction SilentlyContinue
if (-not $g) { 'git='; return }
'git=' + ((git --version) -replace '^git version ', '' -replace '\.windows.*$', '')
Set-Location $env:USERPROFILE
'name=' + (git config user.name)
'email=' + (git config user.email)
git config --get-regexp '^credential\.' 2>$null | ForEach-Object { 'cred=' + $_ }
Get-ChildItem (Join-Path $env:USERPROFILE '.ssh') -Filter 'id_*' -ErrorAction SilentlyContinue | Where-Object { $_.Extension -ne '.pub' } | ForEach-Object { 'key=' + $_.Name }
"#;

impl Engine {
    /// Reads how git is set up on a machine.
    pub async fn git_setup_status(self: &Arc<Self>, machine_id: &str) -> Result<GitSetup, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(GitSetup { git: Some("2.47.0".into()), ..Default::default() });
        }
        if !spec.is_checkable() {
            return Err(format!("{} has no host to connect to.", spec.name));
        }
        let out = if matches!(shell_for(&spec, probe.as_ref()), Shell::Powershell) {
            agent::run_ps(&spec, GIT_STATUS_PS1, std::time::Duration::from_secs(25)).await?
        } else {
            agent::run_sh(&spec, GIT_STATUS_SH, std::time::Duration::from_secs(25)).await?
        };
        Ok(parse_git_setup(&out))
    }

    /// Sets git's name and email on a machine (`git config --global`), and/or
    /// makes git use the machine's GitHub CLI login for github.com
    /// (`gh auth setup-git`). Returns the new state.
    pub async fn git_setup_apply(self: &Arc<Self>, machine_id: &str, identity: Option<(String, String)>, credentials: bool) -> Result<GitSetup, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(GitSetup { git: Some("2.47.0".into()), uses_gh: credentials, ..Default::default() });
        }
        let windows = matches!(shell_for(&spec, probe.as_ref()), Shell::Powershell);
        let q = |s: &str| if windows { ps_quote(s) } else { sh_quote(s) };
        let mut script = String::from(if windows { "$ErrorActionPreference = 'Stop'\n" } else { "set -e\n" });
        if let Some((name, email)) = &identity {
            let bad = |s: &str| s.trim().is_empty() || s.len() > 200 || s.chars().any(char::is_control);
            if bad(name) || bad(email) || !email.contains('@') || email.contains(char::is_whitespace) {
                return Err("Give a name and an email address (like you@example.com).".into());
            }
            script.push_str(&format!("git config --global user.name {}\ngit config --global user.email {}\n", q(name.trim()), q(email.trim())));
        }
        if credentials {
            let gh = self.sessions_of(&spec.id).and_then(|s| s.gh);
            if !gh.as_ref().is_some_and(|g| g.logged_in) {
                return Err(format!("Sign in to the GitHub CLI on {} first; git will then use that login.", spec.name));
            }
            let gh = gh.and_then(|g| g.path).unwrap_or_else(|| "gh".into());
            script.push_str(&format!("{}{} auth setup-git --hostname github.com\n", if windows { "& " } else { "" }, q(&gh)));
        }
        if identity.is_some() || credentials {
            if windows {
                agent::run_ps(&spec, &script, std::time::Duration::from_secs(30)).await?;
            } else {
                agent::run_sh(&spec, &script, std::time::Duration::from_secs(30)).await?;
            }
        }
        self.git_setup_status(&spec.id).await
    }
}

fn parse_git_setup(out: &str) -> GitSetup {
    let mut s = GitSetup::default();
    let value = |v: &str| Some(v.trim().to_string()).filter(|v| !v.is_empty());
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("git=") {
            s.git = value(v);
        } else if let Some(v) = line.strip_prefix("name=") {
            s.name = value(v);
        } else if let Some(v) = line.strip_prefix("email=") {
            s.email = value(v);
        } else if let Some(v) = line.strip_prefix("key=") {
            s.ssh_keys.push(v.trim().to_string());
        } else if let Some((key, helper)) = line.strip_prefix("cred=").and_then(|l| l.split_once(' ')) {
            // `credential.helper` applies everywhere; `credential.<url>.helper` to that host.
            let applies = key == "credential.helper" || (key.ends_with(".helper") && key.contains("github.com"));
            let helper = helper.trim();
            if !applies || helper.is_empty() {
                continue;
            }
            // gh writes `!<its path> auth git-credential`, its path in '…' when
            // it has a space or a backslash (always, on Windows).
            let words = helper_words(helper);
            let short = if matches!(words.as_slice(), [p, a, c, ..] if program_name(p).eq_ignore_ascii_case("gh") && a == "auth" && c == "git-credential") {
                s.uses_gh = true;
                "gh".to_string()
            } else {
                let first = words.first().map(String::as_str).unwrap_or(helper);
                program_name(first).trim_start_matches("git-credential-").to_string()
            };
            if !s.helpers.contains(&short) {
                s.helpers.push(short);
            }
        }
    }
    s
}

/// A credential helper's words as git's shell splits them: a leading `!`
/// dropped, and what's in '…' or "…" kept as one.
fn helper_words(helper: &str) -> Vec<String> {
    let (mut words, mut word, mut quote) = (Vec::new(), None::<String>, None::<char>);
    for c in helper.trim().trim_start_matches('!').chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.get_or_insert_with(String::new).push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            None if c.is_whitespace() => words.extend(word.take()),
            None => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    words
}

/// A program's name without its folder (either slash: a Windows machine's
/// paths are read here too) or `.exe`.
fn program_name(path: &str) -> &str {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.len().checked_sub(4) {
        Some(i) if base.is_char_boundary(i) && base[i..].eq_ignore_ascii_case(".exe") => &base[..i],
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_git_setup() {
        let out = "git=2.47.0\nname=Ada\nemail=\ncred=credential.helper osxkeychain\ncred=credential.https://github.com.helper \ncred=credential.https://github.com.helper !/opt/homebrew/bin/gh auth git-credential\ncred=credential.https://gitlab.com.helper store\nkey=id_ed25519\n";
        let s = parse_git_setup(out);
        assert_eq!(s.git.as_deref(), Some("2.47.0"));
        assert_eq!(s.name.as_deref(), Some("Ada"));
        assert_eq!(s.email, None);
        assert!(s.uses_gh);
        assert_eq!(s.helpers, vec!["osxkeychain", "gh"]);
        assert_eq!(s.ssh_keys, vec!["id_ed25519"]);
        assert_eq!(parse_git_setup("git=\n").git, None);
    }

    #[test]
    fn knows_gh_on_windows_and_quoted_paths() {
        let out = "cred=credential.helper manager\ncred=credential.https://github.com.helper \ncred=credential.https://github.com.helper !'C:\\Program Files\\GitHub CLI\\gh.exe' auth git-credential\n";
        let s = parse_git_setup(out);
        assert!(s.uses_gh);
        assert_eq!(s.helpers, vec!["manager", "gh"]);
        for helper in ["!\"C:\\Program Files\\GitHub CLI\\GH.EXE\" auth git-credential", "!'/home/me/my apps/gh' auth git-credential", "!gh auth git-credential"] {
            assert!(parse_git_setup(&format!("cred=credential.helper {helper}\n")).uses_gh, "{helper}");
        }
        // Other helpers keep their own names, from a quoted path too.
        let s = parse_git_setup("cred=credential.helper !'C:\\Program Files\\Git\\mingw64\\bin\\git-credential-manager.exe'\ncred=credential.helper !gh-other auth git-credential\n");
        assert!(!s.uses_gh);
        assert_eq!(s.helpers, vec!["manager", "gh-other"]);
    }
}

fn checkout(machine_id: &str, r: &Value) -> Checkout {
    let remote = r["remote"].as_str().map(str::to_string);
    Checkout {
        machine_id: machine_id.to_string(),
        path: r["path"].as_str().unwrap_or("").to_string(),
        repo: remote.as_deref().and_then(repo_name),
        remote,
        branch: r["branch"].as_str().unwrap_or("").to_string(),
        upstream: r["upstream"].as_str().map(str::to_string),
        ahead: r["ahead"].as_i64().unwrap_or(0),
        behind: r["behind"].as_i64().unwrap_or(0),
        dirty: r["dirty"].as_u64().unwrap_or(0) as u32,
        last_commit_at: r["last_commit"].as_i64().and_then(|t| Utc.timestamp_opt(t, 0).single()),
        last_commit: r["subject"].as_str().unwrap_or("").to_string(),
    }
}
