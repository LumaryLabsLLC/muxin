//! Agent sessions: terminals on fleet machines running an AI agent (Claude Code,
//! Codex, OpenCode, Pi, ...) or a plain shell.
//!
//! Sessions live in Muxin's own agent on each machine (`muxen-agent`,
//! the `agent/` crate), which Muxin installs when it connects. The agent
//! keeps them running when the window closes or the network drops, and anyone
//! can attach: the app's terminal, the CLI, or the user's own terminal. The
//! agent is the source of truth; the engine keeps the last discovery per
//! machine and refreshes it after changes and on a timer.

pub mod actions;
pub(crate) mod agent;
pub mod git;
pub mod handoff;
pub mod question;
pub mod rules;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use super::Engine;
use crate::config::MachineSpec;
use crate::ops::remote::{ps_quote, sh_quote, shell_for};
use crate::probe::{self, Shell};

/// Output this recent counts as "working" (agents animate while they think).
const BUSY_WINDOW_SECS: i64 = 6;
/// How often machines with live sessions are re-read.
pub const REFRESH_EVERY: Duration = Duration::from_secs(8);
/// Installed agent CLIs are looked up again after this long (it starts an
/// interactive shell on the machine, so not on every refresh).
const AGENTS_EVERY: chrono::Duration = chrono::Duration::minutes(10);

/// An agent CLI Muxin knows how to start.
pub struct AgentKind {
    pub id: &'static str,
    pub name: &'static str,
    /// The command to run; empty for a plain shell.
    pub command: &'static str,
    /// Accepts an initial prompt on its command line.
    pub prompt_arg: bool,
    /// The flag before that prompt (`-i` for `agy`); none when it's positional.
    pub prompt_flag: Option<&'static str>,
}

pub const AGENTS: &[AgentKind] = &[
    AgentKind { id: "claude", name: "Claude Code", command: "claude", prompt_arg: true, prompt_flag: None },
    AgentKind { id: "codex", name: "Codex", command: "codex", prompt_arg: true, prompt_flag: None },
    AgentKind { id: "opencode", name: "OpenCode", command: "opencode", prompt_arg: false, prompt_flag: None },
    AgentKind { id: "gemini", name: "Gemini CLI", command: "gemini", prompt_arg: false, prompt_flag: None },
    AgentKind { id: "agy", name: "Antigravity", command: "agy", prompt_arg: true, prompt_flag: Some("-i") },
    AgentKind { id: "amp", name: "Amp", command: "amp", prompt_arg: false, prompt_flag: None },
    AgentKind { id: "pi", name: "Pi", command: "pi", prompt_arg: true, prompt_flag: None },
    AgentKind { id: "shell", name: "Shell", command: "", prompt_arg: false, prompt_flag: None },
];

pub fn agent(id: &str) -> Option<&'static AgentKind> {
    AGENTS.iter().find(|a| a.id.eq_ignore_ascii_case(id))
}

/// An agent's command line with its first prompt, when its CLI takes one as
/// an argument; else the plain command and the prompt to type in once it's up.
pub fn launch_line(kind: &AgentKind, prompt: Option<&str>, windows: bool) -> (String, Option<String>) {
    let prompt = prompt.map(str::trim).filter(|p| !p.is_empty());
    match (kind.command, prompt) {
        ("", p) => (String::new(), p.map(str::to_string)),
        (c, Some(p)) if kind.prompt_arg => {
            let quoted = if windows { ps_quote(p) } else { sh_quote(p) };
            (kind.prompt_flag.map_or_else(|| format!("{c} {quoted}"), |f| format!("{c} {f} {quoted}")), None)
        }
        (c, p) => (c.to_string(), p.map(str::to_string)),
    }
}

/// One session, as listed in the snapshot.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// `<machine-id>/<name>`.
    pub id: String,
    pub machine_id: String,
    /// The session's name on the machine, e.g. "claude-3f9a".
    pub name: String,
    /// Agent id ("claude", "codex", "shell", ...); empty when unknown.
    pub agent: String,
    pub agent_name: String,
    pub title: String,
    pub cwd: String,
    /// "running" or "exited".
    pub status: String,
    pub exit_code: Option<i32>,
    /// What its agent is doing: "working", "needs_you" (a question or an
    /// approval waits), "done" (it finished its turn; the app shows it until
    /// the session is opened) or "idle". See `state_source` for how it's known.
    pub state: String,
    /// How `state` is known: "hook" or "escape" (the agent said so, see
    /// agent/src/states.rs), "screen" (a question Muxin knows is on its
    /// screen), "output" (a guess from its output: printing is working,
    /// quiet after working is done).
    pub state_source: String,
    /// The agent's own id for its conversation (Claude Code's or Codex's
    /// session id), from its hooks. For resuming it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// It's working (`state` is "working"). Kept for older clients.
    pub busy: bool,
    /// Unread notice ("waiting", "bell" or "exited") until the session is
    /// opened. "waiting": it went to needs_you or done.
    pub attention: Option<String>,
    /// Terminals attached right now.
    pub clients: u32,
    pub created_at: Option<DateTime<Utc>>,
    pub last_output_at: Option<DateTime<Utc>>,
    /// Paste into your own terminal to attach. Empty when it can't be (not persistent).
    pub attach_command: String,
    /// Lives in the machine's agent, so it outlives the window. False for
    /// sessions on machines without an agent (Windows, for now), which run in
    /// the app's terminal and end with it.
    pub persistent: bool,
    /// What its work changed in its folder's git checkout (see git.rs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<git::GitStatus>,
    /// The box whose agent this is (see fleet::boxes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub box_id: Option<String>,
}

