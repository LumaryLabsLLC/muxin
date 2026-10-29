//! Setting up and talking to `muxen-agent` (the `agent/` crate) on a
//! machine. Muxin ships a build per OS and CPU; when it connects to a
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
/// Windows: `%USERPROFILE%\.muxen\bin\muxen-agent.exe` (see `command`).
pub const REMOTE_PATH: &str = "~/.muxen/bin/muxen-agent";
const CTL_TIMEOUT: Duration = Duration::from_secs(25);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);

/// The agent build name for a machine's OS and CPU ("linux-x86_64").
pub fn target(probe: Option<&ProbeResult>) -> Option<String> {
    let p = probe?;
    let os = match p.os.as_deref()? {
        "darwin" | "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match p.arch.as_deref()?.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        _ => return None,
    };
    Some(format!("{os}-{arch}"))
}

/// This binary, its symlinks resolved.
fn this_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()?.canonicalize().ok()
}

/// The source checkout this binary was built in, when it still runs from
/// there (`target/release/muxin`, debug or release).
fn checkout(exe: &std::path::Path) -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).canonicalize().ok()?;
    exe.starts_with(&root).then_some(root)
}

/// The folder of agent builds: in the app bundle, next to the binary, or
/// `dist/agents` in the source checkout it was built in and runs from
/// (`scripts/build-agents.sh`). Not any folder above the binary: one planted
/// there (`/tmp/dist/agents`) would be uploaded to every machine.
pub fn builds_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MUXEN_AGENTS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let exe = this_exe()?;
    let dir = exe.parent()?;
    for candidate in [dir.join("../Resources/agents"), dir.join("agents")] {
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    let dist = checkout(&exe)?.join("dist/agents");
    dist.is_dir().then_some(dist)
}

