//! Control operations: add machines, install runners on them, and start, stop,
//! restart, inspect or remove those runners. Each operation runs in the
//! background, streams its output into `Snapshot::operations`, and refreshes the
//! affected machine when it finishes.
//!
//! Operation log conventions: lines starting with "▸ " are Muxen's own
//! steps, "✓ " marks a success, "error: " and "warning: " are problems; anything
//! else is output from the machine.
//!
//! PUBLIC API — the UI depends on these signatures.

pub(crate) mod cloud;
pub(crate) mod inventory_edit;
pub(crate) mod remote;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{MachineSpec, Settings};
use crate::fleet::model::Runner;
use crate::fleet::Engine;
use crate::github::scope_from_runner_url;
use crate::probe::{self, ProbeResult, Shell};
use remote::{with_params, Param};

const INSTALL_SH: &str = include_str!("scripts/install.sh");
const INSTALL_PS1: &str = include_str!("scripts/install.ps1");
const CONTROL_SH: &str = include_str!("scripts/control.sh");
const CONTROL_PS1: &str = include_str!("scripts/control.ps1");
const INSTALL_DOCKER_SH: &str = include_str!("scripts/install_docker.sh");
const CONTROL_DOCKER_SH: &str = include_str!("scripts/control_docker.sh");
/// GitHub's official runner image; tagged with each runner release.
const RUNNER_IMAGE: &str = "ghcr.io/actions/actions-runner";

const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// Used when GitHub can't tell us the latest runner release.
const FALLBACK_RUNNER_VERSION: &str = "2.329.0";

/// Install one or more runners on a machine.
#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub machine_id: String,
    /// The org ("acme") or repo ("acme/api") to register the runners with.
    pub scope: String,
    /// One runner per name, e.g. ["mac-studio-08", "mac-studio-09"].
    pub names: Vec<String>,
    /// Extra labels on top of the defaults GitHub adds (self-hosted, OS, arch).
    pub labels: Vec<String>,
    /// Org runner group; None means "Default".
    pub group: Option<String>,
    /// Legacy switch, still honoured: false with `mode: Auto` means `Background`.
    pub as_service: bool,
    /// How the runner runs on the machine. `Auto` picks per machine at install time.
    pub mode: InstallMode,
}

/// How an installed runner runs on its machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InstallMode {
    /// Service when the machine allows it (macOS, Windows, Linux with passwordless
    /// sudo), else Docker when it's available, else Background.
    #[default]
    Auto,
    /// launchd / systemd / Windows service: starts at boot. Linux needs passwordless sudo.
    Service,
    /// run.sh in the background: no root needed, but gone after a reboot.
    Background,
    /// A container per runner from GitHub's actions-runner image, restarted by the
    /// Docker daemon. No sudo needed; jobs run in a clean Linux userland.
    Docker {
        /// None: ghcr.io/actions/actions-runner:<latest runner version>.
        image: Option<String>,
        /// Mount the host's Docker socket so jobs can use Docker (root-equivalent).
        docker_socket: bool,
    },
}

impl InstallMode {
    /// "auto" | "service" | "background" | "docker"
    pub fn name(&self) -> &'static str {
        match self {
            InstallMode::Auto => "auto",
            InstallMode::Service => "service",
            InstallMode::Background => "background",
            InstallMode::Docker { .. } => "docker",
        }
    }

    /// From the API/CLI spelling. `image` and `docker_socket` only apply to docker.
    pub fn parse(mode: &str, image: Option<String>, docker_socket: bool) -> Result<InstallMode, String> {
        let image = image.map(|i| i.trim().to_string()).filter(|i| !i.is_empty());
        let mode = match mode.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => InstallMode::Auto,
            "service" => InstallMode::Service,
            "background" | "run" => InstallMode::Background,
            "docker" | "container" => InstallMode::Docker { image: image.clone(), docker_socket },
            other => return Err(format!("\"{other}\" isn't an install mode. Use auto, service, background or docker.")),
        };
        if !matches!(mode, InstallMode::Docker { .. }) && (image.is_some() || docker_socket) {
            return Err("image and docker_socket only apply to mode \"docker\".".into());
        }
        Ok(mode)
    }
}

/// What `Auto` means on this machine, plus a sentence for the log when the choice
/// isn't the obvious one.
pub(crate) fn resolve_mode(mode: &InstallMode, as_service: bool, probe: &ProbeResult) -> (InstallMode, Option<String>) {
    let docker = || InstallMode::Docker { image: None, docker_socket: false };
    match mode {
        InstallMode::Auto if !as_service => (InstallMode::Background, None),
        InstallMode::Auto => match probe.os.as_deref() {
            Some("linux") => match probe.sudo.as_deref() {
                Some("root" | "passwordless") => (InstallMode::Service, None),
                _ if probe.docker.is_some() => (
                    docker(),
                    Some("sudo asks for a password here, so a systemd service isn't possible; using Docker containers instead".into()),
                ),
                _ => (
                    InstallMode::Background,
                    Some("sudo asks for a password and Docker isn't available here, so the runners start in the background and won't come back after a reboot".into()),
                ),
            },
            _ => (InstallMode::Service, None),
        },
        other => (other.clone(), None),
    }
}

fn valid_image(image: &str) -> bool {
    !image.is_empty()
        && image.len() <= 255
        && !image.starts_with(['-', '.', '/', ':'])
        && image.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:@".contains(c))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerAction {
    Start,
    Stop,
    Restart,
    /// Tail the newest _diag log into the operation output (read-only).
    Logs,
    /// Stop, uninstall the service, deregister from GitHub, and drop it from config.toml.
    Remove { delete_files: bool },
}

impl RunnerAction {
    pub fn label(self) -> &'static str {
        match self {
            RunnerAction::Start => "Start",
            RunnerAction::Stop => "Stop",
            RunnerAction::Restart => "Restart",
            RunnerAction::Logs => "Logs",
            RunnerAction::Remove { .. } => "Remove",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            RunnerAction::Start => "start",
            RunnerAction::Stop => "stop",
            RunnerAction::Restart => "restart",
            RunnerAction::Logs => "logs",
            RunnerAction::Remove { .. } => "remove",
        }
    }
}

/// A machine to add to config.toml.
#[derive(Debug, Clone)]
pub struct NewMachine {
    pub name: String,
    /// An ssh destination ("ci@studio.local", an ~/.ssh/config alias) or "local".
    pub host: String,
}

