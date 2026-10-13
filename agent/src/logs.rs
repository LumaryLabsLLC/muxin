//! Logs on this machine, for the app's log viewer: where they are
//! (`sources`), following one on a link channel (`command`, run by link.rs),
//! and reading its last lines (`read`, for agents and the CLI).
//!
//! A source is `{"kind", "target"}`:
//! - `system`: the systemd journal (Linux) or the unified log (a Mac; a
//!   target there narrows it to one process);
//! - `kernel`: the kernel's messages (Linux);
//! - `service`: one systemd unit (`"user": true` for the user's own);
//! - `file`: a log file, followed like `tail -F`;
//! - `docker`: a container's output.
//!
//! Everything runs as the user, so it sees what the user may read.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

/// Lines shown before following (and at most for `read`).
const DEFAULT_LINES: u64 = 500;
const MAX_LINES: u64 = 5000;

/// `{"op": "log_sources"}`: what there is to read here.
pub fn sources() -> Value {
    if cfg!(windows) {
        let (docker, docker_note) = containers();
        return json!({
            "ok": true,
            "system": crate::win::event_logs(),
            "system_note": Value::Null,
            "files": files(),
            "docker": docker,
            "docker_note": docker_note,
            "services": crate::win::service_sources(),
            "services_note": "A service's events from Windows' System and Application logs.",
        });
    }
    let mac = cfg!(target_os = "macos");
    let journal = !mac && which("journalctl");
    let mut system = Vec::new();
    if mac {
        system.push(json!({ "kind": "system", "name": "Everything", "detail": "The system log" }));
    } else if journal {
        system.push(json!({ "kind": "system", "name": "Everything", "detail": "The systemd journal" }));
        system.push(json!({ "kind": "kernel", "name": "Kernel", "detail": "The kernel's messages" }));
    }
    let (docker, docker_note) = containers();
    json!({
        "ok": true,
        "system": system,
        // Without these groups the journal shows only the user's own logs.
        "system_note": (journal && !journal_groups()).then_some("Only your own logs: add yourself to the systemd-journal group to see the whole system."),
        "files": files(),
        "docker": docker,
        "docker_note": docker_note,
        "services": if journal { units() } else { Vec::new() },
        "services_note": mac.then_some("On a Mac, services write to the system log: open Everything and filter it by the service's name."),
    })
}

fn which(program: &str) -> bool {
    crate::root::tool(program).is_some()
}

/// Output of a short command, if it worked within a few seconds.
pub(crate) fn quick(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = out.read_to_string(&mut text);
        text
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then(|| reader.join().unwrap_or_default()),
            Ok(None) if started.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn journal_groups() -> bool {
    let root = crate::sys::is_root();
    root || quick("id", &["-Gn"]).is_some_and(|g| g.split_whitespace().any(|g| ["systemd-journal", "adm", "wheel", "sudo"].contains(&g)))
}

/// The services systemd has loaded, the system's and the user's own, with
/// their state ("running", "failed", "exited", "dead"): failed ones first
/// (their logs say why), then running ones.
fn units() -> Vec<Value> {
    let mut out = Vec::new();
    for user in [false, true] {
        let mut args = vec!["list-units", "--type=service", "--all", "--no-legend", "--plain", "--no-pager"];
        if user {
            args.insert(0, "--user");
        }
        let Some(text) = quick("systemctl", &args) else { continue };
        for line in text.lines() {
            // UNIT LOAD ACTIVE SUB DESCRIPTION...
            let parts: Vec<&str> = line.split_whitespace().collect();
            let [unit, load, active, sub, ..] = parts[..] else { continue };
            if load != "loaded" {
                continue;
            }
            let state = if active == "failed" { "failed" } else { sub };
            let name = unit.trim_end_matches(".service");
            out.push(json!({ "kind": "service", "target": unit, "name": name, "detail": parts[4..].join(" "), "user": user, "state": state }));
        }
    }
    out.sort_by_key(|u| match u["state"].as_str() {
        Some("failed") => 0,
        Some("running") => 1,
        _ => 2,
    });
    out.truncate(400);
    out
}

/// Containers, running ones first; or why there are none to show.
fn containers() -> (Vec<Value>, Option<String>) {
    if !which("docker") {
        return (Vec::new(), None);
    }
    let docker = crate::root::tool("docker").unwrap_or_else(|| "docker".into());
    let Some(text) = quick(&docker, &["ps", "-a", "--format", "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}\t{{.Label \"com.docker.compose.project\"}}"]) else {
        return (Vec::new(), Some("Docker didn't answer: is it running, and may this user use it (the docker group)?".into()));
    };
    let mut list: Vec<Value> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let [id, name, image, state, status, project] = f[..] else { return None };
            let project = (!project.is_empty()).then_some(project);
            Some(json!({ "kind": "docker", "target": id, "name": name, "detail": format!("{image} · {status}"), "running": state == "running", "project": project }))
        })
        .collect();
    list.sort_by_key(|c| c["running"] != true);
    // Each Compose project's containers together, in one log.
    let mut projects: Vec<(String, usize, usize)> = Vec::new();
    for c in &list {
        if let Some(p) = c["project"].as_str() {
            match projects.iter_mut().find(|(name, ..)| name == p) {
                Some((_, n, up)) => {
                    *n += 1;
                    *up += usize::from(c["running"] == true);
                }
                None => projects.push((p.to_string(), 1, usize::from(c["running"] == true))),
            }
        }
    }
    projects.sort();
    let mut out: Vec<Value> = projects
        .into_iter()
        .map(|(name, n, up)| json!({ "kind": "compose", "target": name, "name": name, "detail": format!("Compose · {up} of {n} running"), "running": up > 0, "project": name }))
        .collect();
    out.extend(list);
    (out, None)
}

