//! Waking a machine on this one's network (Wake on LAN): its "magic packet",
//! broadcast from here, for a machine that's asleep where the app isn't.

use serde_json::{Value, json};

/// Six 0xFF, then the network card's address sixteen times.
pub fn packet(mac: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = mac.split([':', '-']).map(|b| u8::from_str_radix(b, 16).ok()).collect::<Option<_>>()?;
    if bytes.len() != 6 {
        return None;
    }
    let mut out = vec![0xFF; 6];
    for _ in 0..16 {
        out.extend(&bytes);
    }
    Some(out)
}

/// `{"op": "wake", "mac", "broadcast"?}`.
pub fn wake(req: &Value) -> Value {
    let Some(p) = req["mac"].as_str().and_then(packet) else {
        return json!({ "ok": false, "error": "That isn't a network card's address." });
    };
    let mut sent = false;
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        let _ = sock.set_broadcast(true);
        let mut to = vec!["255.255.255.255".to_string()];
        if let Some(b) = req["broadcast"].as_str().filter(|b| !b.is_empty()) {
            to.push(b.to_string());
        }
        for addr in to {
            for port in [9, 7] {
                sent |= sock.send_to(&p, (addr.as_str(), port)).is_ok();
            }
        }
    }
    if sent { json!({ "ok": true }) } else { json!({ "ok": false, "error": "Couldn't send on this network." }) }
}

#[cfg(test)]
mod tests {
    use super::packet;

    #[test]
    fn makes_a_magic_packet() {
        let p = packet("3c:22:fb:9a:41:07").unwrap();
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[6..12], &[0x3c, 0x22, 0xfb, 0x9a, 0x41, 0x07]);
        assert_eq!(&p[96..], &[0x3c, 0x22, 0xfb, 0x9a, 0x41, 0x07]);
        assert!(packet("not a mac").is_none());
        assert!(packet("3c:22:fb:9a:41").is_none());
    }
}
