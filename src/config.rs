//! On-disk configuration.
//!
//! - `fleet.toml` — the inventory: machines and the runners they host. Edited by hand.
//! - `settings.json` — app settings changed from the dashboard (GitHub orgs/repos, intervals).
//! - `state.json` — the last machine check, so a restart still shows "checked 42m ago".

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::probe::{ProbeResult, Shell};
use crate::util::slug;

#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
    pub inventory: PathBuf,
    pub settings: PathBuf,
    pub state: PathBuf,
    /// Agent workspaces, shared by every Muxen process (see fleet::workspaces).
    pub workspaces: PathBuf,
}

impl Paths {
    /// `$MUXEN_HOME`, else `$XDG_CONFIG_HOME/muxen`, else `~/.config/muxen`.
    pub fn resolve(dir: Option<PathBuf>, inventory: Option<PathBuf>) -> Paths {
        let dir = dir.or_else(|| std::env::var_os("MUXEN_HOME").map(PathBuf::from)).unwrap_or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".config")));
            match base {
                Some(base) => {
                    let dir = base.join("muxen");
                    // Muxen was called OpenRunner: bring its folder along, once.
                    let old = base.join("openrunner");
                    if !dir.exists() && old.is_dir() && std::fs::rename(&old, &dir).is_ok() {
                        tracing::info!("moved {} to {}", display_path(&old), display_path(&dir));
                    }
                    dir
                }
                None => PathBuf::from(".muxen"),
            }
        });
        Paths {
            inventory: inventory.unwrap_or_else(|| dir.join("fleet.toml")),
            settings: dir.join("settings.json"),
            state: dir.join("state.json"),
            workspaces: dir.join("workspaces.json"),
            dir,
        }
    }
}

