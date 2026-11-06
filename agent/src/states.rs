//! What each session's agent says it's doing, rather than a guess from its
//! output: `working`, `needs_you` (a question or an approval waits), `done`
//! (it finished its turn) or `idle`.
//!
//! Two ways in:
//! - Hooks in Claude Code's and Codex's settings (see hooks.rs) run
//!   `muxen-agent report <agent>` with the hook's JSON on stdin. It finds its
//!   session by `MUXEN_SESSION`, which the session server sets in every
//!   session it starts.
//! - Any program can print `ESC ] 7501 ; state=<state> BEL` (herdr's code),
//!   which the session server reads from the output (see `Escapes`).
//!
//! Either way the state lands in `~/.muxen/state/<session>.json`. A file, not
//! the session server's memory, so it works with a session server of any
//! version (it goes on running the build it started with) and with `listen`
//! and `link`, which read it too. The agent's own id for the conversation is
//! kept there as well, for resuming it later.

use std::io::Read;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::{home_dir, now};

/// A reported "working" holds while the agent prints (Claude Code and Codex
/// animate while they work). Quiet this long, it stopped without saying so
/// (Claude Code says nothing when Esc interrupts it): back to guessing.
pub const WORKING_QUIET: i64 = 45;
/// What's printed this soon after someone typed is an echo, not the agent
/// reacting to an answer.
const ECHO: i64 = 2;

pub const STATES: [&str; 4] = ["working", "needs_you", "done", "idle"];

fn dir() -> PathBuf {
    home_dir().join("state")
}

/// A session's name as a file name (the server refuses names with slashes).
fn file(name: &str) -> Option<PathBuf> {
    let ok = !name.is_empty() && name.len() <= 200 && !name.starts_with('.') && !name.contains(['/', '\\', '\0']);
    ok.then(|| dir().join(format!("{name}.json")))
}

pub fn read(name: &str) -> Option<Value> {
    let text = std::fs::read_to_string(file(name)?).ok()?;
    serde_json::from_str::<Value>(&text).ok().filter(Value::is_object)
}

/// Writes a session's report as it is (the session server, on a restart).
pub fn replace(name: &str, report: &Value) -> std::io::Result<()> {
    write(name, report)
}

fn write(name: &str, report: &Value) -> std::io::Result<()> {
    let path = file(name).ok_or_else(|| std::io::Error::other("not a session name"))?;
    std::fs::create_dir_all(dir())?;
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, report.to_string())?;
    std::fs::rename(&tmp, &path)
}

/// Forgets a session's state: it ended, or starts over.
pub fn remove(name: &str) {
    if let Some(path) = file(name) {
        let _ = std::fs::remove_file(path);
    }
}

/// Every session's report, by session name.
pub fn all() -> Map<String, Value> {
    let mut out = Map::new();
    let Ok(entries) = std::fs::read_dir(dir()) else { return out };
    for e in entries.flatten() {
        let file_name = e.file_name().to_string_lossy().to_string();
        let Some(name) = file_name.strip_suffix(".json") else { continue };
        if let Some(v) = std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(Value::is_object) {
            out.insert(name.to_string(), v);
        }
    }
    out
}

/// Forgets the states of sessions that are gone (`names`: the ones there are).
pub fn prune(names: &[&str]) {
    for name in all().keys() {
        if !names.contains(&name.as_str()) {
            remove(name);
        }
    }
}

/// The state a report still stands for, given what the session did since:
/// `working` while it prints, `needs_you` until someone answers and it
/// reacts, `done` and `idle` until the next report. `None`: it stopped
/// holding (or there's none), so the app goes by the output.
pub fn holds(report: &Value, last_output: i64, last_input: i64, now: i64) -> Option<&str> {
    let state = report["state"].as_str().filter(|s| STATES.contains(s))?;
    let at = report["at"].as_i64().unwrap_or(0);
    match state {
        "working" if now - last_output.max(at) > WORKING_QUIET => None,
        // Answered from the terminal: typed since, and it printed after that.
        "needs_you" if last_input > at && last_output > last_input + ECHO => None,
        s => Some(s),
    }
}

