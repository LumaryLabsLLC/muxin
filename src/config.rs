//! On-disk configuration, in one folder (`~/.config/muxin`, `%APPDATA%\muxin`):
//!
//! - `config.toml` — everything the user sets: `[settings]` (GitHub, checks,
//!   cloud), `[ui]` (the app's look, fonts, shortcuts) and the `[[machine]]`
//!   list. The app edits each part in place, so comments on machines survive.
//! - `state.json` — the last machine check, so a restart still shows "checked 42m ago".
//! - Internal state: pairing keys (`direct.json`, `identity`), `workspaces.json`.

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
    /// config.toml.
    pub config: PathBuf,
    /// Where the machines are read from: config.toml, unless `-f` names another file.
    pub inventory: PathBuf,
    /// Where settings are kept: config.toml.
    pub settings: PathBuf,
    pub state: PathBuf,
    /// Agent workspaces, shared by every Muxin process (see fleet::workspaces).
    pub workspaces: PathBuf,
    /// Cloud machines destroyed lately (see DestroyedCloud). Its own file, so
    /// the processes that write state.json don't save over each other's.
    pub cloud_history: PathBuf,
}

impl Paths {
    /// `$MUXIN_HOME` (or `$MUXEN_HOME`), else the platform's config folder:
    /// `~/.config/muxin` ($XDG_CONFIG_HOME on Linux), `%APPDATA%\muxin` on Windows.
    pub fn resolve(dir: Option<PathBuf>, inventory: Option<PathBuf>) -> Paths {
        let base = config_base();
        let chosen = dir.or_else(home_from_env);
        // Only the default folder takes over the old default folder's files; a
        // folder that was asked for keeps to itself.
        let dir = chosen.clone().unwrap_or_else(|| default_dir(base.as_deref()));
        migrate(&dir, if chosen.is_some() { None } else { base.as_deref() });
        let config = dir.join("config.toml");
        Paths {
            inventory: inventory.unwrap_or_else(|| config.clone()),
            settings: config.clone(),
            config,
            state: dir.join("state.json"),
            workspaces: dir.join("workspaces.json"),
            cloud_history: dir.join("cloud-history.json"),
            dir,
        }
    }
}

/// The folder `Paths::resolve` picks without `--config-dir`, and without
/// moving old files into it: for the crash log, set up before the arguments are read.
pub fn dir_from_env() -> PathBuf {
    home_from_env().unwrap_or_else(|| default_dir(config_base().as_deref()))
}

fn home_from_env() -> Option<PathBuf> {
    std::env::var_os("MUXIN_HOME").map(PathBuf::from).or_else(|| std::env::var_os("MUXEN_HOME").map(PathBuf::from))
}

fn default_dir(base: Option<&Path>) -> PathBuf {
    base.map(|b| b.join("muxin")).unwrap_or_else(|| PathBuf::from(".muxin"))
}

/// Where per-user config folders go on this platform.
fn config_base() -> Option<PathBuf> {
    if cfg!(windows) {
        dirs::config_dir()
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".config")))
    }
}

/// Muxin used to keep `fleet.toml`, `settings.json` and `ui.json` in
/// `~/.config/muxen` (and before that `~/.config/openrunner`). The first time,
/// they become one config.toml in `dir`, and the rest of the old folder (pairing
/// keys, workspaces) is copied along. The old folder is left as it was.
fn migrate(dir: &Path, base: Option<&Path>) {
    let config = dir.join("config.toml");
    if config.exists() {
        return;
    }
    const OLD_FILES: [&str; 3] = ["fleet.toml", "settings.json", "ui.json"];
    let candidates = [Some(dir.to_path_buf()), base.map(|b| b.join("muxen")), base.map(|b| b.join("openrunner"))];
    let Some(old) = candidates.into_iter().flatten().find(|d| OLD_FILES.iter().any(|f| d.join(f).exists())) else { return };
    if create_private_dir(dir).is_err() {
        return;
    }
    if old != dir {
        copy_dir(&old, dir, &OLD_FILES);
    }
    let mut text = std::fs::read_to_string(old.join("fleet.toml")).unwrap_or_default();
    for (file, key, camel) in [("settings.json", "settings", true), ("ui.json", "ui", false)] {
        let Some(value) = std::fs::read_to_string(old.join(file)).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else { continue };
        let value = json_for_toml(value, camel);
        match toml::Value::try_from(&value).map_err(|e| e.to_string()).and_then(|v| set_section(&text, key, &v)) {
            Ok(t) => text = t,
            Err(e) => tracing::warn!("couldn't bring {} into config.toml: {e}", display_path(&old.join(file))),
        }
    }
    match write_atomic(&config, text.as_bytes()) {
        Ok(()) => tracing::info!("moved Muxin's settings from {} into {}", display_path(&old), display_path(&config)),
        Err(e) => tracing::warn!("couldn't write {}: {e:#}", display_path(&config)),
    }
}

