//! The snapshot is the single document the dashboard renders. It is rebuilt
//! whenever inventory, machine checks or GitHub data change, and pushed to
//! browsers over SSE. Keep `web/src/types.ts` in sync with this file.

use chrono::{DateTime, Utc};
use serde::Serialize;

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
    /// "GITHUB_TOKEN", "GH_TOKEN", "OPENRUNNER_GITHUB_TOKEN", "gh" or "" when no token was found.
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
    /// "online" | "offline" | "unchecked" | "manual"
    pub status: String,
    pub checking: bool,
    pub notes: Option<String>,
    pub runner_ids: Vec<String>,
    pub summary: RunnerSummary,
    pub probe: Option<Probe>,
    pub diagnostics: Vec<Diagnostic>,
    /// Agents may create workspaces here (fleet.toml `workspaces`, default true).
    pub workspaces_allowed: bool,
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
    /// Linux: "root" | "passwordless" | "password" — whether OpenRunner can install systemd services.
    pub sudo: Option<String>,
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
    /// Listed in fleet.toml (as opposed to discovered on the machine or on GitHub).
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
    /// For a human: opens a shell in the workspace, e.g. `ssh -t studio 'cd ~/openrunner/workspaces/ws-7f3a2c && exec $SHELL -l'`.
    pub shell: String,
    /// "host" (commands run directly on the machine) or "docker" (inside a container).
    #[serde(default = "host_isolation")]
    pub isolation: String,
    /// Docker workspaces: the image the container runs, e.g. "node:22".
    #[serde(default)]
    pub image: Option<String>,
    /// Docker workspaces: the container's name, e.g. "openrunner-ws-7f3a2c".
    #[serde(default)]
    pub container: Option<String>,
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
