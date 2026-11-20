//! Direct connections to machines' agents: QUIC instead of ssh, once a
//! machine is paired. The app and each agent use self-signed certificates and
//! accept only each other's, by SHA-256 fingerprint; the fingerprints are
//! swapped over ssh when pairing (see agent/src/direct.rs).
//!
//! A machine that's asleep is left alone (asleep.rs). Its agent says when it
//! has woken on a connection this app made beforehand (quiet.rs: TCP to the
//! same address and port, the same pinned certificates).
//!
//! Files, in Muxin's config folder: `identity/cert.der` and
//! `identity/key.der` (this app), `direct.json` (paired machines).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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

/// Where Muxin keeps its files (set once, at startup).
pub fn init(config_dir: PathBuf) {
    #[cfg(test)]
    let config_dir = test_dir(config_dir);
    let _ = DIR.set(config_dir);
}

/// A test run is one process, so the first test to get here would choose the
/// folder for all of them: often one that stays from run to run (with the
/// host keys the ssh tests' servers left in its known_hosts, on ports this
/// run's servers get again), or one its own test deletes while others still
/// use it. A run gets a new folder of its own instead, whichever was asked for.
#[cfg(test)]
fn test_dir(_asked: PathBuf) -> PathBuf {
    static MADE: OnceLock<PathBuf> = OnceLock::new();
    MADE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("muxin-test-app-{}", std::process::id()));
        // One an earlier run left, when its process had this id.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    })
    .clone()
}

/// Muxin's config folder, once set (where this app's keys are kept).
#[cfg_attr(not(any(target_os = "ios", test)), allow(dead_code))]
pub fn config_dir() -> Option<PathBuf> {
    DIR.get().cloned()
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

/// The machines paired for direct connections, by id.
pub fn paired_ids() -> HashSet<String> {
    load().into_keys().collect()
}

/// One address this device has on one of its network interfaces.
#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    /// "en0" (Wi-Fi on an iPhone), "pdp_ip0" (cellular), "utun4" (a VPN).
    pub name: String,
    pub ip: IpAddr,
    /// The size of its network: 24 for 255.255.255.0.
    pub prefix: u8,
}

/// Where this device stands to a paired machine: what explains a machine
/// that doesn't answer an iPhone or iPad that left its network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Way {
    /// This device has a network at all (Wi-Fi, cellular, a cable).
    pub online: bool,
    /// It's on one of the machine's local networks. None when the machine
    /// has no local address to go by (a cloud machine).
    pub near: Option<bool>,
    /// Tailscale is on here.
    pub tailscale: bool,
    /// The machine had a Tailscale address when it was last reached.
    pub on_tailnet: bool,
    /// Its name or address only means something on its own network
    /// ("studio.local", "192.168.1.20").
    pub home_only: bool,
}

/// [`Way`] for each paired machine, from this device's networks right now.
/// Empty when the system won't list them.
pub fn ways() -> HashMap<String, Way> {
    let Some(here) = interfaces() else { return HashMap::new() };
    load().iter().map(|(id, p)| (id.clone(), way(p, &here))).collect()
}

