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

/// Where the build for `target` is.
fn build_path(target: &str) -> Result<PathBuf, String> {
    let dir = builds_dir().ok_or("Muxin can't find its agent builds (run scripts/build-agents.sh in a source checkout).")?;
    let exe = if target.starts_with("windows") { ".exe" } else { "" };
    Ok(dir.join(format!("muxen-agent-{target}{exe}")))
}

/// The build for `target` and its hash (what `muxen-agent hash` prints).
fn build(target: &str) -> Result<(Vec<u8>, String), String> {
    let path = build_path(target)?;
    let bytes = std::fs::read(&path).map_err(|_| format!("Muxin has no agent build for {target} machines yet."))?;
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
        let got = if windows {
            Box::pin(upload_windows(spec, &build_path(&target)?, &hash)).await?
        } else {
            Box::pin(upload(spec, bytes, &hash)).await?
        };
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
async fn upload(spec: &MachineSpec, bytes: Vec<u8>, hash: &str) -> Result<String, String> {
    // The folder from before the rename (OpenRunner) moves over first: running
    // sessions keep their server (its socket moves with the folder). Then the
    // direct-connection service (if set up) moves to the new name or restarts
    // onto the new build. A listener the service doesn't know about (one left
    // from before, holding the port) goes first, or the old build would keep
    // answering.
    let script = upload_script(hash);
    let mut cmd = if spec.is_local() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &script]);
        c
    } else {
        let mut c = Command::new("ssh");
        c.args(probe::ssh_args(spec));
        c.arg(ssh_upload_command(&script));
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

fn upload_script(hash: &str) -> String {
    format!(
        "set -e\nif [ -d \"$HOME/.openrunner\" ] && [ ! -e \"$HOME/.muxen\" ]; then mv \"$HOME/.openrunner\" \"$HOME/.muxen\"; fi\nd=\"$HOME/.muxen/bin\"\nmkdir -p \"$d\"\n{}\nrmdir \"$HOME/.openrunner\" 2>/dev/null || true\n",
        install_script(false, hash)
    )
}

/// The login shell sees one line. POSIX printf restores the script without
/// consuming stdin, which carries the binary rather than shell commands.
fn ssh_upload_command(script: &str) -> String {
    let encoded: String = script.bytes().map(|b| format!("\\0{b:03o}")).collect();
    let command = format!("eval \"$(printf '%b' '{encoded}')\"");
    format!("sh -c {}", sh_quote(&command))
}

/// Verify before the swap, keep a backup and restore it on a failed health check.
fn install_script(windows: bool, hash: &str) -> String {
    let script = if windows {
        include_str!("scripts/install-agent.ps1")
    } else {
        include_str!("scripts/install-agent.sh")
    };
    script
        .replace("\r\n", "\n")
        .replace("__EXPECTED_HASH__", hash)
}

/// Copies the build into place on Windows and prints its hash: over sftp
/// (Windows' sshd stalls on a few hundred KB through a program's stdin), or
/// a copy on this computer; then the verified swap.
async fn upload_windows(
    spec: &MachineSpec,
    build: &std::path::Path,
    hash: &str,
) -> Result<String, String> {
    let fail = |e: String| format!("couldn't install the agent on {}: {e}", spec.name);
    if spec.is_local() {
        // This computer: a copy, then the same swap.
        let bin = dirs::home_dir().unwrap_or_default().join(".muxen").join("bin");
        std::fs::create_dir_all(&bin).map_err(|e| fail(e.to_string()))?;
        std::fs::copy(build, bin.join("muxen-agent.upload.exe"))
            .map_err(|e| fail(e.to_string()))?;
        return run_ps(spec, &install_script(true, hash), CTL_TIMEOUT)
            .await
            .map_err(fail);
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
    tokio::time::timeout(UPLOAD_TIMEOUT, sent)
        .await
        .map_err(|_| fail("the copy took too long".into()))?
        .map_err(fail)?;
    run_ps(spec, &install_script(true, hash), CTL_TIMEOUT)
        .await
        .map_err(fail)
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

    #[test]
    fn windows_verifies_before_an_atomic_swap_and_keeps_a_restore_path() {
        assert!(!install_script(false, "0123456789abcdef").contains('\r'));
        let script = install_script(true, "0123456789abcdef");
        assert!(script.contains("$expected = '0123456789abcdef'"));
        assert!(
            script.find("    Check-Hash $n").unwrap()
                < script.find("[IO.File]::Replace($n, $t, $p)").unwrap()
        );
        assert!(
            script.find("[IO.File]::Replace($n, $t, $p)").unwrap()
                < script.find("    Check-Hash $t").unwrap()
        );
        assert!(script.contains("muxen-agent.old.$PID."));
        assert!(script.contains("[Guid]::NewGuid()"));
        assert!(script.contains("$_.FullName -ne $p"));
        assert!(
            script.contains(
                "Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue"
            )
        );
        assert!(script.contains("[IO.File]::Replace($restore, $t, $null)"));
    }

    #[test]
    fn uploads_keep_the_remote_command_on_one_line_and_remove_the_old_folder() {
        let script = upload_script("0123456789abcdef");
        let command = ssh_upload_command(&script);
        assert!(!command.contains(['\n', '\r']));
        assert!(script.contains("rmdir \"$HOME/.openrunner\" 2>/dev/null || true"));
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        struct Machine {
            dir: PathBuf,
        }

        impl Machine {
            fn new(service: bool) -> Self {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let dir = std::env::temp_dir().join(format!(
                    "muxin-install-{}-{unique}-{sequence}",
                    std::process::id()
                ));
                std::fs::create_dir(&dir).unwrap();
                std::fs::create_dir_all(dir.join("tools")).unwrap();
                std::fs::create_dir_all(dir.join("bin")).unwrap();
                let me = Self { dir };
                let systemctl = if service {
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$MUXEN_TEST_EVENTS\"\ncase \"$*\" in\n'--user is-enabled muxen-agent.service') exit 0;;\n'--user restart muxen-agent.service') if [ ! -e \"$MUXEN_TEST_FAILED\" ]; then touch \"$MUXEN_TEST_FAILED\"; exit 1; fi;;\nesac\n"
                } else {
                    "#!/bin/sh\nexit 1\n"
                };
                me.write("tools/systemctl", systemctl.as_bytes());
                me.write("tools/launchctl", b"#!/bin/sh\nexit 1\n");
                me.write("tools/pkill", b"#!/bin/sh\nexit 1\n");
                me
            }

            fn write(&self, name: &str, bytes: &[u8]) {
                let path = self.dir.join(name);
                std::fs::write(&path, bytes).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }

            fn run(&self, binary: &[u8]) -> std::process::Output {
                self.run_with_shell(binary, false)
            }

            fn run_with_shell(&self, binary: &[u8], ssh: bool) -> std::process::Output {
                let script = format!(
                    "d={}\n{}",
                    sh_quote(&self.dir.join("bin").display().to_string()),
                    install_script(false, "new")
                );
                let command = if ssh {
                    ssh_upload_command(&script)
                } else {
                    script
                };
                let mut child = std::process::Command::new("/bin/sh")
                    .args(["-c", &command])
                    .env(
                        "PATH",
                        format!(
                            "{}:{}",
                            self.dir.join("tools").display(),
                            std::env::var("PATH").unwrap_or_default()
                        ),
                    )
                    .env("MUXEN_TEST_EVENTS", self.dir.join("events"))
                    .env("MUXEN_TEST_FAILED", self.dir.join("failed"))
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child.stdin.take().unwrap().write_all(binary).unwrap();
                child.wait_with_output().unwrap()
            }

            fn installed(&self) -> Vec<u8> {
                std::fs::read(self.dir.join("bin/muxen-agent")).unwrap()
            }

            fn previous(&self) -> Vec<u8> {
                std::fs::read(self.dir.join("bin/muxen-agent.prev")).unwrap()
            }

            fn no_staging_files(&self) {
                assert!(
                    std::fs::read_dir(self.dir.join("bin")).unwrap().all(|e| !e
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".new."))
                );
            }
        }

        impl Drop for Machine {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        fn binary(staged: &str, live: &str) -> Vec<u8> {
            format!("#!/bin/sh\ncase \"$1\" in\nhash) case \"$0\" in *.new.*) echo {staged};; *) {live};; esac;;\nmigrate) exit 0;;\nesac\n").into_bytes()
        }

        #[test]
        fn a_good_upload_keeps_the_previous_binary() {
            let machine = Machine::new(false);
            machine.write("bin/muxen-agent", b"previous");
            let new = binary("new", "echo new");
            let result = machine.run(&new);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "new");
            assert_eq!(machine.installed(), new);
            assert_eq!(machine.previous(), b"previous");
            machine.no_staging_files();
        }

        #[test]
        fn the_single_line_ssh_command_preserves_the_script_and_binary_stdin() {
            let machine = Machine::new(false);
            machine.write("bin/muxen-agent", b"previous");
            let new = binary("new", "echo new");
            let result = machine.run_with_shell(&new, true);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(machine.installed(), new);
            assert_eq!(machine.previous(), b"previous");
            assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "new");
        }

        #[test]
        fn a_wrong_upload_leaves_the_live_binary_and_backup_alone() {
            let machine = Machine::new(true);
            machine.write("bin/muxen-agent", b"previous");
            machine.write("bin/muxen-agent.prev", b"older");
            let result = machine.run(&binary("wrong", "echo new"));
            assert!(!result.status.success());
            assert!(String::from_utf8_lossy(&result.stderr).contains("wrong hash"));
            assert_eq!(machine.installed(), b"previous");
            assert_eq!(machine.previous(), b"older");
            assert!(
                !machine.dir.join("events").exists(),
                "no service command before verification"
            );
            machine.no_staging_files();
        }

        #[test]
        fn a_binary_that_cannot_run_does_not_replace_the_live_one() {
            let machine = Machine::new(false);
            machine.write("bin/muxen-agent", b"previous");
            assert!(!machine.run(b"#!/bin/sh\nexit 23\n").status.success());
            assert_eq!(machine.installed(), b"previous");
            assert!(!machine.dir.join("bin/muxen-agent.prev").exists());
            machine.no_staging_files();
        }

        #[test]
        fn a_failed_health_check_restores_the_previous_binary() {
            for health in ["echo wrong", "exit 23"] {
                let machine = Machine::new(false);
                machine.write("bin/muxen-agent", b"previous");
                let result = machine.run(&binary("new", health));
                assert!(!result.status.success());
                assert!(
                    String::from_utf8_lossy(&result.stderr).contains("Restored the previous agent")
                );
                assert_eq!(machine.installed(), b"previous");
                assert_eq!(machine.previous(), b"previous");
                machine.no_staging_files();
            }
        }

        #[test]
        fn a_failed_service_restart_restores_and_restarts_the_previous_agent() {
            let machine = Machine::new(true);
            machine.write("bin/muxen-agent", b"previous");
            let result = machine.run(&binary("new", "echo new"));
            assert!(!result.status.success());
            assert_eq!(machine.installed(), b"previous");
            let events = std::fs::read_to_string(machine.dir.join("events")).unwrap();
            assert_eq!(
                events
                    .lines()
                    .filter(|l| *l == "--user restart muxen-agent.service")
                    .count(),
                2
            );
            machine.no_staging_files();
        }

        #[test]
        fn a_first_install_is_removed_if_its_health_check_fails() {
            let machine = Machine::new(false);
            assert!(!machine.run(&binary("new", "exit 23")).status.success());
            assert!(!machine.dir.join("bin/muxen-agent").exists());
            assert!(!machine.dir.join("bin/muxen-agent.prev").exists());
            machine.no_staging_files();
        }

        #[test]
        fn a_first_install_can_succeed_without_a_backup() {
            let machine = Machine::new(false);
            let new = binary("new", "echo new");
            assert!(machine.run(&new).status.success());
            assert_eq!(machine.installed(), new);
            assert!(!machine.dir.join("bin/muxen-agent.prev").exists());
            machine.no_staging_files();
        }
    }
}
