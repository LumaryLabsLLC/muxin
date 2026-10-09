//! Agent workspaces: a folder on a fleet machine (optionally a git checkout)
//! that an AI agent checks out as its work environment — runs commands, reads
//! and writes files, syncs a local tree — like Amp's Orbs, on your own hardware.
//!
//! Workspaces persist until destroyed; nothing is cleaned up automatically.
//! They live in `<config dir>/workspaces.json` so the window and every agent
//! process (MCP server, CLI) see the same set.
//!
//! Path checks keep agents from wandering out of a workspace by mistake; they
//! aren't a sandbox — `workspace_exec` runs whatever it's given as the
//! machine's ssh user.
//!
//! PUBLIC API — the UI, the MCP server and the CLI depend on these signatures.

mod demo;
mod registry;
pub(crate) mod transport;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::Engine;
use super::model::{Machine, WorkspaceCommand, WorkspaceInfo};
use crate::config::MachineSpec;
use crate::ops::remote::{self, Param, ps_quote, sh_quote, with_params};
use crate::probe::Shell;
pub(crate) use registry::WorkspaceRecord;
use transport::{Limits, Output};

pub use demo::DemoTree;

const LIB_SH: &str = include_str!("scripts/lib.sh");
const EXEC_SH: &str = include_str!("scripts/exec.sh");
const EXEC_PS1: &str = include_str!("scripts/exec.ps1");
const FILES_SH: &str = include_str!("scripts/files.sh");
const FILES_PS1: &str = include_str!("scripts/files.ps1");
const CHANGES_SH: &str = include_str!("scripts/changes.sh");
const CREATE_SH: &str = include_str!("scripts/create.sh");
const DESTROY_SH: &str = include_str!("scripts/destroy.sh");
const OTHER_PS1: &str = include_str!("scripts/other.ps1");
const DOCKER_SH: &str = include_str!("scripts/docker.sh");
const EXEC_DOCKER_SH: &str = include_str!("scripts/exec_docker.sh");

/// Image for docker workspaces when none is given: Debian with git, curl and
/// a compiler toolchain (what the official language images build on).
pub const DEFAULT_IMAGE: &str = "buildpack-deps:bookworm";

const DEFAULT_ROOT: &str = "~/muxen/workspaces";
const DEFAULT_ROOT_WINDOWS: &str = "~\\muxen\\workspaces";

pub const EXEC_TIMEOUT_DEFAULT: u64 = 600;
pub const EXEC_TIMEOUT_MAX: u64 = 3600;
/// Extra time Muxen waits beyond the command's own timeout before hanging up.
const EXEC_GRACE: u64 = 30;
const FILE_TIMEOUT: Duration = Duration::from_secs(60);
const CREATE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DESTROY_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const SYNC_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const READ_DEFAULT: usize = 256 * 1024;
pub const READ_MAX: usize = 1024 * 1024;
pub const WRITE_MAX: usize = 5 * 1024 * 1024;
pub const LIST_DEPTH_DEFAULT: u32 = 2;
pub const LIST_DEPTH_MAX: u32 = 10;
pub const LIST_MAX_ENTRIES: usize = 2000;
const CHANGES_LIMITS: Limits = Limits { head: 60 * 1024, tail: 4 * 1024, line: 4 * 1024 };
const COMMAND_MAX: usize = 64 * 1024;
const TAIL_LINE_MAX: usize = 400;
/// Machines with less free disk than this aren't picked automatically.
const MIN_FREE_DISK: u64 = 5_000_000_000;

/// Where and what to create.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CreateWorkspace {
    /// Machine id. None picks the least-loaded online machine that allows workspaces
    /// (and matches `platform` when given).
    pub machine: Option<String>,
    /// Platform prefix to match when picking a machine: "linux", "linux-x64", "darwin-arm64", "windows".
    pub platform: Option<String>,
    /// Label; defaults to the repo name, else the id.
    pub name: Option<String>,
    /// Git URL, or "owner/repo" for GitHub.
    pub repo: Option<String>,
    /// Branch, tag or commit to check out.
    pub git_ref: Option<String>,
    /// Create and switch to a new branch after checkout.
    pub branch: Option<String>,
    /// Who is creating it ("claude-code", "cli", "app", ...).
    pub owner: Option<String>,
}