/// Copies a folder's contents (but `skip`) into another, recursively.
fn copy_dir(from: &Path, to: &Path, skip: &[&str]) {
    let Ok(entries) = std::fs::read_dir(from) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        if skip.iter().any(|s| name == *s) {
            continue;
        }
        let (src, dst) = (e.path(), to.join(&name));
        if src.is_dir() {
            if std::fs::create_dir_all(&dst).is_ok() {
                copy_dir(&src, &dst, &[]);
            }
        } else if !dst.exists() {
            let _ = std::fs::copy(&src, &dst);
        }
    }
}

/// JSON settings as TOML can hold them: no nulls, and (`camel`) keys in snake_case.
fn json_for_toml(v: serde_json::Value, camel: bool) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (if camel { snake_case(&k) } else { k }, json_for_toml(v, camel)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().filter(|v| !v.is_null()).map(|v| json_for_toml(v, camel)).collect()),
        other => other,
    }
}

fn snake_case(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// config.toml sections

const CONFIG_HEADER: &str = "# Muxin: settings, look and machines. The app writes [settings] and [ui];\n# change them here or in Settings. Comments on machines are kept.\n\n";

/// Reads one section of config.toml (`[settings]`, `[ui]`), defaults if it's
/// missing or doesn't read.
pub fn read_section<T: serde::de::DeserializeOwned + Default>(path: &Path, key: &str) -> T {
    let Ok(text) = std::fs::read_to_string(path) else { return T::default() };
    let table = match text.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("ignoring {}: {}", display_path(path), e.to_string().trim());
            return T::default();
        }
    };
    match table.get(key).cloned().map(|v| v.try_into::<T>()) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            tracing::warn!("ignoring [{key}] in {}: {}", display_path(path), e.to_string().trim());
            T::default()
        }
        None => T::default(),
    }
}

/// Writes one section of config.toml in place; the rest of the file stays as it is.
pub fn write_section<T: Serialize>(path: &Path, key: &str, value: &T) -> Result<(), String> {
    crate::ops::inventory_edit::edit_file(path, |text| set_section(text, key, value))
}