/// Shortens paths under the home directory to `~/...` for display.
pub fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// Inventory (fleet.toml)

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryFile {
    #[serde(default, rename = "machine")]
    machines: Vec<MachineSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineSpec {
    pub name: String,
    /// Stable id used in URLs. Defaults to a slug of `name`.
    #[serde(default)]
    pub id: String,
    /// "local" for this computer, any `ssh` destination otherwise. Empty: listed but never checked.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Shown until the first successful check, e.g. "darwin-arm64".
    #[serde(default)]
    pub platform: Option<String>,
    /// "sh" or "powershell". Auto-detected when omitted.
    #[serde(default)]
    pub shell: Option<Shell>,
    /// Runner names expected on this machine. Missing ones are flagged.
    #[serde(default)]
    pub runners: Vec<String>,
    /// Extra glob patterns to look for runner folders (each containing a `.runner` file).
    #[serde(default)]
    pub runner_dirs: Vec<String>,
    /// Include runners found on the machine that aren't listed in `runners`.
    #[serde(default = "yes")]
    pub discover: bool,
    /// Let AI agents create workspaces on this machine.
    #[serde(default = "yes")]
    pub workspaces: bool,
    /// Where workspaces live on the machine. Default `~/muxen/workspaces`.
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// Extra arguments for `ssh`, e.g. ["-i", "~/.ssh/ci"].
    #[serde(default)]
    pub ssh_options: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

fn yes() -> bool {
    true
}

impl MachineSpec {
    pub fn is_local(&self) -> bool {
        matches!(self.host.trim().to_ascii_lowercase().as_str(), "local" | "localhost" | "127.0.0.1" | "::1")
    }

    pub fn is_checkable(&self) -> bool {
        !self.host.trim().is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub machines: Vec<MachineSpec>,
    pub warnings: Vec<String>,
}

impl Inventory {
    pub fn parse(text: &str) -> Result<Inventory> {
        let file: InventoryFile = toml::from_str(text).map_err(|e| anyhow::anyhow!("{}", e.to_string().trim()))?;
        let mut inv = Inventory { machines: Vec::new(), warnings: Vec::new() };
        let mut ids = HashSet::new();
        let mut runner_owner: HashMap<String, String> = HashMap::new();

        for mut m in file.machines {
            m.name = m.name.trim().to_string();
            if m.name.is_empty() {
                inv.warnings.push("A [[machine]] has an empty name and was skipped.".into());
                continue;
            }
            let id = assign_machine_id(&m.name, &m.id, &mut ids);
            if id != machine_id_base(&m.name, &m.id) {
                inv.warnings.push(format!("Two machines are called \"{}\"; the second one is shown as \"{id}\".", m.name));
            }
            m.id = id;
            m.runners.retain(|r| !r.trim().is_empty());
            for r in &m.runners {
                if let Some(other) = runner_owner.insert(r.to_ascii_lowercase(), m.name.clone()) {
                    if other != m.name {
                        inv.warnings.push(format!("Runner \"{r}\" is listed under both {other} and {}.", m.name));
                    }
                }
            }
            inv.machines.push(m);
        }
        Ok(inv)
    }

    pub fn load(path: &Path) -> Result<Inventory> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", display_path(path)))?;
        Inventory::parse(&text)
    }
}

fn machine_id_base(name: &str, explicit_id: &str) -> String {
    if explicit_id.trim().is_empty() { slug(name) } else { slug(explicit_id) }
}

/// A machine's id: its `id` (or `name`) slugged, with "-2", "-3"... appended to
/// repeats in file order. Shared with the fleet.toml editor so both agree.
pub fn assign_machine_id(name: &str, explicit_id: &str, taken: &mut HashSet<String>) -> String {
    let base = machine_id_base(name, explicit_id);
    let mut id = base.clone();
    let mut n = 2;
    while !taken.insert(id.clone()) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// The inventory written on first run: just this computer, with runners discovered.
pub fn default_inventory() -> String {
    let host = local_hostname();
    format!(
        r#"# Muxen fleet inventory
#
# Each [[machine]] is a computer that hosts self-hosted GitHub Actions runners,
# or one you just want to keep an eye on. Muxen checks machines over SSH
# (or directly when host = "local") and reads CPU, memory, disk, uptime and
# which runners are installed and running. Checks are read-only.
#
# Runners installed in the usual places (~/actions-runner*, ~/runners/*,
# /opt/actions-runner*, C:\actions-runner*) are discovered automatically.
# List them in `runners` so a missing one gets flagged.

[[machine]]
name = "{host}"
host = "local"
# runners = ["{runner}-01"]

# [[machine]]
# name = "Mac Studio"
# host = "ci@mac-studio.local"        # anything `ssh` accepts, including ~/.ssh/config aliases
# runners = ["mac-studio-01", "mac-studio-02"]
# runner_dirs = ["/Volumes/Fast/runners/*"]   # extra places to look for runners

# [[machine]]
# name = "Windows NUC"
# host = "nuc"
# shell = "powershell"                # auto-detected when omitted

# [[machine]]
# name = "StudioNAS"                  # no runners: listed under "Other machines"
# host = "nas"
"#,
        runner = slug(&host),
    )
}

pub fn local_hostname() -> String {
    let raw = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .or_else(|| std::process::Command::new("hostname").output().ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if raw.is_empty() { "This computer".into() } else { raw }
}

// ---------------------------------------------------------------------------
// Settings (settings.json)

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub github: GitHubSettings,
    pub github_poll_seconds: u32,
    /// 0 turns automatic machine checks off.
    pub check_interval_minutes: u32,
    /// Where to look for git checkouts on every machine: "dir" or "dir:depth".
    pub code_folders: Vec<String>,
    /// Repositories the user starred in Muxen ("owner/name"), in the order added.
    pub favorite_repos: Vec<String>,
}

/// Common places people keep code, a few levels deep, plus repos sitting right in the home folder.
pub fn default_code_folders() -> Vec<String> {
    ["~/Developer", "~/code", "~/Code", "~/src", "~/projects", "~/Projects", "~/repos", "~/git", "~/dev", "~/work", "~/workspace"]
        .iter()
        .map(|d| format!("{d}:4"))
        .chain(["~/muxen/workspaces:2".to_string(), "~:1".to_string()])
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GitHubSettings {
    pub orgs: Vec<String>,
    pub repos: Vec<String>,
    /// Recently pushed repos per org to scan for queued and running jobs.
    pub scan_repos: u32,
    pub api_url: String,
    /// Which of the GitHub CLI's accounts to use; empty follows its active account.
    pub account: String,
    /// Use the GitHub CLI login on this machine (by id) instead of this computer's.
    pub account_machine: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { github: GitHubSettings::default(), github_poll_seconds: 20, check_interval_minutes: 15, code_folders: default_code_folders(), favorite_repos: Vec::new() }
    }
}

impl Default for GitHubSettings {
    fn default() -> Self {
        GitHubSettings { orgs: Vec::new(), repos: Vec::new(), scan_repos: 10, api_url: "https://api.github.com".into(), account: String::new(), account_machine: String::new() }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(s) => s.normalized().unwrap_or_default(),
                Err(e) => {
                    tracing::warn!("ignoring {}: {e}", display_path(path));
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    /// Trims, de-duplicates and validates. Returns a message suitable for the UI on error.
    pub fn normalized(mut self) -> Result<Settings, String> {
        let mut seen = HashSet::new();
        let mut orgs = Vec::new();
        for raw in &self.github.orgs {
            let org = strip_github_url(raw);
            if org.is_empty() {
                continue;
            }
            if org.contains('/') || !valid_name(&org) {
                return Err(format!("\"{raw}\" isn't a valid organization name."));
            }
            if seen.insert(org.to_ascii_lowercase()) {
                orgs.push(org);
            }
        }
        let mut repos = Vec::new();
        for raw in &self.github.repos {
            let repo = strip_github_url(raw);
            if repo.is_empty() {
                continue;
            }
            match repo.split_once('/') {
                Some((owner, name)) if valid_name(owner) && valid_name(name) && !name.contains('/') => {}
                _ => return Err(format!("\"{raw}\" isn't a repository. Use owner/name.")),
            }
            if seen.insert(repo.to_ascii_lowercase()) {
                repos.push(repo);
            }
        }
        self.github.orgs = orgs;
        self.github.repos = repos;
        let mut seen = HashSet::new();
        self.favorite_repos = std::mem::take(&mut self.favorite_repos)
            .iter()
            .map(|r| strip_github_url(r))
            .filter(|r| r.split_once('/').is_some_and(|(o, n)| valid_name(o) && valid_name(n) && !n.contains('/')))
            .filter(|r| seen.insert(r.to_ascii_lowercase()))
            .collect();
        self.github.scan_repos = self.github.scan_repos.min(50);
        self.github.account = self.github.account.trim().trim_start_matches('@').to_string();
        let api = self.github.api_url.trim().trim_end_matches('/');
        self.github.api_url = if api.is_empty() { "https://api.github.com".into() } else { api.into() };
        if !self.github.api_url.starts_with("https://") && !self.github.api_url.starts_with("http://") {
            return Err("The GitHub API URL must start with https://".into());
        }
        self.github_poll_seconds = self.github_poll_seconds.clamp(10, 3600);
        self.check_interval_minutes = self.check_interval_minutes.min(24 * 60);
        Ok(self)
    }
}

/// Accepts "acme", "acme/api", "https://github.com/acme/api(.git)" and returns "acme" / "acme/api".
pub fn strip_github_url(raw: &str) -> String {
    let mut s = raw.trim();
    for prefix in ["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.split_once('/').map(|(_, path)| path).unwrap_or("");
        }
    }
    s.trim_matches('/').trim_end_matches(".git").to_string()
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ---------------------------------------------------------------------------
// Persisted state (state.json)

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PersistedState {
    pub probes: HashMap<String, ProbeResult>,
    pub check_started_at: Option<DateTime<Utc>>,
    pub check_finished_at: Option<DateTime<Utc>>,
}

impl PersistedState {
    pub fn load(path: &Path) -> PersistedState {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec(self)?)
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inventory_and_dedupes_ids() {
        let inv = Inventory::parse(
            r#"
            [[machine]]
            name = "Mac Studio"
            host = "studio"
            runners = ["mac-studio-01", ""]

            [[machine]]
            name = "Mac Studio"
            host = "local"
            "#,
        )
        .unwrap();
        assert_eq!(inv.machines[0].id, "mac-studio");
        assert_eq!(inv.machines[1].id, "mac-studio-2");
        assert_eq!(inv.machines[0].runners, vec!["mac-studio-01"]);
        assert!(inv.machines[1].is_local());
        assert!(inv.machines[0].discover);
        assert_eq!(inv.warnings.len(), 1);
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = Inventory::parse("[[machine]]\nname = \"a\"\nhots = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("hots"), "{err}");
    }

    #[test]
    fn default_inventory_parses() {
        let inv = Inventory::parse(&default_inventory()).unwrap();
        assert_eq!(inv.machines.len(), 1);
        assert!(inv.machines[0].is_local());
    }

    #[test]
    fn normalizes_settings() {
        let mut s = Settings::default();
        s.github.orgs = vec![" acme ".into(), "https://github.com/Acme".into(), "".into()];
        s.github.repos = vec!["https://github.com/acme/api.git".into()];
        s.github_poll_seconds = 1;
        let s = s.normalized().unwrap();
        assert_eq!(s.github.orgs, vec!["acme"]);
        assert_eq!(s.github.repos, vec!["acme/api"]);
        assert_eq!(s.github_poll_seconds, 10);

        let mut bad = Settings::default();
        bad.github.repos = vec!["just-a-name".into()];
        assert!(bad.normalized().is_err());
    }
}
