//! WebRTC for both sides of a share: a peer connection with one data
//! channel, its events as a stream, and the certificate checks.

use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use rtc::peer_connection::certificate::CertificateParams;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::crypto::{self, SignatureScheme};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCCertificate, RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState,
};

/// The data channel's label.
pub const LABEL: &str = "muxin";
/// A guest that falls this far behind (bytes waiting to go out) is dropped
/// rather than held in memory.
pub const MAX_BUFFERED: usize = 16 << 20;

/// What a peer connection reports.
pub enum Event {
    /// Every local candidate is known: the description is whole.
    Gathered,
    State(RTCPeerConnectionState),
    Channel(Arc<dyn DataChannel>),
}

struct Handler(mpsc::UnboundedSender<Event>);

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.0.send(Event::Gathered);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.0.send(Event::State(state));
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let _ = self.0.send(Event::Channel(dc));
    }
}

/// A new share's certificate and its fingerprint.
pub fn certificate() -> Result<(RTCCertificate, [u8; 32]), String> {
    let provider = crypto::default_provider().map_err(|e| e.to_string())?;
    let params = CertificateParams::new(vec!["muxin-share".to_string()]).map_err(|e| e.to_string())?;
    let cert = RTCCertificate::generate(provider.crypto(), SignatureScheme::EcdsaP256Sha256, params).map_err(|e| e.to_string())?;
    let fp = cert.get_fingerprints(provider.crypto()).map_err(|e| e.to_string())?.into_iter().next().ok_or("no fingerprint")?;
    let bytes = super::ticket::fingerprint_bytes(&fp.value).ok_or("odd fingerprint")?;
    Ok((cert, bytes))
}

/// A peer connection using the server's STUN and TURN servers (`ice`), and
/// `cert` when given (the host's, which guests check).
pub async fn peer(ice: &Value, cert: Option<RTCCertificate>) -> Result<(Arc<dyn PeerConnection>, mpsc::UnboundedReceiver<Event>), String> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut config = RTCConfigurationBuilder::new().with_ice_servers(ice_servers(ice));
    if let Some(c) = cert {
        config = config.with_certificates(vec![c]);
    }
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config.build())
        .with_handler(Arc::new(Handler(tx)))
        .with_runtime(runtime())
        .with_udp_addrs(vec!["0.0.0.0:0".to_string(), "[::]:0".to_string()])
        .build()
        .await
        .map_err(|e| format!("Couldn't start a connection: {e}"))?;
    Ok((Arc::new(pc), rx))
}

fn runtime() -> Arc<dyn webrtc::runtime::Runtime> {
    static RT: std::sync::OnceLock<Arc<dyn webrtc::runtime::Runtime>> = std::sync::OnceLock::new();
    RT.get_or_init(|| webrtc::runtime::default_runtime().expect("tokio runtime")).clone()
}