/// Adds what's reported to a session list's entries (`sessions` of the
/// server's `list`): `state`, `state_at` and `state_source` while the report
/// holds, and `conversation` whenever it's known.
pub fn annotate(sessions: &mut Value, reports: &Map<String, Value>, now: i64) {
    for s in sessions.as_array_mut().into_iter().flatten() {
        let Some(r) = s["name"].as_str().and_then(|n| reports.get(n)) else { continue };
        if let Some(c) = r["conversation"].as_str().filter(|c| !c.is_empty()) {
            s["conversation"] = json!(c);
        }
        let running = s["status"].as_str() == Some("running");
        let (out, input) = (s["last_output"].as_i64().unwrap_or(0), s["last_input"].as_i64().unwrap_or(0));
        if let Some(state) = holds(r, out, input, now).filter(|_| running) {
            s["state"] = json!(state);
            s["state_at"] = r["at"].clone();
            s["state_source"] = r["source"].clone();
        }
    }
}

// MARK: Hooks

/// What a hook event means for the session: a state, `Some(None)` for "keep
/// the state" (the conversation id may still be news), `None` to ignore it.
fn meaning(input: &Value) -> Option<Option<&'static str>> {
    // A subagent's own events (Claude Code's Task tool) aren't the session's.
    if input.get("agent_id").is_some_and(|a| !a.is_null()) {
        return None;
    }
    let event = input["hook_event_name"].as_str().unwrap_or("");
    Some(match event {
        // A new conversation (or a resumed one) waits for its first message;
        // a compaction happens in the middle of work.
        "SessionStart" => match input["source"].as_str() {
            Some("compact") => None,
            _ => Some("idle"),
        },
        "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" => Some("working"),
        // Claude Code's question tool waits for an answer.
        "PreToolUse" => Some(if input["tool_name"].as_str() == Some("AskUserQuestion") { "needs_you" } else { "working" }),
        "PermissionRequest" => Some("needs_you"),
        "Notification" => {
            let kind = input["notification_type"].as_str().unwrap_or("");
            let message = input["message"].as_str().unwrap_or("").to_lowercase();
            // Not "waiting for your input" after a minute's quiet: that's done, still.
            let asks = matches!(kind, "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input") || (kind.is_empty() && message.contains("permission"));
            if !asks {
                return None;
            }
            Some("needs_you")
        }
        "Stop" | "StopFailure" => Some("done"),
        // Codex: Esc stopped it, the user is right there.
        "Interrupt" | "SessionEnd" => Some("idle"),
        _ => return None,
    })
}

/// The program that ran this hook: the first of this process's ancestors
/// that isn't a shell (hooks run through one or two).
fn hook_owner() -> Option<u32> {
    let mut pid = parent_of(std::process::id())?;
    for _ in 0..8 {
        let (name, parent) = process(pid)?;
        if !matches!(name.trim_start_matches('-'), "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh" | "mksh" | "ash" | "tcsh" | "csh" | "busybox") {
            return Some(pid);
        }
        pid = parent;
    }
    None
}

fn parent_of(pid: u32) -> Option<u32> {
    process(pid).map(|(_, parent)| parent)
}

/// A process's name and parent.
#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<(String, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (name) state ppid ...": the name can hold spaces and parentheses.
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let name = stat[open + 1..close].to_string();
    let ppid = stat[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
    Some((name, ppid))
}

#[cfg(target_os = "macos")]
fn process(pid: u32) -> Option<(String, u32)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
    if got != size {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(info.pbi_comm.as_ptr()) }.to_string_lossy().to_string();
    Some((name, info.pbi_ppid))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process(_pid: u32) -> Option<(String, u32)> {
    None
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    false
}