/// What a session shows beside it, the most urgent last: a machine or a list
/// of sessions shows its most urgent one's (see `rollup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    Idle,
    /// It finished its turn, and nobody opened it since.
    Done,
    Working,
    /// It exited with an error, and nobody opened it since.
    Failed,
    /// A question or an approval waits (or it rang the bell).
    NeedsYou,
}

impl Mark {
    /// A few words for it: "Needs you", "Working", ...
    pub fn label(self) -> &'static str {
        match self {
            Mark::NeedsYou => "Needs you",
            Mark::Failed => "Exited",
            Mark::Working => "Working",
            Mark::Done => "Done",
            Mark::Idle => "Idle",
        }
    }
}

impl SessionInfo {
    /// What to show beside it. "Done" lasts until it's opened (its unread
    /// notice), "needs you" as long as it does.
    pub fn mark(&self) -> Mark {
        if self.status == "exited" {
            return if self.attention.is_some() && self.exit_code.is_some_and(|c| c != 0) { Mark::Failed } else { Mark::Idle };
        }
        match self.state.as_str() {
            "needs_you" => Mark::NeedsYou,
            _ if self.attention.as_deref() == Some("bell") => Mark::NeedsYou,
            "working" => Mark::Working,
            "done" if self.attention.is_some() => Mark::Done,
            _ => Mark::Idle,
        }
    }
}

/// The most urgent mark among sessions: needs you, then an error, working, done.
pub fn rollup<'a>(sessions: impl IntoIterator<Item = &'a SessionInfo>) -> Mark {
    sessions.into_iter().map(SessionInfo::mark).max().unwrap_or(Mark::Idle)
}

/// What the last discovery found on one machine.
#[derive(Debug, Clone, Default)]
pub struct MachineSessions {
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub checked_at: Option<DateTime<Utc>>,
    /// The agent's version, once it's set up there.
    pub agent_version: Option<String>,
    /// Ids of the agent CLIs found on PATH.
    pub agents: Vec<String>,
    pub agents_checked_at: Option<DateTime<Utc>>,
    /// The GitHub CLI there, looked up with the agent CLIs.
    pub gh: Option<crate::fleet::model::GhCli>,
    pub sessions: Vec<SessionInfo>,
    /// Its boxes (see fleet::boxes).
    pub boxes: Vec<crate::fleet::boxes::BoxInfo>,
    /// Recent bells and exits the agent saw, oldest first.
    pub events: Vec<RawEvent>,
    pub error: Option<String>,
    /// Muxin's hooks there, per agent CLI ("on", "off", "none" or what went
    /// wrong), as the agent last applied them; and whether that was for on.
    pub hooks: Option<Value>,
    pub hooks_sent: Option<bool>,
}

/// A bell or an exit, as the agent reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct RawEvent {
    pub at: i64,
    /// "bell" or "exited".
    pub kind: String,
    /// The session's name.
    pub session: String,
    /// Exit status for "exited".
    pub detail: String,
}

/// Something a session wants you to know: it rang the bell, went quiet after
/// working (finished, or waiting for an answer), or its agent exited.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotice {
    pub id: u64,
    pub session_id: String,
    pub machine_id: String,
    /// "waiting", "bell" or "exited".
    pub kind: String,
    pub title: String,
    pub message: String,
    pub at: DateTime<Utc>,
}

/// What `start_session` needs.
#[derive(Debug, Clone, Default)]
pub struct StartSession {
    pub machine_id: String,
    pub agent: String,
    /// Folder on the machine; "~" or empty for home.
    pub cwd: String,
    pub title: Option<String>,
    /// Initial prompt for the agent.
    pub prompt: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// Run straight over ssh in the app's terminal instead of in the agent
    /// (machines without one, Windows for now). Ends when the window closes.
    pub direct: bool,
    /// A command to run instead of the agent's own (direct sessions only).
    pub command: Option<String>,
    /// Text typed into the terminal once it's up, without pressing Enter: a
    /// command for the user to look at and run themselves.
    pub typed: Option<String>,
}

/// A session without the agent: what to run when its terminal opens.
#[derive(Debug, Clone)]
pub struct DirectSession {
    pub info: SessionInfo,
    run: String,
    windows: bool,
}

pub enum Control<'a> {
    Stop,
    Restart,
    Send { text: &'a str, enter: bool },
    Read { lines: u32 },
}

/// How to attach a terminal: a program and its arguments, run in a local PTY.
#[derive(Debug, Clone)]
pub struct AttachCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl Engine {
    /// Sets up the machine's agent if needed, then re-reads its sessions (and,
    /// now and then, which agent CLIs are installed).
    pub async fn refresh_sessions(self: &Arc<Self>, machine_id: &str) -> Result<MachineSessions, String> {
        self.discover_sessions(machine_id, false).await
    }

    /// `refresh_sessions`, looking up the installed CLIs (and the GitHub CLI's
    /// login) now rather than when they're due: a sign-in there changes them.
    pub async fn refresh_tools(self: &Arc<Self>, machine_id: &str) -> Result<MachineSessions, String> {
        self.discover_sessions(machine_id, true).await
    }