impl Engine {
    /// Starts installing runners. Returns the operation id.
    pub fn install_runners(self: &Arc<Self>, req: InstallRequest) -> Result<u64, String> {
        let req = self.validate_install(req)?;
        let (spec, probe) = self.machine(&req.machine_id).ok_or("That machine isn't in config.toml anymore.")?;
        let title = match req.names.as_slice() {
            [one] => format!("Install {one} on {}", spec.name),
            many => format!("Install {} runners on {}", many.len(), spec.name),
        };
        let op = self.begin_op("install", title, Some(spec.id.clone()), None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = match me.demo_world().cloned() {
                Some(_) => demo_install(&me, op, &req, &spec).await,
                None => install(&me, op, &req, &spec, probe).await,
            };
            finish(&me, op, result);
        });
        Ok(op)
    }

    /// Starts an action on a runner (id as in `Runner::id`). Returns the operation id.
    pub fn runner_action(self: &Arc<Self>, runner_id: &str, action: RunnerAction) -> Result<u64, String> {
        let snap = self.current();
        let runner = snap.runners.iter().find(|r| r.id == runner_id).cloned().ok_or("That runner isn't in the fleet anymore.")?;
        let machine = runner.machine_id.as_deref().and_then(|id| self.machine(id));
        let reachable = runner
            .machine_id
            .as_deref()
            .and_then(|id| snap.machines.iter().find(|m| m.id == id))
            .is_some_and(|m| m.status == "online");
        let dir = runner.local.as_ref().map(|l| l.dir.clone());
        let container = runner.local.as_ref().and_then(|l| l.container.clone());

        let plan = match action {
            // The last check may be stale (one that ran while the machine
            // restarted): look again before settling for GitHub alone, which
            // would leave the runner's service and folder on the machine.
            RunnerAction::Remove { delete_files } if machine.as_ref().is_some_and(|(spec, _)| spec.is_checkable()) && !(reachable && dir.is_some()) => {
                Plan::CheckFirst { delete_files }
            }
            RunnerAction::Remove { delete_files } => self.remove_plan(&runner, delete_files)?,
            _ => {
                let (spec, probe) = machine.clone().ok_or_else(|| {
                    format!("{} isn't on a machine in config.toml, so Muxen can't control it. It can only be removed from GitHub.", runner.name)
                })?;
                if !spec.is_checkable() {
                    return Err(format!("{} has no host in config.toml.", spec.name));
                }
                let dir = dir.clone().ok_or_else(|| {
                    format!("{} wasn't found on {} at the last check. Check the machine again, or install the runner.", runner.name, spec.name)
                })?;
                Plan::OnMachine { spec, probe, dir, container, scope: None, delete_files: false }
            }
        };

        if action != RunnerAction::Logs {
            if let Some(mid) = runner.machine_id.as_deref() {
                if self.machine_busy(mid) {
                    return Err("Another operation is already changing this machine. Wait for it to finish.".into());
                }
            }
        }

        let title = match action {
            RunnerAction::Logs => format!("Logs · {}", runner.name),
            other => format!("{} {}", other.label(), runner.name),
        };
        let op = self.begin_op(action.kind(), title, runner.machine_id.clone(), Some(runner.id.clone()));
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = match me.demo_world().cloned() {
                Some(_) => demo_action(&me, op, &runner, action).await,
                None => run_action(&me, op, &runner, action, plan).await,
            };
            finish(&me, op, result);
        });
        Ok(op)
    }

    /// How to remove a runner, from what the last check and GitHub said.
    fn remove_plan(&self, runner: &Runner, delete_files: bool) -> Result<Plan, String> {
        let snap = self.current();
        let machine = runner.machine_id.as_deref().and_then(|id| self.machine(id));
        let reachable = runner.machine_id.as_deref().and_then(|id| snap.machines.iter().find(|m| m.id == id)).is_some_and(|m| m.status == "online");
        let dir = runner.local.as_ref().map(|l| l.dir.clone());
        let on_machine = machine.filter(|(spec, _)| spec.is_checkable() && reachable && dir.is_some());
        Ok(match on_machine {
            Some((spec, probe)) => {
                let scope = runner_scope(runner).ok_or_else(|| format!("Muxen doesn't know which GitHub org or repo {} is registered with.", runner.name))?;
                Plan::OnMachine {
                    spec,
                    probe,
                    dir: dir.unwrap_or_default(),
                    container: runner.local.as_ref().and_then(|l| l.container.clone()),
                    scope: Some(scope),
                    delete_files,
                }
            }
            None if runner.github.is_some() => Plan::GitHubOnly,
            None if runner.declared => Plan::InventoryOnly,
            None => return Err(format!("{} isn't on a reachable machine or on GitHub, so there's nothing to remove.", runner.name)),
        })
    }

    /// Cancels the workflow run a queued or running job belongs to (all of
    /// its jobs, as GitHub cancels whole runs). Returns the operation id.
    pub fn cancel_run(self: &Arc<Self>, job_id: u64) -> Result<u64, String> {
        let snap = self.current();
        let job = snap.jobs.queued.iter().chain(snap.jobs.running.iter()).find(|j| j.id == job_id).cloned().ok_or("That job isn't queued or running anymore.")?;
        if self.demo_world().is_some() {
            return Err("The demo can't cancel runs.".into());
        }
        let op = self.begin_op("cancel-run", format!("Cancel {} in {}", job.workflow, job.repo), None, None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let settings = me.settings();
            me.op_log(op, format!("Cancelling run {} of {} ({})", job.run_id, job.workflow, job.repo));
            let result = me.github().cancel_run(&settings.github, &job.repo, job.run_id).await;
            if result.is_ok() {
                me.op_log(op, "GitHub is cancelling it; its jobs stop in a few seconds.");
                me.trigger_sync();
            }
            finish(&me, op, result);
        });
        Ok(op)
    }

    /// Adds a machine to config.toml (keeping its comments and formatting) and checks it.
    /// Returns the new machine's id.
    pub fn add_machine(self: &Arc<Self>, machine: NewMachine) -> Result<String, String> {
        let name = machine.name.trim().to_string();
        let host = machine.host.trim().to_string();
        validate_machine(&name, &host)?;
        let taken = self.current().machines.iter().any(|m| m.name.eq_ignore_ascii_case(&name));
        if taken {
            return Err(format!("A machine called \"{name}\" is already in the fleet."));
        }

        let mut new_id = None;
        let edit = |text: &str| {
            let (out, id) = inventory_edit::add_machine(text, &name, &host)?;
            new_id = Some(id);
            Ok(out)
        };
        match self.demo_world() {
            Some(w) => w.edit_inventory(edit)?,
            None => inventory_edit::edit_file(self.inventory_path(), edit)?,
        }
        let id = new_id.ok_or("The machine wasn't added.")?;
        self.reload_inventory_now();
        self.trigger_check(Some(id.clone()));
        Ok(id)
    }

    /// Removes a machine from config.toml. Doesn't touch the machine itself.
    pub fn remove_machine(self: &Arc<Self>, machine_id: &str) -> Result<(), String> {
        if self.machine(machine_id).is_none() {
            return Err("That machine isn't in config.toml anymore.".into());
        }
        if self.machine_busy(machine_id) {
            return Err("An operation is still running on this machine. Wait for it to finish.".into());
        }
        self.forget_machine(machine_id)
    }

    /// Gives a machine a new name in config.toml. Its id doesn't change, so
    /// its runners, sessions, favorites and history stay with it.
    pub fn rename_machine(self: &Arc<Self>, machine_id: &str, name: &str) -> Result<(), String> {
        let name = name.trim();
        validate_name(name)?;
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in config.toml anymore.")?;
        if spec.name == name {
            return Ok(());
        }
        if self.current().machines.iter().any(|m| m.id != spec.id && m.name.eq_ignore_ascii_case(name)) {
            return Err(format!("A machine called \"{name}\" is already in the fleet."));
        }
        let edit = |text: &str| inventory_edit::rename_machine(text, &spec.id, name);
        match self.demo_world() {
            Some(w) => w.edit_inventory(edit)?,
            None => inventory_edit::edit_file(self.inventory_path(), edit)?,
        }
        self.reload_inventory_now();
        Ok(())
    }

    /// Drops a machine from config.toml, for an operation that is itself
    /// what's changing it (destroying a cloud machine).
    pub(crate) fn forget_machine(self: &Arc<Self>, machine_id: &str) -> Result<(), String> {
        let edit = |text: &str| inventory_edit::remove_machine(text, machine_id);
        match self.demo_world() {
            Some(w) => w.edit_inventory(edit)?,
            None => inventory_edit::edit_file(self.inventory_path(), edit)?,
        }
        self.reload_inventory_now();
        Ok(())
    }

    /// Unused runner names for a machine: "<machine-id>-01", "-02", ... skipping taken ones.
    pub fn suggest_runner_names(&self, machine_id: &str, count: usize) -> Vec<String> {
        let taken: HashSet<String> = self.current().runners.iter().map(|r| r.name.to_ascii_lowercase()).collect();
        (1..)
            .map(|i| format!("{machine_id}-{i:02}"))
            .filter(|n| !taken.contains(&n.to_ascii_lowercase()))
            .take(count)
            .collect()
    }

    fn validate_install(&self, mut req: InstallRequest) -> Result<InstallRequest, String> {
        let (spec, _) = self.machine(&req.machine_id).ok_or("That machine isn't in config.toml anymore.")?;
        if !spec.is_checkable() {
            return Err(format!("{} has no host in config.toml, so Muxen can't reach it.", spec.name));
        }
        if self.machine_busy(&spec.id) {
            return Err(format!("Another operation is already changing {}. Wait for it to finish.", spec.name));
        }

        req.scope = crate::config::strip_github_url(&req.scope);
        if req.scope.is_empty() {
            return Err("Pick the GitHub org or repo to register the runners with.".into());
        }
        if req.scope.split('/').count() > 2 || !req.scope.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)) {
            return Err(format!("\"{}\" isn't an org or owner/repo.", req.scope));
        }

        req.names = req.names.iter().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect();
        if req.names.is_empty() {
            return Err("Give the runner a name.".into());
        }
        let taken: HashSet<String> = self.current().runners.iter().map(|r| r.name.to_ascii_lowercase()).collect();
        let mut seen = HashSet::new();
        for n in &req.names {
            if !valid_runner_name(n) {
                return Err(format!("\"{n}\" isn't a valid runner name. Use letters, numbers, '-', '_' and '.'."));
            }
            if !seen.insert(n.to_ascii_lowercase()) {
                return Err(format!("\"{n}\" is listed twice."));
            }
            if taken.contains(&n.to_ascii_lowercase()) {
                return Err(format!("A runner called \"{n}\" already exists."));
            }
        }

        req.labels = req.labels.iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        for l in &req.labels {
            if l.len() > 100 || !l.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c)) {
                return Err(format!("\"{l}\" isn't a valid label. Use letters, numbers, '-', '_', '.', ':' and '/'."));
            }
        }
        if let InstallMode::Docker { image: Some(image), .. } = &req.mode {
            if !valid_image(image) {
                return Err(format!("\"{image}\" isn't a Docker image reference."));
            }
        }
        if matches!(req.mode, InstallMode::Docker { .. }) && (spec.shell == Some(Shell::Powershell)) {
            return Err(format!("Docker runners need a machine with a POSIX shell; {} uses PowerShell.", spec.name));
        }
        req.group = req.group.map(|g| g.trim().to_string()).filter(|g| !g.is_empty() && !g.eq_ignore_ascii_case("default"));
        if let Some(g) = &req.group {
            if g.len() > 100 || !g.chars().all(|c| c.is_ascii_alphanumeric() || " -_.".contains(c)) {
                return Err(format!("\"{g}\" isn't a valid runner group name."));
            }
            if req.scope.contains('/') {
                return Err("Runner groups only apply to organization runners.".into());
            }
        }
        Ok(req)
    }
}

