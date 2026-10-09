//! How Muxen reaches each machine: directly (QUIC, once paired), over a
//! live ssh link, or one ssh call at a time; and turning direct connections on
//! and off.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use super::sessions::agent;
use super::{Engine, direct, link};

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
    /// How Muxen reaches the machine right now (measured, connecting if needed).
    pub async fn connection(self: &Arc<Self>, machine_id: &str) -> Result<Connection, String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        let paired = direct::paired(&spec.id);
        let mut out = Connection {
            machine_id: spec.id.clone(),
            via: "none".into(),
            direct: paired.is_some(),
            address: paired.as_ref().map(|p| format!("{}:{} (UDP)", p.host, p.port)),
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
        if let Some(l) = link::get(&spec).await {
            let started = Instant::now();
            if l.request(json!({ "op": "ping" }), Duration::from_secs(10)).await.is_ok() {
                out.latency_ms = Some((started.elapsed().as_secs_f64() * 10_000.).round() / 10.);
                out.via = l.transport().into();
                out.network = l.network().map(str::to_string);
                out.peer = l.peer().map(|a| a.ip().to_string());
            }
        }
        if out.direct && out.via == "ssh" {
            // A Mac blocks the agent's network until Local Network access is allowed there.
            let mac = self.current().machines.iter().any(|m| m.id == spec.id && m.platform.starts_with("darwin"));
            out.note = Some(if mac {
                format!("On {}, allow muxen-agent under Privacy & Security › Local Network.", spec.name)
            } else {
                "Direct didn't connect (UDP 47821 blocked?), so it's ssh.".into()
            });
        }
        // Its local-network address didn't answer, so the connection took the
        // long way. Away from home that's expected; but when Tailscale itself
        // reaches the machine on this network, something here blocked us.
        let lan = paired.as_ref().map(|p| p.lan.clone()).unwrap_or_default();
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
                out.note = Some("Not reachable on this network: allow Muxen in Privacy & Security › Local Network, then reopen it.".into());
            }
        }
        if out.via == "direct" && out.network.as_deref() == Some("Tailscale") {
            if let Some(d) = out.tailnet.as_ref().filter(|d| d.path == "relay") {
                out.note = Some(format!("Relayed by Tailscale{}: slower.", d.relay.as_ref().map(|r| format!(" ({r})")).unwrap_or_default()));
            }
        }
        Ok(out)
    }

    /// Pairs with the machine's agent so Muxen connects to it directly
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
        if !enabled {
            if let Some(l) = link::get(&spec).await {
                let _ = l.request(json!({ "op": "unpair" }), Duration::from_secs(30)).await;
            }
            direct::save(&spec.id, None)?;
            link::reset(&spec.id);
            return self.connection(&spec.id).await;
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
        Ok(out)
    }
}

/// Tailscale reaches the machine at one of its local-network addresses: it's
/// on the same network as this computer.
fn same_network(d: &super::model::TailnetDevice, lan: &[String]) -> bool {
    // Tailscale's own direct address isn't in what we keep; it's read fresh.
    super::tailscale::cached_direct_addr(&d.dns_name).is_some_and(|a| lan.iter().any(|l| a.starts_with(&format!("{l}:"))))
}
