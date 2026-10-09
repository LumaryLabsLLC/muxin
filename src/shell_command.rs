//! The `muxen` command in the user's PATH, so terminals (and other agents,
//! with `muxen mcp`) can run it: a link to this executable in
//! /usr/local/bin on macOS (asking for an administrator's password when the
//! folder isn't writable) and ~/.local/bin on Linux; on Windows, this
//! executable's folder added to the user's PATH.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// "installed" | "missing" | "other" (something else is there) | "unsupported"
    pub state: &'static str,
    /// Where the command lives: the link, or on Windows the folder in PATH.
    pub path: String,
    /// What runs: this executable, or what the link points to now.
    pub target: Option<String>,
    /// Something to know, in plain words ("Open a new terminal to use it.").
    pub note: Option<String>,
    /// "other" that install may replace: a link to another copy of Muxen.
    pub replaceable: bool,
}

/// Whether `muxen` is in the PATH and runs this copy of Muxen.
pub fn status() -> Status {
    let exe = match this_exe() {
        Ok(exe) => exe,
        Err(e) => return Status { state: "unsupported", path: String::new(), target: None, note: Some(e), replaceable: false },
    };
    imp::status(&exe)
}

pub fn install() -> Result<Status, String> {
    let exe = this_exe()?;
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        return Err("macOS is running Muxen from a temporary copy. Move Muxen to Applications, open it from there, and try again.".into());
    }
    imp::install(&exe)?;
    Ok(imp::status(&exe))
}

pub fn remove() -> Result<Status, String> {
    let exe = this_exe()?;
    imp::remove(&exe)?;
    Ok(imp::status(&exe))
}