/// Host aliases from ~/.ssh/config (no wildcards), for the "Add machine" suggestions.
pub fn ssh_hosts() -> Vec<String> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    std::fs::read_to_string(home.join(".ssh").join("config")).map(|t| parse_ssh_hosts(&t)).unwrap_or_default()
}

/// Git hosting entries in ~/.ssh/config aren't machines you'd put runners on.
fn is_git_host(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["github.com", "gitlab.com", "bitbucket.org", "codeberg.org", "ssh.dev.azure.com", "vs-ssh.visualstudio.com"]
        .iter()
        .any(|g| n == *g || n.ends_with(&format!(".{g}")))
}

fn parse_ssh_hosts(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        // "Host a b", "Host=a", "host a # comment"
        let (key, rest) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some(parts) => parts,
            None => continue,
        };
        if !key.eq_ignore_ascii_case("host") {
            continue;
        }
        let rest = rest.split('#').next().unwrap_or("");
        for name in rest.split(|c: char| c.is_whitespace() || c == '=').map(|n| n.trim_matches('"')) {
            if !name.is_empty() && !name.contains(['*', '?', '!']) && !is_git_host(name) && !hosts.iter().any(|h| h == name) {
                hosts.push(name.to_string());
            }
        }
    }
    hosts.sort();
    hosts
}

// ---------------------------------------------------------------------------

enum Plan {
    /// Run the control script on the runner's machine.
    OnMachine { spec: MachineSpec, probe: Option<ProbeResult>, dir: String, container: Option<String>, scope: Option<String>, delete_files: bool },
    /// The machine is gone or unreachable: deregister through the GitHub API.
    GitHubOnly,
    /// Remove, but check the runner's machine first and plan from that.
    CheckFirst { delete_files: bool },
    /// Not installed anywhere we can see; just drop it from config.toml.
    InventoryOnly,
}

fn step(engine: &Arc<Engine>, op: u64, text: impl std::fmt::Display) {
    engine.op_log(op, format!("▸ {text}"));
}

fn finish(engine: &Arc<Engine>, op: u64, result: Result<(), String>) {
    if let Err(e) = &result {
        engine.op_log(op, format!("error: {e}"));
    }
    engine.op_finish(op, result);
}

fn valid_runner_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with(['-', '.'])
        && n.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Give the machine a name.".into());
    }
    if name.len() > 80 || name.chars().any(char::is_control) {
        return Err("That name is too long or has odd characters.".into());
    }
    Ok(())
}

fn validate_machine(name: &str, host: &str) -> Result<(), String> {
    validate_name(name)?;
    if host.is_empty() {
        return Err("Enter an ssh destination (like ci@studio.local or an ~/.ssh/config alias), or \"local\".".into());
    }
    // A leading '-' would be read by ssh as an option.
    if host.starts_with('-') || host.len() > 255 || !host.chars().all(|c| c.is_ascii_alphanumeric() || "@._-:[]%".contains(c)) {
        return Err(format!("\"{host}\" isn't an ssh destination Muxen can use."));
    }
    Ok(())
}

/// "org:acme" -> "acme"; falls back to the gitHubUrl in the runner's .runner file.
fn runner_scope(runner: &Runner) -> Option<String> {
    runner
        .github
        .as_ref()
        .map(|g| g.scope.split_once(':').map(|(_, s)| s.to_string()).unwrap_or_else(|| g.scope.clone()))
        .or_else(|| runner.local.as_ref().and_then(|l| l.url.as_deref()).and_then(scope_from_runner_url))
}

/// Names the install script reported as done ("✓ <name> is installed").
fn installed_names(engine: &Engine, op: u64, names: &[String]) -> Vec<String> {
    let snap = engine.current();
    let Some(o) = snap.operations.iter().find(|o| o.id == op) else { return Vec::new() };
    names.iter().filter(|n| o.log.iter().any(|l| l == &format!("✓ {n} is installed"))).cloned().collect()
}

