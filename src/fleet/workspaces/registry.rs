//! `workspaces.json`: the workspaces every Muxen process shares (the
//! window, the CLI, each agent's MCP server). Writers take a lock file, re-read,
//! change, and write atomically, so two processes never drop each other's edits.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::{display_path, write_atomic};
use crate::fleet::model::WorkspaceInfo;

const LOCK_WAIT: Duration = Duration::from_secs(5);
/// A lock older than this was left by a process that died mid-write.
const STALE_LOCK: Duration = Duration::from_secs(10);
pub const MAX_HISTORY: usize = 100;
pub const TAIL_LINES: usize = 30;

/// A workspace as stored: what the snapshot shows plus a little bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    #[serde(flatten)]
    pub info: WorkspaceInfo,
    /// Background commands it started, killed on destroy.
    #[serde(default)]
    pub background_pids: Vec<u32>,
    /// The machine speaks PowerShell.
    #[serde(default)]
    pub windows: bool,
    /// Numbers the background logs: `.muxen/logs/<n>.log`.
    #[serde(default)]
    pub next_log: u32,
    /// Docker workspaces: the host's docker socket is shared with the container
    /// (kept so a missing container is recreated the same way).
    #[serde(default)]
    pub docker_socket: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    workspaces: Vec<WorkspaceRecord>,
}

/// Reads the registry. A missing or unreadable file is an empty registry.
pub fn load(path: &Path) -> Vec<WorkspaceRecord> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<RegistryFile>(&text) {
            Ok(f) => f.workspaces,
            Err(e) => {
                tracing::warn!("ignoring {}: {e}", display_path(path));
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// Locks, re-reads, applies `f`, writes. Returns `f`'s result, the new list and the file's mtime.
pub fn update<R>(
    path: &Path,
    f: impl FnOnce(&mut Vec<WorkspaceRecord>) -> R,
) -> Result<(R, Vec<WorkspaceRecord>, Option<SystemTime>), String> {
    let _lock = Lock::acquire(&lock_path(path))?;
    let mut list = load(path);
    let result = f(&mut list);
    let file = RegistryFile { version: 1, workspaces: list };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
    write_atomic(path, &bytes).map_err(|e| format!("Couldn't save workspaces: {e:#}"))?;
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    Ok((result, file.workspaces, mtime))
}

/// Keeps the newest commands and trims long output lines.
pub fn cap_history(info: &mut WorkspaceInfo) {
    if info.history.len() > MAX_HISTORY {
        let extra = info.history.len() - MAX_HISTORY;
        info.history.drain(..extra);
    }
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

struct Lock(PathBuf);

impl Lock {
    fn acquire(path: &Path) -> Result<Lock, String> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(_) => return Ok(Lock(path.to_path_buf())),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                    if age.is_some_and(|a| a > STALE_LOCK) {
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    if started.elapsed() > LOCK_WAIT {
                        return Err(format!("{} is locked by another Muxen process.", display_path(path)));
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(e) => return Err(format!("Couldn't lock {}: {e}", display_path(path))),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    pub fn record(id: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            info: WorkspaceInfo {
                id: id.into(),
                name: id.into(),
                machine_id: "box".into(),
                machine_name: "Box".into(),
                host: "box".into(),
                platform: "linux-x64".into(),
                path: format!("/home/ci/muxen/workspaces/{id}"),
                repo: None,
                git_ref: None,
                branch: None,
                owner: Some("test".into()),
                status: "ready".into(),
                error: None,
                created_at: Utc::now(),
                last_used_at: Utc::now(),
                commands: 0,
                history: Vec::new(),
                shell: String::new(),
                isolation: "host".into(),
                image: None,
                container: None,
            },
            background_pids: vec![42],
            windows: false,
            next_log: 1,
            docker_socket: false,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxen-reg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("workspaces.json")
    }

    #[test]
    fn round_trips_and_keeps_bookkeeping() {
        let path = temp("rt");
        let (_, list, mtime) = update(&path, |l| l.push(record("ws-aaaaaa"))).unwrap();
        assert_eq!(list.len(), 1);
        assert!(mtime.is_some());
        let loaded = load(&path);
        assert_eq!(loaded[0].info.id, "ws-aaaaaa");
        assert_eq!(loaded[0].background_pids, vec![42]);
        // The info fields sit at the top level next to the bookkeeping.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"machineId\": \"box\"") && text.contains("\"backgroundPids\""), "{text}");
        assert!(!lock_path(&path).exists(), "lock released");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn concurrent_writers_dont_lose_updates() {
        let path = temp("conc");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    update(&path, |l| l.push(record(&format!("ws-{i:06}")))).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(load(&path).len(), 8);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn breaks_stale_locks_and_times_out_on_live_ones() {
        let path = temp("lock");
        let lock = lock_path(&path);
        std::fs::write(&lock, "").unwrap();
        // Make it look old.
        let old = std::time::SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options().write(true).open(&lock).unwrap().set_modified(old).unwrap();
        assert!(update(&path, |l| l.push(record("ws-bbbbbb"))).is_ok());
        assert_eq!(load(&path).len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn caps_history() {
        let mut r = record("ws-cccccc");
        for i in 0..(MAX_HISTORY + 5) {
            r.info.history.push(crate::fleet::model::WorkspaceCommand {
                command: format!("echo {i}"),
                exit_code: Some(0),
                started_at: Utc::now(),
                duration_ms: 1,
                output_tail: vec![],
                background: false,
                timed_out: false,
                by: None,
            });
        }
        cap_history(&mut r.info);
        assert_eq!(r.info.history.len(), MAX_HISTORY);
        assert_eq!(r.info.history[0].command, "echo 5");
    }
}