fn this_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't tell where Muxen is: {e}"))?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// A link of ours: it points to a Muxen executable (this one, or another
/// copy), not to something else that happens to be called muxen.
#[cfg(unix)]
fn is_muxen(target: &Path) -> bool {
    let s = target.to_string_lossy();
    target.file_name().is_some_and(|n| n == "muxen") || s.contains("Muxen.app/")
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io::ErrorKind;

    /// Where the link goes on this OS.
    fn link_path() -> Result<PathBuf, String> {
        if cfg!(target_os = "macos") {
            Ok(PathBuf::from("/usr/local/bin/muxen"))
        } else {
            let home = std::env::var_os("HOME").ok_or("HOME isn't set.")?;
            Ok(PathBuf::from(home).join(".local/bin/muxen"))
        }
    }

    pub fn status(exe: &Path) -> Status {
        match link_path() {
            Ok(link) => status_at(&link, exe),
            Err(e) => Status { state: "unsupported", path: String::new(), target: None, note: Some(e), replaceable: false },
        }
    }

    pub(super) fn status_at(link: &Path, exe: &Path) -> Status {
        let link = link.to_path_buf();
        let path = link.display().to_string();
        let Ok(meta) = std::fs::symlink_metadata(&link) else {
            return Status { state: "missing", path, target: None, note: None, replaceable: false };
        };
        if !meta.file_type().is_symlink() {
            return Status { state: "other", path, target: None, note: Some("A different program called muxen is there; Muxen leaves it alone.".into()), replaceable: false };
        }
        let target = std::fs::read_link(&link).ok();
        let resolved = std::fs::canonicalize(&link).ok();
        let shown = target.as_ref().map(|t| t.display().to_string());
        let ours = target.as_deref().is_some_and(is_muxen);
        let other = |note: &str, replaceable| Status { state: "other", path: path.clone(), target: shown.clone(), note: Some(note.into()), replaceable };
        match resolved {
            Some(r) if r == exe => Status { state: "installed", path: path.clone(), target: shown.clone(), note: in_path_note(&link), replaceable: false },
            _ if !ours => other("It links to a different program called muxen; Muxen leaves it alone.", false),
            None => other("It points to a copy of Muxen that isn't there anymore.", true),
            Some(_) => other("It runs another copy of Muxen.", true),
        }
    }

    /// Linux: ~/.local/bin is only in PATH when the login that started this
    /// app found it (Ubuntu adds it at login, once it exists).
    fn in_path_note(link: &Path) -> Option<String> {
        if cfg!(target_os = "macos") {
            return None;
        }
        let dir = link.parent()?;
        let in_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
        (!in_path).then(|| format!("{} isn't in your PATH yet. It usually is after you log in again; or add it in your shell's profile.", dir.display()))
    }

    pub fn install(exe: &Path) -> Result<(), String> {
        install_at(&link_path()?, exe)
    }

    pub(super) fn install_at(link: &Path, exe: &Path) -> Result<(), String> {
        refuse_others(link)?;
        match link_here(link, exe) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::PermissionDenied && cfg!(target_os = "macos") => {
                let dir = link.parent().map(|d| d.display().to_string()).unwrap_or_default();
                as_admin(&format!("mkdir -p {} && ln -sfn {} {}", sh(&dir), sh(&exe.display().to_string()), sh(&link.display().to_string())))
            }
            Err(e) => Err(format!("Couldn't create {}: {e}", link.display())),
        }
    }

    pub fn remove(_exe: &Path) -> Result<(), String> {
        remove_at(&link_path()?)
    }

    pub(super) fn remove_at(link: &Path) -> Result<(), String> {
        let Ok(meta) = std::fs::symlink_metadata(link) else { return Ok(()) };
        let ours = meta.file_type().is_symlink() && std::fs::read_link(link).is_ok_and(|t| is_muxen(&t));
        if !ours {
            return Err(format!("{} isn't a link to Muxen, so Muxen leaves it alone.", link.display()));
        }
        match std::fs::remove_file(link) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::PermissionDenied && cfg!(target_os = "macos") => as_admin(&format!("rm -f {}", sh(&link.display().to_string()))),
            Err(e) => Err(format!("Couldn't remove {}: {e}", link.display())),
        }
    }

    /// Only ever replace a link to Muxen, never someone else's file.
    fn refuse_others(link: &Path) -> Result<(), String> {
        match std::fs::symlink_metadata(link) {
            Ok(m) if !m.file_type().is_symlink() => Err(format!("There's already a file at {} that isn't a link to Muxen. Remove it first if you don't need it.", link.display())),
            Ok(_) if !std::fs::read_link(link).is_ok_and(|t| is_muxen(&t)) => {
                Err(format!("{} already links to another program. Remove it first if you don't need it.", link.display()))
            }
            _ => Ok(()),
        }
    }

    fn link_here(link: &Path, exe: &Path) -> std::io::Result<()> {
        if let Some(dir) = link.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::symlink_metadata(link).is_ok() {
            std::fs::remove_file(link)?;
        }
        std::os::unix::fs::symlink(exe, link)
    }

    /// Runs a shell command as an administrator; macOS asks for the password.
    fn as_admin(command: &str) -> Result<(), String> {
        let applescript = format!("do shell script \"{}\" with administrator privileges", command.replace('\\', "\\\\").replace('"', "\\\""));
        let out = std::process::Command::new("osascript").arg("-e").arg(applescript).output().map_err(|e| format!("Couldn't ask for an administrator's password: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") || err.to_lowercase().contains("cancel") {
            Err("Cancelled.".into())
        } else {
            Err(format!("It didn't work: {}", err.trim()))
        }
    }

    /// Single-quoted for sh.
    fn sh(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    // The user's PATH as stored (unexpanded, keeping its kind), changed in
    // place; SetEnvironmentVariable on a scratch name then tells Explorer and
    // new terminals to reload the environment.
    const READ: &str = r#"
$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
$p = if ($k) { $k.GetValue('Path', '', 'DoNotExpandEnvironmentNames') } else { '' }
[Console]::Out.Write($p)
"#;

    const WRITE: &str = r#"
$k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
$p = $k.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
$kind = if ($p) { $k.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
$parts = @($p -split ';' | Where-Object { $_ -and ($_.TrimEnd('\') -ne $env:MUXEN_DIR.TrimEnd('\')) })
if ($env:MUXEN_ADD -eq '1') { $parts += $env:MUXEN_DIR }
$k.SetValue('Path', ($parts -join ';'), $kind)
[Environment]::SetEnvironmentVariable('MUXEN_PATH_CHANGED', $null, 'User')
"#;

    fn powershell(script: &str, env: &[(&str, &str)]) -> Result<String, String> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = std::process::Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]).creation_flags(CREATE_NO_WINDOW);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().map_err(|e| format!("Couldn't run PowerShell: {e}"))?;
        if !out.status.success() {
            return Err(format!("PowerShell: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn dir_of(exe: &Path) -> Result<String, String> {
        let dir = exe.parent().ok_or("Couldn't tell which folder Muxen is in.")?.display().to_string();
        // canonicalize gives \\?\C:\...; PATH wants C:\...
        Ok(dir.strip_prefix(r"\\?\").unwrap_or(&dir).to_string())
    }

    fn same(a: &str, b: &str) -> bool {
        a.trim_end_matches('\\').eq_ignore_ascii_case(b.trim_end_matches('\\'))
    }

    pub fn status(exe: &Path) -> Status {
        let dir = match dir_of(exe) {
            Ok(d) => d,
            Err(e) => return Status { state: "unsupported", path: String::new(), target: None, note: Some(e), replaceable: false },
        };
        let target = Some(exe.display().to_string().trim_start_matches(r"\\?\").to_string());
        match powershell(READ, &[]) {
            Ok(path) if path.split(';').any(|p| same(p, &dir)) => Status { state: "installed", path: dir, target, note: Some("Open a new terminal to use it.".into()), replaceable: false },
            Ok(_) => Status { state: "missing", path: dir, target, note: None, replaceable: false },
            Err(e) => Status { state: "unsupported", path: dir, target, note: Some(e), replaceable: false },
        }
    }

    pub fn install(exe: &Path) -> Result<(), String> {
        powershell(WRITE, &[("MUXEN_DIR", &dir_of(exe)?), ("MUXEN_ADD", "1")]).map(|_| ())
    }

    pub fn remove(exe: &Path) -> Result<(), String> {
        powershell(WRITE, &[("MUXEN_DIR", &dir_of(exe)?), ("MUXEN_ADD", "0")]).map(|_| ())
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    #[test]
    fn knows_its_own_links() {
        assert!(is_muxen(Path::new("/Applications/Muxen.app/Contents/MacOS/muxen")));
        assert!(is_muxen(Path::new("/home/me/.cargo/bin/muxen")));
        assert!(!is_muxen(Path::new("/usr/bin/python3")));
    }

    #[test]
    fn links_replaces_and_removes_only_its_own() {
        let tmp = std::env::temp_dir().join(format!("muxen-shell-command-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let exe = tmp.join("Muxen.app/Contents/MacOS/muxen");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "").unwrap();
        let exe = std::fs::canonicalize(&exe).unwrap();
        let link = tmp.join("bin/muxen");

        assert_eq!(imp::status_at(&link, &exe).state, "missing");
        imp::install_at(&link, &exe).unwrap();
        assert_eq!(imp::status_at(&link, &exe).state, "installed");
        // Again: replaces its own link.
        imp::install_at(&link, &exe).unwrap();
        assert_eq!(imp::status_at(&link, &exe).state, "installed");
        // A link to another copy is "other" and gets replaced.
        let old = tmp.join("old/muxen");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "").unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&old, &link).unwrap();
        assert_eq!((imp::status_at(&link, &exe).state, imp::status_at(&link, &exe).replaceable), ("other", true));
        imp::install_at(&link, &exe).unwrap();
        assert_eq!(imp::status_at(&link, &exe).state, "installed");
        imp::remove_at(&link).unwrap();
        assert_eq!(imp::status_at(&link, &exe).state, "missing");

        // Someone else's file, or a link to another program: left alone.
        std::fs::write(&link, "#!/bin/sh").unwrap();
        assert_eq!(imp::status_at(&link, &exe).state, "other");
        assert!(imp::install_at(&link, &exe).is_err());
        assert!(imp::remove_at(&link).is_err());
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/usr/bin/env", &link).unwrap();
        assert_eq!((imp::status_at(&link, &exe).state, imp::status_at(&link, &exe).replaceable), ("other", false));
        assert!(imp::install_at(&link, &exe).is_err());
        assert!(imp::remove_at(&link).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
