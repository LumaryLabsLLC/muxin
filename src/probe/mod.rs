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
    /// The network card it's reached by: its address (to wake the machine
    /// with), and its IPv4 address and broadcast. Kept while it's offline.
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub lan_ip: Option<String>,
    #[serde(default)]
    pub lan_broadcast: Option<String>,
    /// Linux: updates want a reboot (/var/run/reboot-required).
    #[serde(default)]
    pub reboot_required: bool,
    /// Linux: systemd services that failed (the first few).
    #[serde(default)]
    pub failed_units: Vec<String>,
    /// What it runs in when it's virtual ("kvm", "qemu", "vmware", "docker",
    /// "apple", ...): a VM or a container. None: real hardware, or not said.
    #[serde(default)]
    pub virt: Option<String>,
    /// Its Docker containers that publish ports, running or not.
    #[serde(default)]
    pub published: Vec<PublishedContainer>,
    /// Where ssh really goes for it, from ~/.ssh/config.
    #[serde(default)]
    pub route: Option<SshRoute>,
    /// Linux: it has /dev/kvm, so it can run VMs (Windows in Docker, say).
    #[serde(default)]
    pub kvm: bool,
    /// The account the machine turned away before asking for its password,
    /// after the key wasn't accepted for one ssh guessed: there's no such
    /// account there, or ssh there doesn't let it in.
    #[serde(default)]
    pub refused_user: Option<String>,
}

/// A Docker container that publishes ports on its machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PublishedContainer {
    pub name: String,
    /// Docker's state: "running" | "exited" | "created" | "paused" | ...
    pub state: String,
    pub image: String,
    pub ports: Vec<PublishedPort>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PublishedPort {
    /// The machine's address it listens on ("" or "0.0.0.0": all of them).
    pub host_ip: String,
    pub host_port: u16,
    /// Inside the container: "22/tcp".
    pub target: String,
}

/// Where ssh goes for a machine: through a jump host (`ProxyJump`, its last
/// hop), and the address and port at the end. And as whom: the account it
/// signs in as there (its config's User, else this computer's login name).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SshRoute {
    pub jump: Option<String>,
    pub hostname: String,
    pub port: u16,
    pub user: Option<String>,
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
            // How to wake it, from when it was on.
            self.mac = self.mac.take().or_else(|| p.mac.clone());
            self.lan_ip = self.lan_ip.take().or_else(|| p.lan_ip.clone());
            self.lan_broadcast = self.lan_broadcast.take().or_else(|| p.lan_broadcast.clone());
            self.virt = self.virt.take().or_else(|| p.virt.clone());
            // Its containers as last seen, to explain the machines in them.
            if self.published.is_empty() {
                self.published = p.published.clone();
            }
            self.route = self.route.take().or_else(|| p.route.clone());
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

/// "name|state|image|127.0.0.1:2222->22/tcp :::8080->80/tcp ..." from the probe.
fn parse_published(value: &str) -> Option<PublishedContainer> {
    let mut parts = value.splitn(4, '|');
    let (name, state, image, ports) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let ports: Vec<PublishedPort> = ports
        .split_whitespace()
        .filter_map(|p| {
            let (host, target) = p.split_once("->")?;
            let (ip, port) = host.rsplit_once(':')?;
            Some(PublishedPort { host_ip: ip.to_string(), host_port: port.parse().ok()?, target: target.to_string() })
        })
        .collect();
    (!name.is_empty() && !ports.is_empty()).then(|| PublishedContainer { name: name.into(), state: state.into(), image: image.into(), ports })
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
    let mut r = check(spec, remembered).await;
    // ssh's route and sign-in only matter where ssh runs (not an iPad).
    if spec.is_checkable() && !spec.is_local() && !cfg!(target_os = "ios") {
        r.route = ssh_route(spec).await;
        r.refused_user = refused_user(spec, &r).await;
    }
    r
}

