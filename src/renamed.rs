//! Muxen became Muxin (0.5.1). On a Mac the new Muxin.app lands next to the
//! old Muxen.app, so the first time an installed Muxin opens, the old app
//! goes to the Trash and a `muxen` command that ran it runs Muxin. Settings,
//! sessions and sign-ins carry over by themselves: the bundle id, folders and
//! Keychain items kept their names.

/// Retires the old app, when this is an installed Muxin (in Applications;
/// not a build, nor a copy running from the disk image).
#[cfg(target_os = "macos")]
pub fn retire_old_app() {
    use std::path::{Path, PathBuf};

    let Some(exe) = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()) else { return };
    let Some(app) = exe.ancestors().find(|a| a.extension().is_some_and(|e| e == "app")) else { return };
    let folders: Vec<PathBuf> = [Some(PathBuf::from("/Applications")), dirs::home_dir().map(|h| h.join("Applications"))].into_iter().flatten().collect();
    if !folders.iter().any(|f| app.parent() == Some(f.as_path())) {
        return;
    }
    let is_old = |old: &Path| old != app && std::fs::read_to_string(old.join("Contents/Info.plist")).is_ok_and(|p| p.contains("<string>com.lumarylabs.muxen</string>"));
    for old in folders.iter().map(|f| f.join("Muxen.app")).filter(|o| is_old(o)) {
        let Some(trash) = dirs::home_dir().map(|h| h.join(".Trash")) else { return };
        let mut to = trash.join("Muxen.app");
        if to.exists() {
            to = trash.join(format!("Muxen {}.app", chrono::Local::now().format("%H.%M.%S")));
        }
        match std::fs::rename(&old, &to) {
            Ok(()) => tracing::info!("moved {} to the Trash: it's Muxin now", old.display()),
            Err(e) => tracing::warn!("couldn't move {} to the Trash: {e}", old.display()),
        }
        // A `muxen` command that ran it runs this one now, where that needs no
        // password (else Settings › General › Command line offers it).
        let link = Path::new("/usr/local/bin/muxen");
        if std::fs::read_link(link).is_ok_and(|t| t.starts_with(&old)) && std::fs::remove_file(link).is_ok() {
            let _ = std::os::unix::fs::symlink(&exe, link);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn retire_old_app() {}