/// How a workspace's commands run. Separate from `CreateWorkspace` so existing
/// callers keep compiling; `create_workspace` uses the default (host).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkspaceIsolation {
    /// "host" (default): commands run directly on the machine as the ssh user.
    /// "docker": a fresh container per workspace, with the folder mounted at /workspace.
    pub isolation: Option<String>,
    /// Docker image, e.g. "node:22", "rust:1", "python:3.13". Defaults to `DEFAULT_IMAGE`.
    pub image: Option<String>,
    /// Share the host's docker socket with the container (docker-in-docker). Off by default.
    pub docker_socket: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecOptions {
    /// Seconds before the command is killed (default 600, max 3600). Ignored for background commands.
    pub timeout_secs: Option<u64>,
    pub env: Vec<(String, String)>,
    /// Folder relative to the workspace to run in.
    pub cwd: Option<String>,
    /// Start it detached (dev servers, watchers); output goes to a log file in the workspace.
    pub background: bool,
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    /// None if it timed out or runs in the background.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// Output was longer than the cap; the middle was dropped.
    pub truncated: bool,
    pub background_pid: Option<u32>,
    /// Background commands: log file path relative to the workspace.
    pub log_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub size: u64,
    pub truncated: bool,
    /// Not valid UTF-8; `content` is empty.
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// Relative to the workspace.
    pub path: String,
    /// "file" | "dir" | "link"
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncDirection {
    /// Local folder → workspace.
    Push,
    /// Workspace → local folder.
    Pull,
}

impl Engine {
    /// Creates a workspace and waits until it's ready (folder made, repo cloned).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn create_workspace(self: &Arc<Self>, req: CreateWorkspace) -> Result<WorkspaceInfo, String> {
        self.create_workspace_with(req, WorkspaceIsolation::default()).await
    }

    /// Like `create_workspace`, choosing how commands run: on the host, or in a
    /// fresh docker container (folder mounted at /workspace).
    pub async fn create_workspace_with(self: &Arc<Self>, req: CreateWorkspace, iso: WorkspaceIsolation) -> Result<WorkspaceInfo, String> {
        self.refresh_from_disk();
        let docker = match clean_opt(iso.isolation).map(|s| s.to_ascii_lowercase()).as_deref() {
            None | Some("host") | Some("none") => false,
            Some("docker") | Some("container") => true,
            Some(other) => return Err(format!("\"{other}\" isn't an isolation Muxen knows. Use \"host\" or \"docker\".")),
        };
        let image_req = clean_opt(iso.image);
        if !docker && image_req.is_some() {
            return Err("An image only applies to docker workspaces; pass isolation \"docker\" too.".into());
        }
        let image = if docker { Some(validate_image(image_req.as_deref().unwrap_or(DEFAULT_IMAGE))?) } else { None };
        let name = clean_opt(req.name);
        if let Some(n) = &name {
            if n.chars().count() > 80 || n.chars().any(char::is_control) {
                return Err("The workspace name is too long or has odd characters.".into());
            }
        }
        let git_ref = clean_opt(req.git_ref);
        let branch = clean_opt(req.branch);
        for r in git_ref.iter().chain(branch.iter()) {
            validate_git_ref(r)?;
        }
        let settings = self.settings();
        let gh_host = github_web_host(&settings.github.api_url);
        let repo = clean_opt(req.repo).map(|r| normalize_repo(&r, &gh_host)).transpose()?;
        if repo.is_none() && (git_ref.is_some() || branch.is_some()) {
            return Err("gitRef and branch need a repo to clone.".into());
        }

        // Pick the machine and make sure we know what it runs.
        let snap = self.current();
        let machine_id = match clean_opt(req.machine) {
            Some(m) => {
                let found = snap
                    .machines
                    .iter()
                    .find(|x| x.id == m || x.name.eq_ignore_ascii_case(&m))
                    .ok_or_else(|| format!("Machine \"{m}\" not found in the fleet. Known machines: {}.", machine_names(&snap.machines)))?;
                if found.host.is_empty() {
                    return Err(format!("{} has no host in config.toml, so Muxen can't reach it.", found.name));
                }
                if !found.workspaces_allowed {
                    return Err(format!(
                        "Workspaces are turned off for {}. Turn them on from its page in Muxen, or remove `workspaces = false` from its entry in config.toml.",
                        found.name
                    ));
                }
                found.id.clone()
            }
            None => pick_machine_for(&snap.machines, clean_opt(req.platform).as_deref(), docker)?.id.clone(),
        };
        let (spec, mut probe) = self.machine(&machine_id).ok_or("Machine not found in config.toml anymore.")?;
        if self.demo_world().is_none() && !probe.as_ref().is_some_and(|p| p.ok) {
            self.run_check(Some(vec![spec.id.clone()])).await;
            probe = self.machine(&spec.id).and_then(|(_, p)| p);
            match &probe {
                Some(p) if p.ok => {}
                Some(p) => return Err(format!("{} is unreachable: {}", spec.name, p.error.clone().unwrap_or_default())),
                None => return Err(format!("{} is unreachable: the check didn't run.", spec.name)),
            }
        }
        let windows = remote::shell_for(&spec, probe.as_ref()) == Shell::Powershell;
        if docker && windows {
            return Err(format!("Docker workspaces aren't supported on Windows machines yet ({}). Use isolation \"host\" there.", spec.name));
        }
        let machine = self.current().machines.iter().find(|m| m.id == spec.id).cloned();

        let id = self.unused_workspace_id();
        let root = spec.workspace_root.clone().filter(|r| !r.trim().is_empty()).unwrap_or_else(|| {
            if windows { DEFAULT_ROOT_WINDOWS.into() } else { DEFAULT_ROOT.into() }
        });
        let sep = if windows { "\\" } else { "/" };
        let now = Utc::now();
        let info = WorkspaceInfo {
            id: id.clone(),
            name: name.unwrap_or_else(|| repo.as_ref().map(|r| r.short_name.clone()).unwrap_or_else(|| id.clone())),
            machine_id: spec.id.clone(),
            machine_name: spec.name.clone(),
            host: spec.host.trim().to_string(),
            platform: machine.as_ref().map(|m| m.platform.clone()).unwrap_or_default(),
            path: format!("{}{sep}{id}", root.trim_end_matches(['/', '\\'])),
            repo: repo.as_ref().map(|r| r.shown.clone()),
            git_ref: git_ref.clone(),
            branch: branch.clone(),
            owner: clean_opt(req.owner),
            status: "creating".into(),
            error: None,
            created_at: now,
            last_used_at: now,
            commands: 0,
            history: Vec::new(),
            shell: String::new(),
            isolation: if docker { "docker" } else { "host" }.into(),
            image: image.clone(),
            container: docker.then(|| container_name(&id)),
        };
        let record = WorkspaceRecord { info, background_pids: Vec::new(), windows, next_log: 1, docker_socket: docker && iso.docker_socket };
        self.update_workspaces(|list| list.push(record.clone()))?;

        let title = match &image {
            Some(img) => format!("Create workspace {id} on {} ({img})", spec.name),
            None => format!("Create workspace {id} on {}", spec.name),
        };
        let op = self.begin_op("workspace", title, Some(spec.id.clone()), None);
        let result = match self.demo_world() {
            Some(_) => demo::create(self, op, &record, repo.as_ref().map(|r| r.shown.as_str())).await.map_err(|e| (e, None)),
            None => self.run_create(op, &spec, &record, &root, repo.as_ref()).await,
        };
        let finished = match result {
            Ok(path) => {
                self.update_workspaces(|list| {
                    if let Some(r) = list.iter_mut().find(|r| r.info.id == id) {
                        r.info.status = "ready".into();
                        r.info.path = path.clone();
                        r.info.last_used_at = Utc::now();
                    }
                })?;
                self.op_log(op, format!("✓ Workspace {id} is ready at {path}"));
                self.op_finish(op, Ok(()));
                Ok(())
            }
            Err((e, folder)) => {
                // Keep the folder's path (if one was made) so destroying the workspace cleans it up.
                let _ = self.update_workspaces(|list| {
                    if let Some(r) = list.iter_mut().find(|r| r.info.id == id) {
                        r.info.status = "failed".into();
                        r.info.error = Some(e.clone());
                        if let Some(p) = folder.clone() {
                            r.info.path = p;
                        }
                    }
                });
                self.op_log(op, format!("error: {e}"));
                self.op_finish(op, Err(e.clone()));
                Err(e)
            }
        };
        finished?;
        self.workspace_info(&id).ok_or_else(|| "The workspace was created but then disappeared.".to_string())
    }

    /// Runs a shell command in the workspace with the machine user's login environment.
    pub async fn workspace_exec(self: &Arc<Self>, id: &str, command: &str, opts: ExecOptions) -> Result<ExecResult, String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("Give a command to run.".into());
        }
        if command.len() > COMMAND_MAX {
            return Err("That command is too long; write a script file with workspace_write_file and run it.".into());
        }
        let rec = self.ready_workspace(id)?;
        let cwd = match opts.cwd.as_deref() {
            Some(c) => rel_path(c)?,
            None => String::new(),
        };
        for (k, v) in &opts.env {
            validate_env(k, v)?;
        }
        let timeout = opts.timeout_secs.unwrap_or(EXEC_TIMEOUT_DEFAULT).clamp(1, EXEC_TIMEOUT_MAX);
        let started_at = Utc::now();
        let started = Instant::now();

        let result = match self.demo_world() {
            Some(_) => demo::exec(self, &rec, command, &cwd, opts.background).await?,
            None => {
                let spec = self.workspace_machine(&rec)?;
                let log = if opts.background {
                    let n = self.update_workspaces(|list| {
                        list.iter_mut().find(|r| r.info.id == rec.info.id).map(|r| {
                            let n = r.next_log.max(1);
                            r.next_log = n + 1;
                            n
                        })
                    })?;
                    Some(format!(".muxen/logs/{}.log", n.unwrap_or(1)))
                } else {
                    None
                };
                self.run_exec(&spec, &rec, command, &cwd, &opts.env, timeout, log).await?
            }
        };

        let duration_ms = started.elapsed().as_millis() as u64;
        let result = ExecResult { duration_ms, ..result };
        let entry = WorkspaceCommand {
            command: command.to_string(),
            exit_code: result.exit_code,
            started_at,
            duration_ms,
            output_tail: output_tail(&result.stdout, &result.stderr),
            background: opts.background,
            timed_out: result.timed_out,
            by: clean_opt(opts.by),
        };
        let pid = result.background_pid;
        self.update_workspaces(|list| {
            if let Some(r) = list.iter_mut().find(|r| r.info.id == rec.info.id) {
                r.info.history.push(entry);
                registry::cap_history(&mut r.info);
                r.info.commands += 1;
                r.info.last_used_at = Utc::now();
                // Container pids mean nothing on the host; removing the container stops those.
                if let (Some(p), None) = (pid, &r.info.container) {
                    r.background_pids.push(p);
                }
            }
        })?;
        Ok(result)
    }

    pub async fn workspace_read_file(&self, id: &str, path: &str, max_bytes: usize) -> Result<FileContent, String> {
        let rec = self.ready_workspace(id)?;
        let rel = rel_path(path)?;
        if rel.is_empty() {
            return Err("Give the path of a file in the workspace.".into());
        }
        let max = if max_bytes == 0 { READ_DEFAULT } else { max_bytes.min(READ_MAX) };
        if let Some(w) = self.demo_world() {
            return demo::read(w, &rec, &rel, max);
        }
        let spec = self.workspace_machine(&rec)?;
        let max_s = max.to_string();
        let out = self
            .run_files(&spec, &rec, &[("OR_ACTION", Param::Str("read")), ("OR_REL", Param::Str(&rel)), ("OR_MAX", Param::Str(&max_s))])
            .await?;
        let size: u64 = field(&out.stdout, "size").and_then(|s| s.parse().ok()).unwrap_or(0);
        let data = field(&out.stdout, "data").unwrap_or_default();
        let bytes = B64.decode(data.trim()).map_err(|e| format!("Couldn't decode the file from the machine: {e}"))?;
        Ok(file_content(rel, bytes, size, max))
    }

    /// Writes (or appends) a text file, creating parent folders. Returns the new size.
    pub async fn workspace_write_file(&self, id: &str, path: &str, content: &str, append: bool) -> Result<u64, String> {
        let rec = self.ready_workspace(id)?;
        let rel = rel_path(path)?;
        if rel.is_empty() {
            return Err("Give the path of a file in the workspace.".into());
        }
        if content.len() > WRITE_MAX {
            return Err(format!("That's more than {} — copy big files with workspace_sync instead.", crate::util::bytes(WRITE_MAX as u64)));
        }
        let size = match self.demo_world() {
            Some(w) => demo::write(w, &rec, &rel, content, append),
            None => {
                let spec = self.workspace_machine(&rec)?;
                let encoded = B64.encode(content.as_bytes());
                let out = self
                    .run_files(
                        &spec,
                        &rec,
                        &[
                            ("OR_ACTION", Param::Str("write")),
                            ("OR_REL", Param::Str(&rel)),
                            ("OR_B64", Param::Str(&encoded)),
                            ("OR_APPEND", Param::Bool(append)),
                        ],
                    )
                    .await?;
                field(&out.stdout, "size").and_then(|s| s.parse().ok()).unwrap_or(content.len() as u64)
            }
        };
        self.touch_workspace(&rec.info.id);
        Ok(size)
    }

    pub async fn workspace_list_files(&self, id: &str, path: &str, depth: u32) -> Result<Vec<FileEntry>, String> {
        let rec = self.ready_workspace(id)?;
        let rel = rel_path(path)?;
        let depth = if depth == 0 { LIST_DEPTH_DEFAULT } else { depth.min(LIST_DEPTH_MAX) };
        if let Some(w) = self.demo_world() {
            return demo::list(w, &rec, &rel, depth);
        }
        let spec = self.workspace_machine(&rec)?;
        let depth_s = depth.to_string();
        let out = self
            .run_files(&spec, &rec, &[("OR_ACTION", Param::Str("list")), ("OR_REL", Param::Str(&rel)), ("OR_DEPTH", Param::Str(&depth_s))])
            .await?;
        Ok(parse_listing(&out.stdout, &rel))
    }

    /// `git status` plus the diff of uncommitted changes (capped), or a note when it isn't a git checkout.
    pub async fn workspace_changes(&self, id: &str) -> Result<String, String> {
        let rec = self.ready_workspace(id)?;
        if let Some(w) = self.demo_world() {
            return Ok(demo::changes(w, &rec));
        }
        let spec = self.workspace_machine(&rec)?;
        let (script, windows) = if rec.windows {
            (with_params(Shell::Powershell, &[("OR_ACTION", Param::Str("changes")), ("OR_DIR", Param::Str(&rec.info.path))], OTHER_PS1), true)
        } else {
            (with_params(Shell::Sh, &[("OR_DIR", Param::Str(&rec.info.path))], CHANGES_SH), false)
        };
        let out = run_checked(&spec, windows, script, FILE_TIMEOUT, CHANGES_LIMITS, &[], None).await?;
        let mut text = out.stdout;
        if out.truncated {
            text.push_str("\n[the diff was cut; run `git diff` with workspace_exec for the rest]");
        }
        Ok(text)
    }

    /// rsync between a folder on this computer and the workspace. Returns rsync's summary.
    pub async fn workspace_sync(&self, id: &str, local_path: &str, direction: SyncDirection, delete: bool) -> Result<String, String> {
        let rec = self.ready_workspace(id)?;
        let local = expand_home(local_path.trim());
        if local.as_os_str().is_empty() {
            return Err("Give a local folder to sync.".into());
        }
        if let Some(_w) = self.demo_world() {
            let summary = demo::sync(&rec, &local, direction, delete).await;
            self.touch_workspace(&rec.info.id);
            return Ok(summary);
        }
        if rec.windows {
            return Err("Sync uses rsync, which Muxen doesn't support for Windows machines. Use workspace_write_file or git instead.".into());
        }
        let spec = self.workspace_machine(&rec)?;
        let have_rsync = tokio::process::Command::new("rsync")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success());
        if !have_rsync {
            return Err("rsync isn't installed on this computer, so Muxen can't sync folders.".into());
        }
        match direction {
            SyncDirection::Push if !local.is_dir() => return Err(format!("{} isn't a folder on this computer.", local.display())),
            SyncDirection::Pull => std::fs::create_dir_all(&local).map_err(|e| format!("Couldn't create {}: {e}", local.display()))?,
            _ => {}
        }
        let args = rsync_args(&spec, &rec.info.path, &local.to_string_lossy(), direction, delete);
        let run = tokio::process::Command::new("rsync").args(&args).kill_on_drop(true).output();
        let output = tokio::time::timeout(SYNC_TIMEOUT, run)
            .await
            .map_err(|_| "The sync took longer than 30 minutes and was stopped.".to_string())?
            .map_err(|e| format!("Couldn't run rsync: {e}"))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            let tail: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).rev().take(4).collect();
            return Err(format!("rsync failed: {}", tail.into_iter().rev().collect::<Vec<_>>().join(" ")));
        }
        self.touch_workspace(&rec.info.id);
        let out = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
        Ok(lines[lines.len().saturating_sub(16)..].join("\n"))
    }

    /// Stops background commands and removes the workspace (and its folder unless `keep_files`).
    pub async fn destroy_workspace(self: &Arc<Self>, id: &str, keep_files: bool) -> Result<(), String> {
        self.refresh_from_disk();
        let rec = self.find_workspace(id)?;
        let wid = rec.info.id.clone();
        let title = if keep_files { format!("Forget workspace {wid}") } else { format!("Delete workspace {wid}") };
        let op = self.begin_op("workspace", title, Some(rec.info.machine_id.clone()), None);
        let previous = rec.info.status.clone();
        self.update_workspaces(|list| {
            if let Some(r) = list.iter_mut().find(|r| r.info.id == wid) {
                r.info.status = "destroying".into();
            }
        })?;

        // A path still starting with "~" was never confirmed by the machine: no folder was made.
        let has_folder = !rec.info.path.starts_with('~');
        let cleaned: Result<(), String> = match (self.demo_world(), self.machine(&rec.info.machine_id)) {
            (Some(_), _) => demo::destroy(self, op, &rec, keep_files).await,
            (None, Some((spec, _))) if spec.is_checkable() && has_folder => self.run_destroy(op, &spec, &rec, keep_files).await,
            (None, Some(_)) => {
                self.op_log(op, format!("No folder was created on {}; forgetting the workspace.", rec.info.machine_name));
                Ok(())
            }
            (None, None) => {
                self.op_log(op, format!("warning: {} isn't in config.toml anymore, so its files can't be cleaned up; forgetting the workspace.", rec.info.machine_name));
                Ok(())
            }
        };
        match cleaned {
            Ok(()) => {
                self.update_workspaces(|list| list.retain(|r| r.info.id != wid))?;
                self.op_log(op, format!("✓ Workspace {wid} is gone"));
                self.op_finish(op, Ok(()));
                Ok(())
            }
            Err(e) => {
                let _ = self.update_workspaces(|list| {
                    if let Some(r) = list.iter_mut().find(|r| r.info.id == wid) {
                        r.info.status = previous.clone();
                    }
                });
                self.op_log(op, format!("error: {e}"));
                self.op_finish(op, Err(e.clone()));
                Err(e)
            }
        }
    }

    /// Current workspaces, most recently used first.
    pub fn workspaces(&self) -> Vec<WorkspaceInfo> {
        self.current().workspaces.clone()
    }

    /// Turns agent workspaces on or off for a machine (edits config.toml).
    pub fn set_workspaces_allowed(self: &Arc<Self>, machine_id: &str, allowed: bool) -> Result<(), String> {
        if self.machine(machine_id).is_none() {
            return Err(format!("Machine \"{machine_id}\" not found in config.toml."));
        }
        let edit = |text: &str| crate::ops::inventory_edit::set_workspaces(text, machine_id, allowed);
        match self.demo_world() {
            Some(w) => w.edit_inventory(edit)?,
            None => crate::ops::inventory_edit::edit_file(self.inventory_path(), edit)?,
        }
        self.reload_inventory_now();
        Ok(())
    }

    // -----------------------------------------------------------------------

    fn find_workspace(&self, id: &str) -> Result<WorkspaceRecord, String> {
        let id = id.trim();
        let records = self.workspace_records();
        if let Some(r) = records.iter().find(|r| r.info.id == id) {
            return Ok(r.clone());
        }
        let by_name: Vec<&WorkspaceRecord> = records.iter().filter(|r| r.info.name.eq_ignore_ascii_case(id)).collect();
        match by_name.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(format!("Workspace \"{id}\" not found. Use list_workspaces to see the workspaces.")),
            _ => Err(format!("Several workspaces are called \"{id}\"; use the id (ws-…).")),
        }
    }

    /// The workspace, if it's ready for commands.
    fn ready_workspace(&self, id: &str) -> Result<WorkspaceRecord, String> {
        self.refresh_from_disk();
        let rec = self.find_workspace(id)?;
        match rec.info.status.as_str() {
            "ready" => Ok(rec),
            "creating" => Err(format!("Workspace {} is being created; try again once it's ready.", rec.info.id)),
            "destroying" => Err(format!("Workspace {} is being destroyed.", rec.info.id)),
            _ => Err(format!(
                "Workspace {} failed to set up ({}). Destroy it and create a new one.",
                rec.info.id,
                rec.info.error.clone().unwrap_or_default()
            )),
        }
    }

    fn workspace_machine(&self, rec: &WorkspaceRecord) -> Result<MachineSpec, String> {
        let (spec, _) = self.machine(&rec.info.machine_id).ok_or_else(|| {
            format!(
                "Machine {} not found in config.toml anymore, so Muxen can't reach workspace {}. Destroy it with keepFiles to forget it.",
                rec.info.machine_name, rec.info.id
            )
        })?;
        if !spec.is_checkable() {
            return Err(format!("{} has no host in config.toml.", spec.name));
        }
        Ok(spec)
    }

    fn workspace_info(&self, id: &str) -> Option<WorkspaceInfo> {
        self.current().workspaces.iter().find(|w| w.id == id).cloned()
    }

    fn unused_workspace_id(&self) -> String {
        let taken: Vec<String> = self.workspace_records().into_iter().map(|r| r.info.id).collect();
        loop {
            let id = format!("ws-{}", random_id(6));
            if !taken.contains(&id) {
                return id;
            }
        }
    }

    fn touch_workspace(&self, id: &str) {
        let _ = self.update_workspaces(|list| {
            if let Some(r) = list.iter_mut().find(|r| r.info.id == id) {
                r.info.last_used_at = Utc::now();
            }
        });
    }

    async fn run_create(
        self: &Arc<Self>,
        op: u64,
        spec: &MachineSpec,
        rec: &WorkspaceRecord,
        root: &str,
        repo: Option<&Repo>,
    ) -> Result<String, (String, Option<String>)> {
        self.op_log(op, format!("▸ Creating {} on {} ({})", rec.info.id, spec.name, spec.host.trim()));
        let folder: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let attempt = |auth: Option<String>| {
            let folder = folder.clone();
            async move {
            let auth_s = auth.clone().unwrap_or_default();
            let repo_url = repo.map(|r| r.url.clone()).unwrap_or_default();
            let repo_shown = repo.map(|r| r.shown.clone()).unwrap_or_default();
            let git_ref = rec.info.git_ref.clone().unwrap_or_default();
            let branch = rec.info.branch.clone().unwrap_or_default();
            let container = rec.info.container.clone().unwrap_or_default();
            let image = rec.info.image.clone().unwrap_or_default();
            let mut params = vec![
                ("OR_ROOT", Param::Str(root)),
                ("OR_ID", Param::Str(&rec.info.id)),
                ("OR_REPO", Param::Str(&repo_url)),
                ("OR_REPO_SHOWN", Param::Str(&repo_shown)),
                ("OR_REF", Param::Str(&git_ref)),
                ("OR_BRANCH", Param::Str(&branch)),
                ("OR_AUTH", Param::Str(&auth_s)),
            ];
            let script = if rec.windows {
                params.insert(0, ("OR_ACTION", Param::Str("create")));
                with_params(Shell::Powershell, &params, OTHER_PS1)
            } else {
                params.extend([
                    ("OR_CONTAINER", Param::Str(&container)),
                    ("OR_IMAGE", Param::Str(&image)),
                    ("OR_SOCKET", Param::Bool(rec.docker_socket)),
                ]);
                with_params(Shell::Sh, &params, &create_script(!container.is_empty()))
            };
            let mut secrets = Vec::new();
            if let Some(a) = &auth {
                secrets.push(a.clone());
                if let Some(enc) = a.strip_prefix("AUTHORIZATION: basic ") {
                    secrets.push(enc.to_string());
                    if let Some(tok) = B64.decode(enc).ok().and_then(|b| String::from_utf8(b).ok()) {
                        if let Some(t) = tok.strip_prefix("x-access-token:") {
                            secrets.push(t.to_string());
                        }
                    }
                }
            }
            let me = self.clone();
            let log = move |line: &str| match line.strip_prefix("path=") {
                Some(p) => {
                    *folder.lock().unwrap() = Some(p.trim().to_string());
                    me.op_log(op, format!("Folder: {p}"))
                }
                None if line == "ready" => {}
                None => me.op_log(op, line.to_string()),
            };
            run_checked(spec, rec.windows, script, CREATE_TIMEOUT, Limits::OUTPUT, &secrets, Some(&log)).await
            }
        };
        let fail = |message: String| (message, folder.lock().unwrap().clone());

        let out = match attempt(None).await {
            Ok(out) => out,
            Err(e) => {
                // A private GitHub repo: try again with the user's token, passed only through the environment.
                let retry = match repo {
                    Some(r) if r.on_github && e.code == Some(4) => {
                        self.github().git_credentials(&self.settings().github).await.filter(|(host, _)| *host == r.host)
                    }
                    _ => None,
                };
                let Some((_, token)) = retry else { return Err(fail(e.message)) };
                self.op_log(op, "▸ The clone needs credentials; retrying with your GitHub token");
                let header = format!("AUTHORIZATION: basic {}", B64.encode(format!("x-access-token:{token}")));
                attempt(Some(header)).await.map_err(|e| fail(e.message))?
            }
        };
        field(&out.stdout, "path").ok_or_else(|| fail("The machine didn't report the workspace folder.".to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_exec(
        &self,
        spec: &MachineSpec,
        rec: &WorkspaceRecord,
        command: &str,
        cwd: &str,
        env: &[(String, String)],
        timeout: u64,
        log: Option<String>,
    ) -> Result<ExecResult, String> {
        let timeout_s = timeout.to_string();
        let log_s = log.clone().unwrap_or_default();
        let shell = if rec.windows { Shell::Powershell } else { Shell::Sh };
        let params = [
            ("OR_DIR", Param::Str(&rec.info.path)),
            ("OR_CWD", Param::Str(cwd)),
            ("OR_CMD", Param::Str(command)),
            ("OR_TIMEOUT", Param::Str(&timeout_s)),
            ("OR_BG", Param::Bool(log.is_some())),
            ("OR_LOG", Param::Str(&log_s)),
            ("OR_ID", Param::Str(&rec.info.id)),
        ];
        let exports = env_exports(shell, env);
        let script = match (&rec.info.container, shell) {
            (Some(container), Shell::Sh) => {
                let image = rec.info.image.clone().unwrap_or_else(|| DEFAULT_IMAGE.to_string());
                let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
                let names = names.join(" ");
                let all = [
                    ("OR_DIR", Param::Str(&rec.info.path)),
                    ("OR_CWD", Param::Str(cwd)),
                    ("OR_CMD", Param::Str(command)),
                    ("OR_TIMEOUT", Param::Str(&timeout_s)),
                    ("OR_BG", Param::Bool(log.is_some())),
                    ("OR_LOG", Param::Str(&log_s)),
                    ("OR_ID", Param::Str(&rec.info.id)),
                    ("OR_CONTAINER", Param::Str(container)),
                    ("OR_IMAGE", Param::Str(&image)),
                    ("OR_SOCKET", Param::Bool(rec.docker_socket)),
                    ("OR_ENV", Param::Str(&names)),
                ];
                with_params(shell, &all, &format!("{exports}{LIB_SH}\n{DOCKER_SH}\n{EXEC_DOCKER_SH}"))
            }
            (_, Shell::Sh) => with_params(shell, &params, &format!("{exports}{LIB_SH}\n{EXEC_SH}")),
            (_, Shell::Powershell) => with_params(shell, &params, &format!("{exports}{EXEC_PS1}")),
        };
        let marker = transport::new_marker();
        let out = transport::run(
            spec,
            rec.windows,
            &marker,
            transport::with_marker(&marker, rec.windows, &script),
            Duration::from_secs(timeout + EXEC_GRACE),
            Limits::OUTPUT,
            &[],
            None,
        )
        .await?;
        exec_result(out, &spec.name, log)
    }

    async fn run_files(&self, spec: &MachineSpec, rec: &WorkspaceRecord, params: &[(&str, Param<'_>)]) -> Result<Output, String> {
        let mut all = vec![("OR_DIR", Param::Str(&rec.info.path))];
        for (k, v) in params {
            all.push((k, match v {
                Param::Str(s) => Param::Str(s),
                Param::List(l) => Param::List(l),
                Param::Bool(b) => Param::Bool(*b),
            }));
        }
        let (script, windows) = if rec.windows {
            (with_params(Shell::Powershell, &all, FILES_PS1), true)
        } else {
            (with_params(Shell::Sh, &all, FILES_SH), false)
        };
        run_checked(spec, windows, script, FILE_TIMEOUT, Limits::DATA, &[], None).await.map_err(|e| e.message)
    }

    async fn run_destroy(self: &Arc<Self>, op: u64, spec: &MachineSpec, rec: &WorkspaceRecord, keep: bool) -> Result<(), String> {
        self.op_log(op, format!("▸ Cleaning up on {} ({})", spec.name, spec.host.trim()));
        let pids: Vec<String> = rec.background_pids.iter().map(u32::to_string).collect();
        let pids = pids.join(" ");
        let script = if rec.windows {
            with_params(
                Shell::Powershell,
                &[
                    ("OR_ACTION", Param::Str("destroy")),
                    ("OR_DIR", Param::Str(&rec.info.path)),
                    ("OR_ID", Param::Str(&rec.info.id)),
                    ("OR_PIDS", Param::Str(&pids)),
                    ("OR_KEEP", Param::Bool(keep)),
                ],
                OTHER_PS1,
            )
        } else {
            let container = rec.info.container.clone().unwrap_or_default();
            let image = rec.info.image.clone().unwrap_or_default();
            with_params(
                Shell::Sh,
                &[
                    ("OR_DIR", Param::Str(&rec.info.path)),
                    ("OR_ID", Param::Str(&rec.info.id)),
                    ("OR_PIDS", Param::Str(&pids)),
                    ("OR_KEEP", Param::Bool(keep)),
                    ("OR_CONTAINER", Param::Str(&container)),
                    ("OR_IMAGE", Param::Str(&image)),
                ],
                &format!("{LIB_SH}\n{DESTROY_SH}"),
            )
        };
        let me = self.clone();
        let log = move |line: &str| me.op_log(op, line.to_string());
        run_checked(spec, rec.windows, script, DESTROY_TIMEOUT, Limits::OUTPUT, &[], Some(&log)).await.map(|_| ()).map_err(|e| e.message)
    }
}

// ---------------------------------------------------------------------------
// Registry plumbing used by the engine.

pub(crate) fn load_registry(path: &std::path::Path) -> Vec<WorkspaceRecord> {
    registry::load(path)
}

pub(crate) fn update_registry<R>(
    path: &std::path::Path,
    f: impl FnOnce(&mut Vec<WorkspaceRecord>) -> R,
) -> Result<(R, Vec<WorkspaceRecord>, Option<std::time::SystemTime>), String> {
    registry::update(path, f)
}

pub(crate) fn demo_seed(world: &crate::demo::World) -> Vec<WorkspaceRecord> {
    demo::seed(world)
}

/// What the snapshot shows: stored records with current machine names and a
/// ready-to-run shell command, most recently used first.
pub(crate) fn snapshot_infos(records: &[WorkspaceRecord], machines: &[Machine], specs: &[MachineSpec]) -> Vec<WorkspaceInfo> {
    let mut out: Vec<WorkspaceInfo> = records
        .iter()
        .map(|r| {
            let mut info = r.info.clone();
            if let Some(m) = machines.iter().find(|m| m.id == info.machine_id) {
                info.machine_name = m.name.clone();
                info.host = m.host.clone();
                if !m.platform.is_empty() {
                    info.platform = m.platform.clone();
                }
            }
            if let Some(spec) = specs.iter().find(|s| s.id == info.machine_id) {
                info.shell = match &info.container {
                    Some(c) => docker_shell_command(spec, c),
                    None => shell_command(spec, &info.path, r.windows),
                };
            }
            info
        })
        .collect();
    out.sort_by(|a, b| b.last_used_at.cmp(&a.last_used_at));
    out
}

// ---------------------------------------------------------------------------
// Helpers

/// A failure from a workspace script: the reason and the script's exit code.
struct ScriptError {
    message: String,
    code: Option<i32>,
}

/// Runs a script that reports failure through its exit status (`error: ...` on stderr).
async fn run_checked(
    spec: &MachineSpec,
    windows: bool,
    script: String,
    timeout: Duration,
    limits: Limits,
    secrets: &[String],
    on_line: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<Output, ScriptError> {
    let marker = transport::new_marker();
    let out = transport::run(spec, windows, &marker, transport::with_marker(&marker, windows, &script), timeout, limits, secrets, on_line)
        .await
        .map_err(|message| ScriptError { message, code: None })?;
    if out.timed_out {
        return Err(ScriptError { message: format!("{} is unreachable: no answer within {}s.", spec.name, timeout.as_secs()), code: None });
    }
    if !out.started {
        return Err(ScriptError { message: format!("{} is unreachable: {}", spec.name, out.reason()), code: out.status });
    }
    match out.status {
        Some(0) => Ok(out),
        code => Err(ScriptError { message: out.reason(), code }),
    }
}

impl From<ScriptError> for String {
    fn from(e: ScriptError) -> String {
        e.message
    }
}

fn exec_result(out: Output, machine: &str, log: Option<String>) -> Result<ExecResult, String> {
    if !out.started {
        return Err(format!("{machine} is unreachable: {}", out.reason()));
    }
    const SETUP: [&str; 5] = [
        "error: the workspace folder is gone",
        "error: there's no folder",
        "error: docker isn't available",
        "error: couldn't pull",
        "error: couldn't start a container",
    ];
    let setup_error = out.exit == Some(97) && out.stderr.lines().next().is_some_and(|l| SETUP.iter().any(|p| l.starts_with(p)));
    if setup_error {
        return Err(out.reason());
    }
    if out.exit.is_none() && !out.timed_out {
        return Err(format!("The ssh connection to {machine} dropped while the command ran: {}", out.reason()));
    }
    let background_pid = if log.is_some() { field(&out.stdout, "pid").and_then(|p| p.parse().ok()) } else { None };
    Ok(ExecResult {
        exit_code: if out.timed_out || log.is_some() { None } else { out.exit },
        stdout: if log.is_some() { String::new() } else { out.stdout },
        stderr: out.stderr,
        duration_ms: 0,
        timed_out: out.timed_out,
        truncated: out.truncated,
        background_pid,
        log_path: log,
    })
}

/// `export NAME='value'` (sh) or `$env:NAME = 'value'` (PowerShell) lines.
fn env_exports(shell: Shell, env: &[(String, String)]) -> String {
    env.iter()
        .map(|(k, v)| match shell {
            Shell::Sh => format!("export {k}={}\n", sh_quote(v)),
            Shell::Powershell => format!("$env:{k} = {}\n", ps_quote(v)),
        })
        .collect()
}

/// The value of a `key=value` line in script output.
fn field(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    text.lines().find_map(|l| l.strip_prefix(&prefix)).map(|v| v.trim().to_string())
}

fn file_content(path: String, bytes: Vec<u8>, size: u64, max: usize) -> FileContent {
    let truncated = size > max as u64 || bytes.len() < size as usize;
    let text = match String::from_utf8(bytes) {
        Ok(t) => Some(t),
        // A cut can land inside a multi-byte character; keep the valid part.
        Err(e) if truncated && e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut b = e.into_bytes();
            b.truncate(valid);
            String::from_utf8(b).ok()
        }
        Err(_) => None,
    };
    match text.filter(|t| !t.contains('\0')) {
        Some(content) => FileContent { path, content, size, truncated, binary: false },
        None => FileContent { path, content: String::new(), size, truncated, binary: true },
    }
}

fn parse_listing(text: &str, base: &str) -> Vec<FileEntry> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let kind = match parts.next()? {
                "d" => "dir",
                "l" => "link",
                _ => "file",
            };
            let size = parts.next()?.trim().parse().unwrap_or(0);
            let rel = parts.next()?.trim_start_matches("./");
            if rel.is_empty() {
                return None;
            }
            let path = if base.is_empty() { rel.to_string() } else { format!("{base}/{rel}") };
            Some(FileEntry { path, kind: kind.into(), size: if kind == "dir" { 0 } else { size } })
        })
        .take(LIST_MAX_ENTRIES)
        .collect()
}

/// The last lines of a command's output, for history.
fn output_tail(stdout: &str, stderr: &str) -> Vec<String> {
    let lines: Vec<String> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|l| !l.trim().is_empty())
        .map(|l| if l.chars().count() > TAIL_LINE_MAX { format!("{}…", l.chars().take(TAIL_LINE_MAX).collect::<String>()) } else { l.to_string() })
        .collect();
    lines[lines.len().saturating_sub(registry::TAIL_LINES)..].to_vec()
}