    async fn discover_sessions(self: &Arc<Self>, machine_id: &str, tools: bool) -> Result<MachineSessions, String> {
        if let Some(world) = self.demo_world() {
            let found = self.sessions_of(machine_id).unwrap_or_else(|| world.sessions(machine_id));
            self.maybe_look_at_git(machine_id, Some(&found), &found);
            return Ok(found);
        }
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if !spec.is_checkable() {
            return Err(format!("{} has no host to connect to.", spec.name));
        }
        spec.refuse_if_disconnected()?;
        let previous = self.sessions_of(&spec.id);
        // Hooks go in (or out) with the look at the installed tools: also
        // when the setting changed since this machine last had them applied.
        let choices = self.settings().agents;
        let hooks_due = previous.as_ref().is_none_or(|p| p.hooks_sent != Some(choices.hooks));
        let detect = tools || hooks_due || previous.as_ref().and_then(|p| p.agents_checked_at).is_none_or(|t| Utc::now() - t > AGENTS_EVERY);
        let agents: Vec<&str> = if detect { AGENTS.iter().map(|a| a.command).filter(|c| !c.is_empty()).collect() } else { Vec::new() };
        // The choices go with when they were made: the machine keeps the newest (agent/src/settings.rs).
        let request = json!({ "op": "discover", "agents": agents, "settings": choices.for_machines() });
        let result = match agent::ensure(&spec, probe.as_ref()).await {
            Ok(()) => agent::ctl(&spec, &request).await,
            Err(e) => Err(e),
        };
        let found = match result {
            Ok(reply) => {
                let mut found = parse_discovery(&spec, &reply);
                // Agents before 0.1.1 keep sessions whose shell exited (or whose
                // agent quit cleanly); close those like the current one does.
                let done: Vec<String> = found.sessions.iter().filter(|s| closes_on_exit(s)).map(|s| s.name.clone()).collect();
                for name in &done {
                    let _ = agent::ctl(&spec, &json!({ "op": "stop", "name": name })).await;
                }
                found.sessions.retain(|s| !done.contains(&s.name));
                if found.agents_checked_at.is_none() {
                    // Not looked up this time: keep what we knew.
                    if let Some(p) = &previous {
                        found.agents = p.agents.clone();
                        found.agents_checked_at = p.agents_checked_at;
                        found.gh = p.gh.clone();
                        found.hooks = p.hooks.clone();
                        found.hooks_sent = p.hooks_sent;
                    }
                } else {
                    // What the machine went by (agents before 0.6 ignore it all).
                    found.hooks_sent = Some(reply["settings"]["hooks"]["on"].as_bool().unwrap_or(choices.hooks));
                }
                // A newer choice made in another Muxin: this one takes it.
                if reply["settings"].is_object() {
                    self.adopt_agent_settings(&reply["settings"]);
                }
                self.guess_states(&spec, previous.as_ref(), &mut found).await;
                found
            }
            Err(e) => MachineSessions {
                checked_at: Some(Utc::now()),
                error: Some(e),
                // Keep what we knew; the machine may just be briefly unreachable.
                ..previous.clone().unwrap_or_default()
            },
        };
        self.set_sessions(&spec.id, found.clone());
        if found.error.is_none() {
            self.maybe_look_at_git(&spec.id, previous.as_ref(), &found);
        }
        // Working now: look again once it should have gone quiet, rather than
        // only trusting the agent to say so.
        if found.sessions.iter().any(|s| s.busy) {
            self.settle_sessions(&spec.id);
        }
        Ok(found)
    }

    /// The states of sessions whose agent doesn't say (no hooks, an older
    /// agent): one that went quiet after working is done, or needs you when a
    /// question Muxin knows is on its screen (question.rs). It stays so until
    /// it works again.
    async fn guess_states(&self, spec: &MachineSpec, previous: Option<&MachineSessions>, found: &mut MachineSessions) {
        for s in found.sessions.iter_mut().filter(|s| s.state_source == "output" && s.status == "running" && s.state == "idle") {
            let Some(b) = previous.and_then(|p| p.sessions.iter().find(|b| b.name == s.name)) else { continue };
            if s.agent.is_empty() || s.agent == "shell" {
                continue;
            }
            let guessed = matches!(b.state.as_str(), "needs_you" | "done") && matches!(b.state_source.as_str(), "output" | "screen");
            let printed = b.last_output_at != s.last_output_at;
            // Quiet as it was: as it was.
            if guessed && !printed {
                (s.state, s.state_source) = (b.state.clone(), b.state_source.clone());
                continue;
            }
            // Working, then quiet (or it printed since the last guess): its screen says which.
            if b.state == "working" || guessed {
                let read = agent::ctl(spec, &json!({ "op": "read", "name": s.name, "lines": 60 })).await;
                let screen = read.ok().and_then(|r| r["text"].as_str().map(str::to_string)).unwrap_or_default();
                let asks = !screen.is_empty() && question::read(&screen).options.len() >= 2;
                (s.state, s.state_source) = if asks { ("needs_you".into(), "screen".into()) } else { ("done".into(), "output".into()) };
            }
        }
    }