/// The account a machine turns away, when its key wasn't accepted for one
/// ssh guessed (the host names none). Asked once, after that failure, and
/// only of a machine that offers keyboard-interactive sign-in: see `turns_away`.
async fn refused_user(spec: &MachineSpec, r: &ProbeResult) -> Option<String> {
    let error = r.error.as_deref()?;
    if names_user(spec) || crate::fleet::build::unreachable_kind(error) != "auth" || !error.contains("keyboard-interactive") {
        return None;
    }
    let user = ssh_user(spec, Some(r))?;
    turns_away(spec).await.then_some(user)
}

/// Whether the machine's host or settings say which account to sign in as.
fn names_user(spec: &MachineSpec) -> bool {
    spec.host.contains('@') || spec.user.as_deref().is_some_and(|u| !u.is_empty())
}

/// The account ssh signs in to a machine as: the one its host or settings
/// name, else ssh's config's (from the last check), else the one its error
/// names, else this computer's login name.
pub fn ssh_user(spec: &MachineSpec, p: Option<&ProbeResult>) -> Option<String> {
    let named = spec.host.trim().rsplit_once('@').map(|(u, _)| u.to_string()).or_else(|| spec.user.clone());
    named
        .or_else(|| p.and_then(|p| p.route.as_ref()?.user.clone()))
        .or_else(|| p.and_then(|p| signed_in_as(p.error.as_deref()?)))
        .or_else(|| std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).ok())
        .filter(|u| !u.is_empty())
}

/// Who ssh signed in as, from its "adib@box.local: Permission denied (...)"
/// (after a known-hosts warning, say: a check's error is one line).
fn signed_in_as(error: &str) -> Option<String> {
    error
        .lines()
        .find_map(|l| l.split_once(": Permission denied"))
        .and_then(|(who, _)| who.split_whitespace().last()?.rsplit_once('@'))
        .map(|(u, _)| u.to_string())
        .filter(|u| !u.is_empty())
}

/// The command that gives a machine this computer's public key (like
/// "~/.ssh/id_ed25519.pub"; ssh-copy-id's own pick without one), signed in as
/// `user` when the host names no one: ssh-copy-id alone guesses this
/// computer's login name, which may not be an account there.
pub fn copy_id_command(spec: &MachineSpec, user: Option<&str>, key: Option<&str>) -> String {
    let host = spec.host.trim();
    let key = key.map(|k| format!(" -i {k}")).unwrap_or_default();
    let port = spec.port.map(|p| format!(" -p {p}")).unwrap_or_default();
    let target = match user.filter(|u| !u.is_empty()) {
        Some(u) if !host.contains('@') => format!("{u}@{host}"),
        _ => host.to_string(),
    };
    format!("ssh-copy-id{key}{port} {target}")
}

/// Whether the machine closes the connection for ssh's account before it
/// asks for a password: it has no such account, or doesn't let it in (a Mac's
/// Remote Login can allow only some users). A password sign-in can't tell:
/// servers ask for one even for accounts they don't have. Keyboard-interactive
/// on a Mac says no first. ssh is stopped at the prompt when there is one, so
/// nothing is ever typed or sent.
#[cfg(unix)]
async fn turns_away(spec: &MachineSpec) -> bool {
    let Some(askpass) = askpass_stop() else { return false };
    let mut cmd = Command::new("ssh");
    // ssh keeps the first value it's given for an option: these go before
    // ssh_args' BatchMode=yes, which turns keyboard-interactive off.
    cmd.args([
        "-o", "BatchMode=no",
        "-o", "PreferredAuthentications=keyboard-interactive",
        "-o", "KbdInteractiveAuthentication=yes",
        "-o", "PubkeyAuthentication=no",
        "-o", "NumberOfPasswordPrompts=1",
        "-o", "ControlPath=none",
    ]);
    cmd.args(ssh_args(spec)).arg("true");
    cmd.env("SSH_ASKPASS", askpass).env("SSH_ASKPASS_REQUIRE", "force");
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    // Without the terminal Muxin may have been started from: an older ssh
    // would ask there instead of through SSH_ASKPASS.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match tokio::time::timeout(Duration::from_secs(15), cmd.output()).await {
        Ok(Ok(out)) => closed_before_asking(&String::from_utf8_lossy(&out.stderr)),
        _ => false,
    }
}

