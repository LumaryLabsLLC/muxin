//! Boxes: one task each, worked on by an agent CLI in a container of its own
//! on this machine. Muxin creates a box (its image, a checkout of the repo,
//! the container) and then hands it here: this keeps its record, starts its
//! agent in a session, pauses it, and deletes it.
//!
//! A box lives in `~/muxen/boxes/<id>/`: `box.json` (its record), `repo/`
//! (the checkout, at the same path inside the container) and `home/` (the
//! agent's home, /home/box, with this user's agent folders like ~/.claude and
//! ~/.codex mounted in it, so the agent is signed in as it is here). Its
//! session runs the agent inside with `docker exec`, by way of
//! `home/.muxin/start`, which Muxin wrote: the first time it starts the agent
//! on the task, after that it continues the agent's last conversation. Text
//! for the agent waits in `home/.muxin/next` until it starts.
//!
//! Requests name a box by `box_id` (a link's requests have an `id` of their own).
//!
//! Pausing stops the container (its disk stays, its processes end).
//! `muxen-agent box-watch` pauses running boxes whose agent has been quiet
//! for a while (an open terminal nobody types in doesn't count), or has
//! ended; it runs while any box does, so boxes pause whether or not Muxin is
//! open.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// How often the watcher looks at running boxes: a box whose agent quit
/// pauses within moments.
const WATCH_EVERY: Duration = Duration::from_secs(10);
/// A box that just started has a moment to bring up its session (it's
/// started before `resume` answers, so this only covers the overlap).
const GRACE_SECS: i64 = 10;
/// Quiet this long (no output, which typing makes too), a box pauses (its
/// own `idle_minutes` wins; 0 never pauses).
const IDLE_MINUTES: i64 = 15;
/// How long the agent CLI gets to start before text is typed into it (CLIs
/// that don't take their first prompt as an argument).
const TYPE_AFTER: Duration = Duration::from_secs(4);

fn dir() -> PathBuf {
    crate::sys::user_home().join("muxen").join("boxes")
}

/// "box-" and lowercase letters or digits.
fn valid_id(id: &str) -> bool {
    id.strip_prefix("box-").is_some_and(|r| !r.is_empty() && r.len() <= 24 && r.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
}

fn folder(id: &str) -> PathBuf {
    dir().join(id)
}

fn read(id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(folder(id).join("box.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn write(b: &Value) -> Result<(), String> {
    let id = b["id"].as_str().unwrap_or("");
    let path = folder(id).join("box.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(b).unwrap_or_default()).map_err(|e| format!("couldn't save it: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("couldn't save it: {e}"))
}

/// Every box here, newest first.
fn all() -> Vec<Value> {
    let mut out: Vec<Value> = std::fs::read_dir(dir())
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str().map(String::from)).filter(|n| valid_id(n)).filter_map(|n| read(&n)).collect())
        .unwrap_or_default();
    out.sort_by_key(|b| std::cmp::Reverse(b["created"].as_i64().unwrap_or(0)));
    out
}

fn err(message: impl Into<String>) -> Value {
    json!({ "ok": false, "error": message.into() })
}

/// The docker command: where Muxin found it when it made the box, else on
/// the PATH (a Mac's Docker Desktop, OrbStack and Colima put it outside the
/// PATH ssh gives).
fn docker(b: &Value) -> String {
    if let Some(d) = b["docker"].as_str().filter(|d| std::path::Path::new(d).is_file()) {
        return d.to_string();
    }
    let home = crate::sys::user_home();
    let mut dirs: Vec<PathBuf> = std::env::var("PATH").unwrap_or_default().split(':').map(PathBuf::from).collect();
    dirs.extend(["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"].map(PathBuf::from));
    dirs.push(home.join(".orbstack/bin"));
    dirs.into_iter().map(|d| d.join("docker")).find(|p| p.is_file()).map(|p| p.display().to_string()).unwrap_or_else(|| "docker".into())
}

fn run_docker(b: &Value, args: &[&str]) -> Result<String, String> {
    let out = Command::new(docker(b)).args(args).stdin(Stdio::null()).output().map_err(|e| format!("couldn't run docker: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let e = String::from_utf8_lossy(&out.stderr);
        Err(e.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("docker failed").to_string())
    }
}

/// Sign-ins agent CLIs read from the environment: a box's agent gets the
/// ones set in the machine's own shell (the session's login shell).
const SIGN_IN_VARS: &[&str] = &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "OPENAI_API_KEY", "CODEX_API_KEY", "AMP_API_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY"];

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The session's command: start the container (a paused box) and run the
/// agent in it, in the checkout (at the same path in there as here).
fn run_line(b: &Value) -> String {
    let (d, c) = (sh_quote(&docker(b)), b["container"].as_str().unwrap_or(""));
    let repo = folder(b["id"].as_str().unwrap_or("")).join("repo");
    let env: String = ["TERM", "COLORTERM"].iter().chain(SIGN_IN_VARS).map(|v| format!(" -e {v}")).collect();
    format!("{d} start {c} >/dev/null 2>&1; exec {d} exec -it{env} -w {} {c} sh /home/box/.muxin/start", sh_quote(&repo.display().to_string()))
}

/// The session server's sessions (none when it isn't running).
fn sessions() -> Vec<Value> {
    if crate::sys::connect().is_err() {
        return Vec::new();
    }
    crate::client::handle(&json!({ "op": "list" }))["sessions"].as_array().cloned().unwrap_or_default()
}

fn session_of<'a>(id: &str, sessions: &'a [Value]) -> Option<&'a Value> {
    sessions.iter().find(|s| s["name"].as_str() == Some(id))
}

