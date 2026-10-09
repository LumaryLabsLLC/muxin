//! Computers on the local network, for adding machines: whatever advertises
//! ssh over Bonjour (Macs with Remote Login on, Linux with Avahi), and Macs
//! that don't (yet) — Remote Login is off there.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkHost {
    /// What it calls itself ("Mac-Webtech-025", "beelink").
    pub name: String,
    /// Its Bonjour name, for ssh ("Mac-Webtech-025.local").
    pub host: String,
    /// Its addresses on this network.
    pub addresses: Vec<String>,
    /// Advertises ssh: it can be added as it is.
    pub ssh: bool,
    /// A Mac (Remote Login is how its ssh is turned on).
    pub mac: bool,
}

/// Apple devices that aren't computers (by their advertised model).
fn not_a_computer(model: &str) -> bool {
    ["AudioAccessory", "AppleTV", "iPhone", "iPad", "Watch", "RealityDevice", "iPod"].iter().any(|p| model.starts_with(p))
}

/// Listens on the local network for `listen`, then lists what answered.
pub fn browse(listen: Duration) -> Vec<NetworkHost> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return Vec::new() };
    let types = ["_ssh._tcp.local.", "_sftp-ssh._tcp.local.", "_companion-link._tcp.local."];
    let receivers: Vec<(&str, _)> = types.iter().filter_map(|t| daemon.browse(t).ok().map(|rx| (*t, rx))).collect();
    // By Bonjour host name, lowercased.
    let mut found: BTreeMap<String, NetworkHost> = BTreeMap::new();
    let deadline = Instant::now() + listen;
    while Instant::now() < deadline {
        for (ty, rx) in &receivers {
            while let Ok(event) = rx.try_recv() {
                let mdns_sd::ServiceEvent::ServiceResolved(s) = event else { continue };
                let host = s.get_hostname().trim_end_matches('.').to_string();
                if host.is_empty() {
                    continue;
                }
                let apple = *ty == "_companion-link._tcp.local.";
                let model = s.get_property_val_str("rpMd").unwrap_or("");
                if apple && not_a_computer(model) {
                    continue;
                }
                let instance = s.get_fullname().split('.').next().unwrap_or("").to_string();
                let entry = found.entry(host.to_ascii_lowercase()).or_insert_with(|| NetworkHost {
                    name: host.trim_end_matches(".local").to_string(),
                    host: host.clone(),
                    addresses: Vec::new(),
                    ssh: false,
                    mac: false,
                });
                // The name people gave it reads better than the host name.
                if apple && !instance.is_empty() {
                    entry.name = instance;
                }
                entry.ssh |= !apple;
                entry.mac |= apple;
                for ip in s.get_addresses_v4() {
                    let ip = ip.to_string();
                    if !entry.addresses.contains(&ip) {
                        entry.addresses.push(ip);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = daemon.shutdown();
    let mut hosts: Vec<NetworkHost> = found.into_values().collect();
    hosts.sort_by(|a, b| b.ssh.cmp(&a.ssh).then(a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())));
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speakers_and_phones_are_not_machines() {
        assert!(not_a_computer("AudioAccessory5,1"));
        assert!(not_a_computer("AppleTV11,1"));
        assert!(not_a_computer("iPhone15,2"));
        assert!(!not_a_computer("MacBookPro18,3"));
        assert!(!not_a_computer(""));
    }
}