/// Log files worth offering: the usual places, and ones written lately under
/// the home folder (an app's storage/logs, a runner's _diag), newest first.
fn files() -> Vec<Value> {
    let home = crate::sys::user_home();
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    // A big home folder takes long to look through: a couple of seconds at most.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut add_dir = |dir: &std::path::Path, depth: usize, recent_only: bool| {
        walk(dir, depth, deadline, &mut |p| {
            let log = p.extension().is_some_and(|e| e == "log") || p.file_name().is_some_and(|n| ["syslog", "messages", "system.log"].contains(&n.to_string_lossy().as_ref()));
            let recent = std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age < Duration::from_secs(14 * 86400));
            if log && (!recent_only || recent) && !found.contains(&p.to_path_buf()) {
                found.push(p.to_path_buf());
            }
        });
    };
    if cfg!(windows) {
        add_dir(&std::path::PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into())), 3, true);
    } else {
        add_dir(std::path::Path::new("/var/log"), 2, false);
    }
    add_dir(&home.join(".muxen"), 1, false);
    if cfg!(target_os = "macos") {
        add_dir(&home.join("Library/Logs"), 3, true);
        add_dir(std::path::Path::new("/Library/Logs"), 2, true);
    }
    add_dir(&home, 4, true);
    let mut files: Vec<(SystemTime, Value)> = found
        .into_iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            let modified = meta.modified().ok()?;
            let readable = std::fs::File::open(&p).is_ok();
            let shown = p.strip_prefix(&home).map(|r| format!("~{}{}", std::path::MAIN_SEPARATOR, r.display())).unwrap_or_else(|_| p.display().to_string());
            let name = p.file_name()?.to_string_lossy().into_owned();
            Some((modified, json!({ "kind": "file", "target": p.display().to_string(), "name": name, "detail": shown, "size": meta.len(), "modified": modified.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0), "readable": readable })))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(80).map(|(_, v)| v).collect()
}

/// Calls `found` for the files under `dir`, `depth` folders deep (until
/// `deadline`), skipping the big trees nobody keeps logs in.
fn walk(dir: &std::path::Path, depth: usize, deadline: Instant, found: &mut dyn FnMut(&std::path::Path)) {
    const SKIP: [&str; 16] = [
        "node_modules", ".git", ".cache", ".cargo", ".rustup", "target", ".npm", "Library", "snap", "Pictures", "Movies", "Music", "Applications", "vendor", "dist", ".venv",
    ];
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if Instant::now() > deadline {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_file() {
            found(&path);
        } else if kind.is_dir() && depth > 0 {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Bundles (.app, .photoslibrary, ...) are folders, but not ones with logs.
            let bundle = name.contains('.') && !name.starts_with('.') && path.join("Contents").is_dir();
            if !SKIP.contains(&name.as_ref()) && !bundle && (!name.starts_with('.') || name == ".muxen") {
                walk(&path, depth - 1, deadline, found);
            }
        }
    }
}

