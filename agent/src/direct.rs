//! Direct connections: `muxen-agent listen` accepts QUIC connections from
//! Muxen and serves the same link protocol over them, so the app needn't
//! go through ssh once a machine is set up (and video can stream later).
//!
//! Both sides use self-signed certificates and accept only each other's, by
//! SHA-256 fingerprint. The app's fingerprint arrives over ssh when it pairs
//! (`{"op": "pair"}` on a link), which also installs `listen` as a user service
//! (systemd on Linux, launchd on macOS) so it survives reboots.
//!
//! Files, in ~/.muxen/direct: `cert.der` and `key.der` (this machine's
//! identity), `trusted` (one app fingerprint per line), `port`.

use std::collections::HashSet;
use std::io::Write;
use std::net::{Ipv6Addr, SocketAddr};
use std::os::unix::net::UnixStream;
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

fn write_private(path: &PathBuf, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(path).map_err(|e| e.to_string())?;
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
    transport.max_idle_timeout(Some(Duration::from_secs(12).try_into().map_err(|_| "bad timeout")?));
    config.transport_config(Arc::new(transport));
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port()));
    let endpoint = quinn::Endpoint::server(config, addr).map_err(|e| format!("can't listen on UDP port {}: {e}", addr.port()))?;
    eprintln!("muxen-agent {} listening on UDP {}", crate::VERSION, addr.port());
    // Stopped (an update, a restart): tell connected apps right away, so they
    // reconnect instead of waiting for silence.
    let closing = endpoint.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        closing.close(0u32.into(), b"restarting");
        let _ = tokio::time::timeout(Duration::from_millis(500), closing.wait_idle()).await;
        std::process::exit(0);
    });
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            // Each bidirectional stream is one link.
            while let Ok((send, recv)) = conn.accept_bi().await {
                tokio::spawn(bridge(send, recv));
            }
        });
    }
    Ok(())
}

/// Runs a link (the same code as `muxen-agent link`) over a QUIC stream.
async fn bridge(mut send: quinn::SendStream, mut recv: quinn::RecvStream) {
    let Ok((ours, theirs)) = UnixStream::pair() else { return };
    let Ok(theirs_out) = theirs.try_clone() else { return };
    std::thread::spawn(move || crate::link::serve(Box::new(theirs), Box::new(theirs_out)));
    let _ = ours.set_nonblocking(true);
    let Ok(ours) = tokio::net::UnixStream::from_std(ours) else { return };
    let (mut from_link, mut to_link) = ours.into_split();
    let up = async { tokio::io::copy(&mut recv, &mut to_link).await };
    let down = async {
        let r = tokio::io::copy(&mut from_link, &mut send).await;
        let _ = send.finish();
        r
    };
    let _ = tokio::join!(up, down);
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

/// (local network, tailnet) addresses.
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
/// is brought up to date.
pub fn migrate() -> i32 {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let old_unit = home.join(".config/systemd/user/openrunner-agent.service");
    let old_plist = home.join("Library/LaunchAgents/com.lumarylabs.openrunner-agent.plist");
    if old_unit.exists() {
        let _ = run("systemctl", &["--user", "disable", "--now", "openrunner-agent.service"]);
        let _ = std::fs::remove_file(&old_unit);
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    } else if old_plist.exists() {
        let uid = unsafe { libc::getuid() };
        let _ = run("launchctl", &["bootout", &format!("gui/{uid}/com.lumarylabs.openrunner-agent")]);
        let _ = std::fs::remove_file(&old_plist);
    } else {
        refresh_unit();
        return 0;
    }
    if dir().join("trusted").exists() {
        install_service();
    }
    0
}

/// `{"op": "unpair"}`: stops `listen` and forgets every app.
pub fn unpair() -> Value {
    let _ = std::fs::remove_file(dir().join("trusted"));
    remove_service();
    json!({ "ok": true })
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

/// Brings an installed systemd unit up to date (without restarting it), so the
/// restart after an update already leaves sessions alone.
fn refresh_unit() {
    let Ok(home) = std::env::var("HOME") else { return };
    let path = PathBuf::from(home).join(format!(".config/systemd/user/{SERVICE}.service"));
    let Ok(current) = std::fs::read_to_string(&path) else { return };
    let wanted = systemd_unit();
    if current != wanted && std::fs::write(&path, wanted).is_ok() {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
}

/// Runs `listen` as a service of this user: (how, a note when it couldn't).
fn install_service() -> (&'static str, Option<String>) {
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
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
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
    let spawned = {
        use std::os::unix::process::CommandExt;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(dir().join("listen.log"));
        let mut cmd = Command::new(exe());
        cmd.arg("listen").stdin(Stdio::null()).stdout(Stdio::null());
        if let Ok(l) = log {
            cmd.stderr(l);
        }
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().is_ok()
    };
    if spawned { ("background", Some("runs until the machine restarts".into())) } else { ("none", Some("couldn't start it".into())) }
}

fn remove_service() {
    let home = std::env::var("HOME").unwrap_or_default();
    if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        let _ = run("launchctl", &["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")]);
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
}
