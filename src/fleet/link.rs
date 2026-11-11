//! A live connection to a machine's agent (`muxen-agent link`), kept open
//! and shared by everything that talks to that machine: agent requests,
//! scripts, terminals, and the agent's own notices. One ssh connection per
//! machine instead of one per request (~25 ms a request instead of ~400 ms).
//! The protocol is described in agent/src/link.rs.
//!
//! Links are made on first use and replaced when they drop. When one can't be
//! made (an old agent, the machine away), callers fall back to plain ssh and
//! it's tried again a little later.
//!
//! Over a direct connection each terminal gets a stream of its own (a lane,
//! `Link::lane`), so a busy one doesn't queue in front of the one being
//! typed into, and streams take priorities (`set_priority`). A terminal let
//! go drops its stream, and with it what the agent still had queued for it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot};

use std::pin::Pin;

use crate::config::MachineSpec;
use crate::probe;

/// Channel 0: requests, replies and events, as JSON.
pub const CONTROL: u32 = 0;
pub const JSON: u8 = 10;
/// Either side: this channel is finished.
pub const CLOSE: u8 = 11;
/// Terminal frames, as the session server speaks them (used by the app's terminals).
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub const DATA: u8 = 0;
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub const RESIZE: u8 = 1;
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub const EXIT: u8 = 2;

const MAX_FRAME: usize = 64 << 20;
/// How long to wait for a new link to say hello (ssh connect + agent start).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
/// After a failed attempt, use plain ssh for this long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// The first agent version that speaks it.
const MIN_VERSION: (u32, u32) = (0, 2);

/// Who goes first when a direct link is busy (QUIC stream priorities, both
/// ways): the terminal the user is typing into, then requests (channel 0 of
/// the machine's link, with the desktop and copies on it), then terminals in
/// the background. Over ssh everything shares one stream, in order.
pub const FOCUSED: i32 = 2;
pub const CONTROL_PRIORITY: i32 = 1;
pub const BACKGROUND: i32 = 0;

pub struct Link {
    pub machine: String,
    version: OnceLock<String>,
    out: mpsc::UnboundedSender<Vec<u8>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    channels: Mutex<HashMap<u32, mpsc::UnboundedSender<(u8, Vec<u8>)>>>,
    next_id: AtomicU64,
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    next_channel: AtomicU32,
    alive: AtomicBool,
    /// Flips to true when the link goes down, for whoever waits on it.
    down: tokio::sync::watch::Sender<bool>,
    /// Dropped on purpose (`reset`), not lost.
    retired: AtomicBool,
    events: broadcast::Sender<Value>,
    /// What keeps the connection open.
    holder: Mutex<Option<Holder>>,
    /// "direct" (QUIC) or "ssh".
    transport: &'static str,
    /// The machine's address, as connected to (direct) or as ssh resolves it.
    peer: OnceLock<std::net::SocketAddr>,
    /// Its stream's priority (direct links), applied before the next write.
    priority: Arc<AtomicI32>,
    /// Where its tasks run (a lane's too).
    rt: tokio::runtime::Handle,
    /// A stream of its own for one terminal (`lane`), not the machine's link.
    lane: AtomicBool,
}

enum Holder {
    Ssh(super::child::Child),
    Direct(quinn::Connection),
    /// A lane: a stream of the machine's direct connection, which stays.
    Lane,
}

/// Where frames to the agent go.
enum Writer {
    Pipe(Pin<Box<dyn tokio::io::AsyncWrite + Send>>),
    Quic(quinn::SendStream),
}

