//! Helper sessions: a session an agent started from inside another one (its
//! parent). Muxin's CLI and MCP server know they run in a session from
//! `MUXEN_SESSION` (the session server sets it), and say so when they start
//! one: `{"op": "start", ..., "parent": "codex-3f9a", "keep": false}`.
//!
//! Which session started which is kept here, `~/.muxen/helpers.json`
//! (`{"<helper>": {"parent": "<name>", "keep": false, "at": <unix>}}`), rather
//! than in the session server, which goes on running the build it started
//! with: so a session server of any version has helpers. A parent on another
//! machine is `<machine-id>/<name>` (as the app that started it calls it);
//! Muxin itself ends those.
//!
//! A helper ends with its parent unless it was started with `keep`: the
//! session server ends them when the parent is stopped or closes, and
//! `discover` ends any it missed (an older session server). Agents may start
//! helpers unless the user turned that off (Settings › Agents: `helpers`).

use serde_json::{Map, Value, json};

use crate::{home_dir, now};

fn path() -> std::path::PathBuf {
    home_dir().join("helpers.json")
}

/// Every helper here, by session name.
pub fn all() -> Map<String, Value> {
    std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn write(helpers: &Map<String, Value>) {
    if helpers.is_empty() {
        let _ = std::fs::remove_file(path());
        return;
    }
    let tmp = path().with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&tmp, Value::Object(helpers.clone()).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path());
    }
}

/// Changes the helpers kept here, one process at a time (several Muxins
/// look at a machine at once): `change` says whether it changed them.
fn change(change: impl FnOnce(&mut Map<String, Value>) -> bool) {
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home_dir().join("helpers.lock"));
    let _held = lock.as_ref().ok().filter(|f| f.lock().is_ok());
    let mut helpers = all();
    if change(&mut helpers) {
        write(&helpers);
    }
}

/// Notes that `name` was started from inside `parent`.
pub fn record(name: &str, parent: &str, keep: bool) {
    change(|h| {
        h.insert(name.to_string(), json!({ "parent": parent, "keep": keep, "at": now() }));
        true
    });
}

/// Forgets a helper (it ended, or never started).
pub fn forget(name: &str) {
    change(|h| h.remove(name).is_some());
}

/// A helper noted this recently may not be in a session list yet: it's
/// noted just before it starts, and a list taken a moment before is still
/// being looked at elsewhere.
const NEW: i64 = 60;

/// Forgets the helpers that are gone (`names`: the sessions there are).
pub fn prune(names: &[&str]) {
    change(|h| prune_in(h, names, now()));
}

fn prune_in(helpers: &mut Map<String, Value>, names: &[&str], now: i64) -> bool {
    let before = helpers.len();
    helpers.retain(|name, h| names.contains(&name.as_str()) || now - h["at"].as_i64().unwrap_or(0) < NEW);
    helpers.len() != before
}

/// A parent on this machine: a session's name, not `<machine-id>/<name>`.
fn here(parent: &str) -> bool {
    !parent.is_empty() && !parent.contains('/')
}

/// The helpers that end with `parent` (the ones started without `keep`).
pub fn ending_with(helpers: &Map<String, Value>, parent: &str) -> Vec<String> {
    helpers.iter().filter(|(_, h)| h["parent"].as_str() == Some(parent) && h["keep"].as_bool() != Some(true)).map(|(name, _)| name.clone()).collect()
}