    /// One more look at a machine's sessions a moment after a busy one should
    /// have gone quiet (one pending look per machine).
    fn settle_sessions(self: &Arc<Self>, machine_id: &str) {
        static PENDING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
        let pending = PENDING.get_or_init(Default::default);
        if !pending.lock().unwrap().insert(machine_id.to_string()) {
            return;
        }
        let (me, id) = (self.clone(), machine_id.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(BUSY_WINDOW_SECS as u64 + 1)).await;
            pending.lock().unwrap().remove(&id);
            let _ = me.refresh_sessions(&id).await;
        });
    }

    /// Starts an agent (or shell) in a new session and returns it.
    pub async fn start_session(self: &Arc<Self>, req: StartSession) -> Result<SessionInfo, String> {
        let kind = agent(&req.agent).ok_or_else(|| {
            format!("Unknown agent \"{}\". Use one of: {}.", req.agent, AGENTS.iter().map(|a| a.id).collect::<Vec<_>>().join(", "))
        })?;
        let (spec, probe) = self.machine(&req.machine_id).ok_or_else(|| format!("No machine called {}.", req.machine_id))?;
        let name = format!("{}-{}", kind.id, super::workspaces::random_id(4));
        let cwd = if req.cwd.trim().is_empty() { "~".to_string() } else { req.cwd.trim().to_string() };
        let title = req.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| default_title(kind, &cwd));
        // "Shell · home", "Shell · home 2", ... so sessions on a machine are told apart
        // (knowing which it has: the CLI starts without having asked).
        if self.sessions_of(&spec.id).is_none_or(|f| f.checked_at.is_none()) && self.demo_world().is_none() {
            let _ = self.refresh_sessions(&spec.id).await;
        }
        let taken: Vec<String> = self.sessions_of(&spec.id).map(|f| f.sessions.into_iter().map(|s| s.title).collect()).unwrap_or_default();
        let title = unique_title(&title, &taken);

        let windows = matches!(shell_for(&spec, probe.as_ref()), Shell::Powershell);
        if req.direct {
            return Ok(self.start_direct(&spec, kind, name, cwd, title, &req, windows));
        }
        if let Some(world) = self.demo_world() {
            let info = world.start_session(&spec, kind, &name, &cwd, &title);
            let mut found = self.sessions_of(&spec.id).unwrap_or_else(|| world.sessions(&spec.id));
            found.sessions.insert(0, info.clone());
            self.set_sessions(&spec.id, found);
            return Ok(info);
        }
        agent::ensure(&spec, probe.as_ref()).await?;

        // The agent's command line; the prompt rides along when the CLI takes one.
        let prompt = req.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty());
        let run_line = match &req.command {
            Some(c) => c.clone(),
            None => launch_line(kind, prompt, windows).0,
        };
        let request = json!({
            "op": "start",
            "name": name,
            "agent": kind.id,
            "title": title,
            "cwd": cwd,
            "run": run_line,
            "cols": req.cols.clamp(40, 500),
            "rows": req.rows.clamp(10, 200),
        });
        agent::ctl(&spec, &request).await?;

        // A CLI without a prompt argument gets the prompt typed in once it's up.
        if let (Some(p), false) = (prompt, kind.prompt_arg || kind.command.is_empty()) {
            let me = self.clone();
            let id = format!("{}/{name}", spec.id);
            let p = p.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let _ = me.control_session(&id, Control::Send { text: &p, enter: true }).await;
            });
        }

        // Typed in for the user to run (Enter is theirs to press).
        if let Some(t) = req.typed.clone().filter(|t| !t.trim().is_empty()) {
            let me = self.clone();
            let id = format!("{}/{name}", spec.id);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let _ = me.control_session(&id, Control::Send { text: &t, enter: false }).await;
            });
        }

        // A missing CLI or a broken login shell fails within moments.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let found = self.refresh_sessions(&spec.id).await?;
        let session = found.sessions.into_iter().find(|s| s.name == name);
        match session {
            Some(s) if s.status == "running" => Ok(s),
            exited => {
                let id = format!("{}/{name}", spec.id);
                let output = self.control_session(&id, Control::Read { lines: 20 }).await.unwrap_or_default();
                let _ = self.control_session(&id, Control::Stop).await;
                let code = exited.and_then(|s| s.exit_code);
                let why = match code {
                    Some(127) => format!("{} isn't installed on {} (or isn't on its PATH).", kind.name, spec.name),
                    Some(c) => format!("{} exited right away with code {c} on {}.", kind.name, spec.name),
                    None => format!("{} exited right away on {}.", kind.name, spec.name),
                };
                let last = output.lines().rev().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("[exited with code")).unwrap_or("");
                Err(if last.is_empty() { why } else { format!("{why} It said: {last}") })
            }
        }
    }

    /// Stops, restarts, types into or reads a session. `Read` returns the screen
    /// and scrollback text; the others return an empty string.
    /// What a waiting session is asking, read off its screen: a choice, or
    /// the end of what it said (see question.rs).
    pub async fn session_question(self: &Arc<Self>, id: &str) -> Result<question::Question, String> {
        let screen = self.control_session(id, Control::Read { lines: 60 }).await?;
        Ok(question::read(&screen))
    }

    /// Picks one of a waiting session's choices (`key`, as the question gave
    /// it; `selected`, the one its cursor was on; `confirm`, the menu wants
    /// Enter after a number), and marks it read.
    pub async fn answer_session(self: &Arc<Self>, id: &str, key: &str, selected: Option<&str>, confirm: bool) -> Result<(), String> {
        let (keys, enter) = question::keys_for(key, selected, confirm);
        self.control_session(id, Control::Send { text: &keys, enter }).await?;
        self.ack_session(id);
        Ok(())
    }

    pub async fn control_session(self: &Arc<Self>, id: &str, action: Control<'_>) -> Result<String, String> {
        if self.direct_session(id).is_some() {
            return match action {
                Control::Stop => {
                    self.remove_direct(id);
                    Ok(String::new())
                }
                Control::Restart => {
                    self.set_direct_status(id, "running");
                    Ok(String::new())
                }
                _ => Err(format!("{id} runs only in the Muxin window, so it can't be read or typed into from here.")),
            };
        }
        let (machine_id, name) = split_id(id)?;
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if let Some(world) = self.demo_world() {
            let text = world.control_session(&spec.id, name, &action);
            if matches!(action, Control::Stop) {
                if let Some(mut found) = self.sessions_of(&spec.id) {
                    found.sessions.retain(|s| s.name != name);
                    self.set_sessions(&spec.id, found);
                }
            }
            return text;
        }
        agent::ensure(&spec, probe.as_ref()).await?;
        let request = match &action {
            Control::Stop => json!({ "op": "stop", "name": name }),
            Control::Restart => json!({ "op": "restart", "name": name }),
            Control::Send { text, enter } => json!({ "op": "send", "name": name, "text": text, "enter": enter }),
            Control::Read { lines } => json!({ "op": "read", "name": name, "lines": lines.clamp(&1, &50_000) }),
        };
        let reply = agent::ctl(&spec, &request).await?;
        if !matches!(action, Control::Read { .. }) {
            let me = self.clone();
            let machine = spec.id.clone();
            tokio::spawn(async move {
                // Give the agent a moment to react before re-reading.
                tokio::time::sleep(Duration::from_millis(400)).await;
                let _ = me.refresh_sessions(&machine).await;
            });
        }
        Ok(reply["text"].as_str().unwrap_or("").to_string())
    }

    /// Gives a session a new title (kept by the agent on its machine, so every
    /// Muxin sees it). An empty title goes back to the one it started with.
    pub async fn rename_session(self: &Arc<Self>, id: &str, title: &str) -> Result<SessionInfo, String> {
        let title = clean_title(title)?;
        if let Some(d) = self.direct_session(id) {
            let title = if title.is_empty() { agent(&d.info.agent).map(|k| default_title(k, &d.info.cwd)).unwrap_or_else(|| d.info.name.clone()) } else { title };
            return self.set_direct_title(id, &title).ok_or_else(|| format!("No session {id}."));
        }
        let (machine_id, name) = split_id(id)?;
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            let mut found = self.sessions_of(&spec.id).ok_or_else(|| format!("No session {id}."))?;
            let s = found.sessions.iter_mut().find(|s| s.name == name).ok_or_else(|| format!("No session {id}."))?;
            s.title = if title.is_empty() { agent(&s.agent).map(|k| default_title(k, &s.cwd)).unwrap_or_else(|| s.name.clone()) } else { title };
            let renamed = s.clone();
            self.set_sessions(&spec.id, found);
            return Ok(renamed);
        }
        agent::ensure(&spec, probe.as_ref()).await?;
        agent::ctl(&spec, &json!({ "op": "rename", "name": name, "title": title })).await?;
        let found = self.refresh_sessions(&spec.id).await?;
        found.sessions.into_iter().find(|s| s.name == name).ok_or_else(|| format!("{id} ended."))
    }

    /// The command that attaches a terminal to a session from this computer.
    pub fn attach_command(&self, id: &str) -> Result<AttachCommand, String> {
        let (machine_id, name) = split_id(id)?;
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if let Some(d) = self.direct_session(id) {
            return Ok(direct_command(&spec, &d));
        }
        if spec.is_local() {
            let home = std::env::var("HOME").unwrap_or_default();
            let program = agent::REMOTE_PATH.replacen('~', &home, 1);
            return Ok(AttachCommand { program, args: vec!["attach".into(), name.into(), "--app".into()] });
        }
        // Same connection options as checks, but with a terminal.
        let mut args: Vec<String> = probe::ssh_args(&spec).into_iter().map(|a| if a == "-T" { "-tt".into() } else { a }).collect();
        args.push("--".into());
        args.push(if agent::is_windows(&spec) { agent::command(&spec, &format!("attach {name} --app")) } else { format!("{} attach {} --app", agent::REMOTE_PATH, sh_quote(name)) });
        Ok(AttachCommand { program: "ssh".into(), args })
    }

    fn start_direct(&self, spec: &MachineSpec, kind: &AgentKind, name: String, cwd: String, title: String, req: &StartSession, windows: bool) -> SessionInfo {
        let prompt = req.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty());
        let run = match &req.command {
            Some(c) => c.clone(),
            None => launch_line(kind, prompt, windows).0,
        };
        let info = SessionInfo {
            id: format!("{}/{name}", spec.id),
            machine_id: spec.id.clone(),
            agent: kind.id.to_string(),
            agent_name: kind.name.to_string(),
            title,
            cwd,
            status: "running".into(),
            exit_code: None,
            state: "idle".into(),
            state_source: "output".into(),
            conversation_id: None,
            busy: false,
            attention: None,
            clients: 1,
            created_at: Some(Utc::now()),
            last_output_at: None,
            attach_command: String::new(),
            persistent: false,
            git: None,
            box_id: None,
            name,
        };
        self.add_direct(DirectSession { info: info.clone(), run, windows });
        info
    }

    /// Machines whose sessions should be re-read on the timer.
    /// (And with a box that isn't paused: it pauses on its machine, with no
    /// session left to say so.)
    pub(crate) fn machines_with_sessions(&self) -> Vec<String> {
        self.all_sessions().into_iter().filter(|(_, m)| !m.sessions.is_empty() || m.boxes.iter().any(|b| b.state != "paused")).map(|(id, _)| id).collect()
    }
}

