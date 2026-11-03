//! Shutting down and restarting this machine, for the user: as root through
//! sudo on Linux and Macs, with `shutdown.exe` on Windows. It answers first and
//! goes a moment later, so the app hears that it's on its way.

use serde_json::{Value, json};

/// `{"op": "machine_power", "action": "shutdown"|"restart", "password"?}`.
pub fn act(req: &Value) -> Value {
    let action = req["action"].as_str().unwrap_or("");
    if !matches!(action, "shutdown" | "restart") {
        return json!({ "ok": false, "error": "Shut down or restart?" });
    }
    match go(action, req["password"].as_str()) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "password": crate::root::wants_password(&e), "error": e }),
    }
}

/// Runs as root with `shutdown`'s flag as $1 and what to try without it as $2.
/// nohup: sudo may end what its command leaves behind. Linux's lingering is what
/// starts the agent's user service at boot, with nobody signed in: without it,
/// a machine that restarts here stays out of reach of an iPad until someone does.
#[cfg(unix)]
const SCRIPT: &str = r#"PATH="$PATH:/usr/sbin:/sbin:/usr/bin:/bin"
command -v shutdown >/dev/null 2>&1 || command -v "$2" >/dev/null 2>&1 || { echo "This machine has no shutdown command." >&2; exit 1; }
if [ -n "${SUDO_USER:-}" ] && command -v loginctl >/dev/null 2>&1; then loginctl enable-linger "$SUDO_USER" >/dev/null 2>&1 || true; fi
nohup sh -c 'sleep 2; shutdown "$0" now || "$1"' "$1" "$2" >/dev/null 2>&1 </dev/null &
"#;

#[cfg(unix)]
fn go(action: &str, password: Option<&str>) -> Result<(), String> {
    // A Mac's -h powers it off; Linux's -P says so outright (-h can just halt).
    let off = if cfg!(target_os = "macos") { ("-h", "halt") } else { ("-P", "poweroff") };
    let (flag, fallback) = if action == "restart" { ("-r", "reboot") } else { off };
    crate::root::as_root(SCRIPT, &[flag, fallback], password)
}

#[cfg(windows)]
fn go(action: &str, _password: Option<&str>) -> Result<(), String> {
    let flag = if action == "restart" { "/r" } else { "/s" };
    let out = crate::sys::no_window(std::process::Command::new("shutdown").args([flag, "/t", "3", "/c", "Muxin was asked to do this."])).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stderr);
    // Asked twice: it's already on its way (1190, in any language).
    if text.contains("1190") {
        return Ok(());
    }
    Err(if text.contains("(5)") { "Windows wants an administrator for that.".into() } else { text.trim().to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_shutting_down_and_restarting() {
        let reply = act(&json!({ "op": "machine_power", "action": "sleep" }));
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "Shut down or restart?");
    }

    /// With stand-ins first on PATH, and not as root: nothing goes down.
    #[cfg(unix)]
    #[test]
    fn the_script_answers_first_and_goes_a_moment_later() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("muxin-power-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mark = dir.join("mark");
        for tool in ["shutdown", "loginctl"] {
            let path = dir.join(tool);
            std::fs::write(&path, format!("#!/bin/sh\necho \"{tool} $*\" >> \"{}\"\n", mark.display())).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(std::process::Command::new("sh").args(["-c", &format!("PATH={}:$PATH; command -v {tool}", dir.display())]).output().unwrap().stdout, format!("{}\n", path.display()).into_bytes());
        }
        let started = std::time::Instant::now();
        let status = std::process::Command::new("sh")
            .args(["-c", SCRIPT, "muxin", "-r", "reboot"])
            .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
            .env("SUDO_USER", "alice")
            .status()
            .unwrap();
        assert!(status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "it should answer before it goes");
        let went = || std::fs::read_to_string(&mark).unwrap_or_default().contains("shutdown -r now");
        assert!(!went(), "it went before it answered");
        while !went() && started.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let log = std::fs::read_to_string(&mark).unwrap_or_default();
        assert!(log.contains("shutdown -r now"), "{log}");
        assert!(log.contains("loginctl enable-linger alice"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
