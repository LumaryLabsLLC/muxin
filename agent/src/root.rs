//! Doing things as root, for the user: through sudo, without a password when
//! sudo allows it here (passwordless, or used a moment ago), else with the
//! one the user typed in the app, on sudo's stdin and nowhere else.

/// What a request says when it needs the user's password (the app asks for it).
pub const NEEDS_PASSWORD: &str = "Muxen needs your password on this machine to do that.";
pub const WRONG_PASSWORD: &str = "That password didn't work.";

/// Whether an error asks for the password (again).
pub fn wants_password(error: &str) -> bool {
    error == NEEDS_PASSWORD || error == WRONG_PASSWORD
}

/// Runs `script` (with `args` as $1, $2, ...) as root.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn as_root(script: &str, args: &[&str], password: Option<&str>) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    // Whether sudo needs a password here at all (its wording differs between versions).
    let free = Command::new("sudo").args(["-n", "true"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if free {
        let out = Command::new("sudo").args(["-n", "sh", "-c", script, "muxen"]).args(args).stdin(Stdio::null()).stdout(Stdio::null()).output().map_err(|e| e.to_string())?;
        return if out.status.success() { Ok(()) } else { Err(last_line(&String::from_utf8_lossy(&out.stderr))) };
    }
    let password = password.filter(|p| !p.is_empty()).ok_or(NEEDS_PASSWORD)?;
    let mut child = Command::new("sudo")
        .args(["-S", "-p", "", "sh", "-c", script, "muxen"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{password}\n").as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("incorrect password") || err.contains("Sorry, try again") {
        Err(WRONG_PASSWORD.into())
    } else if err.contains("not in the sudoers") || err.contains("is not allowed") {
        Err("This account can't use sudo on this machine.".into())
    } else {
        Err(last_line(&err))
    }
}

fn last_line(text: &str) -> String {
    text.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("It didn't work.").trim().to_string()
}

/// Where a command-line tool is: on PATH, or where Homebrew, Docker Desktop
/// and snap put them (the agent's background service starts with a bare PATH).
pub fn tool(name: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs = path.split(':').map(str::to_string).chain(["/opt/homebrew/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin", "/snap/bin", "/usr/bin", "/bin"].map(String::from));
    for dir in dirs.filter(|d| !d.is_empty()) {
        let p = std::path::Path::new(&dir).join(name);
        if std::fs::metadata(&p).is_ok_and(|m| m.is_file()) {
            return Some(p.display().to_string());
        }
    }
    None
}