fn clean_opt(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// A path relative to the workspace, normalized to forward slashes. Rejects
/// absolute paths and `..`, so agents don't wander out by accident.
pub fn rel_path(p: &str) -> Result<String, String> {
    let p = p.trim();
    let drive = p.len() >= 2 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic();
    if p.starts_with('/') || p.starts_with('\\') || p.starts_with('~') || drive {
        return Err(format!("\"{p}\" isn't inside the workspace. Paths are relative to the workspace folder."));
    }
    let mut parts = Vec::new();
    for seg in p.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => return Err(format!("\"{p}\" leaves the workspace. Paths can't use \"..\".")),
            s if s.chars().any(char::is_control) => return Err(format!("\"{p}\" has control characters.")),
            s => parts.push(s),
        }
    }
    Ok(parts.join("/"))
}

fn validate_env(name: &str, value: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(format!("\"{name}\" isn't a valid environment variable name."));
    }
    if value.contains('\0') {
        return Err(format!("The value of {name} contains a NUL byte."));
    }
    Ok(())
}

fn validate_git_ref(r: &str) -> Result<(), String> {
    let bad = r.len() > 200
        || r.starts_with('-')
        || r.contains("..")
        || r.ends_with('/')
        || r.ends_with(".lock")
        || r.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c));
    if bad { Err(format!("\"{r}\" isn't a valid git branch, tag or commit.")) } else { Ok(()) }
}

