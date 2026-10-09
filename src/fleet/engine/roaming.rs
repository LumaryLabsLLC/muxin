//! Keeping machines within reach as this computer moves between networks,
//! with Tailscale's help when it's installed.
//!
//! - Tailscale's view of each machine (online, last seen, how traffic gets
//!   there) is read every few seconds: a machine it sees come back is checked
//!   right away, as is one it loses.
//! - A change of network (or of Tailscale here) retries what failed, drops
//!   connections that no longer work (a fresh one then takes the best way from
//!   here), and moves connections that went through Tailscale back to the
//!   local network when the machine is on it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::json;

use super::super::model::{TailnetDevice, TailscaleStatus};
use super::super::{direct, link, tailscale};
use super::Engine;
use crate::config::MachineSpec;

/// How often to read Tailscale while a machine is offline (it says when one
/// is back), and otherwise.
const WATCH_OFFLINE: Duration = Duration::from_secs(5);
const WATCH: Duration = Duration::from_secs(15);
/// How often to look at which network this computer is on.
const NETWORK_EVERY: Duration = Duration::from_secs(2);
/// How often to see whether a connection through Tailscale could take the
/// local network instead, and how long to leave one alone after trying.
const MOVE_HOME_EVERY: Duration = Duration::from_secs(120);
const MOVE_HOME_PAUSE: Duration = Duration::from_secs(600);

/// What changed in one reading of Tailscale.
#[derive(Default)]
pub(crate) struct TailnetChanges {
    /// Machines Tailscale started or stopped seeing.
    pub flipped: Vec<String>,
    /// Tailscale here started (true) or stopped (false) running.
    pub running: Option<bool>,
}

impl Engine {
    /// Reads Tailscale and updates what it says about each machine.
    pub(crate) async fn refresh_tailnet(self: &Arc<Self>) -> TailnetChanges {
        let read = tailscale::status().await;
        let status = match (&read, tailscale::installed()) {
            (Some(t), _) => Some(TailscaleStatus { state: t.state.clone(), running: t.running(), tailnet: t.name.clone() }),
            // Installed, but its service didn't answer.
            (None, true) => Some(TailscaleStatus { state: "NotRunning".into(), running: false, tailnet: None }),
            (None, false) => None,
        };
        let machines: Vec<(MachineSpec, Option<String>)> = {
            let st = self.state.lock().unwrap();
            st.inventory
                .machines
                .iter()
                .filter(|m| m.is_checkable() && !m.is_local())
                .map(|m| (m.clone(), st.probes.get(&m.id).and_then(|p| p.hostname.clone())))
                .collect()
        };
        // Each machine on the tailnet: by the addresses its agent reported,
        // else by the names it goes by.
        let mut found: HashMap<String, TailnetDevice> = HashMap::new();
        if let Some(t) = read.as_ref().filter(|t| t.running()) {
            for (spec, hostname) in &machines {
                let ips = direct::paired(&spec.id).map(|p| p.tailnet).unwrap_or_default();
                let mut names = vec![spec.host.clone()];
                names.extend(ssh_hostname(spec).await);
                names.extend(hostname.clone());
                if let Some(peer) = t.find(&ips, &names) {
                    found.insert(spec.id.clone(), device(peer));
                }
            }
        }
        for (spec, _) in &machines {
            let v4 = found.get(&spec.id).and_then(|d| d.ips.iter().find(|ip| !ip.contains(':')).cloned());
            tailscale::set_ssh_tailnet(&spec.id, v4);
        }

        let mut changes = TailnetChanges::default();
        let mut st = self.state.lock().unwrap();
        let was = st.tailscale.as_ref().map(|s| s.running);
        let now = status.as_ref().map(|s| s.running);
        if was.is_some() && was != now {
            changes.running = Some(now == Some(true));
        }
        let mut changed = st.tailscale != status;
        st.tailscale = status;
        if now == Some(true) {
            for (spec, _) in &machines {
                let before = st.tailnet.get(&spec.id).cloned();
                match found.remove(&spec.id) {
                    Some(d) => {
                        if before.as_ref().is_some_and(|b| b.online != d.online) {
                            changes.flipped.push(spec.id.clone());
                        }
                        if before.as_ref().is_none_or(|b| !same(b, &d)) {
                            changed = true;
                        }
                        st.tailnet.insert(spec.id.clone(), d);
                    }
                    // Not (or no longer) on the tailnet.
                    None => changed |= st.tailnet.remove(&spec.id).is_some(),
                }
            }
        }
        // While Tailscale is off, what it last said is kept: it's how Muxen
        // knows a machine is reached through it.
        let known: Vec<String> = st.inventory.machines.iter().map(|m| m.id.clone()).collect();
        let before = st.tailnet.len();
        st.tailnet.retain(|id, _| known.contains(id));
        changed |= st.tailnet.len() != before;
        if changed {
            self.save_state(&mut st);
            self.publish(&mut st);
        }
        changes
    }

    /// Follows Tailscale: checks a machine as soon as Tailscale sees it come or
    /// go, and everything out of reach when Tailscale starts here.
    pub(super) async fn tailnet_loop(self: Arc<Self>) {
        if self.demo.is_some() || !tailscale::installed() {
            return;
        }
        loop {
            let changes = self.refresh_tailnet().await;
            if changes.running == Some(true) {
                tracing::info!("Tailscale is running here; retrying machines out of reach");
                direct::retry_all();
                link::retry_all();
                self.check_soon(self.offline_machines());
            }
            if !changes.flipped.is_empty() {
                tracing::debug!("Tailscale sees a change: {:?}", changes.flipped);
                self.check_soon(changes.flipped);
            }
            let wait = if self.offline_machines().is_empty() { WATCH } else { WATCH_OFFLINE };
            tokio::time::sleep(wait).await;
        }
    }

