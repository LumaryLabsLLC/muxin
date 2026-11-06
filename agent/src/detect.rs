//! What's installed: the user's login shell, and which agent CLIs are on the
//! PATH their interactive shell sets up (nvm, fnm, asdf live in rc files).

#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

/// The user's shell on Windows: PowerShell 7 when it's installed, else
/// Windows PowerShell.
#[cfg(windows)]
pub fn user_shell() -> String {
    let dirs: Vec<String> = std::env::var("PATH").map(|p| p.split(';').map(String::from).collect()).unwrap_or_default();
    let installed = dirs.iter().chain([r"C:\Program Files\PowerShell\7".to_string()].iter()).map(|d| std::path::Path::new(d).join("pwsh.exe")).find(|p| p.is_file());
    match installed {
        Some(p) => p.display().to_string(),
        None => std::env::var("SystemRoot").map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe")).unwrap_or_else(|_| "powershell.exe".into()),
    }
}

/// The user's login shell: $SHELL, else the password database, else /bin/sh.
#[cfg(unix)]
pub fn user_shell() -> String {
    if let Some(s) = std::env::var("SHELL").ok().filter(|s| !s.is_empty()) {
        return s;
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            if let Ok(s) = std::ffi::CStr::from_ptr((*pw).pw_shell).to_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    "/bin/sh".into()
}

/// What the user's sessions see: the PATH folders, and where Claude Code and
/// Codex keep their settings (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`; None when
/// not set).
#[derive(Debug, Default)]
pub struct ShellEnv {
    pub path: Vec<String>,
    pub claude_config_dir: Option<String>,
    pub codex_home: Option<String>,
}

/// Windows keeps the user's in the environment (no rc files).
#[cfg(windows)]
pub fn shell_env() -> ShellEnv {
    let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
    ShellEnv {
        path: std::env::var("PATH").map(|p| p.split(';').filter(|d| !d.is_empty()).map(String::from).collect()).unwrap_or_default(),
        claude_config_dir: var("CLAUDE_CONFIG_DIR"),
        codex_home: var("CODEX_HOME"),
    }
}

/// The PATH an interactive login shell sets up, then this process's own;
/// the settings folders as that shell has them.
#[cfg(unix)]
pub fn shell_env() -> ShellEnv {
    let found = interactive_env();
    let get = |k: &str| found.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mut path: Vec<String> = get("PATH").map(|p| p.split(':').map(String::from).collect()).unwrap_or_default();
    if let Ok(p) = std::env::var("PATH") {
        path.extend(p.split(':').map(String::from));
    }
    let var = |k: &str| match found.is_empty() {
        // The shell didn't answer: this process's own.
        true => std::env::var(k).ok(),
        false => get(k),
    }
    .filter(|v| !v.is_empty());
    ShellEnv { path, claude_config_dir: var("CLAUDE_CONFIG_DIR"), codex_home: var("CODEX_HOME") }
}

/// The commands among `names` found on those PATH folders.
pub fn agents(names: &[String], dirs: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| {
            dirs.iter().any(|d| {
                let f = std::path::Path::new(d).join(n);
                runnable(&f)
            })
        })
        .cloned()
        .collect()
}

/// Whether a command is there and can run (on Windows, with one of the
/// extensions that run: claude.cmd, codex.exe, ...).
#[cfg(unix)]
fn runnable(f: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(f).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn runnable(f: &std::path::Path) -> bool {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    exts.split(';').filter(|e| !e.is_empty()).chain([".ps1"]).any(|e| {
        let mut name = f.as_os_str().to_owned();
        name.push(e.to_ascii_lowercase());
        std::path::Path::new(&name).is_file()
    })
}

/// Asks an interactive login shell for its PATH and the agents' settings
/// folders, giving up after a few seconds.
#[cfg(unix)]
fn interactive_env() -> Vec<(String, String)> {
    interactive_output().map(|out| parse_env(&out)).unwrap_or_default()
}

/// `__OR_<name>=<value>` lines, as the shell printed them (rc files may print too).
#[cfg(unix)]
fn parse_env(out: &str) -> Vec<(String, String)> {
    ["PATH", "CLAUDE_CONFIG_DIR", "CODEX_HOME"]
        .iter()
        .filter_map(|k| out.lines().rev().find_map(|l| l.strip_prefix(&format!("__OR_{k}="))).map(|v| (k.to_string(), v.to_string())))
        .collect()
}

#[cfg(unix)]
fn interactive_output() -> Option<String> {
    let mut cmd = Command::new(user_shell());
    let script = "printf '__OR_PATH=%s\\n__OR_CLAUDE_CONFIG_DIR=%s\\n__OR_CODEX_HOME=%s\\n' \"$PATH\" \"$CLAUDE_CONFIG_DIR\" \"$CODEX_HOME\"";
    cmd.args(["-l", "-i", "-c", script]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    // In a session of its own, without a terminal: an interactive shell takes
    // over the terminal it finds (and stops its process group with SIGTTIN when
    // that's not in front), which is the one Muxin was started from when the
    // agent runs for this computer.
    crate::sys::own_group(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < Duration::from_secs(6) => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                crate::sys::kill_tree(child.id(), false);
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

#[cfg(all(test, unix))]
#[test]
fn reads_what_the_shell_printed() {
    let out = "Welcome!\n__OR_PATH=/usr/bin:/bin\n__OR_CLAUDE_CONFIG_DIR=\n__OR_CODEX_HOME=/home/a/.codex-work\n";
    let env = parse_env(out);
    assert_eq!(env, vec![("PATH".into(), "/usr/bin:/bin".into()), ("CLAUDE_CONFIG_DIR".into(), "".into()), ("CODEX_HOME".into(), "/home/a/.codex-work".into())]);
}