/// A repository to clone.
struct Repo {
    /// What git clones.
    url: String,
    /// Shown in the UI and logs (no credentials).
    shown: String,
    /// "api" for acme/api.
    short_name: String,
    host: String,
    /// https on the GitHub host Muxen has a token for.
    on_github: bool,
}

pub(crate) fn github_web_host(api_url: &str) -> String {
    let host = api_url.split("://").nth(1).unwrap_or(api_url).split('/').next().unwrap_or("api.github.com");
    if host == "api.github.com" { "github.com".into() } else { host.to_string() }
}

/// "owner/repo" means GitHub; anything else must look like a git URL or a path on the machine.
fn normalize_repo(raw: &str, gh_host: &str) -> Result<Repo, String> {
    let raw = raw.trim();
    if raw.starts_with('-') || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("\"{raw}\" isn't a repository Muxen can clone."));
    }
    let short = |s: &str| {
        s.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or(s).trim_end_matches(".git").to_string()
    };
    let simple = raw.split('/').count() == 2
        && !raw.contains(':')
        && !raw.starts_with('.')
        && raw.split('/').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)));
    if simple {
        let path = raw.trim_end_matches(".git");
        return Ok(Repo {
            url: format!("https://{gh_host}/{path}.git"),
            shown: path.to_string(),
            short_name: short(path),
            host: gh_host.to_string(),
            on_github: true,
        });
    }
    let is_url = ["https://", "http://", "ssh://", "git://", "file://"].iter().any(|p| raw.starts_with(p));
    let scp_like = raw.contains('@') && raw.contains(':') && !raw.contains("://");
    let path_like = raw.starts_with('/') || raw.starts_with("~/");
    if !(is_url || scp_like || path_like) {
        return Err(format!("\"{raw}\" isn't a repository. Use owner/repo for GitHub, or a git URL."));
    }
    // Never echo credentials embedded in a URL.
    let shown = match raw.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_, after)) if rest.find('@') < rest.find('/') => format!("{scheme}://{after}"),
            _ => raw.to_string(),
        },
        None => raw.to_string(),
    };
    let host = raw
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or("").rsplit('@').next().unwrap_or("").to_string())
        .unwrap_or_default();
    Ok(Repo {
        url: raw.to_string(),
        short_name: short(&shown),
        on_github: raw.starts_with("https://") && host.eq_ignore_ascii_case(gh_host),
        host,
        shown,
    })
}

