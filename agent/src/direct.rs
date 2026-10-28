//! Direct connections: `muxen-agent listen` accepts QUIC connections from
//! Muxen and serves the same link protocol over them, so the app needn't
//! go through ssh once a machine is set up (and video can stream later).
//!
//! Both sides use self-signed certificates and accept only each other's, by
//! SHA-256 fingerprint. The app's fingerprint arrives over ssh when it pairs
//! (`{"op": "pair"}` on a link), which also installs `listen` as a user service
//! (systemd on Linux, launchd on macOS) so it survives reboots. Other apps
//! (an iPad, paired from a Mac) are added with `{"op": "trust"}` once it runs.
//!
//! Files, in ~/.muxen/direct: `cert.der` and `key.der` (this machine's
//! identity), `trusted` (one app fingerprint per line), `port`.

use std::collections::HashSet;
use std::io::Write;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use serde_json::{Value, json};

use crate::home_dir;

pub const DEFAULT_PORT: u16 = 47821;
const ALPN: &[u8] = b"muxen/1";
const SERVICE: &str = "muxen-agent";
const LAUNCHD_LABEL: &str = "com.lumarylabs.muxen-agent";

fn dir() -> PathBuf {
    let d = home_dir().join("direct");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// This machine's certificate and key, made on first use.
fn identity() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), String> {
    let (cert_path, key_path) = (dir().join("cert.der"), dir().join("key.der"));
    if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        return Ok((CertificateDer::from(cert), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))));
    }
    let made = rcgen::generate_simple_self_signed(vec!["muxen-agent".to_string()]).map_err(|e| e.to_string())?;
    let cert = made.cert.der().to_vec();
    let key = made.key_pair.serialize_der();
    write_private(&key_path, &key)?;
    std::fs::write(&cert_path, &cert).map_err(|e| e.to_string())?;
    Ok((CertificateDer::from(cert), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))))
}

/// Writes a file only this user can read (on Windows, the user's profile already is).
fn write_private(path: &PathBuf, bytes: &[u8]) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())
}

fn trusted() -> HashSet<String> {
    std::fs::read_to_string(dir().join("trusted")).unwrap_or_default().lines().map(|l| l.trim().to_lowercase()).filter(|l| l.len() == 64).collect()
}

pub fn port() -> u16 {
    std::fs::read_to_string(dir().join("port")).ok().and_then(|p| p.trim().parse().ok()).unwrap_or(DEFAULT_PORT)
}

/// Accepts only the apps paired with this machine.
#[derive(Debug)]
struct PairedApps {
    trusted: Arc<RwLock<HashSet<String>>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for PairedApps {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, cert: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        // Re-read, so pairing takes effect without a restart.
        *self.trusted.write().unwrap() = trusted();
        if self.trusted.read().unwrap().contains(&fingerprint(cert.as_ref())) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General("this app isn't paired with this machine".into()))
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

/// `muxen-agent listen`: serves links over QUIC until stopped.
pub fn listen() -> i32 {
    // Programs that need the logged-in session start here (see gui.rs).
    #[cfg(unix)]
    crate::gui::serve();
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("muxen-agent: {e}");
            return 1;
        }
    };
    match runtime.block_on(accept_loop()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("muxen-agent listen: {e}");
            1
        }
    }
}