#[cfg(not(unix))]
async fn turns_away(_spec: &MachineSpec) -> bool {
    false
}

/// What `turns_away` gives ssh as SSH_ASKPASS: it says ssh asked (ASKED, on
/// ssh's stderr) and ends ssh, its parent, before an answer goes out. An
/// askpass that only fails would have ssh send an empty one.
#[cfg(unix)]
const ASKPASS_STOP: &str = "#!/bin/sh\necho \"muxin: asked: $1\" >&2\nkill -TERM \"$PPID\"\nexit 1\n";
#[cfg_attr(not(unix), allow(dead_code))]
const ASKED: &str = "muxin: asked:";

/// ASKPASS_STOP as a file to run, in this user's cache folder.
#[cfg(unix)]
fn askpass_stop() -> Option<std::path::PathBuf> {
    static PATH: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let dir = dirs::cache_dir()?.join("muxen");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("askpass-stop");
        let tmp = dir.join(format!("askpass-stop.{}", std::process::id()));
        std::fs::write(&tmp, ASKPASS_STOP).ok()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
        Some(path)
    })
    .clone()
}

/// From ssh's stderr in `turns_away`: the machine closed the connection
/// ("Connection closed by fe80::1%en0 port 22"), and nothing asked for a
/// password first.
#[cfg_attr(not(unix), allow(dead_code))]
fn closed_before_asking(stderr: &str) -> bool {
    !stderr.contains(ASKED) && stderr.lines().any(|l| l.trim().starts_with("Connection closed by ") && l.contains(" port "))
}

/// Where ssh goes for the machine (its config resolved by `ssh -G`).
pub async fn ssh_route(spec: &MachineSpec) -> Option<SshRoute> {
    let out = tokio::time::timeout(Duration::from_secs(3), Command::new("ssh").arg("-G").args(ssh_args(spec)).output()).await.ok()?.ok()?;
    parse_route(&String::from_utf8_lossy(&out.stdout))
}

/// `ssh -G` output to a route: its HostName, Port and the last ProxyJump hop
/// (as a bare host: no user, no port).
pub fn parse_route(text: &str) -> Option<SshRoute> {
    let get = |key: &str| text.lines().find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()));
    let hostname = get("hostname ").filter(|h| !h.is_empty())?;
    let port = get("port ").and_then(|p| p.parse().ok()).unwrap_or(22);
    let user = get("user ").filter(|u| !u.is_empty());
    let jump = get("proxyjump ").filter(|j| !j.is_empty() && j != "none").and_then(|j| {
        let hop = j.rsplit(',').next()?.trim().trim_start_matches("ssh://");
        let hop = hop.rsplit_once('@').map(|(_, h)| h).unwrap_or(hop);
        let host = match hop.strip_prefix('[') {
            Some(rest) => rest.split(']').next().unwrap_or(rest),
            None => hop.split(':').next().unwrap_or(hop),
        };
        (!host.is_empty()).then(|| host.to_string())
    });
    Some(SshRoute { jump, hostname, port, user })
}