/// This computer's agent build name ("darwin-aarch64").
fn host_target() -> String {
    let os = if std::env::consts::OS == "macos" { "darwin" } else { std::env::consts::OS };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// Where the build for `target` is. With no folder of builds, a
/// `muxen-agent` built next to the binary (`cargo build --workspace` in a
/// checkout) serves this computer's own platform.
fn build_path(target: &str) -> Result<PathBuf, String> {
    let exe = if target.starts_with("windows") { ".exe" } else { "" };
    if let Some(dir) = builds_dir() {
        return Ok(dir.join(format!("muxen-agent-{target}{exe}")));
    }
    let this = this_exe();
    if target == host_target()
        && let Some(sibling) = this.as_deref().and_then(|e| e.parent()).map(|d| d.join(format!("muxen-agent{exe}")))
        && sibling.is_file()
    {
        return Ok(sibling);
    }
    Err(match this {
        Some(e) if checkout(&e).is_some() => {
            let cargo = if cfg!(debug_assertions) { "cargo build --workspace" } else { "cargo build --release --workspace" };
            format!("This build of Muxin has no agent builds. In its checkout, run `{cargo}` for this computer, or scripts/build-agents.sh for every platform.")
        }
        Some(e) => format!("Muxin can't find its agent builds next to {}. Reinstall Muxin, or set MUXEN_AGENTS_DIR to a folder of them.", e.display()),
        None => "Muxin can't tell where it's installed, so it can't find its agent builds. Set MUXEN_AGENTS_DIR to a folder of them.".to_string(),
    })
}

/// The build for `target` and its hash (what `muxen-agent hash` prints).
fn build(target: &str) -> Result<(Vec<u8>, String), String> {
    let path = build_path(target)?;
    let bytes = read_build(&path).ok_or_else(|| format!("Muxin has no agent build for {target} machines yet."))?;
    let hash = format!("{:016x}", fnv(&bytes));
    Ok((bytes, hash))
}

/// A build's bytes. In the iPad app they're gzipped (`<name>.gz`), as data
/// rather than programs the App Store would look for a signature on.
fn read_build(path: &std::path::Path) -> Option<Vec<u8>> {
    if let Ok(bytes) = std::fs::read(path) {
        return Some(bytes);
    }
    #[cfg(target_os = "ios")]
    {
        use std::io::Read;
        let packed = std::fs::read(format!("{}.gz", path.display())).ok()?;
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(&packed[..]).read_to_end(&mut bytes).ok()?;
        return Some(bytes);
    }
    #[allow(unreachable_code)]
    None
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Which machines run Windows, by machine id, as their last check said (so
/// links and requests know how to name the agent there).
fn windows_machines() -> &'static Mutex<HashMap<String, bool>> {
    static OS: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    OS.get_or_init(Default::default)
}

/// Remembers a machine's OS from its check.
pub fn note_os(machine_id: &str, os: Option<&str>) {
    if let Some(os) = os {
        windows_machines().lock().unwrap().insert(machine_id.to_string(), os == "windows");
    }
}

/// Whether a machine runs Windows (as far as its last check says).
pub fn is_windows(spec: &MachineSpec) -> bool {
    if spec.is_local() {
        return cfg!(windows);
    }
    windows_machines().lock().unwrap().get(&spec.id).copied().unwrap_or(false)
}

/// The command line ssh hands the machine's shell to run `muxen-agent <args>`.
/// On Windows it's PowerShell with an encoded command, which reads the same
/// whether sshd's shell is cmd.exe or PowerShell.
pub fn command(spec: &MachineSpec, args: &str) -> String {
    if is_windows(spec) {
        use base64::Engine as _;
        let call = format!("& (Join-Path $env:USERPROFILE '.muxen\\bin\\muxen-agent.exe') {args}");
        let utf16: Vec<u8> = call.encode_utf16().flat_map(u16::to_le_bytes).collect();
        format!("powershell -NoProfile -NonInteractive -EncodedCommand {}", base64::engine::general_purpose::STANDARD.encode(utf16))
    } else {
        format!("{REMOTE_PATH} {args}")
    }
}

/// Machines already checked to have the right build, by machine id.
fn verified() -> &'static Mutex<HashMap<String, String>> {
    static VERIFIED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    VERIFIED.get_or_init(Default::default)
}

/// Makes sure the machine runs this Muxin's agent build, installing it if
/// not. Cheap after the first time per machine.
pub async fn ensure(spec: &MachineSpec, probe: Option<&ProbeResult>) -> Result<(), String> {
    spec.refuse_if_disconnected()?;
    // An iPad uses the agent a paired machine runs. One it's adding, not
    // paired yet, gets this build, from the app, over the iPad's own ssh.
    if cfg!(target_os = "ios") && crate::fleet::direct::paired(&spec.id).is_some() {
        return running_agent(spec).await;
    }
    let target = target(probe).ok_or_else(|| match probe {
        Some(p) => format!("Muxin has no agent for {}-{} machines yet.", p.os.as_deref().unwrap_or("?"), p.arch.as_deref().unwrap_or("?")),
        None => format!("Muxin hasn't reached {} yet, so it doesn't know which agent build it needs. Refresh it first.", spec.name),
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
    let windows = target.starts_with("windows");
    note_os(&spec.id, Some(if windows { "windows" } else { "unix" }));
    let installed = if windows {
        run_ps(spec, "$a = Join-Path $env:USERPROFILE '.muxen\\bin\\muxen-agent.exe'; if (Test-Path $a) { & $a hash } else { 'none' }", CTL_TIMEOUT).await?
    } else {
        // One that can't run (another system's build) is replaced like a missing one.
        run_sh(spec, &format!("a={REMOTE_PATH}; if [ -x \"$a\" ]; then \"$a\" hash 2>/dev/null || echo broken; else echo none; fi"), CTL_TIMEOUT).await?
    };
    if installed.trim() != hash {
        tracing::info!("installing muxen-agent ({target}) on {}", spec.name);
        // Boxed: these futures are big, and `ensure` sits inside nearly every request's.
        let got = if windows { Box::pin(upload_windows(spec, &build_path(&target)?, bytes, &hash)).await? } else { Box::pin(upload(spec, bytes, &hash)).await? };
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

/// Muxin on an iPad doesn't update a paired machine's agent (Muxin on the Mac
/// does): it uses the one that machine runs, over its direct link.
async fn running_agent(spec: &MachineSpec) -> Result<(), String> {
    match crate::fleet::link::current_or_direct(spec).await {
        Some(_) => Ok(()),
        None => Err(format!("Can't reach {}. It may be off, or not paired with this iPad.", spec.name)),
    }
}

/// Forget that a machine was verified (after an error, it's checked again).
pub fn forget(machine_id: &str) {
    verified().lock().unwrap().remove(machine_id);
}

/// Copies the build into place atomically and prints its hash.
async fn upload(spec: &MachineSpec, bytes: Vec<u8>, hash: &str) -> Result<String, String> {
    let script = upload_script(hash);
    let mut child = if spec.is_local() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &script]);
        crate::fleet::child::Child::local(c).map_err(|e| format!("couldn't run sh: {e}"))?
    } else {
        crate::fleet::child::ssh(spec, &format!("sh -c {}", sh_quote(&script))).await?
    };
    let mut stdin = child.take_stdin().ok_or("no stdin")?;
    let mut stdout = child.take_stdout().ok_or("no stdout")?;
    let mut stderr = child.take_stderr().ok_or("no stderr")?;
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

/// The script that puts the build streamed to its stdin in place. The upload
/// is checked first: a stream cut off on the way (`cat` just sees its end)
/// would otherwise replace the working agent with part of one. The staged
/// file has to say it's the build (`hash` hashes its own file); else it goes,
/// and the agent there stays as it was. (It runs in a shell of its own, so a
/// crash's "Segmentation fault" stays out of the message.)
///
/// Then the folder from before the rename (OpenRunner) moves over: running
/// sessions keep their server (its socket moves with the folder). Then the
/// direct-connection service (if set up) moves to the new name or restarts
/// onto the new build. A listener the service doesn't know about (one left
/// from before, holding the port) goes first, or the old build would keep
/// answering. (With no service, `migrate` starts the listener again.)
fn upload_script(hash: &str) -> String {
    format!(
        "want={}; {}",
        sh_quote(hash),
        r#"set -e; if [ -d "$HOME/.openrunner" ] && [ ! -e "$HOME/.muxen" ]; then mv "$HOME/.openrunner" "$HOME/.muxen"; fi; d="$HOME/.muxen/bin"; mkdir -p "$d"; n="$d/muxen-agent.new.$$"; cat > "$n"; chmod 755 "$n"; got=$(sh -c '"$1" hash; exit $?' sh "$n" 2>/dev/null) || got=; if [ "$got" != "$want" ]; then rm -f "$n"; echo "the upload arrived incomplete (its hash is ${got:-unreadable}, not $want), so the agent there is as it was" >&2; exit 3; fi; mv -f "$n" "$d/muxen-agent"; rm -f "$d/openrunner-agent"; "$d/muxen-agent" migrate >/dev/null 2>&1 || true; rmdir "$HOME/.openrunner" 2>/dev/null || true; if systemctl --user is-enabled muxen-agent.service >/dev/null 2>&1; then pkill -f "$d/muxen-agent listen" >/dev/null 2>&1 || true; systemctl --user restart muxen-agent.service >/dev/null 2>&1 || true; elif launchctl print "gui/$(id -u)/com.lumarylabs.muxen-agent" >/dev/null 2>&1; then pkill -f "$d/muxen-agent listen" >/dev/null 2>&1 || true; launchctl kickstart -k "gui/$(id -u)/com.lumarylabs.muxen-agent" >/dev/null 2>&1 || true; fi; "$d/muxen-agent" hash"#
    )
}

/// Puts an uploaded build (`muxen-agent.upload.exe`) in place on Windows and
/// prints its hash, once the upload says it's the build (`hash`); else it
/// goes, and the agent there stays as it was. A running agent holds its file,
/// but Windows lets it be renamed: the old one moves aside and goes once
/// nothing runs it; the desktop helper, running the old build, goes now. Then
/// `migrate` brings up to date what an older build set up (its firewall rule)
/// and starts the direct-connection listener again from the new build.
fn swap_ps1(hash: &str) -> String {
    format!("$want = {}\n{SWAP_PS1}", crate::ops::remote::ps_quote(hash))
}

const SWAP_PS1: &str = "$ErrorActionPreference = 'Stop'\n\
         $d = Join-Path $env:USERPROFILE '.muxen\\bin'\n\
         $t = Join-Path $d 'muxen-agent.exe'\n\
         $u = Join-Path $d 'muxen-agent.upload.exe'\n\
         $got = ''\n\
         try { $got = (& $u hash | Out-String).Trim() } catch { }\n\
         if ($got -ne $want) { Remove-Item -Force $u -ErrorAction SilentlyContinue; [Console]::Error.WriteLine(\"error: the upload arrived incomplete (its hash is $(if ($got) { $got } else { 'unreadable' }), not $want), so the agent there is as it was\"); exit 3 }\n\
         if (Test-Path $t) { Move-Item -Force $t (Join-Path $d \"muxen-agent.old.$PID.exe\") }\n\
         Move-Item -Force $u $t\n\
         Get-CimInstance Win32_Process -Filter \"Name='muxen-agent.exe'\" | Where-Object { $_.CommandLine -match ' gui-serve' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }\n\
         & $t migrate | Out-Null\n\
         Get-ChildItem $d -Filter 'muxen-agent.old.*.exe' | ForEach-Object { Remove-Item -Force $_.FullName -ErrorAction SilentlyContinue }\n\
         & $t hash\n";

/// Copies the build into place on Windows and prints its hash: over sftp
/// (Windows' sshd stalls on a few hundred KB through a program's stdin), or
/// a copy on this computer; then SWAP_PS1.
async fn upload_windows(spec: &MachineSpec, build: &std::path::Path, bytes: Vec<u8>, hash: &str) -> Result<String, String> {
    let fail = |e: String| format!("couldn't install the agent on {}: {e}", spec.name);
    // An iPad: over its own ssh client's sftp.
    #[cfg(target_os = "ios")]
    {
        let _ = build;
        tokio::time::timeout(UPLOAD_TIMEOUT, crate::fleet::ssh::sftp_put(spec, &bytes, ".muxen/bin/muxen-agent.upload.exe"))
            .await
            .map_err(|_| fail("the copy took too long".into()))?
            .map_err(fail)?;
        return run_ps(spec, &swap_ps1(hash), CTL_TIMEOUT).await.map_err(fail);
    }
    #[cfg(not(target_os = "ios"))]
    let _ = bytes;
    if spec.is_local() {
        // This computer: a copy, then the same swap.
        let bin = dirs::home_dir().unwrap_or_default().join(".muxen").join("bin");
        std::fs::create_dir_all(&bin).map_err(|e| fail(e.to_string()))?;
        std::fs::copy(build, bin.join("muxen-agent.upload.exe")).map_err(|e| fail(e.to_string()))?;
        return run_ps(spec, &swap_ps1(hash), CTL_TIMEOUT).await.map_err(fail);
    }
    // sftp takes ssh's options, but its own spelling of the port and user.
    let mut args: Vec<String> = Vec::new();
    let mut ssh = probe::ssh_args(spec).into_iter().filter(|a| a != "-T").peekable();
    while let Some(a) = ssh.next() {
        match a.as_str() {
            "-p" => args.extend(["-P".into(), ssh.next().unwrap_or_default()]),
            "-l" => args.extend(["-o".into(), format!("User={}", ssh.next().unwrap_or_default())]),
            _ => args.push(a),
        }
    }
    let batch = format!("-mkdir .muxen\n-mkdir .muxen/bin\nput \"{}\" .muxen/bin/muxen-agent.upload.exe\n", build.display());
    let mut cmd = Command::new("sftp");
    cmd.args(["-q", "-b", "-"]).args(&args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| fail(format!("couldn't run sftp: {e}")))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let sent = async move {
        stdin.write_all(batch.as_bytes()).await.map_err(|e| e.to_string())?;
        drop(stdin);
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err).await;
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if status.success() { Ok(()) } else { Err(err.lines().last().unwrap_or("sftp failed").trim().to_string()) }
    };
    tokio::time::timeout(UPLOAD_TIMEOUT, sent).await.map_err(|_| fail("the copy took too long".into()))?.map_err(fail)?;
    run_ps(spec, &swap_ps1(hash), CTL_TIMEOUT).await.map_err(fail)
}

/// Runs a short PowerShell script on a Windows machine (with the user's profile, like workspace commands).
pub(crate) async fn run_ps(spec: &MachineSpec, script: &str, timeout: Duration) -> Result<String, String> {
    let marker = transport::new_marker();
    let limits = Limits { head: 1 << 20, tail: 1 << 20, line: 1 << 20 };
    let out = Box::pin(transport::run(spec, true, &marker, transport::with_marker(&marker, true, script), timeout, limits, &[], None)).await?;
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

/// Runs a short sh script on the machine (login shell, like workspace commands).
pub(crate) async fn run_sh(spec: &MachineSpec, script: &str, timeout: Duration) -> Result<String, String> {
    let marker = transport::new_marker();
    let out = Box::pin(transport::run(spec, false, &marker, transport::with_marker(&marker, false, script), timeout, Limits::OUTPUT, &[], None)).await?;
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
    let token = if is_windows(spec) {
        let q = crate::ops::remote::ps_quote;
        run_ps(spec, &format!("& {} auth token --hostname {}\n", q(gh), q(hostname)), CTL_TIMEOUT).await?
    } else {
        run_sh(spec, &format!("{} auth token --hostname {}\n", sh_quote(gh), sh_quote(hostname)), CTL_TIMEOUT).await?
    };
    let token = token.trim().to_string();
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
    spec.refuse_if_disconnected()?;
    if let Some(link) = crate::fleet::link::get(spec).await {
        if let Ok(reply) = link.request(request.clone(), CTL_TIMEOUT).await {
            return Ok(reply);
        }
    }
    let json = request.to_string();
    let out = if is_windows(spec) {
        // UTF-8 to the agent (PowerShell 5.1 pipes in the console's code page otherwise).
        let script = format!("$OutputEncoding = New-Object Text.UTF8Encoding $false\n$a = Join-Path $env:USERPROFILE '.muxen\\bin\\muxen-agent.exe'\n@'\n{json}\n'@ | & $a ctl\n");
        run_ps(spec, &script, CTL_TIMEOUT).await
    } else {
        let script = format!("{REMOTE_PATH} ctl <<'__OR_REQUEST__'\n{json}\n__OR_REQUEST__\n");
        run_sh(spec, &script, CTL_TIMEOUT).await
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_ipad_uses_the_running_agent_only_over_a_direct_link() {
        // Not paired: no link to use, and no ssh tried instead.
        let mut spec: MachineSpec = toml::from_str("name = \"Beelink\"\nhost = \"beelink.invalid\"").unwrap();
        spec.id = format!("unpaired-{}", std::process::id());
        let err = running_agent(&spec).await.unwrap_err();
        assert!(err.contains("not paired with this iPad"), "{err}");
    }

    #[test]
    fn the_install_scripts_check_the_upload_before_the_swap() {
        let sh = upload_script("0123456789abcdef");
        assert!(sh.starts_with("want='0123456789abcdef'; set -e;"));
        assert!(sh.find("hash; exit $?' sh \"$n\"").unwrap() < sh.find("mv -f \"$n\"").unwrap());
        let ps = swap_ps1("0123456789abcdef");
        assert!(ps.starts_with("$want = '0123456789abcdef'\n"));
        assert!(ps.find("& $u hash").unwrap() < ps.find("Move-Item -Force $u $t").unwrap());
    }

    /// An upload that stops partway (or isn't the build) doesn't replace the
    /// agent: it's left as it was, and the upload goes. (The script stops
    /// before anything that touches services.)
    #[cfg(unix)]
    #[test]
    fn a_cut_off_upload_leaves_the_agent_alone() {
        use std::io::Write as _;
        let cut_off: &[u8] = b"\x7fELF\x02\x01\x01\x00 and then the stream stopped";
        let not_the_build: &[u8] = b"#!/bin/sh\necho 0000000000000000\n";
        for (i, upload) in [cut_off, not_the_build].into_iter().enumerate() {
            let home = std::env::temp_dir().join(format!("muxen-upload-{}-{i}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            let bin = home.join(".muxen/bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("muxen-agent"), b"the agent that works").unwrap();
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", &upload_script("0123456789abcdef")])
                .env("HOME", &home)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(upload).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(!out.status.success(), "case {i}");
            assert!(String::from_utf8_lossy(&out.stderr).contains("arrived incomplete"), "case {i}");
            assert_eq!(std::fs::read(bin.join("muxen-agent")).unwrap(), b"the agent that works", "case {i}");
            assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 1, "case {i}: the upload is gone");
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn host_target_names_a_build_like_a_check_would() {
        let probe = ProbeResult { os: Some(std::env::consts::OS.into()), arch: Some(std::env::consts::ARCH.into()), ..Default::default() };
        assert_eq!(target(Some(&probe)), Some(host_target()));
    }

    #[test]
    fn only_a_binary_inside_the_checkout_uses_it() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).canonicalize().unwrap();
        assert_eq!(checkout(&root.join("target/release/muxin")), Some(root));
        assert_eq!(checkout(std::path::Path::new("/Applications/Muxin.app/Contents/MacOS/muxin")), None);
    }
}
