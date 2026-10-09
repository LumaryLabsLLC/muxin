//! Tailscale on this computer: which of the user's machines are on their
//! tailnet, at what addresses, whether Tailscale sees them online, and how
//! traffic gets to them. Read-only: it runs `tailscale status --json` and
//! never changes Tailscale's settings.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;

/// How long one reading of `tailscale status` stays good.
const FRESH: Duration = Duration::from_secs(4);

/// What Tailscale on this computer knows.
#[derive(Debug, Clone, Default)]
pub struct Tailnet {
    /// Tailscale's own state: "Running", "Stopped", "NeedsLogin", "Starting", ...
    pub state: String,
    /// The tailnet's name, e.g. "you@example.com" or "example.org".
    pub name: Option<String>,
    pub peers: Vec<Peer>,
}

/// Another device on the tailnet.
#[derive(Debug, Clone, Default)]
pub struct Peer {
    pub host_name: String,
    /// Its MagicDNS name, without the trailing dot ("box.tail1234.ts.net").
    pub dns_name: String,
    /// "linux", "macOS", "windows", "iOS", "android", ...
    pub os: String,
    pub ips: Vec<IpAddr>,
    pub online: bool,
    /// When Tailscale last heard from it (only meaningful while it's offline).
    pub last_seen: Option<DateTime<Utc>>,
    /// Where traffic goes when it's sent straight to the device (peer-to-peer).
    pub direct_addr: Option<String>,
    /// The Tailscale relay (DERP region code, e.g. "mia") used otherwise.
    pub relay: Option<String>,
    /// Traffic went to it recently.
    pub active: bool,
}

impl Tailnet {
    pub fn running(&self) -> bool {
        self.state == "Running"
    }

