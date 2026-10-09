//! Secrets Muxen keeps for the user (cloud tokens): in the macOS Keychain, or
//! elsewhere in a file only the user can read, next to config.toml.

use std::path::{Path, PathBuf};

const SERVICE: &str = "Muxen";

/// Stores a secret under a name ("digitalocean-token").
pub fn set(dir: &Path, name: &str, value: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = dir;
        security_framework::passwords::set_generic_password(SERVICE, name, value.as_bytes()).map_err(|e| format!("Couldn't save to the Keychain: {e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut all = read_file(dir);
        all.insert(name.to_string(), value.to_string());
        write_file(dir, &all)
    }
}

pub fn get(dir: &Path, name: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let _ = dir;
        security_framework::passwords::get_generic_password(SERVICE, name).ok().and_then(|b| String::from_utf8(b).ok()).filter(|s| !s.is_empty())
    }
    #[cfg(not(target_os = "macos"))]
    {
        read_file(dir).remove(name).filter(|s| !s.is_empty())
    }
}

pub fn delete(dir: &Path, name: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = dir;
        let _ = security_framework::passwords::delete_generic_password(SERVICE, name);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut all = read_file(dir);
        if all.remove(name).is_some() {
            let _ = write_file(dir, &all);
        }
    }
}

#[allow(dead_code)]
fn file(dir: &Path) -> PathBuf {
    dir.join("secrets.json")
}

#[cfg(not(target_os = "macos"))]
fn read_file(dir: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(file(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
fn write_file(dir: &Path, all: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    let path = file(dir);
    let bytes = serde_json::to_vec_pretty(all).map_err(|e| e.to_string())?;
    crate::config::write_atomic(&path, &bytes).map_err(|e| format!("Couldn't save {}: {e:#}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