async fn check(spec: &MachineSpec, remembered: Option<Shell>) -> ProbeResult {
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
        // A Windows host without a POSIX shell: try PowerShell once, unless the
        // user pinned the shell. Its shell's complaint about `sh` is in the
        // machine's language, so this goes by ssh getting through; when
        // PowerShell doesn't answer either, the first error stands.
        Err(e @ RunError::Output { code, .. }) if shell == Shell::Sh && spec.shell.is_none() && !spec.is_local() && ssh_got_through(code) => {
            match run(spec, Shell::Powershell).await {
                Ok(out) => Ok((out, Shell::Powershell)),
                Err(_) => Err(e),
            }
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
    // doesn't make an ssh link (it's also how Muxin first reaches a machine),
    // only a direct one to a paired machine.
    if shell == Shell::Sh && !spec.is_local() {
        if let Some(link) = crate::fleet::link::current_or_direct(spec).await {
            let req = serde_json::json!({ "op": "sh", "script": script, "login": false, "timeout": TIMEOUT.as_secs() });
            if let Ok(reply) = link.request(req, TIMEOUT).await {
                let stdout = reply["stdout"].as_str().unwrap_or("").to_string();
                if stdout.lines().any(|l| l.starts_with("probe=")) {
                    return Ok(stdout);
                }
            }
        }
    }
    let mut child = if spec.is_local() {
        let c = match shell {
            Shell::Sh => {
                let mut c = Command::new("/bin/sh");
                c.arg("-s");
                c
            }
            Shell::Powershell => {
                let mut c = Command::new("powershell");
                c.args(crate::ops::remote::ps_stdin_args());
                c
            }
        };
        crate::fleet::child::Child::local(c).map_err(|e| RunError::Spawn(format!("Couldn't start the check: {e}")))?
    } else {
        let shell = match shell {
            Shell::Sh => "sh -s".to_string(),
            Shell::Powershell => format!("powershell {}", crate::ops::remote::ps_stdin_args().join(" ")),
        };
        match crate::fleet::child::ssh(spec, &shell).await {
            Ok(c) => c,
            // Muxin's own client: it didn't get in (what ssh says on stderr, exiting 255).
            Err(e) if cfg!(target_os = "ios") => return Err(RunError::Output { code: Some(255), stderr: e }),
            Err(e) => return Err(RunError::Spawn(e)),
        }
    };
    let mut stdin = child.take_stdin().expect("piped stdin");
    let output = async move {
        // The script is small; ignore write errors (e.g. ssh exiting early) and read why from stderr.
        let script = if shell == Shell::Powershell { crate::ops::remote::ps_sendable(&script) } else { script };
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
            let dirs: Vec<String> = spec.runner_dirs.iter().map(|d| crate::ops::remote::ps_quote(d)).collect();
            format!("$OR_DIRS = @({})\n{PROBE_PS1}", dirs.join(", "))
        }
    }
}