async fn accept_loop() -> Result<(), String> {
    let (cert, key) = identity()?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PairedApps { trusted: Arc::new(RwLock::new(trusted())), algorithms: provider.signature_verification_algorithms };
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(vec![cert], key)
        .map_err(|e| e.to_string())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(|e| e.to_string())?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
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
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port()));
    let socket = udp_socket(addr).map_err(|e| format!("can't listen on UDP port {}: {e}", addr.port()))?;
    let endpoint = quinn::Endpoint::new(quinn::EndpointConfig::default(), Some(config), socket, Arc::new(quinn::TokioRuntime))
        .map_err(|e| format!("can't listen on UDP port {}: {e}", addr.port()))?;
    eprintln!("muxen-agent {} listening on UDP {}", crate::VERSION, addr.port());
    // Stopped (an update, a restart): tell connected apps right away, so they
    // reconnect instead of waiting for silence.
    let closing = endpoint.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        }
        #[cfg(windows)]
        {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
        }
        closing.close(0u32.into(), b"restarting");
        let _ = tokio::time::timeout(Duration::from_millis(500), closing.wait_idle()).await;
        std::process::exit(0);
    });
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            // An app unpaired while it's connected (an iPad removed on the
            // Mac) goes within seconds, not when it next reconnects.
            let app = conn.peer_identity().and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok()).and_then(|certs| certs.first().map(|c| fingerprint(c.as_ref())));
            if let Some(app) = app {
                let watched = conn.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = watched.closed() => return,
                            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        }
                        if !trusted().contains(&app) {
                            watched.close(1u32.into(), b"unpaired");
                            return;
                        }
                    }
                });
            }
            // Each bidirectional stream is one link.
            while let Ok((send, recv)) = conn.accept_bi().await {
                tokio::spawn(bridge(send, recv));
            }
        });
    }
    Ok(())
}

/// Runs a link (the same code as `muxen-agent link`) over a QUIC stream.
/// The listener's UDP socket: IPv6 and IPv4 both, with big buffers (Linux's
/// default receive buffer drops a copy's bursts).
fn udp_socket(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        let _ = s.set_only_v6(false);
    }
    let _ = s.set_send_buffer_size(4 << 20);
    let _ = s.set_recv_buffer_size(4 << 20);
    s.bind(&addr.into())?;
    Ok(s.into())
}

async fn bridge(mut send: quinn::SendStream, mut recv: quinn::RecvStream) {
    let Ok((ours, theirs)) = stream_pair() else { return };
    let Ok(theirs_out) = theirs.try_clone() else { return };
    std::thread::spawn(move || crate::link::serve(Box::new(theirs), Box::new(theirs_out)));
    let _ = ours.set_nonblocking(true);
    #[cfg(unix)]
    let Ok(ours) = tokio::net::UnixStream::from_std(ours) else { return };
    #[cfg(windows)]
    let Ok(ours) = tokio::net::TcpStream::from_std(ours) else { return };
    let (mut from_link, mut to_link) = ours.into_split();
    // The app's side ending (closed, or gone quiet past the idle timeout) ends
    // the link's input too, so it lets go of what the connection had open
    // (terminals, logs being followed) instead of waiting on it forever.
    let up = async {
        use tokio::io::AsyncWriteExt;
        let r = tokio::io::copy(&mut recv, &mut to_link).await;
        let _ = to_link.shutdown().await;
        r
    };
    let down = async {
        let r = tokio::io::copy(&mut from_link, &mut send).await;
        let _ = send.finish();
        r
    };
    let _ = tokio::join!(up, down);
}

/// Two connected streams: one for the QUIC side, one for the link.
#[cfg(unix)]
fn stream_pair() -> std::io::Result<(std::os::unix::net::UnixStream, std::os::unix::net::UnixStream)> {
    std::os::unix::net::UnixStream::pair()
}

/// Windows has no socket pairs: a loopback connection, taking only the one we made.
#[cfg(windows)]
fn stream_pair() -> std::io::Result<(std::net::TcpStream, std::net::TcpStream)> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let ours = std::net::TcpStream::connect(listener.local_addr()?)?;
    loop {
        let (theirs, from) = listener.accept()?;
        if from == ours.local_addr()? {
            let _ = ours.set_nodelay(true);
            let _ = theirs.set_nodelay(true);
            return Ok((ours, theirs));
        }
    }
}