/// Whether a report from the program `pid` speaks for the session, which
/// `before` last heard from. Another agent CLI that the session's own one
/// started (a `claude -p` in its Bash tool, say) runs the same hooks in the
/// same session: it doesn't, while the one it heard from still runs.
fn speaks_for(before: Option<&Value>, pid: Option<u32>, alive: impl Fn(u32) -> bool) -> bool {
    let (Some(before), Some(pid)) = (before, pid) else { return true };
    if before["source"].as_str() != Some("hook") {
        return true;
    }
    match before["pid"].as_u64().map(|p| p as u32) {
        Some(owner) => owner == pid || !alive(owner),
        None => true,
    }
}

/// The report a hook makes of `input`, given the session's last one; `None`
/// when it changes nothing.
fn from_hook(agent: &str, input: &Value, before: Option<&Value>, pid: Option<u32>, at: i64, alive: impl Fn(u32) -> bool) -> Option<Value> {
    let state = meaning(input)?;
    if !speaks_for(before, pid, alive) {
        return None;
    }
    let conversation = input["session_id"].as_str().filter(|c| !c.is_empty() && c.len() <= 200).map(str::to_string).or_else(|| before.and_then(|b| b["conversation"].as_str().map(str::to_string)));
    let state = state.map(str::to_string).or_else(|| before.and_then(|b| b["state"].as_str().map(str::to_string)));
    let mut report = json!({
        "state": state,
        "at": at,
        "source": "hook",
        "agent": agent,
        "event": input["hook_event_name"],
        "conversation": conversation,
    });
    if let Some(pid) = pid {
        report["pid"] = json!(pid);
    }
    if before.is_some_and(|b| b["state"] == report["state"] && b["conversation"] == report["conversation"] && b["pid"] == report["pid"]) && state_kept(input) {
        return None;
    }
    Some(report)
}

/// Events that say nothing new when the state is already that one (and so
/// needn't touch the file): every tool call is two of them.
fn state_kept(input: &Value) -> bool {
    matches!(input["hook_event_name"].as_str(), Some("PreToolUse" | "PostToolUse" | "PostToolUseFailure"))
}

/// `muxen-agent report <agent>`, run by a hook with its JSON on stdin. Quiet,
/// and always a success: whatever happens, the agent goes on. (Claude Code
/// adds what some hooks print to the conversation, so nothing is printed.)
pub fn report(args: &[String]) -> i32 {
    let session = std::env::var("MUXIN_SESSION").ok().filter(|s| !s.is_empty()).or_else(|| std::env::var("MUXEN_SESSION").ok().filter(|s| !s.is_empty()));
    let Some(session) = session else { return 0 };
    let agent = args.first().map(String::as_str).unwrap_or("").chars().filter(char::is_ascii_alphanumeric).take(20).collect::<String>();
    let mut text = String::new();
    let _ = std::io::stdin().take(32 << 20).read_to_string(&mut text);
    let Ok(input) = serde_json::from_str::<Value>(&text) else { return 0 };
    let before = read(&session);
    if let Some(report) = from_hook(&agent, &input, before.as_ref(), hook_owner(), now(), alive) {
        let _ = write(&session, &report);
    }
    0
}

// MARK: The escape code

/// The states the escape code can set, as herdr and Muxin name them.
fn escape_state(word: &str) -> Option<Option<&'static str>> {
    Some(match word.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "working" | "busy" | "running" | "thinking" => Some("working"),
        "blocked" | "needs_you" | "waiting" | "input" | "question" => Some("needs_you"),
        "done" | "finished" | "complete" | "completed" => Some("done"),
        "idle" => Some("idle"),
        // Back to guessing from the output.
        "clear" | "reset" | "none" | "auto" | "unknown" => None,
        _ => return None,
    })
}

/// What one escape code's text (`state=blocked;...`) asks for.
fn parse_escape(body: &str) -> Option<Option<&'static str>> {
    body.split(';').find_map(|kv| kv.split_once('=').filter(|(k, _)| k.trim() == "state").and_then(|(_, v)| escape_state(v)))
}

