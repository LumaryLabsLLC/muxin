//! WebRTC for both sides of a share: a peer connection with one data
//! channel, its events as a stream, and the certificate checks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
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

/// The largest message sent: a data channel carries 64 KB a message (RFC
/// 8841's default) and maybe no more, so a bigger frame (a whole screen)
/// goes in pieces: `[channel][MORE][part]`..., then the last part with the
/// frame's own kind. The other side puts them back together (`Pieces`).
const PIECE: usize = 48 * 1024;

/// What one channel sends goes one frame at a time, so its pieces never mix
/// with another frame's (other channels' frames may come in between).
fn channel_lock(dc: &Arc<dyn DataChannel>, channel: u32) -> Arc<tokio::sync::Mutex<()>> {
    let key = (Arc::as_ptr(dc) as *const () as usize, channel);
    LOCKS.get_or_init(Default::default).lock().unwrap().entry(key).or_default().clone()
}

/// Each data channel's per-channel send locks, by its address.
static LOCKS: OnceLock<Mutex<HashMap<(usize, u32), Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();

/// Forgets a closed data channel's locks.
pub fn forget(dc: &Arc<dyn DataChannel>) {
    if let Some(locks) = LOCKS.get() {
        let id = Arc::as_ptr(dc) as *const () as usize;
        locks.lock().unwrap().retain(|(d, _), _| *d != id);
    }
}

/// Sends a frame (in pieces when it's big), waiting while too much is
/// already on its way; an error when the other side has gone or fell too far
/// behind. A frame always goes whole: the sending carries on even when the
/// caller stops waiting for it.
pub async fn send(dc: &Arc<dyn DataChannel>, frame: Vec<u8>) -> Result<(), String> {
    let Some((channel, kind, _)) = super::parse_frame(&frame) else { return Ok(()) };
    let lock = channel_lock(dc, channel);
    if frame.len() <= PIECE {
        let _one_at_a_time = lock.lock().await;
        return send_one(dc, frame).await;
    }
    let dc = dc.clone();
    let sending = tokio::spawn(async move {
        let _one_at_a_time = lock.lock().await;
        for part in parts_of(channel, kind, &frame[5..]) {
            send_one(&dc, part).await?;
        }
        Ok(())
    });
    sending.await.map_err(|e| e.to_string())?
}

/// A frame's payload as messages of at most `PIECE` bytes: MORE frames,
/// then the last part with its own kind.
fn parts_of(channel: u32, kind: u8, payload: &[u8]) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = payload.chunks(PIECE - 5).map(|p| super::frame(channel, super::MORE, p)).collect();
    if let Some(last) = parts.last_mut() {
        last[4] = kind;
    }
    parts
}

/// One message, once there's room for it.
async fn send_one(dc: &Arc<dyn DataChannel>, frame: Vec<u8>) -> Result<(), String> {
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

/// Frames put back together from their pieces (see `PIECE`), per channel.
#[derive(Default)]
pub struct Pieces {
    parts: HashMap<u32, Vec<u8>>,
    held: usize,
    /// Channels whose frame was too big: its remaining parts are let go too.
    dropping: std::collections::HashSet<u32>,
}

impl Pieces {
    /// The most held at once: a frame bigger than this is dropped (the
    /// other side sending more than any frame needs).
    const MAX: usize = 64 << 20;

    /// A message off the data channel: the frame, once its last piece is in.
    pub fn add(&mut self, msg: Vec<u8>) -> Option<Vec<u8>> {
        let (channel, kind, body) = super::parse_frame(&msg)?;
        if self.dropping.contains(&channel) {
            // The rest of a frame that was too big, up to and with its last part.
            if kind != super::MORE {
                self.dropping.remove(&channel);
            }
            return None;
        }
        if kind == super::MORE {
            if self.held + body.len() > Self::MAX {
                self.held -= self.parts.remove(&channel).map_or(0, |p| p.len());
                self.dropping.insert(channel);
                return None;
            }
            self.held += body.len();
            self.parts.entry(channel).or_default().extend_from_slice(body);
            return None;
        }
        match self.parts.remove(&channel) {
            None => Some(msg),
            Some(mut whole) => {
                self.held -= whole.len();
                whole.extend_from_slice(body);
                Some(super::frame(channel, kind, &whole))
            }
        }
    }
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
    fn big_frames_go_in_pieces_and_come_back_whole() {
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let parts = parts_of(7, super::super::DATA, &big);
        assert!(parts.len() > 1 && parts.iter().all(|p| p.len() <= PIECE));
        assert!(parts[..parts.len() - 1].iter().all(|p| p[4] == super::super::MORE));
        let mut pieces = Pieces::default();
        // Another channel's frame in between doesn't get mixed in.
        assert_eq!(pieces.add(parts[0].clone()), None);
        let small = super::super::frame(9, super::super::DATA, b"hi");
        assert_eq!(pieces.add(small.clone()), Some(small));
        let mut whole = None;
        for p in &parts[1..] {
            whole = pieces.add(p.clone());
        }
        assert_eq!(whole, Some(super::super::frame(7, super::super::DATA, &big)));
        assert_eq!(pieces.held, 0);
    }

    #[test]
    fn a_frame_too_big_is_let_go_whole() {
        let mut pieces = Pieces::default();
        let part = vec![1u8; PIECE - 5];
        let more = super::super::frame(3, super::super::MORE, &part);
        let mut parts = 0;
        while pieces.held + part.len() <= Pieces::MAX {
            assert_eq!(pieces.add(more.clone()), None);
            parts += 1;
        }
        assert!(parts > 0);
        // Over the limit: this part and the rest go, the last one too.
        assert_eq!(pieces.add(more.clone()), None);
        assert_eq!(pieces.add(more.clone()), None);
        assert_eq!(pieces.add(super::super::frame(3, super::super::DATA, b"end")), None);
        assert_eq!(pieces.held, 0);
        // The next frame on that channel comes through.
        let next = super::super::frame(3, super::super::DATA, b"next");
        assert_eq!(pieces.add(next.clone()), Some(next));
    }

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


#[cfg(test)]
mod relay_check {
    use super::*;

    /// Whether this WebRTC gets a relay address from the share server's
    /// relay (`MUXIN_TEST_SHARE_SERVER`; makes one share's credentials).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn gets_a_relay_address() {
        let server = super::super::signal::Server::new(&std::env::var("MUXIN_TEST_SHARE_SERVER").expect("MUXIN_TEST_SHARE_SERVER"));
        let room = "UkVMQVlDSEVDS1JFTEFZQw";
        let key = super::super::signal::host_key();
        let (ice, _) = server.check_in(room, &key).await.unwrap();
        let (pc, mut events) = peer(&ice, None).await.unwrap();
        let _dc = pc.create_data_channel(LABEL, None).await.unwrap();
        let offer = pc.create_offer(None).await.unwrap();
        let t = std::time::Instant::now();
        pc.set_local_description(offer).await.unwrap();
        gathered(&pc, &mut events, &ice).await;
        let sdp = pc.local_description().await.unwrap().sdp;
        let types: Vec<&str> = sdp.lines().filter_map(|l| l.split(" typ ").nth(1)).map(|t| t.split(' ').next().unwrap_or("")).collect();
        println!("after {:?}: candidate types {types:?}", t.elapsed());
        server.close(room, &key).await;
        assert!(types.contains(&"relay"), "no relay address");
    }
}