    /// Notices this computer joining another network (Wi-Fi to a hotspot,
    /// home to a café) and adjusts every machine's connection.
    pub(super) async fn network_loop(self: Arc<Self>) {
        if self.demo.is_some() {
            return;
        }
        let mut last = tokio::task::spawn_blocking(route_signature).await.unwrap_or_default();
        let mut since_move = Instant::now();
        loop {
            tokio::time::sleep(NETWORK_EVERY).await;
            let now = tokio::task::spawn_blocking(route_signature).await.unwrap_or_default();
            if now != last {
                tracing::info!("network changed ({last} → {now})");
                last = now;
                self.network_changed();
                since_move = Instant::now();
            } else if since_move.elapsed() >= MOVE_HOME_EVERY {
                since_move = Instant::now();
                for id in self.linked_machines() {
                    let me = self.clone();
                    self.rt.spawn(async move { me.move_home(&id).await });
                }
            }
        }
    }

    fn network_changed(self: &Arc<Self>) {
        tailscale::invalidate();
        tailscale::reset_ssh_routes();
        direct::retry_all();
        link::retry_all();
        moves().lock().unwrap().clear();
        // Each live connection: still there? One that doesn't answer is
        // dropped, and losing it starts a check and a fresh connection, the
        // best way from here. One that does may have a shorter way now.
        for id in self.linked_machines() {
            let me = self.clone();
            self.rt.spawn(async move {
                let Some(l) = link::current(&id) else { return };
                if l.request(json!({ "op": "ping" }), Duration::from_secs(3)).await.is_ok() {
                    me.move_home(&id).await;
                }
            });
        }
        // Machines that were out of reach may not be from here.
        self.check_soon(self.offline_machines());
        let me = self.clone();
        self.rt.spawn(async move {
            me.refresh_tailnet().await;
        });
    }

    /// A direct connection that went through Tailscale (or the internet) while
    /// the machine answers on this network too: move it the short way.
    pub(super) async fn move_home(self: &Arc<Self>, id: &str) {
        let Some(l) = link::current(id) else { return };
        if l.transport() != "direct" || l.network() == Some("Local network") {
            return;
        }
        let Some(paired) = direct::paired(id).filter(|p| !p.lan.is_empty()) else { return };
        if moves().lock().unwrap().get(id).is_some_and(|t| t.elapsed() < MOVE_HOME_PAUSE) {
            return;
        }
        if !direct::lan_answers(&paired).await {
            return;
        }
        let Some((spec, _)) = self.machine(id) else { return };
        moves().lock().unwrap().insert(id.to_string(), Instant::now());
        tracing::info!("{} answers on the local network; moving its connection there", spec.name);
        link::reset(id);
        let _ = link::get(&spec).await;
    }

    fn check_soon(self: &Arc<Self>, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move { me.run_check(Some(ids)).await });
    }

    /// Machines the last check couldn't reach.
    fn offline_machines(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        st.inventory.machines.iter().filter(|m| m.is_checkable() && st.probes.get(&m.id).is_some_and(|p| !p.ok)).map(|m| m.id.clone()).collect()
    }

    /// Machines with a live connection right now.
    fn linked_machines(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        st.inventory.machines.iter().filter(|m| link::current(&m.id).is_some()).map(|m| m.id.clone()).collect()
    }
}

fn device(p: &tailscale::Peer) -> TailnetDevice {
    TailnetDevice {
        name: p.short_name().to_string(),
        dns_name: p.dns_name.clone(),
        ips: p.ips.iter().map(|ip| ip.to_string()).collect(),
        os: p.os.clone(),
        online: p.online,
        last_seen_at: if p.online { None } else { p.last_seen },
        path: p.path().into(),
        relay: (p.path() == "relay").then(|| p.relay.clone()).flatten(),
        read_at: Utc::now(),
    }
}

/// The same apart from when it was read.
fn same(a: &TailnetDevice, b: &TailnetDevice) -> bool {
    TailnetDevice { read_at: b.read_at, ..a.clone() } == *b
}

/// Where ssh actually goes for the machine (its config's HostName), looked
/// up once per host.
async fn ssh_hostname(spec: &MachineSpec) -> Option<String> {
    static NAMES: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    let names = NAMES.get_or_init(Default::default);
    let key = format!("{}|{}", spec.id, spec.host);
    if let Some(found) = names.lock().unwrap().get(&key) {
        return found.clone();
    }
    let found = direct::resolve_host(spec).await;
    names.lock().unwrap().insert(key, found.clone());
    found
}

/// When each machine's connection was last moved to the local network.
fn moves() -> &'static Mutex<HashMap<String, Instant>> {
    static MOVES: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MOVES.get_or_init(Default::default)
}

/// Which network this computer is on, as the addresses it would send from
/// (nothing is sent: connecting a UDP socket only picks a route).
fn route_signature() -> String {
    let from = |bind: &str, to: &str| {
        std::net::UdpSocket::bind(bind).and_then(|s| {
            s.connect(to)?;
            s.local_addr()
        })
    };
    let v4 = from("0.0.0.0:0", "1.1.1.1:53").map(|a| a.ip().to_string()).unwrap_or_default();
    let v6 = from("[::]:0", "[2606:4700:4700::1111]:53").map(|a| a.ip().to_string()).unwrap_or_default();
    format!("{v4} {v6}").trim().to_string()
}