async fn install(engine: &Arc<Engine>, op: u64, req: &InstallRequest, spec: &MachineSpec, probe: Option<ProbeResult>) -> Result<(), String> {
    let settings: Settings = engine.settings();
    // Always look at the machine first: the install mode depends on what it can do
    // right now (sudo, Docker), and the last check may be old.
    step(engine, op, format!("Checking {}", spec.name));
    let probe = probe::probe(spec, probe.and_then(|p| p.shell)).await;
    if !probe.ok {
        return Err(format!("Couldn't reach {}: {}", spec.name, probe.error.unwrap_or_default()));
    }
    let (mode, why) = resolve_mode(&req.mode, req.as_service, &probe);
    engine.op_log(op, format!("Install mode: {}{}", mode_label(&mode), why.map(|w| format!(" ({w})")).unwrap_or_default()));
    if let InstallMode::Docker { image, docker_socket } = &mode {
        return install_docker(engine, op, req, spec, &probe, image.clone(), *docker_socket).await;
    }
    let as_service = mode == InstallMode::Service;
    let shell = remote::shell_for(spec, Some(&probe));

    step(engine, op, format!("Requesting a registration token from GitHub for {}", req.scope));
    let token = engine.github().registration_token(&settings.github, &req.scope).await?;
    step(engine, op, "Looking up the latest runner release");
    let version = engine
        .github()
        .latest_runner_release(&settings.github)
        .await
        .unwrap_or_else(|| FALLBACK_RUNNER_VERSION.to_string());
    engine.op_log(op, format!("actions-runner {version}"));

    step(engine, op, format!("Installing on {} ({})", spec.name, spec.host.trim()));
    let url = engine.github().web_url(&settings.github, &req.scope);
    let labels = req.labels.join(",");
    let group = req.group.clone().unwrap_or_default();
    let params = [
        ("OR_VERSION", Param::Str(&version)),
        ("OR_URL", Param::Str(&url)),
        ("OR_TOKEN", Param::Str(&token)),
        ("OR_NAMES", Param::List(&req.names)),
        ("OR_LABELS", Param::Str(&labels)),
        ("OR_GROUP", Param::Str(&group)),
        ("OR_SERVICE", Param::Bool(as_service)),
        ("OR_BASE", Param::Str("")),
    ];
    let script = match shell {
        Shell::Sh => with_params(shell, &params, INSTALL_SH),
        Shell::Powershell => with_params(shell, &params, INSTALL_PS1),
    };
    let ran = remote::run(engine, op, spec, shell, script, &[token], INSTALL_TIMEOUT).await;
    record_installed(engine, op, req, spec, ran).await
}

fn mode_label(mode: &InstallMode) -> String {
    match mode {
        InstallMode::Auto | InstallMode::Service => mode.name().into(),
        InstallMode::Background => "background (run.sh, not restarted after a reboot)".into(),
        InstallMode::Docker { image, docker_socket } => format!(
            "Docker{}{}",
            image.as_ref().map(|i| format!(" ({i})")).unwrap_or_default(),
            if *docker_socket { ", with the host's Docker socket" } else { "" }
        ),
    }
}

/// Containers from GitHub's runner image: no sudo, restarted by the Docker daemon.
#[allow(clippy::too_many_arguments)]
async fn install_docker(
    engine: &Arc<Engine>,
    op: u64,
    req: &InstallRequest,
    spec: &MachineSpec,
    probe: &ProbeResult,
    image: Option<String>,
    docker_socket: bool,
) -> Result<(), String> {
    if probe.docker.is_none() {
        let why = probe.docker_error.clone().unwrap_or_else(|| "it isn't installed".into());
        return Err(format!("Docker isn't usable on {}: {why}", spec.name));
    }
    if probe.os.as_deref() == Some("windows") {
        return Err("Docker runners aren't supported on Windows machines yet.".into());
    }
    let settings = engine.settings();
    step(engine, op, format!("Requesting a registration token from GitHub for {}", req.scope));
    let token = engine.github().registration_token(&settings.github, &req.scope).await?;
    let (image, fallback) = match image {
        Some(i) => (i, String::new()),
        None => {
            step(engine, op, "Looking up the latest runner release");
            let version = engine.github().latest_runner_release(&settings.github).await.unwrap_or_else(|| FALLBACK_RUNNER_VERSION.to_string());
            (format!("{RUNNER_IMAGE}:{version}"), format!("{RUNNER_IMAGE}:latest"))
        }
    };
    let url = engine.github().web_url(&settings.github, &req.scope);
    let labels = docker_labels(&req.labels).join(",");
    let group = req.group.clone().unwrap_or_default();
    step(engine, op, format!("Starting runner containers on {} ({})", spec.name, spec.host.trim()));
    let script = docker_install_script(&image, &fallback, &url, &req.scope, &token, &req.names, &labels, &group, docker_socket, false);
    let ran = remote::run(engine, op, spec, Shell::Sh, script, &[token], INSTALL_TIMEOUT).await;
    record_installed(engine, op, req, spec, ran).await
}

