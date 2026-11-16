//! Quiet connections: how this app hears that a machine woke.
//!
//! A machine that's asleep is left alone (asleep.rs), so its agent has to
//! say when it's awake again. It can't call this app: a Mac lets a program
//! start a connection on the local network only once its owner allowed it
//! there (Privacy & Security › Local Network), and answering needs no leave.
//! So this app calls ahead of time, and the call stays open with nothing
//! said on it (agent/src/quiet.rs has the other side):
//!
//! - TCP to the address and port its direct link goes to, with TLS and the
//!   certificates the two pinned when they paired (`ALPN`).
//! - The agent says `ready`, and this app closes its half right away. Its
//!   goodbye reaches the machine while it's up, so nothing that happens here
//!   later sends it anything: quitting, letting go of the connection, moving
//!   to another network. (Closing a connection that's still open both ways
//!   sends a packet, and a sleeping Mac wakes for any packet.)
//! - Then nothing from either side, and no keep-alives.
//! - When the machine has fully woken, its agent says `awake` and closes.
//!
//! One connection a machine, kept by the app that stays open (the window, an
//! iPad): `take` says what to do when a machine woke, or when its
//! connection ended without a word (nobody will say it woke, so the app
//! looks for itself again).
//!
//! What can't be helped: a call cut short before this app's half is closed
//! (no answer within `SETUP`, or its caller gone) closes a connection that's
//! still open both ways, and that sends a packet. Calls are made while the
//! machine is up, or in the seconds a look woke it for; one that went back
//! to sleep in that moment is woken once more by it.

use std::collections::HashMap;
use std::io::{self, Read};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::direct::{self, Paired};

/// The protocol's name in the TLS handshake.
pub const ALPN: &[u8] = b"muxen-wake/2";
/// How long the machine gets to take the connection and say `ready`. It may
/// be in the few seconds it's up for a look at it in its sleep.
const SETUP: Duration = Duration::from_secs(5);

type Hook = Arc<dyn Fn(&str) + Send + Sync>;

struct Hooks {
    woke: Hook,
    lost: Hook,
}

/// Who hears: the app that stays open, once (and each test that listens).
fn hooks() -> &'static Mutex<Vec<Arc<Hooks>>> {
    static HOOKS: OnceLock<Mutex<Vec<Arc<Hooks>>>> = OnceLock::new();
    HOOKS.get_or_init(Default::default)
}

/// The user's choice (Settings::wake_notices).
static ON: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// A machine's connection, waited on by a task of its own.
struct Held {
    serial: u64,
    /// This app's end of it, as the machine sees it.
    local: SocketAddr,
    /// The same as this computer has it (an IPv6 address with its scope).
    bound: SocketAddr,
    waiting: tokio::task::JoinHandle<()>,
}

fn held_ones() -> &'static Mutex<HashMap<String, Held>> {
    static HELD: OnceLock<Mutex<HashMap<String, Held>>> = OnceLock::new();
    HELD.get_or_init(Default::default)
}

/// This app hears machines wake from now on: `woke` is called with the id of
/// one whose agent says it has, `lost` with one whose connection ended
/// without that. Only an app that stays open does (the window, an iPad); a
/// command that runs and ends has nobody left to hear.
pub fn take(woke: impl Fn(&str) + Send + Sync + 'static, lost: impl Fn(&str) + Send + Sync + 'static) {
    hooks().lock().unwrap().push(Arc::new(Hooks { woke: Arc::new(woke), lost: Arc::new(lost) }));
}

/// Whether this app is one that hears them (`take` was called).
pub fn taking() -> bool {
    !hooks().lock().unwrap().is_empty()
}

/// Waits to hear machines wake from now on, or no longer does: the
/// connections it has are let go of then.
pub fn set_on(on: bool) {
    ON.store(on, Ordering::SeqCst);
    if !on {
        let all: Vec<Held> = held_ones().lock().unwrap().drain().map(|(_, h)| h).collect();
        for h in all {
            h.waiting.abort();
        }
    }
}

