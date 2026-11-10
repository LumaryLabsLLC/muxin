//! Agent sessions that run in a desktop app rather than in Muxin's terminal:
//! Claude's desktop app (its Code tab), which runs Claude Code with
//! `CLAUDE_CODE_ENTRYPOINT=claude-desktop` and the app's own id for the
//! session in `CLAUDE_CODE_HOST_SESSION_ID`.
//!
//! Muxin's Claude hook reports for those too (states.rs keeps each as
//! `desktop-claude-<id>`), and `discover` lists them beside Muxin's own
//! sessions, with the title and folder from the app's file for the session.
//! The apps open them in Claude (`claude://code/continue?session=<id>`).
//!
//! The app's file also says when you last looked at the session there
//! (`lastFocusedAt`): a "done" you've seen in Claude is no longer news.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

/// The state files' names for these start with this.
pub const PREFIX: &str = "desktop-";
pub const CLAUDE_DESKTOP: &str = "claude-desktop";
/// A report this old is forgotten: the conversation was left long ago.
const FORGET_AFTER: i64 = 3 * 86400;
/// "Working" this long with no sign of it: it was stopped without saying so.
const WORKING_QUIET: i64 = 15 * 60;

/// The session the hook running now is for, when it runs in Claude's desktop
/// app: its state file's name and the app's id for it.
pub fn from_env(agent: &str) -> Option<(String, String)> {
    if agent != "claude" || std::env::var("CLAUDE_CODE_ENTRYPOINT").ok()? != CLAUDE_DESKTOP {
        return None;
    }
    let id = std::env::var("CLAUDE_CODE_HOST_SESSION_ID").ok()?;
    valid_id(&id).then(|| (name_for(&id), id))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn name_for(id: &str) -> String {
    format!("{PREFIX}claude-{}", id.strip_prefix("local_").unwrap_or(id))
}

/// What the report keeps besides the state: which app, its id for the
/// session, and the folder (from the hook).
pub fn annotate_report(report: &mut Value, app_session: &str, input: &Value) {
    report["app"] = json!(CLAUDE_DESKTOP);
    report["app_session"] = json!(app_session);
    if let Some(cwd) = input["cwd"].as_str().filter(|c| !c.is_empty()) {
        report["cwd"] = json!(cwd);
    }
}

/// The sessions to list, as the session server lists its own, from the
/// state reports (`states::all`). Forgets the ones that are over: archived
/// in the app, or left for days.
pub fn listed(reports: &Map<String, Value>, now: i64) -> Vec<Value> {
    // (Asked every moment by `link`'s watch: nothing to read without one.)
    if !reports.keys().any(|n| n.starts_with(PREFIX)) {
        return Vec::new();
    }
    let dirs = claude_sessions_dirs();
    let mut out = Vec::new();
    for (name, r) in reports.iter().filter(|(n, _)| n.starts_with(PREFIX)) {
        let Some(id) = r["app_session"].as_str().filter(|i| valid_id(i)) else { continue };
        let meta = claude_session_file(&dirs, id);
        if let Some(session) = listed_one(name, r, meta.as_ref(), now) {
            out.push(session);
        } else {
            crate::states::forget(name);
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s["created"].as_i64().unwrap_or(0)));
    out
}

/// One of them as the session server would list it; `None` once it's over.
fn listed_one(name: &str, r: &Value, meta: Option<&Value>, now: i64) -> Option<Value> {
    let at = r["at"].as_i64().unwrap_or(0);
    let ms = |key: &str| meta.and_then(|m| m[key].as_i64()).map(|t| t / 1000);
    let active = at.max(ms("lastActivityAt").unwrap_or(0));
    if meta.is_some_and(|m| m["isArchived"].as_bool() == Some(true)) || now - active > FORGET_AFTER {
        return None;
    }
    let mut state = r["state"].as_str().unwrap_or("idle");
    // Looked at in Claude since it finished: seen.
    if state == "done" && ms("lastFocusedAt").is_some_and(|seen| seen > at) {
        state = "idle";
    }
    if state == "working" && now - active > WORKING_QUIET {
        state = "idle";
    }
    let cwd = meta.and_then(|m| m["cwd"].as_str()).or(r["cwd"].as_str()).unwrap_or("");
    let folder = cwd.rsplit(['/', '\\']).find(|p| !p.is_empty()).unwrap_or("Claude");
    let title = meta.and_then(|m| m["title"].as_str()).filter(|t| !t.trim().is_empty()).unwrap_or(folder);
    Some(json!({
        "name": name,
        "agent": "claude",
        "app": CLAUDE_DESKTOP,
        "app_session": r["app_session"],
        "title": title,
        "cwd": cwd,
        "status": "running",
        "state": state,
        "state_at": at,
        "state_source": "hook",
        "conversation": r["conversation"],
        "created": ms("createdAt").unwrap_or(at),
        "last_output": active,
    }))
}

/// Where Claude's desktop app keeps a file per session:
/// `claude-code-sessions/<account>/<organization>/<id>.json` in its folder.
fn claude_sessions_dirs() -> Vec<PathBuf> {
    let home = crate::sys::user_home();
    let roots: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![home.join("Library/Application Support/Claude")]
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).into_iter().map(|d| d.join("Claude")).collect()
    } else {
        vec![home.join(".config/Claude")]
    };
    let mut out = Vec::new();
    for sessions in roots.into_iter().map(|r| r.join("claude-code-sessions")) {
        let accounts = std::fs::read_dir(&sessions).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir());
        for account in accounts {
            out.extend(std::fs::read_dir(&account).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    out
}

fn claude_session_file(dirs: &[PathBuf], id: &str) -> Option<Value> {
    dirs.iter().map(|d| d.join(format!("{id}.json"))).find(|p| p.is_file()).and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn report(state: &str, at: i64) -> Value {
        json!({ "state": state, "at": at, "source": "hook", "agent": "claude", "app": CLAUDE_DESKTOP, "app_session": "local_abc", "cwd": "/Users/me/code/api", "conversation": "conv-1" })
    }

    fn meta(title: &str, focused: i64, archived: bool) -> Value {
        json!({ "sessionId": "local_abc", "title": title, "cwd": "/Users/me/code/api", "isArchived": archived, "createdAt": (NOW - 3600) * 1000, "lastActivityAt": (NOW - 60) * 1000, "lastFocusedAt": focused * 1000 })
    }

    #[test]
    fn named_after_the_apps_id() {
        assert_eq!(name_for("local_4da60e52-8368"), "desktop-claude-4da60e52-8368");
        assert!(!valid_id("../x") && !valid_id("") && valid_id("local_4da6-x"));
    }

    #[test]
    fn listed_with_the_apps_title_and_folder() {
        let s = listed_one("desktop-claude-abc", &report("needs_you", NOW - 30), Some(&meta("Fix the login", NOW - 600, false)), NOW).unwrap();
        assert_eq!(s["title"], "Fix the login");
        assert_eq!(s["cwd"], "/Users/me/code/api");
        assert_eq!(s["state"], "needs_you");
        assert_eq!(s["app"], CLAUDE_DESKTOP);
        assert_eq!(s["app_session"], "local_abc");
        assert_eq!(s["created"], NOW - 3600);
        // No file from the app: the folder's name.
        assert_eq!(listed_one("desktop-claude-abc", &report("working", NOW - 30), None, NOW).unwrap()["title"], "api");
    }

    #[test]
    fn done_and_seen_in_claude_is_idle() {
        let unseen = listed_one("d", &report("done", NOW - 30), Some(&meta("t", NOW - 600, false)), NOW).unwrap();
        assert_eq!(unseen["state"], "done");
        let seen = listed_one("d", &report("done", NOW - 30), Some(&meta("t", NOW - 5, false)), NOW).unwrap();
        assert_eq!(seen["state"], "idle");
    }

    #[test]
    fn archived_or_long_left_is_over() {
        assert!(listed_one("d", &report("done", NOW - 30), Some(&meta("t", 0, true)), NOW).is_none());
        assert!(listed_one("d", &report("done", NOW - FORGET_AFTER - 10), None, NOW).is_none());
    }

    #[test]
    fn working_without_a_sign_for_long_is_idle() {
        let mut m = meta("t", 0, false);
        m["lastActivityAt"] = json!((NOW - WORKING_QUIET - 10) * 1000);
        assert_eq!(listed_one("d", &report("working", NOW - WORKING_QUIET - 10), Some(&m), NOW).unwrap()["state"], "idle");
    }
}