/// A box's shells are "<box id>-sh-<n>" (its agent's session has the box's id).
fn is_shell_of(id: &str, name: &str) -> bool {
    name.strip_prefix(id).and_then(|r| r.strip_prefix("-sh-")).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// A box's sessions: its agent's and its shells'.
fn box_sessions<'a>(id: &'a str, sessions: &'a [Value]) -> impl Iterator<Item = &'a Value> + 'a {
    sessions.iter().filter(move |s| s["name"].as_str().is_some_and(|n| n == id || is_shell_of(id, n)))
}

fn stop_sessions(id: &str) {
    for s in box_sessions(id, &sessions()) {
        let _ = crate::client::handle(&json!({ "op": "stop", "name": s["name"] }));
    }
}

/// A shell's command: bash in the box's container and checkout, with the
/// sign-ins the machine's shell has (like its agent).
fn shell_line(b: &Value) -> String {
    let (d, c) = (sh_quote(&docker(b)), b["container"].as_str().unwrap_or(""));
    let repo = folder(b["id"].as_str().unwrap_or("")).join("repo");
    let env: String = ["TERM", "COLORTERM"].iter().chain(SIGN_IN_VARS).map(|v| format!(" -e {v}")).collect();
    // Not a login shell: Debian's /etc/profile would drop the agent CLIs' folder from the PATH.
    format!("exec {d} exec -it{env} -w {} {c} bash -i", sh_quote(&repo.display().to_string()))
}

/// What a box is doing: "running" while its agent's session is, "starting"
/// just after it was started, else "paused" (stopped on purpose, idle, or
/// its machine restarted).
fn state_of(b: &Value, sessions: &[Value], now: i64) -> &'static str {
    if b["state"].as_str() != Some("running") {
        return "paused";
    }
    match session_of(b["id"].as_str().unwrap_or(""), sessions) {
        Some(s) if s["status"].as_str() == Some("running") => "running",
        _ if now - b["state_at"].as_i64().unwrap_or(0) < GRACE_SECS => "starting",
        _ => "paused",
    }
}

/// The boxes here, each with its state worked out from `sessions`.
pub fn list_with(sessions: &[Value]) -> Vec<Value> {
    let now = crate::now();
    all()
        .into_iter()
        .map(|mut b| {
            b["state"] = json!(state_of(&b, sessions, now));
            b["path"] = json!(folder(b["id"].as_str().unwrap_or("")).join("repo").display().to_string());
            b
        })
        .collect()
}

pub fn list() -> Value {
    json!({ "ok": true, "boxes": list_with(&sessions()), "now": crate::now() })
}