/// The program and arguments that show a source: its last `lines`, then
/// what comes (when `follow`).
pub fn command(source: &Value, lines: u64, follow: bool) -> Result<(String, Vec<String>), String> {
    let lines = lines.clamp(1, MAX_LINES).to_string();
    let target = source["target"].as_str().unwrap_or("").to_string();
    let s = |v: &str| v.to_string();
    let mac = cfg!(target_os = "macos");
    if cfg!(windows) && matches!(source["kind"].as_str(), Some("system" | "kernel" | "service" | "file")) {
        let script = crate::win::log_script(source["kind"].as_str().unwrap_or(""), &target, lines.parse().unwrap_or(500), follow)?;
        return Ok(crate::win::log_command(script));
    }
    let journal = |extra: Vec<String>| {
        let mut args = vec![s("--no-pager"), s("-o"), s("short-iso"), s("-n"), lines.clone()];
        args.extend(extra);
        if follow {
            args.push(s("-f"));
        }
        Ok((s("journalctl"), args))
    };
    match source["kind"].as_str().unwrap_or("") {
        "system" if mac => {
            // Recent history, then live. A target narrows it to one process.
            if !target.is_empty() && !target.chars().all(|c| c.is_ascii_alphanumeric() || "._- ".contains(c)) {
                return Err("That process name has odd characters.".into());
            }
            let predicate = if target.is_empty() { String::new() } else { format!(" --predicate 'process == \"{target}\"'") };
            let mut script = format!("log show --last 10m --style compact{predicate} 2>/dev/null | tail -n {lines}");
            if follow {
                script.push_str(&format!("; exec log stream --style compact{predicate}"));
            }
            Ok((s("sh"), vec![s("-c"), script]))
        }
        "system" => journal(Vec::new()),
        "kernel" => journal(vec![s("-k")]),
        "service" => {
            if target.is_empty() || target.starts_with('-') {
                return Err("Which service?".into());
            }
            let mut extra = vec![s("-u"), target];
            if source["user"].as_bool() == Some(true) {
                extra.insert(0, s("--user"));
            }
            journal(extra)
        }
        "file" => {
            if target.is_empty() {
                return Err("Which file?".into());
            }
            let path = expand_home(&target);
            std::fs::File::open(&path).map_err(|e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => format!("{target} is readable only by root (or its group)."),
                std::io::ErrorKind::NotFound => format!("There's no {target} on this machine."),
                _ => format!("Couldn't open {target}: {e}"),
            })?;
            let mut args = vec![s("-n"), lines];
            if follow {
                args.push(s("-F"));
            }
            args.push(s("--"));
            args.push(path);
            Ok((s("tail"), args))
        }
        "docker" => {
            if target.is_empty() || target.starts_with('-') {
                return Err("Which container?".into());
            }
            let mut args = vec![s("logs"), s("--timestamps"), s("--tail"), lines];
            if follow {
                args.push(s("-f"));
            }
            args.push(target);
            Ok((crate::root::tool("docker").unwrap_or_else(|| s("docker")), args))
        }
        "compose" => {
            if target.is_empty() || !target.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
                return Err("Which Compose project?".into());
            }
            let mut args = vec![s("compose"), s("-p"), target, s("logs"), s("--timestamps"), s("--tail"), lines];
            if follow {
                args.push(s("-f"));
            }
            Ok((crate::root::tool("docker").unwrap_or_else(|| s("docker")), args))
        }
        other => Err(format!("No log source called \"{other}\".")),
    }
}

fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => path.to_string(),
    }
}

