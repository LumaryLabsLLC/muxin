//! Quiet connections: how an app hears that this machine woke, without this
//! machine starting a connection.
//!
//! An app lets go of a machine that sleeps (sleep.rs), so someone has to say
//! when it's awake again. This machine can't call the app: a Mac lets a
//! program start a connection on the local network only once its owner
//! allowed it (Privacy & Security › Local Network), and asks about the agent
//! on every Mac. Answering needs no leave. So the app calls ahead of time,
//! and the call stays open with nothing said on it:
//!
//! - The app connects over TCP to the port `listen` has (the same number as
//!   for QUIC), with TLS and the certificates the two pinned when they
//!   paired (`ALPN`).
//! - This side says `ready`. The app closes its half right away: its goodbye
//!   gets here while the machine is up, so whatever the app does later
//!   (quits, drops the connection, moves to another network) sends nothing.
//! - Then nothing, from either side, and no keep-alives: a sleeping Mac
//!   wakes for any packet, and so does the computer an app runs on.
//! - When the machine has fully woken, this side says `awake` and closes.
//!
//! Only where the agent can tell that its machine sleeps (`sleep::supported`).
//! A connection that can't be made, or is lost, costs nothing: the app then
//! looks at the machine itself, at its checks.

use std::io::{self, Write};
use std::net::{Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// The protocol's name in the TLS handshake. (`muxen-wake/1` was a
/// connection the other way round: the one a Mac asks its owner about.)
pub const ALPN: &[u8] = b"muxen-wake/2";
const READY: &[u8] = b"ready\n";
const AWAKE: &[u8] = b"awake\n";
/// How long a caller gets to say who it is.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// Callers saying who they are at once; more are turned away.
const HANDSHAKES: usize = 16;
/// Connections kept for one app, and in all: past that, the oldest goes. An
/// app that left said nothing (to a machine that may be asleep, it mustn't),
/// so what it left behind stays until the machine next wakes.
const PER_APP: usize = 4;
const IN_ALL: usize = 64;

/// One app waiting to hear that this machine woke.
struct Waiting {
    /// Its certificate's fingerprint.
    app: String,
    from: SocketAddr,
    tcp: TcpStream,
    tls: rustls::ServerConnection,
}

fn waiting() -> &'static Mutex<Vec<Waiting>> {
    static WAITING: OnceLock<Mutex<Vec<Waiting>>> = OnceLock::new();
    WAITING.get_or_init(Default::default)
}

static SERVING: AtomicBool = AtomicBool::new(false);
static HANDSHAKING: AtomicUsize = AtomicUsize::new(0);

/// Whether apps can wait here to hear that the machine woke: a direct
/// link's hello says so (`"wake_notice": "tcp"`).
pub fn serving() -> bool {
    SERVING.load(Ordering::SeqCst)
}

/// Takes quiet connections on TCP `port` from now on, in `listen`.
pub fn serve(port: u16) {
    if !crate::sleep::supported() {
        return;
    }
    // A `listen` this one takes over from may still be letting go of the port.
    let mut bound = listener(port);
    for _ in 0..4 {
        if bound.is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(500));
        bound = listener(port);
    }
    if let Err(e) = bound.and_then(start) {
        // Apps go on without: they look at the machine themselves.
        eprintln!("muxen-agent: apps can't wait to hear this machine wake (TCP port {port}): {e}");
    }
}

/// IPv6 and IPv4 both, as the QUIC listener has it.
fn listener(port: u16) -> Result<TcpListener, String> {
    use socket2::{Domain, Protocol, Socket, Type};
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
    let bind = || -> io::Result<TcpListener> {
        let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        let _ = s.set_only_v6(false);
        // A `listen` started again binds while the one before still lets go.
        let _ = s.set_reuse_address(true);
        s.bind(&addr.into())?;
        s.listen(64)?;
        Ok(s.into())
    };
    bind().map_err(|e| e.to_string())
}