/// `{"op": "pair", "client": "<app fingerprint>"}` on a link (so over ssh):
/// trusts the app, makes sure `listen` runs as a service, and says how to
/// reach it.
pub fn pair(req: &Value) -> Value {
    let client = req["client"].as_str().unwrap_or("").trim().to_lowercase();
    if client.len() != 64 || !client.chars().all(|c| c.is_ascii_hexdigit()) {
        return json!({ "ok": false, "error": "pair needs the app's certificate fingerprint" });
    }
    let (cert, _) = match identity() {
        Ok(i) => i,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't make this machine's certificate: {e}") }),
    };
    let mut all = trusted();
    all.insert(client);
    let list: String = all.iter().map(|f| format!("{f}\n")).collect();
    if let Err(e) = write_private(&dir().join("trusted"), list.as_bytes()) {
        return json!({ "ok": false, "error": e });
    }
    if let Some(p) = req["port"].as_u64().filter(|p| (1024..=65535).contains(p)) {
        let _ = std::fs::write(dir().join("port"), p.to_string());
    }
    let service = install_service();
    json!({ "ok": true, "fingerprint": fingerprint(cert.as_ref()), "port": port(), "service": service.0, "note": service.1, "addresses": lan_addresses(), "tailnet": tailnet_addresses() })
}

/// `{"op": "trust", "client": "<app fingerprint>"}` on any link: trusts
/// another app (an iPad paired from a Mac) where direct connections are
/// already set up, without restarting `listen` (it reads `trusted` for each
/// new connection, so a direct link can carry this). Says how to reach the
/// machine, as `pair` does; `{"ok": false}` where they aren't set up yet.
pub fn trust(req: &Value) -> Value {
    let client = req["client"].as_str().unwrap_or("").trim().to_lowercase();
    if client.len() != 64 || !client.chars().all(|c| c.is_ascii_hexdigit()) {
        return json!({ "ok": false, "error": "trust needs the app's certificate fingerprint" });
    }
    if !dir().join("trusted").exists() {
        return json!({ "ok": false, "error": "Direct connections aren't set up on this machine yet." });
    }
    let (cert, _) = match identity() {
        Ok(i) => i,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't read this machine's certificate: {e}") }),
    };
    let mut all = trusted();
    all.insert(client);
    let list: String = all.iter().map(|f| format!("{f}\n")).collect();
    if let Err(e) = write_private(&dir().join("trusted"), list.as_bytes()) {
        return json!({ "ok": false, "error": e });
    }
    json!({ "ok": true, "fingerprint": fingerprint(cert.as_ref()), "port": port(), "addresses": lan_addresses(), "tailnet": tailnet_addresses() })
}

/// This machine's addresses on its local networks (not loopback, Docker
/// bridges or VPN tunnels), for apps on the same network to connect directly.
pub fn lan_addresses() -> Vec<String> {
    addresses().0
}

/// Its addresses on a Tailscale network (100.64.0.0/10 on a Tailscale
/// interface, fd7a:115c:a1e0::/48), for apps elsewhere on the tailnet.
pub fn tailnet_addresses() -> Vec<String> {
    addresses().1
}

/// (local network, tailnet) addresses, as Windows lists them.
#[cfg(windows)]
fn addresses() -> (Vec<String>, Vec<String>) {
    let (mut lan, mut tailnet) = (Vec::new(), Vec::new());
    let rows = crate::win::ps_json("Get-NetIPAddress -AddressState Preferred | Where-Object { $_.InterfaceAlias -notmatch 'Loopback' } | Select-Object IPAddress, InterfaceAlias | ConvertTo-Json -Compress").unwrap_or_default();
    for r in rows {
        let (Some(ip), alias) = (r["IPAddress"].as_str().and_then(|a| a.parse::<std::net::IpAddr>().ok()), r["InterfaceAlias"].as_str().unwrap_or("")) else { continue };
        if is_tailnet(ip) && (alias.contains("Tailscale") || ip.is_ipv6()) {
            if !tailnet.contains(&ip.to_string()) {
                tailnet.push(ip.to_string());
            }
            continue;
        }
        // Not WSL's, Hyper-V's or Docker's virtual switches, nor VPNs.
        if ["vEthernet", "WSL", "Docker", "Hyper-V", "VirtualBox", "VMware", "Tailscale", "ZeroTier", "WireGuard"].iter().any(|p| alias.contains(p)) {
            continue;
        }
        if let std::net::IpAddr::V4(v4) = ip {
            if v4.is_private() && !lan.contains(&v4.to_string()) {
                lan.push(v4.to_string());
            }
        }
    }
    tailnet.sort_by_key(|a| a.contains(':'));
    (lan, tailnet)
}

