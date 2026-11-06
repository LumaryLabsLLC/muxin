//! Sessions saved to disk as they go, so a session server that stops (a
//! crash, a reboot, a restart of the machine) brings them back when it starts
//! again: the list, titles, folders and recent scrollback, with the programs
//! started fresh (daemon.rs `restore`).
//!
//! One folder per session, `~/.muxen/sessions/<name>/`:
//! - `session.json`: what it is (name, agent, title, folder, command, size,
//!   whether it had exited), written when that changes;
//! - `screen`: its recent scrollback and screen as escape sequences, written
//!   now and then while it prints.
//!
//! Each file is written whole to a temporary name and renamed into place, so
//! a process that dies halfway leaves the last complete copy.
//!
//! A running session server goes on running the build it started with after
//! the agent is updated, so builds older and newer than this one read these
//! files: fields are only ever added, never renamed or given a new meaning,
//! and readers ignore the ones they don't know. `v` says which build's set it
//! is (1: the first).

use std::path::PathBuf;

use serde_json::Value;

/// Which set of fields `session.json` has (see above).
pub const VERSION: i64 = 1;

/// Where sessions are saved: `~/.muxen/sessions` (another folder in tests).
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Store {
        Store { root }
    }

    /// The session server's own: `~/.muxen/sessions`.
    pub fn home() -> Store {
        Store::new(crate::home_dir().join("sessions"))
    }

    /// A session's folder (the server refuses names with slashes).
    fn folder(&self, name: &str) -> Option<PathBuf> {
        let ok = !name.is_empty() && name.len() <= 200 && !name.starts_with('.') && !name.contains(['/', '\\', '\0']);
        ok.then(|| self.root.join(name))
    }

    /// Saves what a session is (`session.json`).
    pub fn write_meta(&self, name: &str, meta: &Value) -> std::io::Result<()> {
        self.write(name, "session.json", meta.to_string().as_bytes())
    }

    /// Saves a session's scrollback and screen (`screen`).
    pub fn write_screen(&self, name: &str, screen: &[u8]) -> std::io::Result<()> {
        self.write(name, "screen", screen)
    }

    fn write(&self, name: &str, file: &str, bytes: &[u8]) -> std::io::Result<()> {
        let dir = self.folder(name).ok_or_else(|| std::io::Error::other("not a session name"))?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(file);
        let tmp = dir.join(format!(".{file}.{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)
    }

    /// A session's saved scrollback and screen (empty when there's none).
    pub fn screen(&self, name: &str) -> Vec<u8> {
        self.folder(name).and_then(|d| std::fs::read(d.join("screen")).ok()).unwrap_or_default()
    }

    /// Forgets a session: it ended.
    pub fn remove(&self, name: &str) {
        if let Some(dir) = self.folder(name) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Every saved session's `session.json`, oldest first. A folder without a
    /// readable one (a session that died before its first save, a damaged
    /// file) is dropped.
    pub fn all(&self) -> Vec<Value> {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return Vec::new() };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !e.path().is_dir() {
                continue;
            }
            match std::fs::read_to_string(e.path().join("session.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(|m| usable(m, &name)) {
                Some(meta) => out.push(meta),
                None => self.remove(&name),
            }
        }
        out.sort_by_key(|m| m["created"].as_i64().unwrap_or(0));
        out
    }

    /// The names of the saved sessions, without reading them.
    pub fn names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return Vec::new() };
        entries.flatten().filter(|e| e.path().join("session.json").is_file()).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| !n.starts_with('.')).collect()
    }
}

/// Whether a saved `session.json` can bring its session back: it's this
/// folder's, and says where it ran.
fn usable(meta: &Value, folder: &str) -> bool {
    meta.is_object() && meta["name"].as_str() == Some(folder) && meta["cwd"].as_str().is_some_and(|c| !c.is_empty())
}

/// An empty store of its own, for a test.
#[cfg(test)]
pub fn scratch(tag: &str) -> Store {
    let root = std::env::temp_dir().join(format!("muxen-saved-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    Store::new(root)
}

#[cfg(test)]
impl Store {
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn saves_and_reads_back_and_leaves_nothing_half_written() {
        let store = scratch("roundtrip");
        let meta = json!({ "v": VERSION, "name": "claude-3f9a", "agent": "claude", "title": "Claude Code · api", "cwd": "/srv/api", "run": "claude 'fix it'", "created": 100, "cols": 120, "rows": 40 });
        store.write_meta("claude-3f9a", &meta).unwrap();
        store.write_screen("claude-3f9a", b"\x1b[1mhello\x1b[0m").unwrap();
        assert_eq!(store.all(), vec![meta]);
        assert_eq!(store.screen("claude-3f9a"), b"\x1b[1mhello\x1b[0m");
        assert_eq!(store.names(), vec!["claude-3f9a".to_string()]);
        let files: Vec<String> = std::fs::read_dir(store.root.join("claude-3f9a")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert!(files.iter().all(|f| !f.starts_with('.')), "{files:?}");
        store.remove("claude-3f9a");
        assert!(store.all().is_empty());
        assert!(store.screen("claude-3f9a").is_empty());
    }

    #[test]
    fn another_builds_files_are_read_and_damaged_ones_dropped() {
        let store = scratch("compat");
        // A newer build's, with fields this one doesn't know: read, the rest ignored.
        store.write_meta("codex-1", &json!({ "v": 9, "name": "codex-1", "cwd": "/srv", "created": 2, "later": { "x": 1 } })).unwrap();
        // The first build's fields only, and no screen yet.
        store.write_meta("shell-1", &json!({ "v": 1, "name": "shell-1", "cwd": "~", "created": 1 })).unwrap();
        // Damaged, another folder's, no folder: gone.
        std::fs::create_dir_all(store.root.join("bad")).unwrap();
        std::fs::write(store.root.join("bad/session.json"), "{ half").unwrap();
        store.write_meta("moved", &json!({ "name": "elsewhere", "cwd": "/srv" })).unwrap();
        store.write_meta("nowhere", &json!({ "name": "nowhere" })).unwrap();
        let names: Vec<String> = store.all().iter().map(|m| m["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["shell-1", "codex-1"]);
        assert!(!store.root.join("bad").exists() && !store.root.join("moved").exists());
        assert!(store.write_meta("../escape", &json!({})).is_err());
        assert!(store.write_meta(".hidden", &json!({})).is_err());
        let _ = std::fs::remove_dir_all(&store.root);
    }
}