/// Notices from a new discovery: events logged since `since`, and sessions
/// that came to need you or finished. `before` is the previous discovery.
pub(crate) fn new_notices(before: &MachineSessions, after: &MachineSessions, since: i64, machine: &str) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    let find = |name: &str| after.sessions.iter().chain(before.sessions.iter()).find(|s| s.name == name);
    for e in after.events.iter().filter(|e| e.at > since) {
        let Some(s) = find(&e.session) else { continue };
        let who = if s.agent_name.is_empty() { "The session".to_string() } else { s.agent_name.clone() };
        let (title, kind) = match e.kind.as_str() {
            "bell" => (format!("{who} needs you"), "bell"),
            "exited" => match e.detail.as_str() {
                "" | "0" => (format!("{who} exited"), "exited"),
                code => (format!("{who} exited with code {code}"), "exited"),
            },
            _ => continue,
        };
        out.push((s.id.clone(), kind.to_string(), title, format!("{} · {machine}", s.title)));
    }
    // It came to need you, or finished its turn (a shell only when it says so).
    for s in after.sessions.iter().filter(|s| s.status == "running") {
        let Some(b) = before.sessions.iter().find(|b| b.name == s.name) else { continue };
        let said = matches!(s.state_source.as_str(), "hook" | "escape");
        let who = if s.agent_name.is_empty() || (s.agent == "shell" && said) { "The session".to_string() } else { s.agent_name.clone() };
        if s.state == b.state || (s.agent == "shell" && !said) {
            continue;
        }
        let title = match s.state.as_str() {
            "needs_you" => format!("{who} needs you"),
            "done" => format!("{who} is done"),
            _ => continue,
        };
        out.push((s.id.clone(), "waiting".into(), title, format!("{} · {machine}", s.title)));
    }
    out
}