/// (local network, tailnet) addresses.
#[cfg(unix)]
fn addresses() -> (Vec<String>, Vec<String>) {
    let (mut lan, mut tailnet) = (Vec::new(), Vec::new());
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return (lan, tailnet);
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_flags & libc::IFF_UP as u32 == 0 || ifa.ifa_flags & libc::IFF_LOOPBACK as u32 != 0 {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
            let ip: Option<std::net::IpAddr> = match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let a = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    Some(std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).into())
                }
                libc::AF_INET6 => {
                    let a = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    Some(std::net::Ipv6Addr::from(a.sin6_addr.s6_addr).into())
                }
                _ => None,
            };
            let Some(ip) = ip else { continue };
            let tunnel = name.starts_with("tailscale") || name.starts_with("utun");
            if is_tailnet(ip) && (tunnel || ip.is_ipv6()) {
                if !tailnet.contains(&ip.to_string()) {
                    tailnet.push(ip.to_string());
                }
                continue;
            }
            if ["docker", "br-", "veth", "virbr", "tailscale", "utun", "wg", "zt", "lo", "awdl", "llw", "bridge"].iter().any(|p| name.starts_with(p)) {
                continue;
            }
            if let std::net::IpAddr::V4(v4) = ip {
                if v4.is_private() && !lan.contains(&v4.to_string()) {
                    lan.push(v4.to_string());
                }
            }
        }
        libc::freeifaddrs(list);
    }
    // IPv4 first: it's what Tailscale's MagicDNS hands out first too.
    tailnet.sort_by_key(|a| a.contains(':'));
    (lan, tailnet)
}

fn is_tailnet(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 64
        }
        std::net::IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// `muxen-agent migrate`, run after an update: the direct-connection service
/// from before the rename (OpenRunner) becomes Muxen's, and an installed one
/// is brought up to date. A running `listen` that no service restarts (see
/// `restart_listener`) starts again from the new build.
pub fn migrate() -> i32 {
    if cfg!(windows) {
        narrow_firewall_rule();
        restart_listener();
        return 0;
    }
    let home = crate::sys::user_home();
    let old_unit = home.join(".config/systemd/user/openrunner-agent.service");
    let old_plist = home.join("Library/LaunchAgents/com.lumarylabs.openrunner-agent.plist");
    if old_unit.exists() {
        let _ = run("systemctl", &["--user", "disable", "--now", "openrunner-agent.service"]);
        let _ = std::fs::remove_file(&old_unit);
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    } else if old_plist.exists() {
        let _ = run("launchctl", &["bootout", &format!("gui/{}/com.lumarylabs.openrunner-agent", uid())]);
        let _ = std::fs::remove_file(&old_plist);
    } else {
        refresh_unit();
        restart_listener();
        return 0;
    }
    if dir().join("trusted").exists() {
        install_service();
    }
    0
}

