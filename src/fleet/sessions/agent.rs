//! Setting up and talking to `muxen-agent` (the `agent/` crate) on a
//! machine. Muxen ships a build per OS and CPU; when it connects to a
//! machine it makes sure `~/.muxen/bin/muxen-agent` is that exact
//! build (uploading it over ssh when it isn't), then runs `muxen-agent
//! ctl` for every session request.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::super::workspaces::transport::{self, Limits};
use crate::config::MachineSpec;
use crate::ops::remote::sh_quote;
use crate::probe::{self, ProbeResult};

/// Where the agent lives on every machine (and how attach commands name it).
pub const REMOTE_PATH: &str = "~/.muxen/bin/muxen-agent";
const CTL_TIMEOUT: Duration = Duration::from_secs(25);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);

/// The agent build name for a machine's OS and CPU ("linux-x86_64").
pub fn target(probe: Option<&ProbeResult>) -> Option<String> {
    let p = probe?;
    let os = match p.os.as_deref()? {
        "darwin" | "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match p.arch.as_deref()?.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        _ => return None,
    };
    Some(format!("{os}-{arch}"))
}

/// The folder of agent builds: in the app bundle, next to the binary, or
/// `dist/agents` in a source checkout (`scripts/build-agents.sh`).
pub fn builds_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MUXEN_AGENTS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?;
    for candidate in [dir.join("../Resources/agents"), dir.join("agents")] {
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    dir.ancestors().map(|a| a.join("dist/agents")).find(|c| c.is_dir())
}

/// The build for `target` and its hash (what `muxen-agent hash` prints).
fn build(target: &str) -> Result<(Vec<u8>, String), String> {
    let dir = builds_dir().ok_or("Muxen can't find its agent builds (run scripts/build-agents.sh in a source checkout).")?;
    let path = dir.join(format!("muxen-agent-{target}"));
    let bytes = std::fs::read(&path).map_err(|_| format!("Muxen has no agent build for {target} machines yet."))?;
    let hash = format!("{:016x}", fnv(&bytes));
    Ok((bytes, hash))
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Machines already checked to have the right build, by machine id.
fn verified() -> &'static Mutex<HashMap<String, String>> {
    static VERIFIED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    VERIFIED.get_or_init(Default::default)
}

/// Makes sure the machine runs this Muxen's agent build, installing it if
/// not. Cheap after the first time per machine.
pub async fn ensure(spec: &MachineSpec, probe: Option<&ProbeResult>) -> Result<(), String> {
    let target = target(probe).ok_or_else(|| match probe {
        Some(p) => format!("Muxen has no agent for {}-{} machines yet.", p.os.as_deref().unwrap_or("?"), p.arch.as_deref().unwrap_or("?")),
        None => format!("Muxen hasn't reached {} yet, so it doesn't know which agent build it needs. Refresh it first.", spec.name),
    })?;
    let (bytes, hash) = build(&target)?;
    if verified().lock().unwrap().get(&spec.id) == Some(&hash) {
        return Ok(());
    }
    // One check (and install) at a time per machine.
    let gate = {
        static GATES: OnceLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
        GATES.get_or_init(Default::default).lock().unwrap().entry(spec.id.clone()).or_default().clone()
    };
    let _turn = gate.lock().await;
    if verified().lock().unwrap().get(&spec.id) == Some(&hash) {
        return Ok(());
    }
    let installed = run_sh(spec, &format!("a={REMOTE_PATH}; if [ -x \"$a\" ]; then \"$a\" hash; else echo none; fi"), CTL_TIMEOUT).await?;
    if installed.trim() != hash {
        tracing::info!("installing muxen-agent ({target}) on {}", spec.name);
        let got = upload(spec, bytes).await?;
        if got.trim() != hash {
            return Err(format!("The agent didn't install correctly on {} (got {}).", spec.name, got.trim()));
        }
        // A link still runs the old build; the next use links to the new one,
        // directly again if the machine is paired.
        crate::fleet::link::reset(&spec.id);
        crate::fleet::direct::retry_now(&spec.id);
    }
    verified().lock().unwrap().insert(spec.id.clone(), hash);
    Ok(())
}

/// Forget that a machine was verified (after an error, it's checked again).
pub fn forget(machine_id: &str) {
    verified().lock().unwrap().remove(machine_id);
}

