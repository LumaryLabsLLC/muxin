//! A share's link and what it proves: `https://<server>/s#1.<ticket>`, the
//! ticket being base64url of the share's id (16 bytes), secret (32) and
//! certificate fingerprint (32). Muxin also takes `muxin://join?server=
//! <server>#1.<ticket>` (the link's page opening the app).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

const VERSION: &str = "1";

#[derive(Clone, PartialEq, Eq)]
pub struct Ticket {
    /// Where the two sides meet: `https://muxin.dev` (http only for
    /// this computer's own addresses, while developing).
    pub server: String,
    pub id: [u8; 16],
    pub secret: [u8; 32],
    /// SHA-256 of the share's DTLS certificate.
    pub fingerprint: [u8; 32],
}

impl std::fmt::Debug for Ticket {
    // Never the secret, in logs or errors.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket").field("server", &self.server).field("room", &self.room()).finish_non_exhaustive()
    }
}

impl Ticket {
    /// A new share's ticket, for a certificate with this fingerprint.
    pub fn new(server: &str, fingerprint: [u8; 32]) -> Result<Ticket, String> {
        let server = origin(server)?;
        let rng = SystemRandom::new();
        let (mut id, mut secret) = ([0u8; 16], [0u8; 32]);
        rng.fill(&mut id).map_err(|_| "no randomness")?;
        rng.fill(&mut secret).map_err(|_| "no randomness")?;
        Ok(Ticket { server, id, secret, fingerprint })
    }

    /// The link to give out.
    pub fn link(&self) -> String {
        format!("{}/s#{}", self.server, self.fragment())
    }

    /// The part after `#`.
    pub fn fragment(&self) -> String {
        let mut bytes = Vec::with_capacity(80);
        bytes.extend(self.id);
        bytes.extend(self.secret);
        bytes.extend(self.fingerprint);
        format!("{VERSION}.{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Reads a link (or the app's own form of one).
    pub fn parse(link: &str) -> Result<Ticket, String> {
        let link = link.trim();
        let (before, fragment) = link.split_once('#').ok_or("That isn't a Muxin share link.")?;
        let server = if let Some(query) = before.strip_prefix("muxin://join") {
            let query = query.trim_start_matches('/').trim_start_matches('?');
            let value = query.split('&').find_map(|kv| kv.strip_prefix("server=")).ok_or("That link doesn't say where to connect.")?;
            percent_decode(value)
        } else {
            let after_scheme = before.split_once("://").map(|(_, rest)| rest).ok_or("That isn't a Muxin share link.")?;
            let host = after_scheme.split('/').next().unwrap_or("");
            let scheme = before.split_once("://").map(|(s, _)| s).unwrap_or("");
            format!("{scheme}://{host}")
        };
        let server = origin(&server)?;
        let (version, data) = fragment.split_once('.').ok_or("That share link is cut short.")?;
        if version != VERSION {
            return Err("That share link is from a newer Muxin. Update Muxin to open it.".into());
        }
        let bytes = URL_SAFE_NO_PAD.decode(data.trim()).map_err(|_| "That share link is damaged.")?;
        if bytes.len() != 80 {
            return Err("That share link is cut short.".into());
        }
        let mut t = Ticket { server, id: [0; 16], secret: [0; 32], fingerprint: [0; 32] };
        t.id.copy_from_slice(&bytes[..16]);
        t.secret.copy_from_slice(&bytes[16..48]);
        t.fingerprint.copy_from_slice(&bytes[48..]);
        Ok(t)
    }

    /// The room the server knows the share by.
    pub fn room(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.id)
    }

    /// The guest's proof that it has the link, bound to this connection's
    /// two certificates (fingerprints as lowercase hex).
    pub fn proof(&self, host_fp: &str, guest_fp: &str) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.secret);
        hex(hmac::sign(&key, format!("muxin-share-v1\n{host_fp}\n{guest_fp}").as_bytes()).as_ref())
    }

    /// Whether a guest's proof is right (in constant time).
    pub fn check_proof(&self, host_fp: &str, guest_fp: &str, proof: &str) -> bool {
        let Some(tag) = unhex(proof) else { return false };
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.secret);
        hmac::verify(&key, format!("muxin-share-v1\n{host_fp}\n{guest_fp}").as_bytes(), &tag).is_ok()
    }

    /// The 6-digit code both sides show for a guest's connection.
    pub fn code(&self, guest_fp: &str) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.secret);
        let tag = hmac::sign(&key, format!("muxin-share-code-v1\n{guest_fp}").as_bytes());
        let n = u32::from_be_bytes([tag.as_ref()[0], tag.as_ref()[1], tag.as_ref()[2], tag.as_ref()[3]]) % 1_000_000;
        format!("{n:06}")
    }
}