/// After an update, `listen` still runs the build from before (Windows lets a
/// running program's file be moved aside; Unix keeps the replaced file for
/// whoever runs it), so direct connections would reach the old agent. One
/// that systemd or launchd runs is restarted by the update itself (see
/// upload_script in the app); this stops the others and starts them again
/// from this build: Windows' task, and one started in the background (a
/// container has no systemd).
fn restart_listener() {
    if !home_dir().join("direct").is_dir() {
        return;
    }
    #[cfg(windows)]
    {
        let script = format!("if (-not @({LISTENERS}).Count) {{ 'idle'; exit 0 }}\n{}\"running=$running\"", restart_task_ps());
        match crate::win::ps(&script) {
            Ok(out) if out.contains("idle") || out.contains("running=True") => {}
            // Nobody signed in, so the task waits for that: until then, in the background.
            _ => {
                let _ = crate::sys::spawn_background(&["listen"], dir().join("listen.log"));
            }
        }
    }
    #[cfg(unix)]
    {
        let managed = if cfg!(target_os = "macos") {
            run("launchctl", &["print", &format!("gui/{}/{LAUNCHD_LABEL}", uid())])
        } else {
            run("systemctl", &["--user", "is-enabled", &format!("{SERVICE}.service")])
        };
        // Anchored: the update's own shell has "muxen-agent listen" in its command line too.
        let listener = r"/\.muxen/bin/muxen-agent listen$";
        if managed || !run("pkill", &["-f", listener]) {
            return;
        }
        // It lets go of its port as it ends.
        for _ in 0..30 {
            if !run("pgrep", &["-f", listener]) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = crate::sys::spawn_background(&["listen"], dir().join("listen.log"));
    }
}

/// `{"op": "unpair", "client": "<app fingerprint>"}`: forgets that app, and
/// stops `listen` once no app is left. Without a fingerprint (older apps),
/// forgets every app.
pub fn unpair(req: &Value) -> Value {
    let left = after_unpair(trusted(), req["client"].as_str().unwrap_or(""));
    if left.is_empty() {
        let _ = std::fs::remove_file(dir().join("trusted"));
        remove_service();
    } else {
        let list: String = left.iter().map(|f| format!("{f}\n")).collect();
        if let Err(e) = write_private(&dir().join("trusted"), list.as_bytes()) {
            return json!({ "ok": false, "error": e });
        }
    }
    json!({ "ok": true })
}

/// The apps still trusted once `client` is unpaired ("" for every app).
fn after_unpair(mut trusted: HashSet<String>, client: &str) -> HashSet<String> {
    let client = client.trim().to_lowercase();
    if client.is_empty() {
        trusted.clear();
    } else {
        trusted.remove(&client);
    }
    trusted
}

/// This user's id (launchd's domains are by user).
fn uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn run(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| home_dir().join("bin/muxen-agent").display().to_string())
}

/// The systemd unit for `listen`. KillMode=process: restarting it (an update,
/// pairing again) restarts only the listener. The session server it may have
/// started, and every session in it, keeps running.
fn systemd_unit() -> String {
    format!(
        "[Unit]\nDescription=Muxen agent (direct connections)\n\n[Service]\nExecStart={} listen\nRestart=always\nRestartSec=3\nKillMode=process\n\n[Install]\nWantedBy=default.target\n",
        exe()
    )
}

/// Before 0.5.7 the firewall rule let the agent in from any address: one made
/// then is narrowed to FIREWALL_REMOTE. Only where pairing made one; changing
/// it takes an administrator, as making it did, and otherwise it stays as it was.
#[cfg(windows)]
fn narrow_firewall_rule() {
    if !home_dir().join("direct").is_dir() {
        return;
    }
    let _ = crate::win::ps(&format!(
        "try {{ Get-NetFirewallRule -DisplayName {task} -ErrorAction Stop | Where-Object {{ ($_ | Get-NetFirewallAddressFilter).RemoteAddress -contains 'Any' }} | Set-NetFirewallRule -RemoteAddress {remote} -ErrorAction Stop }} catch {{ }}",
        task = crate::win::quote(TASK),
        remote = FIREWALL_REMOTE
    ));
}

#[cfg(not(windows))]
fn narrow_firewall_rule() {}

