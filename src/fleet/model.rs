//! The snapshot is the single document the dashboard renders. It is rebuilt
//! whenever inventory, machine checks or GitHub data change, and pushed to
//! browsers over SSE. Keep `web/src/types.ts` in sync with this file.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub version: u64,
    pub generated_at: DateTime<Utc>,
    /// Latest of the last machine check and the last GitHub sync.
    pub updated_at: Option<DateTime<Utc>>,
    pub demo: bool,
    pub inventory: InventoryInfo,
    pub check: CheckInfo,
    pub github: GitHubInfo,
    pub counts: Counts,
    pub machines: Vec<Machine>,
    pub runners: Vec<Runner>,
    pub jobs: Jobs,
    pub attention: Vec<Attention>,
    /// Control operations (install, start, stop, ...), newest first.
    pub operations: Vec<Operation>,
    /// Agent workspaces on fleet machines, most recently used first.
    pub workspaces: Vec<WorkspaceInfo>,
    /// Agent sessions (terminals running Claude Code, Codex, ...), by machine, newest first.
    pub sessions: Vec<super::sessions::SessionInfo>,
    /// What sessions asked for your attention, newest first.
    pub notifications: Vec<super::sessions::SessionNotice>,
    /// Git checkouts found on the machines, by repository then machine.
    pub checkouts: Vec<Checkout>,
    /// Tailscale on this computer; None when it isn't installed.
    pub tailscale: Option<TailscaleStatus>,
}

/// Tailscale on this computer.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TailscaleStatus {
    /// Tailscale's state: "Running", "Stopped", "NeedsLogin", ...
    pub state: String,
    pub running: bool,
    /// The tailnet's name, e.g. "you@example.com".
    pub tailnet: Option<String>,
}

/// A machine as Tailscale sees it, when it's on the user's tailnet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TailnetDevice {
    /// Its MagicDNS name's first part ("box").
    pub name: String,
    /// Its full MagicDNS name ("box.tail1234.ts.net").
    pub dns_name: String,
    pub ips: Vec<String>,
    pub os: String,
    /// Whether Tailscale sees it online.
    pub online: bool,
    /// When Tailscale last heard from it, while it's offline.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// How Tailscale reaches it from here: "direct" (peer-to-peer), "relay"
    /// (through a Tailscale relay, slower), or "idle" (no traffic lately).
    pub path: String,
    /// The relay's region code (e.g. "mia"), when relayed.
    pub relay: Option<String>,
    /// When Muxen read this from Tailscale.
    pub read_at: DateTime<Utc>,
}

/// A git checkout on one of the machines.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkout {
    pub machine_id: String,
    pub path: String,
    /// "owner/name" when its origin is on GitHub.
    pub repo: Option<String>,
    pub remote: Option<String>,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    /// Files with uncommitted changes (including untracked).
    pub dirty: u32,
    pub last_commit_at: Option<DateTime<Utc>>,
    pub last_commit: String,
}