/// Whether to open one for a machine that offers it.
pub fn wanted() -> bool {
    taking() && ON.load(Ordering::SeqCst)
}

/// This app's end of the connection it would hear the machine's wake on
/// ("192.168.1.20:61234"), while it has one.
pub fn held(machine_id: &str) -> Option<String> {
    held_ones().lock().unwrap().get(machine_id).map(|h| h.local.to_string())
}

/// The same, when the machine's word would still get here on it: this
/// computer still has the address the connection was made from. (One that
/// went away while the machine was up, on leaving a network, leaves a
/// connection nothing arrives on.) What a machine going to sleep is said to
/// have promised.
pub fn holds(machine_id: &str) -> Option<String> {
    let (local, bound) = held_ones().lock().unwrap().get(machine_id).map(|h| (h.local, h.bound))?;
    here(bound).then(|| local.to_string())
}

/// Whether this computer has the address now.
fn here(mut addr: SocketAddr) -> bool {
    addr.set_port(0);
    UdpSocket::bind(addr).is_ok()
}

/// Lets go of a machine's connection (the machine was removed, or unpaired).
/// Nothing is sent: its agent finds out when it next has something to say.
pub fn let_go(machine_id: &str) {
    if let Some(h) = held_ones().lock().unwrap().remove(machine_id) {
        h.waiting.abort();
    }
}

/// Whether the machine's wake would still be heard on `notice` (this app's
/// end of a connection, as a check of the machine wrote it down): this app
/// still has that connection, and this computer still has that address.
pub fn arrives(notice: &str) -> bool {
    let Ok(addr) = notice.parse::<SocketAddr>() else { return false };
    let bound = held_ones().lock().unwrap().values().find(|h| h.local == addr).map(|h| h.bound);
    bound.is_some_and(here)
}

/// Opens the machine's connection, at `to` (where its direct link goes),
/// and waits on it from now on. Returns this app's end of it. One it had
/// before is let go of, also when this one can't be made: that one was made
/// another way or at another time, and whether a word still arrives on it
/// is anyone's guess. Better none, and a look at the machine at each check.
pub async fn open(machine_id: &str, paired: &Paired, to: SocketAddr) -> Result<String, String> {
    let made = tokio::time::timeout(SETUP, call(paired, to)).await.map_err(|_| format!("{to}: no answer")).and_then(|made| made);
    let (conn, bound) = match made {
        Ok(made) => made,
        Err(e) => {
            let_go(machine_id);
            return Err(e);
        }
    };
    let local = SocketAddr::new(bound.ip().to_canonical(), bound.port());
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let id = machine_id.to_string();
    // Kept before it's waited on: what the wait hears is about this one.
    let mut all = held_ones().lock().unwrap();
    let waiting = tokio::spawn(wait(id.clone(), serial, conn));
    if let Some(before) = all.insert(id, Held { serial, local, bound, waiting }) {
        before.waiting.abort();
    }
    Ok(local.to_string())
}

/// The call itself: connected, known to be the machine's agent, told
/// `ready`, and this app's half closed. With this computer's end of it.
async fn call(paired: &Paired, to: SocketAddr) -> Result<(Conn, SocketAddr), String> {
    let config = direct::client_tls(paired, ALPN)?;
    let tcp = TcpStream::connect(to).await.map_err(|e| format!("{to}: {e}"))?;
    let _ = tcp.set_nodelay(true);
    let local = tcp.local_addr().map_err(|e| e.to_string())?;
    let name = ServerName::try_from("muxen-agent").map_err(|e| e.to_string())?;
    let tls = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
    let mut conn = Conn { tcp, tls };
    match conn.line().await {
        Ok(Some(said)) if said == "ready" => {}
        Ok(Some(said)) => return Err(format!("{to}: its agent said {said:?}")),
        Ok(None) => return Err(format!("{to}: its agent hung up")),
        Err(e) => return Err(format!("{to}: {e}")),
    }
    // This app's goodbye, now: nothing is left for it to send, ever.
    conn.tls.send_close_notify();
    conn.flush().await.map_err(|e| e.to_string())?;
    conn.tcp.shutdown().await.map_err(|e| e.to_string())?;
    Ok((conn, local))
}