/// Someone typed into a session through Muxin (`send`): an agent that was
/// waiting (it needed you, or it was done and Enter sends the message) is
/// working on that now, and says so itself a moment later. Until then, the
/// old state isn't the answer to what was just asked (`session_wait`). The
/// report from before, to put back should the typing fail.
pub fn typed(name: &str, enter: bool, at: i64) -> Option<Value> {
    let before = read(name)?;
    let next = after_typing(&before, enter, at)?;
    write(name, &next).ok()?;
    Some(before)
}

fn after_typing(report: &Value, enter: bool, at: i64) -> Option<Value> {
    let waiting = match report["state"].as_str() {
        Some("needs_you") => true,
        Some("done") => enter,
        _ => false,
    };
    if !waiting {
        return None;
    }
    let mut next = report.clone();
    next["state"] = json!("working");
    next["at"] = json!(at);
    next["event"] = json!("typed");
    Some(next)
}

/// Records a state a session's program printed (`None`: forget it).
pub fn from_escape(name: &str, state: Option<&str>) {
    let Some(state) = state else {
        remove(name);
        return;
    };
    let conversation = read(name).and_then(|b| b["conversation"].as_str().map(str::to_string));
    let _ = write(name, &json!({ "state": state, "at": now(), "source": "escape", "conversation": conversation }));
}

const ESCAPE: &[u8] = b"\x1b]7501;";
/// Longer than this, it isn't one.
const ESCAPE_MAX: usize = 256;

/// Finds OSC 7501 in a session's output as it comes, across reads.
#[derive(Default)]
pub struct Escapes {
    carry: Vec<u8>,
}

impl Escapes {
    /// The states asked for in this output (`None`: back to guessing), in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Option<&'static str>> {
        // The usual case: nothing carried over, no escape code in it.
        if self.carry.is_empty() && !bytes.windows(4).any(|w| w == b"\x1b]75") {
            self.carry = partial_start(bytes).to_vec();
            return Vec::new();
        }
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        let mut keep = None;
        while let Some(found) = buf[at..].windows(ESCAPE.len()).position(|w| w == ESCAPE) {
            let body = at + found + ESCAPE.len();
            match buf[body..].iter().position(|&b| b == 0x07 || b == 0x1b) {
                // ESC needs its backslash after it (ST), which may come next time.
                Some(n) if buf[body + n] == 0x1b && body + n + 1 >= buf.len() => {
                    keep = Some(at + found);
                    break;
                }
                Some(n) if n <= ESCAPE_MAX => {
                    if let Some(state) = parse_escape(&String::from_utf8_lossy(&buf[body..body + n])) {
                        out.push(state);
                    }
                    at = body + n + 1;
                }
                Some(n) => at = body + n,
                None => {
                    if buf.len() - body <= ESCAPE_MAX {
                        keep = Some(at + found);
                    }
                    break;
                }
            }
        }
        self.carry = match keep {
            Some(from) => buf[from..].to_vec(),
            None => partial_start(&buf[at.min(buf.len())..]).to_vec(),
        };
        out
    }
}

