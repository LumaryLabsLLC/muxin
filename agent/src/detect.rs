//! What's installed: the user's login shell, and which agent CLIs are on the
//! PATH their interactive shell sets up (nvm, fnm, asdf live in rc files).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The user's login shell: $SHELL, else the password database, else /bin/sh.
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

/// The PATH an interactive login shell sets up, then this process's own.
pub fn path_dirs() -> Vec<String> {
    let mut dirs: Vec<String> = interactive_path().map(|p| p.split(':').map(String::from).collect()).unwrap_or_default();
    if let Ok(p) = std::env::var("PATH") {
        dirs.extend(p.split(':').map(String::from));
    }
    dirs
}

/// The commands among `names` found on those PATH folders.
pub fn agents(names: &[String], dirs: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| {
            dirs.iter().any(|d| {
                let f = std::path::Path::new(d).join(n);
                std::fs::metadata(&f).is_ok_and(|m| m.is_file() && {
                    use std::os::unix::fs::PermissionsExt;
                    m.permissions().mode() & 0o111 != 0
                })
            })
        })
        .cloned()
        .collect()
}

/// Asks an interactive login shell for its PATH, giving up after a few seconds.
fn interactive_path() -> Option<String> {
    let mut child = Command::new(user_shell())
        .args(["-l", "-i", "-c", "printf '__OR_PATH=%s\\n' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < Duration::from_secs(6) => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    out.lines().rev().find_map(|l| l.strip_prefix("__OR_PATH=").map(str::to_string))
}