/// Brings an installed systemd unit up to date (without restarting it), so the
/// restart after an update already leaves sessions alone.
fn refresh_unit() {
    let Ok(home) = std::env::var("HOME") else { return };
    if cfg!(windows) {
        return;
    }
    let path = PathBuf::from(home).join(format!(".config/systemd/user/{SERVICE}.service"));
    let Ok(current) = std::fs::read_to_string(&path) else { return };
    let wanted = systemd_unit();
    if current != wanted && std::fs::write(&path, wanted).is_ok() {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
}

/// Starts the background service (installing it when needed), for what has
/// to run in the logged-in session (see gui.rs).
#[cfg_attr(windows, allow(dead_code))]
pub fn start_service() {
    let _ = install_service();
}

/// The scheduled task that runs `listen` on Windows.
#[cfg(windows)]
const TASK: &str = "Muxen agent";

/// Where Windows' firewall lets apps in from (a PowerShell list): the
/// machine's own subnets, and Tailscale's addresses (see `is_tailnet`).
#[cfg(windows)]
const FIREWALL_REMOTE: &str = "'LocalSubnet', '100.64.0.0/10', 'fd7a:115c:a1e0::/48'";

/// PowerShell: the running `listen` processes.
#[cfg(windows)]
const LISTENERS: &str = "Get-CimInstance Win32_Process -Filter \"Name='muxen-agent.exe'\" | Where-Object { $_.CommandLine -match ' listen' }";

/// PowerShell that stops every `listen` and starts the task again, so the
/// listener runs the build in place; `$running` says whether one came up.
#[cfg(windows)]
fn restart_task_ps() -> String {
    format!(
        "Stop-ScheduledTask -TaskName {task} -ErrorAction SilentlyContinue\n\
         {LISTENERS} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}\n\
         Start-ScheduledTask -TaskName {task}\n\
         $running = $false\n\
         foreach ($i in 1..20) {{ Start-Sleep -Milliseconds 250; if (@({LISTENERS}).Count) {{ $running = $true; break }} }}\n",
        task = crate::win::quote(TASK)
    )
}

/// Runs `listen` as a service of this user: (how, a note when it couldn't).
fn install_service() -> (&'static str, Option<String>) {
    #[cfg(windows)]
    {
        // A task at sign-in, in the user's own desktop session (where the
        // screen and clipboard are), through a windowless console (nothing
        // shows on their screen), started now too; and a way in through
        // Windows' firewall for the agent (administrators only), on any
        // network but only from its own subnets and from the tailnet.
        let script = format!(
            "$exe = {exe}\n\
             $action = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument ('--headless \"' + $exe + '\" listen')\n\
             $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME\n\
             $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited\n\
             $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew\n\
             Register-ScheduledTask -TaskName {task} -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null\n\
             try {{ Remove-NetFirewallRule -DisplayName {task} -ErrorAction SilentlyContinue; New-NetFirewallRule -DisplayName {task} -Direction Inbound -Program $exe -Protocol UDP -RemoteAddress {remote} -Action Allow -Profile Any -ErrorAction Stop | Out-Null; $fw = 'yes' }} catch {{ $fw = 'no' }}\n\
             {restart}\
             \"fw=$fw running=$running\"",
            exe = crate::win::quote(&exe()),
            task = crate::win::quote(TASK),
            remote = FIREWALL_REMOTE,
            restart = restart_task_ps()
        );
        return match crate::win::ps(&script) {
            Ok(out) if out.contains("running=True") => ("task", (!out.contains("fw=yes")).then(|| "Windows' firewall may block it: allow muxen-agent.exe, or pair as an administrator".to_string())),
            // Nobody signed in to the desktop: the task waits for that; meanwhile, run it until restart.
            Ok(_) => match crate::sys::spawn_background(&["listen"], dir().join("listen.log")) {
                Ok(()) => ("task", Some("starts when you sign in to Windows; running now until then".into())),
                Err(e) => ("none", Some(format!("couldn't start it: {e}"))),
            },
            Err(e) => ("none", Some(format!("couldn't set up the task: {e}"))),
        };
    }
    #[allow(unreachable_code)]
    let home = std::env::var("HOME").unwrap_or_default();
    if cfg!(target_os = "macos") {
        let plist_dir = PathBuf::from(&home).join("Library/LaunchAgents");
        let _ = std::fs::create_dir_all(&plist_dir);
        let plist = plist_dir.join(format!("{LAUNCHD_LABEL}.plist"));
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>listen</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>AbandonProcessGroup</key><true/>
  <key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
            exe(),
            home_dir().join("direct/listen.log").display()
        );
        if std::fs::write(&plist, body).is_err() {
            return ("none", Some("couldn't write the launchd agent".into()));
        }
        let domain = format!("gui/{}", uid());
        let _ = run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
        if run("launchctl", &["bootstrap", &domain, &plist.display().to_string()]) {
            return ("launchd", None);
        }
    } else if run("systemctl", &["--user", "show-environment"]) {
        let unit_dir = PathBuf::from(&home).join(".config/systemd/user");
        let _ = std::fs::create_dir_all(&unit_dir);
        if std::fs::write(unit_dir.join(format!("{SERVICE}.service")), systemd_unit()).is_ok()
            && run("systemctl", &["--user", "daemon-reload"])
            && run("systemctl", &["--user", "enable", "--now", &format!("{SERVICE}.service")])
        {
            let _ = run("systemctl", &["--user", "restart", &format!("{SERVICE}.service")]);
            return ("systemd", None);
        }
    }
    // No service manager for this user: run it in the background until reboot.
    let spawned = crate::sys::spawn_background(&["listen"], dir().join("listen.log")).is_ok();
    if spawned { ("background", Some("runs until the machine restarts".into())) } else { ("none", Some("couldn't start it".into())) }
}