/// Records a box Muxin made (its container is there, its agent not started).
pub fn save(req: &Value) -> Value {
    let mut b = req["box"].clone();
    let id = b["id"].as_str().unwrap_or("").to_string();
    if !valid_id(&id) {
        return err("a box needs an id like box-ab12cd");
    }
    if !folder(&id).join("repo").is_dir() {
        return err(format!("{} has no folder here", id));
    }
    let container = b["container"].as_str().unwrap_or("");
    if !container.starts_with("muxen-box-") || !container.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return err("that isn't a container Muxin made");
    }
    b["state"] = json!("paused");
    b["state_at"] = json!(crate::now());
    if b["created"].as_i64().is_none() {
        b["created"] = json!(crate::now());
    }
    match write(&b) {
        Ok(()) => json!({ "ok": true, "box": b }),
        Err(e) => err(e),
    }
}

/// Gets a box's agent working: types `prompt` in when it's running, else
/// starts its container and its agent (continuing where it left off), with
/// `prompt` as what it's asked next.
pub fn resume(req: &Value) -> Value {
    let id = req["box_id"].as_str().unwrap_or("");
    let Some(mut b) = valid_id(id).then(|| read(id)).flatten() else { return err(format!("there's no box {id} here")) };
    let prompt = req["prompt"].as_str().map(str::trim).filter(|p| !p.is_empty());
    let current = sessions();
    match session_of(id, &current) {
        Some(s) if s["status"].as_str() == Some("running") => {
            if let Some(p) = prompt {
                send(id, p);
            }
            if b["state"].as_str() != Some("running") {
                b["state"] = json!("running");
                b["state_at"] = json!(crate::now());
                let _ = write(&b);
            }
            ensure_watch();
            b["state"] = json!("running");
            return json!({ "ok": true, "box": b });
        }
        // Its agent crashed: that session goes, a new one starts.
        Some(_) => {
            let _ = crate::client::handle(&json!({ "op": "stop", "name": id }));
        }
        None => {}
    }
    let container = b["container"].as_str().unwrap_or("").to_string();
    if let Err(e) = run_docker(&b, &["start", &container]) {
        return err(format!("its container didn't start: {e}"));
    }
    let typed = b["type_prompt"].as_bool() == Some(true);
    let next = folder(id).join("home").join(".muxin").join("next");
    match (prompt, typed) {
        (Some(p), false) => {
            if let Err(e) = std::fs::write(&next, p) {
                return err(format!("couldn't hand it the prompt: {e}"));
            }
        }
        _ => {
            let _ = std::fs::remove_file(&next);
        }
    }
    b["state"] = json!("running");
    b["state_at"] = json!(crate::now());
    if let Err(e) = write(&b) {
        return err(e);
    }
    let start = json!({
        "op": "start",
        "name": id,
        "agent": b["agent"],
        "title": b["name"],
        "cwd": folder(id).join("repo").display().to_string(),
        "run": run_line(&b),
        "cols": req["cols"].as_u64().unwrap_or(120),
        "rows": req["rows"].as_u64().unwrap_or(40),
    });
    let reply = crate::client::handle(&start);
    if reply["ok"].as_bool() != Some(true) {
        b["state"] = json!("paused");
        let _ = write(&b);
        return err(format!("its agent didn't start: {}", reply["error"].as_str().unwrap_or("no reason given")));
    }
    if let (Some(p), true) = (prompt, typed) {
        std::thread::sleep(TYPE_AFTER);
        send(id, p);
    }
    ensure_watch();
    json!({ "ok": true, "box": b })
}

/// Opens a shell in a running box, beside its agent: bash in its container
/// and checkout. Its name is "<box id>-sh-<n>"; pausing the box ends it.
pub fn shell(req: &Value) -> Value {
    let id = req["box_id"].as_str().unwrap_or("");
    let Some(b) = valid_id(id).then(|| read(id)).flatten() else { return err(format!("there's no box {id} here")) };
    let current = sessions();
    if state_of(&b, &current, crate::now()) != "running" {
        return err(format!("{id} is paused: resume it first"));
    }
    let Some(name) = (1..1000).map(|n| format!("{id}-sh-{n}")).find(|n| session_of(n, &current).is_none()) else { return err("it has too many shells") };
    let start = json!({
        "op": "start",
        "name": name,
        "agent": "shell",
        "title": format!("Shell · {}", b["name"].as_str().unwrap_or(id)),
        "cwd": folder(id).join("repo").display().to_string(),
        "run": shell_line(&b),
        "cols": req["cols"].as_u64().unwrap_or(120),
        "rows": req["rows"].as_u64().unwrap_or(40),
    });
    let reply = crate::client::handle(&start);
    if reply["ok"].as_bool() != Some(true) {
        return err(format!("the shell didn't start: {}", reply["error"].as_str().unwrap_or("no reason given")));
    }
    json!({ "ok": true, "session": name })
}