/// The GitHub CLI on a machine (agents there use it for issues and pull requests).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GhCli {
    pub installed: bool,
    pub logged_in: bool,
    pub account: Option<String>,
    /// Where gh is on that machine.
    #[serde(skip)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InventoryInfo {
    pub path: String,
    pub loaded_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CheckInfo {
    pub running: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub next_at: Option<DateTime<Utc>>,
    /// 0 means automatic checks are off.
    pub interval_minutes: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GitHubInfo {
    /// At least one org or repo is configured.
    pub configured: bool,
    /// The last sync reached GitHub for at least one scope.
    pub live: bool,
    pub syncing: bool,
    /// "GITHUB_TOKEN", "GH_TOKEN", "MUXEN_GITHUB_TOKEN", "gh" or "" when no token was found.
    pub token_source: String,
    pub login: Option<String>,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub scopes: Vec<ScopeStatus>,
    pub rate_limit: Option<RateLimit>,
    /// Orgs/repos found in runner config files that aren't configured yet.
    pub detected: Vec<String>,
    pub scanned_repos: Vec<String>,
    pub poll_seconds: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeStatus {
    /// "org" or "repo"
    pub kind: String,
    pub name: String,
    pub runners: usize,
    pub synced_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimit {
    pub limit: u32,
    pub remaining: u32,
    pub reset_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub machines: usize,
    pub runner_machines: usize,
    pub other_machines: usize,
    pub runners: usize,
    pub working: usize,
    pub idle: usize,
    pub offline: usize,
    pub unknown: usize,
    pub queued: usize,
    pub running: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub id: String,
    pub name: String,
    /// SSH destination, "local", or "" when the machine can't be checked.
    pub host: String,
    pub local: bool,
    /// e.g. "darwin-arm64", "linux-x64", "windows-x64". From the last check, else the inventory hint.
    pub platform: String,
    /// "runner" when it hosts runners, otherwise "other".
    pub kind: String,
    /// "online" | "offline" | "unchecked" | "manual" | "disconnected" (on
    /// purpose: Muxen leaves it alone until it's connected again)
    pub status: String,
    pub checking: bool,
    pub notes: Option<String>,
    pub runner_ids: Vec<String>,
    pub summary: RunnerSummary,
    pub probe: Option<Probe>,
    pub diagnostics: Vec<Diagnostic>,
    /// Agents may create workspaces here (config.toml `workspaces`, default true).
    pub workspaces_allowed: bool,
    /// Agent CLIs found on the machine ("claude", "codex", ...), from the last session check.
    pub agents: Vec<String>,
    /// The Muxen agent's version there (it runs agent sessions), once set up.
    pub agent_version: Option<String>,
    /// Why sessions aren't available here, when they aren't.
    pub sessions_error: Option<String>,
    /// The GitHub CLI there, once looked up.
    pub gh: Option<GhCli>,
    /// Why it can't be reached, when `status` is "offline".
    pub unreachable: Option<Unreachable>,
    /// It on the user's tailnet, as Tailscale last saw it.
    pub tailnet: Option<TailnetDevice>,
    /// A cloud machine Muxen created: where it runs and what it costs.
    pub cloud: Option<CloudInfo>,
    /// Its monitors, virtual ones included, as last listed (empty until then).
    pub displays: Vec<DisplayInfo>,
    /// Another of the user's machines it runs on (a VM or container there),
    /// or is reached through (a jump host).
    pub parent: Option<ParentMachine>,
    /// A VM Muxen made (and can destroy): its host, system and container.
    pub vm: Option<crate::config::VmRef>,
    /// Where a direct connection reaches it (a forwarded port), from config.toml.
    pub direct_address: Option<String>,
}

/// The machine another one runs on or is reached through, as Muxen worked it
/// out from ~/.ssh/config, the machines' checks and their Docker containers.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParentMachine {
    pub machine_id: String,
    pub name: String,
    /// "vm": a virtual machine on it; "container": a container on it;
    /// "inside": something on it, kind unknown; "through": a different
    /// machine that ssh reaches through it (a jump host).
    pub relation: String,
    /// The Docker container it runs in there, when one publishes its ssh port.
    pub container: Option<String>,
    /// That container's state as the parent's last check saw it ("running", "exited", ...).
    pub container_state: Option<String>,
}

impl ParentMachine {
    /// It runs on the parent (not just reached through it).
    pub fn inside(&self) -> bool {
        self.relation != "through"
    }

    /// "VM on Beelink", "Container on Beelink", "On Beelink", "Through Beelink".
    pub fn label(&self) -> String {
        match self.relation.as_str() {
            "vm" => format!("VM on {}", self.name),
            "container" => format!("Container on {}", self.name),
            "through" => format!("Through {}", self.name),
            _ => format!("On {}", self.name),
        }
    }
}

impl Machine {
    /// Muxen can't use it now: it's offline, or disconnected on purpose.
    pub fn is_down(&self) -> bool {
        matches!(self.status.as_str(), "offline" | "disconnected")
    }

    pub fn is_disconnected(&self) -> bool {
        self.status == "disconnected"
    }
}

/// One of a machine's monitors: a real one, or a virtual one Muxen added.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct DisplayInfo {
    /// What to pass as `display`: a RandR monitor's name on Linux ("all": the
    /// whole desktop), the display's number on a Mac.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// The main (primary) display.
    #[serde(default)]
    pub main: bool,
    /// A virtual monitor Muxen added (and can remove).
    #[serde(default, rename = "virtual")]
    pub is_virtual: bool,
    /// The sizes it can be set to, biggest first (empty: it can't be, or
    /// nobody knows).
    #[serde(default)]
    pub modes: Vec<(u32, u32)>,
    /// Any size can be given to it, not only `modes` (Linux).
    #[serde(default)]
    pub any_size: bool,
    /// How much the desktop scales things up on it (2: twice as big, as
    /// sharp); 0 when unknown.
    #[serde(default)]
    pub scale: f64,
    /// The scales it can be set to (empty: it can't be).
    #[serde(default)]
    pub scales: Vec<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudInfo {
    /// "digitalocean"
    pub provider: String,
    /// The droplet's id there.
    pub id: u64,
    pub region: String,
    pub size: String,
    /// USD an hour, billed while it exists (powered off too).
    pub price_hourly: f64,
    pub created_at: Option<DateTime<Utc>>,
    /// What it has cost since it was created, in USD.
    pub cost_so_far: f64,
    /// What it has cost this month (UTC), in USD.
    pub this_month: f64,
    /// The most it costs in a month (DigitalOcean caps it), in USD.
    pub price_monthly: f64,
    /// Idle (no busy session, job or CPU) since then, as this app has seen it.
    pub idle_since: Option<DateTime<Utc>>,
    /// "on", "off" or "starting", as last seen; None until known.
    pub power: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSummary {
    pub total: usize,
    pub working: usize,
    pub idle: usize,
    pub offline: usize,
    pub unknown: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub checked_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub os_version: Option<String>,
    /// "laptop" | "desktop", when the machine says.
    pub form_factor: Option<String>,
    /// Whether it has a graphical session to show (Linux: an X server). None: not said.
    pub desktop: Option<bool>,
    pub cores: Option<u32>,
    pub cpu_pct: Option<f64>,
    pub mem_total: Option<u64>,
    pub mem_used: Option<u64>,
    pub disk_total: Option<u64>,
    pub disk_free: Option<u64>,
    pub disk_path: Option<String>,
    pub uptime_sec: Option<u64>,
    pub load: Vec<f64>,
    /// Docker server version, when the machine's user can use Docker.
    pub docker: Option<String>,
    /// Linux: "root" | "passwordless" | "password" — whether Muxen can install systemd services.
    pub sudo: Option<String>,
    /// The last time a check reached the machine.
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// Why a machine can't be reached, in plain words.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Unreachable {
    /// "offline" (no answer: shut down, asleep, off the network) | "refused" |
    /// "auth" | "unknown-host" | "host-key" | "tailscale-off" (this computer's
    /// Tailscale is off or signed out, and the machine is on the tailnet) |
    /// "powered-off" (a cloud machine) | "host-down" (the machine it runs on or
    /// is reached through is off) | "stopped" (its container there isn't
    /// running) | "installing" (a VM Muxen just made) | "error"
    pub kind: String,
    /// e.g. "Beelink is offline".
    pub title: String,
    pub detail: String,
    /// A command to try, when there's a fix on this side.
    pub hint: Option<String>,
    /// The last time a check reached it.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// What the check said, word for word.
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    /// "error" | "warning" | "unknown" | "info"
    pub level: String,
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
    pub runner_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Runner {
    /// "<machineId>/<name>", or "github/<name>" when no machine claims it.
    pub id: String,
    pub name: String,
    /// None when GitHub knows the runner but the inventory doesn't.
    pub machine_id: Option<String>,
    /// "working" | "idle" | "offline" | "unknown"
    pub state: String,
    /// Where `state` came from: "github" (live) | "probe" (last check) | "none"
    pub source: String,
    /// Listed in config.toml (as opposed to discovered on the machine or on GitHub).
    pub declared: bool,
    pub labels: Vec<String>,
    pub github: Option<GitHubRunner>,
    pub local: Option<LocalRunner>,
    /// The job this runner is running, if known (see `jobs.running`).
    pub job_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubRunner {
    pub id: u64,
    /// "online" | "offline"
    pub status: String,
    pub busy: bool,
    pub os: String,
    /// "org:<name>" or "repo:<owner>/<name>"
    pub scope: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRunner {
    pub dir: String,
    /// gitHubUrl from the runner's .runner file
    pub url: Option<String>,
    pub version: Option<String>,
    /// launchd / systemd / Windows service name, if installed as a service
    pub service: Option<String>,
    /// "active" | "inactive" | "unknown"
    pub service_state: Option<String>,
    pub listener: bool,
    pub worker: bool,
    pub listener_pid: Option<u32>,
    pub worker_pid: Option<u32>,
    /// Docker runners: the container (its `dir` is `docker://<container>`).
    pub container: Option<String>,
    pub image: Option<String>,
    /// "running" | "exited" | "restarting" | "created" | "paused" | "dead"
    pub container_state: Option<String>,
    /// Restarts Docker has done; a climbing count means a crash loop.
    pub restarts: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Jobs {
    pub queued: Vec<Job>,
    pub running: Vec<Job>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: u64,
    pub run_id: u64,
    pub name: String,
    pub workflow: String,
    pub repo: String,
    pub branch: Option<String>,
    pub event: Option<String>,
    pub actor: Option<String>,
    pub url: String,
    /// "queued" | "in_progress" | "waiting"
    pub status: String,
    pub labels: Vec<String>,
    pub runner_name: Option<String>,
    pub runner_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    /// Queued jobs: runner ids whose labels satisfy the job's labels.
    pub matching: Vec<String>,
    /// How many of `matching` are currently online (working or idle).
    pub matching_online: usize,
}

impl Job {
    /// Why it hasn't started, in a few words: who can take it, or which of
    /// its labels no runner has.
    pub fn waiting_reason(&self, runners: &[Runner]) -> String {
        if self.matching_online > 0 {
            return format!("{} runner{} can take it", self.matching_online, if self.matching_online == 1 { "" } else { "s" });
        }
        let anyone_has = |label: &str| runners.iter().any(|r| r.labels.iter().any(|l| l.eq_ignore_ascii_case(label)));
        let missing: Vec<&str> = self.labels.iter().map(String::as_str).filter(|l| !l.eq_ignore_ascii_case("self-hosted") && !anyone_has(l)).collect();
        match missing.as_slice() {
            [] if !self.matching.is_empty() => "Its runners are offline".into(),
            [] => "No runner has all its labels".into(),
            [one] => format!("No runner has the {one} label"),
            many => format!("No runner has {}", many.join(", ")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attention {
    pub id: String,
    /// "error" | "warning" | "info"
    pub level: String,
    pub title: String,
    pub detail: String,
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    pub label: String,
    /// "settings" | "check" | "machine" | "runner" | "link" | "queue"
    pub kind: String,
    /// machine/runner id or URL, depending on `kind`
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub id: u64,
    /// "install" | "start" | "stop" | "restart" | "remove" | "logs" | "machine"
    pub kind: String,
    /// e.g. "Install mac-studio-08 on Mac Studio"
    pub title: String,
    pub machine_id: Option<String>,
    pub runner_id: Option<String>,
    /// "running" | "succeeded" | "failed"
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Output lines, with secrets masked.
    pub log: Vec<String>,
    pub error: Option<String>,
    /// How far along it is, for operations that move data (copies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<OpProgress>,
    /// It can be stopped (cancel_operation).
    pub cancellable: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OpProgress {
    /// Bytes done so far.
    pub done: u64,
    /// Bytes in all, when known.
    pub total: Option<u64>,
    /// Bytes per second, lately.
    pub rate: Option<f64>,
}

/// A work environment an agent checked out on a fleet machine: a folder
/// (optionally a git checkout) where it runs commands and edits files.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    /// "ws-7f3a2c"
    pub id: String,
    /// Human label: the name given at creation, else the repo name.
    pub name: String,
    pub machine_id: String,
    pub machine_name: String,
    pub host: String,
    pub platform: String,
    /// Absolute path on the machine.
    pub path: String,
    pub repo: Option<String>,
    pub git_ref: Option<String>,
    pub branch: Option<String>,
    /// Who created it, e.g. the MCP client name ("claude-code") or "cli" / "app".
    pub owner: Option<String>,
    /// "creating" | "ready" | "failed" | "destroying"
    pub status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    /// Total commands run, including ones dropped from `history`.
    pub commands: usize,
    /// Recent commands, oldest first (capped).
    pub history: Vec<WorkspaceCommand>,
    /// For a human: opens a shell in the workspace, e.g. `ssh -t studio 'cd ~/muxen/workspaces/ws-7f3a2c && exec $SHELL -l'`.
    pub shell: String,
    /// "host" (commands run directly on the machine) or "docker" (inside a container).
    #[serde(default = "host_isolation")]
    pub isolation: String,
    /// Docker workspaces: the image the container runs, e.g. "node:22".
    #[serde(default)]
    pub image: Option<String>,
    /// Docker workspaces: the container's name, e.g. "muxen-ws-7f3a2c".
    #[serde(default)]
    pub container: Option<String>,
    /// Worktree workspaces: the checkout on the same machine it's a git worktree of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_of: Option<String>,
    /// What the work in it changed, as last looked at (see sessions::git); never stored.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub git: Option<super::sessions::git::GitStatus>,
}

fn host_isolation() -> String {
    "host".into()
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCommand {
    pub command: String,
    /// None while running, for background commands, or when it timed out.
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u64,
    /// Last lines of combined output.
    pub output_tail: Vec<String>,
    pub background: bool,
    pub timed_out: bool,
    pub by: Option<String>,
}
