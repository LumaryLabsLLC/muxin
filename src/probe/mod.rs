//! Machine checks. A small read-only script runs on each machine (directly for
//! `host = "local"`, otherwise over the system `ssh`, so ~/.ssh/config, keys and
//! agents all just work) and reports resource usage plus installed runners.

use std::process::Stdio;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::MachineSpec;

const PROBE_SH: &str = include_str!("scripts/probe.sh");
const PROBE_PS1: &str = include_str!("scripts/probe.ps1");
const TIMEOUT: Duration = Duration::from_secs(45);
/// `-Command -` runs stdin line by line and breaks multi-line blocks; read it whole instead.
const PS_READ_STDIN: &str = "[Console]::In.ReadToEnd() | Invoke-Expression";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Sh,
    Powershell,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProbeResult {
    pub checked_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub shell: Option<Shell>,
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub os_version: Option<String>,
    /// "laptop" | "desktop", when the machine says.
    #[serde(default)]
    pub form_factor: Option<String>,
    /// Whether it has a graphical session to show (Linux: an X server). None: not said.
    #[serde(default)]
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
    pub runners: Vec<ProbedRunner>,
    /// Docker server version when the machine's user can talk to the daemon.
    pub docker: Option<String>,
    /// Why Docker is installed but unusable (e.g. "permission denied").
    pub docker_error: Option<String>,
    /// Linux only: "root" | "passwordless" | "password" — whether services can be installed.
    pub sudo: Option<String>,
    /// The last time a check reached the machine (this one, when it did).
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl ProbeResult {
    /// A failed check keeps what describes the machine itself (its shell, OS,
    /// name) and when it was last reached, from the check before.
    pub fn after(mut self, previous: Option<&ProbeResult>) -> ProbeResult {
        if self.ok {
            self.last_seen_at = Some(self.checked_at);
            return self;
        }
        if let Some(p) = previous {
            self.last_seen_at = if p.ok { Some(p.checked_at) } else { p.last_seen_at };
            self.shell = self.shell.or(p.shell);
            self.hostname = self.hostname.take().or_else(|| p.hostname.clone());
            self.os = self.os.take().or_else(|| p.os.clone());
            self.arch = self.arch.take().or_else(|| p.arch.clone());
            self.os_version = self.os_version.take().or_else(|| p.os_version.clone());
            self.form_factor = self.form_factor.take().or_else(|| p.form_factor.clone());
            if !self.ok {
                self.desktop = self.desktop.or(p.desktop);
            }
        }
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProbedRunner {
    pub dir: String,
    /// agentName from .runner; falls back to the folder name.
    pub name: String,
    /// gitHubUrl from .runner, e.g. https://github.com/acme or https://github.com/acme/api
    pub url: Option<String>,
    pub version: Option<String>,
    pub service: Option<String>,
    pub service_state: Option<String>,
    pub listener_pid: Option<u32>,
    pub worker_pid: Option<u32>,
    /// Set when the runner lives in a Docker container (`dir` is then `docker://<container>`).
    pub container: Option<String>,
    pub image: Option<String>,
    /// Docker's state: "running" | "exited" | "restarting" | "created" | "paused" | "dead"
    pub container_state: Option<String>,
    /// How many times Docker restarted it (a climbing count means it's crash-looping).
    pub restarts: Option<u32>,
}

/// The `dir` of a runner that lives in a container.
pub fn container_dir(container: &str) -> String {
    format!("docker://{container}")
}

impl ProbeResult {
    pub fn failed(started: Instant, error: impl Into<String>) -> ProbeResult {
        ProbeResult {
            checked_at: Utc::now(),
            duration_ms: started.elapsed().as_millis() as u64,
            ok: false,
            error: Some(error.into()),
            ..Default::default()
        }
    }
}

/// Checks one machine. Never fails: problems are reported in `ProbeResult::error`.
pub async fn probe(spec: &MachineSpec, remembered: Option<Shell>) -> ProbeResult {
    let started = Instant::now();
    if !spec.is_checkable() {
        return ProbeResult::failed(started, "No host configured");
    }
    let shell = spec.shell.or(remembered).unwrap_or(if spec.is_local() && cfg!(windows) {
        Shell::Powershell
    } else {
        Shell::Sh
    });

    let first = run(spec, shell).await;
    let result = match first {
        // A Windows host without a POSIX shell: try PowerShell once, unless the user pinned the shell.
        Err(RunError::Output { ref stderr, .. })
            if shell == Shell::Sh && spec.shell.is_none() && !spec.is_local() && looks_like_no_sh(stderr) =>
        {
            run(spec, Shell::Powershell).await.map(|out| (out, Shell::Powershell))
        }
        other => other.map(|out| (out, shell)),
    };

    // The usual host didn't answer from this network, but the machine is on
    // the tailnet: try it there, and keep using that while it works.
    let result = match result {
        Err(e) if !spec.is_local() && matches!(crate::fleet::build::unreachable_kind(&e.to_string()), "offline" | "unknown-host") => {
            match crate::fleet::tailscale::ssh_fallback(&spec.id) {
                Some(address) => {
                    crate::fleet::tailscale::use_ssh_fallback(&spec.id, true);
                    match run(spec, shell).await {
                        Ok(out) => {
                            tracing::info!("{} answers over Tailscale ({address}), not at {}", spec.name, spec.host.trim());
                            Ok((out, shell))
                        }
                        Err(_) => {
                            crate::fleet::tailscale::use_ssh_fallback(&spec.id, false);
                            Err(e)
                        }
                    }
                }
                None => Err(e),
            }
        }
        other => other,
    };

    match result {
        Ok((stdout, shell)) => {
            let mut r = parse(&stdout);
            r.shell = Some(shell);
            r.checked_at = Utc::now();
            r.duration_ms = started.elapsed().as_millis() as u64;
            if !r.ok {
                r.error = Some("The machine answered, but the check printed nothing usable.".into());
            }
            r
        }
        Err(e) => ProbeResult::failed(started, e.to_string()),
    }
}

#[derive(Debug)]
enum RunError {
    Spawn(String),
    Timeout,
    Output { code: Option<i32>, stderr: String },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(e) => write!(f, "{e}"),
            RunError::Timeout => write!(f, "Timed out after {}s", TIMEOUT.as_secs()),
            RunError::Output { code, stderr } => {
                let msg = last_lines(stderr, 3);
                match (msg.is_empty(), code) {
                    (false, _) => write!(f, "{msg}"),
                    (true, Some(c)) => write!(f, "Exited with status {c}"),
                    (true, None) => write!(f, "Terminated by a signal"),
                }
            }
        }
    }
}

async fn run(spec: &MachineSpec, shell: Shell) -> Result<String, RunError> {
    let script = script_for(spec, shell);
    // Over the machine's live link when one is up (no ssh handshake). A check
    // doesn't make one: it's also how Muxen first reaches a machine.
    if shell == Shell::Sh && !spec.is_local() {
        if let Some(link) = crate::fleet::link::current(&spec.id) {
            let req = serde_json::json!({ "op": "sh", "script": script, "login": false, "timeout": TIMEOUT.as_secs() });
            if let Ok(reply) = link.request(req, TIMEOUT).await {
                let stdout = reply["stdout"].as_str().unwrap_or("").to_string();
                if stdout.lines().any(|l| l.starts_with("probe=")) {
                    return Ok(stdout);
                }
            }
        }
    }
    let mut cmd = if spec.is_local() {
        match shell {
            Shell::Sh => {
                let mut c = Command::new("/bin/sh");
                c.arg("-s");
                c
            }
            Shell::Powershell => {
                let mut c = Command::new("powershell");
                c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", PS_READ_STDIN]);
                c
            }
        }
    } else {
        let mut c = Command::new("ssh");
        c.args(ssh_args(spec));
        c.arg(match shell {
            Shell::Sh => "sh -s".to_string(),
            Shell::Powershell => {
                format!("powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"{PS_READ_STDIN}\"")
            }
        });
        c
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        RunError::Spawn(if spec.is_local() { format!("Couldn't start the check: {e}") } else { format!("Couldn't run ssh: {e}") })
    })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let output = async move {
        // The script is small; ignore write errors (e.g. ssh exiting early) and read why from stderr.
        let _ = stdin.write_all(script.as_bytes()).await;
        drop(stdin);
        child.wait_with_output().await
    };
    let output = tokio::time::timeout(TIMEOUT, output)
        .await
        .map_err(|_| RunError::Timeout)?
        .map_err(|e| RunError::Spawn(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    // Scripts end with key=value output even if one command inside failed; only treat it as a
    // failure when nothing came back.
    if !output.status.success() && !stdout.lines().any(|l| l.starts_with("probe=")) {
        return Err(RunError::Output {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(stdout)
}

/// The public key ssh offers by default (the first of its default identities
/// that exists here), as "~/.ssh/id_ed25519.pub".
pub fn default_ssh_key() -> Option<String> {
    let ssh = dirs::home_dir()?.join(".ssh");
    ["id_rsa", "id_ecdsa", "id_ecdsa_sk", "id_ed25519", "id_ed25519_sk"]
        .iter()
        .find(|k| ssh.join(format!("{k}.pub")).is_file() && ssh.join(k).is_file())
        .map(|k| format!("~/.ssh/{k}.pub"))
}

/// The public key ssh offers this machine (its config's IdentityFile, else a
/// default one), as a path like "~/.ssh/id_ed25519.pub".
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub async fn ssh_key_for(spec: &MachineSpec) -> Option<String> {
    let home = dirs::home_dir()?;
    let out = tokio::process::Command::new("ssh").arg("-G").args(ssh_args(spec)).output().await.ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("identityfile ").map(|p| p.trim().to_string()))
        .find(|p| {
            let full = p.strip_prefix("~/").map(|rest| home.join(rest)).unwrap_or_else(|| p.into());
            full.is_file() && std::path::PathBuf::from(format!("{}.pub", full.display())).is_file()
        })
        .map(|p| format!("{p}.pub"))
}

pub fn ssh_args(spec: &MachineSpec) -> Vec<String> {
    let mut args: Vec<String> = [
        "-T",
        "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=8",
        "-o", "ServerAliveInterval=5",
        "-o", "ServerAliveCountMax=3",
        "-o", "StrictHostKeyChecking=accept-new",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(port) = spec.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    if let Some(user) = spec.user.as_deref().filter(|u| !u.is_empty()) {
        args.extend(["-l".into(), user.to_string()]);
    }
    args.extend(spec.ssh_options.iter().cloned());
    // Away from home, through Tailscale, when the usual host only works there
    // (ssh's config still supplies the user, key and port).
    if let Some(address) = crate::fleet::tailscale::ssh_override(&spec.id) {
        args.extend(["-o".into(), format!("HostName={address}")]);
    }
    args.push(spec.host.trim().to_string());
    args
}

fn script_for(spec: &MachineSpec, shell: Shell) -> String {
    match shell {
        Shell::Sh => {
            let dirs = spec.runner_dirs.join("\n").replace('\'', r"'\''");
            format!("OR_DIRS='{dirs}'\n{PROBE_SH}")
        }
        Shell::Powershell => {
            let dirs: Vec<String> = spec.runner_dirs.iter().map(|d| format!("'{}'", d.replace('\'', "''"))).collect();
            format!("$OR_DIRS = @({})\n{PROBE_PS1}", dirs.join(", "))
        }
    }
}

fn looks_like_no_sh(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("is not recognized") || s.contains("'sh' is not") || s.contains("the term 'sh'")
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join(" ")
}

/// Parses the probe's key=value output. Unknown keys and noise (e.g. a chatty
/// shell profile) are ignored.
pub fn parse(out: &str) -> ProbeResult {
    let mut r = ProbeResult::default();
    let mut runners: Vec<(usize, ProbedRunner)> = Vec::new();
    let mut containers: Vec<(usize, ContainerLines)> = Vec::new();

    for line in out.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        let text = || (!value.is_empty()).then(|| value.to_string());
        match key.trim() {
            "probe" => r.ok = true,
            "os" => r.os = text(),
            "arch" => r.arch = text(),
            "hostname" => r.hostname = text(),
            "os_version" => r.os_version = text().filter(|v| v != "macOS"),
            "form_factor" => r.form_factor = text(),
            "desktop" => r.desktop = Some(value == "yes"),
            "cores" => r.cores = value.parse().ok(),
            "cpu_pct" => r.cpu_pct = value.parse::<f64>().ok().map(|v| v.clamp(0.0, 100.0)),
            "mem_total" => r.mem_total = value.parse().ok().filter(|v| *v > 0),
            "mem_used" => r.mem_used = value.parse().ok(),
            "disk_total" => r.disk_total = value.parse().ok().filter(|v| *v > 0),
            "disk_free" => r.disk_free = value.parse().ok(),
            "disk_path" => r.disk_path = text(),
            "uptime_s" => r.uptime_sec = value.parse().ok(),
            "load" => r.load = value.split_whitespace().filter_map(|v| v.parse().ok()).take(3).collect(),
            "docker" => r.docker = text(),
            "docker_error" => r.docker_error = text(),
            "sudo" => r.sudo = text(),
            k if k.starts_with('c') && k.contains('.') => {
                let (idx, field) = k[1..].split_once('.').unwrap();
                let Ok(idx) = idx.parse::<usize>() else { continue };
                let pos = match containers.iter().position(|(i, _)| *i == idx) {
                    Some(p) => p,
                    None => {
                        containers.push((idx, ContainerLines::default()));
                        containers.len() - 1
                    }
                };
                containers[pos].1.set(field, value);
            }
            k if k.starts_with('r') && k.contains('.') => {
                let (idx, field) = k[1..].split_once('.').unwrap();
                let Ok(idx) = idx.parse::<usize>() else { continue };
                let pos = match runners.iter().position(|(i, _)| *i == idx) {
                    Some(p) => p,
                    None => {
                        runners.push((idx, ProbedRunner::default()));
                        runners.len() - 1
                    }
                };
                let runner = &mut runners[pos].1;
                match field {
                    "dir" => runner.dir = value.to_string(),
                    "config" => {
                        if let Some((name, url)) = parse_runner_config(value) {
                            runner.name = name;
                            runner.url = url;
                        }
                    }
                    "listener" => runner.listener_pid = value.parse().ok(),
                    "worker" => runner.worker_pid = value.parse().ok(),
                    "version" => runner.version = text().map(|v| v.trim_start_matches('v').to_string()),
                    "service" => runner.service = text(),
                    "service_state" => runner.service_state = text(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if let (Some(total), Some(used)) = (r.mem_total, r.mem_used) {
        r.mem_used = Some(used.min(total));
    }
    r.runners = runners
        .into_iter()
        .map(|(_, mut runner)| {
            if runner.name.is_empty() {
                runner.name = runner.dir.rsplit(['/', '\\']).next().unwrap_or_default().to_string();
            }
            runner
        })
        .filter(|runner| !runner.dir.is_empty())
        .collect();
    r.runners.extend(containers.into_iter().filter_map(|(_, c)| c.into_runner()));
    r
}

/// One `c<N>.<field>=` group from the probe: a runner container.
#[derive(Default)]
struct ContainerLines {
    container: String,
    state: String,
    image: String,
    label_name: String,
    label_scope: String,
    restarts: Option<u32>,
    listener: Option<u32>,
    worker: Option<u32>,
    config: Option<(String, Option<String>)>,
    version: Option<String>,
}

impl ContainerLines {
    fn set(&mut self, field: &str, value: &str) {
        match field {
            "container" => self.container = value.to_string(),
            "state" => self.state = value.to_string(),
            "image" => self.image = value.to_string(),
            "name" => self.label_name = value.to_string(),
            "scope" => self.label_scope = value.to_string(),
            "restarts" => self.restarts = value.parse().ok(),
            "listener" => self.listener = value.parse().ok(),
            "worker" => self.worker = value.parse().ok(),
            "config" => self.config = parse_runner_config(value),
            "version" => self.version = (!value.is_empty()).then(|| value.trim_start_matches('v').to_string()),
            _ => {}
        }
    }

    fn into_runner(self) -> Option<ProbedRunner> {
        if self.container.is_empty() {
            return None;
        }
        // The runner's own config wins; then Muxen's label; then the container name.
        let (config_name, config_url) = self.config.unwrap_or_default();
        let name = [config_name, self.label_name, self.container.clone()].into_iter().find(|n| !n.is_empty())?;
        let url = config_url.or_else(|| (!self.label_scope.is_empty()).then(|| format!("https://github.com/{}", self.label_scope)));
        // An image tagged with the runner version tells us the version of a stopped container.
        let tag_version = self.image.rsplit_once(':').map(|(_, t)| t).filter(|t| t.split('.').count() == 3 && t.chars().all(|c| c.is_ascii_digit() || c == '.'));
        let running = self.state == "running";
        Some(ProbedRunner {
            dir: container_dir(&self.container),
            name,
            url,
            version: self.version.or_else(|| tag_version.map(str::to_string)),
            service: None,
            service_state: None,
            listener_pid: if running { self.listener } else { None },
            worker_pid: if running { self.worker } else { None },
            container: Some(self.container),
            image: (!self.image.is_empty()).then_some(self.image),
            container_state: (!self.state.is_empty()).then_some(self.state),
            restarts: self.restarts,
        })
    }
}

/// Reads agentName and gitHubUrl from a runner's `.runner` JSON (which may start with a BOM).
fn parse_runner_config(raw: &str) -> Option<(String, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok()?;
    let name = v.get("agentName")?.as_str()?.to_string();
    let url = v.get("gitHubUrl").and_then(|u| u.as_str()).map(str::to_string);
    Some((name, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_check_remembers_the_machine() {
        let ok = ProbeResult { ok: true, checked_at: Utc::now(), shell: Some(Shell::Sh), os: Some("linux".into()), ..Default::default() }.after(None);
        assert_eq!(ok.last_seen_at, Some(ok.checked_at));
        let failed = ProbeResult { ok: false, error: Some("Host is down".into()), ..Default::default() }.after(Some(&ok));
        assert_eq!((failed.last_seen_at, failed.shell, failed.os.as_deref()), (Some(ok.checked_at), Some(Shell::Sh), Some("linux")));
        // Still down: still the last time it answered.
        let again = ProbeResult { ok: false, ..Default::default() }.after(Some(&failed));
        assert_eq!(again.last_seen_at, Some(ok.checked_at));
    }

    #[test]
    fn parses_probe_output() {
        let out = "\
Welcome to my shell profile!
probe=1
os=darwin
arch=arm64
hostname=studio
os_version=macOS 15.6
cores=24
load=1.20 1.05 0.98
uptime_s=10022400
mem_total=137438953472
mem_used=40000000000
cpu_pct=43.0
disk_total=994662584320
disk_free=106000000000
disk_path=/System/Volumes/Data
r0.dir=/Users/ci/actions-runner-01
r0.config=\u{feff}{  \"agentId\": 12,  \"agentName\": \"mac-studio-01\",  \"gitHubUrl\": \"https://github.com/acme\",  \"workFolder\": \"_work\"}
r0.listener=4242
r0.worker=
r0.version=2.328.0
r0.service=actions.runner.acme.mac-studio-01
r0.service_state=active
r1.dir=/Users/ci/runners/broken
r1.config=not json
r1.listener=
";
        let r = parse(out);
        assert!(r.ok);
        assert_eq!(r.os.as_deref(), Some("darwin"));
        assert_eq!(r.cores, Some(24));
        assert_eq!(r.load, vec![1.2, 1.05, 0.98]);
        assert_eq!(r.cpu_pct, Some(43.0));
        assert_eq!(r.disk_free, Some(106_000_000_000));
        assert_eq!(r.runners.len(), 2);
        assert_eq!(r.runners[0].name, "mac-studio-01");
        assert_eq!(r.runners[0].url.as_deref(), Some("https://github.com/acme"));
        assert_eq!(r.runners[0].listener_pid, Some(4242));
        assert_eq!(r.runners[0].worker_pid, None);
        assert_eq!(r.runners[0].service_state.as_deref(), Some("active"));
        assert_eq!(r.runners[1].name, "broken");
    }

    #[test]
    fn parses_docker_and_runner_containers() {
        let out = "\
probe=1
os=linux
sudo=password
docker=29.8.1
c0.container=muxen-beelink-01
c0.state=running
c0.image=ghcr.io/actions/actions-runner:2.338.0
c0.name=beelink-01
c0.scope=acme
c0.restarts=0
c0.listener=4242
c0.worker=4300
c0.config={\"agentName\":\"beelink-01\",\"gitHubUrl\":\"https://github.com/acme\"}
c0.version=2.339.1
c1.container=old-runner
c1.state=exited
c1.image=ghcr.io/actions/actions-runner:2.329.0
c1.name=
c1.scope=
c1.restarts=7
";
        let r = parse(out);
        assert_eq!(r.docker.as_deref(), Some("29.8.1"));
        assert_eq!(r.sudo.as_deref(), Some("password"));
        assert_eq!(r.runners.len(), 2);
        let a = &r.runners[0];
        assert_eq!(a.name, "beelink-01");
        assert_eq!(a.dir, "docker://muxen-beelink-01");
        assert_eq!(a.container.as_deref(), Some("muxen-beelink-01"));
        assert_eq!((a.listener_pid, a.worker_pid), (Some(4242), Some(4300)));
        assert_eq!(a.version.as_deref(), Some("2.339.1"), "the running binary wins over the image tag");
        assert_eq!(a.url.as_deref(), Some("https://github.com/acme"));
        // A stopped container from someone else: named after the container, version from the tag.
        let b = &r.runners[1];
        assert_eq!(b.name, "old-runner");
        assert_eq!(b.container_state.as_deref(), Some("exited"));
        assert_eq!(b.listener_pid, None);
        assert_eq!(b.version.as_deref(), Some("2.329.0"));
        assert_eq!(b.restarts, Some(7));
        assert_eq!(b.url, None);
    }

    #[test]
    fn empty_output_is_not_ok() {
        assert!(!parse("").ok);
    }

    #[test]
    fn builds_ssh_args() {
        let spec: MachineSpec = toml::from_str(
            "name = \"x\"\nhost = \"ci@studio\"\nport = 2222\nssh_options = [\"-i\", \"~/.ssh/ci\"]",
        )
        .unwrap();
        let args = ssh_args(&spec);
        assert_eq!(args.last().unwrap(), "ci@studio");
        assert!(args.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(args.windows(2).any(|w| w == ["-i", "~/.ssh/ci"]));
    }
}