/// Helpers whose parent on this machine is gone (`present`: the sessions
/// there are, saved ones too), and that end with it.
pub fn orphans(helpers: &Map<String, Value>, present: &[&str]) -> Vec<String> {
    helpers
        .iter()
        .filter(|(name, h)| {
            let parent = h["parent"].as_str().unwrap_or("");
            present.contains(&name.as_str()) && here(parent) && !present.contains(&parent) && h["keep"].as_bool() != Some(true)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Adds `parent` and `keep` to a session list's entries.
pub fn annotate(sessions: &mut Value, helpers: &Map<String, Value>) {
    for s in sessions.as_array_mut().into_iter().flatten() {
        let Some(h) = s["name"].as_str().and_then(|n| helpers.get(n)) else { continue };
        s["parent"] = h["parent"].clone();
        s["keep"] = json!(h["keep"].as_bool() == Some(true));
    }
}

/// Why a session can't be started from inside `parent`, if it can't: the
/// user turned helpers off (`choices`: settings.rs), or the parent on this
/// machine isn't one of its sessions (`sessions`, the server's list).
pub fn refused(parent: &str, choices: &Value, sessions: Option<&Value>) -> Option<String> {
    if !crate::settings::on(choices, "helpers", true) {
        return Some("Agents can't start other sessions: the user turned that off in Muxin (Settings › General › Agents).".into());
    }
    if here(parent) {
        let known = sessions.and_then(Value::as_array).is_some_and(|a| a.iter().any(|s| s["name"].as_str() == Some(parent)));
        if !known {
            return Some(format!("There's no session called {parent} on this machine to start a helper from."));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn helpers_end_with_their_parent_unless_kept() {
        let helpers = map(json!({
            "codex-0001": { "parent": "claude-aaaa", "keep": false },
            "codex-0002": { "parent": "claude-aaaa", "keep": true },
            "codex-0003": { "parent": "codex-0001" },
            "shell-0004": { "parent": "mac/claude-bbbb" },
        }));
        assert_eq!(ending_with(&helpers, "claude-aaaa"), ["codex-0001"]);
        assert_eq!(ending_with(&helpers, "codex-0001"), ["codex-0003"]);
        assert!(ending_with(&helpers, "nobody").is_empty());
        // The parent's gone: its helper goes (not the kept one, nor one whose parent is on another machine).
        let present = ["codex-0001", "codex-0002", "codex-0003", "shell-0004"];
        assert_eq!(orphans(&helpers, &present), ["codex-0001"]);
        // Still there: nothing goes. A helper that's gone itself isn't stopped again.
        assert!(orphans(&helpers, &["claude-aaaa", "codex-0001", "codex-0002"]).is_empty());
        // A helper's helper whose parent went: it goes too.
        assert_eq!(orphans(&helpers, &["codex-0003"]), ["codex-0003"]);
    }

    #[test]
    fn a_helper_just_noted_isnt_forgotten_by_a_list_from_before_it() {
        let mut helpers = map(json!({ "codex-new": { "parent": "claude-a", "at": 1000 }, "codex-old": { "parent": "claude-a", "at": 100 }, "codex-here": { "parent": "claude-a", "at": 100 } }));
        // A list from a moment before codex-new started: it stays; the old one that's gone goes.
        assert!(prune_in(&mut helpers, &["claude-a", "codex-here"], 1010));
        let mut kept: Vec<&String> = helpers.keys().collect();
        kept.sort();
        assert_eq!(kept, ["codex-here", "codex-new"]);
        // A minute on, and still not there: it never started.
        assert!(prune_in(&mut helpers, &["claude-a", "codex-here"], 1100));
        assert_eq!(helpers.keys().collect::<Vec<_>>(), ["codex-here"]);
        assert!(!prune_in(&mut helpers, &["codex-here"], 1100));
    }

    #[test]
    fn lists_say_which_session_started_each() {
        let helpers = map(json!({ "codex-0001": { "parent": "claude-aaaa", "keep": true, "at": 5 } }));
        let mut sessions = json!([{ "name": "claude-aaaa" }, { "name": "codex-0001" }]);
        annotate(&mut sessions, &helpers);
        assert!(sessions[0].get("parent").is_none());
        assert_eq!((sessions[1]["parent"].as_str(), sessions[1]["keep"].as_bool()), (Some("claude-aaaa"), Some(true)));
    }

    #[test]
    fn starting_one_is_refused_when_helpers_are_off_or_the_parent_isnt_here() {
        let list = json!([{ "name": "claude-aaaa" }]);
        assert_eq!(refused("claude-aaaa", &json!({}), Some(&list)), None);
        assert_eq!(refused("claude-aaaa", &json!({ "helpers": { "on": true, "at": 5 } }), Some(&list)), None);
        let off = refused("claude-aaaa", &json!({ "helpers": { "on": false, "at": 5 } }), Some(&list)).unwrap();
        assert!(off.contains("turned that off"), "{off}");
        assert!(refused("codex-9999", &json!({}), Some(&list)).unwrap().contains("no session called codex-9999"));
        // A parent on another machine: Muxin found it there.
        assert_eq!(refused("mac/claude-bbbb", &json!({}), None), None);
        assert!(refused("mac/claude-bbbb", &json!({ "helpers": { "on": false, "at": 5 } }), None).is_some());
    }
}