pub fn way(p: &Paired, here: &[Interface]) -> Way {
    let starts = |i: &Interface, names: &[&str]| names.iter().any(|n| i.name.starts_with(n));
    let tunnel = |i: &Interface| starts(i, &["utun", "tailscale", "ipsec", "ppp", "wg", "zt"]);
    let cellular = |i: &Interface| starts(i, &["pdp_ip", "rmnet", "wwan"]);
    let lan: Vec<Ipv4Addr> = p.lan.iter().filter_map(|a| a.parse().ok()).collect();
    let shares = |i: &Interface| match i.ip {
        IpAddr::V4(me) if (1..32).contains(&i.prefix) => {
            let mask = u32::MAX << (32 - i.prefix);
            lan.iter().any(|m| u32::from(me) & mask == u32::from(*m) & mask)
        }
        _ => false,
    };
    let host = p.host.trim().trim_end_matches('.').to_ascii_lowercase();
    let host_ip = host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
    let local_name = [".local", ".lan", ".home", ".home.arpa", ".internal", ".localdomain"].iter().any(|s| host.ends_with(s)) || !host.contains('.');
    Way {
        online: here.iter().any(|i| !tunnel(i)),
        near: (!lan.is_empty()).then(|| here.iter().any(|i| !tunnel(i) && !cellular(i) && shares(i))),
        // Carriers hand out 100.64.0.0/10 too: Tailscale's is the one on a tunnel.
        tailscale: here.iter().any(|i| is_tailnet(i.ip) && (tunnel(i) || i.ip.is_ipv6())),
        on_tailnet: p.tailnet.iter().any(|a| a.parse::<IpAddr>().is_ok()) || host_ip.is_some_and(is_tailnet) || host.ends_with(".ts.net"),
        home_only: match host_ip {
            Some(IpAddr::V4(a)) => a.is_private() || a.is_loopback() || a.is_link_local(),
            // fc00::/7 (a private network's own, Tailscale's aside) and fe80::/10.
            Some(IpAddr::V6(a)) => a.is_loopback() || (a.segments()[0] & 0xfe00 == 0xfc00 && !is_tailnet(a.into())) || a.segments()[0] & 0xffc0 == 0xfe80,
            None => local_name,
        },
    }
}

/// An address Tailscale gives: 100.64.0.0/10, or fd7a:115c:a1e0::/48.
fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.octets()[0] == 100 && a.octets()[1] & 0xc0 == 64,
        IpAddr::V6(a) => a.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// This device's addresses by interface, without loopback and link-local
/// ones. None when the system won't say.
#[cfg(unix)]
pub fn interfaces() -> Option<Vec<Interface>> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs gives a list that's read here and freed once, below;
    // each entry's pointers are checked before they're read, as the family says.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_flags & libc::IFF_UP as u32 == 0 || ifa.ifa_flags & libc::IFF_LOOPBACK as u32 != 0 {
                continue;
            }
            let v4 = |sa: *const libc::sockaddr| Ipv4Addr::from(u32::from_be((*(sa as *const libc::sockaddr_in)).sin_addr.s_addr));
            // (The mask is read as the address's family: its own isn't always set.)
            let (ip, prefix): (IpAddr, u8) = match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET if v4(ifa.ifa_addr).is_link_local() => continue,
                libc::AF_INET => (v4(ifa.ifa_addr).into(), if ifa.ifa_netmask.is_null() { 0 } else { u32::from(v4(ifa.ifa_netmask)).leading_ones() as u8 }),
                libc::AF_INET6 => {
                    let a = std::net::Ipv6Addr::from((*(ifa.ifa_addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr);
                    if a.segments()[0] & 0xffc0 == 0xfe80 {
                        continue;
                    }
                    // Nothing here goes by an IPv6 network's size.
                    (a.into(), 0)
                }
                _ => continue,
            };
            out.push(Interface { name: std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned(), ip, prefix });
        }
        libc::freeifaddrs(list);
    }
    Some(out)
}

#[cfg(not(unix))]
pub fn interfaces() -> Option<Vec<Interface>> {
    None
}

/// Machines the user keeps on ssh: Muxin doesn't suggest connecting to them
/// directly. `ssh-only.json`, a list of machine ids.
fn ssh_only_store() -> Option<PathBuf> {
    DIR.get().map(|d| d.join("ssh-only.json"))
}

