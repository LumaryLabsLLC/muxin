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

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
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

/// One checkout's state.
fn describe(dir: &Path) -> Value {
    let status = git(dir, &["status", "--porcelain=v2", "--branch"]).unwrap_or_default();
    let mut branch = String::new();
    let mut upstream = None;
    let (mut ahead, mut behind, mut dirty) = (0i64, 0i64, 0u32);
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("# branch.head ") {
            branch = v.to_string();
        } else if let Some(v) = line.strip_prefix("# branch.upstream ") {
            upstream = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("# branch.ab ") {
            let mut parts = v.split(' ');
            ahead = parts.next().and_then(|a| a.trim_start_matches('+').parse().ok()).unwrap_or(0);
            behind = parts.next().and_then(|b| b.trim_start_matches('-').parse().ok()).unwrap_or(0);
        } else if !line.starts_with('#') && !line.is_empty() {
            dirty += 1;
        }
    }
    let remote = git(dir, &["config", "--get", "remote.origin.url"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let last = git(dir, &["log", "-1", "--format=%ct%x09%s"]).unwrap_or_default();
    let (when, subject) = last.trim().split_once('\t').unwrap_or(("", ""));
    json!({
        "path": dir.display().to_string(),
        "remote": remote,
        "branch": branch,
        "upstream": upstream,
        "ahead": ahead,
        "behind": behind,
        "dirty": dirty,
        "last_commit": when.parse::<i64>().ok(),
        "subject": subject,
    })
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

/// Whether the GitHub CLI is here and signed in (for agents that use it).
pub fn gh_status(path_dirs: &[String]) -> Value {
    let gh = path_dirs.iter().map(|d| Path::new(d).join("gh")).find(|p| p.is_file());
    let Some(gh) = gh else { return json!({ "installed": false }) };
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