fn start(listener: TcpListener) -> Result<(), String> {
    let mut config = crate::direct::server_tls(ALPN)?;
    // Nothing is sent after `ready` until the machine wakes: no tickets to resume with.
    config.send_tls13_tickets = 0;
    let config = Arc::new(config);
    static TELLING: Once = Once::new();
    TELLING.call_once(|| {
        // For as long as the process runs.
        std::mem::forget(crate::sleep::listen(|change| {
            if change == crate::sleep::Change::Awake {
                tell_all();
            }
        }));
    });
    thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(tcp) = tcp else {
                thread::sleep(Duration::from_millis(200));
                continue;
            };
            if HANDSHAKING.fetch_add(1, Ordering::SeqCst) >= HANDSHAKES {
                HANDSHAKING.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            let config = config.clone();
            thread::spawn(move || {
                let _ = admit(tcp, config);
                HANDSHAKING.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    SERVING.store(true, Ordering::SeqCst);
    Ok(())
}

/// One caller: a paired app (its certificate says which) is kept, and told so.
fn admit(mut tcp: TcpStream, config: Arc<rustls::ServerConfig>) -> Result<(), String> {
    let from = tcp.peer_addr().map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(HANDSHAKE)).and_then(|_| tcp.set_write_timeout(Some(HANDSHAKE))).map_err(|e| e.to_string())?;
    let mut tls = rustls::ServerConnection::new(config).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + HANDSHAKE;
    while tls.is_handshaking() {
        if Instant::now() > deadline {
            return Err("it took too long to say who it is".into());
        }
        tls.complete_io(&mut tcp).map_err(|e| e.to_string())?;
    }
    let app = tls.peer_certificates().and_then(|certs| certs.first()).map(|c| crate::direct::fingerprint(c.as_ref())).ok_or("it showed no certificate")?;
    // Kept before it's told so: a wake from here on reaches it.
    let mut all = waiting().lock().unwrap();
    tls.writer().write_all(READY).map_err(|e| e.to_string())?;
    while tls.wants_write() {
        tls.write_tls(&mut tcp).map_err(|e| e.to_string())?;
    }
    let from = SocketAddr::new(from.ip().to_canonical(), from.port());
    all.push(Waiting { app, from, tcp, tls });
    trim(&mut all);
    Ok(())
}

/// Lets go of the oldest connections past what's kept (they're in the order
/// they came).
fn trim(all: &mut Vec<Waiting>) {
    let newest = all.last().map(|w| w.app.clone()).unwrap_or_default();
    while all.iter().filter(|w| w.app == newest).count() > PER_APP {
        let oldest = all.iter().position(|w| w.app == newest).expect("counted");
        all.remove(oldest);
    }
    while all.len() > IN_ALL {
        all.remove(0);
    }
}

/// The machine has fully woken: every app that waits hears, and its
/// connection ends. One unpaired meanwhile hears nothing.
fn tell_all() {
    let all = std::mem::take(&mut *waiting().lock().unwrap());
    if all.is_empty() {
        return;
    }
    let known = crate::direct::trusted();
    for mut w in all {
        if !known.contains(&w.app) {
            continue;
        }
        // In `listen`'s log, beside the sleep and the wake. Said, not known
        // to have arrived: an app that's gone finds the machine awake when
        // it next looks.
        let now = crate::now();
        match w.tell() {
            Ok(()) => eprintln!("{now} muxen-agent: told the app at {} it woke", w.from),
            Err(e) => eprintln!("{now} muxen-agent: couldn't tell the app at {} it woke: {e}", w.from),
        }
    }
}

impl Waiting {
    fn tell(&mut self) -> io::Result<()> {
        // What the app said as it closed its half is read first: closing
        // with that unread would reset the connection, and `awake` with it.
        self.tcp.set_nonblocking(true)?;
        loop {
            match self.tls.read_tls(&mut self.tcp) {
                Ok(0) => break,
                Ok(_) => {
                    if self.tls.process_new_packets().is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        self.tcp.set_nonblocking(false)?;
        self.tcp.set_write_timeout(Some(Duration::from_secs(2)))?;
        self.tls.writer().write_all(AWAKE)?;
        self.tls.send_close_notify();
        while self.tls.wants_write() {
            self.tls.write_tls(&mut self.tcp)?;
        }
        // The system delivers it from here, trying again by itself while
        // the network comes back after the wake.
        self.tcp.shutdown(Shutdown::Write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    /// Takes any agent: the test pins nothing.
    #[derive(Debug)]
    struct AnyAgent(rustls::crypto::WebPkiSupportedAlgorithms);

    impl ServerCertVerifier for AnyAgent {
        fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
        }

        fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.supported_schemes()
        }
    }

    /// Reads a line of what this machine says (without its end); "" once it has closed.
    fn line(tls: &mut rustls::ClientConnection, tcp: &mut TcpStream) -> io::Result<String> {
        let mut stream = rustls::Stream::new(tls, tcp);
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => out.push(byte[0]),
                Err(e) => return Err(e),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// An app with a certificate of its own: its fingerprint, and how it calls.
    fn app() -> (String, Arc<rustls::ClientConfig>) {
        let made = rcgen::generate_simple_self_signed(vec!["muxen-app".to_string()]).unwrap();
        let (cert, key) = (made.cert.der().to_vec(), made.key_pair.serialize_der());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = AnyAgent(provider.signature_verification_algorithms);
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_auth_cert(vec![CertificateDer::from(cert.clone())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
            .unwrap();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        (crate::direct::fingerprint(&cert), Arc::new(tls))
    }

    /// Calls as an app does: the handshake, `ready`, then its half closed.
    /// Err when this machine doesn't take it.
    fn call(port: u16, config: &Arc<rustls::ClientConfig>) -> io::Result<(rustls::ClientConnection, TcpStream)> {
        let mut tcp = TcpStream::connect(("127.0.0.1", port))?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut tls = rustls::ClientConnection::new(config.clone(), ServerName::try_from("muxen-agent").unwrap()).map_err(io::Error::other)?;
        let said = line(&mut tls, &mut tcp)?;
        if said != "ready" {
            return Err(io::Error::other(format!("it said {said:?}")));
        }
        tls.send_close_notify();
        while tls.wants_write() {
            tls.write_tls(&mut tcp)?;
        }
        tcp.shutdown(Shutdown::Write)?;
        Ok((tls, tcp))
    }

    fn held() -> usize {
        waiting().lock().unwrap().len()
    }

    /// An app that waits hears `awake` when the machine has fully woken, and
    /// nothing before; one that isn't paired isn't taken. What an app left
    /// behind is let go of as new ones come.
    #[test]
    fn a_waiting_app_hears_the_wake() {
        use crate::sleep::{Change, test_change};
        let _turn = crate::sleep::test_lock();
        waiting().lock().unwrap().clear();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        start(listener).unwrap();
        assert!(serving());

        let (paired, config) = app();
        let (_, stranger) = app();
        // Not paired with this machine: turned away, and nothing is kept.
        assert!(call(port, &stranger).is_err());
        assert_eq!(held(), 0);
        let trusted = crate::home_dir().join("direct/trusted");
        std::fs::write(&trusted, format!("{paired}\n")).unwrap();
        let (mut tls, mut tcp) = call(port, &config).expect("a paired app is taken");
        assert_eq!(held(), 1);

        // Going to sleep says nothing on it, and neither does being asleep.
        test_change(Change::Sleep);
        tcp.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let quiet = line(&mut tls, &mut tcp).unwrap_err();
        assert!(matches!(quiet.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{quiet}");
        assert_eq!(held(), 1);

        // Awake: said once, and the connection ends there.
        test_change(Change::Awake);
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        assert_eq!(line(&mut tls, &mut tcp).unwrap(), "awake");
        assert_eq!(line(&mut tls, &mut tcp).unwrap(), "", "closed after it");
        assert_eq!(held(), 0);

        // An app that came back again and again without a word: the oldest go.
        let calls: Vec<_> = (0..PER_APP + 2).map(|_| call(port, &config).unwrap()).collect();
        assert_eq!(held(), PER_APP);
        let (mut first_tls, mut first_tcp) = calls.into_iter().next().unwrap();
        assert_ne!(line(&mut first_tls, &mut first_tcp).ok().as_deref(), Some("awake"), "let go of, not told");

        // Unpaired while it waits: it hears nothing at the next wake.
        let (mut tls, mut tcp) = call(port, &config).unwrap();
        std::fs::write(&trusted, "").unwrap();
        test_change(Change::Sleep);
        test_change(Change::Awake);
        assert_ne!(line(&mut tls, &mut tcp).ok().as_deref(), Some("awake"));
        // The wake's listeners run on a thread of their own.
        let deadline = Instant::now() + Duration::from_secs(5);
        while held() != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(held(), 0);
        let _ = std::fs::remove_file(&trusted);
    }
}
