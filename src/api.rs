//! The agent-facing API: one layer behind both the MCP server (`muxin mcp`)
//! and the scriptable CLI (`muxin status --json`, `muxin install`, ...).
//!
//! Responses are compact and explicit on purpose: stable ids, plain state
//! strings, a one-line `summary` where it helps, and operation logs capped to a
//! tail. Errors carry a `code` an agent can branch on and, when we know it, a
//! `hint` with the exact fix.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fleet::Engine;
use crate::fleet::model::{Job, Machine, Operation, Runner, Snapshot, WorkspaceCommand, WorkspaceInfo};
use crate::fleet::workspaces::{CreateWorkspace, ExecOptions, SyncDirection};
use crate::ops::{InstallMode, InstallRequest, NewMachine, RunnerAction};
use crate::util;

/// GitHub data older than this is refreshed before answering.
const GITHUB_MAX_AGE_SECS: i64 = 30;
const DEFAULT_TAIL: usize = 80;
const LOGS_TAIL: usize = 200;
const INSTALL_TIMEOUT_SECS: u64 = 900;
const ACTION_TIMEOUT_SECS: u64 = 180;
const CHECK_WAIT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    /// "not_found" | "invalid" | "busy" | "github" | "unreachable" | "operation_failed" | "internal"
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// For "operation_failed": the operation and the state it left things in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    fn new(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError { code, message: message.into(), hint: None, details: None }
    }

    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new("not_found", message)
    }

    pub fn invalid(message: impl Into<String>) -> ApiError {
        ApiError::new("invalid", message)
    }

    fn with_hint(mut self, hint: impl Into<String>) -> ApiError {
        self.hint = Some(hint.into());
        self
    }

    /// Exit code for the CLI: 2 for bad input, 1 for everything else.
    pub fn exit_code(&self) -> u8 {
        match self.code {
            "not_found" | "invalid" => 2,
            _ => 1,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(h) = &self.hint {
            write!(f, " (hint: {h})")?;
        }
        Ok(())
    }
}

/// Maps a message from the ops layer onto an error code.
fn op_error(message: String) -> ApiError {
    let lower = message.to_ascii_lowercase();
    let code = if lower.contains("another operation") || lower.contains("still running") {
        "busy"
    } else if lower.contains("isn't in config.toml") || lower.contains("isn't in muxen") {
        "not_found"
    } else {
        "invalid"
    };
    ApiError::new(code, message)
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Who's asking, so hints name the right next step: an MCP tool or a CLI command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Mcp,
    Cli,
}