fn machine_names(machines: &[Machine]) -> String {
    machines.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", ")
}

/// "macos" → "darwin", "x86_64" → "x64", ... so agents can say it however they like.
fn normalize_platform(p: &str) -> String {
    let p = p.trim().to_ascii_lowercase().replace('_', "-");
    let (os, arch) = p.split_once('-').map(|(o, a)| (o.to_string(), Some(a.to_string()))).unwrap_or((p.clone(), None));
    let os = match os.as_str() {
        "mac" | "macos" | "osx" | "apple" => "darwin".to_string(),
        "win" | "win32" => "windows".to_string(),
        o => o.to_string(),
    };
    let arch = arch.map(|a| match a.as_str() {
        "amd64" | "x86-64" | "x8664" | "86-64" => "x64".to_string(),
        "aarch64" => "arm64".to_string(),
        a => a.to_string(),
    });
    match arch {
        Some(a) => format!("{os}-{a}"),
        None => os,
    }
}

/// The online machine that allows workspaces with the most headroom.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn pick_machine<'a>(machines: &'a [Machine], platform: Option<&str>) -> Result<&'a Machine, String> {
    pick_machine_for(machines, platform, false)
}

/// Like `pick_machine`; for docker workspaces, prefers machines whose last
/// check found docker (when no check has reported docker yet, any will do —
/// the create step checks for real).
pub(crate) fn pick_machine_for<'a>(machines: &'a [Machine], platform: Option<&str>, docker: bool) -> Result<&'a Machine, String> {
    let want = platform.map(normalize_platform);
    let platform_ok = |m: &Machine| want.as_deref().is_none_or(|w| m.platform.starts_with(w));
    let disk_ok = |m: &Machine| m.probe.as_ref().and_then(|p| p.disk_free).is_none_or(|f| f >= MIN_FREE_DISK);
    let mut candidates: Vec<&Machine> = machines
        .iter()
        .filter(|m| m.status == "online" && m.workspaces_allowed && platform_ok(m) && disk_ok(m) && !(docker && m.platform.starts_with("windows")))
        .collect();
    if docker && candidates.iter().any(|m| m.probe.as_ref().is_some_and(|p| p.docker.is_some())) {
        candidates.retain(|m| m.probe.as_ref().is_some_and(|p| p.docker.is_some()));
    }
    let score = |m: &Machine| {
        let p = m.probe.as_ref();
        let cpu = p.and_then(|p| p.cpu_pct).unwrap_or(50.0);
        let mem = p.and_then(|p| Some(p.mem_used? as f64 / p.mem_total?.max(1) as f64 * 100.0)).unwrap_or(50.0);
        let low_disk = p.and_then(|p| p.disk_free).is_some_and(|f| f < 20_000_000_000);
        cpu + mem + m.summary.working as f64 * 20.0 + if low_disk { 100.0 } else { 0.0 }
    };
    candidates
        .into_iter()
        .min_by(|a, b| score(a).total_cmp(&score(b)).then_with(|| a.name.cmp(&b.name)))
        .ok_or_else(|| {
            let what = match (&want, docker) {
                (Some(w), true) => format!("allows workspaces, runs {w} and can run docker"),
                (Some(w), false) => format!("allows workspaces and runs {w}"),
                (None, true) => "allows workspaces and can run docker".into(),
                (None, false) => "allows workspaces".into(),
            };
            let online: Vec<String> = machines
                .iter()
                .filter(|m| m.status == "online")
                .map(|m| format!("{} ({}{})", m.id, m.platform, if m.workspaces_allowed { "" } else { ", workspaces off" }))
                .collect();
            if online.is_empty() {
                format!("No machine is online right now. Run check_machines, or add a machine.")
            } else {
                format!("No online machine {what} and has free disk. Online: {}.", online.join(", "))
            }
        })
}

