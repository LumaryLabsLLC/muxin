//! Schedules: a command, or an agent session, that runs on this machine at
//! set times, whether or not Muxen is open anywhere. Each one is kept in
//! `~/.muxen/schedules/<id>.json` and registered with this machine's own
//! scheduler (cron on Linux, launchd on a Mac, Task Scheduler on Windows),
//! which runs `muxen-agent schedule-run <id>`. A run's output goes to
//! `~/.muxen/schedules/<id>/`, and a line about it to its runs.jsonl.
//!
//! A schedule, as Muxen sends it:
//!
//! ```json
//! { "id": "s-ab12cd", "name": "Nightly tests", "enabled": true,
//!   "when": { "every": "day", "at": "09:00" },
//!   "what": { "kind": "command", "command": "make test", "cwd": "~/code/api" } }
//! ```
//!
//! `when.every`: "minutes" or "hours" (with `n`; "hours" at `at`'s minute),
//! "day", "weekdays", or "week" (with `days`: ["mon", "thu"]), at `at` ("HH:MM").
//! `what.kind`: "command" (run in the user's login shell) or "agent" (an
//! agent session started like Muxen starts one: `agent`, `title`, `cwd`,
//! `run`, and `send`, text typed in once it's up).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const MARK_BEGIN: &str = "# muxen schedules: managed by Muxen (edits between these lines are replaced)";
const MARK_END: &str = "# end of muxen schedules";
/// Runs kept per schedule.
const KEEP_RUNS: usize = 30;
/// A command that runs longer than this is ended.
const MAX_RUN: Duration = Duration::from_secs(6 * 3600);

