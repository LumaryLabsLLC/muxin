//! Git checkouts on this machine: where they are, which GitHub repository
//! they track, and how they stand (branch, ahead/behind, uncommitted changes);
//! plus fetch/pull/push, and whether the GitHub CLI is signed in here.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Folders that never hold checkouts worth listing, are huge, or (on macOS)
/// are privacy-protected, where even looking makes the system ask the user.
const SKIP: &[&str] = &[
    "node_modules", "target", "vendor", "dist", "build", "out", "Pods", "DerivedData", "Library", "venv", "__pycache__",
    "Applications", "Movies", "Music", "Pictures", "Desktop", "Documents", "Downloads", "Public", "go", "snap",
];
const MAX_REPOS: usize = 300;

fn home() -> PathBuf {
    crate::sys::user_home()
}

fn expand(dir: &str) -> PathBuf {
    match dir.trim() {
        "~" => home(),
        d if d.starts_with("~/") => home().join(&d[2..]),
        d => PathBuf::from(d),
    }
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_REPOS || out.iter().any(|p| p == dir) {
        return;
    }
    if dir.join(".git").exists() {
        out.push(dir.to_path_buf());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) // not following symlinks
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && !SKIP.contains(&name.as_ref())
        })
        .map(|e| e.path())
        .collect();
    children.sort();
    for child in children {
        walk(&child, depth - 1, out);
    }
}

/// Runs a read-only git command for the scan. A checkout's own config can name
/// commands for git to run (core.fsmonitor on status, gpg.program on log), so
/// those are turned off here: looking at a repository runs nothing from it.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "log.showSignature=false"])
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A checkout's branch and where it stands, from `git status`.
#[derive(Default)]
struct Status {
    branch: String,
    upstream: Option<String>,
    ahead: i64,
    behind: i64,
    /// Files with uncommitted changes, untracked ones included.
    dirty: u32,
    untracked: u32,
}

fn status(dir: &Path) -> Status {
    let text = git(dir, &["status", "--porcelain=v2", "--branch"]).unwrap_or_default();
    let mut s = Status::default();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("# branch.head ") {
            s.branch = v.to_string();
        } else if let Some(v) = line.strip_prefix("# branch.upstream ") {
            s.upstream = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("# branch.ab ") {
            let mut parts = v.split(' ');
            s.ahead = parts.next().and_then(|a| a.trim_start_matches('+').parse().ok()).unwrap_or(0);
            s.behind = parts.next().and_then(|b| b.trim_start_matches('-').parse().ok()).unwrap_or(0);
        } else if !line.starts_with('#') && !line.is_empty() {
            s.dirty += 1;
            if line.starts_with("? ") {
                s.untracked += 1;
            }
        }
    }
    s
}