/// `{"op": "logs_read", "source", "lines"?, "filter"?}`: a source's last
/// lines, once (lines holding `filter`, ignoring case, when given).
pub fn read(req: &Value) -> Value {
    let lines = req["lines"].as_u64().unwrap_or(200).clamp(1, MAX_LINES);
    let filter = req["filter"].as_str().unwrap_or("").to_lowercase();
    // Filtering reads more, so there are `lines` that match.
    let fetch = if filter.is_empty() { lines } else { MAX_LINES };
    let (program, args) = match command(&req["source"], fetch, false) {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let output = match Command::new(&program).args(&args).stdin(Stdio::null()).output() {
        Ok(o) => o,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't run {program}: {e}") }),
    };
    // Both outputs: docker writes a container's stderr to its own.
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let err = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && text.trim().is_empty() {
        return json!({ "ok": false, "error": err.lines().last().unwrap_or("It didn't work.").trim() });
    }
    text.push_str(&err);
    let mut kept: Vec<&str> = text.lines().filter(|l| filter.is_empty() || l.to_lowercase().contains(&filter)).collect();
    if req["source"]["kind"] == "compose" {
        kept = by_time(kept);
    }
    let from = kept.len().saturating_sub(lines as usize);
    json!({ "ok": true, "lines": kept[from..] })
}

/// Compose's lines ("web-1  | 2026-10-12T16:11:51.427785108Z ...") in time
/// order: it gives each container's last lines one container after another.
/// A line without a time stays after the line before it.
pub(crate) fn by_time<S: AsRef<str>>(lines: Vec<S>) -> Vec<S> {
    let mut last = String::new();
    let mut keyed: Vec<(String, S)> = lines
        .into_iter()
        .map(|l| {
            if let Some(t) = stamp(l.as_ref()) {
                last = t;
            }
            (last.clone(), l)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, l)| l).collect()
}

/// A Compose line's time, written so times sort as text (nine places of
/// fraction, however many Docker gave).
fn stamp(line: &str) -> Option<String> {
    let (_, rest) = line.split_once('|')?;
    let word = rest.trim_start().split_whitespace().next()?;
    let b = word.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let (secs, after) = word.split_at(19);
    let frac: String = after.strip_prefix('.').map(|f| f.chars().take_while(char::is_ascii_digit).collect()).unwrap_or_default();
    Some(format!("{secs}.{frac:0<9}"))
}

/// How many lines a stream starts with.
pub fn initial_lines(req: &Value) -> u64 {
    req["lines"].as_u64().unwrap_or(DEFAULT_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn puts_compose_lines_in_time_order() {
        let lines = vec![
            "cache-1  | 2026-10-12T16:11:51.4277Z ready",
            "cache-1  | 2026-10-12T16:20:12.587451332Z saving",
            "web-1    | 2026-10-12T16:11:56.612834458Z starting",
            "web-1    |   at line 2",
            "api-1    | 2026-10-12T16:11:51.42778Z up",
        ];
        assert_eq!(by_time(lines), vec![
            "cache-1  | 2026-10-12T16:11:51.4277Z ready",
            "api-1    | 2026-10-12T16:11:51.42778Z up",
            "web-1    | 2026-10-12T16:11:56.612834458Z starting",
            "web-1    |   at line 2",
            "cache-1  | 2026-10-12T16:20:12.587451332Z saving",
        ]);
        assert_eq!(stamp("no bar here"), None);
    }

    #[test]
    fn builds_commands() {
        let file = std::env::temp_dir().join("muxen-logs-test.log");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let path = file.display().to_string();
        let (p, a) = command(&json!({ "kind": "file", "target": path }), 50, true).unwrap();
        if cfg!(windows) {
            // Windows reads files, its event logs and services with PowerShell (win::log_script).
            assert_eq!(p, "powershell");
        } else {
            assert_eq!(p, "tail");
            assert_eq!(a, ["-n", "50", "-F", "--", path.as_str()]);
            let read = read(&json!({ "source": { "kind": "file", "target": path }, "lines": 1 }));
            assert_eq!(read["lines"], json!(["two"]));
            assert!(command(&json!({ "kind": "file", "target": "/nonexistent/x.log" }), 10, false).unwrap_err().contains("no /nonexistent"));
        }
        let (p, a) = command(&json!({ "kind": "docker", "target": "abc123" }), 10, false).unwrap();
        assert!(p.trim_end_matches(".exe").ends_with("docker"), "{p}");
        assert_eq!(a.join(" "), "logs --timestamps --tail 10 abc123");
        assert!(command(&json!({ "kind": "docker", "target": "--privileged" }), 10, false).is_err());
        assert!(command(&json!({ "kind": "nope" }), 10, false).is_err());
        if !cfg!(target_os = "macos") && !cfg!(windows) {
            let (p, a) = command(&json!({ "kind": "service", "target": "nginx.service", "user": true }), 20, true).unwrap();
            assert_eq!((p.as_str(), a.join(" ").as_str()), ("journalctl", "--no-pager -o short-iso -n 20 --user -u nginx.service -f"));
        }
    }
}
