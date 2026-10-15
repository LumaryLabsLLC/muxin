//! Secrets Muxin keeps for the user (cloud tokens): in the macOS Keychain, in
//! Windows' Credential Manager, or elsewhere in a file only the user can
//! read, next to config.toml.

use std::path::{Path, PathBuf};

#[cfg(any(target_os = "macos", windows))]
// The app was called Muxen when these were first saved; the name stays so
// sign-ins made then keep working.
const SERVICE: &str = "Muxen";

/// Stores a secret under a name ("digitalocean-token").
pub fn set(dir: &Path, name: &str, value: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = dir;
        security_framework::passwords::set_generic_password(SERVICE, name, value.as_bytes()).map_err(|e| format!("Couldn't save to the Keychain: {e}"))
    }
    #[cfg(windows)]
    {
        let _ = dir;
        credentials::set(name, value).map_err(|e| format!("Couldn't save to Windows' Credential Manager: {e}"))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
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
    #[cfg(windows)]
    {
        let _ = dir;
        credentials::get(name).filter(|s| !s.is_empty())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
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
    #[cfg(windows)]
    {
        let _ = dir;
        credentials::delete(name);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let mut all = read_file(dir);
        if all.remove(name).is_some() {
            let _ = write_file(dir, &all);
        }
    }
}

/// Windows' Credential Manager: generic credentials named "Muxen/<name>",
/// for this user, kept on this computer.
#[cfg(windows)]
mod credentials {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Security::Credentials::*;

    fn target(name: &str) -> Vec<u16> {
        format!("{}/{name}", super::SERVICE).encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn set(name: &str, value: &str) -> Result<(), String> {
        let mut target = target(name);
        let mut user: Vec<u16> = "Muxen".encode_utf16().chain(std::iter::once(0)).collect();
        let mut blob = value.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: std::ptr::null_mut(),
            LastWritten: FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 },
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: user.as_mut_ptr(),
        };
        if unsafe { CredWriteW(&cred, 0) } != 0 { Ok(()) } else { Err(std::io::Error::last_os_error().to_string()) }
    }

    pub fn get(name: &str) -> Option<String> {
        let target = target(name);
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) } == 0 || found.is_null() {
            return None;
        }
        let value = unsafe {
            let c = &*found;
            let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
            String::from_utf8(bytes.to_vec()).ok()
        };
        unsafe { CredFree(found as *const std::ffi::c_void) };
        value
    }

    pub fn delete(name: &str) {
        let target = target(name);
        unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
    }
}

#[allow(dead_code)]
fn file(dir: &Path) -> PathBuf {
    dir.join("secrets.json")
}

#[cfg(not(any(target_os = "macos", windows)))]
fn read_file(dir: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(file(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

#[cfg(not(any(target_os = "macos", windows)))]
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