/// What a direct session's terminal runs: the agent in the user's login shell,
/// in its folder, over ssh (or locally).
fn direct_command(spec: &MachineSpec, d: &DirectSession) -> AttachCommand {
    let cwd = d.info.cwd.trim();
    if d.windows {
        let dir = match cwd {
            "" | "~" => "$HOME".to_string(),
            c => ps_quote(c),
        };
        let script = if d.run.is_empty() {
            format!("powershell -NoLogo -NoExit -Command \"Set-Location {dir}\"")
        } else {
            format!("powershell -NoLogo -Command \"Set-Location {dir}; {}\"", d.run.replace('"', "\\\""))
        };
        // This computer: PowerShell itself.
        if spec.is_local() {
            let mut args = vec!["-NoLogo".to_string()];
            if d.run.is_empty() {
                args.extend(["-NoExit".into(), "-Command".into(), format!("Set-Location {dir}")]);
            } else {
                args.extend(["-Command".into(), format!("Set-Location {dir}; {}", d.run)]);
            }
            return AttachCommand { program: "powershell".into(), args };
        }
        let mut args: Vec<String> = probe::ssh_args(spec).into_iter().map(|a| if a == "-T" { "-tt".into() } else { a }).collect();
        args.push("--".into());
        args.push(script);
        return AttachCommand { program: "ssh".into(), args };
    }
    let cd = match cwd {
        "" | "~" => "cd \"$HOME\"".to_string(),
        c if c.starts_with("~/") => format!("cd \"$HOME\"/{}", sh_quote(&c[2..])),
        c => format!("cd {}", sh_quote(c)),
    };
    let shell = "\"${SHELL:-/bin/sh}\"";
    let run = if d.run.is_empty() { format!("exec {shell} -l") } else { format!("exec {shell} -l -i -c {}", sh_quote(&d.run)) };
    let script = format!("{cd} || {{ echo \"No folder {cwd} on this machine.\"; exit 1; }}; {run}");
    if spec.is_local() {
        return AttachCommand { program: "/bin/sh".into(), args: vec!["-c".into(), script] };
    }
    let mut args: Vec<String> = probe::ssh_args(spec).into_iter().map(|a| if a == "-T" { "-tt".into() } else { a }).collect();
    args.push("--".into());
    args.push(format!("sh -c {}", sh_quote(&script)));
    AttachCommand { program: "ssh".into(), args }
}

/// An exited session that should just go away: a shell, or an agent that quit cleanly.
fn closes_on_exit(s: &SessionInfo) -> bool {
    s.status == "exited" && (s.agent == "shell" || s.exit_code == Some(0))
}

#[cfg(test)]
#[test]
fn titles_name_the_folder() {
    let shell = agent("shell").unwrap();
    for (cwd, title) in [("~", "Shell · home"), ("/home/adib", "Shell · home"), ("/Users/adib/", "Shell · home"), ("/root", "Shell · home"), ("~/code/api", "Shell · api"), ("/home/adib/openrunner", "Shell · openrunner")] {
        assert_eq!(default_title(shell, cwd), title, "{cwd}");
    }
}

/// `title`, or `title 2`, `title 3`, ... when a session already has it.
fn unique_title(title: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == title) {
        return title.to_string();
    }
    (2..).map(|n| format!("{title} {n}")).find(|t| !taken.contains(t)).unwrap_or_else(|| title.to_string())
}

/// A title as the user typed it, on one line. Empty is allowed (it means "the
/// one it started with").
fn clean_title(title: &str) -> Result<String, String> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.chars().count() > 80 || title.chars().any(char::is_control) {
        return Err("That title is too long or has odd characters.".into());
    }
    Ok(title)
}

/// A short default title: the agent and the folder it works in.
fn default_title(kind: &AgentKind, cwd: &str) -> String {
    let path = cwd.trim_end_matches('/');
    // The home folder, however it's written: "~", /home/adib, /Users/adib, /root.
    let parts: Vec<&str> = path.split('/').collect();
    let home = path.is_empty() || path == "~" || path == "/root" || matches!(parts[..], ["", "home" | "Users", user] if !user.is_empty());
    let folder = if home { "home" } else { path.rsplit('/').next().filter(|f| !f.is_empty()).unwrap_or("home") };
    format!("{} · {folder}", kind.name)
}

fn split_id(id: &str) -> Result<(&str, &str), String> {
    id.split_once('/')
        .filter(|(m, n)| !m.is_empty() && !n.is_empty())
        .ok_or_else(|| format!("\"{id}\" isn't a session id; they look like <machine-id>/<name>, e.g. beelink/claude-3f9a."))
}

/// The paste-into-your-terminal attach command (detach with Ctrl-] then d).
fn user_attach_command(spec: &MachineSpec, name: &str) -> String {
    let attach = if agent::is_windows(spec) { agent::command(spec, &format!("attach {name}")) } else { format!("{} attach {name}", agent::REMOTE_PATH) };
    if spec.is_local() {
        return attach;
    }
    let mut ssh = String::from("ssh -t");
    if let Some(port) = spec.port {
        ssh.push_str(&format!(" -p {port}"));
    }
    let host = spec.host.trim();
    match spec.user.as_deref().filter(|u| !u.is_empty()) {
        Some(user) if !host.contains('@') => ssh.push_str(&format!(" {user}@{host}")),
        _ => ssh.push_str(&format!(" {host}")),
    }
    format!("{ssh} {attach}")
}