/// The user's labels plus "docker", so workflows can target container runners.
fn docker_labels(labels: &[String]) -> Vec<String> {
    let mut out = labels.to_vec();
    if !out.iter().any(|l| l.eq_ignore_ascii_case("docker")) {
        out.push("docker".into());
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn docker_install_script(
    image: &str,
    fallback: &str,
    url: &str,
    scope: &str,
    token: &str,
    names: &[String],
    labels: &str,
    group: &str,
    docker_socket: bool,
    stub: bool,
) -> String {
    let params = [
        ("OR_IMAGE", Param::Str(image)),
        ("OR_IMAGE_FALLBACK", Param::Str(fallback)),
        ("OR_URL", Param::Str(url)),
        ("OR_SCOPE", Param::Str(scope)),
        ("OR_TOKEN", Param::Str(token)),
        ("OR_NAMES", Param::List(names)),
        ("OR_LABELS", Param::Str(labels)),
        ("OR_GROUP", Param::Str(group)),
        ("OR_SOCKET", Param::Bool(docker_socket)),
        ("OR_STUB", Param::Bool(stub)),
    ];
    with_params(Shell::Sh, &params, INSTALL_DOCKER_SH)
}

/// Adds what got installed to config.toml (even if a later runner failed) and refreshes.
async fn record_installed(engine: &Arc<Engine>, op: u64, req: &InstallRequest, spec: &MachineSpec, ran: Result<(), String>) -> Result<(), String> {
    let done = if ran.is_ok() { req.names.clone() } else { installed_names(engine, op, &req.names) };
    if !done.is_empty() {
        step(engine, op, format!("Adding {} to config.toml", done.join(", ")));
        let edit = inventory_edit::edit_file(engine.inventory_path(), |t| inventory_edit::add_runners(t, &spec.id, &done));
        if let Err(e) = edit {
            engine.op_log(op, format!("warning: {e} The runners still show up because Muxen discovers them."));
        }
        engine.reload_inventory_now();
    }
    refresh(engine, op, Some(spec)).await;
    ran
}

async fn run_action(engine: &Arc<Engine>, op: u64, runner: &Runner, action: RunnerAction, plan: Plan) -> Result<(), String> {
    let settings = engine.settings();
    let plan = match plan {
        Plan::CheckFirst { delete_files } => {
            let mid = runner.machine_id.clone().unwrap_or_default();
            step(engine, op, format!("Checking {mid} first"));
            engine.run_check(Some(vec![mid])).await;
            let fresh = engine.current().runners.iter().find(|r| r.id == runner.id).cloned().unwrap_or_else(|| runner.clone());
            engine.remove_plan(&fresh, delete_files)?
        }
        plan => plan,
    };
    match plan {
        Plan::OnMachine { spec, probe, dir, container, scope, delete_files } => {
            let shell = if container.is_some() { Shell::Sh } else { remote::shell_for(&spec, probe.as_ref()) };
            let mut secrets = Vec::new();
            let mut token = String::new();
            if let (RunnerAction::Remove { .. }, Some(scope)) = (action, scope.as_deref()) {
                step(engine, op, format!("Requesting a removal token from GitHub for {scope}"));
                token = match engine.github().remove_token(&settings.github, scope).await {
                    Ok(t) => t,
                    // The repo or org is gone (its runners with it), or out of reach: still clean up the machine.
                    Err(e) if e.contains("wasn't found") => {
                        engine.op_log(op, format!("! {e} Removing it from the machine only; if GitHub still lists it, remove it there."));
                        String::new()
                    }
                    Err(e) => return Err(e),
                };
                if !token.is_empty() {
                    secrets.push(token.clone());
                }
            }
            step(engine, op, format!("{} {} on {}", verb(action), runner.name, spec.name));
            let script = match &container {
                Some(c) => control_docker_script(action, c, &token, delete_files),
                None => {
                    let params = [
                        ("OR_ACTION", Param::Str(action.kind())),
                        ("OR_DIR", Param::Str(&dir)),
                        ("OR_TOKEN", Param::Str(&token)),
                        ("OR_DELETE", Param::Bool(delete_files)),
                    ];
                    match shell {
                        Shell::Sh => with_params(shell, &params, CONTROL_SH),
                        Shell::Powershell => with_params(shell, &params, CONTROL_PS1),
                    }
                }
            };
            remote::run(engine, op, &spec, shell, script, &secrets, CONTROL_TIMEOUT).await?;
            if let RunnerAction::Remove { .. } = action {
                // A folder deleted by hand leaves the runner registered: finish that through the API.
                if let Some(gh) = runner.github.as_ref() {
                    engine.sync_github().await;
                    let registered = engine.current().runners.iter().any(|r| r.github.as_ref().is_some_and(|g| g.id == gh.id && g.scope == gh.scope));
                    if registered {
                        step(engine, op, format!("Removing {} from GitHub ({})", runner.name, gh.scope));
                        engine.github().delete_runner(&settings.github, &gh.scope, gh.id).await?;
                    }
                }
                forget_runner(engine, op, runner);
            }
            if action != RunnerAction::Logs {
                refresh(engine, op, Some(&spec)).await;
            }
            Ok(())
        }
        Plan::GitHubOnly => {
            let gh = runner.github.as_ref().ok_or("GitHub doesn't know this runner.")?;
            step(engine, op, format!("Removing {} from GitHub ({})", runner.name, gh.scope));
            engine.github().delete_runner(&settings.github, &gh.scope, gh.id).await?;
            engine.op_log(op, format!("✓ {} is no longer registered", runner.name));
            if let Some(mid) = &runner.machine_id {
                engine.op_log(op, format!("! {mid} couldn't be reached, so {}'s service and folder are still there. Remove them on the machine when it's back.", runner.name));
            }
            forget_runner(engine, op, runner);
            refresh(engine, op, None).await;
            Ok(())
        }
        Plan::InventoryOnly => {
            forget_runner(engine, op, runner);
            refresh(engine, op, None).await;
            Ok(())
        }
        Plan::CheckFirst { .. } => unreachable!("planned above"),
    }
}

fn control_docker_script(action: RunnerAction, container: &str, token: &str, delete_files: bool) -> String {
    let params = [
        ("OR_ACTION", Param::Str(action.kind())),
        ("OR_CONTAINER", Param::Str(container)),
        ("OR_TOKEN", Param::Str(token)),
        ("OR_DELETE", Param::Bool(delete_files)),
    ];
    with_params(Shell::Sh, &params, CONTROL_DOCKER_SH)
}

/// Drops a removed runner from config.toml if it was listed there.
fn forget_runner(engine: &Arc<Engine>, op: u64, runner: &Runner) {
    let (Some(mid), true) = (runner.machine_id.as_deref(), runner.declared) else { return };
    step(engine, op, format!("Removing {} from config.toml", runner.name));
    let edit = |t: &str| inventory_edit::remove_runner(t, mid, &runner.name);
    let result = match engine.demo_world() {
        Some(w) => w.edit_inventory(edit),
        None => inventory_edit::edit_file(engine.inventory_path(), edit),
    };
    if let Err(e) = result {
        engine.op_log(op, format!("warning: {e}"));
    }
    engine.reload_inventory_now();
}

/// Re-checks the machine (if any) and syncs GitHub so the dashboard shows the result.
async fn refresh(engine: &Arc<Engine>, op: u64, spec: Option<&MachineSpec>) {
    if let Some(spec) = spec {
        step(engine, op, format!("Checking {}", spec.name));
        engine.run_check(Some(vec![spec.id.clone()])).await;
    }
    engine.sync_github().await;
}

fn verb(action: RunnerAction) -> &'static str {
    match action {
        RunnerAction::Start => "Starting",
        RunnerAction::Stop => "Stopping",
        RunnerAction::Restart => "Restarting",
        RunnerAction::Logs => "Reading the log of",
        RunnerAction::Remove { .. } => "Removing",
    }
}

// ---------------------------------------------------------------------------
// Demo mode: the same steps against the simulated fleet, nothing leaves the process.

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn demo_install(engine: &Arc<Engine>, op: u64, req: &InstallRequest, spec: &MachineSpec) -> Result<(), String> {
    let w = engine.demo_world().cloned().ok_or("not in demo mode")?;
    step(engine, op, format!("Checking {}", spec.name));
    pause(300).await;
    w.reachable(&spec.id, spec.host.trim())?;
    let probe = w.probe(spec).await;
    let (mode, why) = resolve_mode(&req.mode, req.as_service, &probe);
    engine.op_log(op, format!("Install mode: {}{}", mode_label(&mode), why.map(|w| format!(" ({w})")).unwrap_or_default()));
    if let InstallMode::Docker { image, docker_socket } = &mode {
        if probe.docker.is_none() {
            return Err(format!("Docker isn't usable on {}: it isn't installed", spec.name));
        }
        return demo_install_docker(engine, op, req, spec, image.clone(), *docker_socket).await;
    }
    let as_service = mode == InstallMode::Service;
    step(engine, op, format!("Requesting a registration token from GitHub for {}", req.scope));
    pause(500).await;
    step(engine, op, "Looking up the latest runner release");
    pause(300).await;
    engine.op_log(op, "actions-runner 2.329.0");
    step(engine, op, format!("Installing on {} ({})", spec.name, spec.host.trim()));
    pause(400).await;
    w.reachable(&spec.id, spec.host.trim())?;

    let (os, arch) = w.runner_platform(&spec.id);
    let home = match os {
        "win" => "C:\\actions-runner".to_string(),
        "linux" => "/home/ci/actions-runner".to_string(),
        _ => "/Users/ci/actions-runner".to_string(),
    };
    let sep = if os == "win" { "\\" } else { "/" };
    engine.op_log(op, format!("Downloading actions-runner 2.329.0 for {os}-{arch}…"));
    pause(900).await;
    for name in &req.names {
        let dir = format!("{home}{sep}{name}");
        let url = format!("https://github.com/{}", req.scope);
        let lines = [
            format!("Setting up {name} in {dir}"),
            "--------------------------------------------------------------------------------".into(),
            "|        Self-hosted runner registration                                       |".into(),
            "--------------------------------------------------------------------------------".into(),
            "# Authentication".into(),
            format!("√ Connected to GitHub ({url})"),
            "# Runner Registration".into(),
            format!("√ Runner successfully added ({name})"),
            "√ Runner connection is good".into(),
            "# Runner settings".into(),
            "√ Settings Saved.".into(),
        ];
        for l in lines {
            engine.op_log(op, l);
            pause(140).await;
        }
        if as_service {
            engine.op_log(op, format!("Installing {name} as a service…"));
            pause(300).await;
            engine.op_log(op, match os {
                "osx" => format!("Creating launch runner in /Users/ci/Library/LaunchAgents/actions.runner.{}.{name}.plist", req.scope.replace('/', "-")),
                "win" => format!("Service actions.runner.{}.{name} successfully installed", req.scope.replace('/', "-")),
                _ => format!("Creating launch runner in /etc/systemd/system/actions.runner.{}.{name}.service", req.scope.replace('/', "-")),
            });
            engine.op_log(op, format!("Starting actions.runner.{}.{name}", req.scope.replace('/', "-")));
        } else {
            engine.op_log(op, format!("Starting {name} in the background (not a service, so it won't come back after a reboot)…"));
        }
        pause(250).await;
        engine.op_log(op, format!("✓ {name} is installed"));
    }

    w.install_runners(&spec.id, &req.names, &req.labels, false);
    step(engine, op, format!("Adding {} to config.toml", req.names.join(", ")));
    w.edit_inventory(|t| inventory_edit::add_runners(t, &spec.id, &req.names))?;
    engine.reload_inventory_now();
    refresh(engine, op, Some(spec)).await;
    Ok(())
}

async fn demo_install_docker(engine: &Arc<Engine>, op: u64, req: &InstallRequest, spec: &MachineSpec, image: Option<String>, docker_socket: bool) -> Result<(), String> {
    let w = engine.demo_world().cloned().ok_or("not in demo mode")?;
    step(engine, op, format!("Requesting a registration token from GitHub for {}", req.scope));
    pause(500).await;
    let image = image.unwrap_or_else(|| format!("{RUNNER_IMAGE}:2.329.0"));
    step(engine, op, format!("Starting runner containers on {} ({})", spec.name, spec.host.trim()));
    engine.op_log(op, format!("Pulling {image} (about 2 GB the first time)…"));
    pause(400).await;
    for l in ["2.329.0: Pulling from actions/actions-runner", "Digest: sha256:7c1f0d2e9a4b…", &format!("Status: Downloaded newer image for {image}")] {
        engine.op_log(op, l.to_string());
        pause(250).await;
    }
    if docker_socket {
        engine.op_log(op, "warning: the runners get this machine's Docker socket, which is root-equivalent access to the machine. Only run trusted workflows on them.");
    }
    for name in &req.names {
        engine.op_log(op, format!("Setting up {name} (container muxen-{name}, volume muxen-{name})"));
        pause(300).await;
        for l in [
            "# Authentication".to_string(),
            format!("√ Connected to GitHub (https://github.com/{})", req.scope),
            "# Runner Registration".into(),
            format!("√ Runner successfully added ({name})"),
            "√ Runner connection is good".into(),
            "√ Settings Saved.".into(),
        ] {
            engine.op_log(op, l);
            pause(140).await;
        }
        engine.op_log(op, format!("✓ {name} is installed"));
    }
    w.install_runners(&spec.id, &req.names, &docker_labels(&req.labels), true);
    step(engine, op, format!("Adding {} to config.toml", req.names.join(", ")));
    w.edit_inventory(|t| inventory_edit::add_runners(t, &spec.id, &req.names))?;
    engine.reload_inventory_now();
    refresh(engine, op, Some(spec)).await;
    Ok(())
}

async fn demo_action(engine: &Arc<Engine>, op: u64, runner: &Runner, action: RunnerAction) -> Result<(), String> {
    let w = engine.demo_world().cloned().ok_or("not in demo mode")?;
    let machine = runner.machine_id.as_deref().and_then(|id| engine.machine(id));
    if let Some((spec, _)) = &machine {
        step(engine, op, format!("{} {} on {}", verb(action), runner.name, spec.name));
        pause(400).await;
        w.reachable(&spec.id, spec.host.trim())?;
    }
    let container = runner.local.as_ref().and_then(|l| l.container.clone());
    let service = match &container {
        Some(c) => format!("container {c}"),
        None => format!("service actions.runner.acme.{}", runner.name),
    };
    match action {
        RunnerAction::Start => {
            engine.op_log(op, format!("Starting {service}…"));
            pause(700).await;
            w.set_runner_online(&runner.name, true);
            engine.op_log(op, "✓ Started");
        }
        RunnerAction::Stop => {
            engine.op_log(op, format!("Stopping {service}…"));
            pause(700).await;
            w.set_runner_online(&runner.name, false);
            engine.op_log(op, "✓ Stopped");
        }
        RunnerAction::Restart => {
            engine.op_log(op, format!("Stopping {service}…"));
            pause(500).await;
            w.set_runner_online(&runner.name, false);
            engine.op_log(op, format!("Starting {service}…"));
            pause(700).await;
            w.set_runner_online(&runner.name, true);
            engine.op_log(op, "✓ Restarted");
        }
        RunnerAction::Logs => {
            let dir = runner.local.as_ref().map(|l| l.dir.clone()).unwrap_or_default();
            engine.op_log(op, match &container {
                Some(c) => format!("==> docker logs {c} (last 200 lines)"),
                None => format!("==> {dir}/_diag/Runner_20261008-141502-utc.log (last 200 lines)"),
            });
            for line in w.diag_log(&runner.name) {
                engine.op_log(op, line);
            }
            return Ok(());
        }
        RunnerAction::Remove { delete_files } => {
            if runner.machine_id.is_none() {
                step(engine, op, format!("Removing {} from GitHub (org:acme)", runner.name));
                pause(600).await;
            } else {
                step(engine, op, "Requesting a removal token from GitHub for acme");
                pause(400).await;
                engine.op_log(op, format!("Stopping {service}…"));
                pause(400).await;
                if container.is_none() {
                    engine.op_log(op, "Uninstalling the service…");
                    pause(300).await;
                }
                engine.op_log(op, "Removing the runner from GitHub…");
                engine.op_log(op, "√ Runner removed successfully");
                if let Some(c) = &container {
                    engine.op_log(op, format!("Removed container {c}"));
                    if delete_files {
                        engine.op_log(op, format!("Deleted volume {c}"));
                    }
                } else if delete_files {
                    let dir = runner.local.as_ref().map(|l| l.dir.clone()).unwrap_or_default();
                    engine.op_log(op, format!("Deleted {dir}"));
                }
            }
            w.remove_runner(&runner.name);
            engine.op_log(op, "✓ Removed");
            forget_runner(engine, op, runner);
        }
    }
    refresh(engine, op, machine.as_ref().map(|(spec, _)| spec)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_config_hosts() {
        let text = "\
Host beelink beelink-dev
  HostName 10.0.0.4
Host *.internal
Host=umbrel
host github.com # comment
Host !bad ok? fine
Match host x
";
        assert_eq!(parse_ssh_hosts(text), vec!["beelink", "beelink-dev", "fine", "umbrel"]);
    }

    #[test]
    fn parses_install_modes() {
        assert_eq!(InstallMode::parse("", None, false).unwrap(), InstallMode::Auto);
        assert_eq!(InstallMode::parse("Service", None, false).unwrap(), InstallMode::Service);
        assert_eq!(
            InstallMode::parse("docker", Some(" ghcr.io/me/runner:1 ".into()), true).unwrap(),
            InstallMode::Docker { image: Some("ghcr.io/me/runner:1".into()), docker_socket: true }
        );
        assert!(InstallMode::parse("vm", None, false).is_err());
        assert!(InstallMode::parse("service", Some("x".into()), false).is_err(), "image only goes with docker");
        assert!(valid_image("ghcr.io/actions/actions-runner:2.329.0"));
        assert!(valid_image("me/runner@sha256:abc"));
        assert!(!valid_image("--privileged"));
        assert!(!valid_image("a b"));
        assert_eq!(docker_labels(&["gpu".into()]), vec!["gpu", "docker"]);
        assert_eq!(docker_labels(&["Docker".into()]), vec!["Docker"]);
    }

    #[test]
    fn auto_mode_follows_the_machine() {
        let machine = |os: &str, sudo: Option<&str>, docker: Option<&str>| ProbeResult {
            ok: true,
            os: Some(os.into()),
            sudo: sudo.map(String::from),
            docker: docker.map(String::from),
            ..Default::default()
        };
        let auto = |p: &ProbeResult| resolve_mode(&InstallMode::Auto, true, p).0;
        assert_eq!(auto(&machine("darwin", None, Some("29"))), InstallMode::Service);
        assert_eq!(auto(&machine("windows", None, None)), InstallMode::Service);
        assert_eq!(auto(&machine("linux", Some("passwordless"), Some("29"))), InstallMode::Service);
        assert_eq!(auto(&machine("linux", Some("root"), None)), InstallMode::Service);
        assert_eq!(auto(&machine("linux", Some("password"), Some("29"))), InstallMode::Docker { image: None, docker_socket: false });
        let (mode, why) = resolve_mode(&InstallMode::Auto, true, &machine("linux", Some("password"), None));
        assert_eq!(mode, InstallMode::Background);
        assert!(why.unwrap().contains("reboot"));
        // The legacy switch still means "background", and explicit modes are kept.
        assert_eq!(resolve_mode(&InstallMode::Auto, false, &machine("darwin", None, None)).0, InstallMode::Background);
        assert_eq!(resolve_mode(&InstallMode::Service, true, &machine("linux", Some("password"), None)).0, InstallMode::Service);
    }

    #[test]
    fn docker_install_script_keeps_the_token_out_of_argv() {
        let script = docker_install_script(
            "ghcr.io/actions/actions-runner:2.329.0",
            "ghcr.io/actions/actions-runner:latest",
            "https://github.com/acme",
            "acme",
            "SECRET'TOKEN",
            &["b-01".into()],
            "gpu,docker",
            "",
            false,
            false,
        );
        assert!(script.starts_with("OR_IMAGE='ghcr.io/actions/actions-runner:2.329.0'\n"));
        assert!(script.contains("OR_TOKEN='SECRET'\\''TOKEN'\n"), "{}", &script[..400]);
        // The token is only ever piped in on stdin.
        assert!(script.contains("printf '%s' \"$OR_TOKEN\" | docker run --rm -i"));
        assert!(!script.contains("--token \"$OR_TOKEN\""));
    }

    #[test]
    fn validates_names_and_hosts() {
        assert!(valid_runner_name("mac-studio-08"));
        assert!(valid_runner_name("win_01.a"));
        assert!(!valid_runner_name("-x"));
        assert!(!valid_runner_name("a b"));
        assert!(!valid_runner_name("a'b"));
        assert!(validate_machine("Beelink", "ci@beelink.local").is_ok());
        assert!(validate_machine("Beelink", "local").is_ok());
        assert!(validate_machine("Beelink", "-oProxyCommand=x").is_err());
        assert!(validate_machine("Beelink", "a b").is_err());
        assert!(validate_machine("", "x").is_err());
    }
}

/// End-to-end: real engine, real scripts, against a stub runner on this machine.
#[cfg(all(test, unix))]
mod engine_tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::config::Paths;
    use crate::fleet::model::Operation;

    fn write_exec(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A runner folder whose scripts behave like the real ones, minus GitHub.
    fn stub_runner(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("_diag")).unwrap();
        std::fs::write(dir.join(".runner"), format!("{{\"agentName\":\"{name}\",\"gitHubUrl\":\"https://github.com/acme\"}}")).unwrap();
        std::fs::write(dir.join("_diag/Runner_20261008-120000-utc.log"), "[INFO Listener] Listening for Jobs\n").unwrap();
        write_exec(&dir.join("config.sh"), "#!/bin/sh\nexit 0\n");
        write_exec(&dir.join("run.sh"), "#!/bin/sh\nDIR=$(cd \"$(dirname \"$0\")\" && pwd)\n\"$DIR/bin/Runner.Listener\" run\n");
        write_exec(&dir.join("bin/Runner.Listener"), "#!/bin/sh\n[ \"$1\" = --version ] && { echo 2.329.0; exit 0; }\nsleep 120\n");
    }

    async fn wait(engine: &Arc<Engine>, op: u64) -> Operation {
        for _ in 0..600 {
            if let Some(o) = engine.current().operations.iter().find(|o| o.id == op && o.status != "running") {
                return o.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("operation {op} didn't finish");
    }

    fn state(engine: &Engine, id: &str) -> String {
        engine.current().runners.iter().find(|r| r.id == id).map(|r| r.state.clone()).unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn controls_a_local_runner() {
        let tmp = std::env::temp_dir().join(format!("muxen-ops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let base = tmp.join("runners");
        stub_runner(&base.join("or-test-01"), "or-test-01");
        std::fs::write(
            tmp.join("config.toml"),
            format!("# test fleet\n[[machine]]\nname = \"Here\"\nhost = \"local\"\nrunners = [\"or-test-01\"]\nrunner_dirs = [\"{}/*\"]\n", base.display()),
        )
        .unwrap();

        let engine = Engine::new(Paths::resolve(Some(tmp.clone()), None), false);
        engine.run_check(None).await;
        let id = "here/or-test-01";
        assert_eq!(state(&engine, id), "offline", "{:?}", engine.current().machines[0].diagnostics);

        let op = engine.runner_action(id, RunnerAction::Logs).unwrap();
        let o = wait(&engine, op).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert!(o.log.iter().any(|l| l.contains("Listening for Jobs")), "{:?}", o.log);

        let o = wait(&engine, engine.runner_action(id, RunnerAction::Start).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert_eq!(state(&engine, id), "idle");

        let o = wait(&engine, engine.runner_action(id, RunnerAction::Stop).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert!(o.log.iter().any(|l| l == "Stopped."), "{:?}", o.log);
        assert_eq!(state(&engine, id), "offline");

        // Machines can be added and removed without losing the file's comments.
        let new_id = engine.add_machine(NewMachine { name: "Spare".into(), host: "spare.local".into() }).unwrap();
        assert_eq!(new_id, "spare");
        assert!(engine.current().machines.iter().any(|m| m.id == "spare"));
        engine.remove_machine("spare").unwrap();
        let text = std::fs::read_to_string(tmp.join("config.toml")).unwrap();
        assert!(text.starts_with("# test fleet") && !text.contains("Spare"), "{text}");
        assert_eq!(engine.suggest_runner_names("here", 2), vec!["here-01", "here-02"]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn demo_docker_install() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir()), None), true);
        engine.run_check(Some(vec!["ubuntu-nuc".into()])).await;
        // sudo asks for a password on the demo NUC, so Auto picks Docker.
        let req = InstallRequest {
            machine_id: "ubuntu-nuc".into(),
            scope: "acme".into(),
            names: vec!["nuc-docker-01".into()],
            labels: vec![],
            group: None,
            as_service: true,
            mode: InstallMode::Auto,
        };
        let o = wait(&engine, engine.install_runners(req).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert!(o.log.iter().any(|l| l.starts_with("Install mode: Docker")), "{:?}", o.log);
        let snap = engine.current();
        let r = snap.runners.iter().find(|r| r.id == "ubuntu-nuc/nuc-docker-01").expect("installed runner shows up");
        let local = r.local.as_ref().unwrap();
        assert_eq!(local.container.as_deref(), Some("muxen-nuc-docker-01"));
        assert_eq!(local.dir, "docker://muxen-nuc-docker-01");
        assert!(r.labels.iter().any(|l| l == "docker"), "{:?}", r.labels);
        assert_eq!(snap.machines.iter().find(|m| m.id == "ubuntu-nuc").unwrap().probe.as_ref().unwrap().docker.as_deref(), Some("29.1.3"));

        let o = wait(&engine, engine.runner_action("ubuntu-nuc/nuc-docker-01", RunnerAction::Stop).unwrap()).await;
        assert_eq!(o.status, "succeeded");
        assert!(o.log.iter().any(|l| l.contains("container muxen-nuc-docker-01")), "{:?}", o.log);
        assert_eq!(state(&engine, "ubuntu-nuc/nuc-docker-01"), "offline");
    }

    /// Lines an operation logged (after the coalesced publish catches up).
    async fn op_log(engine: &Engine, op: u64) -> Vec<String> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        engine.current().operations.iter().find(|o| o.id == op).map(|o| o.log.clone()).unwrap_or_default()
    }

    /// Real Docker on a real machine, without GitHub: registration is stubbed, and the one
    /// path that does reach GitHub uses a fake token that can't register anything.
    /// Run with: MUXEN_TEST_DOCKER_HOST=<ssh host> cargo test docker_runner_lifecycle -- --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a machine with Docker: MUXEN_TEST_DOCKER_HOST=<ssh host>"]
    async fn docker_runner_lifecycle_on_real_host() {
        let Ok(host) = std::env::var("MUXEN_TEST_DOCKER_HOST") else { return };
        let tmp = std::env::temp_dir().join(format!("muxen-docker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("config.toml"), format!("[[machine]]\nname = \"Box\"\nhost = \"{host}\"\n")).unwrap();
        let engine = Engine::new(Paths::resolve(Some(tmp.clone()), None), false);
        engine.run_check(None).await;
        let (spec, probe) = engine.machine("box").unwrap();
        let probe = probe.expect("checked");
        assert!(probe.ok, "{:?}", probe.error);
        assert!(probe.docker.is_some(), "Docker should be usable on {host}");
        let image = "ghcr.io/actions/actions-runner:latest";
        let name = format!("ortest-{}", std::process::id());
        let id = format!("box/{name}");
        let run_script = |script: String| {
            let engine = engine.clone();
            let spec = spec.clone();
            async move {
                let op = engine.begin_op("test", "test".into(), Some("box".into()), None);
                let ran = remote::run(&engine, op, &spec, Shell::Sh, script, &["FAKE0TOKEN".into()], INSTALL_TIMEOUT).await;
                engine.op_finish(op, Ok(()));
                (ran, op_log(&engine, op).await)
            }
        };

        // Leftovers from an earlier run of this test that panicked.
        let _ = run_script(
            "docker ps -aq --filter name=muxen-ortest- | xargs -r docker rm -f; docker volume ls -q --filter name=muxen-ortest- | xargs -r docker volume rm".into(),
        )
        .await;

        // 1. A real registration attempt with a fake token fails cleanly: GitHub says no,
        //    and no volume or container is left behind.
        let script = docker_install_script(image, "", "https://github.com/muxen-test-no-such-org-7f3a", "muxen-test-no-such-org-7f3a", "FAKE0TOKEN", &[name.clone()], "docker", "", false, false);
        let (ran, log) = run_script(script).await;
        assert!(ran.is_err(), "{log:?}");
        assert!(log.iter().any(|l| l.contains("404") || l.contains("Not Found")), "{log:?}");
        assert!(log.iter().all(|l| !l.contains("FAKE0TOKEN")), "token leaked into the log: {log:?}");
        let (_, leftovers) = run_script(format!("docker volume ls -q --filter name=muxen-{name}; docker ps -aq --filter name=muxen-{name}")).await;
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // 2. Install with registration stubbed: a long-running container with a restart policy.
        let script = docker_install_script(image, "", "https://github.com/muxen-test", "muxen-test", "FAKE0TOKEN", &[name.clone()], "docker", "", false, true);
        let (ran, log) = run_script(script).await;
        assert!(ran.is_ok(), "{log:?}");
        assert!(log.iter().any(|l| l == &format!("✓ {name} is installed")), "{log:?}");
        let (_, inspect) = run_script(format!("docker inspect -f '{{{{.HostConfig.RestartPolicy.Name}}}} {{{{index .Config.Labels \"muxen.runner\"}}}}' muxen-{name}; docker inspect muxen-{name} | grep -c FAKE0TOKEN")).await;
        assert_eq!(inspect, vec![format!("unless-stopped {name}"), "0".to_string()], "{inspect:?}");

        // 3. Machine checks find it as a container runner.
        engine.run_check(Some(vec!["box".into()])).await;
        let snap = engine.current();
        let r = snap.runners.iter().find(|r| r.id == id).unwrap_or_else(|| panic!("{id} not found in {:?}", snap.runners.iter().map(|r| &r.id).collect::<Vec<_>>()));
        let local = r.local.clone().unwrap();
        assert_eq!(local.container.as_deref(), Some(format!("muxen-{name}").as_str()));
        assert_eq!(local.container_state.as_deref(), Some("running"));
        assert_eq!(local.url.as_deref(), Some("https://github.com/muxen-test"));
        assert_eq!(r.state, "idle", "{local:?}");

        // 4. Stop / start / restart / logs through the engine (none of these talk to GitHub).
        let o = wait(&engine, engine.runner_action(&id, RunnerAction::Stop).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert_eq!(state(&engine, &id), "offline");
        let o = wait(&engine, engine.runner_action(&id, RunnerAction::Start).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert_eq!(state(&engine, &id), "idle");
        let o = wait(&engine, engine.runner_action(&id, RunnerAction::Restart).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert_eq!(state(&engine, &id), "idle");
        let o = wait(&engine, engine.runner_action(&id, RunnerAction::Logs).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert!(o.log.iter().any(|l| l.contains(&format!("docker logs muxen-{name}"))), "{:?}", o.log);

        // 5. Remove without a GitHub token (nothing registered), deleting the volume too.
        let runner = engine.current().runners.iter().find(|r| r.id == id).cloned().unwrap();
        let (spec, probe) = engine.machine("box").unwrap();
        let plan = Plan::OnMachine { spec, probe, dir: local.dir.clone(), container: local.container.clone(), scope: None, delete_files: true };
        let op = engine.begin_op("remove", "test".into(), Some("box".into()), Some(id.clone()));
        let removed = run_action(&engine, op, &runner, RunnerAction::Remove { delete_files: true }, plan).await;
        engine.op_finish(op, Ok(()));
        let log = op_log(&engine, op).await;
        assert!(removed.is_ok(), "{log:?}");
        assert!(log.iter().any(|l| l == &format!("Deleted volume muxen-{name}")), "{log:?}");
        assert!(!engine.current().runners.iter().any(|r| r.id == id));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "slow: runs the demo's simulated delays (~15s)"]
    async fn demo_install_stop_remove() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir()), None), true);
        engine.run_check(None).await;

        let req = InstallRequest {
            machine_id: "ubuntu-nuc".into(),
            scope: "acme".into(),
            names: engine.suggest_runner_names("ubuntu-nuc", 2),
            labels: vec!["gpu".into()],
            group: None,
            as_service: true,
            mode: InstallMode::Auto,
        };
        assert_eq!(req.names, vec!["ubuntu-nuc-01", "ubuntu-nuc-02"]);
        let o = wait(&engine, engine.install_runners(req).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        let snap = engine.current();
        let r = snap.runners.iter().find(|r| r.id == "ubuntu-nuc/ubuntu-nuc-01").expect("installed runner shows up");
        assert!(r.declared && r.labels.contains(&"gpu".to_string()), "{r:?}");

        let o = wait(&engine, engine.runner_action("ubuntu-nuc/ubuntu-nuc-01", RunnerAction::Stop).unwrap()).await;
        assert_eq!(o.status, "succeeded");
        assert_eq!(state(&engine, "ubuntu-nuc/ubuntu-nuc-01"), "offline");

        let o = wait(&engine, engine.runner_action("ubuntu-nuc/ubuntu-nuc-01", RunnerAction::Remove { delete_files: true }).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        assert!(!engine.current().runners.iter().any(|r| r.name == "ubuntu-nuc-01"));

        // The GitHub-only runner can be deregistered; the asleep laptop can't take installs.
        let o = wait(&engine, engine.runner_action("github/linux-spot-01", RunnerAction::Remove { delete_files: false }).unwrap()).await;
        assert_eq!(o.status, "succeeded", "{:?}", o.log);
        let req = InstallRequest {
            machine_id: "laravel-macbook".into(),
            scope: "acme".into(),
            names: vec!["laravel-01".into()],
            labels: vec![],
            group: None,
            as_service: true,
            mode: InstallMode::Auto,
        };
        let o = wait(&engine, engine.install_runners(req).unwrap()).await;
        assert_eq!(o.status, "failed");
        assert!(o.error.unwrap_or_default().contains("timed out"));

        let id = engine.add_machine(NewMachine { name: "Beelink".into(), host: "beelink".into() }).unwrap();
        engine.run_check(Some(vec![id.clone()])).await;
        let m = engine.current().machines.iter().find(|m| m.id == id).cloned().unwrap();
        assert_eq!(m.status, "online");
        assert_eq!(m.platform, "linux-x64");
    }
}
