//! Direct connections to machines' agents: QUIC instead of ssh, once a
//! machine is paired. The app and each agent use self-signed certificates and
//! accept only each other's, by SHA-256 fingerprint; the fingerprints are
//! swapped over ssh when pairing (see agent/src/direct.rs).
//!
//! Files, in Muxen's config folder: `identity/cert.der` and
//! `identity/key.der` (this app), `direct.json` (paired machines).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};

use crate::config::MachineSpec;
use crate::probe;

const ALPN: &[u8] = b"muxen/1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// After a direct connection fails, use ssh for this long before trying again
/// (sooner when the network or Tailscale changes: see `retry_all`).
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Home first: the machine's local-network addresses get this long to answer
/// before its Tailscale addresses are tried, and they twice as long before
/// wherever its name leads.
const HEAD_START: Duration = Duration::from_millis(150);

/// How to reach a paired machine directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Paired {
    /// The agent's certificate fingerprint.
    pub fingerprint: String,
    pub host: String,
    pub port: u16,
    /// The machine's own addresses on its local networks: tried first, so a
    /// machine on the same network isn't reached through a VPN or the internet.
    #[serde(default)]
    pub lan: Vec<String>,
    /// Its Tailscale addresses, for reaching it away from its network.
    #[serde(default)]
    pub tailnet: Vec<String>,
}

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where Muxen keeps its files (set once, at startup).
pub fn init(config_dir: PathBuf) {
    let _ = DIR.set(config_dir);
}

fn store() -> Option<PathBuf> {
    DIR.get().map(|d| d.join("direct.json"))
}

fn load() -> HashMap<String, Paired> {
    store().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn paired(machine_id: &str) -> Option<Paired> {
    load().remove(machine_id)
}

pub fn save(machine_id: &str, entry: Option<Paired>) -> Result<(), String> {
    let path = store().ok_or("Muxen's config folder isn't set")?;
    let mut all = load();
    match entry {
        Some(e) => all.insert(machine_id.to_string(), e),
        None => all.remove(machine_id),
    };
    crate::config::write_atomic(&path, &serde_json::to_vec_pretty(&all).unwrap_or_default()).map_err(|e| e.to_string())?;
    failures().lock().unwrap().remove(machine_id);
    Ok(())
}

/// Try the direct connection again right away (after the agent was updated).
pub fn retry_now(machine_id: &str) {
    failures().lock().unwrap().remove(machine_id);
}

/// Try every machine's direct connection again right away (the network or
/// Tailscale changed, so what failed may work now).
pub fn retry_all() {
    failures().lock().unwrap().clear();
}

pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// This app's certificate and key, made on first use.
fn identity() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), String> {
    let dir = DIR.get().ok_or("Muxen's config folder isn't set")?.join("identity");
    let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
    if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        return Ok((CertificateDer::from(cert), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let made = rcgen::generate_simple_self_signed(vec!["muxen-app".to_string()]).map_err(|e| e.to_string())?;
    let (cert, key) = (made.cert.der().to_vec(), made.key_pair.serialize_der());
    {
        use std::io::Write;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        opts.mode(0o600);
        opts.open(&key_path).and_then(|mut f| f.write_all(&key)).map_err(|e| e.to_string())?;
    }
    std::fs::write(&cert_path, &cert).map_err(|e| e.to_string())?;
    Ok((CertificateDer::from(cert), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))))
}

/// This app's fingerprint, which paired agents trust.
pub fn app_fingerprint() -> Result<String, String> {
    identity().map(|(cert, _)| fingerprint(cert.as_ref()))
}