/// `text` with `[key]` replaced by `value`: where it was, or (new) at the top,
/// above the machines.
pub fn set_section<T: Serialize>(text: &str, key: &str, value: &T) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| format!("config.toml doesn't read: {}", e.to_string().trim()))?;
    let rendered = toml::to_string(value).map_err(|e| e.to_string())?;
    let fresh: toml_edit::DocumentMut = rendered.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    let mut table = fresh.as_table().clone();
    table.set_implicit(false);
    doc.insert(key, toml_edit::Item::Table(table));
    // Fixed places above the machines: [settings], then [ui], then others.
    // Reading the file renumbers places, so every section gets its own again.
    fn place(t: &mut toml_edit::Table, position: isize) {
        t.set_position(Some(position));
        for (_, item) in t.iter_mut() {
            if let Some(sub) = item.as_table_mut() {
                place(sub, position);
            }
        }
    }
    for (k, position) in [("settings", -30), ("ui", -20)] {
        if let Some(t) = doc.get_mut(k).and_then(|i| i.as_table_mut()) {
            place(t, position);
        }
    }
    if !matches!(key, "settings" | "ui") {
        if let Some(t) = doc.get_mut(key).and_then(|i| i.as_table_mut()) {
            place(t, -10);
        }
    }
    // The header goes on whichever section comes first; the others start after a blank line.
    let first = ["settings", "ui"].into_iter().find(|k| doc.contains_key(k));
    for k in ["settings", "ui"] {
        if let Some(t) = doc.get_mut(k).and_then(|i| i.as_table_mut()) {
            t.decor_mut().set_prefix(if Some(k) == first { CONFIG_HEADER } else { "\n" });
        }
    }
    // A blank line between the sections and the first machine.
    if let Some(first) = doc.get_mut("machine").and_then(|m| m.as_array_of_tables_mut()).and_then(|a| a.get_mut(0)) {
        let p = first.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
        if !p.starts_with('\n') {
            first.decor_mut().set_prefix(format!("\n{p}"));
        }
    }
    let out = doc.to_string();
    Inventory::parse(&out).map_err(|e| format!("config.toml would be invalid: {e}"))?;
    Ok(out)
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
// Inventory (config.toml)

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryFile {
    #[serde(default, rename = "machine")]
    machines: Vec<MachineSpec>,
    /// The other parts of config.toml (read elsewhere).
    #[serde(default)]
    #[allow(dead_code)]
    settings: Option<toml::Table>,
    #[serde(default)]
    #[allow(dead_code)]
    ui: Option<toml::Table>,
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
    /// A cloud machine Muxin created (and can power off or destroy).
    #[serde(default)]
    pub cloud: Option<CloudRef>,
    /// A VM (or Linux container) Muxin created in Docker on another of the
    /// user's machines (and can start, stop or destroy).
    #[serde(default)]
    pub vm: Option<VmRef>,
    /// Disconnected on purpose: Muxin doesn't connect to it, check it or look
    /// at its sessions until it's connected again (its sessions go on there).
    #[serde(default)]
    pub disconnected: bool,
    /// Where a direct connection reaches it ("host:port"), when its own
    /// addresses don't: a VM behind its host's NAT, a router's forwarded port.
    #[serde(default)]
    pub direct_address: Option<String>,
}

/// Where a cloud machine lives, as config.toml records it:
/// `cloud = { provider = "digitalocean", id = 123, region = "nyc3", size = "s-2vcpu-4gb", price_hourly = 0.03571, created_at = "..." }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CloudRef {
    pub provider: String,
    pub id: u64,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub size: String,
    /// What it costs per hour while it exists (powered off too), in USD.
    #[serde(default)]
    pub price_hourly: f64,
    /// RFC 3339.
    #[serde(default)]
    pub created_at: String,
}

/// Where a VM Muxin made runs, as config.toml records it:
/// `vm = { host = "beelink", os = "windows", version = "11", container = "muxen-win-dev", created_at = "..." }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VmRef {
    /// The machine it runs on (its id).
    pub host: String,
    /// "windows" | "linux"
    pub os: String,
    /// What it runs: "11", "10", "2025" (Windows), "ubuntu-24.04", "debian-12", ... (Linux).
    #[serde(default)]
    pub version: String,
    /// Its Docker container there.
    pub container: String,
    /// Its screen in a browser (Windows: the installer's viewer), on the host's 127.0.0.1.
    #[serde(default)]
    pub web_port: Option<u16>,
    /// RFC 3339.
    #[serde(default)]
    pub created_at: String,
}

/// Cloud machines destroyed lately, from cloud-history.json.
pub fn read_cloud_history(path: &Path) -> Vec<DestroyedCloud> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Adds one (replacing an earlier entry for the same droplet) and forgets
/// those destroyed more than about a year ago.
pub fn add_cloud_history(path: &Path, entry: DestroyedCloud) -> Result<()> {
    let mut all = read_cloud_history(path);
    let keep_from = entry.destroyed_at - chrono::Duration::days(400);
    all.retain(|e| e.destroyed_at > keep_from && e.cloud.id != entry.cloud.id);
    all.push(entry);
    write_atomic(path, serde_json::to_string_pretty(&all)?.as_bytes())
}

/// A cloud machine Muxin destroyed, kept so this month's costs still count it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestroyedCloud {
    pub name: String,
    pub cloud: CloudRef,
    pub destroyed_at: DateTime<Utc>,
}

fn yes() -> bool {
    true
}

impl MachineSpec {
    pub fn is_local(&self) -> bool {
        match self.host.trim().to_ascii_lowercase().as_str() {
            "local" => true,
            // A loopback address with a port, user or ssh options of its own
            // is something else reached through a forwarded port (a VM's), not
            // this computer.
            "localhost" | "127.0.0.1" | "::1" => self.port.is_none() && self.user.is_none() && self.ssh_options.is_empty(),
            _ => false,
        }
    }

