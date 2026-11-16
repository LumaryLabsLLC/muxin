//! How Muxin reaches each machine: directly (QUIC, once paired), over a
//! live ssh link, or one ssh call at a time; and turning direct connections on
//! and off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use super::sessions::agent;
use super::{Engine, direct, link};
use crate::config::MachineSpec;

#[derive(Debug, Clone, Serialize)]
pub struct Connection {
    pub machine_id: String,
    /// "direct" (QUIC), "ssh" (a live link over ssh), or "none" (no link up right now).
    pub via: String,
    /// Paired for direct connections.
    pub direct: bool,
    /// Where the direct connection goes.
    pub address: Option<String>,
    /// A round trip to the agent just now, in milliseconds.
    pub latency_ms: Option<f64>,
    /// "Local network", "Tailscale", "Internet" or "This computer".
    pub network: Option<String>,
    /// The machine's address, as connected to.
    pub peer: Option<String>,
    /// How the machine keeps the direct listener running ("systemd", "launchd", "background").
    pub service: Option<String>,
    pub note: Option<String>,
    /// The machine on the user's tailnet, when it is (with how Tailscale
    /// reaches it from here: "direct" peer-to-peer, "relay", or "idle").
    pub tailnet: Option<super::model::TailnetDevice>,
    /// Tailscale on this computer, when it's installed.
    pub tailscale: Option<super::model::TailscaleStatus>,
}