/// Copies the build into place atomically and prints its hash.
async fn upload(spec: &MachineSpec, bytes: Vec<u8>) -> Result<String, String> {
    // The folder from before the rename (OpenRunner) moves over first: running
    // sessions keep their server (its socket moves with the folder). Then the
    // direct-connection service (if set up) moves to the new name or restarts
    // onto the new build. A listener the service doesn't know about (one left
    // from before, holding the port) goes first, or the old build would keep
    // answering.
    let script = r#"set -e; if [ -d "$HOME/.openrunner" ] && [ ! -e "$HOME/.muxen" ]; then mv "$HOME/.openrunner" "$HOME/.muxen"; fi; d="$HOME/.muxen/bin"; mkdir -p "$d"; n="$d/muxen-agent.new.$$"; cat > "$n"; chmod 755 "$n"; mv -f "$n" "$d/muxen-agent"; rm -f "$d/openrunner-agent"; "$d/muxen-agent" migrate >/dev/null 2>&1 || true; rmdir "$HOME/.openrunner" 2>/dev/null || true; if systemctl --user is-enabled muxen-agent.service >/dev/null 2>&1; then pkill -f "$d/muxen-agent listen" >/dev/null 2>&1 || true; systemctl --user restart muxen-agent.service >/dev/null 2>&1 || true; elif launchctl print "gui/$(id -u)/com.lumarylabs.muxen-agent" >/dev/null 2>&1; then pkill -f "$d/muxen-agent listen" >/dev/null 2>&1 || true; launchctl kickstart -k "gui/$(id -u)/com.lumarylabs.muxen-agent" >/dev/null 2>&1 || true; fi; "$d/muxen-agent" hash"#;
    let mut cmd = if spec.is_local() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    } else {
        let mut c = Command::new("ssh");
        c.args(probe::ssh_args(spec));
        c.arg(format!("sh -c {}", sh_quote(script)));
        c
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("couldn't run ssh: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let work = async move {
        stdin.write_all(&bytes).await.map_err(|e| e.to_string())?;
        drop(stdin);
        let (mut out, mut err) = (String::new(), String::new());
        let _ = tokio::join!(stdout.read_to_string(&mut out), stderr.read_to_string(&mut err));
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if status.success() { Ok(out) } else { Err(format!("couldn't install the agent on {}: {}", spec.name, err.trim())) }
    };
    tokio::time::timeout(UPLOAD_TIMEOUT, work).await.map_err(|_| format!("{} took too long to receive the agent.", spec.name))?
}

/// Runs a short sh script on the machine (login shell, like workspace commands).
pub(crate) async fn run_sh(spec: &MachineSpec, script: &str, timeout: Duration) -> Result<String, String> {
    let marker = transport::new_marker();
    let out = transport::run(spec, false, &marker, transport::with_marker(&marker, false, script), timeout, Limits::OUTPUT, &[], None).await?;
    if out.timed_out {
        return Err(format!("{} is unreachable: no answer within {}s.", spec.name, timeout.as_secs()));
    }
    if !out.started {
        return Err(format!("{} is unreachable: {}", spec.name, out.reason()));
    }
    match out.status {
        Some(0) => Ok(out.stdout),
        _ => Err(out.reason()),
    }
}

/// The token of the GitHub CLI's active login on a machine. It travels over the
/// same ssh connection as everything else and is never logged. (Not `--user`:
/// on Linux that goes to the keyring, which can wait forever without a desktop.)
pub async fn gh_token(spec: &MachineSpec, gh: &str, hostname: &str) -> Result<String, String> {
    let script = format!("{} auth token --hostname {}\n", sh_quote(gh), sh_quote(hostname));
    let token = run_sh(spec, &script, CTL_TIMEOUT).await?.trim().to_string();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(format!("The GitHub CLI on {} gave no token. Sign it in again.", spec.name));
    }
    Ok(token)
}

/// One request to the agent; its JSON reply, or the error it gave. Over the
/// machine's live link when there is one, else a one-off ssh call.
pub async fn ctl(spec: &MachineSpec, request: &Value) -> Result<Value, String> {
    let reply = ctl_reply(spec, request).await?;
    if reply["ok"].as_bool() == Some(true) {
        Ok(reply)
    } else {
        Err(reply["error"].as_str().unwrap_or("the agent couldn't do that").to_string())
    }
}

/// Like `ctl`, but the agent's whole reply, refusals included (for the
/// details beside the error). Err only when it couldn't be asked.
pub async fn ctl_reply(spec: &MachineSpec, request: &Value) -> Result<Value, String> {
    if let Some(link) = crate::fleet::link::get(spec).await {
        if let Ok(reply) = link.request(request.clone(), CTL_TIMEOUT).await {
            return Ok(reply);
        }
    }
    let json = request.to_string();
    let script = format!("{REMOTE_PATH} ctl <<'__OR_REQUEST__'\n{json}\n__OR_REQUEST__\n");
    let out = run_sh(spec, &script, CTL_TIMEOUT).await;
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            forget(&spec.id);
            return Err(e);
        }
    };
    let reply: Value = out
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .and_then(|l| serde_json::from_str(l).ok())
        .ok_or_else(|| format!("The agent on {} gave no answer.", spec.name))?;
    Ok(reply)
}