/// Accepts only the agent this machine was paired with.
#[derive(Debug)]
struct PinnedAgent {
    fingerprint: String,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedAgent {
    fn verify_server_cert(&self, cert: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(cert.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("that isn't the agent this machine was paired with".into()))
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn failures() -> &'static Mutex<HashMap<String, Instant>> {
    static FAILED: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    FAILED.get_or_init(Default::default)
}

static V4: OnceLock<quinn::Endpoint> = OnceLock::new();
static V6: OnceLock<quinn::Endpoint> = OnceLock::new();

/// Moves the endpoints to fresh sockets (their connections come along). On
/// macOS a socket made before the user allowed Local Network access stays
/// cut off from the local network; a new one isn't.
pub fn fresh_sockets() {
    for (cell, bind) in [(&V4, "0.0.0.0:0"), (&V6, "[::]:0")] {
        if let Some(e) = cell.get() {
            if let Ok(socket) = udp_socket(bind.parse().expect("valid address")) {
                if let Err(err) = e.rebind(socket) {
                    tracing::debug!("couldn't move to a fresh socket: {err}");
                }
            }
        }
    }
}

/// One client endpoint per address family, shared by every connection.
fn endpoint(v6: bool) -> Result<quinn::Endpoint, String> {
    let (cell, bind) = if v6 { (&V6, "[::]:0") } else { (&V4, "0.0.0.0:0") };
    if let Some(e) = cell.get() {
        return Ok(e.clone());
    }
    let socket = udp_socket(bind.parse().expect("valid address")).map_err(|e| e.to_string())?;
    let e = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, socket, Arc::new(quinn::TokioRuntime)).map_err(|e| e.to_string())?;
    Ok(cell.get_or_init(|| e).clone())
}

/// A UDP socket with room for bursts. macOS gives UDP a 9 KB send buffer, so a
/// copy's packets were dropped on the way out and the connection slowed to a
/// crawl; Linux's default receive buffer is small too.
fn udp_socket(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    let _ = s.set_send_buffer_size(4 << 20);
    let _ = s.set_recv_buffer_size(4 << 20);
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// Opens a direct connection to a paired machine (or says why not).
pub async fn connect(machine_id: &str, p: &Paired) -> Result<quinn::Connection, String> {
    if failures().lock().unwrap().get(machine_id).is_some_and(|t| t.elapsed() < RETRY_AFTER) {
        return Err("a direct connection failed a moment ago".into());
    }
    let result = try_connect(p).await;
    if result.is_err() {
        failures().lock().unwrap().insert(machine_id.to_string(), Instant::now());
    }
    result
}

async fn try_connect(p: &Paired) -> Result<quinn::Connection, String> {
    let config = client_config(p)?;
    // Every way to the machine at once, home first: its local-network
    // addresses right away, its Tailscale ones a moment later, then wherever
    // its name leads (a VPN, the internet). The first to answer wins; the
    // others are dropped.
    let parse = |ips: &[String]| -> Vec<SocketAddr> { ips.iter().filter_map(|ip| ip.parse().ok()).map(|ip| SocketAddr::new(ip, p.port)).collect() };
    let (lan, tailnet) = (parse(&p.lan), parse(&p.tailnet));
    let mut attempts: FuturesUnordered<Attempt> = FuturesUnordered::new();
    for a in &lan {
        attempts.push(attempt(&config, *a, Duration::ZERO, Duration::from_millis(1500), p.port));
    }
    for a in tailnet.iter().filter(|a| !lan.contains(a)) {
        // IPv4 a touch sooner: same path, a shorter address to show.
        let delay = HEAD_START + if a.is_ipv6() { Duration::from_millis(50) } else { Duration::ZERO };
        attempts.push(attempt(&config, *a, delay, Duration::from_secs(4), p.port));
    }
    let (host, port, config2) = (p.host.clone(), p.port, config.clone());
    let known: Vec<SocketAddr> = lan.iter().chain(&tailnet).copied().collect();
    attempts.push(Box::pin(async move {
        tokio::time::sleep(HEAD_START * 2).await;
        let mut by_name: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.map(|a| a.collect()).unwrap_or_default();
        // IPv4 first: home networks and VPNs route it most reliably.
        by_name.sort_by_key(|a| a.is_ipv6());
        by_name.retain(|a| !known.contains(a));
        let mut last = format!("{host} has no other address");
        for a in by_name {
            match attempt(&config2, a, Duration::ZERO, CONNECT_TIMEOUT, port).await {
                Ok(c) => return Ok(c),
                Err(e) => last = e,
            }
        }
        Err(last)
    }));
    let mut errors = Vec::new();
    while let Some(result) = attempts.next().await {
        match result {
            Ok(conn) => return Ok(conn),
            Err(e) => errors.push(e),
        }
    }
    Err(errors.join("; "))
}

type Attempt = std::pin::Pin<Box<dyn std::future::Future<Output = Result<quinn::Connection, String>> + Send>>;

/// One try at one address, after `delay`.
fn attempt(config: &quinn::ClientConfig, addr: SocketAddr, delay: Duration, limit: Duration, port: u16) -> Attempt {
    let config = config.clone();
    Box::pin(async move {
        tokio::time::sleep(delay).await;
        let ep = endpoint(addr.is_ipv6())?;
        let connecting = ep.connect_with(config, addr, "muxen-agent").map_err(|e| format!("{addr}: {e}"))?;
        match tokio::time::timeout(limit, connecting).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(e)) => Err(format!("{addr}: {e}")),
            Err(_) => Err(format!("{addr}: no answer (is UDP port {port} open?)")),
        }
    })
}

/// Whether the machine answers on its local network right now: for moving a
/// connection that went through Tailscale back to the short way.
pub async fn lan_answers(p: &Paired) -> bool {
    let Ok(config) = client_config(p) else { return false };
    let mut attempts: FuturesUnordered<Attempt> =
        p.lan.iter().filter_map(|ip| ip.parse().ok()).map(|ip| attempt(&config, SocketAddr::new(ip, p.port), Duration::ZERO, Duration::from_millis(800), p.port)).collect();
    while let Some(result) = attempts.next().await {
        if let Ok(conn) = result {
            conn.close(0u32.into(), b"just looking");
            return true;
        }
    }
    false
}

fn client_config(p: &Paired) -> Result<quinn::ClientConfig, String> {
    let (cert, key) = identity()?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedAgent { fingerprint: p.fingerprint.clone(), algorithms: provider.signature_verification_algorithms };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| e.to_string())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| e.to_string())?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(4)));
    // Everything to a machine shares one stream (terminals, the desktop,
    // copies): let a lot be in flight, or a slow Wi-Fi round trip caps it at
    // a few MB/s (quinn's default is 1.25 MB per stream).
    transport.stream_receive_window(quinn::VarInt::from_u32(16 << 20));
    transport.receive_window(quinn::VarInt::from_u32(32 << 20));
    transport.send_window(32 << 20);
    transport.max_idle_timeout(Some(Duration::from_secs(12).try_into().map_err(|_| "bad timeout")?));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

/// The machine's address as ssh resolves it (aliases in ~/.ssh/config included).
pub async fn resolve_host(spec: &MachineSpec) -> Option<String> {
    let out = tokio::process::Command::new("ssh").arg("-G").args(probe::ssh_args(spec)).output().await.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix("hostname ").map(|h| h.trim().to_string())).filter(|h| !h.is_empty())
}
