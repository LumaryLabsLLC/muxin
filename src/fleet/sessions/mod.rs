//! Agent sessions: terminals on fleet machines running an AI agent (Claude Code,
//! Codex, OpenCode, Pi, ...) or a plain shell.
//!
//! Sessions live in Muxen's own agent on each machine (`muxen-agent`,
//! the `agent/` crate), which Muxen installs when it connects. The agent
//! keeps them running when the window closes or the network drops, and anyone
//! can attach: the app's terminal, the CLI, or the user's own terminal. The
//! agent is the source of truth; the engine keeps the last discovery per
//! machine and refreshes it after changes and on a timer.

pub(crate) mod agent;

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

/// An agent CLI Muxen knows how to start.
pub struct AgentKind {
    pub id: &'static str,
    pub name: &'static str,
    /// The command to run; empty for a plain shell.
    pub command: &'static str,
    /// Accepts an initial prompt as its first argument.
    pub prompt_arg: bool,
}

pub const AGENTS: &[AgentKind] = &[
    AgentKind { id: "claude", name: "Claude Code", command: "claude", prompt_arg: true },
    AgentKind { id: "codex", name: "Codex", command: "codex", prompt_arg: true },
    AgentKind { id: "opencode", name: "OpenCode", command: "opencode", prompt_arg: false },
    AgentKind { id: "gemini", name: "Gemini CLI", command: "gemini", prompt_arg: false },
    AgentKind { id: "amp", name: "Amp", command: "amp", prompt_arg: false },
    AgentKind { id: "pi", name: "Pi", command: "pi", prompt_arg: true },
    AgentKind { id: "shell", name: "Shell", command: "", prompt_arg: false },
];

pub fn agent(id: &str) -> Option<&'static AgentKind> {
    AGENTS.iter().find(|a| a.id.eq_ignore_ascii_case(id))
}

/// One session, as listed in the snapshot.
#[derive(Debug, Clone, Serialize)]
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
    /// Printed output within the last few seconds of the last check: the agent is working.
    pub busy: bool,
    /// Unread notice ("waiting", "bell" or "exited") until the session is opened.
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
    /// Recent bells and exits the agent saw, oldest first.
    pub events: Vec<RawEvent>,
    pub error: Option<String>,
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
        if let Some(world) = self.demo_world() {
            return Ok(self.sessions_of(machine_id).unwrap_or_else(|| world.sessions(machine_id)));
        }
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if !spec.is_checkable() {
            return Err(format!("{} has no host to connect to.", spec.name));
        }
        if matches!(shell_for(&spec, probe.as_ref()), Shell::Powershell) {
            let found = MachineSessions {
                checked_at: Some(Utc::now()),
                error: Some("Sessions on Windows run while Muxen is open; they can't keep running in the background yet.".into()),
                ..Default::default()
            };
            self.set_sessions(&spec.id, found.clone());
            return Ok(found);
        }
        let previous = self.sessions_of(&spec.id);
        let detect = previous.as_ref().and_then(|p| p.agents_checked_at).is_none_or(|t| Utc::now() - t > AGENTS_EVERY);
        let agents: Vec<&str> = if detect { AGENTS.iter().map(|a| a.command).filter(|c| !c.is_empty()).collect() } else { Vec::new() };
        let result = match agent::ensure(&spec, probe.as_ref()).await {
            Ok(()) => agent::ctl(&spec, &json!({ "op": "discover", "agents": agents })).await,
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
                    }
                }
                found
            }
            Err(e) => MachineSessions {
                checked_at: Some(Utc::now()),
                error: Some(e),
                // Keep what we knew; the machine may just be briefly unreachable.
                ..previous.unwrap_or_default()
            },
        };
        self.set_sessions(&spec.id, found.clone());
        // Working now: look again once it should have gone quiet, rather than
        // only trusting the agent to say so.
        if found.sessions.iter().any(|s| s.busy) {
            self.settle_sessions(&spec.id);
        }
        Ok(found)
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
        // "Shell · home", "Shell · home 2", ... so sessions on a machine are told apart.
        let taken: Vec<String> = self.sessions_of(&spec.id).map(|f| f.sessions.into_iter().map(|s| s.title).collect()).unwrap_or_default();
        let title = unique_title(&title, &taken);

        let windows = matches!(shell_for(&spec, probe.as_ref()), Shell::Powershell);
        if req.direct || windows {
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
        let run_line = match (&req.command, kind.command, prompt) {
            (Some(c), _, _) => c.clone(),
            (None, "", _) => String::new(),
            (None, c, Some(p)) if kind.prompt_arg => format!("{c} {}", sh_quote(p)),
            (None, c, _) => c.to_string(),
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
                _ => Err(format!("{id} runs only in the Muxen window, so it can't be read or typed into from here.")),
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
        args.push(format!("{} attach {} --app", agent::REMOTE_PATH, sh_quote(name)));
        Ok(AttachCommand { program: "ssh".into(), args })
    }

    fn start_direct(&self, spec: &MachineSpec, kind: &AgentKind, name: String, cwd: String, title: String, req: &StartSession, windows: bool) -> SessionInfo {
        let prompt = req.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty());
        let run = match (&req.command, kind.command, prompt) {
            (Some(c), _, _) => c.clone(),
            (None, "", _) => String::new(),
            (None, c, Some(p)) if kind.prompt_arg => format!("{c} {}", if windows { ps_quote(p) } else { sh_quote(p) }),
            (None, c, _) => c.to_string(),
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
            busy: false,
            attention: None,
            clients: 1,
            created_at: Some(Utc::now()),
            last_output_at: None,
            attach_command: String::new(),
            persistent: false,
            name,
        };
        self.add_direct(DirectSession { info: info.clone(), run, windows });
        info
    }

    /// Machines whose sessions should be re-read on the timer.
    pub(crate) fn machines_with_sessions(&self) -> Vec<String> {
        self.all_sessions().into_iter().filter(|(_, m)| !m.sessions.is_empty()).map(|(id, _)| id).collect()
    }
}