impl Link {
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// "direct" (QUIC) or "ssh".
    pub fn transport(&self) -> &'static str {
        self.transport
    }

    /// The machine's address, when known.
    pub fn peer(&self) -> Option<std::net::SocketAddr> {
        self.peer.get().copied()
    }

    /// "Local network", "Tailscale", "Internet" or "This computer".
    pub fn network(&self) -> Option<&'static str> {
        self.peer().map(|a| network_of(a.ip()))
    }

    /// The agent's version.
    pub fn version(&self) -> &str {
        self.version.get().map(String::as_str).unwrap_or("")
    }

    /// Sends a request and waits for its reply.
    pub async fn request(&self, mut req: Value, timeout: Duration) -> Result<Value, String> {
        if !self.alive() {
            return Err("the link to the machine closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        req["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(CONTROL, JSON, &serde_json::to_vec(&req).unwrap_or_default());
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err("the link to the machine closed".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                // A slow request, or a dead connection? A quick ping tells; a
                // dead one is dropped so the next request connects afresh.
                if req["op"] != "ping" && Box::pin(self.request(json!({ "op": "ping" }), Duration::from_secs(3))).await.is_err() {
                    self.shut();
                }
                if req["op"] == "ping" {
                    self.shut();
                }
                Err(format!("no answer within {}s", timeout.as_secs()))
            }
        }
    }

    /// A new channel (for a terminal) and what arrives on it.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn open_channel(&self) -> (u32, mpsc::UnboundedReceiver<(u8, Vec<u8>)>) {
        let channel = self.next_channel.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.channels.lock().unwrap().insert(channel, tx);
        (channel, rx)
    }

    /// A link of its own on the machine's direct connection (a QUIC stream
    /// the agent serves as one more link), for a terminal: what the others
    /// send doesn't queue in front of it, and its priority can be its own.
    /// None over ssh, where there's only the one stream: use this link.
    ///
    /// Every agent that connects directly serves each stream as a link, so
    /// older ones take lanes too; they only keep their side of the streams
    /// in turn (they answer `priority` with an error).
    pub fn lane(&self) -> Option<Arc<Link>> {
        if !self.alive() {
            return None;
        }
        let conn = match self.holder.lock().unwrap().as_ref() {
            Some(Holder::Direct(c)) => c.clone(),
            _ => return None,
        };
        let (lane, out_rx) = Link::new(&self.machine, Holder::Lane, self.transport, self.rt.clone());
        lane.lane.store(true, Ordering::Relaxed);
        let _ = lane.version.set(self.version().to_string());
        if let Some(peer) = self.peer() {
            let _ = lane.peer.set(peer);
        }
        let weak = Arc::downgrade(&lane);
        self.rt.spawn(async move {
            // Opens at once, unless the agent's 100 streams are all in use.
            let opened = tokio::time::timeout(Duration::from_secs(5), conn.open_bi()).await;
            let Some(lane) = weak.upgrade() else { return };
            match opened {
                Ok(Ok((send, recv))) => {
                    lane.serve(Box::pin(recv), Writer::Quic(send), out_rx, None);
                }
                _ => lane.shut(),
            }
        });
        Some(lane)
    }

    /// A lane (see `lane`), rather than the machine's link.
    pub fn is_lane(&self) -> bool {
        self.lane.load(Ordering::Relaxed)
    }

    /// Where this link's stream goes in line (`FOCUSED`, `CONTROL_PRIORITY`,
    /// `BACKGROUND`), here and at the agent. Nothing over ssh.
    pub fn set_priority(&self, priority: i32) {
        if self.transport != "direct" || self.priority.swap(priority, Ordering::Relaxed) == priority {
            return;
        }
        // The agent's side: no reply waited for (an older agent says it doesn't know the op).
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({ "id": id, "op": "priority", "priority": priority });
        self.send(CONTROL, JSON, &serde_json::to_vec(&req).unwrap_or_default());
    }

    pub fn send(&self, channel: u32, kind: u8, payload: &[u8]) {
        let mut frame = Vec::with_capacity(9 + payload.len());
        frame.extend(channel.to_be_bytes());
        frame.push(kind);
        frame.extend((payload.len() as u32).to_be_bytes());
        frame.extend(payload);
        let _ = self.out.send(frame);
    }

    /// Lets go of a channel (detaches a terminal; the session keeps running).
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn close_channel(&self, channel: u32) {
        if self.channels.lock().unwrap().remove(&channel).is_some() {
            self.send(channel, CLOSE, &[]);
        }
    }

    /// Events the agent pushes, like `{"event": "sessions"}`.
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Waits until the link goes down.
    pub async fn closed(&self) {
        let mut rx = self.down.subscribe();
        let _ = rx.wait_for(|down| *down).await;
    }

    /// Whether Muxin dropped it on purpose, rather than losing the machine.
    pub fn retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }

    fn shut(&self) {
        self.alive.store(false, Ordering::Relaxed);
        self.down.send_replace(true);
        self.pending.lock().unwrap().clear();
        self.channels.lock().unwrap().clear();
        match self.holder.lock().unwrap().take() {
            Some(Holder::Ssh(mut c)) => c.start_kill(),
            Some(Holder::Direct(conn)) => conn.close(0u32.into(), b"bye"),
            Some(Holder::Lane) | None => {}
        }
    }
}