fn send(id: &str, text: &str) {
    let _ = crate::client::handle(&json!({ "op": "send", "name": id, "text": text, "enter": true }));
}

pub fn pause(req: &Value) -> Value {
    let id = req["box_id"].as_str().unwrap_or("");
    let Some(mut b) = valid_id(id).then(|| read(id)).flatten() else { return err(format!("there's no box {id} here")) };
    match pause_box(&mut b) {
        Ok(()) => json!({ "ok": true, "box": b }),
        Err(e) => err(e),
    }
}

/// `{"op": "box_switch", "box_id", "agent", "start", "type_prompt"}`: the box
/// changes agents (a handoff): its agent's session ends, `start` becomes its
/// launcher (~/.muxin/start), and the new agent starts fresh, not continuing
/// the last one's conversation, when the box starts again (box_resume).
pub fn switch(req: &Value) -> Value {
    let id = req["box_id"].as_str().unwrap_or("");
    let Some(mut b) = valid_id(id).then(|| read(id)).flatten() else { return err(format!("there's no box {id} here")) };
    let agent = req["agent"].as_str().unwrap_or("");
    let start = req["start"].as_str().unwrap_or("");
    if agent.is_empty() || !agent.chars().all(|c| c.is_ascii_lowercase()) || start.is_empty() {
        return err("which agent, and how it starts?");
    }
    let _ = crate::client::handle(&json!({ "op": "stop", "name": id }));
    let muxin = folder(id).join("home").join(".muxin");
    let launcher = muxin.join("start");
    if let Err(e) = std::fs::write(&launcher, start) {
        return err(format!("couldn't write its launcher: {e}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755));
    }
    let _ = std::fs::remove_file(muxin.join("started"));
    b["agent"] = json!(agent);
    b["type_prompt"] = json!(req["type_prompt"].as_bool() == Some(true));
    // Just now, so the watcher gives the new agent its grace before it looks.
    b["state_at"] = json!(crate::now());
    match write(&b) {
        Ok(()) => json!({ "ok": true, "box": b }),
        Err(e) => err(e),
    }
}

/// Ends the box's sessions (its agent and its shells) and stops its
/// container; its files stay.
fn pause_box(b: &mut Value) -> Result<(), String> {
    let id = b["id"].as_str().unwrap_or("").to_string();
    stop_sessions(&id);
    let container = b["container"].as_str().unwrap_or("").to_string();
    if let Err(e) = run_docker(b, &["stop", "-t", "10", &container]) {
        // Gone already is stopped enough.
        if !e.contains("No such container") {
            return Err(format!("its container didn't stop: {e}"));
        }
    }
    b["state"] = json!("paused");
    b["state_at"] = json!(crate::now());
    write(b)
}

/// Removes a box: its session, its container and its folder (the checkout
/// with any work that wasn't pushed).
pub fn delete(req: &Value) -> Value {
    let id = req["box_id"].as_str().unwrap_or("");
    if !valid_id(id) {
        return err(format!("there's no box {id} here"));
    }
    let path = folder(id);
    let Ok(meta) = std::fs::symlink_metadata(&path) else { return err(format!("there's no box {id} here")) };
    if !meta.is_dir() {
        return err(format!("{} isn't a box's folder", path.display()));
    }
    let b = read(id).unwrap_or_else(|| json!({ "id": id, "container": format!("muxen-{id}") }));
    stop_sessions(id);
    let container = b["container"].as_str().unwrap_or("").to_string();
    if let Err(e) = run_docker(&b, &["rm", "-f", &container])
        && !e.contains("No such container")
    {
        return err(format!("its container wasn't removed: {e}"));
    }
    if std::fs::remove_dir_all(&path).is_err() {
        // Files the agent made with sudo belong to root: remove them from a container.
        if let Some(image) = b["image"].as_str() {
            let mount = format!("{}:/box", path.display());
            let _ = run_docker(&b, &["run", "--rm", "--user", "0", "-v", &mount, "--entrypoint", "sh", image, "-c", "rm -rf /box/repo /box/home"]);
        }
        if let Err(e) = std::fs::remove_dir_all(&path) {
            return err(format!("its folder {} wasn't removed: {e}", path.display()));
        }
    }
    json!({ "ok": true })
}

/// Why a running box should pause now, if it should.
fn pause_reason(b: &Value, sessions: &[Value], now: i64) -> Option<&'static str> {
    if b["state"].as_str() != Some("running") || now - b["state_at"].as_i64().unwrap_or(0) < GRACE_SECS {
        return None;
    }
    let id = b["id"].as_str().unwrap_or("");
    let Some(s) = session_of(id, sessions) else { return Some("its agent ended") };
    if s["status"].as_str() != Some("running") {
        return Some("its agent exited");
    }
    // Quiet is quiet whether or not a terminal is open on it: typing makes the
    // agent (or a shell in the box) redraw, so someone using it keeps it going.
    let idle = b["idle_minutes"].as_i64().unwrap_or(IDLE_MINUTES);
    let last = box_sessions(id, sessions).filter_map(|s| s["last_output"].as_i64()).max().unwrap_or(now);
    (idle > 0 && now - last >= idle * 60).then_some("it was idle")
}

fn watch_pid_path() -> PathBuf {
    crate::home_dir().join("box-watch.pid")
}

/// The watcher running now: its pid and build (`self_hash`).
fn watcher() -> Option<(i32, String)> {
    let text = std::fs::read_to_string(watch_pid_path()).ok()?;
    let mut parts = text.split_whitespace();
    let pid: i32 = parts.next()?.parse().ok()?;
    Some((pid, parts.next().unwrap_or("").to_string()))
}

#[cfg(unix)]
fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

#[cfg(not(unix))]
fn alive(_pid: i32) -> bool {
    false
}

/// Makes sure this build's watcher runs (replacing another build's).
pub fn ensure_watch() {
    if let Some((pid, build)) = watcher()
        && alive(pid)
    {
        if build == crate::self_hash() {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let _ = crate::sys::spawn_background(&["box-watch"], crate::home_dir().join("box-watch.log"));
}

/// `muxen-agent box-watch`: pauses idle boxes until none is running.
pub fn watch() -> i32 {
    let me = std::process::id() as i32;
    let build = crate::self_hash();
    if let Some((pid, other)) = watcher()
        && pid != me
        && alive(pid)
        && other == build
    {
        return 0; // one is running already
    }
    let _ = std::fs::write(watch_pid_path(), format!("{me} {build}"));
    let mut quiet = 0;
    loop {
        std::thread::sleep(WATCH_EVERY);
        // Another took over (a newer build).
        if watcher().is_none_or(|(pid, _)| pid != me) {
            return 0;
        }
        let current = sessions();
        let now = crate::now();
        let mut running = 0;
        for mut b in all().into_iter().filter(|b| b["state"].as_str() == Some("running")) {
            running += 1;
            let id = b["id"].as_str().unwrap_or("").to_string();
            if let Some(why) = pause_reason(&b, &current, now) {
                // Started again since this look (Resume): not this time.
                match read(&id) {
                    Some(fresh) if fresh["state"] == b["state"] && fresh["state_at"] == b["state_at"] => b = fresh,
                    _ => continue,
                }
                match pause_box(&mut b) {
                    Ok(()) => eprintln!("{now} paused {id}: {why}"),
                    Err(e) => eprintln!("{now} couldn't pause {id} ({why}): {e}"),
                }
            }
        }
        // Nothing running for two looks: done until a box starts again.
        quiet = if running == 0 { quiet + 1 } else { 0 };
        if quiet >= 2 {
            let _ = std::fs::remove_file(watch_pid_path());
            return 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(at: i64, idle: Option<i64>) -> Value {
        let mut b = json!({ "id": "box-ab12cd", "state": "running", "state_at": at });
        if let Some(i) = idle {
            b["idle_minutes"] = json!(i);
        }
        b
    }

    fn session(status: &str, clients: u64, last_output: i64) -> Value {
        json!({ "name": "box-ab12cd", "status": status, "clients": clients, "last_output": last_output })
    }

    #[test]
    fn the_agent_runs_in_the_checkout_with_the_machines_sign_ins() {
        let line = run_line(&json!({ "id": "box-ab12cd", "container": "muxen-box-ab12cd", "docker": "/nonexistent/docker" }));
        let repo = folder("box-ab12cd").join("repo").display().to_string();
        assert!(line.contains(&format!("-w '{repo}' muxen-box-ab12cd sh /home/box/.muxin/start")), "{line}");
        assert!(line.contains(" -e ANTHROPIC_API_KEY") && line.contains(" -e OPENAI_API_KEY"), "{line}");
        assert!(line.starts_with("'") && line.contains(" start muxen-box-ab12cd "), "{line}");
    }

    #[test]
    fn ids_are_boxes() {
        assert!(valid_id("box-ab12cd"));
        assert!(!valid_id("box-"));
        assert!(!valid_id("box-../x"));
        assert!(!valid_id("box-AB12"));
        assert!(!valid_id("ws-ab12cd"));
    }

    #[test]
    fn pauses_when_quiet_or_ended() {
        let now = 100_000;
        let b = running(now - 3600, None);
        let one = |s: Value| vec![s];
        // Working (recent output, which typing makes too): it stays.
        assert_eq!(pause_reason(&b, &one(session("running", 0, now - 60)), now), None);
        assert_eq!(pause_reason(&b, &one(session("running", 1, now - 60)), now), None);
        // Quiet for longer than 15 minutes, a terminal open on it or not.
        assert_eq!(pause_reason(&b, &one(session("running", 0, now - 16 * 60)), now), Some("it was idle"));
        assert_eq!(pause_reason(&b, &one(session("running", 2, now - 16 * 60)), now), Some("it was idle"));
        assert_eq!(pause_reason(&b, &one(session("exited", 0, now)), now), Some("its agent exited"));
        assert_eq!(pause_reason(&b, &[], now), Some("its agent ended"));
    }

    #[test]
    fn a_shell_in_use_keeps_the_box_going() {
        let now = 100_000;
        let b = running(now - 3600, None);
        let agent = session("running", 0, now - 3600);
        let shell = json!({ "name": "box-ab12cd-sh-1", "status": "running", "clients": 1, "last_output": now - 30 });
        let other = json!({ "name": "claude-3f9a", "status": "running", "clients": 0, "last_output": now });
        assert_eq!(pause_reason(&b, &[agent.clone(), shell], now), None);
        // Another session's output doesn't count, and a shell alone isn't its agent.
        assert_eq!(pause_reason(&b, &[agent, other.clone()], now), Some("it was idle"));
        let only_shell = json!({ "name": "box-ab12cd-sh-1", "status": "running", "clients": 1, "last_output": now });
        assert_eq!(pause_reason(&b, &[only_shell, other], now), Some("its agent ended"));
    }

    #[test]
    fn a_new_box_and_never_idle_ones_stay() {
        let now = 100_000;
        assert_eq!(pause_reason(&running(now - 5, None), &[], now), None);
        let never = running(now - 3600, Some(0));
        assert_eq!(pause_reason(&never, &[session("running", 0, now - 86_400)], now), None);
        let paused = json!({ "id": "box-ab12cd", "state": "paused", "state_at": 0 });
        assert_eq!(pause_reason(&paused, &[], now), None);
    }

    #[test]
    fn shells_belong_to_their_box() {
        assert!(is_shell_of("box-ab12cd", "box-ab12cd-sh-1"));
        assert!(is_shell_of("box-ab12cd", "box-ab12cd-sh-12"));
        assert!(!is_shell_of("box-ab12cd", "box-ab12cd"));
        assert!(!is_shell_of("box-ab12cd", "box-ab12cd-sh-"));
        assert!(!is_shell_of("box-ab12cd", "box-ab12cdx-sh-1"));
        assert!(!is_shell_of("box-ab", "box-ab12cd-sh-1"));
    }

    #[test]
    fn state_follows_the_session() {
        let now = 100_000;
        let b = running(now - 3600, None);
        assert_eq!(state_of(&b, &[session("running", 0, now)], now), "running");
        assert_eq!(state_of(&b, &[], now), "paused");
        assert_eq!(state_of(&running(now - 5, None), &[], now), "starting");
        // Its agent quit (Ctrl-C, /exit): paused, not "starting".
        assert_eq!(state_of(&running(now - 30, None), &[], now), "paused");
        assert_eq!(state_of(&json!({ "id": "box-ab12cd", "state": "paused" }), &[session("running", 0, now)], now), "paused");
    }
}