fn origin(dir: &Path) -> Option<String> {
    git(dir, &["config", "--get", "remote.origin.url"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// One checkout's state.
fn describe(dir: &Path) -> Value {
    let s = status(dir);
    let last = git(dir, &["log", "-1", "--format=%ct%x09%s"]).unwrap_or_default();
    let (when, subject) = last.trim().split_once('\t').unwrap_or(("", ""));
    json!({
        "path": dir.display().to_string(),
        "remote": origin(dir),
        "branch": s.branch,
        "upstream": s.upstream,
        "ahead": s.ahead,
        "behind": s.behind,
        "dirty": s.dirty,
        "last_commit": when.parse::<i64>().ok(),
        "subject": subject,
    })
}

/// What the work in a folder changed, for a session there: its checkout's
/// branch, and the files, lines and commits since it left the default branch
/// (`origin/HEAD`), uncommitted changes included. `{"git": false}` outside one.
pub fn info(path: &str) -> Value {
    let dir = expand(path);
    let Some(root) = git(&dir, &["rev-parse", "--show-toplevel"]).map(|r| r.trim().to_string()) else {
        return json!({ "path": path, "git": false });
    };
    let s = status(&dir);
    let base = git(&dir, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
        .map(|b| b.trim().to_string())
        .or_else(|| ["origin/main", "origin/master"].iter().find(|b| git(&dir, &["rev-parse", "--verify", "--quiet", b]).is_some()).map(|b| b.to_string()));
    let fork = base.as_deref().and_then(|b| git(&dir, &["merge-base", "HEAD", b])).map(|m| m.trim().to_string());
    let (mut files, mut added, mut removed, mut commits) = (0u64, 0u64, 0u64, 0u64);
    if let Some(m) = &fork {
        (files, added, removed) = shortstat(&git(&dir, &["diff", "--no-ext-diff", "--no-textconv", "--shortstat", m]).unwrap_or_default());
        commits = git(&dir, &["rev-list", "--count", &format!("{m}..HEAD")]).and_then(|c| c.trim().parse().ok()).unwrap_or(0);
    }
    json!({
        "path": path,
        "git": true,
        "root": root,
        "remote": origin(&dir),
        "branch": s.branch,
        "upstream": s.upstream,
        "ahead": s.ahead,
        "behind": s.behind,
        "dirty": s.dirty,
        "untracked": s.untracked,
        "base": base,
        "files": files + s.untracked as u64,
        "added": added,
        "removed": removed,
        "commits": commits,
        // A worktree's .git is a file pointing back to the checkout it came from.
        "worktree": Path::new(&root).join(".git").is_file(),
    })
}

/// " 3 files changed, 120 insertions(+), 40 deletions(-)" -> (3, 120, 40).
fn shortstat(text: &str) -> (u64, u64, u64) {
    let mut out = (0, 0, 0);
    for part in text.split(',') {
        let mut words = part.split_whitespace();
        let (Some(n), Some(what)) = (words.next().and_then(|n| n.parse().ok()), words.next()) else { continue };
        match what {
            w if w.starts_with("file") => out.0 = n,
            w if w.starts_with("insertion") => out.1 = n,
            w if w.starts_with("deletion") => out.2 = n,
            _ => {}
        }
    }
    out
}

/// Every checkout under `roots` (each "dir" or "dir:depth"), described.
pub fn scan(roots: &[String]) -> Vec<Value> {
    let mut found = Vec::new();
    let mut seen_roots = Vec::new();
    for root in roots {
        let (dir, depth) = match root.rsplit_once(':') {
            Some((d, n)) if n.chars().all(|c| c.is_ascii_digit()) => (d, n.parse().unwrap_or(3)),
            _ => (root.as_str(), 3),
        };
        // ~/developer and ~/Developer are one folder on a case-insensitive disk.
        let Ok(dir) = std::fs::canonicalize(expand(dir)) else { continue };
        if dir.is_dir() && !seen_roots.contains(&dir) {
            seen_roots.push(dir.clone());
            walk(&dir, depth, &mut found);
        }
    }
    // git is quick per checkout, but there can be many: a few at a time.
    let mut out = Vec::with_capacity(found.len());
    for chunk in found.chunks(8) {
        let handles: Vec<_> = chunk.iter().cloned().map(|d| thread::spawn(move || describe(&d))).collect();
        out.extend(handles.into_iter().filter_map(|h| h.join().ok()));
    }
    out
}

/// fetch, pull (fast-forward only) or push in a checkout.
pub fn act(path: &str, action: &str) -> Result<String, String> {
    let dir = expand(path);
    if !dir.join(".git").exists() {
        return Err(format!("{} isn't a git checkout", dir.display()));
    }
    let args: &[&str] = match action {
        "fetch" => &["fetch", "--prune"],
        "pull" => &["pull", "--ff-only"],
        "push" => &["push"],
        other => return Err(format!("unknown git action \"{other}\"")),
    };
    let out = run_with_timeout(Command::new("git").arg("-C").arg(&dir).args(args).env("GIT_TERMINAL_PROMPT", "0"), Duration::from_secs(120))?;
    let text = format!("{}{}", out.1, out.2).trim().to_string();
    if out.0 { Ok(text) } else { Err(if text.is_empty() { format!("git {action} failed") } else { text }) }
}

fn run_with_timeout(cmd: &mut Command, limit: Duration) -> Result<(bool, String, String), String> {
    let child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let started = Instant::now();
    let id = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    loop {
        if let Ok(out) = rx.try_recv() {
            let out = out.map_err(|e| e.to_string())?;
            return Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()));
        }
        if started.elapsed() > limit {
            crate::sys::kill_one(id, false);
            return Err(format!("timed out after {}s", limit.as_secs()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The GitHub CLI in those PATH folders (gh.exe on Windows), else where its
/// Windows installer puts it: a PATH read before it was installed (a
/// long-running agent's) doesn't have it yet.
fn find_gh(path_dirs: &[String]) -> Option<PathBuf> {
    let name = if cfg!(windows) { "gh.exe" } else { "gh" };
    let mut places: Vec<PathBuf> = path_dirs.iter().map(|d| Path::new(d).join(name)).collect();
    if cfg!(windows) {
        let programs = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        places.push(programs.join("GitHub CLI").join("gh.exe"));
    }
    places.into_iter().find(|p| p.is_file())
}

/// Whether the GitHub CLI is here and signed in (for agents that use it).
pub fn gh_status(path_dirs: &[String]) -> Value {
    let Some(gh) = find_gh(path_dirs) else { return json!({ "installed": false }) };
    let out = run_with_timeout(Command::new(&gh).args(["auth", "status", "--hostname", "github.com"]), Duration::from_secs(8));
    let (ok, stdout, stderr) = out.unwrap_or((false, String::new(), String::new()));
    let text = format!("{stdout}{stderr}");
    // "Logged in to github.com account octocat" (newer) or "... as octocat" (older).
    let account = text.lines().find_map(|l| {
        let l = l.trim().trim_start_matches('✓').trim();
        let rest = l.strip_prefix("Logged in to github.com account ").or_else(|| l.strip_prefix("Logged in to github.com as "))?;
        rest.split_whitespace().next().map(str::to_string)
    });
    json!({ "installed": true, "path": gh.display().to_string(), "logged_in": ok && account.is_some(), "account": account })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_github_cli_on_the_path() {
        let tmp = std::env::temp_dir().join(format!("muxen-gh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("bin")).unwrap();
        // On Windows the program is gh.exe; a plain "gh" there isn't it.
        let name = if cfg!(windows) { "gh.exe" } else { "gh" };
        std::fs::write(tmp.join("bin").join(name), b"").unwrap();
        let dirs = [tmp.join("empty").display().to_string(), tmp.join("bin").display().to_string()];
        assert_eq!(find_gh(&dirs), Some(tmp.join("bin").join(name)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reads_shortstat() {
        assert_eq!(shortstat(" 3 files changed, 120 insertions(+), 40 deletions(-)\n"), (3, 120, 40));
        assert_eq!(shortstat(" 1 file changed, 1 insertion(+)\n"), (1, 1, 0));
        assert_eq!(shortstat(" 2 files changed, 7 deletions(-)\n"), (2, 0, 7));
        assert_eq!(shortstat(""), (0, 0, 0));
    }

    /// A checkout whose config names an fsmonitor command: plain `git status`
    /// runs it, the scan's doesn't.
    #[cfg(unix)]
    #[test]
    fn looking_at_a_checkout_runs_nothing_from_its_config() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!("muxen-fsmonitor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let marker = tmp.join("ran");
        let hook = tmp.join("fsmonitor.sh");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap().status.success());
        run(&["init", "-q"]);
        run(&["config", "core.fsmonitor", hook.to_str().unwrap()]);

        assert!(git(&repo, &["status", "--porcelain=v2", "--branch"]).is_some());
        assert!(!marker.exists(), "the scan ran the checkout's fsmonitor command");
        // Plain git does run it, so the check above means something.
        let _ = Command::new("git").arg("-C").arg(&repo).arg("status").output();
        assert!(marker.exists(), "git didn't run core.fsmonitor here; the test proves nothing");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A clone of a repository with two commits, then a branch with work on it.
    #[test]
    fn info_counts_what_a_branch_changed_since_the_default_branch() {
        let tmp = std::env::temp_dir().join(format!("muxen-repos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (origin, work) = (tmp.join("origin"), tmp.join("work"));
        std::fs::create_dir_all(&origin).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
            assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        run(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("a.txt"), "one\ntwo\n").unwrap();
        run(&origin, &["add", "."]);
        run(&origin, &["commit", "-q", "-m", "first"]);
        run(&tmp, &["clone", "-q", origin.to_str().unwrap(), "work"]);
        run(&work, &["checkout", "-q", "-b", "fix"]);
        std::fs::write(work.join("a.txt"), "one\n2\nthree\n").unwrap();
        run(&work, &["commit", "-q", "-am", "change"]);
        std::fs::write(work.join("new.txt"), "x\n").unwrap();

        let v = info(work.to_str().unwrap());
        assert_eq!(v["git"], true);
        assert_eq!(v["branch"], "fix");
        assert_eq!(v["base"], "origin/main");
        assert_eq!((v["commits"].as_u64(), v["added"].as_u64(), v["removed"].as_u64()), (Some(1), Some(2), Some(1)));
        // a.txt changed, new.txt is new.
        assert_eq!((v["files"].as_u64(), v["untracked"].as_u64(), v["dirty"].as_u64()), (Some(2), Some(1), Some(1)));
        assert_eq!(v["worktree"], false);

        run(&work, &["worktree", "add", "-q", "-b", "other", tmp.join("wt").to_str().unwrap(), "origin/main"]);
        let wt = info(tmp.join("wt").to_str().unwrap());
        assert_eq!((wt["branch"].as_str(), wt["worktree"].as_bool(), wt["files"].as_u64()), (Some("other"), Some(true), Some(0)));

        assert_eq!(info(tmp.to_str().unwrap())["git"], false);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