/// A command a human can paste to get a shell inside the workspace.
pub(crate) fn shell_command(spec: &MachineSpec, path: &str, windows: bool) -> String {
    if spec.is_local() {
        return if windows { format!("Set-Location {}", ps_quote(path)) } else { format!("cd {} && exec \"$SHELL\" -l", sh_quote(path)) };
    }
    let mut parts = vec!["ssh".to_string(), "-t".to_string()];
    if let Some(port) = spec.port {
        parts.push(format!("-p {port}"));
    }
    if let Some(user) = spec.user.as_deref().filter(|u| !u.is_empty()) {
        parts.push(format!("-l {user}"));
    }
    parts.extend(spec.ssh_options.iter().map(|o| display_arg(o)));
    parts.push(spec.host.trim().to_string());
    let inner = if windows {
        format!("\"powershell -NoExit -Command Set-Location '{path}'\"")
    } else {
        format!("\"cd '{path}' && exec \\$SHELL -l\"")
    };
    parts.push(inner);
    parts.join(" ")
}

/// `muxen-ws-<id>`.
pub(crate) fn container_name(id: &str) -> String {
    format!("muxen-ws-{}", id.trim_start_matches("ws-"))
}

/// A docker image reference: registry/name:tag or name@sha256:..., nothing that
/// could be read as a flag.
fn validate_image(image: &str) -> Result<String, String> {
    let ok = !image.is_empty()
        && image.len() <= 255
        && !image.starts_with(['-', '.', '/', ':', '@'])
        && image.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@".contains(c));
    if ok { Ok(image.to_string()) } else { Err(format!("\"{image}\" isn't a docker image name (like node:22 or ghcr.io/acme/dev:latest).")) }
}