/// Whether ssh reached the machine and its shell ran the command (ssh exits
/// 255 for its own failures: refused, keys, host keys, ...).
fn ssh_got_through(code: Option<i32>) -> bool {
    code.is_some_and(|c| c != 255)
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
            "mac" => r.mac = text().filter(|m| m.len() == 17 && m != "00:00:00:00:00:00"),
            "lan" => {
                let mut parts = value.split_whitespace();
                r.lan_ip = parts.next().map(|a| a.split('/').next().unwrap_or(a).to_string());
                r.lan_broadcast = parts.next().map(str::to_string);
            }
            "reboot_required" => r.reboot_required = value == "yes",
            "failed_units" => r.failed_units = value.split_whitespace().map(str::to_string).collect(),
            "virt" => r.virt = text().filter(|v| v != "none"),
            "kvm" => r.kvm = value == "yes",
            "published" => r.published.extend(parse_published(value)),
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
        // The runner's own config wins; then Muxin's label; then the container name.
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
    fn parses_virtual_machines_and_published_ports() {
        let r = parse("probe=1\nvirt=qemu\npublished=zn-win11|exited|dockurr/windows|127.0.0.1:2222->22/tcp :::8006->8006/tcp 100.1.2.3:47822->47821/udp \npublished=bare|running|alpine|\n");
        assert_eq!(r.virt.as_deref(), Some("qemu"));
        assert_eq!(r.published.len(), 1, "a container without ports isn't listed");
        let c = &r.published[0];
        assert_eq!((c.name.as_str(), c.state.as_str()), ("zn-win11", "exited"));
        assert_eq!(c.ports[0], PublishedPort { host_ip: "127.0.0.1".into(), host_port: 2222, target: "22/tcp".into() });
        assert_eq!((c.ports[1].host_ip.as_str(), c.ports[1].host_port), ("::", 8006));
        assert_eq!(c.ports.len(), 3);
    }

    #[test]
    fn reads_where_ssh_goes() {
        let r = parse_route("user adib\nhostname 127.0.0.1\nport 2222\nproxyjump beelink\n").unwrap();
        assert_eq!(r, SshRoute { jump: Some("beelink".into()), hostname: "127.0.0.1".into(), port: 2222, user: Some("adib".into()) });
        let r = parse_route("hostname 10.0.0.5\nport 22\nproxyjump a@bastion:2200,ssh://me@[fd00::1]:22\n").unwrap();
        assert_eq!(r.jump.as_deref(), Some("fd00::1"));
        let r = parse_route("hostname adib-ser\nport 22\n").unwrap();
        assert_eq!(r.jump, None);
    }

    #[test]
    fn knows_which_account_ssh_signs_in_as() {
        let inv = crate::config::Inventory::parse("[[machine]]\nname = \"A\"\nhost = \"ahanna@a.local\"\n\n[[machine]]\nname = \"B\"\nhost = \"b.local\"\nuser = \"bee\"\n\n[[machine]]\nname = \"C\"\nhost = \"c.local\"\n").unwrap();
        let (a, b, c) = (&inv.machines[0], &inv.machines[1], &inv.machines[2]);
        let route = |user: &str| ProbeResult { route: Some(SshRoute { hostname: "c.local".into(), port: 22, user: Some(user.into()), ..Default::default() }), ..Default::default() };
        assert_eq!(ssh_user(a, Some(&route("adib"))).as_deref(), Some("ahanna"));
        assert_eq!(ssh_user(b, Some(&route("adib"))).as_deref(), Some("bee"));
        assert_eq!(ssh_user(c, Some(&route("adib"))).as_deref(), Some("adib"));
        let denied = ProbeResult { error: Some("cee@c.local: Permission denied (publickey).".into()), ..Default::default() };
        assert_eq!(ssh_user(c, Some(&denied)).as_deref(), Some("cee"));
        // A check's error is ssh's last lines, joined.
        let error = "Warning: Permanently added 'c.local' (ED25519) to the list of known hosts. cee@c.local: Permission denied (publickey,password,keyboard-interactive).";
        assert_eq!(signed_in_as(error).as_deref(), Some("cee"));
        assert_eq!(signed_in_as("Permission denied, please try again."), None);
        assert!(names_user(a) && names_user(b) && !names_user(c));

        assert_eq!(copy_id_command(c, Some("adib"), Some("~/.ssh/id_ed25519.pub")), "ssh-copy-id -i ~/.ssh/id_ed25519.pub adib@c.local");
        assert_eq!(copy_id_command(a, Some("adib"), None), "ssh-copy-id ahanna@a.local");
        assert_eq!(copy_id_command(c, None, None), "ssh-copy-id c.local");
    }

    #[test]
    fn tells_a_turned_away_account_from_a_password_prompt() {
        // macOS: no such account, or Remote Login doesn't allow it.
        assert!(closed_before_asking("Connection closed by fe80::1c2b:3d4e:5f60:7a8b%en0 port 22\n"));
        assert!(closed_before_asking("Connection closed by 192.168.1.20 port 22\r\n"));
        // It asked: the account is there.
        assert!(!closed_before_asking("muxin: asked: (ahanna@mac-webtech-025.local) Password:\n"));
        assert!(!closed_before_asking("muxin: asked: (ahanna@host) Password:\nConnection closed by 10.0.0.2 port 22\n"));
        // It doesn't offer keyboard-interactive, or never got that far: can't tell.
        assert!(!closed_before_asking("adib@box: Permission denied (publickey,password).\n"));
        assert!(!closed_before_asking("kex_exchange_identification: Connection closed by remote host\n"));
        assert!(!closed_before_asking("ssh: connect to host box port 22: Operation timed out\n"));
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
    fn tries_powershell_whenever_ssh_got_through() {
        // cmd.exe's 9009 and PowerShell's 1, whatever language they complain in.
        assert!(ssh_got_through(Some(9009)));
        assert!(ssh_got_through(Some(1)));
        // ssh's own failures, or killed: not a shell's answer.
        assert!(!ssh_got_through(Some(255)));
        assert!(!ssh_got_through(None));
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