/// Waits for what the agent says next, which is that its machine woke.
async fn wait(machine_id: String, serial: u64, mut conn: Conn) {
    let said = conn.line().await;
    // Still the one kept for the machine (not one let go of meanwhile).
    let ours = {
        let mut all = held_ones().lock().unwrap();
        let ours = all.get(&machine_id).is_some_and(|h| h.serial == serial);
        if ours {
            all.remove(&machine_id);
        }
        ours
    };
    if !ours {
        return;
    }
    let hearing: Vec<Arc<Hooks>> = hooks().lock().unwrap().clone();
    match said {
        Ok(Some(said)) if said == "awake" => {
            tracing::debug!("{machine_id} says it woke");
            hearing.iter().for_each(|h| (h.woke)(&machine_id));
        }
        other => {
            tracing::debug!("{machine_id} won't say when it wakes any more: {other:?}");
            hearing.iter().for_each(|h| (h.lost)(&machine_id));
        }
    }
}

/// TLS over the connection, driven by hand: a line is all that's ever read.
struct Conn {
    tcp: TcpStream,
    tls: rustls::ClientConnection,
}

impl Conn {
    /// Sends what TLS has to send.
    async fn flush(&mut self) -> io::Result<()> {
        while self.tls.wants_write() {
            let mut out = Vec::new();
            self.tls.write_tls(&mut out)?;
            self.tcp.write_all(&out).await?;
        }
        Ok(())
    }