/// The create script: make the folder (and clone) on the host, then, for docker
/// workspaces, check docker first and start the container last.
fn create_script(docker: bool) -> String {
    if docker {
        format!(
            "{DOCKER_SH}\nor_docker_check || exit 6\n{CREATE_SH}\nor_docker_up \"$(pwd -P)\" || exit 6\necho \"Container $OR_CONTAINER is running\"\n"
        )
    } else {
        CREATE_SH.to_string()
    }
}

/// A shell inside a docker workspace's container, for a human.
pub(crate) fn docker_shell_command(spec: &MachineSpec, container: &str) -> String {
    let inner = format!("docker start {container} >/dev/null && docker exec -it -w /workspace {container} sh -c 'exec bash 2>/dev/null || exec sh'");
    if spec.is_local() {
        return inner;
    }
    let mut parts = vec!["ssh".to_string(), "-t".to_string()];
    if let Some(port) = spec.port {
        parts.push(format!("-p {port}"));
    }
    if let Some(user) = spec.user.as_deref().filter(|u| !u.is_empty()) {
        parts.push(format!("-l {user}"));
    }
    parts.extend(spec.ssh_options.iter().map(|o| display_arg(o)));
    parts.push(spec.host.trim().to_string());
    parts.push(format!("\"{inner}\""));
    parts.join(" ")
}