enum Slot {
    Up(Arc<Link>),
    Down(Instant),
}

fn slots() -> &'static Mutex<HashMap<String, Slot>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    SLOTS.get_or_init(Default::default)
}

/// One connection attempt at a time per machine.
fn gate(machine_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    GATES.get_or_init(Default::default).lock().unwrap().entry(machine_id.to_string()).or_default().clone()
}

type OnConnect = Box<dyn Fn(Arc<Link>) + Send + Sync>;

fn on_connect() -> &'static Mutex<Option<OnConnect>> {
    static HOOK: OnceLock<Mutex<Option<OnConnect>>> = OnceLock::new();
    HOOK.get_or_init(Default::default)
}

/// Called with each new link (the engine listens to its events).
pub fn set_on_connect(f: impl Fn(Arc<Link>) + Send + Sync + 'static) {
    *on_connect().lock().unwrap() = Some(Box::new(f));
}

/// The machine's live link, if one is up right now.
pub fn current(machine_id: &str) -> Option<Arc<Link>> {
    match slots().lock().unwrap().get(machine_id) {
        Some(Slot::Up(l)) if l.alive() => Some(l.clone()),
        _ => None,
    }
}

/// The machine's link, connecting if there isn't one (unless that failed a
/// moment ago). None means: use plain ssh this time.
pub async fn get(spec: &MachineSpec) -> Option<Arc<Link>> {
    if spec.disconnected {
        return None;
    }
    // An escape hatch: plain ssh for everything.
    if std::env::var_os("MUXEN_NO_LINK").is_some() {
        return None;
    }
    if let Some(l) = current(&spec.id) {
        return Some(l);
    }
    if matches!(slots().lock().unwrap().get(&spec.id), Some(Slot::Down(at)) if at.elapsed() < RETRY_AFTER) {
        return None;
    }
    let gate = gate(&spec.id);
    let _turn = gate.lock().await;
    if let Some(l) = current(&spec.id) {
        return Some(l);
    }
    match connect(spec).await {
        Ok(link) => Some(up(spec, link)),
        Err(e) => {
            tracing::debug!("no link to {}: {e}", spec.name);
            slots().lock().unwrap().insert(spec.id.clone(), Slot::Down(Instant::now()));
            None
        }
    }
}

/// The machine's live link, or a new direct one when it's paired; never a new
/// ssh link. For checks: they don't start ssh links (a check is also how Muxin
/// first reaches a machine), but a paired machine's own way in costs nothing,
/// and where ssh can't run (an iPad) it's the only way in. A failure here is
/// left to the direct connection's own wait, so `get` still tries ssh.
pub async fn current_or_direct(spec: &MachineSpec) -> Option<Arc<Link>> {
    if spec.disconnected || spec.is_local() || std::env::var_os("MUXEN_NO_LINK").is_some() {
        return None;
    }
    if let Some(l) = current(&spec.id) {
        return Some(l);
    }
    let paired = super::direct::paired(&spec.id)?;
    let gate = gate(&spec.id);
    let _turn = gate.lock().await;
    if let Some(l) = current(&spec.id) {
        return Some(l);
    }
    match connect_direct(spec, &paired).await {
        Ok(link) => Some(up(spec, link)),
        Err(e) => {
            tracing::debug!("no direct connection to {}: {e}", spec.name);
            None
        }
    }
}

/// Keeps a new link as the machine's and tells the engine.
fn up(spec: &MachineSpec, link: Arc<Link>) -> Arc<Link> {
    tracing::debug!("linked to {} (agent {})", spec.name, link.version());
    slots().lock().unwrap().insert(spec.id.clone(), Slot::Up(link.clone()));
    if let Some(hook) = on_connect().lock().unwrap().as_ref() {
        hook(link.clone());
    }
    link
}