fn dir() -> PathBuf {
    let d = crate::home_dir().join("schedules");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn valid_id(id: &str) -> bool {
    id.starts_with("s-") && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn exe() -> String {
    let name = if cfg!(windows) { "muxen-agent.exe" } else { "muxen-agent" };
    crate::home_dir().join("bin").join(name).display().to_string()
}

fn read(id: &str) -> Option<Value> {
    std::fs::read_to_string(dir().join(format!("{id}.json"))).ok().and_then(|t| serde_json::from_str(&t).ok())
}

fn all() -> Vec<Value> {
    let mut out: Vec<Value> = std::fs::read_dir(dir())
        .map(|d| d.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).filter_map(|e| std::fs::read_to_string(e.path()).ok()).filter_map(|t| serde_json::from_str(&t).ok()).collect())
        .unwrap_or_default();
    out.sort_by(|a: &Value, b: &Value| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
    out
}

/// The last runs of a schedule, newest first.
fn runs(id: &str, n: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(dir().join(id).join("runs.jsonl")).unwrap_or_default();
    let mut v: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    v.reverse();
    v.truncate(n);
    v
}

/// Every schedule here, with its last run; and which scheduler runs them.
pub fn list() -> Value {
    let (scheduler, problem) = scheduler();
    let items: Vec<Value> = all()
        .into_iter()
        .map(|mut s| {
            let id = s["id"].as_str().unwrap_or("").to_string();
            s["last_run"] = runs(&id, 1).into_iter().next().unwrap_or(Value::Null);
            s["running"] = json!(running(&id));
            s
        })
        .collect();
    json!({ "ok": true, "schedules": items, "scheduler": scheduler, "problem": problem, "now": crate::now() })
}

/// Saves a schedule (new or changed) and registers it.
pub fn save(req: &Value) -> Value {
    let s = &req["schedule"];
    let id = s["id"].as_str().unwrap_or("");
    if !valid_id(id) {
        return json!({ "ok": false, "error": "a schedule needs an id like s-ab12cd" });
    }
    if let Err(e) = check(s) {
        return json!({ "ok": false, "error": e });
    }
    if let (None, Some(problem)) = scheduler() {
        return json!({ "ok": false, "error": problem });
    }
    let path = dir().join(format!("{id}.json"));
    let before = std::fs::read(&path).ok();
    if let Err(e) = std::fs::write(&path, serde_json::to_vec_pretty(s).unwrap_or_default()) {
        return json!({ "ok": false, "error": format!("couldn't save it: {e}") });
    }
    match register_all() {
        Ok(()) => json!({ "ok": true, "schedule": s }),
        Err(e) => {
            // As it was: the one before, or none.
            let _ = match before {
                Some(b) => std::fs::write(&path, b),
                None => std::fs::remove_file(&path),
            };
            let _ = register_all();
            json!({ "ok": false, "error": e })
        }
    }
}

/// Removes a schedule, its registration and its runs.
pub fn delete(req: &Value) -> Value {
    let id = req["schedule_id"].as_str().unwrap_or("");
    if !valid_id(id) || read(id).is_none() {
        return json!({ "ok": false, "error": format!("no schedule {id} here") });
    }
    let _ = std::fs::remove_file(dir().join(format!("{id}.json")));
    unregister(id);
    let _ = std::fs::remove_dir_all(dir().join(id));
    // Nothing runs schedules here: nothing to take it out of.
    if scheduler().0.is_none() {
        return json!({ "ok": true });
    }
    match register_all() {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

/// Runs one now, in the background.
pub fn run_now(req: &Value) -> Value {
    let id = req["schedule_id"].as_str().unwrap_or("");
    if !valid_id(id) || read(id).is_none() {
        return json!({ "ok": false, "error": format!("no schedule {id} here") });
    }
    if running(id) {
        return json!({ "ok": false, "error": "it's running now" });
    }
    match crate::sys::spawn_background(&["schedule-run", id], dir().join("background.log")) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": format!("couldn't start it: {e}") }),
    }
}

/// A schedule's last runs, and the output of one (the newest, or `started`).
pub fn log(req: &Value) -> Value {
    let id = req["schedule_id"].as_str().unwrap_or("");
    if !valid_id(id) {
        return json!({ "ok": false, "error": format!("no schedule {id} here") });
    }
    let list = runs(id, KEEP_RUNS);
    let pick = match req["started"].as_i64() {
        Some(t) => list.iter().find(|r| r["started"].as_i64() == Some(t)).cloned(),
        None => list.first().cloned(),
    };
    let output = pick.as_ref().and_then(|r| r["started"].as_i64()).map(|t| tail(&dir().join(id).join(format!("{t}.log")), 64 * 1024)).unwrap_or_default();
    json!({ "ok": true, "runs": list, "run": pick, "output": output })
}

fn tail(path: &std::path::Path, max: u64) -> String {
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max {
        use std::io::Seek;
        let _ = f.seek(std::io::SeekFrom::Start(len - max));
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// What's wrong with a schedule, if anything.
fn check(s: &Value) -> Result<(), String> {
    if s["name"].as_str().is_none_or(|n| n.trim().is_empty()) {
        return Err("give it a name".into());
    }
    let w = &s["when"];
    match w["every"].as_str().unwrap_or("") {
        "minutes" | "hours" => {
            let n = w["n"].as_u64().unwrap_or(0);
            let max = if w["every"] == "minutes" { 59 } else { 23 };
            if n == 0 || n > max {
                return Err(format!("every {n} {}: pick 1 to {max}", w["every"].as_str().unwrap_or("")));
            }
        }
        "day" | "weekdays" => {}
        "week" if days(w).is_empty() => return Err("pick the days of the week".into()),
        "week" => {}
        other => return Err(format!("unknown schedule \"{other}\"")),
    }
    if !matches!(w["every"].as_str(), Some("minutes")) && at(w).is_none() {
        return Err("a time like 09:00".into());
    }
    match s["what"]["kind"].as_str().unwrap_or("") {
        "command" if s["what"]["command"].as_str().is_none_or(|c| c.trim().is_empty()) => Err("a command to run".into()),
        "agent" if s["what"]["run"].as_str().is_none() => Err("an agent to start".into()),
        "command" | "agent" => Ok(()),
        other => Err(format!("unknown kind \"{other}\"")),
    }
}

/// `at` as (hour, minute).
fn at(w: &Value) -> Option<(u32, u32)> {
    let (h, m) = w["at"].as_str()?.split_once(':')?;
    let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

/// Days of the week, 0 = Sunday.
fn days(w: &Value) -> Vec<u32> {
    let names = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    match w["every"].as_str() {
        Some("weekdays") => vec![1, 2, 3, 4, 5],
        _ => w["days"].as_array().map(|a| a.iter().filter_map(|d| d.as_str()).filter_map(|d| names.iter().position(|n| d.to_ascii_lowercase().starts_with(n))).map(|i| i as u32).collect()).unwrap_or_default(),
    }
}

/// The schedule as a cron time: "30 9 * * 1-5".
fn cron_time(w: &Value) -> Option<String> {
    let (h, m) = at(w).unwrap_or((0, 0));
    let n = w["n"].as_u64().unwrap_or(1);
    Some(match w["every"].as_str()? {
        "minutes" => format!("*/{n} * * * *"),
        "hours" => format!("{m} */{n} * * *"),
        "day" => format!("{m} {h} * * *"),
        "weekdays" | "week" => format!("{m} {h} * * {}", days(w).iter().map(u32::to_string).collect::<Vec<_>>().join(",")),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Registering with the machine's scheduler

/// Which scheduler runs them here, and why none can when none can.
fn scheduler() -> (Option<&'static str>, Option<String>) {
    if cfg!(windows) {
        (Some("task-scheduler"), None)
    } else if cfg!(target_os = "macos") {
        (Some("launchd"), None)
    } else if crate::root::tool("crontab").is_none() {
        (None, Some("this machine has no cron: install it (apt install cron, or dnf install cronie)".into()))
    } else if !cron_running() {
        (None, Some("cron isn't running here (a container has none): start it, e.g. sudo systemctl enable --now cron".into()))
    } else {
        (Some("cron"), None)
    }
}

/// A cron daemon is running (Linux: one named cron or crond in /proc).
fn cron_running() -> bool {
    std::fs::read_dir("/proc")
        .map(|d| d.flatten().any(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| matches!(c.trim(), "cron" | "crond" | "cronie" | "fcron"))))
        .unwrap_or(true)
}

/// Registers every schedule here (and drops what's gone), in one go.
fn register_all() -> Result<(), String> {
    let schedules: Vec<Value> = all().into_iter().filter(|s| s["enabled"].as_bool() != Some(false)).collect();
    #[cfg(windows)]
    {
        for s in all() {
            let id = s["id"].as_str().unwrap_or("");
            if s["enabled"].as_bool() == Some(false) {
                unregister(id);
            }
        }
        for s in &schedules {
            windows_task(s)?;
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    if cfg!(target_os = "macos") {
        let labels: Vec<String> = schedules.iter().filter_map(|s| s["id"].as_str()).map(label).collect();
        // Ones turned off or removed.
        if let Ok(entries) = std::fs::read_dir(launch_agents()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(l) = name.strip_suffix(".plist").filter(|l| l.starts_with(LABEL_PREFIX))
                    && !labels.iter().any(|x| x == l)
                {
                    launchd_remove(l);
                }
            }
        }
        for s in &schedules {
            launchd_add(s)?;
        }
        return Ok(());
    }
    let lines: Vec<String> = schedules
        .iter()
        .filter_map(|s| Some(format!("{} {} schedule-run {} >/dev/null 2>&1", cron_time(&s["when"])?, exe(), s["id"].as_str()?)))
        .collect();
    cron_write(&lines)
}

fn unregister(id: &str) {
    #[cfg(windows)]
    {
        let _ = crate::win::ps(&format!("Unregister-ScheduledTask -TaskName {} -TaskPath '\\Muxen\\' -Confirm:$false -ErrorAction SilentlyContinue", crate::win::quote(&task_name(id))));
        return;
    }
    #[allow(unreachable_code)]
    if cfg!(target_os = "macos") {
        launchd_remove(&label(id));
    }
    // cron: register_all rewrites the block without it.
}

/// The user's crontab with Muxen's block replaced by `lines`.
fn cron_write(lines: &[String]) -> Result<(), String> {
    let crontab = crate::root::tool("crontab").ok_or_else(|| scheduler().1.unwrap_or_default())?;
    let current = Command::new(&crontab).arg("-l").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    // Nothing of Muxen's there, and nothing to add: leave it alone.
    if lines.is_empty() && !current.contains(MARK_BEGIN) {
        return Ok(());
    }
    let mut out = String::new();
    let mut inside = false;
    for l in current.lines() {
        if l == MARK_BEGIN {
            inside = true;
        } else if l == MARK_END {
            inside = false;
        } else if !inside {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !lines.is_empty() {
        out.push_str(MARK_BEGIN);
        out.push('\n');
        for l in lines {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str(MARK_END);
        out.push('\n');
    }
    let mut child = Command::new(&crontab).arg("-").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("couldn't run crontab: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(out.as_bytes());
    }
    let o = child.wait_with_output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(()) } else { Err(format!("crontab refused it: {}", String::from_utf8_lossy(&o.stderr).trim())) }
}

const LABEL_PREFIX: &str = "com.lumarylabs.muxen.schedule.";

fn label(id: &str) -> String {
    format!("{LABEL_PREFIX}{id}")
}

fn launch_agents() -> PathBuf {
    crate::sys::user_home().join("Library/LaunchAgents")
}

fn launchd_add(s: &Value) -> Result<(), String> {
    let id = s["id"].as_str().unwrap_or("");
    let w = &s["when"];
    let (h, m) = at(w).unwrap_or((0, 0));
    let n = w["n"].as_u64().unwrap_or(1);
    let dict = |pairs: &[(&str, u32)]| format!("<dict>{}</dict>", pairs.iter().map(|(k, v)| format!("<key>{k}</key><integer>{v}</integer>")).collect::<String>());
    let timing = match w["every"].as_str().unwrap_or("") {
        "minutes" => format!("<key>StartInterval</key><integer>{}</integer>", n * 60),
        "hours" if n == 1 => format!("<key>StartCalendarInterval</key>{}", dict(&[("Minute", m)])),
        "hours" => format!("<key>StartCalendarInterval</key><array>{}</array>", (0..24).step_by(n as usize).map(|hr| dict(&[("Hour", hr), ("Minute", m)])).collect::<String>()),
        "day" => format!("<key>StartCalendarInterval</key>{}", dict(&[("Hour", h), ("Minute", m)])),
        _ => format!("<key>StartCalendarInterval</key><array>{}</array>", days(w).iter().map(|d| dict(&[("Weekday", *d), ("Hour", h), ("Minute", m)])).collect::<String>()),
    };
    let l = label(id);
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{l}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>schedule-run</string><string>{id}</string></array>
  {timing}
  <key>StandardOutPath</key><string>/dev/null</string>
  <key>StandardErrorPath</key><string>/dev/null</string>
</dict></plist>
"#,
        exe()
    );
    let _ = std::fs::create_dir_all(launch_agents());
    let plist = launch_agents().join(format!("{l}.plist"));
    std::fs::write(&plist, body).map_err(|e| format!("couldn't write {}: {e}", plist.display()))?;
    // In the user's desktop session (its Keychain, what agents sign in
    // with), else their background one (nobody signed in to the screen).
    for domain in [format!("gui/{}", uid()), format!("user/{}", uid())] {
        let _ = quiet("launchctl", &["bootout", &format!("{domain}/{l}")]);
        if quiet("launchctl", &["bootstrap", &domain, &plist.display().to_string()]) {
            return Ok(());
        }
    }
    Err("launchd didn't take it".into())
}

fn launchd_remove(l: &str) {
    for domain in [format!("gui/{}", uid()), format!("user/{}", uid())] {
        let _ = quiet("launchctl", &["bootout", &format!("{domain}/{l}")]);
    }
    let _ = std::fs::remove_file(launch_agents().join(format!("{l}.plist")));
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

fn quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg_attr(not(windows), allow(dead_code))]
fn task_name(id: &str) -> String {
    format!("Muxen schedule {id}")
}

/// A task in Task Scheduler's Muxen folder, for this user while they're
/// signed in, through a windowless console.
#[cfg(windows)]
fn windows_task(s: &Value) -> Result<(), String> {
    let id = s["id"].as_str().unwrap_or("");
    let w = &s["when"];
    let (h, m) = at(w).unwrap_or((0, 0));
    let n = w["n"].as_u64().unwrap_or(1);
    let time = format!("'{h:02}:{m:02}'");
    let names = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let trigger = match w["every"].as_str().unwrap_or("") {
        "minutes" => format!("New-ScheduledTaskTrigger -Once -At (Get-Date).Date -RepetitionInterval (New-TimeSpan -Minutes {n})"),
        "hours" => format!("New-ScheduledTaskTrigger -Once -At ((Get-Date).Date.AddMinutes({m})) -RepetitionInterval (New-TimeSpan -Hours {n})"),
        "day" => format!("New-ScheduledTaskTrigger -Daily -At {time}"),
        _ => format!("New-ScheduledTaskTrigger -Weekly -DaysOfWeek {} -At {time}", days(w).iter().map(|d| names[*d as usize]).collect::<Vec<_>>().join(",")),
    };
    let script = format!(
        "$exe = {exe}\n\
         $action = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument ('--headless \"' + $exe + '\" schedule-run {id}')\n\
         $trigger = {trigger}\n\
         $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited\n\
         $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Hours 6)\n\
         Register-ScheduledTask -TaskName {task} -TaskPath '\\Muxen\\' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null\n\
         'ok'",
        exe = crate::win::quote(&exe()),
        task = crate::win::quote(&task_name(id)),
    );
    crate::win::ps(&script).map(|_| ()).map_err(|e| format!("Task Scheduler didn't take it: {e}"))
}

// ---------------------------------------------------------------------------
// Running one

fn lock_path(id: &str) -> PathBuf {
    dir().join(id).join("running")
}

/// A run of it is going on now (its lock names a live process).
fn running(id: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(lock_path(id)) else { return false };
    let Ok(pid) = text.trim().parse::<u32>() else { return false };
    alive(pid)
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
}

/// `muxen-agent schedule-run <id>`: what the scheduler runs.
pub fn run(id: &str) -> i32 {
    if !valid_id(id) {
        return 2;
    }
    let Some(s) = read(id) else {
        // Gone, but still registered somewhere: drop that.
        unregister(id);
        return 0;
    };
    if running(id) {
        return 0;
    }
    let folder = dir().join(id);
    let _ = std::fs::create_dir_all(&folder);
    let _ = std::fs::write(lock_path(id), std::process::id().to_string());
    let started = crate::now();
    let clock = Instant::now();
    let what = &s["what"];
    let record = match what["kind"].as_str().unwrap_or("") {
        "agent" => match start_session(&s) {
            Ok(session) => json!({ "started": started, "ok": true, "session": session }),
            Err(e) => {
                let _ = std::fs::write(folder.join(format!("{started}.log")), &e);
                json!({ "started": started, "ok": false, "error": e })
            }
        },
        _ => {
            let code = run_command(what, &folder.join(format!("{started}.log")));
            json!({ "started": started, "ok": code == Some(0), "exit_code": code, "seconds": clock.elapsed().as_secs() })
        }
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(folder.join("runs.jsonl")) {
        let _ = writeln!(f, "{record}");
    }
    trim(&folder);
    let _ = std::fs::remove_file(lock_path(id));
    0
}

/// Runs the command in the user's login shell, its output to `log`.
fn run_command(what: &Value, log: &std::path::Path) -> Option<i32> {
    let command = what["command"].as_str().unwrap_or("");
    let cwd = crate::sys::expand_home(what["cwd"].as_str().filter(|c| !c.trim().is_empty()).unwrap_or("~"));
    let out = std::fs::File::create(log).ok()?;
    let err = out.try_clone().ok()?;
    let shell = crate::detect::user_shell();
    let mut cmd = Command::new(&shell);
    if cfg!(windows) {
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", command]);
    } else {
        cmd.args(["-l", "-c", command]);
    }
    cmd.current_dir(if cwd.is_dir() { cwd } else { crate::sys::user_home() }).stdin(Stdio::null()).stdout(out).stderr(err);
    let mut child = cmd.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if started.elapsed() > MAX_RUN => {
                let _ = child.kill();
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(log) {
                    let _ = writeln!(f, "\n[muxen: ended after {} hours]", MAX_RUN.as_secs() / 3600);
                }
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(_) => return None,
        }
    }
}

/// Starts the schedule's agent session in the session server (starting
/// that if need be), typing its prompt in when the agent takes it that way.
fn start_session(s: &Value) -> Result<String, String> {
    let what = &s["what"];
    let agent = what["agent"].as_str().unwrap_or("shell");
    // Its own name each run: the schedule's id and the time.
    let id = s["id"].as_str().unwrap_or("s-0").trim_start_matches("s-");
    let name = format!("{agent}-{}{}", &id[..id.len().min(4)], crate::now() % 100_000);
    let title = what["title"].as_str().filter(|t| !t.is_empty()).or(s["name"].as_str()).unwrap_or("Scheduled");
    let req = json!({ "op": "start", "name": name, "agent": agent, "title": title, "cwd": what["cwd"].as_str().unwrap_or("~"), "run": what["run"].as_str().unwrap_or("") });
    let reply = crate::client::handle(&req);
    if reply["ok"].as_bool() != Some(true) {
        return Err(reply["error"].as_str().unwrap_or("the session didn't start").to_string());
    }
    if let Some(text) = what["send"].as_str().filter(|t| !t.is_empty()) {
        std::thread::sleep(Duration::from_secs(3));
        let _ = crate::client::handle(&json!({ "op": "send", "name": name, "text": text, "enter": true }));
    }
    Ok(name)
}

/// Keeps the last KEEP_RUNS runs and their logs.
fn trim(folder: &std::path::Path) {
    let path = folder.join("runs.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= KEEP_RUNS {
        return;
    }
    let (old, keep) = lines.split_at(lines.len() - KEEP_RUNS);
    for l in old {
        if let Some(t) = serde_json::from_str::<Value>(l).ok().and_then(|v| v["started"].as_i64()) {
            let _ = std::fs::remove_file(folder.join(format!("{t}.log")));
        }
    }
    let _ = std::fs::write(&path, keep.join("\n") + "\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_schedules_into_cron_times() {
        let w = |v: Value| cron_time(&v).unwrap();
        assert_eq!(w(json!({ "every": "minutes", "n": 15 })), "*/15 * * * *");
        assert_eq!(w(json!({ "every": "hours", "n": 2, "at": "00:30" })), "30 */2 * * *");
        assert_eq!(w(json!({ "every": "day", "at": "09:05" })), "5 9 * * *");
        assert_eq!(w(json!({ "every": "weekdays", "at": "18:00" })), "0 18 * * 1,2,3,4,5");
        assert_eq!(w(json!({ "every": "week", "days": ["mon", "Thursday"], "at": "07:00" })), "0 7 * * 1,4");
    }

    #[test]
    fn checks_what_it_saves() {
        let ok = json!({ "name": "Tests", "when": { "every": "day", "at": "09:00" }, "what": { "kind": "command", "command": "make test" } });
        assert!(check(&ok).is_ok());
        let mut bad = ok.clone();
        bad["when"]["at"] = json!("25:00");
        assert!(check(&bad).is_err());
        let mut bad = ok.clone();
        bad["what"]["command"] = json!("  ");
        assert!(check(&bad).is_err());
        assert!(check(&json!({ "name": "x", "when": { "every": "week", "at": "09:00" }, "what": { "kind": "command", "command": "a" } })).is_err());
        assert!(valid_id("s-ab12cd") && !valid_id("../x") && !valid_id("s-a/b"));
    }
}