    /// The next line the agent says, without its end. None once it closed.
    async fn line(&mut self) -> io::Result<Option<String>> {
        let mut line = Vec::new();
        let mut raw = vec![0u8; 4096];
        loop {
            self.flush().await?;
            let mut byte = [0u8; 1];
            loop {
                match self.tls.reader().read(&mut byte) {
                    Ok(0) => return Ok(None),
                    Ok(_) if byte[0] == b'\n' => return Ok(Some(String::from_utf8_lossy(&line).into_owned())),
                    Ok(_) if line.len() < 64 => line.push(byte[0]),
                    Ok(_) => return Err(io::Error::other("its agent said too much")),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
            let n = self.tcp.read(&mut raw).await?;
            if n == 0 {
                return Ok(None);
            }
            let mut rest = &raw[..n];
            while !rest.is_empty() {
                self.tls.read_tls(&mut rest)?;
                self.tls.process_new_packets().map_err(io::Error::other)?;
            }
        }
    }
}

/// What this app heard, for tests: the ids of machines that woke, and of
/// those whose connection ended without a word. (One listener for all the
/// tests of a process; an engine that hears adds its own.)
#[cfg(test)]
pub(crate) fn test_heard() -> (tokio::sync::broadcast::Receiver<String>, tokio::sync::broadcast::Receiver<String>) {
    use tokio::sync::broadcast::{Sender, channel};
    static HEARD: OnceLock<(Sender<String>, Sender<String>)> = OnceLock::new();
    let (woke, lost) = HEARD.get_or_init(|| {
        let (woke, lost) = (channel(16).0, channel(16).0);
        let (w, l): (Sender<String>, Sender<String>) = (Sender::clone(&woke), Sender::clone(&lost));
        take(
            move |id| {
                let _ = w.send(id.to_string());
            },
            move |id| {
                let _ = l.send(id.to_string());
            },
        );
        set_on(true);
        (woke, lost)
    });
    (woke.subscribe(), lost.subscribe())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rustls::client::danger::HandshakeSignatureValid;
    use rustls::pki_types::{CertificateDer, UnixTime};
    use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
    use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
    use std::io::Write;
    use std::sync::mpsc;

    #[test]
    fn nothing_is_held_for_a_machine_never_called() {
        assert_eq!((held("quiet-test-nobody"), holds("quiet-test-nobody")), (None, None));
        assert!(!arrives("192.0.2.7:1") && !arrives("not an address"));
        // An address this computer has, and one it doesn't (any more): a
        // connection made from the second is one nothing arrives on.
        assert!(here("127.0.0.1:61234".parse().unwrap()) && !here("192.0.2.7:61234".parse().unwrap()));
        // Letting go of nothing is fine.
        let_go("quiet-test-nobody");
    }

    /// How a caller left: whether its half is closed for good (TLS's goodbye
    /// with nothing before it, then the connection's own end), and whose
    /// certificate it showed.
    pub(crate) type Left = (bool, Option<String>);

    /// Something that answers as an agent does, one caller at a time: it
    /// asks who calls, says `ready`, reads until the app has closed its half
    /// (and says on `closed` how it left), then says what it's told to and
    /// closes (None: closes without a word).
    pub(crate) struct FakeAgent {
        pub(crate) paired: Paired,
        pub(crate) at: SocketAddr,
        pub(crate) closed: mpsc::Receiver<Left>,
        pub(crate) say: mpsc::Sender<Option<&'static str>>,
    }

    /// Takes any app's certificate, as long as it shows one: the stand-in
    /// says whose it was, and the test checks it.
    #[derive(Debug)]
    struct AnyApp(rustls::crypto::WebPkiSupportedAlgorithms);

    impl ClientCertVerifier for AnyApp {
        fn root_hint_subjects(&self) -> &[DistinguishedName] {
            &[]
        }

        fn verify_client_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
            Ok(ClientCertVerified::assertion())
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

    pub(crate) fn fake_agent() -> FakeAgent {
        let made = rcgen::generate_simple_self_signed(vec!["muxen-agent".to_string()]).unwrap();
        let (cert, key) = (made.cert.der().to_vec(), made.key_pair.serialize_der());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let apps = AnyApp(provider.signature_verification_algorithms);
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(Arc::new(apps))
            .with_single_cert(vec![cert.clone().into()], rustls::pki_types::PrivateKeyDer::Pkcs8(key.into()))
            .unwrap();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        tls.send_tls13_tickets = 0;
        let tls = Arc::new(tls);
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let at = listener.local_addr().unwrap();
        let (closed_tx, closed) = mpsc::channel();
        let (say, told) = mpsc::channel::<Option<&'static str>>();
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(mut tcp) = tcp else { return };
                let Ok(mut conn) = rustls::ServerConnection::new(tls.clone()) else { return };
                if rustls::Stream::new(&mut conn, &mut tcp).write_all(b"ready\n").is_err() {
                    // Not an app this agent's certificate is good for.
                    continue;
                }
                let mut rest = Vec::new();
                let goodbye = rustls::Stream::new(&mut conn, &mut tcp).read_to_end(&mut rest).is_ok() && rest.is_empty();
                // And the connection's own end behind TLS's goodbye: the
                // app's half is closed, not only said to be.
                let _ = tcp.set_read_timeout(Some(Duration::from_secs(2)));
                let ended = matches!(tcp.read(&mut [0u8; 1]), Ok(0));
                let app = conn.peer_certificates().and_then(|certs| certs.first()).map(|c| direct::fingerprint(c.as_ref()));
                let _ = closed_tx.send((goodbye && ended, app));
                match told.recv() {
                    Ok(Some(text)) => {
                        let _ = rustls::Stream::new(&mut conn, &mut tcp).write_all(text.as_bytes());
                        conn.send_close_notify();
                        let _ = conn.complete_io(&mut tcp);
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
                let _ = tcp.shutdown(std::net::Shutdown::Both);
            }
        });
        let paired = Paired { fingerprint: direct::fingerprint(&cert), host: "127.0.0.1".into(), port: at.port(), lan: Vec::new(), tailnet: Vec::new() };
        FakeAgent { paired, at, closed, say }
    }

    /// The next thing heard about `id`, when something is within `within`.
    async fn next(heard: &mut tokio::sync::broadcast::Receiver<String>, id: &str, within: Duration) -> bool {
        tokio::time::timeout(within, async {
            loop {
                match heard.recv().await {
                    Ok(who) if who == id => return true,
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    /// The whole of this app's side: it calls as itself (its certificate),
    /// is told `ready`, closes its half at once, and hears `awake` however
    /// much later. A connection that ends without that is said to be lost;
    /// one let go of says nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn hears_a_machine_wake_on_a_connection_it_closed_its_half_of() {
        direct::init(std::env::temp_dir().join("muxin-quiet-test"));
        let (mut woke, mut lost) = test_heard();
        let agent = fake_agent();
        let id = "quiet-test-book";
        let long = Duration::from_secs(10);
        let short = Duration::from_millis(400);
        // Its half closed for good, and the certificate it's paired by shown.
        let left = Ok((true, Some(direct::app_fingerprint().unwrap())));

        let at = open(id, &agent.paired, agent.at).await.expect("its agent takes the call");
        assert_eq!((held(id), holds(id)), (Some(at.clone()), Some(at.clone())));
        assert!(arrives(&at) && wanted());
        // This app's goodbye is with the agent already: it has nothing left to send.
        assert_eq!(agent.closed.recv_timeout(long), left);
        assert!(!next(&mut woke, id, short).await, "nothing is heard while it sleeps");
        agent.say.send(Some("awake\n")).unwrap();
        assert!(next(&mut woke, id, long).await);
        assert_eq!(held(id), None);
        assert!(!arrives(&at));

        // Its agent stopped, or the machine is shutting down: no word.
        open(id, &agent.paired, agent.at).await.unwrap();
        assert_eq!(agent.closed.recv_timeout(long), left);
        agent.say.send(None).unwrap();
        assert!(next(&mut lost, id, long).await);
        assert_eq!(held(id), None);
        // Words that aren't `awake` count for nothing either.
        open(id, &agent.paired, agent.at).await.unwrap();
        assert_eq!(agent.closed.recv_timeout(long), left);
        agent.say.send(Some("asleep\n")).unwrap();
        assert!(next(&mut lost, id, long).await);

        // Let go of on purpose (the machine was removed): whatever its
        // agent says later is for nobody.
        open(id, &agent.paired, agent.at).await.unwrap();
        assert_eq!(agent.closed.recv_timeout(long), left);
        let_go(id);
        assert_eq!(held(id), None);
        agent.say.send(Some("awake\n")).unwrap();
        assert!(!next(&mut woke, id, short).await && !next(&mut lost, id, short).await);

        // A new call takes the place of the one before, which then counts for nothing.
        let other = fake_agent();
        let first = open(id, &agent.paired, agent.at).await.unwrap();
        assert_eq!(agent.closed.recv_timeout(long), left);
        let second = open(id, &other.paired, other.at).await.unwrap();
        assert_eq!(other.closed.recv_timeout(long), left);
        assert_ne!(first, second);
        assert_eq!(held(id), Some(second));
        agent.say.send(None).unwrap();
        assert!(!next(&mut lost, id, short).await, "the one let go of for it says nothing");
        other.say.send(Some("awake\n")).unwrap();
        assert!(next(&mut woke, id, long).await);

        // Something else at the machine's address: not its agent, no connection.
        let wrong = Paired { fingerprint: agent.paired.fingerprint.clone(), ..other.paired.clone() };
        assert!(open(id, &wrong, other.at).await.is_err());
        assert_eq!(held(id), None);
        // Nothing listens there at all. A call that fails doesn't leave the
        // one from before in its place: whether a word still comes on that
        // one, nobody knows.
        open(id, &agent.paired, agent.at).await.unwrap();
        assert_eq!(agent.closed.recv_timeout(long), left);
        let nobody = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap();
        assert!(open(id, &agent.paired, nobody).await.is_err());
        assert_eq!((held(id), holds(id)), (None, None));
        agent.say.send(None).unwrap();
        assert!(!next(&mut lost, id, short).await, "and it's let go of without a word");
    }
}
