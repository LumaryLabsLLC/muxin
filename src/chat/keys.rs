//! The API keys of the providers the user connected (Settings › AI). A key
//! is kept where Muxin keeps its other secrets (crate::cloud::secrets: the
//! Keychain on a Mac, Credential Manager on Windows, elsewhere a file only
//! the user can read), never in config.toml, and never shown again once
//! it's saved. `[settings.chat.providers]` says which providers have one.
//!
//! `MUXIN_AI_KEYS` names a file to keep them in instead, for checking a
//! build that isn't signed: macOS asks the user each time such a build
//! reads the Keychain, and waits for the answer before anything else.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn name(provider: &str) -> String {
    format!("ai-{provider}")
}

/// Where a Muxin's keys are kept.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    /// With Muxin's other secrets, for the Muxin whose folder this is.
    Secrets(PathBuf),
    /// In a file only the user can read.
    File(PathBuf),
}

impl Place {
    /// For the Muxin whose folder is `dir`. The simulated machines (`demo`)
    /// keep theirs in a file of their own: a key tried there is never the
    /// user's real one in the Keychain.
    pub fn of(dir: &Path, demo: bool) -> Place {
        match std::env::var_os("MUXIN_AI_KEYS").filter(|p| !p.is_empty()) {
            Some(path) => Place::File(path.into()),
            None if demo => Place::File(dir.join("demo").join("ai-keys.json")),
            None => Place::Secrets(dir.to_path_buf()),
        }
    }
}

fn read(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write(path: &Path, keys: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(folder) = path.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    crate::config::write_atomic(path, serde_json::to_string(keys).unwrap_or_default().as_bytes()).map_err(|e| format!("Couldn't save the key: {e:#}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The key kept for `provider`.
pub fn get(place: &Place, provider: &str) -> Option<String> {
    let key = match place {
        Place::File(path) => read(path).remove(provider),
        Place::Secrets(dir) => crate::cloud::secrets::get(dir, &name(provider)),
    };
    key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

/// Keeps `key` for `provider`.
pub fn set(place: &Place, provider: &str, key: &str) -> Result<(), String> {
    match place {
        Place::File(path) => {
            let mut keys = read(path);
            keys.insert(provider.to_string(), key.trim().to_string());
            write(path, &keys)
        }
        Place::Secrets(dir) => crate::cloud::secrets::set(dir, &name(provider), key.trim()),
    }
}

/// Forgets `provider`'s key.
pub fn forget(place: &Place, provider: &str) {
    match place {
        Place::File(path) => {
            let mut keys = read(path);
            if keys.remove(provider).is_some() {
                let _ = write(path, &keys);
            }
        }
        Place::Secrets(dir) => crate::cloud::secrets::delete(dir, &name(provider)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_in_a_file_are_kept_by_provider_and_forgotten_one_at_a_time() {
        let file = std::env::temp_dir().join(format!("muxin-keys-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let place = Place::File(file.clone());
        assert_eq!(get(&place, "anthropic"), None);
        set(&place, "anthropic", " sk-ant-1 \n").unwrap();
        set(&place, "openai", "sk-2").unwrap();
        assert_eq!((get(&place, "anthropic").as_deref(), get(&place, "openai").as_deref()), (Some("sk-ant-1"), Some("sk-2")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        forget(&place, "anthropic");
        assert_eq!((get(&place, "anthropic"), get(&place, "openai").as_deref()), (None, Some("sk-2")));
        // The simulated machines never keep theirs with the user's real ones.
        let dir = std::path::Path::new("/tmp/muxin-home");
        if std::env::var_os("MUXIN_AI_KEYS").is_none() {
            assert_eq!(Place::of(dir, true), Place::File(dir.join("demo/ai-keys.json")));
            assert_eq!(Place::of(dir, false), Place::Secrets(dir.to_path_buf()));
        }
    }
}