/// The server's `ice` list as WebRTC takes it.
fn ice_servers(ice: &Value) -> Vec<RTCIceServer> {
    ice.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let urls: Vec<String> = match &s["urls"] {
                Value::String(u) => vec![u.clone()],
                Value::Array(a) => a.iter().filter_map(|u| u.as_str().map(String::from)).collect(),
                _ => return None,
            };
            // STUN, and TURN over UDP: what the WebRTC here speaks (browsers
            // take the rest of the list).
            let urls: Vec<String> = urls.into_iter().filter(|u| u.starts_with("stun:") || (u.starts_with("turn:") && (!u.contains("transport=") || u.contains("transport=udp")))).collect();
            (!urls.is_empty()).then(|| RTCIceServer {
                urls,
                username: s["username"].as_str().unwrap_or("").to_string(),
                credential: s["credential"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// The one SHA-256 certificate fingerprint in an SDP, as lowercase hex; None
/// when there's none, or more than one that differ.
pub fn fingerprint(sdp: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in sdp.lines() {
        let Some(rest) = line.trim().strip_prefix("a=fingerprint:") else { continue };
        let (algo, value) = rest.split_once(' ')?;
        if !algo.eq_ignore_ascii_case("sha-256") {
            return None;
        }
        let hex = value.trim().replace(':', "").to_lowercase();
        if hex.len() != 64 || found.as_ref().is_some_and(|f| *f != hex) {
            return None;
        }
        found = Some(hex);
    }
    found
}

/// Waits until the description has the addresses that matter: this
/// computer's own, its public one when there's a STUN server (`ice`), and
/// the relay's when there's a relay. They come within a few hundred ms;
/// the end of gathering itself can take far longer (or never come), and
/// the description goes whole, so waiting for it would only delay joining.
pub async fn gathered(pc: &Arc<dyn PeerConnection>, events: &mut mpsc::UnboundedReceiver<Event>, ice: &Value) {
    let servers = ice_servers(ice);
    let has = |scheme: &str| servers.iter().any(|s| s.urls.iter().any(|u| u.starts_with(scheme)));
    let (public, relay) = (has("stun:"), has("turn:"));
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
    while tokio::time::Instant::now() < deadline {
        if matches!(events.try_recv(), Ok(Event::Gathered)) {
            return;
        }
        let sdp = pc.local_description().await.map(|d| d.sdp).unwrap_or_default();
        if sdp.contains("typ host") && (!public || sdp.contains("typ srflx")) && (!relay || sdp.contains("typ relay")) {
            // A moment for the other networks' addresses.
            tokio::time::sleep(Duration::from_millis(100)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sends a frame, waiting while too much is already on its way; an error
/// when the other side has gone or fell too far behind.
pub async fn send(dc: &Arc<dyn DataChannel>, frame: Vec<u8>) -> Result<(), String> {
    let mut waited = Duration::ZERO;
    while dc.outstanding_bytes().await.unwrap_or(0) > MAX_BUFFERED {
        if waited > Duration::from_secs(10) {
            return Err("the other side fell too far behind".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    }
    dc.send(BytesMut::from(&frame[..])).await.map_err(|e| e.to_string())
}

/// Waits (briefly) until what was sent has gone out, so a last word
/// arrives before the connection closes.
pub async fn flush(dc: &Arc<dyn DataChannel>) {
    for _ in 0..40 {
        if dc.outstanding_bytes().await.unwrap_or(0) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// What arrives on a data channel, as frames (None: it closed).
pub async fn next_message(dc: &Arc<dyn DataChannel>) -> Option<Vec<u8>> {
    loop {
        match dc.poll().await? {
            DataChannelEvent::OnMessage(m) => return Some(m.data.to_vec()),
            DataChannelEvent::OnClose => return None,
            _ => {}
        }
    }
}

/// Waits for a data channel to open (false: it closed first, or took too long).
pub async fn opened(dc: &Arc<dyn DataChannel>) -> bool {
    let wait = async {
        loop {
            match dc.poll().await {
                Some(DataChannelEvent::OnOpen) => return true,
                Some(DataChannelEvent::OnClose) | None => return false,
                Some(_) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait).await.unwrap_or(false)
}

/// A JSON message on channel 0.
pub fn json_frame(v: &Value) -> Vec<u8> {
    super::frame(0, super::JSON, v.to_string().as_bytes())
}

/// An `{"event": "error"}` to tell the other side why.
pub fn error_frame(why: &str) -> Vec<u8> {
    json_frame(&json!({ "event": "error", "error": why }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_one_fingerprint() {
        let a = "AA:".repeat(31) + "AA";
        let b = "BB:".repeat(31) + "BB";
        let one = format!("v=0\r\na=fingerprint:sha-256 {a}\r\nm=application\r\na=fingerprint:sha-256 {a}\r\n");
        assert_eq!(fingerprint(&one), Some("aa".repeat(32)));
        let two = format!("a=fingerprint:sha-256 {a}\r\na=fingerprint:sha-256 {b}\r\n");
        assert_eq!(fingerprint(&two), None);
        assert_eq!(fingerprint(&format!("a=fingerprint:sha-1 {a}\r\n")), None);
        assert_eq!(fingerprint("v=0\r\n"), None);
    }

    #[test]
    fn keeps_only_stun_and_turn() {
        let servers = ice_servers(&json!([
            { "urls": "stun:stun.example.com:3478" },
            { "urls": ["turn:t.example.com:3478?transport=udp", "turn:t.example.com:80?transport=tcp", "turns:t.example.com:443?transport=tcp", "http://nope"], "username": "u", "credential": "c" },
            { "urls": "javascript:alert(1)" }
        ]));
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].urls, vec!["turn:t.example.com:3478?transport=udp".to_string()]);
        assert_eq!(servers[1].username, "u");
    }
}

