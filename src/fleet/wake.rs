//! Waking a machine that's asleep. One whose agent answers (a Mac that
//! wakes for the network: reaching it wakes it for a moment) is woken for
//! real by its agent. Otherwise Wake on LAN: its "magic packet", broadcast
//! from this computer and from every machine that's on its network and
//! online, so it works away from home too. Its network card's address
//! comes from its last check (see probe.sh).

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

/// What waking a machine came to.
#[derive(Debug, Clone, PartialEq)]
pub struct Woke {
    /// Its agent woke it for real: it's awake.
    pub awake: bool,
    /// Where the Wake on LAN signal went from, when one was sent.
    pub sent_from: Option<String>,
    /// For people.
    pub message: String,
}

/// How a machine that's asleep took being woken.
enum Roused {
    Awake,
    /// It woke for a moment and can't stay awake (a laptop's closed lid).
    StillAsleep { lid_closed: bool },
}

impl Engine {
    /// Whether a machine can be woken: its network card is known (for the
    /// wake signal), or it's asleep with an agent that answers.
    pub fn can_wake(&self, machine_id: &str) -> bool {
        self.machine(machine_id).and_then(|(_, p)| p).is_some_and(|p| p.mac.is_some() || p.asleep)
    }

    /// Wakes a machine. One that's asleep and answers (a Mac that wakes for
    /// the network) is woken for real by its agent, as a key press would.
    /// Otherwise the wake signal goes out, and it's looked at again in a
    /// little while.
    pub async fn wake(self: &Arc<Self>, machine_id: &str) -> Result<Woke, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        let probe = probe.ok_or_else(|| format!("Muxin hasn't reached {} yet, so it doesn't know its network card.", spec.name))?;
        if self.demo_world().is_some() {
            return Ok(Woke { awake: false, sent_from: Some("this computer".into()), message: format!("Sent {} the wake signal from this computer. Muxin looks again in a moment.", spec.name) });
        }
        if probe.asleep {
            match self.rouse(&spec).await {
                Some(Roused::Awake) => return Ok(Woke { awake: true, sent_from: None, message: format!("{} is awake.", spec.name) }),
                Some(Roused::StillAsleep { lid_closed: true }) => {
                    return Ok(Woke { awake: false, sent_from: None, message: format!("{} woke for a moment, but it can't stay awake with its lid closed.", spec.name) });
                }
                Some(Roused::StillAsleep { .. }) => return Ok(Woke { awake: false, sent_from: None, message: format!("{} woke for a moment and went back to sleep.", spec.name) }),
                // No answer: the wake signal, below.
                None => {}
            }
        }
        let mac = probe.mac.clone().ok_or_else(|| match probe.asleep {
            true => format!("{} didn't answer, and Muxin doesn't know its network card to send it the wake signal.", spec.name),
            false => format!("Muxin doesn't know {}'s network card yet: it learns it the next time the machine is on.", spec.name),
        })?;
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
        // A machine takes a little while to wake: look again, twice. A Mac
        // the signal woke is in a dark wake: its agent makes it a full one.
        let me = self.clone();
        self.runtime().spawn(async move {
            for wait in [20, 40] {
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                if matches!(me.rouse(&spec).await, Some(Roused::Awake)) {
                    break;
                }
                me.run_check(Some(vec![spec.id.clone()])).await;
                if me.current().machines.iter().any(|m| m.id == spec.id && m.status == "online") {
                    break;
                }
            }
        });
        let from = from.join(" and ");
        Ok(Woke { awake: false, message: format!("Sent the wake signal from {from}. Muxin looks again in a moment."), sent_from: Some(from) })
    }

    /// Reaches a machine that's asleep, which wakes it for a moment, and has
    /// its agent wake it for real; then checks it. None: it didn't answer.
    async fn rouse(self: &Arc<Self>, spec: &crate::config::MachineSpec) -> Option<Roused> {
        use std::time::Duration;
        // Reached on purpose, though it's asleep.
        let _looking = super::asleep::look(&spec.id);
        let link = super::link::reach(spec).await.ok()?;
        let mut lid_closed = false;
        // Awake after all (it woke, and its notice hasn't come): just look.
        if link.asleep().is_some() || !link.says_sleep() {
            let reply = link.request(json!({ "op": "wake_self" }), Duration::from_secs(20)).await.ok();
            match reply {
                Some(r) if r["ok"].as_bool() == Some(true) => {
                    if r["awake"].as_bool() != Some(true) {
                        link.hang_up();
                        return Some(Roused::StillAsleep { lid_closed: r["lid_closed"].as_bool() == Some(true) });
                    }
                }
                // An agent from before it could: a Mac's own way to say
                // someone is at it, which takes a moment.
                Some(_) if self.machine(&spec.id).and_then(|(_, p)| p).is_some_and(|p| p.os.as_deref() == Some("darwin")) => {
                    let woke = link.request(json!({ "op": "sh", "script": "caffeinate -u -t 3", "login": false, "timeout": 10 }), Duration::from_secs(15)).await;
                    if woke.is_err() {
                        link.hang_up();
                        return None;
                    }
                    lid_closed = self.machine(&spec.id).and_then(|(_, p)| p).is_some_and(|p| p.lid_closed);
                }
                _ => {
                    link.hang_up();
                    return None;
                }
            }
        }
        link.hang_up();
        // Awake now, as far as its agent could tell. Reaching it in its
        // sleep often goes over ssh (its direct way wakes too slowly to
        // answer in time), and that isn't held against the direct way: the
        // check that follows tries it first.
        self.note_woke(&spec.id);
        self.run_check(Some(vec![spec.id.clone()])).await;
        match self.current().machines.iter().any(|m| m.id == spec.id && m.status == "online") {
            true => {
                self.direct_again_soon(&spec.id);
                Some(Roused::Awake)
            }
            false => Some(Roused::StillAsleep { lid_closed }),
        }
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