pub fn ssh_only() -> HashSet<String> {
    ssh_only_store().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Keeps a machine on ssh (no more suggestions to connect directly), or not.
pub fn set_ssh_only(machine_id: &str, keep: bool) -> Result<(), String> {
    let path = ssh_only_store().ok_or("Muxin's config folder isn't set")?;
    let mut all = ssh_only();
    let changed = if keep { all.insert(machine_id.to_string()) } else { all.remove(machine_id) };
    if !changed {
        return Ok(());
    }
    let mut list: Vec<String> = all.into_iter().collect();
    list.sort();
    crate::config::write_atomic(&path, &serde_json::to_vec_pretty(&list).unwrap_or_default()).map_err(|e| e.to_string())
}

pub fn save(machine_id: &str, entry: Option<Paired>) -> Result<(), String> {
    let path = store().ok_or("Muxin's config folder isn't set")?;
    let mut all = load();
    let unpaired = entry.is_none();
    match entry {
        Some(e) => all.insert(machine_id.to_string(), e),
        None => all.remove(machine_id),
    };
    crate::config::write_atomic(&path, &serde_json::to_vec_pretty(&all).unwrap_or_default()).map_err(|e| e.to_string())?;
    failures().lock().unwrap().remove(machine_id);
    // No longer paired: nothing waits to hear it wake.
    if unpaired {
        super::quiet::let_go(machine_id);
    }
    Ok(())
}

/// Try the direct connection again right away (after the agent was updated,
/// or the machine woke).
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
    let dir = DIR.get().ok_or("Muxin's config folder isn't set")?.join("identity");
    let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
    if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        return Ok((CertificateDer::from(cert), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))));
    }
    crate::config::create_private_dir(&dir).map_err(|e| e.to_string())?;
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
    if held_back(machine_id) {
        return Err("a direct connection failed a moment ago".into());
    }
    let result = try_connect(p).await;
    // No answer from a machine that's asleep says nothing about the way to
    // it: a sleeping Mac takes longer to wake for a packet than this waits.
    // It isn't held against the next try, which is often the one right
    // after it woke.
    if result.is_err() && !super::asleep::known(machine_id) {
        failures().lock().unwrap().insert(machine_id.to_string(), Instant::now());
    }
    result
}

/// Whether the direct way to the machine is left alone for now, because it
/// didn't answer a moment ago (`RETRY_AFTER`).
pub fn held_back(machine_id: &str) -> bool {
    failures().lock().unwrap().get(machine_id).is_some_and(|t| t.elapsed() < RETRY_AFTER)
}

/// For tests: as if the machine's direct way didn't answer just now.
#[cfg(test)]
pub(crate) fn test_hold_back(machine_id: &str) {
    failures().lock().unwrap().insert(machine_id.to_string(), Instant::now());
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

/// How this app calls a paired machine's agent, for `alpn`: as itself, and
/// taking only the agent it was paired with.
pub(super) fn client_tls(p: &Paired, alpn: &[u8]) -> Result<rustls::ClientConfig, String> {
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
    tls.alpn_protocols = vec![alpn.to_vec()];
    Ok(tls)
}

fn client_config(p: &Paired) -> Result<quinn::ClientConfig, String> {
    let tls = client_tls(p, ALPN)?;
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| e.to_string())?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(4)));
    // The machine's link (requests, the desktop, copies) is one stream, each
    // terminal another: let a lot be in flight, or a slow Wi-Fi round trip
    // caps a copy at a few MB/s (quinn's default is 1.25 MB per stream).
    transport.stream_receive_window(quinn::VarInt::from_u32(16 << 20));
    transport.receive_window(quinn::VarInt::from_u32(32 << 20));
    transport.send_window(32 << 20);
    transport.max_idle_timeout(Some(Duration::from_secs(12).try_into().map_err(|_| "bad timeout")?));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