impl Surface {
    fn say(self, mcp: impl Into<String>, cli: impl Into<String>) -> String {
        match self {
            Surface::Mcp => mcp.into(),
            Surface::Cli => cli.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Requests. Doc comments here double as the tool documentation.

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatusRequest {
    /// Check every machine over SSH and sync GitHub before answering (slower, up to ~45s per unreachable machine).
    pub refresh: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineRequest {
    /// Machine id, e.g. "mac-studio".
    pub machine_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunnersRequest {
    /// Only runners on this machine.
    pub machine_id: Option<String>,
    /// Only runners in this state: working, idle, offline or unknown.
    pub state: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckRequest {
    /// Check only this machine; omit to check all of them.
    pub machine_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectMachineRequest {
    pub machine_id: String,
    /// false disconnects. Default true.
    #[serde(default = "default_true")]
    pub connected: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameMachineRequest {
    /// Machine id, e.g. "mac-studio". It stays the same after the rename.
    pub machine_id: String,
    /// The new display name.
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddMachineRequest {
    /// Display name, e.g. "Mac Studio". The id is derived from it.
    pub name: String,
    /// An ssh destination ("ci@studio.local", an ~/.ssh/config alias) or "local" for this computer.
    pub host: String,
    /// Check the machine right away and return what was found (default true).
    #[serde(default = "yes")]
    pub check: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRunnersRequest {
    pub machine_id: String,
    /// GitHub org ("acme") or repo ("acme/api") to register with. Optional when exactly one is watched.
    #[serde(default)]
    pub scope: Option<String>,
    /// Runner names. When empty, `count` names like "<machine-id>-01" are picked.
    #[serde(default)]
    pub names: Vec<String>,
    /// How many runners to install when `names` is empty (default 1).
    #[serde(default)]
    pub count: Option<usize>,
    /// Extra labels; GitHub adds self-hosted, the OS and the architecture itself.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Org runner group (org scopes only).
    #[serde(default)]
    pub runner_group: Option<String>,
    /// Legacy: false is the same as mode "background". Prefer `mode`.
    #[serde(default = "yes")]
    pub as_service: bool,
    /// "auto" (default): a service when the machine allows it, else Docker when available, else background.
    /// "service": launchd / systemd / Windows service (Linux needs passwordless sudo).
    /// "background": run.sh, no root needed, not restarted after a reboot.
    /// "docker": one container per runner from GitHub's actions-runner image, restarted by Docker.
    #[serde(default)]
    pub mode: Option<String>,
    /// mode "docker": image to use (default ghcr.io/actions/actions-runner:<latest runner version>).
    #[serde(default)]
    pub image: Option<String>,
    /// mode "docker": give jobs the host's Docker socket (root-equivalent access to the machine).
    #[serde(default)]
    pub docker_socket: bool,
    /// Wait for the install to finish (default true).
    #[serde(default = "yes")]
    pub wait: bool,
    /// How long to wait, in seconds (default 900).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionName {
    Start,
    Stop,
    Restart,
    Logs,
    Remove,
}

impl std::str::FromStr for ActionName {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "start" => Ok(ActionName::Start),
            "stop" => Ok(ActionName::Stop),
            "restart" => Ok(ActionName::Restart),
            "logs" | "log" => Ok(ActionName::Logs),
            "remove" | "rm" | "delete" => Ok(ActionName::Remove),
            other => Err(ApiError::invalid(format!("Unknown action \"{other}\". Use start, stop, restart, logs or remove."))),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRunRequest {
    /// A queued or running job (`list_jobs`); its whole workflow run is cancelled.
    pub job_id: u64,
    /// Wait for GitHub to accept it (default true).
    #[serde(default = "yes")]
    pub wait: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerActionRequest {
    /// Runner id: "<machine-id>/<runner-name>", or "github/<name>" for a runner no machine lists.
    pub runner_id: String,
    pub action: ActionName,
    /// With action=remove: also delete the runner's folder on the machine.
    #[serde(default)]
    pub delete_files: bool,
    /// Wait for the action to finish (default true).
    #[serde(default = "yes")]
    pub wait: bool,
    /// How long to wait, in seconds (default 180).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRequest {
    pub operation_id: u64,
    /// How many log lines to return from the end (default 80).
    #[serde(default)]
    pub tail: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitHubScopesRequest {
    /// Orgs ("acme") or repos ("acme/api") to start watching. Each is checked against GitHub first.
    pub add: Vec<String>,
    /// Orgs or repos to stop watching.
    pub remove: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateSettingsRequest {
    /// How often the window polls GitHub, in seconds (min 10).
    pub github_poll_seconds: Option<u32>,
    /// How many recently pushed repos per org to scan for queued/running jobs (0-50).
    pub scan_repos: Option<u32>,
    /// Automatic machine checks, in minutes (0 = only on request).
    pub check_interval_minutes: Option<u32>,
    /// GitHub REST API base (GitHub Enterprise Server: https://HOST/api/v3).
    pub api_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Responses

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub summary: String,
    pub counts: Counts,
    pub github: GitHubOut,
    pub last_check_at: Option<DateTime<Utc>>,
    /// Problems worth acting on, most severe first. Read these first.
    pub attention: Vec<AttentionOut>,
    pub machines: Vec<MachineSummary>,
    pub inventory: InventoryOut,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub demo: bool,
}

#[derive(Debug, Serialize)]
pub struct Counts {
    pub machines: usize,
    pub runners: usize,
    pub working: usize,
    pub idle: usize,
    pub offline: usize,
    pub unknown: usize,
    pub queued_jobs: usize,
    pub running_jobs: usize,
}

#[derive(Debug, Serialize)]
pub struct GitHubOut {
    /// At least one org or repo is watched.
    pub configured: bool,
    /// Runner states come live from GitHub (otherwise from the last machine check).
    pub live: bool,
    /// Where the token came from: "gh", "GITHUB_TOKEN", ...; null when none was found.
    pub token: Option<String>,
    pub login: Option<String>,
    pub watching: Vec<ScopeOut>,
    /// Orgs/repos named in runner configs on the machines that aren't watched yet.
    pub detected: Vec<String>,
    pub last_sync_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ScopeOut {
    /// "org" or "repo"
    pub kind: String,
    pub name: String,
    pub runners: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AttentionOut {
    /// "error" | "warning" | "info"
    pub level: String,
    pub title: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InventoryOut {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineSummary {
    pub id: String,
    pub name: String,
    pub host: String,
    /// "online" | "offline" (unreachable) | "unchecked" | "manual" (no host) | "disconnected" (on purpose: connect_machine)
    pub status: String,
    pub checking: bool,
    /// e.g. "darwin-arm64", "linux-x64", "windows-x64"
    pub platform: String,
    /// e.g. "3/7 working", "2 idle", "no runners"
    pub runners: String,
    pub runner_counts: RunnerCounts,
    pub cpu_pct: Option<f64>,
    pub mem_pct: Option<f64>,
    pub disk_free: Option<String>,
    pub disk_free_bytes: Option<u64>,
    pub uptime: Option<String>,
    /// Warnings and errors (info items don't count).
    pub problems: usize,
    /// Worst diagnostic level: "error" | "warning" | "unknown" | "info" | null
    pub worst: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
    /// The last time a check reached it.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// What the last check said, word for word, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why it can't be reached, in plain words: `kind` is "offline" (shut
    /// down, asleep, off the network: nothing to fix), "refused", "auth",
    /// "unknown-host", "host-key" or "error"; `hint` is a command to try.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreachable: Option<crate::fleet::model::Unreachable>,
    /// It on the user's Tailscale network: whether Tailscale sees it online,
    /// and whether traffic to it goes peer-to-peer or through a relay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<crate::fleet::model::TailnetDevice>,
    /// A cloud machine Muxin created: provider, droplet id, region, size,
    /// USD an hour, what it has cost so far and since when it's been idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudOut>,
    /// The machine it runs on (a VM or container there) or is reached
    /// through (relation "through"), with the container it runs in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<crate::fleet::model::ParentMachine>,
    /// A VM Muxin made with vm_create (vm_destroy deletes it): host, os,
    /// version (the system id), container, created_at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm: Option<crate::config::VmRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudOut {
    /// "digitalocean"
    pub provider: String,
    pub droplet_id: u64,
    pub region: String,
    pub size: String,
    /// USD an hour, billed while it exists (powered off too).
    pub price_hourly: f64,
    pub created_at: Option<DateTime<Utc>>,
    /// USD since it was created.
    pub cost_so_far: f64,
    /// USD this month (UTC).
    pub this_month: f64,
    /// The most it costs in a month, in USD.
    pub price_monthly: f64,
    pub idle_since: Option<DateTime<Utc>>,
    /// "on", "off" (still billed) or "starting", as last seen; null until known.
    pub power: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerCounts {
    pub total: usize,
    pub working: usize,
    pub idle: usize,
    pub offline: usize,
    pub unknown: usize,
}

#[derive(Debug, Serialize)]
pub struct MachineDetail {
    #[serde(flatten)]
    pub summary: MachineSummary,
    pub os_version: Option<String>,
    pub cores: Option<u32>,
    pub load: Vec<f64>,
    pub mem_used_bytes: Option<u64>,
    pub mem_total_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub notes: Option<String>,
    pub runners: Vec<RunnerOut>,
    pub diagnostics: Vec<DiagnosticOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticOut {
    /// "error" | "warning" | "unknown" | "info"
    pub level: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerOut {
    pub id: String,
    pub name: String,
    pub machine_id: Option<String>,
    /// "working" | "idle" | "offline" | "unknown"
    pub state: String,
    /// Where `state` came from: "github" (live), "probe" (last machine check) or "none".
    pub source: String,
    /// Listed in config.toml (vs discovered on the machine or on GitHub).
    pub declared: bool,
    pub labels: Vec<String>,
    pub version: Option<String>,
    pub dir: Option<String>,
    pub service: Option<String>,
    pub service_state: Option<String>,
    pub registered_to: Option<String>,
    pub github_status: Option<String>,
    pub job: Option<JobOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobOut {
    pub id: u64,
    pub name: String,
    pub workflow: String,
    pub repo: String,
    pub branch: Option<String>,
    /// "queued" | "in_progress" | "waiting"
    pub status: String,
    pub labels: Vec<String>,
    pub runner_id: Option<String>,
    pub url: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    /// Seconds queued (queued jobs) or running (running jobs).
    pub age_secs: i64,
    /// Queued jobs: how many online runners have every label the job asks for.
    pub matching_online: usize,
}

#[derive(Debug, Serialize)]
pub struct JobsResponse {
    pub summary: String,
    pub queued: Vec<JobOut>,
    pub running: Vec<JobOut>,
    pub scanned_repos: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationOut {
    pub id: u64,
    /// "install" | "start" | "stop" | "restart" | "remove" | "logs"
    pub kind: String,
    pub title: String,
    /// "running" | "succeeded" | "failed"
    pub status: String,
    pub machine_id: Option<String>,
    pub runner_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// The last lines of output. "▸ " lines are Muxin's steps, "✓ " successes,
    /// "error: "/"warning: " problems; anything else is output from the machine.
    pub log_tail: Vec<String>,
    pub log_lines: usize,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct ActionResponse {
    pub operation: OperationOut,
    /// False when `wait` was off or the timeout passed; poll get_operation.
    pub finished: bool,
    pub machine: Option<MachineSummary>,
    /// Runners the operation touched, as they are now (empty after a remove).
    pub runners: Vec<RunnerOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AddMachineResponse {
    pub machine_id: String,
    pub machine: Option<MachineDetail>,
    pub next: String,
}

#[derive(Debug, Serialize)]
pub struct ScopeResult {
    pub scope: String,
    /// "added" | "removed" | "already_watching" | "not_watching" | "failed"
    pub result: String,
    pub runners: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ScopesResponse {
    pub results: Vec<ScopeResult>,
    pub watching: Vec<ScopeOut>,
}

#[derive(Debug, Serialize)]
pub struct SettingsOut {
    pub github_orgs: Vec<String>,
    pub github_repos: Vec<String>,
    pub github_poll_seconds: u32,
    pub scan_repos: u32,
    pub check_interval_minutes: u32,
    pub api_url: String,
    pub config_dir: String,
    pub inventory: String,
}

#[derive(Debug, Serialize)]
pub struct SshHostsResponse {
    /// Host aliases from ~/.ssh/config, usable as `host` in add_machine.
    pub hosts: Vec<String>,
    /// Computers on the user's tailnet (when Tailscale is installed and on
    /// here), online ones first.
    pub tailnet: Vec<TailnetHost>,
    /// Computers on this local network that aren't in Muxin yet: ones
    /// advertising ssh (`ssh: true`, add by `host`), and Macs with Remote
    /// Login off (`ssh: false`: it needs turning on there first).
    pub network: Vec<crate::fleet::discover::NetworkHost>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TailnetHost {
    /// The device's own name, e.g. "box-SER".
    pub name: String,
    /// What to pass as add_machine's `host`: the user's ~/.ssh/config alias
    /// for it when there is one, else its MagicDNS name (reachable from
    /// anywhere on the tailnet).
    pub host: String,
    pub dns_name: String,
    /// "linux", "macOS", "windows", ...
    pub os: String,
    /// Whether Tailscale sees it online.
    pub online: bool,
    /// Already in Muxin, as this machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
}

/// Made-up suggestions for the demo's Add machine dialog.
fn demo_ssh_hosts() -> SshHostsResponse {
    let tail = |name: &str, os: &str| TailnetHost { name: name.into(), host: format!("{name}.tail0000.ts.net"), dns_name: format!("{name}.tail0000.ts.net."), os: os.into(), online: true, machine_id: None };
    let net = |name: &str, ssh: bool, mac: bool| crate::fleet::discover::NetworkHost { name: name.into(), host: format!("{name}.local"), addresses: vec!["192.168.1.60".into()], ssh, mac };
    SshHostsResponse {
        hosts: vec!["build-box".into(), "gpu-rig".into(), "nas".into()],
        tailnet: vec![tail("studio", "macOS"), tail("render-01", "linux")],
        network: vec![net("living-room-mac", false, true), net("pi-garage", true, false)],
    }
}

// ---------------------------------------------------------------------------
// Conversions

pub fn machine_summary(m: &Machine) -> MachineSummary {
    let p = m.probe.as_ref().filter(|p| p.ok);
    let problems = m.diagnostics.iter().filter(|d| matches!(d.level.as_str(), "warning" | "error")).count();
    let worst = ["error", "warning", "unknown", "info"]
        .into_iter()
        .find(|l| m.diagnostics.iter().any(|d| d.level == *l))
        .map(str::to_string);
    let s = &m.summary;
    let runners = if s.total == 0 {
        "no runners".to_string()
    } else if s.working > 0 {
        format!("{}/{} working", s.working, s.total)
    } else if s.idle == s.total {
        format!("{} idle", s.idle)
    } else if s.offline > 0 {
        format!("{}/{} offline", s.offline, s.total)
    } else {
        format!("{} unknown", s.unknown)
    };
    MachineSummary {
        id: m.id.clone(),
        name: m.name.clone(),
        host: m.host.clone(),
        status: m.status.clone(),
        checking: m.checking,
        platform: m.platform.clone(),
        runners,
        runner_counts: RunnerCounts { total: s.total, working: s.working, idle: s.idle, offline: s.offline, unknown: s.unknown },
        cpu_pct: p.and_then(|p| p.cpu_pct).map(round1),
        mem_pct: p.and_then(|p| Some(round1(p.mem_used? as f64 * 100.0 / p.mem_total?.max(1) as f64))),
        disk_free: p.and_then(|p| p.disk_free).map(util::bytes),
        disk_free_bytes: p.and_then(|p| p.disk_free),
        uptime: p.and_then(|p| p.uptime_sec).map(util::uptime),
        problems,
        worst,
        checked_at: m.probe.as_ref().map(|p| p.checked_at),
        last_seen_at: m.probe.as_ref().and_then(|p| p.last_seen_at),
        error: m.probe.as_ref().filter(|p| !p.ok).and_then(|p| p.error.clone()),
        unreachable: m.unreachable.clone(),
        tailnet: m.tailnet.clone(),
        cloud: m.cloud.as_ref().map(|c| CloudOut {
            provider: c.provider.clone(),
            droplet_id: c.id,
            region: c.region.clone(),
            size: c.size.clone(),
            price_hourly: c.price_hourly,
            created_at: c.created_at,
            cost_so_far: c.cost_so_far,
            this_month: c.this_month,
            price_monthly: c.price_monthly,
            idle_since: c.idle_since,
            power: c.power.clone(),
        }),
        parent: m.parent.clone(),
        vm: m.vm.clone(),
    }
}

fn machine_detail(s: &Snapshot, m: &Machine) -> MachineDetail {
    let p = m.probe.as_ref().filter(|p| p.ok);
    MachineDetail {
        summary: machine_summary(m),
        // An offline machine keeps what the last check that reached it found.
        os_version: m.probe.as_ref().and_then(|p| p.os_version.clone()),
        cores: p.and_then(|p| p.cores),
        load: p.map(|p| p.load.clone()).unwrap_or_default(),
        mem_used_bytes: p.and_then(|p| p.mem_used),
        mem_total_bytes: p.and_then(|p| p.mem_total),
        disk_total_bytes: p.and_then(|p| p.disk_total),
        notes: m.notes.clone(),
        runners: m.runner_ids.iter().filter_map(|id| s.runners.iter().find(|r| &r.id == id)).map(|r| runner_out(s, r)).collect(),
        diagnostics: m
            .diagnostics
            .iter()
            .map(|d| DiagnosticOut {
                level: d.level.clone(),
                code: d.code.clone(),
                message: d.message.clone(),
                hint: d.hint.clone(),
                runner_id: d.runner_id.clone(),
            })
            .collect(),
    }
}

fn runner_out(s: &Snapshot, r: &Runner) -> RunnerOut {
    let job = r.job_id.and_then(|id| s.jobs.running.iter().find(|j| j.id == id)).map(job_out);
    RunnerOut {
        id: r.id.clone(),
        name: r.name.clone(),
        machine_id: r.machine_id.clone(),
        state: r.state.clone(),
        source: r.source.clone(),
        declared: r.declared,
        labels: r.labels.clone(),
        version: r.local.as_ref().and_then(|l| l.version.clone()),
        dir: r.local.as_ref().map(|l| l.dir.clone()),
        service: r.local.as_ref().and_then(|l| l.service.clone()),
        service_state: r.local.as_ref().and_then(|l| l.service_state.clone()),
        registered_to: r
            .github
            .as_ref()
            .map(|g| g.scope.clone())
            .or_else(|| r.local.as_ref().and_then(|l| l.url.as_deref()).and_then(crate::github::scope_from_runner_url)),
        github_status: r.github.as_ref().map(|g| if g.busy { "busy".to_string() } else { g.status.clone() }),
        job,
    }
}

fn job_out(j: &Job) -> JobOut {
    let since = if j.status == "in_progress" { j.started_at.unwrap_or(j.created_at) } else { j.created_at };
    JobOut {
        id: j.id,
        name: j.name.clone(),
        workflow: j.workflow.clone(),
        repo: j.repo.clone(),
        branch: j.branch.clone(),
        status: j.status.clone(),
        labels: j.labels.clone(),
        runner_id: j.runner_id.clone(),
        url: j.url.clone(),
        created_at: j.created_at,
        started_at: j.started_at,
        age_secs: (Utc::now() - since).num_seconds().max(0),
        matching_online: j.matching_online,
    }
}

pub fn operation_out(o: &Operation, tail: usize) -> OperationOut {
    let start = o.log.len().saturating_sub(tail);
    OperationOut {
        id: o.id,
        kind: o.kind.clone(),
        title: o.title.clone(),
        status: o.status.clone(),
        machine_id: o.machine_id.clone(),
        runner_id: o.runner_id.clone(),
        started_at: o.started_at,
        finished_at: o.finished_at,
        error: o.error.clone(),
        log_tail: o.log[start..].to_vec(),
        log_lines: o.log.len(),
        truncated: start > 0,
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub(crate) fn find_machine<'a>(s: &'a Snapshot, id: &str) -> ApiResult<&'a Machine> {
    s.machines.iter().find(|m| m.id == id || m.name.eq_ignore_ascii_case(id)).ok_or_else(|| {
        let ids: Vec<&str> = s.machines.iter().map(|m| m.id.as_str()).collect();
        ApiError::not_found(format!("No machine \"{id}\".")).with_hint(if ids.is_empty() {
            "No machines yet; add one first.".to_string()
        } else {
            format!("Known machine ids: {}", ids.join(", "))
        })
    })
}

fn find_runner<'a>(s: &'a Snapshot, id: &str) -> ApiResult<&'a Runner> {
    // Accept a bare runner name too, as long as it's unambiguous.
    if let Some(r) = s.runners.iter().find(|r| r.id == id) {
        return Ok(r);
    }
    let by_name: Vec<&Runner> = s.runners.iter().filter(|r| r.name.eq_ignore_ascii_case(id)).collect();
    match by_name.as_slice() {
        [one] => Ok(one),
        _ => {
            let close: Vec<&str> = s
                .runners
                .iter()
                .filter(|r| r.id.contains(id) || id.contains(r.name.as_str()))
                .map(|r| r.id.as_str())
                .take(8)
                .collect();
            Err(ApiError::not_found(format!("No runner \"{id}\".")).with_hint(if close.is_empty() {
                "Runner ids look like \"<machine-id>/<runner-name>\"; list_runners shows them all.".to_string()
            } else {
                format!("Did you mean: {}", close.join(", "))
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Freshness

/// Picks up other processes' changes, and syncs GitHub when it's stale.
async fn freshen(engine: &Arc<Engine>) -> Arc<Snapshot> {
    engine.refresh_from_disk();
    let s = engine.current();
    let stale = s.github.last_sync_at.is_none_or(|t| (Utc::now() - t).num_seconds() > GITHUB_MAX_AGE_SECS);
    // Unwatched GitHub still needs the token looked up once, or status reports it missing.
    let token_unknown = !s.github.configured && s.github.token_source.is_empty();
    if ((s.github.configured || s.demo) && stale) || token_unknown {
        engine.sync_github().await;
    }
    engine.current()
}

/// Waits until a machine has a check result and isn't being checked.
async fn wait_checked(engine: &Arc<Engine>, machine_id: &str, timeout: Duration) {
    let mut rx = engine.subscribe();
    let done = |s: &Snapshot| s.machines.iter().find(|m| m.id == machine_id).is_none_or(|m| !m.checking && m.probe.is_some());
    let _ = tokio::time::timeout(timeout, async {
        loop {
            if done(&rx.borrow_and_update()) {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await;
}

/// Waits for an operation to leave "running", calling `on_line` for every new log line.
/// Returns the operation as it is when it finishes or the timeout passes, or None if it's unknown.
pub async fn wait_operation(engine: &Arc<Engine>, id: u64, timeout: Duration, mut on_line: impl FnMut(&str)) -> Option<Operation> {
    let mut rx = engine.subscribe();
    let mut printed = 0;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let op = rx.borrow_and_update().operations.iter().find(|o| o.id == id).cloned()?;
        for line in op.log.iter().skip(printed) {
            on_line(line);
        }
        printed = op.log.len();
        if op.status != "running" {
            return Some(op);
        }
        match tokio::time::timeout_at(deadline, rx.changed()).await {
            Ok(Ok(())) => continue,
            _ => return Some(op),
        }
    }
}

// ---------------------------------------------------------------------------
// Read

pub async fn status(engine: &Arc<Engine>, req: StatusRequest, surface: Surface) -> ApiResult<StatusResponse> {
    let s = if req.refresh {
        engine.refresh_from_disk();
        engine.run_check(None).await;
        engine.sync_github().await;
        engine.current()
    } else {
        freshen(engine).await
    };
    Ok(status_from(&s, surface))
}

pub fn status_from(s: &Snapshot, surface: Surface) -> StatusResponse {
    let c = &s.counts;
    let g = &s.github;
    let token = (!g.token_source.is_empty()).then(|| g.token_source.clone());

    let gh_hint = if token.is_none() && !s.demo {
        Some("No GitHub token: run `gh auth login`, or set GITHUB_TOKEN.".to_string())
    } else if !g.configured {
        Some(match g.detected.first() {
            Some(d) => surface.say(
                format!("Not watching GitHub yet. Runner configs mention \"{d}\": call github_scopes with add=[\"{d}\"]."),
                format!("Not watching GitHub yet. Runner configs mention \"{d}\": run `muxin watch {d}`."),
            ),
            None => surface.say(
                "Not watching GitHub yet: call github_scopes with add=[\"<org>\"] or [\"owner/repo\"].",
                "Not watching GitHub yet: run `muxin watch <org>` (or owner/repo).",
            ),
        })
    } else {
        g.scopes.iter().find_map(|sc| sc.hint.clone())
    };

    let attention = s
        .attention
        .iter()
        .map(|a| {
            let target = a.action.as_ref().and_then(|x| x.target.clone());
            let kind = a.action.as_ref().map(|x| x.kind.as_str()).unwrap_or("");
            AttentionOut {
                level: a.level.clone(),
                title: a.title.clone(),
                detail: a.detail.clone(),
                machine_id: (kind == "machine").then(|| target.clone()).flatten(),
                runner_id: (kind == "runner").then_some(target).flatten(),
            }
        })
        .collect::<Vec<_>>();

    let never_checked = s.check.finished_at.is_none() && s.machines.iter().all(|m| m.probe.is_none());
    let summary = format!(
        "{} machine{}, {} runner{}: {} working, {} idle, {} offline{}. {} queued job{}, {} running.{}",
        c.machines,
        plural(c.machines),
        c.runners,
        plural(c.runners),
        c.working,
        c.idle,
        c.offline,
        if c.unknown > 0 { format!(", {} unknown", c.unknown) } else { String::new() },
        c.queued,
        plural(c.queued),
        c.running,
        match attention.len() {
            0 => " Nothing needs attention.".to_string(),
            n => format!(" {n} item{} need{} attention.", plural(n), if n == 1 { "s" } else { "" }),
        }
    );

    StatusResponse {
        summary,
        counts: Counts {
            machines: c.machines,
            runners: c.runners,
            working: c.working,
            idle: c.idle,
            offline: c.offline,
            unknown: c.unknown,
            queued_jobs: c.queued,
            running_jobs: c.running,
        },
        github: GitHubOut {
            configured: g.configured,
            live: g.live,
            token,
            login: g.login.clone(),
            watching: g.scopes.iter().map(|sc| ScopeOut { kind: sc.kind.clone(), name: sc.name.clone(), runners: sc.runners, error: sc.error.clone(), hint: sc.hint.clone() }).collect(),
            detected: g.detected.clone(),
            last_sync_at: g.last_sync_at,
            error: g.error.clone(),
            hint: gh_hint,
        },
        last_check_at: s.check.finished_at,
        attention,
        machines: s.machines.iter().map(machine_summary).collect(),
        inventory: InventoryOut { path: s.inventory.path.clone(), error: s.inventory.error.clone() },
        hint: never_checked.then(|| {
            surface.say(
                "No machine has been checked yet; call check_machines (or status with refresh=true).",
                "No machine has been checked yet; run `muxin status --refresh` (or `muxin check`).",
            )
        }),
        demo: s.demo,
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub async fn machine(engine: &Arc<Engine>, req: MachineRequest) -> ApiResult<MachineDetail> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?;
    Ok(machine_detail(&s, m))
}

pub async fn machines(engine: &Arc<Engine>) -> ApiResult<Vec<MachineSummary>> {
    let s = freshen(engine).await;
    Ok(s.machines.iter().map(machine_summary).collect())
}

pub async fn runners(engine: &Arc<Engine>, req: RunnersRequest) -> ApiResult<Vec<RunnerOut>> {
    let s = freshen(engine).await;
    if let Some(m) = &req.machine_id {
        find_machine(&s, m)?;
    }
    if let Some(st) = &req.state {
        if !matches!(st.as_str(), "working" | "idle" | "offline" | "unknown") {
            return Err(ApiError::invalid(format!("Unknown state \"{st}\". Use working, idle, offline or unknown.")));
        }
    }
    let machine_id = req.machine_id.as_ref().and_then(|m| find_machine(&s, m).ok()).map(|m| m.id.clone());
    Ok(s.runners
        .iter()
        .filter(|r| machine_id.as_ref().is_none_or(|m| r.machine_id.as_ref() == Some(m)))
        .filter(|r| req.state.as_ref().is_none_or(|st| &r.state == st))
        .map(|r| runner_out(&s, r))
        .collect())
}

pub async fn jobs(engine: &Arc<Engine>, surface: Surface) -> ApiResult<JobsResponse> {
    let s = freshen(engine).await;
    let stuck = s.jobs.queued.iter().filter(|j| j.matching_online == 0).count();
    Ok(JobsResponse {
        summary: format!(
            "{} queued{}, {} running, across {} scanned repo{}.",
            s.jobs.queued.len(),
            if stuck > 0 { format!(" ({stuck} with no online runner that matches)") } else { String::new() },
            s.jobs.running.len(),
            s.github.scanned_repos.len(),
            plural(s.github.scanned_repos.len())
        ),
        queued: s.jobs.queued.iter().map(job_out).collect(),
        running: s.jobs.running.iter().map(job_out).collect(),
        scanned_repos: s.github.scanned_repos.clone(),
        hint: (!s.github.configured && !s.demo).then(|| {
            surface.say(
                "Jobs come from GitHub: call github_scopes with add=[\"<org>\"] first.",
                "Jobs come from GitHub: run `muxin watch <org>` first.",
            )
        }),
    })
}

pub fn operations(engine: &Arc<Engine>) -> Vec<OperationOut> {
    engine.current().operations.iter().map(|o| operation_out(o, 0)).collect()
}

pub fn operation(engine: &Arc<Engine>, req: OperationRequest) -> ApiResult<OperationOut> {
    let s = engine.current();
    let o = s.operations.iter().find(|o| o.id == req.operation_id).ok_or_else(|| {
        ApiError::not_found(format!("No operation {} in this Muxin process.", req.operation_id))
            .with_hint("Operation ids are per process; list_operations shows the ones this server started.")
    })?;
    Ok(operation_out(o, req.tail.unwrap_or(DEFAULT_TAIL)))
}

pub fn settings(engine: &Arc<Engine>) -> SettingsOut {
    engine.refresh_from_disk();
    let s = engine.settings();
    let p = engine.paths();
    SettingsOut {
        github_orgs: s.github.orgs,
        github_repos: s.github.repos,
        github_poll_seconds: s.github_poll_seconds,
        scan_repos: s.github.scan_repos,
        check_interval_minutes: s.check_interval_minutes,
        api_url: s.github.api_url,
        config_dir: crate::config::display_path(&p.dir),
        inventory: engine.current().inventory.path.clone(),
    }
}

pub async fn ssh_hosts(engine: &Arc<Engine>) -> SshHostsResponse {
    // The demo makes its own up: nothing of this computer's shows in it.
    if engine.demo_world().is_some() {
        return demo_ssh_hosts();
    }
    let hosts = crate::ops::ssh_hosts();
    // Listening on the local network takes a moment; do it meanwhile.
    let listening = tokio::task::spawn_blocking(|| crate::fleet::discover::browse(Duration::from_millis(2500)));
    let mut tailnet = Vec::new();
    if let Some(t) = crate::fleet::tailscale::status().await.filter(|t| t.running()) {
        // Where each alias really goes, to list a device under the user's own
        // name for it.
        let resolved: Vec<(String, Option<String>)> = futures::future::join_all(hosts.iter().map(|h| async move { (h.clone(), ssh_hostname(h).await) })).await;
        let snap = engine.current();
        for p in t.peers.iter().filter(|p| p.is_computer()) {
            let alias = resolved
                .iter()
                .find(|(a, to)| {
                    let names: Vec<String> = std::iter::once(a.clone()).chain(to.clone()).collect();
                    t.find(&[], &names).is_some_and(|f| f.dns_name == p.dns_name)
                })
                .map(|(a, _)| a.clone());
            let host = alias.unwrap_or_else(|| p.dns_name.clone());
            let machine_id = snap
                .machines
                .iter()
                .find(|m| m.tailnet.as_ref().is_some_and(|d| d.dns_name == p.dns_name) || m.host.eq_ignore_ascii_case(&host))
                .map(|m| m.id.clone());
            tailnet.push(TailnetHost { name: p.host_name.clone(), host, dns_name: p.dns_name.clone(), os: p.os.clone(), online: p.online, machine_id });
        }
        tailnet.sort_by(|a, b| b.online.cmp(&a.online).then(a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())));
    }
    // What's on this network, minus this computer, the fleet, and the tailnet's list.
    let me = crate::config::local_hostname().to_ascii_lowercase();
    // This computer by its address too: its Bonjour name can differ from its hostname.
    let my_ip = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| s.connect("1.1.1.1:53").and_then(|_| s.local_addr())).map(|a| a.ip().to_string()).ok();
    let snap = engine.current();
    let short = |s: &str| s.trim_end_matches('.').trim_end_matches(".local").to_ascii_lowercase();
    let network = listening
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|n| short(&n.host) != me && short(&n.name) != me && !my_ip.as_ref().is_some_and(|ip| n.addresses.contains(ip)))
        .filter(|n| {
            !snap.machines.iter().any(|m| {
                short(&m.host) == short(&n.host) || m.probe.as_ref().and_then(|p| p.hostname.as_deref()).is_some_and(|h| short(h) == short(&n.host) || short(h) == short(&n.name))
            })
        })
        .filter(|n| !tailnet.iter().any(|t| t.name.eq_ignore_ascii_case(&n.name) || short(&t.dns_name).split('.').next() == Some(short(&n.host).as_str())))
        .collect();
    SshHostsResponse { hosts, tailnet, network }
}

/// The host an ssh alias really connects to (its config's HostName).
async fn ssh_hostname(alias: &str) -> Option<String> {
    let out = tokio::time::timeout(Duration::from_secs(3), tokio::process::Command::new("ssh").args(["-G", alias]).output()).await.ok()?.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix("hostname ").map(|h| h.trim().to_string())).filter(|h| !h.is_empty())
}

// ---------------------------------------------------------------------------
// Act

pub async fn check(engine: &Arc<Engine>, req: CheckRequest) -> ApiResult<Vec<MachineSummary>> {
    engine.refresh_from_disk();
    let only = match &req.machine_id {
        Some(id) => {
            let s = engine.current();
            let m = find_machine(&s, id)?;
            if m.host.is_empty() {
                return Err(ApiError::invalid(format!("{} has no host in config.toml, so it can't be checked.", m.name)));
            }
            Some(m.id.clone())
        }
        None => None,
    };
    // This also waits out a check of the same machine that was already in flight.
    engine.run_check(only.clone().map(|id| vec![id])).await;
    engine.sync_github().await;
    let s = engine.current();
    Ok(s.machines.iter().filter(|m| only.as_ref().is_none_or(|id| &m.id == id)).map(machine_summary).collect())
}

pub async fn add_machine(engine: &Arc<Engine>, req: AddMachineRequest, surface: Surface) -> ApiResult<AddMachineResponse> {
    engine.refresh_from_disk();
    let id = engine.add_machine(NewMachine { name: req.name, host: req.host }).map_err(op_error)?;
    let machine = if req.check {
        wait_checked(engine, &id, CHECK_WAIT).await;
        let s = engine.current();
        s.machines.iter().find(|m| m.id == id).map(|m| machine_detail(&s, m))
    } else {
        None
    };
    let next = match machine.as_ref().map(|m| m.summary.status.as_str()) {
        Some("online") => surface.say(
            format!("Machine is reachable. Install runners with install_runners(machine_id=\"{id}\", count=N)."),
            format!("Machine is reachable. Install runners with `muxin install {id} --count N`."),
        ),
        Some("offline") => surface.say(
            "The machine didn't answer over ssh. Make sure key-based `ssh <host>` works without a password prompt, then call check_machines.",
            "The machine didn't answer over ssh. Make sure key-based `ssh <host>` works without a password prompt, then run `muxin check`.",
        ),
        _ => surface.say(format!("Call check_machines(machine_id=\"{id}\") to check it."), "Run `muxin check` to check it.".to_string()),
    };
    Ok(AddMachineResponse { machine_id: id, machine, next })
}

/// Disconnects from a machine (Muxin stops connecting to it, checking it and
/// looking at its sessions, which go on there) or connects again (and checks
/// it, waiting for that).
pub async fn connect_machine(engine: &Arc<Engine>, req: ConnectMachineRequest) -> ApiResult<MachineSummary> {
    engine.refresh_from_disk();
    let id = find_machine(&engine.current(), &req.machine_id)?.id.clone();
    engine.set_connected(&id, req.connected).map_err(op_error)?;
    if req.connected {
        engine.run_check(Some(vec![id.clone()])).await;
    }
    let s = engine.current();
    Ok(machine_summary(find_machine(&s, &id)?))
}

/// Gives a machine a new name; its id (and everything tied to it) stays.
pub async fn rename_machine(engine: &Arc<Engine>, req: RenameMachineRequest) -> ApiResult<MachineSummary> {
    engine.refresh_from_disk();
    let id = find_machine(&engine.current(), &req.machine_id)?.id.clone();
    engine.rename_machine(&id, &req.name).map_err(op_error)?;
    let s = engine.current();
    Ok(machine_summary(find_machine(&s, &id)?))
}

pub async fn remove_machine(engine: &Arc<Engine>, req: MachineRequest) -> ApiResult<MachineSummary> {
    engine.refresh_from_disk();
    let s = engine.current();
    let m = find_machine(&s, &req.machine_id)?.clone();
    engine.remove_machine(&m.id).map_err(op_error)?;
    Ok(machine_summary(&m))
}

pub async fn install_runners(engine: &Arc<Engine>, req: InstallRunnersRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    let s = freshen(engine).await;
    let machine_id = find_machine(&s, &req.machine_id)?.id.clone();

    let scope = match req.scope.map(|x| x.trim().to_string()).filter(|x| !x.is_empty()) {
        Some(scope) => scope,
        None => {
            let settings = engine.settings();
            let watched: Vec<String> = settings.github.orgs.iter().chain(settings.github.repos.iter()).cloned().collect();
            match watched.as_slice() {
                [one] => one.clone(),
                [] => {
                    return Err(ApiError::invalid("Say which GitHub org or repo to register the runners with (scope).").with_hint(surface.say(
                        "Watch it first with github_scopes(add=[\"<org>\"]), or pass scope=\"<org>\" / \"owner/repo\".",
                        "Watch it first with `muxin watch <org>`, or pass --scope <org> / owner/repo.",
                    )));
                }
                many => return Err(ApiError::invalid(format!("Several orgs/repos are watched ({}); pass scope.", many.join(", ")))),
            }
        }
    };

    let names = if req.names.is_empty() {
        let count = req.count.unwrap_or(1);
        if count == 0 || count > 32 {
            return Err(ApiError::invalid("count must be between 1 and 32."));
        }
        engine.suggest_runner_names(&machine_id, count)
    } else {
        req.names
    };

    let mode = InstallMode::parse(req.mode.as_deref().unwrap_or("auto"), req.image, req.docker_socket).map_err(ApiError::invalid)?;
    let op = engine
        .install_runners(InstallRequest {
            machine_id: machine_id.clone(),
            scope,
            names: names.clone(),
            labels: req.labels,
            group: req.runner_group,
            as_service: req.as_service,
            mode,
        })
        .map_err(op_error)?;
    finish_action(engine, op, req.wait, req.timeout_secs.unwrap_or(INSTALL_TIMEOUT_SECS), DEFAULT_TAIL, Some(machine_id), names, surface, on_line).await
}

pub async fn runner_action(engine: &Arc<Engine>, req: RunnerActionRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    let s = freshen(engine).await;
    let runner = find_runner(&s, &req.runner_id)?.clone();

    // Control needs the runner's folder from a machine check; get one if this process has none.
    if let Some(mid) = &runner.machine_id {
        let unchecked = s.machines.iter().find(|m| &m.id == mid).is_some_and(|m| m.probe.is_none());
        if unchecked && req.action != ActionName::Remove {
            engine.run_check(Some(vec![mid.clone()])).await;
            wait_checked(engine, mid, CHECK_WAIT).await;
        }
    }

    let action = match req.action {
        ActionName::Start => RunnerAction::Start,
        ActionName::Stop => RunnerAction::Stop,
        ActionName::Restart => RunnerAction::Restart,
        ActionName::Logs => RunnerAction::Logs,
        ActionName::Remove => RunnerAction::Remove { delete_files: req.delete_files },
    };
    let op = engine.runner_action(&runner.id, action).map_err(op_error)?;
    let tail = if req.action == ActionName::Logs { LOGS_TAIL } else { DEFAULT_TAIL };
    let timeout = req.timeout_secs.unwrap_or(ACTION_TIMEOUT_SECS);
    finish_action(engine, op, req.wait, timeout, tail, runner.machine_id.clone(), vec![runner.name.clone()], surface, on_line).await
}

/// The DigitalOcean account, regions, sizes and defaults for a new cloud machine.
pub async fn cloud_options(engine: &Arc<Engine>) -> crate::cloud::CloudOptions {
    engine.cloud_options().await
}

/// What the cloud machines cost: each one, this month, by the month's end,
/// and the DigitalOcean account's bill when the sign-in may read it.
pub async fn cloud_usage(engine: &Arc<Engine>) -> crate::cloud::usage::CloudUsage {
    freshen(engine).await;
    engine.cloud_usage().await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudCreateRequest {
    pub name: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    /// What it comes with, by id (cloud::setup::EXTRAS: tools, languages, agent CLIs, tailscale).
    #[serde(default)]
    pub install: Option<Vec<String>>,
    /// Wait until it's set up and added (default true; takes a few minutes).
    #[serde(default = "yes")]
    pub wait: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Creates a DigitalOcean droplet and adds it to the machines.
pub async fn cloud_create(engine: &Arc<Engine>, req: CloudCreateRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    freshen(engine).await;
    let machine = crate::cloud::CloudCreate {
        name: req.name,
        region: req.region,
        size: req.size,
        install: req.install,
    };
    let op = engine.cloud_create(machine).map_err(op_error)?;
    finish_action(engine, op, req.wait, req.timeout_secs.unwrap_or(1800), DEFAULT_TAIL, None, Vec::new(), surface, on_line).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudMachineRequest {
    pub machine_id: String,
    /// For power: "on" or "off".
    #[serde(default)]
    pub power: Option<String>,
    #[serde(default = "yes")]
    pub wait: bool,
}

/// Destroys a cloud machine: deletes the droplet and removes it from Muxin.
pub async fn cloud_destroy(engine: &Arc<Engine>, req: CloudMachineRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?.clone();
    let op = engine.cloud_destroy(&m.id).map_err(op_error)?;
    finish_action(engine, op, req.wait, 300, DEFAULT_TAIL, Some(m.id), Vec::new(), surface, on_line).await
}

/// Powers a cloud machine on or off (it's still billed while off).
pub async fn cloud_power(engine: &Arc<Engine>, req: CloudMachineRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?.clone();
    let on = match req.power.as_deref() {
        Some("on") => true,
        Some("off") => false,
        _ => return Err(ApiError::invalid("power must be \"on\" or \"off\".")),
    };
    let op = engine.cloud_power(&m.id, on).map_err(op_error)?;
    finish_action(engine, op, req.wait, 300, DEFAULT_TAIL, Some(m.id), Vec::new(), surface, on_line).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleListRequest {
    #[serde(default)]
    pub machine_id: Option<String>,
}

/// The schedules on each online machine (or one), with each one's last run.
pub async fn schedule_list(engine: &Arc<Engine>, req: ScheduleListRequest) -> ApiResult<Vec<crate::fleet::schedules::MachineSchedules>> {
    let s = freshen(engine).await;
    let id = match &req.machine_id {
        Some(m) => Some(find_machine(&s, m)?.id.clone()),
        None => None,
    };
    Ok(engine.schedules(id.as_deref()).await)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSaveRequest {
    pub machine_id: String,
    /// Given: change that schedule.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub when: crate::fleet::schedules::When,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// Saves a schedule on a machine: new, or changed when `id` is given.
pub async fn schedule_save(engine: &Arc<Engine>, req: ScheduleSaveRequest) -> ApiResult<crate::fleet::schedules::Schedule> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?.clone();
    let input = crate::fleet::schedules::ScheduleInput { id: req.id, name: req.name, when: req.when, command: req.command, agent: req.agent, prompt: req.prompt, folder: req.folder, enabled: req.enabled };
    engine.schedule_save(&m.id, input).await.map_err(|e| ApiError::new("operation_failed", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    pub machine_id: String,
    pub schedule_id: String,
    /// schedule_log: the run that started then (Unix seconds); default the newest.
    #[serde(default)]
    pub started: Option<i64>,
}

pub async fn schedule_delete(engine: &Arc<Engine>, req: ScheduleRequest) -> ApiResult<serde_json::Value> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?.clone();
    engine.schedule_delete(&m.id, &req.schedule_id).await.map_err(|e| ApiError::new("operation_failed", e))?;
    Ok(serde_json::json!({ "deleted": req.schedule_id }))
}

pub async fn schedule_run(engine: &Arc<Engine>, req: ScheduleRequest) -> ApiResult<serde_json::Value> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?.clone();
    engine.schedule_run(&m.id, &req.schedule_id).await.map_err(|e| ApiError::new("operation_failed", e))?;
    Ok(serde_json::json!({ "started": req.schedule_id, "next": "It runs in the background there. Read its output with schedule_log." }))
}

pub async fn schedule_log(engine: &Arc<Engine>, req: ScheduleRequest) -> ApiResult<crate::fleet::schedules::ScheduleLog> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?.clone();
    engine.schedule_log(&m.id, &req.schedule_id, req.started).await.map_err(|e| ApiError::new("operation_failed", e))
}

/// The machines VMs can run on (Docker; Windows also needs Linux with KVM),
/// what each can run, and the defaults.
pub async fn vm_options(engine: &Arc<Engine>) -> crate::ops::vm::VmOptions {
    freshen(engine).await;
    engine.vm_options()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmCreateRequest {
    pub name: String,
    /// A system id from vm_options: "windows-11", "ubuntu-24.04", ...
    pub system: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub cpus: Option<u32>,
    #[serde(default)]
    pub memory_gb: Option<u32>,
    #[serde(default)]
    pub disk_gb: Option<u32>,
    /// Agent CLIs to install on a Linux VM: claude, codex, gemini, opencode, amp, pi.
    #[serde(default)]
    pub install: Vec<String>,
    #[serde(default = "yes")]
    pub wait: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Creates a VM on one of the user's machines and adds it to them.
pub async fn vm_create(engine: &Arc<Engine>, req: VmCreateRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    freshen(engine).await;
    let id = crate::util::slug(req.name.trim());
    let create = crate::ops::vm::VmCreate { name: req.name, system: req.system, host: req.host, cpus: req.cpus, memory_gb: req.memory_gb, disk_gb: req.disk_gb, install: req.install };
    let op = engine.vm_create(create).map_err(op_error)?;
    finish_action(engine, op, req.wait, req.timeout_secs.unwrap_or(600), DEFAULT_TAIL, Some(id), Vec::new(), surface, on_line).await
}

/// Destroys a VM Muxin made: its container and disk, and the machine in Muxin.
pub async fn vm_destroy(engine: &Arc<Engine>, req: CloudMachineRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?.clone();
    let op = engine.vm_destroy(&m.id).map_err(op_error)?;
    finish_action(engine, op, req.wait, 300, DEFAULT_TAIL, None, Vec::new(), surface, on_line).await
}

/// Powers a machine on or off: a cloud machine through its provider, or a VM
/// (or container) in Docker on another of the user's machines by starting or
/// stopping that container there.
pub async fn machine_power(engine: &Arc<Engine>, req: CloudMachineRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    let s = freshen(engine).await;
    let m = find_machine(&s, &req.machine_id)?.clone();
    let on = match req.power.as_deref() {
        Some("on") => true,
        Some("off") => false,
        _ => return Err(ApiError::invalid("power must be \"on\" or \"off\".")),
    };
    let op = engine.machine_power(&m.id, on).map_err(op_error)?;
    finish_action(engine, op, req.wait, 420, DEFAULT_TAIL, Some(m.id), Vec::new(), surface, on_line).await
}

/// Cancels the workflow run a queued or running job belongs to.
pub async fn cancel_run(engine: &Arc<Engine>, req: CancelRunRequest, surface: Surface, on_line: impl FnMut(&str)) -> ApiResult<ActionResponse> {
    freshen(engine).await;
    let op = engine.cancel_run(req.job_id).map_err(op_error)?;
    finish_action(engine, op, req.wait, req.timeout_secs.unwrap_or(60), DEFAULT_TAIL, None, Vec::new(), surface, on_line).await
}

#[allow(clippy::too_many_arguments)]
async fn finish_action(
    engine: &Arc<Engine>,
    op: u64,
    wait: bool,
    timeout_secs: u64,
    tail: usize,
    machine_id: Option<String>,
    runner_names: Vec<String>,
    surface: Surface,
    on_line: impl FnMut(&str),
) -> ApiResult<ActionResponse> {
    let operation = if wait {
        wait_operation(engine, op, Duration::from_secs(timeout_secs.max(1)), on_line).await
    } else {
        engine.current().operations.iter().find(|o| o.id == op).cloned()
    }
    .ok_or_else(|| ApiError::new("internal", format!("Operation {op} disappeared.")))?;

    let s = engine.current();
    let finished = operation.status != "running";
    let names: HashSet<String> = runner_names.iter().map(|n| n.to_ascii_lowercase()).collect();
    let response = ActionResponse {
        finished,
        machine: machine_id.as_ref().and_then(|id| s.machines.iter().find(|m| &m.id == id)).map(machine_summary),
        runners: s.runners.iter().filter(|r| names.contains(&r.name.to_ascii_lowercase())).map(|r| runner_out(&s, r)).collect(),
        next: (!finished).then(|| {
            surface.say(
                format!("Still running. Poll get_operation(operation_id={op}) until status isn't \"running\"."),
                "Still running in the background; this command stopped waiting. Check the result with `muxin status`.",
            )
        }),
        operation: operation_out(&operation, tail),
    };

    if operation.status == "failed" {
        let message = operation.error.clone().unwrap_or_else(|| "The operation failed.".into());
        let hint = failure_hint(&message);
        return Err(ApiError {
            code: "operation_failed",
            message,
            hint,
            details: serde_json::to_value(&response).ok(),
        });
    }
    Ok(response)
}

/// A next step for common failures, when the message doesn't already carry one.
fn failure_hint(message: &str) -> Option<String> {
    let m = message.to_ascii_lowercase();
    if m.contains("admin:org") {
        Some("gh auth refresh -h github.com -s admin:org".into())
    } else if m.contains("sudo") {
        Some("Allow passwordless sudo for the runner user on that machine, or install with mode \"docker\" or \"background\".".into())
    } else if m.contains("timed out") || m.contains("connection refused") || m.contains("could not resolve") {
        Some("The machine didn't answer over ssh. Check that `ssh <host>` works without a password prompt.".into())
    } else {
        None
    }
}

pub async fn github_scopes(engine: &Arc<Engine>, req: GitHubScopesRequest) -> ApiResult<ScopesResponse> {
    engine.refresh_from_disk();
    let mut settings = engine.settings();
    let mut results = Vec::new();

    for raw in &req.remove {
        let name = crate::config::strip_github_url(raw);
        let before = settings.github.orgs.len() + settings.github.repos.len();
        settings.github.orgs.retain(|o| !o.eq_ignore_ascii_case(&name));
        settings.github.repos.retain(|r| !r.eq_ignore_ascii_case(&name));
        let removed = settings.github.orgs.len() + settings.github.repos.len() < before;
        results.push(ScopeResult {
            scope: name,
            result: if removed { "removed" } else { "not_watching" }.into(),
            runners: None,
            error: None,
            hint: None,
        });
    }

    for raw in &req.add {
        let name = crate::config::strip_github_url(raw);
        if name.is_empty() {
            continue;
        }
        let watched = settings.github.orgs.iter().chain(settings.github.repos.iter()).any(|x| x.eq_ignore_ascii_case(&name));
        if watched {
            results.push(ScopeResult { scope: name, result: "already_watching".into(), runners: None, error: None, hint: None });
            continue;
        }
        let t = engine.test_scope(&name).await;
        if t.error.is_none() {
            match t.kind {
                crate::github::ScopeKind::Repo => settings.github.repos.push(t.name.clone()),
                crate::github::ScopeKind::Org => settings.github.orgs.push(t.name.clone()),
            }
            results.push(ScopeResult { scope: t.name, result: "added".into(), runners: Some(t.runners), error: None, hint: None });
        } else {
            results.push(ScopeResult { scope: t.name, result: "failed".into(), runners: None, error: t.error, hint: t.hint });
        }
    }

    engine.update_settings(settings).map_err(ApiError::invalid)?;
    engine.sync_github().await;
    let s = engine.current();
    let watching = s
        .github
        .scopes
        .iter()
        .map(|sc| ScopeOut { kind: sc.kind.clone(), name: sc.name.clone(), runners: sc.runners, error: sc.error.clone(), hint: sc.hint.clone() })
        .collect();
    Ok(ScopesResponse { results, watching })
}

pub fn update_settings(engine: &Arc<Engine>, req: UpdateSettingsRequest) -> ApiResult<SettingsOut> {
    engine.refresh_from_disk();
    let mut s = engine.settings();
    if let Some(v) = req.github_poll_seconds {
        s.github_poll_seconds = v;
    }
    if let Some(v) = req.scan_repos {
        s.github.scan_repos = v;
    }
    if let Some(v) = req.check_interval_minutes {
        s.check_interval_minutes = v;
    }
    if let Some(v) = req.api_url {
        s.github.api_url = v;
    }
    engine.update_settings(s).map_err(ApiError::invalid)?;
    Ok(settings(engine))
}

// ---------------------------------------------------------------------------
// Workspaces: a persistent work environment on one of the user's machines.

const EXEC_DEFAULT_TIMEOUT_SECS: u64 = 600;
const EXEC_MAX_TIMEOUT_SECS: u64 = 3600;
const READ_DEFAULT_BYTES: usize = 256 * 1024;
const READ_MAX_BYTES: usize = 2 * 1024 * 1024;
const LIST_DEFAULT_DEPTH: u32 = 2;
const LIST_MAX_DEPTH: u32 = 10;
const HISTORY_IN_GET: usize = 20;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceCreateRequest {
    /// Machine id to use. Omit to let Muxin pick the least-loaded online machine that allows workspaces.
    pub machine_id: Option<String>,
    /// When picking a machine: platform prefix it must match, e.g. "linux", "linux-x64", "darwin-arm64", "windows".
    pub platform: Option<String>,
    /// Short label, e.g. "api-tests". Defaults to the repo name.
    pub name: Option<String>,
    /// Git URL, or "owner/repo" for GitHub, to clone into the workspace.
    pub repo: Option<String>,
    /// Branch, tag or commit to check out; "pull/123/head" for a pull request's code.
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// New branch to create and switch to after checkout.
    pub branch: Option<String>,
    /// "host" (default): run directly on the machine as its ssh user. "docker": a fresh
    /// container per workspace with the folder mounted at /workspace.
    #[serde(default)]
    pub isolation: Option<String>,
    /// Docker image for isolation "docker", e.g. "node:22", "python:3.13", "rust:1".
    #[serde(default)]
    pub image: Option<String>,
    /// Share the machine's Docker socket with the container (docker-in-docker; root-equivalent).
    #[serde(default)]
    pub docker_socket: bool,
    /// A git checkout's folder on the machine (from local_checkouts): the workspace is a new git
    /// worktree of it, on `branch` (from `ref`, else the default branch), instead of a fresh clone,
    /// so several agents can work on one repository at once. Needs machine_id; not with docker.
    #[serde(default)]
    pub worktree_of: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRequest {
    /// Workspace id, e.g. "ws-7f3a2c".
    pub workspace_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceExecRequest {
    pub workspace_id: String,
    /// Shell command, run with the machine user's login shell environment.
    pub command: String,
    /// Folder relative to the workspace to run in.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Kill it after this many seconds (default 600, max 3600). Ignored with background.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Start it detached (servers, watchers) and return right away with its pid and log file.
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceReadRequest {
    pub workspace_id: String,
    /// File path relative to the workspace.
    pub path: String,
    /// Read at most this many bytes (default 262144, max 2097152).
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceWriteRequest {
    pub workspace_id: String,
    /// File path relative to the workspace; parent folders are created.
    pub path: String,
    /// Text content (UTF-8).
    pub content: String,
    /// Append instead of replacing.
    #[serde(default)]
    pub append: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFilesRequest {
    pub workspace_id: String,
    /// Folder relative to the workspace (default: the workspace itself).
    #[serde(default)]
    pub path: Option<String>,
    /// How deep to list (default 2, max 10).
    #[serde(default)]
    pub depth: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSyncRequest {
    pub workspace_id: String,
    /// Folder on this computer. Relative paths are resolved against this process's working directory.
    pub local_path: String,
    /// "push" copies the local folder into the workspace; "pull" copies the workspace into the local folder.
    pub direction: SyncDirection,
    /// Also delete files on the receiving side that the sending side doesn't have.
    #[serde(default)]
    pub delete: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDestroyRequest {
    pub workspace_id: String,
    /// Keep the folder on the machine; only forget the workspace.
    #[serde(default)]
    pub keep_files: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAllowRequest {
    pub machine_id: String,
    /// Whether agents may create workspaces on the machine.
    pub allowed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceOut {
    pub id: String,
    pub name: String,
    pub machine_id: String,
    pub machine_name: String,
    pub host: String,
    pub platform: String,
    /// Absolute path on the machine.
    pub path: String,
    pub repo: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub branch: Option<String>,
    pub owner: Option<String>,
    /// "host" or "docker".
    pub isolation: String,
    /// Docker image, for docker workspaces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Container name, for docker workspaces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Worktree workspaces: the checkout (a folder on the same machine) it's a git worktree of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_of: Option<String>,
    /// What the work in it changed, as last looked at: branch, files and lines since the
    /// default branch, uncommitted files, and the branch's pull request with its checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<crate::fleet::sessions::git::GitStatus>,
    /// "creating" | "ready" | "failed" | "destroying"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    /// Total commands run here.
    pub commands: usize,
    /// For a human at a terminal: opens a shell in the workspace.
    pub shell: String,
    pub last_command: Option<CommandOut>,
    /// Recent commands, newest last (workspace_get only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<CommandOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandOut {
    pub command: String,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub background: bool,
    pub timed_out: bool,
    pub by: Option<String>,
    pub output_tail: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct WorkspacesResponse {
    pub summary: String,
    pub workspaces: Vec<WorkspaceOut>,
    /// Machines that accept new workspaces (allowed, and not known to be unreachable).
    pub available_machines: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceCreated {
    pub workspace: WorkspaceOut,
    pub next: String,
}

#[derive(Debug, Serialize)]
pub struct ExecOut {
    pub workspace_id: String,
    pub command: String,
    /// The command's exit status; null when it timed out or runs in the background.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// Output was longer than the cap; the middle was dropped.
    pub truncated: bool,
    pub background_pid: Option<u32>,
    /// Background commands: where their output goes, relative to the workspace.
    pub log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FileOut {
    pub workspace_id: String,
    pub path: String,
    pub size: u64,
    pub truncated: bool,
    /// Not text; `content` is empty.
    pub binary: bool,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct WriteOut {
    pub workspace_id: String,
    pub path: String,
    /// File size after the write, in bytes.
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct FileEntryOut {
    pub path: String,
    /// "file" | "dir" | "link"
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct FilesOut {
    pub workspace_id: String,
    pub path: String,
    pub entries: Vec<FileEntryOut>,
}

#[derive(Debug, Serialize)]
pub struct ChangesOut {
    pub workspace_id: String,
    /// `git status` and the uncommitted diff (capped), or a note when it isn't a git checkout.
    pub changes: String,
}

#[derive(Debug, Serialize)]
pub struct SyncOut {
    pub workspace_id: String,
    pub local_path: String,
    pub direction: SyncDirection,
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct DestroyOut {
    pub workspace_id: String,
    pub kept_files: bool,
}

#[derive(Debug, Serialize)]
pub struct AllowOut {
    pub machine_id: String,
    pub workspaces_allowed: bool,
}

fn command_out(c: &WorkspaceCommand) -> CommandOut {
    CommandOut {
        command: c.command.clone(),
        exit_code: c.exit_code,
        started_at: c.started_at,
        duration_ms: c.duration_ms,
        background: c.background,
        timed_out: c.timed_out,
        by: c.by.clone(),
        output_tail: c.output_tail.clone(),
    }
}

pub fn workspace_out(w: &WorkspaceInfo, history: usize) -> WorkspaceOut {
    let start = w.history.len().saturating_sub(history);
    WorkspaceOut {
        id: w.id.clone(),
        name: w.name.clone(),
        machine_id: w.machine_id.clone(),
        machine_name: w.machine_name.clone(),
        host: w.host.clone(),
        platform: w.platform.clone(),
        path: w.path.clone(),
        repo: w.repo.clone(),
        git_ref: w.git_ref.clone(),
        branch: w.branch.clone(),
        isolation: w.isolation.clone(),
        image: w.image.clone(),
        container: w.container.clone(),
        worktree_of: w.worktree_of.clone(),
        git: w.git.clone(),
        owner: w.owner.clone(),
        status: w.status.clone(),
        error: w.error.clone(),
        created_at: w.created_at,
        last_used_at: w.last_used_at,
        commands: w.commands,
        shell: w.shell.clone(),
        last_command: w.history.last().map(command_out),
        history: if history == 0 { Vec::new() } else { w.history[start..].iter().map(command_out).collect() },
    }
}

/// Maps a message from the workspace backend onto an error code.
fn ws_error(message: String, surface: Surface) -> ApiError {
    let m = message.to_ascii_lowercase();
    let code = if m.contains("not implemented") || m.contains("implemented yet") {
        "internal"
    } else if m.contains("no workspace") || m.contains("unknown workspace") || m.contains("isn't in config.toml") || m.contains("no machine") || m.contains("not found") {
        "not_found"
    } else if m.contains("unreachable")
        || m.contains("timed out")
        || m.contains("connection refused")
        || m.contains("could not resolve")
        || m.contains("didn't answer")
        || m.contains("no route to host")
    {
        "unreachable"
    } else if m.contains("still being created") || m.contains("being destroyed") || m.contains("busy") {
        "busy"
    } else {
        "invalid"
    };
    let mut err = ApiError::new(code, message);
    if code == "unreachable" {
        err = err.with_hint("The machine didn't answer over ssh. Check that `ssh <host>` works without a password prompt, or pick another machine.");
    } else if m.contains("doesn't allow") || m.contains("not allowed") || m.contains("disabled") {
        err = err.with_hint(surface.say(
            "Turn workspaces on for that machine (workspace_allow, the toggle in the app, or `workspaces = true` in config.toml), or pick another machine.",
            "Turn workspaces on for that machine (`muxin workspace allow <machine-id>`, the toggle in the app, or `workspaces = true` in config.toml), or pick another machine.",
        ));
    } else if m.contains("rsync") {
        err = err.with_hint("Install rsync on both this computer and the machine.");
    } else if m.contains("docker isn't available") || m.contains("couldn't pull") || m.contains("couldn't start a container") {
        err = err.with_hint("Install Docker on that machine (and add the ssh user to the docker group), use isolation \"host\", or pick a machine where Docker works.");
    }
    err
}

/// The current workspaces, including ones other Muxin processes created.
fn current_workspaces(engine: &Arc<Engine>) -> Vec<WorkspaceInfo> {
    engine.refresh_from_disk();
    engine.workspaces()
}

fn find_workspace(engine: &Arc<Engine>, id: &str, surface: Surface) -> ApiResult<WorkspaceInfo> {
    let all = current_workspaces(engine);
    let id = id.trim();
    if let Some(w) = all.iter().find(|w| w.id == id) {
        return Ok(w.clone());
    }
    // A unique name works too, like machine names do.
    let by_name: Vec<&WorkspaceInfo> = all.iter().filter(|w| w.name.eq_ignore_ascii_case(id)).collect();
    if let [one] = by_name.as_slice() {
        return Ok((*one).clone());
    }
    Err(ApiError::not_found(format!("No workspace \"{id}\".")).with_hint(if all.is_empty() {
        surface.say("There are no workspaces; create one with workspace_create.", "There are no workspaces; create one with `muxin workspace create`.")
    } else {
        let known = all.iter().map(|w| w.id.as_str()).collect::<Vec<_>>().join(", ");
        surface.say(
            &format!("Known workspaces: {known}. Or create one with workspace_create."),
            &format!("Known workspaces: {known}. Or create one with `muxin workspace create`."),
        )
    }))
}

/// Paths inside a workspace must be relative and stay inside it.
fn workspace_path(raw: &str) -> ApiResult<String> {
    let p = raw.trim();
    let windows_abs = p.len() >= 2 && p.as_bytes()[1] == b':';
    if p.starts_with('/') || p.starts_with('\\') || p.starts_with('~') || windows_abs {
        return Err(ApiError::invalid(format!("\"{raw}\" isn't relative to the workspace.")).with_hint("Use a path relative to the workspace folder, e.g. \"src/main.rs\"."));
    }
    let mut depth: i32 = 0;
    for c in Path::new(&p.replace('\\', "/")).components() {
        match c {
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            return Err(ApiError::invalid(format!("\"{raw}\" points outside the workspace.")));
        }
    }
    Ok(p.to_string())
}

pub async fn workspaces_list(engine: &Arc<Engine>) -> WorkspacesResponse {
    // Each one's branch and pull request (a merged one means its work is done).
    let s = engine.current();
    let mut machines: Vec<String> = current_workspaces(engine).iter().filter(|w| w.status == "ready" && s.machines.iter().any(|m| m.id == w.machine_id && !m.is_down())).map(|w| w.machine_id.clone()).collect();
    machines.sort();
    machines.dedup();
    look_at_stale_git(engine, machines.iter().map(String::as_str)).await;
    let all = engine.workspaces();
    let s = engine.current();
    // Unchecked machines count: creating a workspace checks the machine first.
    let available: Vec<String> = s.machines.iter().filter(|m| m.workspaces_allowed && !m.is_down()).map(|m| m.id.clone()).collect();
    let ready = all.iter().filter(|w| w.status == "ready").count();
    WorkspacesResponse {
        summary: format!(
            "{} workspace{} ({} ready). {} machine{} available for new ones{}.",
            all.len(),
            plural(all.len()),
            ready,
            available.len(),
            plural(available.len()),
            if available.is_empty() { "" } else { ": " }.to_string() + &available.join(", ")
        ),
        workspaces: all.iter().map(|w| workspace_out(w, 0)).collect(),
        available_machines: available,
    }
}

pub fn workspace_get(engine: &Arc<Engine>, req: WorkspaceRequest, surface: Surface) -> ApiResult<WorkspaceOut> {
    Ok(workspace_out(&find_workspace(engine, &req.workspace_id, surface)?, HISTORY_IN_GET))
}

pub async fn workspace_create(engine: &Arc<Engine>, req: WorkspaceCreateRequest, owner: &str, surface: Surface) -> ApiResult<WorkspaceCreated> {
    engine.refresh_from_disk();
    let machine = match req.machine_id.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(id) => {
            let s = engine.current();
            let m = find_machine(&s, id)?;
            if !m.workspaces_allowed {
                return Err(ApiError::invalid(format!("{} doesn't allow workspaces.", m.name))
                    .with_hint(surface.say(
                        format!("Turn them on with workspace_allow(machine_id=\"{}\", allowed=true) if the user agrees, or pick another machine.", m.id),
                        format!("Turn them on with `muxin workspace allow {}`, or pick another machine.", m.id),
                    )));
            }
            Some(m.id.clone())
        }
        None => None,
    };
    let clean = |v: Option<String>| v.map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    let iso = crate::fleet::workspaces::WorkspaceIsolation {
        isolation: clean(req.isolation),
        image: clean(req.image),
        docker_socket: req.docker_socket,
    };
    let created = engine
        .create_workspace_with(
            CreateWorkspace {
                machine,
                platform: clean(req.platform),
                name: clean(req.name),
                repo: clean(req.repo),
                git_ref: clean(req.git_ref),
                branch: clean(req.branch),
                owner: Some(owner.to_string()),
                worktree_of: clean(req.worktree_of),
            },
            iso,
        )
        .await
        .map_err(|e| ws_error(e, surface))?;
    let id = &created.id;
    let next = surface.say(
        format!("Run commands with workspace_exec(workspace_id=\"{id}\", command=...). Review edits with workspace_changes. Call workspace_destroy(workspace_id=\"{id}\") when the task is done."),
        format!("Run commands with `muxin workspace exec {id} -- <command>`; review edits with `muxin workspace changes {id}`; destroy it with `muxin workspace rm {id}` when done."),
    );
    Ok(WorkspaceCreated { workspace: workspace_out(&created, 0), next })
}

pub async fn workspace_exec(engine: &Arc<Engine>, req: WorkspaceExecRequest, by: &str, surface: Surface) -> ApiResult<ExecOut> {
    let command = req.command.trim().to_string();
    if command.is_empty() {
        return Err(ApiError::invalid("command is empty."));
    }
    let timeout = req.timeout_secs.unwrap_or(EXEC_DEFAULT_TIMEOUT_SECS);
    if timeout == 0 || timeout > EXEC_MAX_TIMEOUT_SECS {
        return Err(ApiError::invalid(format!("timeout_secs must be between 1 and {EXEC_MAX_TIMEOUT_SECS}."))
            .with_hint(surface.say("For longer work, run it with background=true and read its log file.", "For longer work, run it with --background and read its log file.")));
    }
    for key in req.env.keys() {
        let valid = !key.is_empty() && !key.starts_with(|c: char| c.is_ascii_digit()) && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(ApiError::invalid(format!("\"{key}\" isn't a valid environment variable name.")));
        }
    }
    let cwd = req.cwd.as_deref().map(workspace_path).transpose()?.filter(|c| !c.is_empty() && c != ".");
    let w = find_workspace(engine, &req.workspace_id, surface)?;
    let result = engine
        .workspace_exec(
            &w.id,
            &command,
            ExecOptions {
                timeout_secs: Some(timeout),
                env: req.env.into_iter().collect(),
                cwd,
                background: req.background,
                by: Some(by.to_string()),
            },
        )
        .await
        .map_err(|e| ws_error(e, surface))?;
    let pid = result.background_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
    let next = if let Some(log) = &result.log_path {
        Some(surface.say(
            format!("Running in the background (pid {pid}). Read its output with workspace_read_file(path=\"{log}\") or workspace_exec(command=\"tail -n 50 {log}\")."),
            format!("Running in the background (pid {pid}). Read its output with `muxin workspace cat {} {log}`.", w.id),
        ))
    } else if result.timed_out {
        Some(surface.say(
            format!("Killed after {timeout}s. Raise timeout_secs (max {EXEC_MAX_TIMEOUT_SECS}) or run it with background=true."),
            format!("Killed after {timeout}s. Raise --timeout (max {EXEC_MAX_TIMEOUT_SECS}) or run it with --background."),
        ))
    } else {
        None
    };
    Ok(ExecOut {
        workspace_id: w.id,
        command,
        exit_code: result.exit_code,
        stdout: result.stdout,
        stderr: result.stderr,
        duration_ms: result.duration_ms,
        timed_out: result.timed_out,
        truncated: result.truncated,
        background_pid: result.background_pid,
        log_path: result.log_path,
        next,
    })
}

pub async fn workspace_read_file(engine: &Arc<Engine>, req: WorkspaceReadRequest, surface: Surface) -> ApiResult<FileOut> {
    let path = workspace_path(&req.path)?;
    if path.is_empty() {
        return Err(ApiError::invalid("path is empty."));
    }
    let max = req.max_bytes.unwrap_or(READ_DEFAULT_BYTES);
    if max == 0 || max > READ_MAX_BYTES {
        return Err(ApiError::invalid(format!("max_bytes must be between 1 and {READ_MAX_BYTES}.")));
    }
    let w = find_workspace(engine, &req.workspace_id, surface)?;
    let f = engine.workspace_read_file(&w.id, &path, max).await.map_err(|e| ws_error(e, surface))?;
    Ok(FileOut { workspace_id: w.id, path: f.path, size: f.size, truncated: f.truncated, binary: f.binary, content: f.content })
}

pub async fn workspace_write_file(engine: &Arc<Engine>, req: WorkspaceWriteRequest, surface: Surface) -> ApiResult<WriteOut> {
    let path = workspace_path(&req.path)?;
    if path.is_empty() || path.ends_with('/') {
        return Err(ApiError::invalid("path must name a file."));
    }
    let w = find_workspace(engine, &req.workspace_id, surface)?;
    let size = engine.workspace_write_file(&w.id, &path, &req.content, req.append).await.map_err(|e| ws_error(e, surface))?;
    Ok(WriteOut { workspace_id: w.id, path, size })
}

pub async fn workspace_list_files(engine: &Arc<Engine>, req: WorkspaceFilesRequest, surface: Surface) -> ApiResult<FilesOut> {
    let path = workspace_path(req.path.as_deref().unwrap_or(""))?;
    let depth = req.depth.unwrap_or(LIST_DEFAULT_DEPTH);
    if depth == 0 || depth > LIST_MAX_DEPTH {
        return Err(ApiError::invalid(format!("depth must be between 1 and {LIST_MAX_DEPTH}.")));
    }
    let w = find_workspace(engine, &req.workspace_id, surface)?;
    let entries = engine.workspace_list_files(&w.id, &path, depth).await.map_err(|e| ws_error(e, surface))?;
    Ok(FilesOut {
        workspace_id: w.id,
        path: if path.is_empty() { ".".into() } else { path },
        entries: entries.into_iter().map(|e| FileEntryOut { path: e.path, kind: e.kind, size: e.size }).collect(),
    })
}

pub async fn workspace_changes(engine: &Arc<Engine>, req: WorkspaceRequest, surface: Surface) -> ApiResult<ChangesOut> {
    let w = find_workspace(engine, &req.workspace_id, surface)?;
    let changes = engine.workspace_changes(&w.id).await.map_err(|e| ws_error(e, surface))?;
    Ok(ChangesOut { workspace_id: w.id, changes })
}

pub async fn workspace_sync(engine: &Arc<Engine>, req: WorkspaceSyncRequest, surface: Surface) -> ApiResult<SyncOut> {
    let raw = req.local_path.trim();
    if raw.is_empty() {
        return Err(ApiError::invalid("local_path is empty."));
    }
    let expanded = match raw.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| raw.into()),
        None => std::path::PathBuf::from(raw),
    };
    let local = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().map(|d| d.join(&expanded)).unwrap_or(expanded)
    };
    if req.direction == SyncDirection::Push && !local.is_dir() {
        return Err(ApiError::invalid(format!("{} isn't a folder on this computer.", local.display())));
    }
    let w = find_workspace(engine, &req.workspace_id, surface)?;
    let local_str = local.display().to_string();
    let summary = engine.workspace_sync(&w.id, &local_str, req.direction, req.delete).await.map_err(|e| ws_error(e, surface))?;
    Ok(SyncOut { workspace_id: w.id, local_path: local_str, direction: req.direction, summary })
}

pub async fn workspace_destroy(engine: &Arc<Engine>, req: WorkspaceDestroyRequest, surface: Surface) -> ApiResult<DestroyOut> {
    let w = find_workspace(engine, &req.workspace_id, surface)?;
    engine.destroy_workspace(&w.id, req.keep_files).await.map_err(|e| ws_error(e, surface))?;
    Ok(DestroyOut { workspace_id: w.id, kept_files: req.keep_files })
}

pub fn workspace_allow(engine: &Arc<Engine>, req: WorkspaceAllowRequest, surface: Surface) -> ApiResult<AllowOut> {
    engine.refresh_from_disk();
    let s = engine.current();
    let m = find_machine(&s, &req.machine_id)?;
    if m.host.is_empty() {
        return Err(ApiError::invalid(format!("{} has no host in config.toml, so it can't host workspaces.", m.name)));
    }
    let id = m.id.clone();
    engine.set_workspaces_allowed(&id, req.allowed).map_err(|e| ws_error(e, surface))?;
    Ok(AllowOut { machine_id: id, workspaces_allowed: req.allowed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    fn demo() -> Arc<Engine> {
        Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-api-test")), None), true)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_reads_the_demo_machines() {
        let engine = demo();
        let s = status(&engine, StatusRequest { refresh: true }, Surface::Mcp).await.unwrap();
        assert_eq!(s.counts.machines, 9);
        assert!(s.demo && s.github.live);
        assert!(s.summary.contains("9 machines"), "{}", s.summary);
        let harbor = s.machines.iter().find(|m| m.id == "harbor").unwrap();
        assert_eq!(harbor.status, "offline");
        assert!(harbor.error.is_some());
        assert!(s.attention.iter().any(|a| a.machine_id.as_deref() == Some("harbor")));

        let err = machine(&engine, MachineRequest { machine_id: "nope".into() }).await.unwrap_err();
        assert_eq!(err.code, "not_found");
        assert!(err.hint.unwrap().contains("forge"));
        let m = machine(&engine, MachineRequest { machine_id: "Forge".into() }).await.unwrap();
        assert_eq!(m.runners.len(), 3);
        assert!(runners(&engine, RunnersRequest { state: Some("busy".into()), ..Default::default() }).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adds_a_machine_and_controls_its_runner() {
        let engine = demo();
        engine.run_check(None).await;

        let added = add_machine(&engine, AddMachineRequest { name: "Beelink".into(), host: "beelink".into(), check: true }, Surface::Mcp).await.unwrap();
        assert_eq!(added.machine_id, "beelink");
        assert_eq!(added.machine.as_ref().unwrap().summary.status, "online");

        let mut lines = 0;
        let installed = install_runners(
            &engine,
            InstallRunnersRequest {
                machine_id: "beelink".into(),
                scope: None, // the demo watches exactly one org
                names: vec![],
                count: Some(1),
                labels: vec!["gpu".into()],
                runner_group: None,
                as_service: true,
                mode: None,
                image: None,
                docker_socket: false,
                wait: true,
                timeout_secs: Some(60),
            },
            Surface::Mcp,
            |_| lines += 1,
        )
        .await
        .unwrap();
        assert!(installed.finished && installed.operation.status == "succeeded", "{:?}", installed.operation);
        assert!(lines > 5);
        assert_eq!(installed.runners.len(), 1);
        let rid = installed.runners[0].id.clone();
        assert_eq!(rid, "beelink/beelink-01");

        let act = |action, wait| RunnerActionRequest { runner_id: rid.clone(), action, delete_files: true, wait, timeout_secs: Some(30) };
        let logs = runner_action(&engine, act(ActionName::Logs, true), Surface::Mcp, |_| {}).await.unwrap();
        assert!(!logs.operation.log_tail.is_empty());
        let stopped = runner_action(&engine, act(ActionName::Stop, true), Surface::Mcp, |_| {}).await.unwrap();
        assert_eq!(stopped.runners[0].state, "offline");
        let started = runner_action(&engine, act(ActionName::Start, false), Surface::Mcp, |_| {}).await.unwrap();
        assert!(!started.finished && started.next.is_some());
        let op = wait_operation(&engine, started.operation.id, Duration::from_secs(30), |_| {}).await.unwrap();
        assert_eq!(op.status, "succeeded");
        assert_eq!(operation(&engine, OperationRequest { operation_id: op.id, tail: Some(1) }).unwrap().log_tail.len(), 1);

        let removed = runner_action(&engine, act(ActionName::Remove, true), Surface::Mcp, |_| {}).await.unwrap();
        assert!(removed.runners.is_empty());
        remove_machine(&engine, MachineRequest { machine_id: "beelink".into() }).await.unwrap();
        assert!(!engine.current().machines.iter().any(|m| m.id == "beelink"));

        // Installing on the unreachable laptop fails as an operation, with a hint.
        let err = install_runners(
            &engine,
            InstallRunnersRequest {
                machine_id: "harbor".into(),
                scope: Some("acme".into()),
                names: vec!["laravel-01".into()],
                count: None,
                labels: vec![],
                runner_group: None,
                as_service: true,
                mode: None,
                image: None,
                docker_socket: false,
                wait: true,
                timeout_secs: Some(30),
            },
            Surface::Mcp,
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "operation_failed");
        assert!(err.hint.is_some() && err.details.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hints_name_the_callers_next_step() {
        // A fresh demo engine: nothing checked yet.
        let engine = demo();
        let s = engine.current();
        assert!(status_from(&s, Surface::Mcp).hint.unwrap().contains("check_machines"));
        let cli = status_from(&s, Surface::Cli).hint.unwrap();
        assert!(cli.contains("muxin status --refresh") && !cli.contains("check_machines"), "{cli}");
    }

    #[test]
    fn workspace_paths_stay_inside() {
        assert_eq!(workspace_path("src/main.rs").unwrap(), "src/main.rs");
        assert_eq!(workspace_path("a/../b").unwrap(), "a/../b");
        assert_eq!(workspace_path("").unwrap(), "");
        for bad in ["/etc/passwd", "~/x", "../x", "a/../../x", "C:\\x", "\\\\server\\x"] {
            assert_eq!(workspace_path(bad).unwrap_err().code, "invalid", "{bad}");
        }
    }

    #[test]
    fn workspace_errors_map_to_codes() {
        assert_eq!(ws_error("No workspace \"ws-x\".".into(), Surface::Mcp).code, "not_found");
        assert_eq!(ws_error("ssh: connect to host x port 22: Operation timed out".into(), Surface::Mcp).code, "unreachable");
        assert_eq!(ws_error("Workspaces aren't implemented yet.".into(), Surface::Mcp).code, "internal");
        assert!(ws_error("Mac Studio doesn't allow workspaces.".into(), Surface::Cli).hint.unwrap().contains("muxin workspace allow"));
        let e = ws_error("Mac Studio doesn't allow workspaces.".into(), Surface::Mcp);
        assert_eq!(e.code, "invalid");
        assert!(e.hint.unwrap().contains("workspace_allow"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_requests_are_validated_before_the_backend() {
        let engine = demo();
        let exec = |command: &str, timeout: Option<u64>, cwd: Option<&str>| WorkspaceExecRequest {
            workspace_id: "ws-nope".into(),
            command: command.into(),
            cwd: cwd.map(str::to_string),
            env: BTreeMap::new(),
            timeout_secs: timeout,
            background: false,
        };
        assert_eq!(workspace_exec(&engine, exec("  ", None, None), "test", Surface::Mcp).await.unwrap_err().code, "invalid");
        assert_eq!(workspace_exec(&engine, exec("ls", Some(0), None), "test", Surface::Mcp).await.unwrap_err().code, "invalid");
        assert_eq!(workspace_exec(&engine, exec("ls", Some(99_999), None), "test", Surface::Mcp).await.unwrap_err().code, "invalid");
        assert_eq!(workspace_exec(&engine, exec("ls", None, Some("../..")), "test", Surface::Mcp).await.unwrap_err().code, "invalid");
        let mut bad_env = exec("ls", None, None);
        bad_env.env.insert("1BAD".into(), "x".into());
        assert_eq!(workspace_exec(&engine, bad_env, "test", Surface::Mcp).await.unwrap_err().code, "invalid");
        // Valid input, unknown workspace.
        let missing = workspace_exec(&engine, exec("ls", None, Some("src")), "test", Surface::Cli).await.unwrap_err();
        assert!(missing.hint.as_deref().unwrap_or_default().contains("muxin workspace create"), "{missing:?}");
        assert_eq!(missing.code, "not_found");

        let read = WorkspaceReadRequest { workspace_id: "ws-nope".into(), path: "/etc/hosts".into(), max_bytes: None };
        assert_eq!(workspace_read_file(&engine, read, Surface::Mcp).await.unwrap_err().code, "invalid");
        let files = WorkspaceFilesRequest { workspace_id: "ws-nope".into(), path: None, depth: Some(50) };
        assert_eq!(workspace_list_files(&engine, files, Surface::Mcp).await.unwrap_err().code, "invalid");
        let sync = WorkspaceSyncRequest {
            workspace_id: "ws-nope".into(),
            local_path: "/definitely/not/a/folder".into(),
            direction: SyncDirection::Push,
            delete: false,
        };
        assert_eq!(workspace_sync(&engine, sync, Surface::Mcp).await.unwrap_err().code, "invalid");
        let create = WorkspaceCreateRequest { machine_id: Some("nope".into()), ..Default::default() };
        assert_eq!(workspace_create(&engine, create, "test", Surface::Mcp).await.unwrap_err().code, "not_found");
        assert_eq!(workspace_allow(&engine, WorkspaceAllowRequest { machine_id: "nope".into(), allowed: true }, Surface::Mcp).unwrap_err().code, "not_found");
    }

    /// Needs the workspace backend (src/fleet/workspaces.rs); run with `cargo test -- --ignored`
    /// once it's implemented. Exercises the whole agent flow on the demo machines.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs the workspace backend"]
    async fn workspace_flow_in_demo() {
        let engine = demo();
        engine.run_check(None).await;
        let created = workspace_create(
            &engine,
            WorkspaceCreateRequest { platform: Some("linux".into()), repo: Some("acme/api".into()), ..Default::default() },
            "test-agent",
            Surface::Mcp,
        )
        .await
        .unwrap();
        let id = created.workspace.id.clone();
        assert_eq!(created.workspace.status, "ready");
        assert_eq!(created.workspace.owner.as_deref(), Some("test-agent"));
        assert!(created.workspace.platform.starts_with("linux"));

        let ran = workspace_exec(
            &engine,
            WorkspaceExecRequest { workspace_id: id.clone(), command: "pwd".into(), cwd: None, env: BTreeMap::new(), timeout_secs: None, background: false },
            "test-agent",
            Surface::Mcp,
        )
        .await
        .unwrap();
        assert_eq!(ran.exit_code, Some(0));

        workspace_write_file(&engine, WorkspaceWriteRequest { workspace_id: id.clone(), path: "notes/a.txt".into(), content: "hi".into(), append: false }, Surface::Mcp)
            .await
            .unwrap();
        let read = workspace_read_file(&engine, WorkspaceReadRequest { workspace_id: id.clone(), path: "notes/a.txt".into(), max_bytes: None }, Surface::Mcp).await.unwrap();
        assert_eq!(read.content, "hi");
        let files = workspace_list_files(&engine, WorkspaceFilesRequest { workspace_id: id.clone(), path: None, depth: None }, Surface::Mcp).await.unwrap();
        assert!(!files.entries.is_empty());
        workspace_changes(&engine, WorkspaceRequest { workspace_id: id.clone() }, Surface::Mcp).await.unwrap();

        let got = workspace_get(&engine, WorkspaceRequest { workspace_id: id.clone() }, Surface::Mcp).unwrap();
        assert!(got.commands >= 1 && !got.history.is_empty());
        assert!(workspaces_list(&engine).await.workspaces.iter().any(|w| w.id == id));

        workspace_destroy(&engine, WorkspaceDestroyRequest { workspace_id: id.clone(), keep_files: false }, Surface::Mcp).await.unwrap();
        assert_eq!(workspace_get(&engine, WorkspaceRequest { workspace_id: id }, Surface::Mcp).unwrap_err().code, "not_found");
    }
}

// ---------------------------------------------------------------------------
// Agent sessions: terminals running Claude Code, Codex, ... on fleet machines

use crate::fleet::sessions::{self, Control, SessionInfo, StartSession};

/// Session size for sessions started without a terminal attached.
const HEADLESS_COLS: u16 = 160;
const HEADLESS_ROWS: u16 = 48;
const READ_DEFAULT_LINES: u32 = 200;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListRequest {
    /// Only this machine.
    #[serde(default)]
    pub machine_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStartRequest {
    pub machine_id: String,
    /// "claude", "codex", "opencode", "gemini", "amp", "pi" or "shell".
    pub agent: String,
    /// Folder on the machine to start in. Default: the home folder.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// First message for the agent.
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    /// "<machine-id>/<name>", e.g. "beelink/claude-3f9a". A bare name works when unique.
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameRequest {
    pub session_id: String,
    /// The new title. Empty goes back to the one it started with.
    pub title: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSendRequest {
    pub session_id: String,
    /// Text to type into the session.
    pub text: String,
    /// Press Enter after the text. Default true.
    #[serde(default = "default_true")]
    pub enter: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionReadRequest {
    pub session_id: String,
    /// Lines of scrollback above the screen to include. Default 200.
    #[serde(default)]
    pub lines: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionActionRequest {
    pub session_id: String,
    /// commit, commit_push, pull_request, review, fix_comments, conflicts or recap.
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct SessionActionOut {
    pub session_id: String,
    pub action: String,
    /// What was typed into the agent.
    pub prompt: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct SessionHostOut {
    pub machine_id: String,
    pub machine: String,
    /// The Muxin agent's version there, once it's set up.
    pub agent: Option<String>,
    /// Agent CLIs found on the machine.
    pub agents: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionsOut {
    pub sessions: Vec<SessionInfo>,
    pub machines: Vec<SessionHostOut>,
}

#[derive(Debug, Serialize)]
pub struct SessionReadOut {
    pub session_id: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub busy: bool,
    /// The screen and recent scrollback as plain text.
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct SessionDoneOut {
    pub session_id: String,
    pub done: String,
}

fn session_error(message: String) -> ApiError {
    let m = message.to_ascii_lowercase();
    let code = if m.contains("no machine") || m.contains("no session") {
        "not_found"
    } else if m.contains("unreachable") || m.contains("timed out") || m.contains("connection refused") || m.contains("no route to host") {
        "unreachable"
    } else if m.contains("isn't a session id") || m.contains("unknown agent") || m.contains("isn't a folder") || m.contains("isn't a session action") || m.contains("is a shell") {
        "invalid"
    } else {
        "operation_failed"
    };
    let mut err = ApiError::new(code, message);
    if code == "unreachable" {
        err = err.with_hint("The machine didn't answer over ssh. Check that `ssh <host>` works without a password prompt.");
    }
    err
}

/// Machines to look at: one, or every machine that can be reached.
fn session_machines(engine: &Arc<Engine>, only: Option<&str>) -> ApiResult<Vec<Machine>> {
    let s = engine.current();
    match only {
        Some(id) => Ok(vec![find_machine(&s, id)?.clone()]),
        None => Ok(s.machines.iter().filter(|m| !m.host.is_empty() && !m.is_down()).cloned().collect()),
    }
}

/// Reads git on these machines (their sessions' and workspaces' folders)
/// unless that was just now.
async fn look_at_stale_git<'a>(engine: &Arc<Engine>, machine_ids: impl Iterator<Item = &'a str>) {
    let stale = machine_ids.filter(|id| engine.git_looked_at(id).is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(20)));
    futures::future::join_all(stale.map(|id| engine.look_at_git(id))).await;
}

/// Lists agent sessions, re-reading the machines first.
pub async fn sessions_list(engine: &Arc<Engine>, req: SessionListRequest) -> ApiResult<SessionsOut> {
    let machines = session_machines(engine, req.machine_id.as_deref())?;
    let found = futures::future::join_all(machines.iter().map(|m| engine.refresh_sessions(&m.id))).await;
    // What each session's work changed (git), looked at now unless it was just now.
    look_at_stale_git(engine, machines.iter().zip(&found).filter(|(_, f)| f.as_ref().is_ok_and(|f| !f.sessions.is_empty())).map(|(m, _)| m.id.as_str())).await;
    let snapshot = engine.current();
    let mut out = SessionsOut { sessions: Vec::new(), machines: Vec::new() };
    for (m, f) in machines.iter().zip(found) {
        match f {
            Ok(f) => {
                // As published: with what each one changed.
                out.sessions.extend(f.sessions.into_iter().map(|s| snapshot.sessions.iter().find(|x| x.id == s.id).cloned().unwrap_or(s)));
                out.machines.push(SessionHostOut { machine_id: m.id.clone(), machine: m.name.clone(), agent: f.agent_version, agents: f.agents, error: f.error });
            }
            Err(e) => out.machines.push(SessionHostOut { machine_id: m.id.clone(), machine: m.name.clone(), agent: None, agents: Vec::new(), error: Some(e) }),
        }
    }
    Ok(out)
}

/// Resolves "<machine>/<name>", or a bare name that's unique across the machines.
async fn resolve_session(engine: &Arc<Engine>, id: &str) -> ApiResult<SessionInfo> {
    let id = id.trim();
    if let Some((machine, name)) = id.split_once('/') {
        let m = find_machine(&engine.current(), machine)?.clone();
        let found = engine.refresh_sessions(&m.id).await.map_err(session_error)?;
        return found.sessions.into_iter().find(|s| s.name == name).ok_or_else(|| {
            ApiError::not_found(format!("No session \"{name}\" on {}.", m.name)).with_hint("List sessions with session_list (`muxin session ls`).")
        });
    }
    let all = sessions_list(engine, SessionListRequest::default()).await?;
    let matches: Vec<SessionInfo> = all.sessions.into_iter().filter(|s| s.name == id).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => Err(ApiError::not_found(format!("No session \"{id}\".")).with_hint("Session ids look like <machine-id>/<name>; list them with session_list (`muxin session ls`).")),
        _ => Err(ApiError::invalid(format!("\"{id}\" is on more than one machine; use <machine-id>/{id}."))),
    }
}

pub async fn session_start(engine: &Arc<Engine>, req: SessionStartRequest) -> ApiResult<SessionInfo> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    if sessions::agent(&req.agent).is_none() {
        let ids: Vec<&str> = sessions::AGENTS.iter().map(|a| a.id).collect();
        return Err(ApiError::invalid(format!("Unknown agent \"{}\".", req.agent)).with_hint(format!("Use one of: {}.", ids.join(", "))));
    }
    engine
        .start_session(StartSession {
            machine_id: m.id,
            agent: req.agent.to_ascii_lowercase(),
            cwd: req.cwd.unwrap_or_default(),
            title: req.title,
            prompt: req.prompt,
            cols: HEADLESS_COLS,
            rows: HEADLESS_ROWS,
            // Only the app hosts terminals for sessions without the agent.
            direct: false,
            command: None,
            typed: None,
        })
        .await
        .map_err(session_error)
}

/// Asks a session's agent to commit, open a pull request, review, ...: types
/// that action's prompt (the user's, else Muxin's) into it.
pub async fn session_action(engine: &Arc<Engine>, req: SessionActionRequest) -> ApiResult<SessionActionOut> {
    let s = resolve_session(engine, &req.session_id).await?;
    let action = req.action.trim().to_string();
    let prompt = engine.session_action(&s.id, &action).await.map_err(session_error)?;
    Ok(SessionActionOut { session_id: s.id, action, prompt })
}

pub async fn session_send(engine: &Arc<Engine>, req: SessionSendRequest) -> ApiResult<SessionDoneOut> {
    if req.text.is_empty() && !req.enter {
        return Err(ApiError::invalid("Nothing to send: text is empty and enter is false."));
    }
    let s = resolve_session(engine, &req.session_id).await?;
    if s.status != "running" {
        return Err(ApiError::invalid(format!("{} has exited.", s.id)).with_hint("Restart it with session_restart (`muxin session restart`)."));
    }
    engine.control_session(&s.id, Control::Send { text: &req.text, enter: req.enter }).await.map_err(session_error)?;
    Ok(SessionDoneOut { session_id: s.id, done: "sent".into() })
}

pub async fn session_read(engine: &Arc<Engine>, req: SessionReadRequest) -> ApiResult<SessionReadOut> {
    let s = resolve_session(engine, &req.session_id).await?;
    let lines = req.lines.unwrap_or(READ_DEFAULT_LINES);
    let text = engine.control_session(&s.id, Control::Read { lines }).await.map_err(session_error)?;
    Ok(SessionReadOut { session_id: s.id, status: s.status, exit_code: s.exit_code, busy: s.busy, text })
}

/// What a session is waiting for: a choice (with the keys that pick each
/// option) or, when it's simply done, the end of what it said.
pub async fn session_question(engine: &Arc<Engine>, req: SessionRequest) -> ApiResult<crate::fleet::sessions::question::Question> {
    let s = resolve_session(engine, &req.session_id).await?;
    if s.status != "running" {
        return Err(ApiError::invalid(format!("{} has exited.", s.id)));
    }
    engine.session_question(&s.id).await.map_err(session_error)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAnswerRequest {
    pub session_id: String,
    /// One of session_question's options, by its key ("1", "2", or "#1" for a menu without numbers).
    pub choice: String,
}

/// Picks one of a waiting session's choices, as session_question listed them.
pub async fn session_answer(engine: &Arc<Engine>, req: SessionAnswerRequest) -> ApiResult<SessionDoneOut> {
    let s = resolve_session(engine, &req.session_id).await?;
    let q = engine.session_question(&s.id).await.map_err(session_error)?;
    if !q.options.iter().any(|o| o.key == req.choice) {
        let keys: Vec<String> = q.options.iter().map(|o| format!("{} ({})", o.key, o.label)).collect();
        return Err(ApiError::invalid(if keys.is_empty() { format!("{} isn't asking to choose anything right now.", s.id) } else { format!("No choice \"{}\": it offers {}.", req.choice, keys.join(", ")) }));
    }
    engine.answer_session(&s.id, &req.choice, q.selected.as_deref(), q.confirm).await.map_err(session_error)?;
    Ok(SessionDoneOut { session_id: s.id, done: "answered".into() })
}

pub async fn session_stop(engine: &Arc<Engine>, req: SessionRequest) -> ApiResult<SessionDoneOut> {
    let s = resolve_session(engine, &req.session_id).await?;
    engine.control_session(&s.id, Control::Stop).await.map_err(session_error)?;
    Ok(SessionDoneOut { session_id: s.id, done: "stopped".into() })
}

pub async fn session_restart(engine: &Arc<Engine>, req: SessionRequest) -> ApiResult<SessionDoneOut> {
    let s = resolve_session(engine, &req.session_id).await?;
    engine.control_session(&s.id, Control::Restart).await.map_err(session_error)?;
    Ok(SessionDoneOut { session_id: s.id, done: "restarted".into() })
}

pub async fn session_rename(engine: &Arc<Engine>, req: SessionRenameRequest) -> ApiResult<SessionInfo> {
    let s = resolve_session(engine, &req.session_id).await?;
    engine.rename_session(&s.id, &req.title).await.map_err(session_error)
}

/// The program and arguments that attach this terminal to a session.
pub async fn session_attach(engine: &Arc<Engine>, req: SessionRequest) -> ApiResult<sessions::AttachCommand> {
    let s = resolve_session(engine, &req.session_id).await?;
    engine.attach_command(&s.id).map_err(session_error)
}

// ---------------------------------------------------------------------------
// GitHub: your repositories, issues and pull requests; code on your machines

use crate::fleet::model::Checkout;
use crate::github::hub;

fn github_error(message: String) -> ApiError {
    let lower = message.to_ascii_lowercase();
    let mut err = ApiError::new("github", message);
    if lower.contains("no github token") || lower.contains("rejected the token") || lower.contains("bad credentials") {
        err = err.with_hint("Sign in to GitHub: `gh auth login` (Muxin uses the GitHub CLI's login), or set GITHUB_TOKEN.");
    }
    err
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoRequest {
    /// "owner/name" or a GitHub URL.
    pub repo: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRequest {
    /// An issue or pull request: its URL, or "owner/name#123".
    pub reference: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutsRequest {
    /// Only this machine.
    #[serde(default)]
    pub machine_id: Option<String>,
    /// Only checkouts of this repository ("owner/name").
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitRequest {
    pub machine_id: String,
    /// The checkout's folder on that machine.
    pub path: String,
    /// "fetch", "pull" (fast-forward only) or "push".
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct RepoOut {
    pub repo: hub::Repo,
    pub issues: Vec<hub::Item>,
    pub pulls: Vec<hub::Item>,
    pub runs: Vec<hub::Run>,
    /// Where it's checked out on your machines.
    pub checkouts: Vec<Checkout>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FavoriteRequest {
    /// "owner/name" or a GitHub URL.
    pub repo: String,
    /// false removes it from the favorites.
    #[serde(default = "yes")]
    pub favorite: bool,
}

#[derive(Debug, Serialize)]
pub struct AccountsOut {
    /// The account Muxin uses now: for the GitHub page, managing runners,
    /// and cloning into workspaces.
    pub using: Option<String>,
    /// "gh", or the environment variable the token came from (then switching
    /// between the GitHub CLI's accounts isn't possible); "" when none was found.
    pub token_source: String,
    /// When Muxin uses another machine's GitHub CLI login: that machine's id.
    pub machine_id: Option<String>,
    /// The accounts the GitHub CLI here is signed in to.
    pub accounts: Vec<crate::github::Account>,
    /// Other machines' GitHub CLI logins, usable instead.
    pub machines: Vec<MachineAccount>,
    /// The permissions (OAuth scopes) of the login in use; null when GitHub doesn't say.
    /// Watching an organization's runners needs "admin:org".
    pub scopes: Option<Vec<String>>,
    /// The organizations the login in use belongs to (candidates for github_scopes).
    pub orgs: Vec<String>,
    /// What to run to give the login the permission to read organization runners, when it lacks it.
    pub grant_command: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MachineAccount {
    pub machine_id: String,
    pub machine: String,
    pub login: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UseAccountRequest {
    /// A login the GitHub CLI here is signed in to; with machine_id, optional.
    #[serde(default)]
    pub login: String,
    /// Use this machine's GitHub CLI login instead of this computer's.
    #[serde(default)]
    pub machine_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FavoritesOut {
    pub favorites: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GitOut {
    pub output: String,
}

pub async fn github_inbox(engine: &Arc<Engine>) -> ApiResult<hub::Inbox> {
    if engine.is_demo() {
        return Ok(crate::demo::github::inbox());
    }
    let settings = engine.settings().github;
    engine.github().inbox(&settings).await.map_err(github_error)
}

/// Your repositories, favorites first, then most recently pushed.
pub async fn github_repos(engine: &Arc<Engine>) -> ApiResult<Vec<hub::Repo>> {
    let settings = engine.settings();
    let mut repos = if engine.is_demo() { crate::demo::github::repos() } else { engine.github().my_repos(&settings.github).await.map_err(github_error)? };
    for r in &mut repos {
        r.favorite = settings.favorite_repos.iter().any(|f| f.eq_ignore_ascii_case(&r.full_name));
    }
    repos.sort_by_key(|r| !r.favorite);
    Ok(repos)
}

/// The GitHub CLI's accounts, and which one Muxin uses (read fresh, so a
/// permission granted a moment ago shows).
pub async fn github_accounts(engine: &Arc<Engine>) -> ApiResult<AccountsOut> {
    if engine.is_demo() {
        let login = crate::demo::github::LOGIN.to_string();
        return Ok(AccountsOut {
            using: Some(login.clone()),
            token_source: "gh".into(),
            machine_id: None,
            accounts: vec![crate::github::Account { login, gh_active: true, ok: true, scopes: "repo, read:org, admin:org".into() }],
            machines: Vec::new(),
            scopes: Some(vec!["repo".into(), "read:org".into(), "admin:org".into()]),
            orgs: vec!["acme".into()],
            grant_command: None,
        });
    }
    let settings = engine.settings().github;
    let gh = engine.github();
    gh.reload_login();
    // Machines whose GitHub CLI login isn't known yet (a fresh CLI run) are asked.
    let unknown: Vec<String> = engine
        .current()
        .machines
        .iter()
        .filter(|m| m.host != "local" && m.gh.is_none() && m.status == "online")
        .map(|m| m.id.clone())
        .collect();
    let (accounts, viewer, orgs, _) = futures::join!(
        gh.accounts(&settings),
        gh.viewer(&settings),
        gh.my_orgs(&settings),
        futures::future::join_all(unknown.iter().map(|id| engine.refresh_sessions(id)))
    );
    let scopes = gh.scopes();
    let grant_command = scopes
        .as_ref()
        .filter(|s| !s.iter().any(|x| x == "admin:org"))
        .map(|_| format!("gh auth refresh -h {} -s admin:org", crate::fleet::workspaces::github_web_host(&settings.api_url)));
    let token_source = gh.token_source().await;
    let snapshot = engine.current();
    let machines: Vec<MachineAccount> = snapshot
        .machines
        .iter()
        .filter(|m| m.host != "local")
        .filter_map(|m| {
            let g = m.gh.as_ref().filter(|g| g.logged_in)?;
            Some(MachineAccount { machine_id: m.id.clone(), machine: m.name.clone(), login: g.account.clone()? })
        })
        .collect();
    let accounts = match accounts {
        Ok(a) => a,
        // A token from the environment, or a machine's login, works without the CLI here.
        Err(_) if (!token_source.is_empty() && token_source != "gh") || !machines.is_empty() => Vec::new(),
        Err(e) => return Err(ApiError::new("github", e).with_hint("gh auth login")),
    };
    let machine_id = Some(settings.account_machine).filter(|m| !m.is_empty());
    Ok(AccountsOut { using: viewer.ok().map(|v| v.login), token_source, machine_id, accounts, machines, scopes, orgs: orgs.unwrap_or_default(), grant_command })
}

/// Switches Muxin to another of the GitHub CLI's accounts (not the CLI's own active account).
pub async fn github_use_account(engine: &Arc<Engine>, req: UseAccountRequest) -> ApiResult<AccountsOut> {
    let login = req.login.trim().trim_start_matches('@').to_string();
    let mut settings = engine.settings();
    let source = engine.github().token_source().await;
    if !source.is_empty() && source != "gh" {
        return Err(ApiError::invalid(format!("Muxin is using the token in ${source}. Unset it to switch between the GitHub CLI's accounts.")));
    }
    let previous = settings.clone();
    if let Some(id) = req.machine_id.as_deref().filter(|m| !m.is_empty()) {
        let m = find_machine(&engine.current(), id)?.clone();
        if m.gh.is_none() && m.host != "local" {
            let _ = engine.refresh_sessions(&m.id).await;
        }
    }
    let snapshot = engine.current();
    let machine = match req.machine_id.as_deref().filter(|m| !m.is_empty()) {
        Some(id) => Some(find_machine(&snapshot, id)?).filter(|m| m.host != "local"),
        None => None,
    };
    match machine {
        Some(m) => {
            let Some(account) = m.gh.as_ref().filter(|g| g.logged_in).and_then(|g| g.account.clone()) else {
                return Err(ApiError::not_found(format!("The GitHub CLI on {} isn't signed in.", m.name)).with_hint("Sign it in from the machine's page, or run `gh auth login` there."));
            };
            if !login.is_empty() && !account.eq_ignore_ascii_case(&login) {
                return Err(ApiError::invalid(format!("{} is signed in to GitHub as {account}, not {login}.", m.name)));
            }
            settings.github.account = account;
            settings.github.account_machine = m.id.clone();
        }
        None => {
            let accounts = engine.github().accounts(&settings.github).await.map_err(|e| ApiError::new("github", e).with_hint("gh auth login"))?;
            let Some(account) = accounts.iter().find(|a| a.login.eq_ignore_ascii_case(&login)) else {
                let known: Vec<&str> = accounts.iter().map(|a| a.login.as_str()).collect();
                return Err(ApiError::not_found(format!("The GitHub CLI isn't signed in to \"{login}\" (it has: {}).", known.join(", "))).with_hint("gh auth login"));
            };
            settings.github.account = account.login.clone();
            settings.github.account_machine = String::new();
        }
    }
    engine.update_settings(settings).map_err(ApiError::invalid)?;
    // Make sure GitHub takes it; otherwise stay on the previous login.
    let wanted = engine.settings().github.account;
    let out = github_accounts(engine).await?;
    match out.using.as_deref() {
        Some(u) if u.eq_ignore_ascii_case(&wanted) => Ok(out),
        Some(u) => {
            let _ = engine.update_settings(previous);
            Err(ApiError::invalid(format!("That login turned out to be {u}, not {wanted}, so Muxin kept the previous one. Check which account gh is signed in to there.")))
        }
        None => {
            let _ = engine.update_settings(previous);
            Err(ApiError::new("github", "GitHub didn't accept that login, so Muxin kept the previous one.").with_hint("gh auth login"))
        }
    }
}

/// Adds a repository to the favorites, or removes it.
pub fn github_favorite(engine: &Arc<Engine>, req: FavoriteRequest) -> ApiResult<FavoritesOut> {
    let name = hub::repo_name(&req.repo).ok_or_else(|| ApiError::invalid(format!("\"{}\" isn't a GitHub repository; use owner/name.", req.repo)))?;
    let mut settings = engine.settings();
    settings.favorite_repos.retain(|f| !f.eq_ignore_ascii_case(&name));
    if req.favorite {
        settings.favorite_repos.push(name);
    }
    let saved = engine.update_settings(settings).map_err(ApiError::invalid)?;
    Ok(FavoritesOut { favorites: saved.favorite_repos })
}

pub async fn github_repo(engine: &Arc<Engine>, req: RepoRequest) -> ApiResult<RepoOut> {
    let name = hub::repo_name(&req.repo).ok_or_else(|| ApiError::invalid(format!("\"{}\" isn't a GitHub repository; use owner/name.", req.repo)))?;
    if engine.is_demo() {
        let repo = crate::demo::github::repo_named(&name).ok_or_else(|| ApiError::new("not_found", format!("No repository called {name} in the demo.")))?;
        let items = crate::demo::github::repo_items(&name);
        return Ok(RepoOut { repo, issues: items.issues, pulls: items.pulls, runs: crate::demo::github::runs(&name), checkouts: Vec::new() });
    }
    let settings = engine.settings().github;
    let gh = engine.github();
    let (repo, items, runs) = futures::join!(gh.repo(&settings, &name), gh.repo_items(&settings, &name), gh.repo_runs(&settings, &name));
    let mut repo = repo.map_err(github_error)?;
    repo.favorite = engine.settings().favorite_repos.iter().any(|f| f.eq_ignore_ascii_case(&repo.full_name));
    let items = items.map_err(github_error)?;
    let checkouts = engine.current().checkouts.iter().filter(|c| c.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&name))).cloned().collect();
    Ok(RepoOut { repo, issues: items.issues, pulls: items.pulls, runs: runs.unwrap_or_default(), checkouts })
}

pub async fn github_item(engine: &Arc<Engine>, req: ItemRequest) -> ApiResult<hub::Detail> {
    let (repo, number) = hub::issue_ref(&req.reference)
        .ok_or_else(|| ApiError::invalid(format!("\"{}\" isn't an issue or pull request; use its URL or owner/name#123.", req.reference)))?;
    if engine.is_demo() {
        return crate::demo::github::detail(&repo, number).ok_or_else(|| ApiError::new("not_found", format!("No {repo}#{number} in the demo.")));
    }
    let settings = engine.settings().github;
    engine.github().detail(&settings, &repo, number).await.map_err(github_error)
}

#[derive(Debug, Serialize)]
pub struct GitSetupOut {
    pub machine_id: String,
    #[serde(flatten)]
    pub setup: crate::fleet::code::GitSetup,
    /// git has a name and email to commit with.
    pub can_commit: bool,
    /// How a push over HTTPS gets a GitHub login: "gh" (the machine's GitHub CLI),
    /// "other" (another credential helper or SSH keys; may work), or "none".
    pub push: String,
    /// The machine's GitHub CLI account, when it's signed in.
    pub gh_account: Option<String>,
    /// What `identity: true` sets: the GitHub account's name and its private noreply email.
    pub suggested_name: Option<String>,
    pub suggested_email: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitStatusRequest {
    pub machine_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSetupRequest {
    pub machine_id: String,
    /// Set git's name and email (globally on the machine): the ones given, else the GitHub account's.
    #[serde(default)]
    pub identity: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    /// Make git use the machine's GitHub CLI login for github.com (`gh auth setup-git`).
    #[serde(default)]
    pub credentials: bool,
}

/// The machine, with its GitHub CLI state known (asked for when it isn't yet).
async fn machine_with_gh(engine: &Arc<Engine>, machine_id: &str) -> ApiResult<crate::fleet::model::Machine> {
    let m = find_machine(&engine.current(), machine_id)?.clone();
    if m.gh.is_some() {
        return Ok(m);
    }
    let _ = engine.refresh_sessions(&m.id).await;
    Ok(find_machine(&engine.current(), &m.id)?.clone())
}

async fn git_out(engine: &Arc<Engine>, m: &crate::fleet::model::Machine, setup: crate::fleet::code::GitSetup) -> GitSetupOut {
    let gh_account = m.gh.as_ref().filter(|g| g.logged_in).and_then(|g| g.account.clone());
    let settings = engine.settings().github;
    let login = match gh_account.clone() {
        Some(l) => Some(l),
        None => engine.github().viewer(&settings).await.ok().map(|v| v.login),
    };
    let suggested = match login {
        Some(l) => engine.github().commit_identity(&settings, &l).await.ok(),
        None => None,
    };
    let push = if setup.uses_gh && gh_account.is_some() {
        "gh"
    } else if !setup.helpers.is_empty() || !setup.ssh_keys.is_empty() {
        "other"
    } else {
        "none"
    };
    GitSetupOut {
        machine_id: m.id.clone(),
        can_commit: setup.name.is_some() && setup.email.is_some(),
        push: push.into(),
        gh_account,
        suggested_name: suggested.as_ref().map(|s| s.0.clone()),
        suggested_email: suggested.map(|s| s.1),
        setup,
    }
}

/// Whether git on a machine can commit and push to GitHub.
pub async fn git_status(engine: &Arc<Engine>, machine_id: &str) -> ApiResult<GitSetupOut> {
    let m = machine_with_gh(engine, machine_id).await?;
    let setup = engine.git_setup_status(&m.id).await.map_err(|e| ApiError::new("operation_failed", e))?;
    Ok(git_out(engine, &m, setup).await)
}

/// Sets up git on a machine: identity and/or GitHub credentials through the GitHub CLI.
pub async fn git_setup(engine: &Arc<Engine>, req: GitSetupRequest) -> ApiResult<GitSetupOut> {
    if !req.identity && !req.credentials {
        return Err(ApiError::invalid("Nothing to set up: pass identity and/or credentials."));
    }
    let m = machine_with_gh(engine, &req.machine_id).await?;
    let identity = if req.identity {
        let (name, email) = match (req.name.clone(), req.email.clone()) {
            (Some(n), Some(e)) => (n, e),
            (n, e) => {
                let current = git_out(engine, &m, Default::default()).await;
                match (n.or(current.suggested_name), e.or(current.suggested_email)) {
                    (Some(n), Some(e)) => (n, e),
                    _ => return Err(ApiError::invalid("Couldn't find a GitHub name and email to use; pass name and email.")),
                }
            }
        };
        Some((name, email))
    } else {
        None
    };
    let setup = engine.git_setup_apply(&m.id, identity, req.credentials).await.map_err(|e| {
        let err = ApiError::new("operation_failed", e.clone());
        if e.contains("Sign in to the GitHub CLI") { err.with_hint("gh auth login") } else { err }
    })?;
    Ok(git_out(engine, &m, setup).await)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectRequest {
    pub machine_id: String,
    /// true pairs and connects directly; false goes back to ssh only.
    pub enabled: bool,
}

/// How Muxin reaches a machine: directly (QUIC), a live ssh link, or none.
pub async fn machine_connection(engine: &Arc<Engine>, machine_id: &str) -> ApiResult<crate::fleet::connection::Connection> {
    let m = find_machine(&engine.current(), machine_id)?.clone();
    engine.connection(&m.id).await.map_err(|e| ApiError::new("unreachable", e))
}

/// Turns direct (QUIC) connections to a machine on or off.
pub async fn set_direct(engine: &Arc<Engine>, req: DirectRequest) -> ApiResult<crate::fleet::connection::Connection> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.set_direct(&m.id, req.enabled).await.map_err(|e| ApiError::new("operation_failed", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopInputRequest {
    pub machine_id: String,
    /// One of the machine's displays (desktop_displays); default: the whole desktop or the main display.
    #[serde(default)]
    pub display: Option<String>,
    pub events: Vec<crate::fleet::desktop::DesktopEvent>,
}

/// What's on a machine's screen, or one of its displays (a JPEG, plus its size).
pub async fn desktop_screenshot(engine: &Arc<Engine>, machine_id: &str, display: Option<&str>) -> ApiResult<crate::fleet::desktop::Screenshot> {
    let m = find_machine(&engine.current(), machine_id)?.clone();
    engine.desktop_screenshot(&m.id, display).await.map_err(|e| ApiError::new("operation_failed", e))
}

/// Moves, clicks, scrolls and types on a machine's desktop.
pub async fn desktop_input(engine: &Arc<Engine>, req: DesktopInputRequest) -> ApiResult<serde_json::Value> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let n = engine.desktop_input(&m.id, req.display.as_deref(), &req.events).await.map_err(|e| ApiError::new("operation_failed", e))?;
    Ok(serde_json::json!({ "machine_id": m.id, "events": n, "next": "Take a desktop_screenshot to see the result." }))
}

/// A machine's monitors, virtual ones included.
pub async fn desktop_displays(engine: &Arc<Engine>, req: MachineRequest) -> ApiResult<Vec<crate::fleet::model::DisplayInfo>> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.displays(&m.id).await.map_err(|e| ApiError::new("operation_failed", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddDisplayRequest {
    pub machine_id: String,
    /// Default 1920×1080.
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Agents don't type the user's password: the app asks for it.
fn display_error(e: crate::fleet::desktop::AddDisplayError) -> ApiError {
    match e {
        crate::fleet::desktop::AddDisplayError::NeedsPassword(why) => ApiError::invalid(why)
            .with_hint("Ask the user to add or remove this monitor from the Muxin app (the machine's desktop › Add monitor); it asks for their password there."),
        crate::fleet::desktop::AddDisplayError::Failed(why) => ApiError::new("operation_failed", why),
    }
}

/// Adds a virtual monitor to a machine's desktop.
pub async fn desktop_add_display(engine: &Arc<Engine>, req: AddDisplayRequest) -> ApiResult<crate::fleet::model::DisplayInfo> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    // The main monitor's size unless told otherwise.
    let known = if m.displays.is_empty() && (req.width.is_none() || req.height.is_none()) { engine.displays(&m.id).await.unwrap_or_default() } else { m.displays.clone() };
    let screens: Vec<_> = known.iter().filter(|d| d.id != "all").collect();
    let main = screens.iter().find(|d| d.main).or(screens.first()).map(|d| (d.width.round() as u32, d.height.round() as u32)).unwrap_or((1920, 1080));
    let (w, h) = (req.width.unwrap_or(main.0), req.height.unwrap_or(main.1));
    engine.add_display(&m.id, w, h, req.name.as_deref()).await.map_err(display_error)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDisplayRequest {
    pub machine_id: String,
    pub display: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub scale: Option<f64>,
}

/// Sets a monitor's size (one it offers: see `modes`) or the desktop's scale on it.
pub async fn desktop_set_display(engine: &Arc<Engine>, req: SetDisplayRequest) -> ApiResult<crate::fleet::model::DisplayInfo> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let size = req.width.zip(req.height);
    if size.is_none() && req.scale.is_none() {
        return Err(ApiError::new("invalid", "Pass width and height, or scale."));
    }
    engine.set_display(&m.id, &req.display, size, req.scale).await.map_err(|e| ApiError::new("operation_failed", e))?;
    let shown = engine.current().machines.iter().find(|x| x.id == m.id).and_then(|x| x.displays.iter().find(|d| d.id == req.display).cloned());
    shown.ok_or_else(|| ApiError::new("not_found", format!("{} isn't one of {}'s displays now.", req.display, m.name)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveDisplayRequest {
    pub machine_id: String,
    pub display: String,
}

/// Removes a virtual monitor Muxin added.
pub async fn desktop_remove_display(engine: &Arc<Engine>, req: RemoveDisplayRequest) -> ApiResult<serde_json::Value> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.remove_display(&m.id, &req.display).await.map_err(display_error)?;
    Ok(serde_json::json!({ "machine_id": m.id, "removed": req.display }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSourcesRequest {
    pub machine_id: String,
}

/// What a machine has to read: its system log, log files, containers, services.
pub async fn log_sources(engine: &Arc<Engine>, req: LogSourcesRequest) -> ApiResult<crate::fleet::logs::LogSources> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.log_sources(&m.id).await.map_err(|e| ApiError::new("operation_failed", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadLogsRequest {
    pub machine_id: String,
    /// system, kernel, file, docker or service.
    pub kind: String,
    /// The file's path, the container's name or id, the unit (empty for the system log).
    #[serde(default)]
    pub target: String,
    /// A service of the user's own (systemctl --user).
    #[serde(default)]
    pub user: bool,
    pub lines: Option<u64>,
    pub filter: Option<String>,
}

/// A log's last lines.
pub async fn logs_read(engine: &Arc<Engine>, req: ReadLogsRequest) -> ApiResult<serde_json::Value> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let source = crate::fleet::logs::LogSource { kind: req.kind.clone(), target: req.target.clone(), user: req.user, ..Default::default() };
    let lines = engine.read_logs(&m.id, &source, req.lines.unwrap_or(200), req.filter.as_deref()).await.map_err(|e| ApiError::new("operation_failed", e))?;
    Ok(serde_json::json!({ "machine_id": m.id, "kind": req.kind, "target": req.target, "lines": lines }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServicesRequest {
    pub machine_id: String,
}

/// A machine's services (systemd's on Linux, Homebrew's on a Mac) and Docker containers.
pub async fn machine_services(engine: &Arc<Engine>, req: ServicesRequest) -> ApiResult<crate::fleet::services::ServiceList> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.services(&m.id, true).await.map_err(|e| ApiError::new("operation_failed", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceActionRequest {
    pub machine_id: String,
    /// A service's or container's name (as `machine_services` lists it).
    pub name: String,
    /// start, stop, restart; enable or disable (whether a systemd service starts at boot).
    pub action: String,
    /// Look among containers only.
    #[serde(default)]
    pub container: bool,
}

/// Starts, stops or restarts a service or container.
pub async fn service_action(engine: &Arc<Engine>, req: ServiceActionRequest) -> ApiResult<serde_json::Value> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let action: &'static str = match req.action.as_str() {
        "start" => "start",
        "stop" => "stop",
        "restart" => "restart",
        "enable" => "enable",
        "disable" => "disable",
        other => return Err(ApiError::new("invalid", format!("\"{other}\" isn't an action: start, stop, restart, enable or disable."))),
    };
    let list = engine.services(&m.id, false).await.map_err(|e| ApiError::new("operation_failed", e))?;
    let wanted = req.name.trim_end_matches(".service");
    let pool: Vec<_> = if req.container { list.containers } else { list.services.into_iter().chain(list.containers).collect() };
    let service = pool
        .into_iter()
        .find(|s| s.name == wanted || s.target == req.name)
        .ok_or_else(|| ApiError::new("not_found", format!("{} has no service or container called {}.", m.name, req.name)))?;
    engine.service_action(&m.id, &service, action).await.map_err(|e| match e {
        crate::fleet::desktop::AddDisplayError::NeedsPassword(_) => {
            ApiError::new("invalid", format!("{} wants the user's password for that (sudo).", m.name)).with_hint("Do it from the Muxin app (machine › Services), which asks for it.")
        }
        crate::fleet::desktop::AddDisplayError::Failed(why) => ApiError::new("operation_failed", why),
    })?;
    Ok(serde_json::json!({ "machine_id": m.id, "name": service.name, "kind": service.kind, "action": action, "ok": true }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerRunRequest {
    pub machine_id: String,
    pub image: String,
    pub name: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    /// no, always, unless-stopped (default), on-failure.
    pub restart: Option<String>,
    pub command: Option<String>,
    /// Wait for it to finish (default true).
    pub wait: Option<bool>,
}

/// An operation's result: waited for (up to 30 minutes) unless `wait` is off.
async fn operation_result(engine: &Arc<Engine>, op: u64, wait: bool) -> ApiResult<OperationOut> {
    let done = if wait { wait_operation(engine, op, Duration::from_secs(30 * 60), |_| {}).await } else { None };
    let o = done.or_else(|| engine.current().operations.iter().find(|o| o.id == op).cloned()).ok_or_else(|| ApiError::new("internal", "The operation went missing."))?;
    Ok(operation_out(&o, DEFAULT_TAIL))
}

/// `docker run -d` on a machine (pulling the image first if needed).
pub async fn docker_run(engine: &Arc<Engine>, req: DockerRunRequest) -> ApiResult<OperationOut> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let run = crate::fleet::services::DockerRun {
        image: req.image,
        name: req.name.unwrap_or_default(),
        ports: req.ports,
        env: req.env,
        volumes: req.volumes,
        restart: req.restart.unwrap_or_else(|| "unless-stopped".into()),
        command: req.command.unwrap_or_default(),
    };
    let op = engine.docker_run(&m.id, run).map_err(|e| ApiError::new("invalid", e))?;
    operation_result(engine, op, req.wait.unwrap_or(true)).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerPullRequest {
    pub machine_id: String,
    pub image: String,
    pub wait: Option<bool>,
}

pub async fn docker_pull(engine: &Arc<Engine>, req: DockerPullRequest) -> ApiResult<OperationOut> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let op = engine.docker_pull(&m.id, &req.image).map_err(|e| ApiError::new("invalid", e))?;
    operation_result(engine, op, req.wait.unwrap_or(true)).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeActionRequest {
    pub machine_id: String,
    /// The project's name, as machine_services lists it.
    pub project: String,
    /// up, down, restart or pull; with `service`: start, stop or restart.
    pub action: String,
    /// One of its services, to start, stop or restart just that one.
    pub service: Option<String>,
    pub wait: Option<bool>,
}

pub async fn compose_action(engine: &Arc<Engine>, req: ComposeActionRequest) -> ApiResult<OperationOut> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let list = engine.services(&m.id, false).await.map_err(|e| ApiError::new("operation_failed", e))?;
    let project = list.compose.into_iter().find(|p| p.name == req.project).ok_or_else(|| ApiError::new("not_found", format!("{} has no Compose project called {}.", m.name, req.project)))?;
    let op = match req.service.as_deref().filter(|s| !s.is_empty()) {
        Some(service) => engine.compose_service_action(&m.id, &project, service, &req.action),
        None => engine.compose_action(&m.id, &project, &req.action),
    }
    .map_err(|e| ApiError::new("invalid", e))?;
    operation_result(engine, op, req.wait.unwrap_or(true)).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRequest {
    /// The last N days (default 7, at most 400).
    pub days: Option<u32>,
    /// One machine; default: every machine that's online.
    pub machine_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageModel {
    pub tool: String,
    pub model: String,
    #[serde(flatten)]
    pub tokens: crate::fleet::usage::Tokens,
    /// At the API's list price; null when the model's price isn't known.
    pub cost: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct UsageMachine {
    pub machine_id: String,
    #[serde(flatten)]
    pub tokens: crate::fleet::usage::Tokens,
    pub cost: f64,
}

#[derive(Debug, Serialize)]
pub struct UsageOut {
    pub days: u32,
    /// What it'd all cost at list prices (models without a known price left out).
    pub total_cost: f64,
    pub prices_as_of: &'static str,
    pub models: Vec<UsageModel>,
    pub machines: Vec<UsageMachine>,
    /// Machines that couldn't say, and why.
    pub errors: Vec<serde_json::Value>,
}

/// What the agent CLIs (Claude Code, Codex) used, from their own logs on
/// each machine: tokens by model and machine, and the cost at list prices.
pub async fn usage(engine: &Arc<Engine>, req: UsageRequest) -> ApiResult<UsageOut> {
    let days = req.days.unwrap_or(7).clamp(1, 400);
    let results = match &req.machine_id {
        Some(id) => {
            let m = find_machine(&engine.current(), id)?.clone();
            vec![(m.id.clone(), engine.usage(&m.id, days).await)]
        }
        None => engine.usage_all(days).await,
    };
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for (id, r) in results {
        match r {
            Ok(u) => ok.push(u),
            Err(e) => errors.push(serde_json::json!({ "machine_id": id, "error": e })),
        }
    }
    let models: Vec<UsageModel> = crate::fleet::usage::by_model(&ok).into_iter().map(|(tool, model, tokens, cost)| UsageModel { tool, model, tokens, cost }).collect();
    let machines = ok
        .iter()
        .map(|u| {
            let mut tokens = crate::fleet::usage::Tokens::default();
            let mut cost = 0.;
            for h in &u.hours {
                tokens.add(&h.tokens);
                cost += crate::fleet::usage::cost(&h.model, &h.tokens).unwrap_or(0.);
            }
            UsageMachine { machine_id: u.machine_id.clone(), tokens, cost }
        })
        .collect();
    let total_cost = models.iter().filter_map(|m| m.cost).sum();
    Ok(UsageOut { days, total_cost, prices_as_of: crate::fleet::usage::PRICES_AS_OF, models, machines, errors })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessesRequest {
    pub machine_id: String,
}

/// A machine's processes (the busiest few hundred, with CPU and memory) and its GPUs.
pub async fn machine_processes(engine: &Arc<Engine>, req: ProcessesRequest) -> ApiResult<crate::fleet::procs::ProcessList> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.processes(&m.id).await.map_err(|e| ApiError::new("operation_failed", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSignalRequest {
    pub machine_id: String,
    pub pid: u32,
    /// "stop" (SIGTERM, the default) or "kill" (SIGKILL).
    #[serde(default)]
    pub signal: Option<String>,
}

/// Stops (SIGTERM) or kills (SIGKILL) a process on a machine.
pub async fn process_signal(engine: &Arc<Engine>, req: ProcessSignalRequest) -> ApiResult<serde_json::Value> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let action = match req.signal.as_deref().unwrap_or("stop") {
        "kill" | "KILL" => "kill",
        "stop" | "term" | "TERM" => "stop",
        other => return Err(ApiError::invalid(format!("\"{other}\" isn't a signal: stop or kill."))),
    };
    let process = crate::fleet::services::Service { kind: "process".into(), target: req.pid.to_string(), name: req.pid.to_string(), ..Default::default() };
    engine.service_action(&m.id, &process, action).await.map_err(|e| match e {
        crate::fleet::desktop::AddDisplayError::NeedsPassword(_) => ApiError::new("invalid", format!("{} wants the user's password for that: it's another user's process (sudo).", m.name)).with_hint("Do it from the Muxin app (machine › Services › Processes), which asks for it."),
        crate::fleet::desktop::AddDisplayError::Failed(e) => ApiError::new("operation_failed", e),
    })?;
    Ok(serde_json::json!({ "machine_id": m.id, "pid": req.pid, "done": if action == "kill" { "killed" } else { "stopped" } }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    pub machine_ids: Vec<String>,
    pub command: String,
    /// Seconds each may run (default 600, at most 3600).
    pub timeout_secs: Option<u64>,
    /// Wait for all of them (default true).
    pub wait: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct RunOut {
    pub machine_id: String,
    /// Absent when it couldn't start there (`error` says why).
    pub operation: Option<OperationOut>,
    /// The command's exit status, when it ran to the end.
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// Runs one shell command on several machines at once, in each one's login
/// shell from the home folder.
pub async fn run_on_machines(engine: &Arc<Engine>, req: RunRequest) -> ApiResult<Vec<RunOut>> {
    if req.machine_ids.is_empty() {
        return Err(ApiError::invalid("On which machines? Pass machine_ids."));
    }
    let s = engine.current();
    let ids: Vec<String> = req.machine_ids.iter().map(|id| find_machine(&s, id).map(|m| m.id.clone())).collect::<ApiResult<_>>()?;
    let timeout = req.timeout_secs.map(Duration::from_secs).unwrap_or(crate::fleet::command::DEFAULT_TIMEOUT);
    let started = engine.run_on_machines(&ids, &req.command, timeout);
    let wait = req.wait.unwrap_or(true);
    let mut out = Vec::new();
    for (machine_id, op) in started {
        match op {
            Ok(op) => {
                let o = if wait { wait_operation(engine, op, timeout + Duration::from_secs(30), |_| {}).await } else { None };
                let o = o.or_else(|| engine.current().operations.iter().find(|o| o.id == op).cloned()).ok_or_else(|| ApiError::new("internal", "The operation went missing."))?;
                let exit_code = match (o.status.as_str(), o.error.as_deref()) {
                    ("succeeded", _) => Some(0),
                    (_, Some(e)) => e.strip_prefix("Exited with status ").and_then(|c| c.trim_end_matches('.').parse().ok()),
                    _ => None,
                };
                let error = o.error.clone();
                out.push(RunOut { machine_id, operation: Some(operation_out(&o, DEFAULT_TAIL)), exit_code, error });
            }
            Err(e) => out.push(RunOut { machine_id, operation: None, exit_code: None, error: Some(e) }),
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WakeRequest {
    pub machine_id: String,
}

/// Wakes a machine that's asleep or off (Wake on LAN).
pub async fn wake_machine(engine: &Arc<Engine>, req: WakeRequest) -> ApiResult<serde_json::Value> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let from = engine.wake(&m.id).await.map_err(|e| ApiError::new("operation_failed", e))?;
    Ok(serde_json::json!({ "machine_id": m.id, "sent_from": from, "next": "Muxin checks it again in 20 and 40 seconds; call get_machine then." }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortsRequest {
    pub machine_id: String,
}

/// What's listening on a machine: each TCP port with its program or Docker
/// container, and for web servers the status and title of their front page.
pub async fn machine_ports(engine: &Arc<Engine>, req: PortsRequest) -> ApiResult<Vec<crate::fleet::ports::ListeningPort>> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.ports(&m.id).await.map_err(|e| ApiError::new("operation_failed", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRequest {
    pub machine_id: String,
    pub port: u16,
    /// The port on this computer (default: the same, or the next free one).
    pub local_port: Option<u16>,
}

/// Opens a machine's port on this computer (localhost), for as long as this
/// process runs.
pub async fn forward_port(engine: &Arc<Engine>, req: ForwardRequest) -> ApiResult<crate::fleet::ports::Forward> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.forward(&m.id, req.port, None, req.local_port).await.map_err(|e| ApiError::new("invalid", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeProjectRequest {
    pub machine_id: String,
    /// The project's name, as machine_services lists it.
    pub project: String,
}

/// A Compose project's parts: its services (image, ports, what each waits
/// for, networks, volumes, health check) with their containers' state, and
/// its networks and volumes.
pub async fn compose_project(engine: &Arc<Engine>, req: ComposeProjectRequest) -> ApiResult<crate::fleet::services::ComposeGraph> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    let list = engine.services(&m.id, false).await.map_err(|e| ApiError::new("operation_failed", e))?;
    let project = list.compose.into_iter().find(|p| p.name == req.project).ok_or_else(|| ApiError::new("not_found", format!("{} has no Compose project called {}.", m.name, req.project)))?;
    engine.compose_describe(&m.id, &project).await.map_err(|e| ApiError::new("operation_failed", e))
}

/// Git checkouts on the machines, re-read first.
pub async fn checkouts(engine: &Arc<Engine>, req: CheckoutsRequest) -> ApiResult<Vec<Checkout>> {
    let machines = session_machines(engine, req.machine_id.as_deref())?;
    let _ = futures::future::join_all(machines.iter().map(|m| engine.refresh_checkouts(&m.id))).await;
    let want = req.repo.as_deref().and_then(hub::repo_name);
    Ok(engine
        .current()
        .checkouts
        .iter()
        .filter(|c| machines.iter().any(|m| m.id == c.machine_id))
        .filter(|c| want.as_deref().is_none_or(|w| c.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(w))))
        .cloned()
        .collect())
}

pub async fn checkout_git(engine: &Arc<Engine>, req: GitRequest) -> ApiResult<GitOut> {
    let m = find_machine(&engine.current(), &req.machine_id)?.clone();
    engine.checkout_git(&m.id, &req.path, &req.action).await.map(|output| GitOut { output }).map_err(|e| ApiError::new("operation_failed", e))
}

// ---------------------------------------------------------------------------
// Files: browsing folders on machines, and copying between them.

#[derive(Debug, Deserialize)]
pub struct ListFilesRequest {
    pub machine_id: String,
    /// A folder: "~", "~/build", "/srv/data". Default: the home folder.
    #[serde(default)]
    pub path: String,
}

pub async fn list_files(engine: &Arc<Engine>, req: ListFilesRequest) -> ApiResult<crate::fleet::files::Listing> {
    let s = engine.current();
    let m = find_machine(&s, &req.machine_id)?;
    engine.list_files(&m.id, &req.path).await.map_err(ApiError::invalid)
}

#[derive(Debug, Deserialize)]
pub struct CopyFilesRequest {
    /// The machine with the files.
    pub from: String,
    /// Files and folders on it ("~/build/app.tar.gz", "/srv/data/reports").
    pub paths: Vec<String>,
    /// The machine to copy them to.
    pub to: String,
    /// The folder there (made if missing). Files with the same names are replaced.
    pub to_dir: String,
    /// Wait for the copy to finish (default true).
    #[serde(default = "yes")]
    pub wait: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct CopyFilesResponse {
    /// One per source folder.
    pub operations: Vec<OperationOut>,
    pub finished: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Splits "~/build/app" into ("~/build", "app"); a bare name is in the home folder.
pub fn split_path(path: &str) -> Result<(String, String), String> {
    let p = path.trim().trim_end_matches('/');
    if p.is_empty() || p == "~" {
        return Err("Name a file or folder to copy, not a whole home folder.".into());
    }
    match p.rsplit_once('/') {
        Some(("", name)) => Ok(("/".into(), name.into())),
        Some((dir, name)) => Ok((dir.into(), name.into())),
        None => Ok(("~".into(), p.into())),
    }
}

pub async fn copy_files(engine: &Arc<Engine>, req: CopyFilesRequest, surface: Surface, mut on_line: impl FnMut(&str)) -> ApiResult<CopyFilesResponse> {
    let s = engine.current();
    let from = find_machine(&s, &req.from)?.id.clone();
    let to = find_machine(&s, &req.to)?.id.clone();
    if req.paths.is_empty() {
        return Err(ApiError::invalid("Give at least one path to copy."));
    }
    // One copy per folder the files are in.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for p in &req.paths {
        let (dir, name) = split_path(p).map_err(ApiError::invalid)?;
        match groups.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, names)) => names.push(name),
            None => groups.push((dir, vec![name])),
        }
    }
    let mut ids = Vec::new();
    for (dir, names) in groups {
        let copy = crate::fleet::files::CopyRequest { from: from.clone(), dir, names, to: to.clone(), to_dir: req.to_dir.clone() };
        ids.push(engine.copy_files(copy).map_err(ApiError::invalid)?);
    }
    let timeout = Duration::from_secs(req.timeout_secs.unwrap_or(3600).max(1));
    let mut operations = Vec::new();
    for id in &ids {
        let op = if req.wait {
            wait_operation(engine, *id, timeout, &mut on_line).await
        } else {
            engine.current().operations.iter().find(|o| o.id == *id).cloned()
        }
        .ok_or_else(|| ApiError::new("internal", format!("Operation {id} disappeared.")))?;
        operations.push(op);
    }
    let finished = operations.iter().all(|o| o.status != "running");
    let response = CopyFilesResponse {
        operations: operations.iter().map(|o| operation_out(o, DEFAULT_TAIL)).collect(),
        finished,
        next: (!finished).then(|| {
            surface.say(
                "Still copying. Poll get_operation with each operation id until status isn't \"running\"; cancel_operation stops one.".to_string(),
                "Still copying in the background; this command stopped waiting.",
            )
        }),
    };
    if let Some(failed) = operations.iter().find(|o| o.status == "failed") {
        return Err(ApiError {
            code: "operation_failed",
            message: failed.error.clone().unwrap_or_else(|| "The copy failed.".into()),
            hint: None,
            details: serde_json::to_value(&response).ok(),
        });
    }
    Ok(response)
}

/// Stops a running operation that can be stopped (a copy).
pub fn cancel_operation(engine: &Arc<Engine>, req: OperationRequest) -> ApiResult<OperationOut> {
    if !engine.cancel_op(req.operation_id) {
        return Err(ApiError::invalid(format!("Operation {} isn't running, or can't be stopped.", req.operation_id)));
    }
    operation(engine, req)
}