/// The agent's `discover` reply as what the engine keeps.
pub(crate) fn parse_discovery(spec: &MachineSpec, reply: &Value) -> MachineSessions {
    let now = reply["now"].as_i64().unwrap_or_else(|| Utc::now().timestamp());
    let ts = |v: &Value| v.as_i64().filter(|t| *t > 0).and_then(|t| Utc.timestamp_opt(t, 0).single());
    let mut found = MachineSessions {
        checked_at: Some(Utc::now()),
        agent_version: reply["version"].as_str().map(str::to_string),
        agents: reply["agents"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).filter_map(|c| AGENTS.iter().find(|k| k.command == c)).map(|k| k.id.to_string()).collect())
            .unwrap_or_default(),
        agents_checked_at: reply["agents_checked"].as_bool().unwrap_or(false).then(Utc::now),
        gh: reply["gh"].is_object().then(|| crate::fleet::model::GhCli {
            installed: reply["gh"]["installed"].as_bool().unwrap_or(false),
            logged_in: reply["gh"]["logged_in"].as_bool().unwrap_or(false),
            account: reply["gh"]["account"].as_str().map(str::to_string),
            path: reply["gh"]["path"].as_str().map(str::to_string),
        }),
        boxes: crate::fleet::boxes::parse(&spec.id, reply),
        hooks: reply.get("hooks").filter(|h| h.is_object()).cloned(),
        ..Default::default()
    };
    for s in reply["sessions"].as_array().into_iter().flatten() {
        let name = s["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let kind = s["agent"].as_str().and_then(agent).or_else(|| name.split('-').next().and_then(agent));
        let running = s["status"].as_str() == Some("running");
        let last_output = s["last_output"].as_i64();
        // The same rule as the agent's (its "sessions" event comes when this turns false).
        let printing = running && last_output.is_some_and(|a| now - a < BUSY_WINDOW_SECS);
        // What its agent says (agents from 0.6, with hooks), else what its output says.
        let reported = s["state"].as_str().filter(|st| running && ["working", "needs_you", "done", "idle"].contains(st));
        let (state, state_source) = match reported {
            Some(st) => (st.to_string(), s["state_source"].as_str().filter(|x| !x.is_empty()).unwrap_or("hook").to_string()),
            None => (if printing { "working" } else { "idle" }.to_string(), "output".to_string()),
        };
        found.sessions.push(SessionInfo {
            id: format!("{}/{name}", spec.id),
            machine_id: spec.id.clone(),
            agent: kind.map(|k| k.id.to_string()).unwrap_or_default(),
            agent_name: kind.map(|k| k.name.to_string()).unwrap_or_default(),
            title: s["title"].as_str().filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(|| name.clone()),
            cwd: s["cwd"].as_str().unwrap_or("").to_string(),
            status: if running { "running".into() } else { "exited".into() },
            exit_code: s["exit_code"].as_i64().map(|c| c as i32),
            busy: state == "working",
            state,
            state_source,
            conversation_id: s["conversation"].as_str().filter(|c| !c.is_empty()).map(str::to_string),
            attention: None,
            clients: s["clients"].as_u64().unwrap_or(0) as u32,
            created_at: ts(&s["created"]),
            last_output_at: last_output.and_then(|t| Utc.timestamp_opt(t, 0).single()),
            attach_command: user_attach_command(spec, &name),
            persistent: true,
            git: None,
            box_id: crate::fleet::boxes::box_of_session(&name, &found.boxes).map(|b| b.id.clone()),
            name,
        });
    }
    for e in reply["events"].as_array().into_iter().flatten() {
        if let (Some(at), Some(kind), Some(session)) = (e["at"].as_i64(), e["kind"].as_str(), e["session"].as_str()) {
            found.events.push(RawEvent { at, kind: kind.into(), session: session.into(), detail: e["detail"].as_str().unwrap_or("").into() });
        }
    }
    // Newest first.
    found.sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(host: &str) -> MachineSpec {
        let mut s: MachineSpec = toml::from_str(&format!("name = \"Beelink\"\nhost = \"{host}\"")).unwrap();
        s.id = "beelink".into();
        s
    }

    fn reply(now: i64, claude_output: i64, events: Value) -> Value {
        json!({
            "ok": true, "version": "0.1.0", "agents": ["claude", "codex"], "agents_checked": true, "now": now,
            "sessions": [
                { "name": "claude-ab12", "agent": "claude", "title": "Claude Code · app", "cwd": "/home/adib/app",
                  "created": 900, "status": "running", "exit_code": null, "last_output": claude_output, "clients": 1 },
                { "name": "shell-cd34", "agent": "shell", "title": "", "cwd": "/home/adib",
                  "created": 950, "status": "exited", "exit_code": 2, "last_output": 960, "clients": 0 }
            ],
            "events": events,
        })
    }

    #[test]
    fn parses_discovery() {
        let found = parse_discovery(&spec("beelink"), &reply(1000, 997, json!([])));
        assert_eq!(found.agent_version.as_deref(), Some("0.1.0"));
        assert_eq!(found.agents, vec!["claude", "codex"]);
        assert!(found.agents_checked_at.is_some());
        assert_eq!(found.sessions.len(), 2);
        let shell = &found.sessions[0];
        assert_eq!(shell.id, "beelink/shell-cd34");
        assert_eq!((shell.status.as_str(), shell.exit_code), ("exited", Some(2)));
        assert_eq!(shell.title, "shell-cd34");
        let claude = &found.sessions[1];
        assert_eq!(claude.agent_name, "Claude Code");
        assert!(claude.busy && claude.persistent);
        assert_eq!(claude.clients, 1);
        assert_eq!(claude.attach_command, "ssh -t beelink ~/.muxen/bin/muxen-agent attach claude-ab12");
    }

    #[test]
    fn notices_from_events_and_quiet_agents() {
        let before = parse_discovery(&spec("beelink"), &reply(1000, 998, json!([{ "at": 950, "kind": "bell", "session": "claude-ab12", "detail": "" }])));
        let after = parse_discovery(
            &spec("beelink"),
            &reply(1100, 1000, json!([
                { "at": 950, "kind": "bell", "session": "claude-ab12", "detail": "" },
                { "at": 1050, "kind": "exited", "session": "shell-cd34", "detail": "2" }
            ])),
        );
        // Working, then quiet: the guess (guess_states) says it's done.
        assert_eq!((before.sessions[1].state.as_str(), after.sessions[1].state.as_str()), ("working", "idle"));
        let mut after = after;
        after.sessions[1].state = "done".into();
        let notices = new_notices(&before, &after, 950, "Beelink");
        let kinds: Vec<(&str, &str)> = notices.iter().map(|(id, kind, _, _)| (id.as_str(), kind.as_str())).collect();
        // The old bell doesn't repeat; the exit is new; Claude finished.
        assert_eq!(kinds, vec![("beelink/shell-cd34", "exited"), ("beelink/claude-ab12", "waiting")]);
        assert_eq!(notices[0].2, "Shell exited with code 2");
        assert_eq!(notices[1].2, "Claude Code is done");
        assert_eq!(notices[1].3, "Claude Code · app · Beelink");
        // Once: still done is no news.
        assert!(new_notices(&after, &after, 1050, "Beelink").is_empty());
    }

    #[test]
    fn what_agents_report_is_their_state() {
        let mut r = reply(1000, 900, json!([]));
        r["sessions"][0]["state"] = json!("needs_you");
        r["sessions"][0]["state_source"] = json!("hook");
        r["sessions"][0]["conversation"] = json!("5f0c-conv");
        let found = parse_discovery(&spec("beelink"), &r);
        let claude = &found.sessions[1];
        assert_eq!((claude.state.as_str(), claude.state_source.as_str(), claude.busy), ("needs_you", "hook", false));
        assert_eq!(claude.conversation_id.as_deref(), Some("5f0c-conv"));
        // Without a report (an older agent, no hooks): the output says.
        let busy = parse_discovery(&spec("beelink"), &reply(1000, 999, json!([])));
        assert_eq!((busy.sessions[1].state.as_str(), busy.sessions[1].state_source.as_str(), busy.sessions[1].busy), ("working", "output", true));
        // A state on an exited session means nothing.
        let mut gone = reply(1000, 900, json!([]));
        gone["sessions"][1]["state"] = json!("working");
        assert_eq!(parse_discovery(&spec("beelink"), &gone).sessions[0].state, "idle");
        // Needing you is news; so is a hook-reporting shell finishing (a program in it said).
        let before = parse_discovery(&spec("beelink"), &reply(1000, 999, json!([])));
        let notices = new_notices(&before, &found, 1000, "Beelink");
        assert_eq!(notices.iter().map(|n| n.2.as_str()).collect::<Vec<_>>(), vec!["Claude Code needs you"]);
    }

    #[test]
    fn marks_and_their_rollup() {
        let s = |state: &str, attention: Option<&str>| SessionInfo { status: "running".into(), state: state.into(), attention: attention.map(str::to_string), ..Default::default() };
        assert_eq!(s("working", None).mark(), Mark::Working);
        assert_eq!(s("needs_you", None).mark(), Mark::NeedsYou, "needs you as long as it does, opened or not");
        assert_eq!(s("idle", Some("bell")).mark(), Mark::NeedsYou);
        assert_eq!(s("done", Some("waiting")).mark(), Mark::Done);
        assert_eq!(s("done", None).mark(), Mark::Idle, "done until it's opened");
        let failed = SessionInfo { status: "exited".into(), exit_code: Some(1), attention: Some("exited".into()), ..Default::default() };
        assert_eq!(failed.mark(), Mark::Failed);
        let all = [s("done", Some("waiting")), s("working", None), s("idle", None)];
        assert_eq!(rollup(&all), Mark::Working);
        assert_eq!(rollup(&[s("done", Some("waiting")), failed.clone(), s("needs_you", None)]), Mark::NeedsYou);
        assert_eq!(rollup(&[failed, s("working", None)]), Mark::Failed);
        assert_eq!(rollup(&[]), Mark::Idle);
    }

    #[test]
    fn titles_and_ids() {
        assert_eq!(default_title(agent("claude").unwrap(), "~/code/api/"), "Claude Code · api");
        assert_eq!(default_title(agent("agy").unwrap(), "~/code/api/"), "Antigravity · api");
        // On the user's own machines an agent asks before it acts: only a box's container turns that off.
        assert_eq!(launch_line(agent("agy").unwrap(), None, false).0, "agy");
        assert_eq!(launch_line(agent("agy").unwrap(), Some("fix tests"), false).0, "agy -i 'fix tests'");
        assert_eq!(launch_line(agent("claude").unwrap(), Some("fix tests"), false).0, "claude 'fix tests'");
        assert_eq!(default_title(agent("shell").unwrap(), "~"), "Shell · home");
        assert!(split_id("beelink/claude-1").is_ok());
        assert!(split_id("claude-1").is_err());
        assert_eq!(user_attach_command(&spec("local"), "x"), "~/.muxen/bin/muxen-agent attach x");
        let taken = vec!["Shell · home".to_string(), "Shell · home 2".to_string()];
        assert_eq!(unique_title("Shell · home", &taken), "Shell · home 3");
        assert_eq!(unique_title("Shell · tmp", &taken), "Shell · tmp");
        assert_eq!(clean_title("  API\n server ").unwrap(), "API server");
        assert_eq!(clean_title("   ").unwrap(), "");
        assert!(clean_title(&"x".repeat(81)).is_err());
    }
}