/// The machine's address as ssh resolves it (aliases in ~/.ssh/config included).
pub async fn resolve_host(spec: &MachineSpec) -> Option<String> {
    // An iPad can't run `ssh -G`.
    if cfg!(target_os = "ios") {
        return None;
    }
    let out = tokio::process::Command::new("ssh").arg("-G").args(probe::ssh_args(spec)).output().await.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix("hostname ").map(|h| h.trim().to_string())).filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(name: &str, ip: &str, prefix: u8) -> Interface {
        Interface { name: name.into(), ip: ip.parse().unwrap(), prefix }
    }

    fn machine(host: &str, lan: &[&str], tailnet: &[&str]) -> Paired {
        let list = |a: &[&str]| a.iter().map(|s| s.to_string()).collect();
        Paired { fingerprint: "f".repeat(64), host: host.into(), port: 47821, lan: list(lan), tailnet: list(tailnet) }
    }

    /// A phone tells whether it's on a machine's network from its own
    /// addresses: the same network on Wi-Fi counts, cellular and tunnels don't.
    #[test]
    fn a_device_knows_whether_its_on_a_machines_network() {
        let studio = machine("studio.local", &["192.168.148.202"], &[]);
        let at_home = way(&studio, &[on("en0", "192.168.148.60", 24)]);
        assert_eq!(at_home, Way { online: true, near: Some(true), tailscale: false, on_tailnet: false, home_only: true });

        // On cellular, and on someone else's Wi-Fi.
        assert_eq!(way(&studio, &[on("pdp_ip0", "10.44.1.9", 32), on("pdp_ip0", "2607:fb90::12", 0)]).near, Some(false));
        assert_eq!(way(&studio, &[on("en0", "192.168.1.60", 24)]).near, Some(false));
        // A carrier's address that looks like home's isn't home.
        let ten = machine("10.0.0.5", &["10.0.0.5"], &[]);
        assert_eq!(way(&ten, &[on("pdp_ip0", "10.0.0.77", 24)]).near, Some(false));
        assert_eq!(way(&ten, &[on("en0", "10.0.0.77", 24)]).near, Some(true));
        // Nor is a VPN's.
        assert_eq!(way(&studio, &[on("utun3", "192.168.148.9", 24), on("pdp_ip0", "10.44.1.9", 32)]).near, Some(false));
        // A network of any size: /16 holds it, /28 doesn't.
        assert_eq!(way(&studio, &[on("en0", "192.168.3.60", 16)]).near, Some(true));
        assert_eq!(way(&studio, &[on("en0", "192.168.148.100", 28)]).near, Some(false));
        // A laptop on this phone's own hotspot is on its network.
        let tethered = machine("laptop.local", &["172.20.10.3"], &[]);
        assert_eq!(way(&tethered, &[on("pdp_ip0", "10.44.1.9", 32), on("bridge100", "172.20.10.1", 28)]).near, Some(true));

        // A machine with no local address (a cloud one): no telling.
        let cloud = machine("203.0.113.9", &[], &[]);
        let w = way(&cloud, &[on("en0", "192.168.148.60", 24)]);
        assert_eq!((w.near, w.home_only), (None, false));
    }

    #[test]
    fn a_device_knows_whether_tailscale_could_carry_it() {
        let studio = machine("studio.local", &["192.168.148.202"], &[]);
        let beelink = machine("adib-ser", &["192.168.148.247"], &["100.123.240.0", "fd7a:115c:a1e0::c530:f001"]);
        let cellular = on("pdp_ip0", "10.44.1.9", 32);
        let away = std::slice::from_ref(&cellular);

        // Tailscale is the 100.64.0.0/10 address on a tunnel; a carrier hands those out too.
        assert!(way(&studio, &[cellular.clone(), on("utun4", "100.64.56.41", 32)]).tailscale);
        assert!(way(&studio, &[cellular.clone(), on("utun4", "fd7a:115c:a1e0::1", 0)]).tailscale);
        assert!(!way(&studio, &[on("pdp_ip0", "100.72.3.4", 30)]).tailscale);
        assert!(!way(&studio, &[cellular.clone(), on("utun2", "10.8.0.2", 24)]).tailscale, "another VPN");

        // Which machines it could carry to.
        assert!(way(&beelink, away).on_tailnet);
        assert!(!way(&studio, away).on_tailnet);
        assert!(way(&machine("box.tail0000.ts.net", &["192.168.1.9"], &[]), away).on_tailnet, "added by its Tailscale name");
        assert!(way(&machine("100.100.1.1", &["192.168.1.9"], &[]), away).on_tailnet, "or address");

        // Names that mean something at home only, and ones that work anywhere.
        for (host, home) in [("studio.local", true), ("Studio.local.", true), ("adib-ser", true), ("192.168.1.20", true), ("nas.lan", true), ("fd12:3456::9", true)] {
            assert_eq!(way(&machine(host, &["192.168.1.20"], &[]), away).home_only, home, "{host}");
        }
        for host in ["box.example.com", "203.0.113.9", "2001:db8::9", "fd7a:115c:a1e0::9"] {
            assert!(!way(&machine(host, &["192.168.1.20"], &[]), away).home_only, "{host}");
        }

        // No network at all: a tunnel left up doesn't count as one.
        assert!(!way(&studio, &[]).online);
        assert!(!way(&studio, &[on("utun4", "100.64.56.41", 32)]).online);
        assert!(way(&studio, away).online);
    }

    /// The system's own list: this computer's addresses, none of them
    /// loopback or link-local, each IPv4 one with its network's size.
    #[cfg(unix)]
    #[test]
    fn lists_this_devices_addresses() {
        let here = interfaces().expect("the system lists its interfaces");
        for i in &here {
            assert!(!i.name.is_empty() && !i.ip.is_loopback(), "{i:?}");
            match i.ip {
                IpAddr::V4(a) => assert!(!a.is_link_local() && i.prefix <= 32, "{i:?}"),
                IpAddr::V6(a) => assert!(a.segments()[0] & 0xffc0 != 0xfe80, "{i:?}"),
            }
        }
    }

    /// A direct way that doesn't answer is left alone for a while, and ssh
    /// is used meanwhile. Not when the machine is asleep: a sleeping Mac
    /// takes longer to wake for a packet than a try waits, so its silence
    /// says nothing about the way to it, and the next try (often the one
    /// right after it woke) goes ahead.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_answer_from_a_machine_asleep_doesnt_hold_back_the_next_try() {
        init(std::env::temp_dir().join("muxin-direct-test"));
        // An address nobody answers at.
        let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let nobody = Paired { fingerprint: "f".repeat(64), host: "127.0.0.1".into(), port, lan: vec!["127.0.0.1".into()], tailnet: Vec::new() };
        const HELD: &str = "a direct connection failed a moment ago";
        let (up, nap) = (format!("direct-test-up-{port}"), format!("direct-test-nap-{port}"));

        // A machine that's up, as far as anyone knows: tried once, then left
        // alone for a while.
        assert_ne!(connect(&up, &nobody).await.unwrap_err(), HELD);
        assert!(held_back(&up));
        assert_eq!(connect(&up, &nobody).await.unwrap_err(), HELD);

        // One that's asleep (and looked at on purpose, as a check of it by
        // name or Wake does): every try is a real one.
        const OWNER: usize = 9202;
        super::super::asleep::sync(OWNER, [&nap].into_iter());
        let _looking = super::super::asleep::look(&nap);
        assert!(super::super::asleep::known(&nap) && !super::super::asleep::is(&nap));
        for _ in 0..2 {
            assert_ne!(connect(&nap, &nobody).await.unwrap_err(), HELD);
            assert!(!held_back(&nap));
        }
        super::super::asleep::sync(OWNER, std::iter::empty());
        assert!(!super::super::asleep::known(&nap));

        // What was held against a machine goes when it's to be tried again
        // right away (it woke, its agent was updated).
        retry_now(&up);
        assert!(!held_back(&up));
        assert_ne!(connect(&up, &nobody).await.unwrap_err(), HELD);
    }
}
