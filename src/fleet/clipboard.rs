//! The shared clipboard, once it's turned on in Settings: text copied on this
//! computer can be pasted on the machines (Linux desktops, Macs), and text
//! copied on one of them lands here and on the others. Each machine's agent
//! runs `muxen-agent clip` on a link channel (see agent/src/clipboard.rs); the
//! window watches this computer's clipboard and calls `local_changed`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::json;
use tokio::sync::broadcast;

use super::link::{self, Link};

/// A clipboard's text on a link channel.
const CLIP: u8 = 40;
/// Bigger text isn't shared.
const MAX: usize = 256 * 1024;

struct Shared {
    /// The text last copied anywhere (so nothing echoes back).
    last: Mutex<String>,
    /// What a program in a terminal here last put on the clipboard (see `keep_here`).
    kept: Mutex<Option<String>>,
    /// Machines sharing the clipboard: their link and channel.
    peers: Mutex<HashMap<String, (Arc<Link>, u32)>>,
    /// Text copied on a machine, for this computer's clipboard.
    incoming: broadcast::Sender<String>,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared { last: Mutex::new(String::new()), kept: Mutex::new(None), peers: Mutex::new(HashMap::new()), incoming: broadcast::channel(16).0 })
}

/// Text copied on a machine, to put on this computer's clipboard.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn subscribe() -> broadcast::Receiver<String> {
    shared().incoming.subscribe()
}

/// Something new was copied on this computer: offer it to every machine.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn local_changed(text: &str) {
    if text.is_empty() || text.len() > MAX || kept(text) {
        return;
    }
    {
        let mut last = shared().last.lock().unwrap();
        if *last == text {
            return;
        }
        *last = text.to_string();
    }
    for (link, channel) in shared().peers.lock().unwrap().values() {
        link.send(*channel, CLIP, text.as_bytes());
    }
}

/// A program in one of the terminals here put this on the clipboard (OSC 52):
/// it stays on this computer, so `local_changed` doesn't offer it to the machines.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn keep_here(text: &str) {
    *shared().kept.lock().unwrap() = Some(text.to_string());
}

/// Whether this is the terminal program's copy, seen once (copying the same
/// text yourself afterwards shares it as usual).
fn kept(text: &str) -> bool {
    let mut kept = shared().kept.lock().unwrap();
    let is = kept.as_deref() == Some(text);
    if is {
        *kept = None;
    }
    is
}

/// Something new was copied on a machine: here, and the other machines.
fn remote_changed(from: &str, text: String) {
    if text.is_empty() || text.len() > MAX {
        return;
    }
    {
        let mut last = shared().last.lock().unwrap();
        if *last == text {
            return;
        }
        *last = text.clone();
    }
    for (id, (link, channel)) in shared().peers.lock().unwrap().iter() {
        if id != from {
            link.send(*channel, CLIP, text.as_bytes());
        }
    }
    let _ = shared().incoming.send(text);
}

/// Shares the clipboard with a machine for as long as its link lasts.
pub async fn share(link: Arc<Link>) {
    // Agents before 0.2.4 can't.
    let v: Vec<u32> = link.version().split('.').map(|p| p.parse().unwrap_or(0)).collect();
    if v.as_slice() < [0, 2, 4].as_slice() || shared().peers.lock().unwrap().contains_key(&link.machine) {
        return;
    }
    let (channel, mut rx) = link.open_channel();
    match link.request(json!({ "op": "clipboard", "channel": channel }), Duration::from_secs(15)).await {
        Ok(r) if r["ok"].as_bool() == Some(true) => {}
        Ok(r) => {
            // A machine without a desktop has no clipboard: that's fine.
            tracing::debug!("no shared clipboard with {}: {}", link.machine, r["error"].as_str().unwrap_or("?"));
            link.close_channel(channel);
            return;
        }
        Err(e) => {
            tracing::debug!("no shared clipboard with {}: {e}", link.machine);
            link.close_channel(channel);
            return;
        }
    }
    tracing::debug!("sharing the clipboard with {}", link.machine);
    shared().peers.lock().unwrap().insert(link.machine.clone(), (link.clone(), channel));
    while let Some((kind, text)) = rx.recv().await {
        if kind == CLIP {
            remote_changed(&link.machine, String::from_utf8_lossy(&text).into_owned());
        }
    }
    let mut peers = shared().peers.lock().unwrap();
    if peers.get(&link.machine).is_some_and(|(_, c)| *c == channel) {
        peers.remove(&link.machine);
    }
}

/// Stops sharing with every machine (the setting was turned off).
pub fn stop_all() {
    for (_, (link, channel)) in shared().peers.lock().unwrap().drain() {
        link.close_channel(channel);
    }
}

/// Whether a machine shares the clipboard right now.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn sharing(machine_id: &str) -> bool {
    shared().peers.lock().unwrap().contains_key(machine_id) && link::current(machine_id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_programs_copy_isnt_offered_to_the_machines() {
        keep_here("from a program");
        assert!(kept("from a program"));
        // The same text copied by hand afterwards is shared as usual.
        assert!(!kept("from a program"));
        keep_here("osc 52");
        assert!(!kept("something else"));
    }
}