/// The end of `bytes` that could be the start of an escape code.
fn partial_start(bytes: &[u8]) -> &[u8] {
    for n in (1..ESCAPE.len()).rev() {
        if bytes.len() >= n && bytes[bytes.len() - n..] == ESCAPE[..n] {
            return &bytes[bytes.len() - n..];
        }
    }
    &[]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: &str) -> Value {
        json!({ "session_id": "conv-1", "hook_event_name": event })
    }

    #[test]
    fn hook_events_become_states() {
        let state = |input: Value| from_hook("claude", &input, None, Some(10), 100, |_| true).map(|r| r["state"].as_str().unwrap_or("").to_string());
        assert_eq!(state(hook("UserPromptSubmit")).as_deref(), Some("working"));
        assert_eq!(state(hook("PreToolUse")).as_deref(), Some("working"));
        assert_eq!(state(json!({ "session_id": "c", "hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion" })).as_deref(), Some("needs_you"));
        assert_eq!(state(hook("PermissionRequest")).as_deref(), Some("needs_you"));
        assert_eq!(state(json!({ "hook_event_name": "Notification", "notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash" })).as_deref(), Some("needs_you"));
        assert_eq!(state(json!({ "hook_event_name": "Notification", "message": "Claude needs your permission to use Bash" })).as_deref(), Some("needs_you"));
        // A minute's quiet after it finished isn't a question.
        assert_eq!(state(json!({ "hook_event_name": "Notification", "notification_type": "idle_prompt", "message": "Claude is waiting for your input" })), None);
        assert_eq!(state(hook("Stop")).as_deref(), Some("done"));
        assert_eq!(state(hook("Interrupt")).as_deref(), Some("idle"));
        assert_eq!(state(json!({ "session_id": "c", "hook_event_name": "SessionStart", "source": "startup" })).as_deref(), Some("idle"));
        // A subagent's events aren't the session's.
        assert_eq!(state(json!({ "hook_event_name": "Stop", "agent_id": "a1" })), None);
        assert_eq!(state(hook("SubagentStop")), None);
        assert_eq!(state(hook("Bogus")), None);
    }

    #[test]
    fn the_conversation_is_kept_and_compaction_keeps_the_state() {
        let start = from_hook("claude", &json!({ "session_id": "conv-9", "hook_event_name": "SessionStart", "source": "startup" }), None, Some(10), 100, |_| true).unwrap();
        assert_eq!((start["conversation"].as_str(), start["agent"].as_str(), start["pid"].as_u64()), (Some("conv-9"), Some("claude"), Some(10)));
        let working = from_hook("claude", &json!({ "hook_event_name": "UserPromptSubmit" }), Some(&start), Some(10), 101, |_| true).unwrap();
        assert_eq!((working["state"].as_str(), working["conversation"].as_str()), (Some("working"), Some("conv-9")));
        let compact = from_hook("claude", &json!({ "session_id": "conv-9", "hook_event_name": "SessionStart", "source": "compact" }), Some(&working), Some(10), 102, |_| true);
        // Nothing new: the file isn't touched.
        assert!(compact.is_none_or(|c| c["state"] == "working"));
        // Another tool call while working changes nothing either.
        assert!(from_hook("claude", &hook("PostToolUse"), Some(&json!({ "state": "working", "conversation": "conv-1", "pid": 10, "source": "hook" })), Some(10), 103, |_| true).is_none());
    }

    #[test]
    fn an_agent_run_by_the_sessions_agent_doesnt_speak_for_it() {
        let owner = json!({ "state": "working", "source": "hook", "pid": 10, "conversation": "outer" });
        // `claude -p` in the outer one's Bash tool: its Stop isn't the session's.
        assert!(from_hook("claude", &hook("Stop"), Some(&owner), Some(20), 100, |_| true).is_none());
        // The outer one is gone (quit, restarted): the new one speaks.
        assert!(from_hook("claude", &hook("SessionStart"), Some(&owner), Some(20), 100, |_| false).is_some());
        assert!(from_hook("claude", &hook("Stop"), Some(&owner), Some(10), 100, |_| true).is_some());
        // An escape code doesn't own the session.
        let escaped = json!({ "state": "working", "source": "escape" });
        assert!(from_hook("codex", &hook("Stop"), Some(&escaped), Some(20), 100, |_| true).is_some());
    }

    #[test]
    fn reports_hold_until_the_session_says_otherwise() {
        let r = |state: &str, at: i64| json!({ "state": state, "at": at, "source": "hook" });
        // Working holds while it prints, not after a long quiet.
        assert_eq!(holds(&r("working", 100), 140, 0, 141), Some("working"));
        assert_eq!(holds(&r("working", 100), 100, 0, 100 + WORKING_QUIET + 1), None);
        // A question holds until someone answers and it reacts (an echo isn't that).
        assert_eq!(holds(&r("needs_you", 100), 105, 0, 200), Some("needs_you"));
        assert_eq!(holds(&r("needs_you", 100), 111, 110, 200), Some("needs_you"));
        assert_eq!(holds(&r("needs_you", 100), 115, 110, 200), None);
        assert_eq!(holds(&r("done", 100), 300, 250, 400), Some("done"));
        assert_eq!(holds(&json!({ "state": "nonsense" }), 0, 0, 0), None);
    }

    #[test]
    fn annotates_the_session_list() {
        let mut sessions = json!([
            { "name": "claude-1", "status": "running", "last_output": 100, "last_input": 0 },
            { "name": "codex-2", "status": "exited", "last_output": 100, "last_input": 0 },
            { "name": "shell-3", "status": "running", "last_output": 100, "last_input": 0 }
        ]);
        let mut reports = Map::new();
        reports.insert("claude-1".into(), json!({ "state": "needs_you", "at": 99, "source": "hook", "conversation": "c1" }));
        reports.insert("codex-2".into(), json!({ "state": "working", "at": 99, "source": "hook", "conversation": "c2" }));
        annotate(&mut sessions, &reports, 101);
        assert_eq!((sessions[0]["state"].as_str(), sessions[0]["state_source"].as_str(), sessions[0]["conversation"].as_str()), (Some("needs_you"), Some("hook"), Some("c1")));
        // An exited one keeps its conversation, not a state.
        assert_eq!((sessions[1]["state"].as_str(), sessions[1]["conversation"].as_str()), (None, Some("c2")));
        assert!(sessions[2].get("state").is_none());
    }

    #[test]
    fn finds_escape_codes_across_reads() {
        let mut e = Escapes::default();
        assert_eq!(e.feed(b"hello \x1b]7501;state=blocked\x07 world"), vec![Some("needs_you")]);
        // Split in its middle, and ended by ST.
        assert!(e.feed(b"text \x1b]75").is_empty());
        assert!(e.feed(b"01;state=wor").is_empty());
        assert_eq!(e.feed(b"king\x1b\\ more"), vec![Some("working")]);
        // ST split between ESC and the backslash.
        assert!(e.feed(b"\x1b]7501;state=done\x1b").is_empty());
        assert_eq!(e.feed(b"\\"), vec![Some("done")]);
        // Two in one read, herdr's names and ours, other fields beside it.
        assert_eq!(e.feed(b"\x1b]7501;agent=x;state=idle\x07\x1b]7501;state=needs-you\x07"), vec![Some("idle"), Some("needs_you")]);
        assert_eq!(e.feed(b"\x1b]7501;state=clear\x07"), vec![None]);
        // Not ours, or too long: nothing, and nothing kept forever.
        assert!(e.feed(b"\x1b]7502;state=done\x07\x1b]0;title\x07").is_empty());
        assert!(e.feed(&[b"\x1b]7501;".as_slice(), &[b'x'; 600]].concat()).is_empty());
        assert!(e.carry.len() < 16, "{}", e.carry.len());
        assert!(e.feed(b"plain").is_empty());
        assert!(e.carry.is_empty());
    }

    #[test]
    fn typing_into_a_waiting_agent_puts_it_to_work() {
        let done = json!({ "state": "done", "at": 10, "source": "hook", "conversation": "c1", "pid": 7 });
        // A message with Enter: working now, and the rest kept (who said it, the conversation).
        let next = after_typing(&done, true, 20).unwrap();
        assert_eq!((next["state"].as_str(), next["at"].as_i64(), next["conversation"].as_str(), next["pid"].as_u64()), (Some("working"), Some(20), Some("c1"), Some(7)));
        // Typing without Enter isn't a message yet.
        assert!(after_typing(&done, false, 20).is_none());
        // Any answer to a question is.
        let asks = json!({ "state": "needs_you", "at": 10, "source": "hook" });
        assert_eq!(after_typing(&asks, false, 20).unwrap()["state"], "working");
        // Already working, or idle: nothing to change.
        assert!(after_typing(&json!({ "state": "working", "at": 10 }), true, 20).is_none());
        assert!(after_typing(&json!({ "state": "idle", "at": 10 }), true, 20).is_none());
    }
}