/// Notices from a new discovery: events logged since `since`, and sessions that
/// went quiet after working. `before` is the previous discovery.
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
    // Working, then quiet: the agent finished or is waiting for an answer. Shells don't count.
    for s in &after.sessions {
        let was_busy = before.sessions.iter().any(|b| b.name == s.name && b.busy);
        if was_busy && !s.busy && s.status == "running" && !s.agent.is_empty() && s.agent != "shell" {
            out.push((s.id.clone(), "waiting".into(), format!("{} is waiting for you", s.agent_name), format!("{} · {machine}", s.title)));
        }
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

/// `title`, or `title 2`, `title 3`, ... when a session already has it.
fn unique_title(title: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == title) {
        return title.to_string();
    }
    (2..).map(|n| format!("{title} {n}")).find(|t| !taken.contains(t)).unwrap_or_else(|| title.to_string())
}

/// A short default title: the agent and the folder it works in.
fn default_title(kind: &AgentKind, cwd: &str) -> String {
    let folder = cwd.trim_end_matches('/').rsplit('/').next().filter(|f| !f.is_empty() && *f != "~").unwrap_or("home");
    format!("{} · {folder}", kind.name)
}

fn split_id(id: &str) -> Result<(&str, &str), String> {
    id.split_once('/')
        .filter(|(m, n)| !m.is_empty() && !n.is_empty())
        .ok_or_else(|| format!("\"{id}\" isn't a session id; they look like <machine-id>/<name>, e.g. beelink/claude-3f9a."))
}

/// The paste-into-your-terminal attach command (detach with Ctrl-] then d).
fn user_attach_command(spec: &MachineSpec, name: &str) -> String {
    let attach = format!("{} attach {name}", agent::REMOTE_PATH);
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
        found.sessions.push(SessionInfo {
            id: format!("{}/{name}", spec.id),
            machine_id: spec.id.clone(),
            agent: kind.map(|k| k.id.to_string()).unwrap_or_default(),
            agent_name: kind.map(|k| k.name.to_string()).unwrap_or_default(),
            title: s["title"].as_str().filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(|| name.clone()),
            cwd: s["cwd"].as_str().unwrap_or("").to_string(),
            status: if running { "running".into() } else { "exited".into() },
            exit_code: s["exit_code"].as_i64().map(|c| c as i32),
            // The same rule as the agent's (its "sessions" event comes when this turns false).
            busy: running && last_output.is_some_and(|a| now - a < BUSY_WINDOW_SECS),
            attention: None,
            clients: s["clients"].as_u64().unwrap_or(0) as u32,
            created_at: ts(&s["created"]),
            last_output_at: last_output.and_then(|t| Utc.timestamp_opt(t, 0).single()),
            attach_command: user_attach_command(spec, &name),
            persistent: true,
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
        let notices = new_notices(&before, &after, 950, "Beelink");
        let kinds: Vec<(&str, &str)> = notices.iter().map(|(id, kind, _, _)| (id.as_str(), kind.as_str())).collect();
        // The old bell doesn't repeat; the exit is new; Claude went quiet.
        assert_eq!(kinds, vec![("beelink/shell-cd34", "exited"), ("beelink/claude-ab12", "waiting")]);
        assert_eq!(notices[0].2, "Shell exited with code 2");
        assert_eq!(notices[1].2, "Claude Code is waiting for you");
        assert_eq!(notices[1].3, "Claude Code · app · Beelink");
    }

    #[test]
    fn titles_and_ids() {
        assert_eq!(default_title(agent("claude").unwrap(), "~/code/api/"), "Claude Code · api");
        assert_eq!(default_title(agent("shell").unwrap(), "~"), "Shell · home");
        assert!(split_id("beelink/claude-1").is_ok());
        assert!(split_id("claude-1").is_err());
        assert_eq!(user_attach_command(&spec("local"), "x"), "~/.muxen/bin/muxen-agent attach x");
        let taken = vec!["Shell · home".to_string(), "Shell · home 2".to_string()];
        assert_eq!(unique_title("Shell · home", &taken), "Shell · home 3");
        assert_eq!(unique_title("Shell · tmp", &taken), "Shell · tmp");
    }
}