impl Engine {
    /// Disconnects from a machine or connects to it again. Disconnected,
    /// Muxin closes its link and stops checking it and looking at its
    /// sessions (they go on there) until it's connected again; it's saved in
    /// config.toml (`disconnected = true`), so it lasts. Connecting checks it.
    pub fn set_connected(self: &Arc<Self>, machine_id: &str, connected: bool) -> Result<(), String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if !spec.is_checkable() {
            return Err(format!("{} has no host to connect to.", spec.name));
        }
        if spec.disconnected != connected {
            return Ok(());
        }
        let edit = |text: &str| crate::ops::inventory_edit::set_disconnected(text, &spec.id, !connected);
        match self.demo_world() {
            Some(w) => w.edit_inventory(edit)?,
            None => crate::ops::inventory_edit::edit_file(self.inventory_path(), edit)?,
        }
        // The new setting first, so nothing connects again in between. Reading
        // it closes the link too, as it does in every other Muxin process
        // that has one to the machine (`reload_inventory`).
        self.reload_inventory_now();
        if connected {
            self.trigger_check(Some(spec.id.clone()));
        } else {
            link::reset(&spec.id);
            // Asleep, it's left as it is: closing this sends it nothing.
            super::quiet::let_go(&spec.id);
        }
        Ok(())
    }

    /// How Muxin reaches the machine right now (measured, connecting if needed).
    pub async fn connection(self: &Arc<Self>, machine_id: &str) -> Result<Connection, String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        let paired = direct::paired(&spec.id);
        let mut out = Connection {
            machine_id: spec.id.clone(),
            via: "none".into(),
            direct: paired.is_some(),
            address: paired.as_ref().map(|p| match spec.direct_address.as_deref().filter(|a| !a.trim().is_empty()) {
                Some(a) => format!("{} (UDP, forwarded)", a.trim()),
                None => format!("{}:{} (UDP)", p.host, p.port),
            }),
            latency_ms: None,
            network: None,
            peer: None,
            service: None,
            note: None,
            tailnet: None,
            tailscale: None,
        };
        if self.demo_world().is_some() || !spec.is_checkable() {
            return Ok(out);
        }
        // What Tailscale says right now (how it reaches the machine changes).
        self.refresh_tailnet().await;
        if let Some(m) = self.current().machines.iter().find(|m| m.id == spec.id) {
            out.tailnet = m.tailnet.clone();
        }
        out.tailscale = self.current().tailscale.clone();
        // Asleep: no connection is kept, or made to see how it would go.
        if super::asleep::is(&spec.id) {
            out.note = Some(format!("{} is asleep, so Muxin isn't connected to it.", spec.name));
            return Ok(out);
        }
        if let Some(l) = link::get(&spec).await {
            let started = Instant::now();
            if l.request(json!({ "op": "ping" }), Duration::from_secs(10)).await.is_ok() {
                out.latency_ms = Some((started.elapsed().as_secs_f64() * 10_000.).round() / 10.);
                out.via = l.transport().into();
                out.network = l.network().map(str::to_string);
                out.peer = l.peer().map(|a| a.ip().to_string());
            }
        }
        // Through another machine (ProxyJump to its own loopback): ssh's last
        // hop is "this computer" to that one; the network that counts is the
        // one to it.
        let parent = self.current().machines.iter().find(|m| m.id == spec.id).and_then(|m| m.parent.clone());
        if let Some(p) = parent.clone().filter(|_| out.network.as_deref() == Some("This computer") && !spec.is_local()) {
            out.network = link::current(&p.machine_id).and_then(|l| l.network().map(str::to_string));
            out.peer = None;
        }
        // Inside another machine with no port forwarded to it: direct can't get in.
        let shut_in = parent.as_ref().is_some_and(|p| p.inside()) && spec.direct_address.as_deref().is_none_or(|a| a.trim().is_empty());
        if out.direct && out.via == "ssh" && shut_in {
            let host = parent.map(|p| p.name).unwrap_or_default();
            out.note = Some(format!("{host} doesn't forward a UDP port to it, so it's ssh. VMs Muxin makes now get one."));
        } else if out.direct && out.via == "ssh" {
            let mac = self.current().machines.iter().any(|m| m.id == spec.id && m.platform.starts_with("darwin"));
            out.note = Some(over_ssh_note(&spec.name, mac, self.just_woke(&spec.id)));
        }
        // Its local-network address didn't answer, so the connection took the
        // long way. Away from home that's expected; but when Tailscale itself
        // reaches the machine on this network, something here blocked us.
        // (A machine reached at an address of its own has no local way to look for.)
        let lan = if spec.direct_address.is_some() { Vec::new() } else { paired.as_ref().map(|p| p.lan.clone()).unwrap_or_default() };
        let long_way = out.via == "direct" && !lan.is_empty() && out.network.as_deref().is_some_and(|n| n != "Local network");
        let here = out.tailnet.as_ref().is_some_and(|d| d.path == "direct" && same_network(d, &lan));
        if long_way && (here || out.tailnet.is_none()) {
            // Try the short way now (on a fresh socket if need be), then say
            // what's what.
            self.move_home(machine_id).await;
            if let Some(l) = link::current(machine_id).filter(|l| l.network() == Some("Local network")) {
                out.network = l.network().map(str::to_string);
                out.peer = l.peer().map(|a| a.ip().to_string());
            } else {
                out.note = Some("Not reachable on this network: allow Muxin in Privacy & Security › Local Network, then reopen it.".into());
            }
        }
        if out.via == "direct" && out.network.as_deref() == Some("Tailscale") {
            if let Some(d) = out.tailnet.as_ref().filter(|d| d.path == "relay") {
                out.note = Some(format!("Relayed by Tailscale{}: slower.", d.relay.as_ref().map(|r| format!(" ({r})")).unwrap_or_default()));
            }
        }
        Ok(out)
    }

    /// Pairs with the machine's agent so Muxin connects to it directly
    /// (QUIC) instead of through ssh; or undoes that.
    pub async fn set_direct(self: &Arc<Self>, machine_id: &str, enabled: bool) -> Result<Connection, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if spec.is_local() {
            return Err("This computer is reached without a network; it doesn't need a direct connection.".into());
        }
        if self.demo_world().is_some() {
            return self.connection(&spec.id).await;
        }
        agent::ensure(&spec, probe.as_ref()).await?;
        // Its choice, either way: no more suggestions to connect directly.
        let _ = direct::set_ssh_only(&spec.id, !enabled);
        if !enabled {
            // Only this app: others paired with the machine keep their direct connections.
            if let (Ok(app), Some(l)) = (direct::app_fingerprint(), link::get(&spec).await) {
                let _ = l.request(json!({ "op": "unpair", "client": app }), Duration::from_secs(30)).await;
            }
            direct::save(&spec.id, None)?;
            link::reset(&spec.id);
            let out = self.connection(&spec.id).await;
            self.republish();
            return out;
        }
        // Pair over ssh: the agent learns this app's fingerprint and (re)starts
        // its listener, which would cut a direct link off mid-request.
        let app = direct::app_fingerprint()?;
        direct::save(&spec.id, None)?;
        link::reset(&spec.id);
        let l = link::get(&spec).await.ok_or_else(|| format!("Couldn't reach the agent on {}.", spec.name))?;
        let reply = l.request(json!({ "op": "pair", "client": app }), Duration::from_secs(60)).await?;
        if reply["ok"].as_bool() != Some(true) {
            return Err(reply["error"].as_str().unwrap_or("The agent couldn't set up direct connections.").to_string());
        }
        let fingerprint = reply["fingerprint"].as_str().unwrap_or("").to_string();
        let port = reply["port"].as_u64().unwrap_or(0) as u16;
        let host = direct::resolve_host(&spec).await.unwrap_or_else(|| spec.host.rsplit('@').next().unwrap_or(&spec.host).to_string());
        let list = |k: &str| -> Vec<String> { reply[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default() };
        let (lan, tailnet) = (list("addresses"), list("tailnet"));
        direct::save(&spec.id, Some(direct::Paired { fingerprint, host, port, lan, tailnet }))?;
        // The listener may take a moment to start.
        for _ in 0..6 {
            link::reset(&spec.id);
            if link::get(&spec).await.is_some_and(|l| l.transport() == "direct") {
                break;
            }
            direct::save(&spec.id, direct::paired(&spec.id))?;
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        let mut out = self.connection(&spec.id).await?;
        out.service = reply["service"].as_str().map(str::to_string);
        if out.note.is_none() {
            out.note = reply["note"].as_str().map(str::to_string);
        }
        self.republish();
        Ok(out)
    }

    /// Whether a machine the user just added is to be connected to directly
    /// (`direct`, or with None the "Connect directly" setting). `Some(false)`
    /// keeps it on ssh, with no suggestion to switch.
    pub fn wants_direct(&self, machine_id: &str, direct: Option<bool>) -> bool {
        if cfg!(target_os = "ios") || self.demo_world().is_some() {
            return false;
        }
        if direct == Some(false) {
            let _ = direct::set_ssh_only(machine_id, true);
            self.republish();
            return false;
        }
        direct.unwrap_or_else(|| self.settings().connect_directly)
    }

    /// Connects to a machine that's up directly, unless a direct connection
    /// can't reach it, it's paired already or kept on ssh. None: nothing to
    /// do. A failure leaves it on ssh, and the suggestion to connect directly shows.
    pub async fn connect_directly_now(self: &Arc<Self>, m: &super::model::Machine) -> Option<Result<Connection, String>> {
        if m.status != "online" || !m.can_connect_directly() || direct::paired(&m.id).is_some() || direct::ssh_only().contains(&m.id) {
            return None;
        }
        let result = self.set_direct(&m.id, true).await;
        match &result {
            Ok(c) => tracing::info!("paired with {} for direct connections (now {})", m.name, c.via),
            Err(e) => tracing::info!("couldn't connect to {} directly: {e}", m.name),
        }
        Some(result)
    }

    /// A machine the user just added, connected to directly once its first
    /// check finds it online (within half an hour), when `wants_direct`.
    pub fn connect_directly_once_up(self: &Arc<Self>, machine_id: &str, direct: Option<bool>) {
        if cfg!(test) || !self.wants_direct(machine_id, direct) {
            return;
        }
        let (me, id) = (self.clone(), machine_id.to_string());
        self.runtime().spawn(async move {
            let mut rx = me.subscribe();
            let up = tokio::time::timeout(Duration::from_secs(30 * 60), async {
                loop {
                    match rx.borrow_and_update().machines.iter().find(|m| m.id == id) {
                        None => return None,
                        Some(m) if !m.checking && m.status == "online" => return Some(m.clone()),
                        Some(_) => {}
                    }
                    if rx.changed().await.is_err() {
                        return None;
                    }
                }
            })
            .await;
            if let Ok(Some(m)) = up {
                me.connect_directly_now(&m).await;
            }
        });
    }

    /// The end of making a machine (a cloud machine, a VM): connected to
    /// directly when the "Connect directly" setting says so, logged to `op`.
    pub(crate) async fn connect_directly_after_setup(self: &Arc<Self>, op: u64, machine_id: &str) {
        if self.demo_world().is_some() || !self.settings().connect_directly {
            return;
        }
        let fits = self.current().machines.iter().any(|m| m.id == machine_id && m.can_connect_directly());
        if !fits || direct::ssh_only().contains(machine_id) {
            return;
        }
        match self.set_direct(machine_id, true).await {
            Ok(c) if c.via == "direct" => self.op_log(op, "✓ Connected directly"),
            Ok(_) => self.op_log(op, "! Paired for direct connections, but it's reached over ssh for now"),
            Err(e) => self.op_log(op, format!("! Staying on ssh: {e}")),
        }
    }

    /// Keeps a machine on ssh without changing anything there: Muxin stops
    /// suggesting a direct connection to it (Connect directly on its page
    /// still works).
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn keep_ssh(&self, machine_id: &str) -> Result<(), String> {
        direct::set_ssh_only(machine_id, true)?;
        self.republish();
        Ok(())
    }

    /// A machine that was paired left Muxin: its agent forgets this app too
    /// (only this one). In the background and best-effort: a machine that
    /// can't be reached is removed all the same. `unpaired` waits for it.
    /// The machine's link, kept up for this (`remove_machine`), goes after.
    ///
    /// One that's asleep is told too. Removing it is the user's own act, so
    /// it's reached for this one word, as Wake reaches it: it wakes for a
    /// moment, and sleeps on.
    ///
    /// The connection it would have said it woke on goes too, however this
    /// ends (told, not reached, or out of time): letting go of it sends
    /// nothing, and nobody waits to hear a machine that has left.
    pub(crate) fn unpair_removed(self: &Arc<Self>, spec: MachineSpec) {
        if spec.is_local() || self.demo_world().is_some() {
            link::reset(&spec.id);
            super::quiet::let_go(&spec.id);
            return;
        }
        let id = spec.id.clone();
        let task = self.runtime().spawn(async move {
            // Reached on purpose, asleep or not, until this is done.
            let _looking = super::asleep::look(&spec.id);
            // A link made for this isn't kept: it's hung up below, however
            // the asking ends.
            let mut own = None;
            let ask = async {
                let app = direct::app_fingerprint().ok()?;
                // Over its link when it has one. Else over one made for
                // this: `link::get` lets go of a link whose machine says
                // it's asleep before a word goes over it.
                let l = match link::current(&spec.id) {
                    Some(l) => l,
                    None => own.insert(link::reach(&spec).await.ok()?).clone(),
                };
                l.request(json!({ "op": "unpair", "client": app }), Duration::from_secs(15)).await.ok()
            };
            if tokio::time::timeout(Duration::from_secs(20), ask).await.ok().flatten().is_none() {
                tracing::debug!("couldn't unpair {} on its way out", spec.name);
            }
            if let Some(l) = own {
                l.hang_up();
            }
            link::reset(&spec.id);
            super::quiet::let_go(&spec.id);
        });
        unpairing().lock().unwrap().insert(id, task);
    }

    /// Waits for a removed machine's unpairing (a command about to exit would cut it short).
    pub async fn unpaired(&self, machine_id: &str) {
        let task = unpairing().lock().unwrap().remove(machine_id);
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

/// Removed machines being unpaired, by id.
fn unpairing() -> &'static Mutex<HashMap<String, tokio::task::JoinHandle<()>>> {
    static UNPAIRING: OnceLock<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> = OnceLock::new();
    UNPAIRING.get_or_init(Default::default)
}

/// Tailscale reaches the machine at one of its local-network addresses: it's
/// on the same network as this computer.
fn same_network(d: &super::model::TailnetDevice, lan: &[String]) -> bool {
    // Tailscale's own direct address isn't in what we keep; it's read fresh.
    super::tailscale::cached_direct_addr(&d.dns_name).is_some_and(|a| lan.iter().any(|l| a.starts_with(&format!("{l}:"))))
}

/// What to say of a paired machine that's reached over ssh.
fn over_ssh_note(name: &str, mac: bool, just_woke: bool) -> String {
    if just_woke {
        // Linked as it woke, before its direct way answered: nothing is
        // blocked, and it moves back by itself.
        format!("{name} just woke, so it's ssh for a moment. Muxin goes back to the direct connection by itself.")
    } else if mac {
        // A Mac blocks the agent's network until Local Network access is
        // allowed there. Muxin Agent since 0.10.0, a plain muxen-agent before.
        format!("On {name}, allow Muxin Agent (or muxen-agent, with an older agent) under Privacy & Security › Local Network.")
    } else {
        "Direct didn't connect (UDP 47821 blocked?), so it's ssh.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    /// A paired machine reached over ssh: on a Mac that usually means its
    /// agent isn't allowed on the local network yet. Not when it just woke:
    /// its link was made before its direct way answered, nothing is blocked,
    /// and nobody should be sent to that Mac's settings for it.
    #[test]
    fn a_machine_that_just_woke_isnt_said_to_be_blocked() {
        let blocked = over_ssh_note("Book", true, false);
        assert!(blocked.contains("Local Network") && blocked.starts_with("On Book,"));
        let woke = over_ssh_note("Book", true, true);
        assert_eq!(woke, "Book just woke, so it's ssh for a moment. Muxin goes back to the direct connection by itself.");
        assert!(!woke.contains("Local Network"));
        // The same for a machine that isn't a Mac.
        assert!(over_ssh_note("Box", false, false).contains("UDP 47821"));
        assert!(!over_ssh_note("Box", false, true).contains("UDP"));
    }

    /// A paired machine that's removed is unpaired here, reached or not. The
    /// second machine has a link: its agent is told over that one, which
    /// taking it out of config.toml leaves up until then and closes after.
    /// The third is asleep: its agent is told all the same.
    /// (All in one test: tests share one direct.json, and two saving a
    /// pairing at the same moment would lose one.)
    #[tokio::test(flavor = "multi_thread")]
    async fn a_removed_machine_leaves_no_pairing_behind() {
        let tmp = std::env::temp_dir().join(format!("muxin-unpair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Disconnected, so nothing reaches out to it.
        let text = "[[machine]]\nname = \"Paired\"\nhost = \"paired.invalid\"\ndisconnected = true\n\n[[machine]]\nname = \"Told\"\nhost = \"told.invalid\"\n\n[[machine]]\nname = \"Dozing\"\nhost = \"dozing.invalid\"\n";
        std::fs::write(tmp.join("config.toml"), text).unwrap();
        let engine = Engine::new(Paths::resolve(Some(tmp.clone()), None), false);
        let pairing = |host: &str| direct::Paired { fingerprint: "f".repeat(64), host: host.into(), port: 47821, lan: Vec::new(), tailnet: Vec::new() };
        direct::save("paired", Some(pairing("paired.invalid"))).unwrap();
        engine.remove_machine("paired").unwrap();
        engine.unpaired("paired").await;
        assert!(engine.machine("paired").is_none() && direct::paired("paired").is_none());

        // What an agent got before its end of the link closed.
        async fn asked(mut agent: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>) -> Vec<String> {
            let mut ops = Vec::new();
            while let Some(request) = tokio::time::timeout(Duration::from_secs(10), agent.recv()).await.expect("something still holds the link") {
                ops.push(request["op"].as_str().unwrap_or_default().to_string());
            }
            ops
        }

        direct::save("told", Some(pairing("told.invalid"))).unwrap();
        let (l, agent) = link::piped(&engine.machine("told").unwrap().0).await;
        engine.remove_machine("told").unwrap();
        engine.unpaired("told").await;
        assert!(engine.machine("told").is_none() && direct::paired("told").is_none());
        assert!(link::current("told").is_none() && !l.alive());
        drop(l);
        assert_eq!(asked(agent).await, ["unpair"]);

        // A machine that sleeps has no link, and Muxin leaves it alone; not
        // when it's leaving. It's reached for this one word, which wakes it
        // for a moment (its agent says it's asleep), over a link that isn't
        // kept, and it's left alone again after.
        use super::super::asleep;
        direct::save("dozing", Some(pairing("dozing.invalid"))).unwrap();
        assert!(engine.machine_slept("dozing", &Default::default()) && asleep::is("dozing"));
        // Asleep on another's word too, which outlasts the removal: nothing
        // here leans on the removal dropping the machine's last check first.
        let (another, dozing) = (7075, "dozing".to_string());
        asleep::sync(another, [&dozing].into_iter());
        let agent = link::piped_asleep(&engine.machine("dozing").unwrap().0).await;
        // It had said it would tell this app when it wakes, on a connection
        // the app holds: that goes with it.
        let wakes = super::super::quiet::tests::fake_agent();
        super::super::quiet::open("dozing", &wakes.paired, wakes.at).await.unwrap();
        engine.remove_machine("dozing").unwrap();
        engine.unpaired("dozing").await;
        assert!(engine.machine("dozing").is_none() && direct::paired("dozing").is_none());
        assert!(super::super::quiet::held("dozing").is_none(), "still waiting to hear a machine that has left");
        assert!(link::current("dozing").is_none(), "the link made for it isn't kept");
        assert_eq!(asked(agent).await, ["unpair"]);
        assert!(asleep::is("dozing"), "nothing is still looking at it");
        asleep::sync(another, std::iter::empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
