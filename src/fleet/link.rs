//! A live connection to a machine's agent (`muxen-agent link`), kept open
//! and shared by everything that talks to that machine: agent requests,
//! scripts, terminals, and the agent's own notices. One ssh connection per
//! machine instead of one per request (~25 ms a request instead of ~400 ms).
//! The protocol is described in agent/src/link.rs.
//!
//! Links are made on first use and replaced when they drop. When one can't be
//! made (an old agent, the machine away), callers fall back to plain ssh and
//! it's tried again a little later.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

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
}

enum Holder {
    Ssh(Child),
    Direct(quinn::Connection),
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

    /// Whether Muxen dropped it on purpose, rather than losing the machine.
    pub fn retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }

    fn shut(&self) {
        self.alive.store(false, Ordering::Relaxed);
        self.down.send_replace(true);
        self.pending.lock().unwrap().clear();
        self.channels.lock().unwrap().clear();
        match self.holder.lock().unwrap().take() {
            Some(Holder::Ssh(mut c)) => {
                let _ = c.start_kill();
            }
            Some(Holder::Direct(conn)) => conn.close(0u32.into(), b"bye"),
            None => {}
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
        Ok(link) => {
            tracing::debug!("linked to {} (agent {})", spec.name, link.version());
            slots().lock().unwrap().insert(spec.id.clone(), Slot::Up(link.clone()));
            if let Some(hook) = on_connect().lock().unwrap().as_ref() {
                hook(link.clone());
            }
            Some(link)
        }
        Err(e) => {
            tracing::debug!("no link to {}: {e}", spec.name);
            slots().lock().unwrap().insert(spec.id.clone(), Slot::Down(Instant::now()));
            None
        }
    }
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
    let link = start(spec, recv, send, Holder::Direct(conn), "direct").await?;
    let _ = link.peer.set(peer);
    Ok(link)
}

async fn connect_ssh(spec: &MachineSpec) -> Result<Arc<Link>, String> {
    let mut cmd = if spec.is_local() {
        let mut c = Command::new(local_agent());
        c.arg("link");
        c
    } else {
        let mut c = Command::new("ssh");
        c.args(probe::ssh_args(spec));
        c.arg(super::sessions::agent::command(spec, "link"));
        c
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start the link: {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let link = start(spec, stdout, stdin, Holder::Ssh(child), "ssh").await?;
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
    use super::network_of;

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

/// Runs the link protocol over a connection and waits for the agent's hello.
async fn start(
    spec: &MachineSpec,
    mut stdout: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    mut stdin: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
    holder: Holder,
    transport: &'static str,
) -> Result<Arc<Link>, String> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (events, _) = broadcast::channel(64);
    let link = Arc::new(Link {
        machine: spec.id.clone(),
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
    });

    // Frames to the agent.
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
    });

    // Frames from the agent: replies, events, terminal output.
    let (hello_tx, hello_rx) = oneshot::channel::<Value>();
    let weak = Arc::downgrade(&link);
    tokio::spawn(async move {
        let mut hello = Some(hello_tx);
        loop {
            let mut head = [0u8; 9];
            if stdout.read_exact(&mut head).await.is_err() {
                break;
            }
            let channel = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
            let kind = head[4];
            let len = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as usize;
            if len > MAX_FRAME {
                break;
            }
            let mut payload = vec![0u8; len];
            if stdout.read_exact(&mut payload).await.is_err() {
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