    pub fn is_checkable(&self) -> bool {
        !self.host.trim().is_empty()
    }

    /// Muxin may reach it: it has a host and isn't disconnected.
    pub fn is_connected(&self) -> bool {
        self.is_checkable() && !self.disconnected
    }

    /// Err for a machine disconnected on purpose: what the user sees instead
    /// of Muxin reaching it.
    pub fn refuse_if_disconnected(&self) -> Result<(), String> {
        if self.disconnected {
            return Err(format!("{} is disconnected. Connect to it first.", self.name));
        }
        Ok(())
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
/// repeats in file order. Shared with the config.toml editor so both agree.
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
        r#"# Muxin: your settings and machines. Settings and the app's look go in
# [settings] and [ui], which Muxin adds when you change them.
#
# Each [[machine]] is a computer Muxin connects to: for agent sessions, its
# desktop, files, workspaces, or the GitHub Actions runners on it. Muxin
# checks machines over SSH (or directly when host = "local") and reads CPU,
# memory, disk, uptime and which runners are installed. Checks are read-only.
#
# Runners installed in the usual places (~/actions-runner*, ~/runners/*,
# /opt/actions-runner*, C:\actions-runner*) are discovered automatically.
# List them in `runners` so a missing one gets flagged.

[[machine]]
name = "{host}"
host = "local"
# runners = ["{runner}-01"]

# [[machine]]
# name = "Forge"
# host = "ci@forge.local"             # anything `ssh` accepts, including ~/.ssh/config aliases
# runners = ["forge-01", "forge-02"]
# runner_dirs = ["/Volumes/Fast/runners/*"]   # extra places to look for runners

# [[machine]]
# name = "Beacon"
# host = "deploy@beacon"
# id = "beacon"                       # keeps its id (and history) if you rename it

# [[machine]]
# name = "Vault"                      # no runners: listed under "Other machines"
# host = "vault"
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
// Settings ([settings] in config.toml)

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub github: GitHubSettings,
    pub github_poll_seconds: u32,
    /// 0 turns automatic machine checks off.
    pub check_interval_minutes: u32,
    /// Where to look for git checkouts on every machine: "dir" or "dir:depth".
    pub code_folders: Vec<String>,
    /// Repositories the user starred in Muxin ("owner/name"), in the order added.
    pub favorite_repos: Vec<String>,
    /// Text copied here can be pasted on the machines (Linux desktops, Macs), and back.
    /// Off until the user turns it on (a `true` saved before stays on).
    pub share_clipboard: bool,
    /// Ask GitHub now and then whether a newer Muxin is out (see crate::update).
    pub check_updates: bool,
    /// The editor machines' folders open in: "zed", "cursor", "vscode", or "" for the first one installed.
    pub editor: String,
    /// Cloud machines: how Muxin signs in, what new ones come with, what happens when they sit idle.
    pub cloud: CloudSettings,
    /// Session actions' prompts the user changed, by action ("commit", "review", ...;
    /// see fleet::sessions::actions). The others use Muxin's own.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub prompts: std::collections::BTreeMap<String, String>,
    /// Sharing sessions by link (see crate::share).
    pub share: ShareSettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ShareSettings {
    /// The share server links go through; empty for Muxin's.
    pub server: String,
    /// The name you join others' shares with.
    pub name: String,
}

impl Settings {
    /// The share server new links use.
    pub fn share_server(&self) -> String {
        let s = self.share.server.trim();
        if s.is_empty() { crate::share::default_server() } else { s.to_string() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CloudSettings {
    /// How Muxin signs in to DigitalOcean: "token" (kept in the Keychain), "oauth" (browser sign-in) or "doctl" (its CLI's login).
    pub digitalocean_auth: String,
    /// Defaults for new machines; empty picks for you.
    pub region: String,
    pub size: String,
    /// What a new machine comes with, by id: tools, languages, agent CLIs,
    /// Tailscale (see cloud::setup::EXTRAS).
    pub install: Vec<String>,
    /// When a cloud machine has been idle `idle_hours`: "warn", "poweroff" or "destroy" ("off" does nothing).
    pub idle_action: String,
    pub idle_hours: u32,
}

impl Default for CloudSettings {
    fn default() -> Self {
        CloudSettings {
            digitalocean_auth: "token".into(),
            region: String::new(),
            size: "s-2vcpu-4gb".into(),
            install: crate::cloud::setup::default_extras(),
            idle_action: "warn".into(),
            idle_hours: 3,
        }
    }
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
#[serde(default)]
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
        Settings {
            github: GitHubSettings::default(),
            github_poll_seconds: 20,
            check_interval_minutes: 15,
            code_folders: default_code_folders(),
            favorite_repos: Vec::new(),
            share_clipboard: false,
            check_updates: true,
            editor: String::new(),
            cloud: CloudSettings::default(),
            prompts: Default::default(),
            share: ShareSettings::default(),
        }
    }
}

impl Default for GitHubSettings {
    fn default() -> Self {
        GitHubSettings { orgs: Vec::new(), repos: Vec::new(), scan_repos: 10, api_url: "https://api.github.com".into(), account: String::new(), account_machine: String::new() }
    }
}

impl Settings {
    /// `[settings]` in config.toml.
    pub fn load(path: &Path) -> Settings {
        let mut settings = read_section::<Settings>(path, "settings");
        // An http:// API URL saved before those were refused is asked over https
        // (the token never goes in the clear), rather than dropping every setting.
        if let Some(rest) = settings.github.api_url.trim().strip_prefix("http://") {
            settings.github.api_url = format!("https://{rest}");
        }
        settings.normalized().unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_section(path, "settings", self).map_err(|e| anyhow::anyhow!(e))
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
        // The token goes along with every request: never in the clear.
        if !self.github.api_url.starts_with("https://") {
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
    /// Machines on the user's tailnet, as Tailscale last saw them (kept for
    /// when Tailscale is off).
    pub tailnet: HashMap<String, crate::fleet::model::TailnetDevice>,
    /// Cloud machines' power ("active", "off", ...) as the provider last
    /// reported it, by machine id.
    pub droplets: HashMap<String, String>,
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

/// Writes a whole file (a temporary one, then renamed), readable by this user
/// only from the start: some hold secrets (secrets.json). On Windows the
/// user's own folders already are.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        create_private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    // One left over from before keeps its old permissions: tighten them first.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600)).with_context(|| format!("writing {}", tmp.display()))?;
    }
    file.write_all(bytes).with_context(|| format!("writing {}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Makes a folder that only this user can open (the folders above it, when
/// missing, as usual). One that's already there keeps its permissions; on
/// Windows the user's own folders already are private.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let made = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    };
    #[cfg(not(unix))]
    let made = std::fs::create_dir(dir);
    match made {
        // Made in the meantime (another Muxin process).
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn cloud_history_keeps_one_entry_per_droplet() {
        let dir = std::env::temp_dir().join(format!("muxen-cloud-history-{}", std::process::id()));
        let path = dir.join("cloud-history.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entry = |id: u64, at: &str| DestroyedCloud {
            name: format!("dev-{id}"),
            cloud: CloudRef { provider: "digitalocean".into(), id, region: "nyc3".into(), size: "s-1vcpu-1gb".into(), price_hourly: 0.00893, created_at: "2026-10-09T18:53:17Z".into() },
            destroyed_at: DateTime::parse_from_rfc3339(at).unwrap().with_timezone(&Utc),
        };
        assert!(read_cloud_history(&path).is_empty());
        add_cloud_history(&path, entry(1, "2025-01-01T00:00:00Z")).unwrap();
        add_cloud_history(&path, entry(2, "2026-10-09T21:22:19Z")).unwrap();
        add_cloud_history(&path, entry(2, "2026-10-09T21:22:20Z")).unwrap();
        // Over a year old is gone; the same droplet twice is one entry.
        let all = read_cloud_history(&path);
        assert_eq!(all.iter().map(|e| e.cloud.id).collect::<Vec<_>>(), vec![2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_files_become_one_config_toml() {
        let dir = std::env::temp_dir().join(format!("muxin-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fleet.toml"), "# my machines\n\n[[machine]]\nname = \"Box\" # the NUC\nhost = \"box\"\n").unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"github":{"orgs":["acme"],"repos":[],"scanRepos":7,"apiUrl":"https://api.github.com","account":"","accountMachine":""},"githubPollSeconds":30,"checkIntervalMinutes":5,"shareClipboard":false}"#).unwrap();
        std::fs::write(dir.join("ui.json"), r#"{"mode":"dark","icon_style":"tiles","shortcuts":{"jobs":"ctrl-j"},"desktops":[],"path":null}"#).unwrap();
        migrate(&dir, None);

        let config = dir.join("config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.find("[settings]").unwrap() < text.find("[[machine]]").unwrap(), "settings come first:\n{text}");
        assert!(text.contains("# the NUC"), "comments on machines are kept:\n{text}");
        let s = Settings::load(&config);
        assert_eq!((s.github.orgs.clone(), s.github.scan_repos, s.github_poll_seconds, s.check_interval_minutes, s.share_clipboard), (vec!["acme".to_string()], 7, 30, 5, false));
        let ui: toml::Table = read_section(&config, "ui");
        assert_eq!(ui.get("icon_style").and_then(|v| v.as_str()), Some("tiles"));
        assert_eq!(Inventory::load(&config).unwrap().machines[0].name, "Box");

        // Saving settings again changes only [settings].
        let mut s2 = s.clone();
        s2.check_interval_minutes = 9;
        s2.save(&config).unwrap();
        let text2 = std::fs::read_to_string(&config).unwrap();
        assert!(text2.contains("# the NUC") && text2.contains("icon_style"));
        assert_eq!(Settings::load(&config).check_interval_minutes, 9);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn a_forwarded_loopback_port_is_another_machine() {
        let inv = Inventory::parse("[[machine]]\nname = \"Here\"\nhost = \"127.0.0.1\"\n\n[[machine]]\nname = \"VM\"\nhost = \"127.0.0.1\"\nport = 2201\nssh_options = [\"-J\", \"beelink\"]\n").unwrap();
        assert!(inv.machines[0].is_local());
        assert!(!inv.machines[1].is_local());
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

    #[test]
    fn the_github_api_is_https_only() {
        let api = |url: &str| {
            let mut s = Settings::default();
            s.github.api_url = url.into();
            s.normalized().map(|s| s.github.api_url)
        };
        assert_eq!(api(" https://ghe.example.com/api/v3/ "), Ok("https://ghe.example.com/api/v3".into()));
        assert_eq!(api(""), Ok("https://api.github.com".into()));
        assert!(api("http://ghe.example.com/api/v3").is_err());
        assert!(api("ghe.example.com").is_err());

        // One saved before http:// was refused keeps the rest of the settings.
        let path = std::env::temp_dir().join(format!("muxin-http-api-{}.toml", std::process::id()));
        std::fs::write(&path, "[settings]\ngithub_poll_seconds = 45\nshare_clipboard = true\n\n[settings.github]\napi_url = \"http://ghe.example.com/api/v3\"\n").unwrap();
        let s = Settings::load(&path);
        assert_eq!(s.github.api_url, "https://ghe.example.com/api/v3");
        assert_eq!((s.github_poll_seconds, s.share_clipboard), (45, true));
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("muxin-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = root.join("muxin");
        let secrets = dir.join("secrets.json");
        write_atomic(&secrets, b"{}").unwrap();
        assert_eq!((mode(&dir), mode(&secrets)), (0o700, 0o600));
        // A temporary file left over from before, readable by anyone, isn't.
        std::fs::write(dir.join("secrets.tmp"), b"old").unwrap();
        std::fs::set_permissions(dir.join("secrets.tmp"), std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&secrets, b"{\"token\":\"x\"}").unwrap();
        assert_eq!(mode(&secrets), 0o600);
        // A folder that was already there keeps its permissions.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_atomic(&dir.join("state.json"), b"{}").unwrap();
        assert_eq!(mode(&dir), 0o755);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_clipboard_is_shared_only_once_turned_on() {
        assert!(!Settings::default().share_clipboard);
        let read = |text: &str| toml::from_str::<Settings>(text).unwrap().share_clipboard;
        assert!(!read("github_poll_seconds = 30\n"));
        // Turned on before it was off by default: it stays on.
        assert!(read("share_clipboard = true\n"));
    }
}