fn display_arg(a: &str) -> String {
    if a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@~,".contains(c)) { a.to_string() } else { sh_quote(a) }
}

fn rsync_args(spec: &MachineSpec, remote_path: &str, local: &str, direction: SyncDirection, delete: bool) -> Vec<String> {
    let mut args = vec!["-az".to_string(), "--stats".to_string(), "--exclude=/.muxen/".to_string()];
    if delete {
        args.push("--delete".into());
    }
    let remote = if spec.is_local() {
        format!("{}/", remote_path.trim_end_matches('/'))
    } else {
        let mut ssh = crate::probe::ssh_args(spec);
        ssh.pop(); // the host goes in the remote path
        let ssh: Vec<String> = ssh.iter().map(|a| display_arg(a)).collect();
        args.push("-e".into());
        args.push(format!("ssh {}", ssh.join(" ")));
        format!("{}:{}/", spec.host.trim(), remote_path.trim_end_matches('/'))
    };
    let local = format!("{}/", local.trim_end_matches('/'));
    match direction {
        SyncDirection::Push => {
            args.push(local);
            args.push(remote);
        }
        SyncDirection::Pull => {
            args.push(remote);
            args.push(local);
        }
    }
    args
}

fn expand_home(p: &str) -> std::path::PathBuf {
    match (p.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if p == "~" => dirs::home_dir().unwrap_or_default(),
        _ => std::path::PathBuf::from(p),
    }
}

/// Lowercase base36, from a small per-process generator (no extra dependency).
pub(crate) fn random_id(len: usize) -> String {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)
        ^ (std::process::id() as u64).rotate_left(32);
    let mut x = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed).wrapping_add(seed);
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..len)
        .map(|_| {
            // splitmix64
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            DIGITS[(z % 36) as usize] as char
        })
        .collect()
}

#[cfg(test)]
mod tests;