    /// The device that is this machine: by the addresses its agent reported
    /// (best), else by a name or address the user knows it by.
    pub fn find(&self, ips: &[String], names: &[String]) -> Option<&Peer> {
        let ips: Vec<IpAddr> = ips.iter().filter_map(|i| i.parse().ok()).collect();
        if let Some(p) = self.peers.iter().find(|p| p.ips.iter().any(|i| ips.contains(i))) {
            return Some(p);
        }
        for raw in names {
            let name = raw.rsplit('@').next().unwrap_or(raw).trim().trim_end_matches('.').to_ascii_lowercase();
            // Phones call themselves "localhost".
            if name.is_empty() || name == "localhost" {
                continue;
            }
            if let Ok(ip) = name.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>() {
                if let Some(p) = self.peers.iter().find(|p| p.ips.contains(&ip)) {
                    return Some(p);
                }
                continue;
            }
            let found = self.peers.iter().find(|p| {
                let dns = p.dns_name.to_ascii_lowercase();
                dns == name || p.short_name().eq_ignore_ascii_case(&name) || p.host_name.eq_ignore_ascii_case(&name)
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }
}

impl Peer {
    /// The first part of its MagicDNS name ("box"), else its host name.
    pub fn short_name(&self) -> &str {
        self.dns_name.split('.').next().filter(|s| !s.is_empty()).unwrap_or(&self.host_name)
    }

    /// Whether you can ssh to it (phones and tablets: no).
    pub fn is_computer(&self) -> bool {
        !matches!(self.os.to_ascii_lowercase().as_str(), "ios" | "android" | "tvos")
    }

    /// "direct" (peer-to-peer), "relay" (through a Tailscale relay), or
    /// "idle" (no traffic lately, so Tailscale hasn't picked a path).
    pub fn path(&self) -> &'static str {
        if self.direct_addr.is_some() {
            "direct"
        } else if self.active && self.relay.is_some() {
            "relay"
        } else {
            "idle"
        }
    }
}

/// Reads `tailscale status --json` output (anything before the JSON, like a
/// version warning, is skipped).
pub fn parse(output: &str) -> Option<Tailnet> {
    let json: Value = serde_json::from_str(&output[output.find('{')?..]).ok()?;
    let ips = |v: &Value| -> Vec<IpAddr> { v.as_array().map(|a| a.iter().filter_map(|i| i.as_str()?.parse().ok()).collect()).unwrap_or_default() };
    let text = |v: &Value| v.as_str().map(str::to_string).filter(|s| !s.is_empty());
    let peers = json["Peer"]
        .as_object()
        .map(|peers| {
            peers
                .values()
                .map(|p| Peer {
                    host_name: p["HostName"].as_str().unwrap_or("").to_string(),
                    dns_name: p["DNSName"].as_str().unwrap_or("").trim_end_matches('.').to_string(),
                    os: p["OS"].as_str().unwrap_or("").to_string(),
                    ips: ips(&p["TailscaleIPs"]),
                    online: p["Online"].as_bool().unwrap_or(false),
                    // Go's zero time means "never" (and online devices have it).
                    last_seen: p["LastSeen"].as_str().and_then(|t| t.parse::<DateTime<Utc>>().ok()).filter(|t| t.timestamp() > 0),
                    direct_addr: text(&p["CurAddr"]),
                    relay: text(&p["Relay"]),
                    active: p["Active"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Tailnet {
        state: json["BackendState"].as_str().unwrap_or("").to_string(),
        name: text(&json["CurrentTailnet"]["Name"]),
        peers,
    })
}

/// The Tailscale command-line tool, if Tailscale is installed. On macOS the
/// one inside Tailscale.app comes first: it always matches the running app
/// (a Homebrew one may not, and says so on every call).
fn cli() -> Option<&'static PathBuf> {
    static CLI: OnceLock<Option<PathBuf>> = OnceLock::new();
    CLI.get_or_init(|| {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if cfg!(target_os = "macos") {
            candidates.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
        }
        if cfg!(windows) {
            candidates.push(r"C:\Program Files\Tailscale\tailscale.exe".into());
        }
        let exe = if cfg!(windows) { "tailscale.exe" } else { "tailscale" };
        let path = std::env::var_os("PATH").unwrap_or_default();
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
        // Apps started from the Dock don't get the shell's PATH.
        candidates.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"].iter().map(|d| PathBuf::from(d).join(exe)));
        candidates.into_iter().find(|c| c.is_file())
    })
    .as_ref()
}

/// Whether Tailscale is installed here.
pub fn installed() -> bool {
    cli().is_some()
}

fn cache() -> &'static Mutex<Option<(Instant, Option<Arc<Tailnet>>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Option<Arc<Tailnet>>)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The last reading, without asking Tailscale again.
pub fn cached() -> Option<Arc<Tailnet>> {
    cache().lock().unwrap().as_ref().and_then(|(_, t)| t.clone())
}

/// Where Tailscale sends traffic to a device directly ("192.168.1.9:41641"),
/// from the last reading.
pub fn cached_direct_addr(dns_name: &str) -> Option<String> {
    cached()?.peers.iter().find(|p| p.dns_name == dns_name).and_then(|p| p.direct_addr.clone())
}

/// Forget the last reading (the network changed).
pub fn invalidate() {
    *cache().lock().unwrap() = None;
}

/// What Tailscale knows right now (a reading a few seconds old is reused).
/// None when Tailscale isn't installed or didn't answer.
pub async fn status() -> Option<Arc<Tailnet>> {
    if let Some((at, t)) = cache().lock().unwrap().as_ref() {
        if at.elapsed() < FRESH {
            return t.clone();
        }
    }
    let read = read().await.map(Arc::new);
    *cache().lock().unwrap() = Some((Instant::now(), read.clone()));
    read
}

async fn read() -> Option<Tailnet> {
    let cli = cli()?;
    let mut cmd = tokio::process::Command::new(cli);
    cmd.args(["status", "--json"]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(4), cmd.output()).await.ok()?.ok()?;
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// ssh's way to a machine through Tailscale, for when the host in fleet.toml
/// (or ssh's config) only works at home, like a local-network address.
#[derive(Default)]
struct SshRoute {
    /// The machine's Tailscale address.
    tailnet: Option<String>,
    /// In use: the usual host didn't answer from this network, this did.
    active: bool,
}

fn ssh_routes() -> &'static Mutex<std::collections::HashMap<String, SshRoute>> {
    static ROUTES: OnceLock<Mutex<std::collections::HashMap<String, SshRoute>>> = OnceLock::new();
    ROUTES.get_or_init(Default::default)
}

/// Where the machine is on the tailnet (from the engine, as Tailscale reports it).
pub fn set_ssh_tailnet(machine_id: &str, address: Option<String>) {
    let mut routes = ssh_routes().lock().unwrap();
    let r = routes.entry(machine_id.to_string()).or_default();
    if r.tailnet != address {
        r.active = false;
    }
    r.tailnet = address;
}

/// The Tailscale address to try when the usual host doesn't answer (and
/// isn't already being bypassed).
pub fn ssh_fallback(machine_id: &str) -> Option<String> {
    ssh_routes().lock().unwrap().get(machine_id).filter(|r| !r.active).and_then(|r| r.tailnet.clone())
}

/// Send ssh to the machine through Tailscale (or stop).
pub fn use_ssh_fallback(machine_id: &str, on: bool) {
    if let Some(r) = ssh_routes().lock().unwrap().get_mut(machine_id) {
        r.active = on && r.tailnet.is_some();
    }
}

/// The address ssh should use instead of the usual host, while it's bypassed.
pub fn ssh_override(machine_id: &str) -> Option<String> {
    ssh_routes().lock().unwrap().get(machine_id).filter(|r| r.active).and_then(|r| r.tailnet.clone())
}

/// The network changed: try the usual hosts again.
pub fn reset_ssh_routes() {
    for r in ssh_routes().lock().unwrap().values_mut() {
        r.active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like real `tailscale status --json` output (names and keys made up).
    const STATUS: &str = r#"Warning: client version "1.104.1" != tailscaled server version "1.102.4"
{
  "Version": "1.102.4",
  "BackendState": "Running",
  "TailscaleIPs": ["100.84.36.101", "fd7a:115c:a1e0::f35:2466"],
  "CurrentTailnet": { "Name": "someone@example.com", "MagicDNSSuffix": "tail0000.ts.net", "MagicDNSEnabled": true },
  "Self": { "HostName": "Laptop", "DNSName": "laptop.tail0000.ts.net.", "OS": "macOS" },
  "Peer": {
    "nodekey:a": { "HostName": "box-SER", "DNSName": "box-ser.tail0000.ts.net.", "OS": "linux", "TailscaleIPs": ["100.123.240.0", "fd7a:115c:a1e0::c530:f001"],
                   "Online": true, "LastSeen": "0001-01-01T00:00:00Z", "CurAddr": "192.168.148.247:41641", "Relay": "mia", "Active": true },
    "nodekey:b": { "HostName": "localhost", "DNSName": "phone.tail0000.ts.net.", "OS": "iOS", "TailscaleIPs": ["100.64.56.41"],
                   "Online": false, "LastSeen": "2026-10-08T23:17:05.1Z", "CurAddr": "", "Relay": "iad", "Active": false },
    "nodekey:c": { "HostName": "nas", "DNSName": "nas.tail0000.ts.net.", "OS": "linux", "TailscaleIPs": ["100.99.89.52"],
                   "Online": true, "LastSeen": "0001-01-01T00:00:00Z", "CurAddr": "", "Relay": "mia", "Active": true }
  }
}"#;

    #[test]
    fn reads_status() {
        let t = parse(STATUS).unwrap();
        assert!(t.running());
        assert_eq!(t.name.as_deref(), Some("someone@example.com"));
        assert_eq!(t.peers.len(), 3);
        let box_ = t.peers.iter().find(|p| p.host_name == "box-SER").unwrap();
        assert_eq!((box_.dns_name.as_str(), box_.short_name(), box_.path()), ("box-ser.tail0000.ts.net", "box-ser", "direct"));
        assert_eq!(box_.last_seen, None, "online devices have no last-seen time");
        let phone = t.peers.iter().find(|p| p.os == "iOS").unwrap();
        assert!(!phone.online && phone.last_seen.is_some() && !phone.is_computer());
        assert_eq!(phone.path(), "idle");
        assert_eq!(t.peers.iter().find(|p| p.host_name == "nas").unwrap().path(), "relay");
    }

    #[test]
    fn finds_machines_on_the_tailnet() {
        let t = parse(STATUS).unwrap();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // By the addresses its agent reported.
        assert_eq!(t.find(&s(&["100.123.240.0"]), &[]).unwrap().host_name, "box-SER");
        // By the names ssh knows it by: an alias that isn't Tailscale's, then
        // the HostName ssh resolves it to.
        assert_eq!(t.find(&[], &s(&["beelink", "adib@box-ser"])).unwrap().host_name, "box-SER");
        assert_eq!(t.find(&[], &s(&["box-ser.tail0000.ts.net."])).unwrap().host_name, "box-SER");
        assert_eq!(t.find(&[], &s(&["100.99.89.52"])).unwrap().host_name, "nas");
        assert!(t.find(&[], &s(&["192.168.1.9", "elsewhere"])).is_none());
    }
}