/// Forget recent failures to connect, so the next use tries again right away
/// (the network or Tailscale changed).
pub fn retry_all() {
    slots().lock().unwrap().retain(|_, slot| matches!(slot, Slot::Up(_)));
}

/// Drops the machine's link (a new agent was installed, the machine was
/// removed); the next use connects afresh, right away.
pub fn reset(machine_id: &str) {
    if let Some(Slot::Up(l)) = slots().lock().unwrap().remove(machine_id) {
        l.retired.store(true, Ordering::Relaxed);
        l.shut();
    }
}

fn local_agent() -> PathBuf {
    let bin = dirs::home_dir().unwrap_or_default().join(".muxen").join("bin");
    bin.join(if cfg!(windows) { "muxen-agent.exe" } else { "muxen-agent" })
}

/// Directly when the machine is paired and reachable, else over ssh.
async fn connect(spec: &MachineSpec) -> Result<Arc<Link>, String> {
    if let Some(paired) = super::direct::paired(&spec.id).filter(|_| !spec.is_local()) {
        match connect_direct(spec, &paired).await {
            Ok(link) => return Ok(link),
            // Muxin on an iPad reaches a paired machine only directly. (One
            // it's adding, not paired yet, over its own ssh, below.)
            Err(e) if cfg!(target_os = "ios") => return Err(e),
            Err(e) => tracing::debug!("no direct connection to {}: {e}; using ssh", spec.name),
        }
    }
    connect_ssh(spec).await
}

async fn connect_direct(spec: &MachineSpec, paired: &super::direct::Paired) -> Result<Arc<Link>, String> {
    // Reached at an address of its own choosing (a forwarded port): that one only.
    let mut paired = paired.clone();
    if let Some(address) = spec.direct_address.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        let (host, port) = match address.rsplit_once(':').map(|(h, p)| (h, p.parse::<u16>())) {
            Some((h, Ok(p))) => (h.trim_matches(['[', ']']).to_string(), p),
            _ => (address.to_string(), paired.port),
        };
        let ips: Vec<String> = tokio::net::lookup_host((host.as_str(), port)).await.map(|a| a.map(|s| s.ip().to_string()).collect()).unwrap_or_default();
        paired = super::direct::Paired { lan: ips, tailnet: Vec::new(), host, port, ..paired };
    }
    let paired = &paired;
    let conn = super::direct::connect(&spec.id, paired).await?;
    let peer = conn.remote_address();
    let (mut send, recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    // A stream exists for the other side once something is sent on it.
    let ping = serde_json::to_vec(&json!({ "id": 0, "op": "ping" })).unwrap_or_default();
    let mut frame = CONTROL.to_be_bytes().to_vec();
    frame.push(JSON);
    frame.extend((ping.len() as u32).to_be_bytes());
    frame.extend(ping);
    send.write_all(&frame).await.map_err(|e| e.to_string())?;
    let link = start(spec, Box::pin(recv), Writer::Quic(send), Holder::Direct(conn), "direct").await?;
    let _ = link.peer.set(peer);
    link.set_priority(CONTROL_PRIORITY);
    Ok(link)
}