/// `scheme://host[:port]` of a server, refusing plain http except to this
/// computer (a server run while developing).
fn origin(server: &str) -> Result<String, String> {
    let s = server.trim().trim_end_matches('/');
    let (scheme, rest) = s.split_once("://").ok_or("The share server needs to be an https:// address.")?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains(['@', ' ', '?', '#']) {
        return Err("The share server's address isn't valid.".into());
    }
    let bare = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host),
    };
    let local = ["localhost", "127.0.0.1", "::1"].contains(&bare);
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(format!("https://{host}")),
        "http" if local => Ok(format!("http://{host}")),
        _ => Err("The share server needs to be an https:// address.".into()),
    }
}

/// A DTLS fingerprint as SDP writes it (`AB:CD:...`) as 32 bytes.
pub fn fingerprint_bytes(sdp_value: &str) -> Option<[u8; 32]> {
    let bytes = unhex(&sdp_value.replace(':', ""))?;
    bytes.try_into().ok()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && let Some(b) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Ticket {
        Ticket { server: "https://share.example.com".into(), id: [1; 16], secret: [2; 32], fingerprint: [3; 32] }
    }

    #[test]
    fn links_round_trip() {
        let t = Ticket::new("https://share.example.com/", [9; 32]).unwrap();
        let link = t.link();
        assert!(link.starts_with("https://share.example.com/s#1."));
        assert_eq!(Ticket::parse(&link).unwrap(), t);
        let app = format!("muxin://join?server=https%3A%2F%2Fshare.example.com#{}", t.fragment());
        assert_eq!(Ticket::parse(&app).unwrap(), t);
    }

    #[test]
    fn refuses_plain_http_except_here() {
        assert!(Ticket::new("http://share.example.com", [0; 32]).is_err());
        assert!(Ticket::new("ftp://x", [0; 32]).is_err());
        let local = Ticket::new("http://127.0.0.1:8787", [0; 32]).unwrap();
        assert_eq!(local.server, "http://127.0.0.1:8787");
        assert!(Ticket::new("http://localhost:8787", [0; 32]).is_ok());
        assert!(Ticket::new("http://[::1]:8787", [0; 32]).is_ok());
        // A look-alike host isn't this computer.
        assert!(Ticket::new("http://localhost.evil.com", [0; 32]).is_err());
        let mut t = sample();
        t.server = "http://share.example.com".into();
        assert!(Ticket::parse(&format!("http://share.example.com/s#{}", t.fragment())).is_err());
    }

    #[test]
    fn rejects_damaged_links() {
        let t = sample();
        let link = t.link();
        assert!(Ticket::parse(&link[..link.len() - 4]).is_err());
        assert!(Ticket::parse(&link.replace("#1.", "#2.")).is_err());
        assert!(Ticket::parse("https://share.example.com/s").is_err());
        assert!(!format!("{t:?}").contains(&hex(&t.secret)));
    }

    #[test]
    fn proofs_bind_both_certificates() {
        let t = sample();
        let p = t.proof("aa", "bb");
        assert!(t.check_proof("aa", "bb", &p));
        assert!(!t.check_proof("aa", "cc", &p));
        assert!(!t.check_proof("ab", "bb", &p));
        assert!(!t.check_proof("aa", "bb", "zz"));
        let mut other = sample();
        other.secret = [7; 32];
        assert!(!other.check_proof("aa", "bb", &p));
    }

    #[test]
    fn codes_are_six_digits_per_guest() {
        let t = sample();
        let c = t.code("bb");
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        assert_eq!(c, t.code("bb"));
        assert_ne!(c, t.code("cc"));
    }

    #[test]
    fn reads_sdp_fingerprints() {
        let fp = fingerprint_bytes("CD:AC:9F:A6:01:82:0E:7F:6B:FD:50:50:57:35:CB:5D:54:8E:9F:82:AD:6E:90:1F:CC:05:89:FC:5E:B2:CD:B4").unwrap();
        assert_eq!(fp[0], 0xcd);
        assert!(fingerprint_bytes("CD:AC").is_none());
    }
}