fn remove_service() {
    #[cfg(windows)]
    {
        let task = crate::win::quote(TASK);
        let _ = crate::win::ps(&format!(
            "Unregister-ScheduledTask -TaskName {task} -Confirm:$false -ErrorAction SilentlyContinue\n\
             Remove-NetFirewallRule -DisplayName {task} -ErrorAction SilentlyContinue\n\
             {LISTENERS} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
        ));
        return;
    }
    #[allow(unreachable_code)]
    let home = std::env::var("HOME").unwrap_or_default();
    if cfg!(target_os = "macos") {
        let _ = run("launchctl", &["bootout", &format!("gui/{}/{LAUNCHD_LABEL}", uid())]);
        let _ = std::fs::remove_file(PathBuf::from(&home).join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")));
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", &format!("{SERVICE}.service")]);
        let _ = std::fs::remove_file(PathBuf::from(&home).join(format!(".config/systemd/user/{SERVICE}.service")));
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    let _ = run("pkill", &["-f", "muxen-agent listen"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_tailnet_addresses() {
        assert!(is_tailnet("100.123.240.0".parse().unwrap()));
        assert!(is_tailnet("100.64.0.1".parse().unwrap()));
        assert!(!is_tailnet("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet("192.168.1.4".parse().unwrap()));
        assert!(is_tailnet("fd7a:115c:a1e0::c530:f001".parse().unwrap()));
        assert!(!is_tailnet("fd7a:115c:a1e1::1".parse().unwrap()));
        // Whatever this machine has, never both kinds for one address.
        let (lan, tailnet) = addresses();
        println!("lan {lan:?} tailnet {tailnet:?}");
        assert!(lan.iter().all(|a| !tailnet.contains(a)));
    }

    #[test]
    fn unpairing_one_app_keeps_the_others() {
        let (laptop, desktop) = ("a".repeat(64), "b".repeat(64));
        let both: HashSet<String> = [laptop.clone(), desktop.clone()].into();
        assert_eq!(after_unpair(both.clone(), &format!(" {} ", laptop.to_uppercase())), [desktop.clone()].into());
        assert_eq!(after_unpair(both.clone(), &"c".repeat(64)), both);
        // An app from before says no fingerprint: every app goes, as it did.
        assert!(after_unpair(both, "").is_empty());
    }
}
