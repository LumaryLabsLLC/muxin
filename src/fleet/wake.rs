//! Waking a machine that's asleep (Wake on LAN): its "magic packet",
//! broadcast from this computer and from every machine that's on its
//! network and online, so it works away from home too. Its network card's
//! address comes from its last check (see probe.sh).

use std::sync::Arc;

use serde_json::json;

use super::Engine;

/// Six 0xFF, then the address sixteen times.
fn packet(mac: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = mac.split([':', '-']).map(|b| u8::from_str_radix(b, 16).ok()).collect::<Option<_>>()?;
    (bytes.len() == 6).then(|| {
        let mut out = vec![0xFF; 6];
        for _ in 0..16 {
            out.extend(&bytes);
        }
        out
    })
}

impl Engine {
    /// Whether a machine can be woken: its network card is known.
    pub fn can_wake(&self, machine_id: &str) -> bool {
        self.machine(machine_id).and_then(|(_, p)| p).is_some_and(|p| p.mac.is_some())
    }

    /// Sends the wake signal; says from where. Checks it again in a little while.
    pub async fn wake(self: &Arc<Self>, machine_id: &str) -> Result<String, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        let probe = probe.ok_or_else(|| format!("Muxen hasn't reached {} yet, so it doesn't know its network card.", spec.name))?;
        let mac = probe.mac.clone().ok_or_else(|| format!("Muxen doesn't know {}'s network card yet: it learns it the next time the machine is on.", spec.name))?;
        if self.demo_world().is_some() {
            return Ok("this computer".into());
        }
        let broadcast = probe.lan_broadcast.clone().unwrap_or_default();
        let mut from = Vec::new();
        // From here (when this computer is on its network, that's enough).
        let p = packet(&mac).ok_or("Its network card's address doesn't read right.")?;
        if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
            let _ = sock.set_broadcast(true);
            let mut sent = false;
            for addr in ["255.255.255.255", broadcast.as_str()].into_iter().filter(|a| !a.is_empty()) {
                for port in [9, 7] {
                    sent |= sock.send_to(&p, (addr, port)).is_ok();
                }
            }
            if sent {
                from.push("this computer".to_string());
            }
        }
        // And from the machines on its network that are on.
        let relays: Vec<_> = self
            .current()
            .machines
            .iter()
            .filter(|m| m.id != spec.id && m.status == "online" && !m.local)
            .filter_map(|m| self.machine(&m.id))
            .filter(|(_, p)| p.as_ref().is_some_and(|p| !broadcast.is_empty() && p.lan_broadcast.as_deref() == Some(broadcast.as_str())))
            .collect();
        for (relay, _) in relays {
            let request = json!({ "op": "wake", "mac": mac, "broadcast": broadcast });
            if super::sessions::agent::ctl(&relay, &request).await.is_ok() {
                from.push(relay.name.clone());
            }
        }
        if from.is_empty() {
            return Err("Couldn't send the wake signal from anywhere.".into());
        }
        // A machine takes a little while to wake: look again, twice.
        let me = self.clone();
        let id = spec.id.clone();
        self.runtime().spawn(async move {
            for wait in [20, 40] {
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                me.run_check(Some(vec![id.clone()])).await;
            }
        });
        Ok(from.join(" and "))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn makes_the_same_packet_as_the_agent() {
        let p = super::packet("3c-22-fb-9a-41-07").unwrap();
        assert_eq!(p.len(), 102);
        assert_eq!(&p[96..], &[0x3c, 0x22, 0xfb, 0x9a, 0x41, 0x07]);
    }
}