async fn connect_ssh(spec: &MachineSpec) -> Result<Arc<Link>, String> {
    let mut child = if cfg!(target_os = "ios") && !spec.is_local() {
        // Muxin's own ssh client: what the agent says on stderr is read and dropped.
        let mut c = super::child::ssh(spec, &super::sessions::agent::command(spec, "link")).await?;
        if let Some(mut err) = c.take_stderr() {
            tokio::spawn(async move { tokio::io::copy(&mut err, &mut tokio::io::sink()).await });
        }
        c
    } else {
        let mut cmd = if spec.is_local() {
            let mut c = Command::new(local_agent());
            c.arg("link");
            // Without the terminal Muxin may have been started from: nothing the
            // agent runs (an interactive shell, say) can take it over and stop us.
            #[cfg(unix)]
            unsafe {
                c.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            c
        } else {
            let mut c = Command::new("ssh");
            c.args(probe::ssh_args(spec));
            c.arg(super::sessions::agent::command(spec, "link"));
            c
        };
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        super::child::Child::Process(cmd.spawn().map_err(|e| format!("couldn't start the link: {e}"))?)
    };
    let stdin = child.take_stdin().ok_or("no stdin")?;
    let stdout = child.take_stdout().ok_or("no stdout")?;
    let link = start(spec, stdout, Writer::Pipe(stdin), Holder::Ssh(child), "ssh").await?;
    // Where ssh goes, to say which network the machine is on.
    if spec.is_local() {
        let _ = link.peer.set(std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
    } else if let Some(host) = super::direct::resolve_host(spec).await {
        if let Some(addr) = tokio::net::lookup_host((host.as_str(), 22)).await.ok().and_then(|mut a| a.next()) {
            let _ = link.peer.set(addr);
        }
    }
    Ok(link)
}

/// Which kind of network an address is on.
pub fn network_of(ip: std::net::IpAddr) -> &'static str {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                "This computer"
            } else if o[0] == 100 && (64..128).contains(&o[1]) {
                // 100.64.0.0/10: Tailscale (and other carrier-grade NAT).
                "Tailscale"
            } else if v4.is_private() || v4.is_link_local() {
                "Local network"
            } else {
                "Internet"
            }
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if v6.is_loopback() {
                "This computer"
            } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                "Tailscale"
            } else if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 {
                "Local network"
            } else if let Some(v4) = v6.to_ipv4_mapped() {
                network_of(IpAddr::V4(v4))
            } else {
                "Internet"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_check_opens_no_ssh_link() {
        // Not paired: no link, and no failure that would keep `get` from trying ssh.
        for host in ["unpaired.invalid", "local"] {
            let mut spec: MachineSpec = toml::from_str(&format!("name = \"Unpaired\"\nhost = \"{host}\"")).unwrap();
            spec.id = format!("unpaired-{}", std::process::id());
            assert!(current_or_direct(&spec).await.is_none(), "{host}");
            assert!(!slots().lock().unwrap().contains_key(&spec.id), "{host}");
        }
    }

    /// Over ssh there's one stream: no lanes (terminals use channels of the
    /// machine's link, as before), and no `priority` op for the agent.
    #[tokio::test]
    async fn an_ssh_link_has_no_lanes() {
        let (app, agent) = tokio::io::duplex(1 << 16);
        let (from_agent, to_agent) = tokio::io::split(app);
        let (mut agent_in, mut agent_out) = tokio::io::split(agent);
        let frame = |channel: u32, payload: &[u8]| {
            let mut f = channel.to_be_bytes().to_vec();
            f.push(JSON);
            f.extend((payload.len() as u32).to_be_bytes());
            f.extend(payload);
            f
        };
        agent_out.write_all(&frame(CONTROL, br#"{"event":"hello","version":"0.13.0"}"#)).await.unwrap();
        let spec: MachineSpec = toml::from_str("name = \"Piped\"\nhost = \"piped.invalid\"").unwrap();
        let link = start(&spec, Box::pin(from_agent), Writer::Pipe(Box::pin(to_agent)), Holder::Lane, "ssh").await.unwrap();
        assert!(link.lane().is_none());
        link.set_priority(FOCUSED);
        link.send(CONTROL, JSON, br#"{"id":9,"op":"ping"}"#);
        // The first thing the agent gets is that ping.
        let mut head = [0u8; 9];
        agent_in.read_exact(&mut head).await.unwrap();
        let mut payload = vec![0u8; u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as usize];
        agent_in.read_exact(&mut payload).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&payload).unwrap()["op"], "ping");
    }

    #[test]
    fn names_networks() {
        for (ip, want) in [
            ("192.168.1.50", "Local network"),
            ("10.0.0.3", "Local network"),
            ("100.101.102.103", "Tailscale"),
            ("fd7a:115c:a1e0::1", "Tailscale"),
            ("fe80::1", "Local network"),
            ("127.0.0.1", "This computer"),
            ("8.8.8.8", "Internet"),
            ("::ffff:192.168.0.2", "Local network"),
        ] {
            assert_eq!(network_of(ip.parse().unwrap()), want, "{ip}");
        }
    }
}

impl Link {
    fn new(machine: &str, holder: Holder, transport: &'static str, rt: tokio::runtime::Handle) -> (Arc<Link>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (events, _) = broadcast::channel(64);
        let link = Arc::new(Link {
            machine: machine.to_string(),
            version: OnceLock::new(),
            out: out_tx,
            pending: Mutex::new(HashMap::new()),
            channels: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_channel: AtomicU32::new(1),
            alive: AtomicBool::new(true),
            down: tokio::sync::watch::channel(false).0,
            retired: AtomicBool::new(false),
            events,
            holder: Mutex::new(Some(holder)),
            transport,
            peer: OnceLock::new(),
            priority: Arc::new(AtomicI32::new(BACKGROUND)),
            rt,
            lane: AtomicBool::new(false),
        });
        (link, out_rx)
    }

    /// Runs the link protocol over a connection: frames out from `out_rx`,
    /// frames in to their channels (the agent's hello to `hello`).
    fn serve(self: &Arc<Self>, mut input: Pin<Box<dyn tokio::io::AsyncRead + Send>>, writer: Writer, mut out_rx: mpsc::UnboundedReceiver<Vec<u8>>, hello: Option<oneshot::Sender<Value>>) {
        // Frames to the agent; the stream ends when the link is let go.
        let priority = self.priority.clone();
        self.rt.spawn(async move {
            let mut writer = writer;
            let mut applied = BACKGROUND;
            while let Some(frame) = out_rx.recv().await {
                let ok = match &mut writer {
                    Writer::Pipe(w) => w.write_all(&frame).await.is_ok() && w.flush().await.is_ok(),
                    Writer::Quic(s) => {
                        let p = priority.load(Ordering::Relaxed);
                        if p != applied {
                            let _ = s.set_priority(p);
                            applied = p;
                        }
                        s.write_all(&frame).await.is_ok()
                    }
                };
                if !ok {
                    break;
                }
            }
            if let Writer::Quic(mut s) = writer {
                let _ = s.finish();
            }
        });

        // Frames from the agent: replies, events, terminal output.
        let weak = Arc::downgrade(self);
        self.rt.spawn(async move {
            let mut hello = hello;
            loop {
                let mut head = [0u8; 9];
                if input.read_exact(&mut head).await.is_err() {
                    break;
                }
                let channel = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
                let kind = head[4];
                let len = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as usize;
                if len > MAX_FRAME {
                    break;
                }
                let mut payload = vec![0u8; len];
                if input.read_exact(&mut payload).await.is_err() {
                    break;
                }
                let Some(link) = weak.upgrade() else { break };
                if channel == CONTROL {
                    let Ok(msg) = serde_json::from_slice::<Value>(&payload) else { continue };
                    if let Some(id) = msg["id"].as_u64() {
                        if let Some(tx) = link.pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(msg);
                        }
                    } else if msg["event"] == "hello" {
                        // A lane's hello isn't waited for.
                        if let Some(tx) = hello.take() {
                            let _ = tx.send(msg);
                        }
                    } else {
                        let _ = link.events.send(msg);
                    }
                } else if kind == CLOSE {
                    link.channels.lock().unwrap().remove(&channel);
                } else if let Some(tx) = link.channels.lock().unwrap().get(&channel) {
                    let _ = tx.send((kind, payload));
                }
            }
            if let Some(link) = weak.upgrade() {
                tracing::debug!("link to {} closed", link.machine);
                link.shut();
            }
        });
    }
}

/// Runs the link protocol over a connection and waits for the agent's hello.
async fn start(spec: &MachineSpec, stdout: Pin<Box<dyn tokio::io::AsyncRead + Send>>, stdin: Writer, holder: Holder, transport: &'static str) -> Result<Arc<Link>, String> {
    let (link, out_rx) = Link::new(&spec.id, holder, transport, tokio::runtime::Handle::current());
    let (hello_tx, hello_rx) = oneshot::channel::<Value>();
    link.serve(stdout, stdin, out_rx, Some(hello_tx));

    let hello = match tokio::time::timeout(CONNECT_TIMEOUT, hello_rx).await {
        Ok(Ok(h)) => h,
        Ok(Err(_)) => {
            link.shut();
            return Err("the agent there doesn't speak the link yet".into());
        }
        Err(_) => {
            link.shut();
            return Err("the link didn't answer".into());
        }
    };
    let version = hello["version"].as_str().unwrap_or("").to_string();
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    if (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) < MIN_VERSION {
        link.shut();
        return Err(format!("agent {version} is too old for the link"));
    }
    let _ = link.version.set(version);
    Ok(link)
}
