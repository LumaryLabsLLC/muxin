//! Hosting shares: each one waits for guests at the share server, checks
//! them, asks the user to let them in, then relays what's shared: one
//! session, or a machine (its sessions, its desktop, servers the user
//! picked).
//!
//! The gateway is the security boundary. A guest never speaks the machine's
//! link protocol. It may ask for what the share holds (`sessions`, `watch`,
//! `desktop`, `tcp`, `close`) on channels of its own, and what it sends on
//! them gets through only as the user allows: keys to a session when the
//! user lets it type, mouse and keyboard to the desktop when the user lets
//! it control, bytes to a server the user picked. Its size is never applied
//! (sessions keep the user's), and nothing else (a shell, files, other
//! ports) exists here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::{PeerConnection, RTCCertificate, RTCPeerConnectionState, RTCSessionDescription};

use super::rtc::{self, Event};
use super::signal;
use super::ticket::{Ticket, hex};
use crate::fleet::link::Link;

/// Guests at once, per share (waiting or in).
const MAX_GUESTS: usize = 16;
/// How long a guest has to say hello once connected.
const HELLO_TIMEOUT: Duration = Duration::from_secs(20);
/// Wrong proofs before a share stops answering (someone guessing).
const MAX_BAD_PROOFS: u32 = 20;
/// How often the sharing app checks in at the share server for guests.
const CHECK_IN: Duration = Duration::from_secs(2);
/// The longest a share can last.
pub const MAX_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Channels one guest can have open at once (sessions, the desktop, servers).
const MAX_CHANNELS: usize = 32;
/// How often a machine share's guests hear about its sessions.
const SESSIONS_EVERY: Duration = Duration::from_secs(3);

/// Frame kinds of the desktop (agent/src/desktop.rs).
pub const FRAME: u8 = 20;
pub const INPUT: u8 = 21;
pub const TILES: u8 = 23;

/// What's shared: one session, or (`session_id` empty) a machine.
#[derive(Clone, Debug, Serialize)]
pub struct Target {
    /// `<machine>/<session>`; empty for a machine.
    pub session_id: String,
    pub machine_id: String,
    /// The session's name on the machine.
    pub name: String,
    /// The session's title, or the machine's name.
    pub title: String,
    /// "claude", "codex", "shell"...; empty for a machine.
    pub agent: String,
    pub machine_name: String,
    /// The machine's platform ("darwin-arm64"), for its desktop's keys.
    pub platform: String,
    /// Who shares it, as guests see it.
    pub by: String,
}

impl Target {
    pub fn is_machine(&self) -> bool {
        self.session_id.is_empty()
    }
}

/// What guests can do with a machine's desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Desktop {
    #[default]
    None,
    View,
    Control,
}

/// A server on the machine guests can open.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Port {
    pub port: u16,
    /// What it is ("Checkout dev"), as the user knows it.
    pub name: String,
    /// Where the machine's agent connects for it (127.0.0.1 when empty).
    #[serde(skip)]
    pub host: String,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Ask before letting each guest in.
    pub approve: bool,
    /// Guests may type in sessions.
    pub can_type: bool,
    /// Guests may start, end and restart a machine's sessions (only people
    /// let in, who may type): anything runs there, as the sharer.
    pub manage: bool,
    /// A machine's desktop.
    pub desktop: Desktop,
    /// A machine's servers.
    pub ports: Vec<Port>,
    pub ttl: Duration,
}

/// Where the shared machine is reached.
#[async_trait::async_trait]
pub trait Source: Send + Sync + 'static {
    async fn link(&self) -> Result<Arc<Link>, String>;

    /// Starts a session there, with `agent` ("shell", "claude", ...) in its
    /// home folder: its name.
    async fn start(&self, agent: &str) -> Result<String, String>;

    /// Ends a session there (`restart`: starts its program again).
    async fn stop(&self, name: &str, restart: bool) -> Result<(), String>;

    /// The agent CLIs there ("claude", "codex", ...).
    fn agents(&self) -> Vec<String>;
}

/// The machine through Muxin's engine: its agent up to date, then its link.
pub struct EngineSource {
    pub engine: Arc<crate::fleet::Engine>,
    pub machine_id: String,
}

#[async_trait::async_trait]
impl Source for EngineSource {
    async fn link(&self) -> Result<Arc<Link>, String> {
        let (spec, probe) = self.engine.machine(&self.machine_id).ok_or("That machine is gone.")?;
        crate::fleet::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        crate::fleet::link::get(&spec).await.ok_or_else(|| format!("Can't reach {} right now.", spec.name))
    }

    async fn start(&self, agent: &str) -> Result<String, String> {
        let req = crate::fleet::sessions::StartSession { machine_id: self.machine_id.clone(), agent: agent.into(), cwd: "~".into(), cols: 120, rows: 36, ..Default::default() };
        let info = self.engine.start_session(req).await?;
        Ok(info.id.split_once('/').map(|(_, n)| n.to_string()).unwrap_or(info.id))
    }

    async fn stop(&self, name: &str, restart: bool) -> Result<(), String> {
        use crate::fleet::sessions::Control;
        let id = format!("{}/{name}", self.machine_id);
        self.engine.control_session(&id, if restart { Control::Restart } else { Control::Stop }).await.map(|_| ())
    }

    fn agents(&self) -> Vec<String> {
        self.engine.current().machines.iter().find(|m| m.id == self.machine_id).map(|m| m.agents.clone()).unwrap_or_default()
    }
}

/// A guest, as the UI shows it.
#[derive(Clone, Debug, Serialize)]
pub struct GuestView {
    pub id: u64,
    pub name: String,
    pub code: String,
    /// "app" or "browser".
    pub client: String,
    /// "waiting" (for the user to let them in) or "in".
    pub state: String,
    pub can_type: bool,
    /// Uses the desktop's mouse and keyboard.
    pub can_control: bool,
    /// Starts and ends sessions.
    pub can_manage: bool,
    /// "direct" or "relay", once known.
    pub path: String,
}

/// A share, as the UI shows it.
#[derive(Clone, Debug, Serialize)]
pub struct ShareView {
    pub id: String,
    pub link: String,
    pub target: Target,
    pub approve: bool,
    pub can_type: bool,
    pub manage: bool,
    pub desktop: Desktop,
    pub ports: Vec<Port>,
    /// Unix seconds.
    pub ends_at: i64,
    pub guests: Vec<GuestView>,
    /// Why the machine couldn't be reached for a guest, until it can.
    pub problem: Option<String>,
}

struct Guest {
    view: Mutex<GuestView>,
    /// The user's answer, while the guest waits.
    decide: Mutex<Option<oneshot::Sender<bool>>>,
    control: watch::Sender<Control>,
}

#[derive(Clone, Copy, PartialEq)]
struct Control {
    can_type: bool,
    can_control: bool,
    can_manage: bool,
    /// Why the guest is being let go.
    end: Option<&'static str>,
}

struct Share {
    ticket: Ticket,
    /// The share server, and this share's key there.
    server: signal::Server,
    key: String,
    /// Guests being connected (offers answered, not in yet).
    connecting: std::sync::atomic::AtomicUsize,
    cert: RTCCertificate,
    target: Target,
    source: Arc<dyn Source>,
    options: Mutex<Options>,
    /// Bumps when the desktop or servers shared change.
    access: watch::Sender<u64>,
    ends_at: Mutex<SystemTime>,
    guests: Mutex<HashMap<u64, Arc<Guest>>>,
    stop: watch::Sender<Option<&'static str>>,
    bad_proofs: Mutex<u32>,
    /// Why the machine couldn't be reached for a guest (the sharer sees it;
    /// guests hear only that it can't be reached).
    problem: Mutex<Option<String>>,
}

impl Share {
    fn options(&self) -> Options {
        self.options.lock().unwrap().clone()
    }

    fn view(&self) -> ShareView {
        let o = self.options();
        let mut guests: Vec<GuestView> = self.guests.lock().unwrap().values().map(|g| g.view.lock().unwrap().clone()).collect();
        guests.sort_by_key(|g| g.id);
        ShareView {
            id: self.ticket.room(),
            link: self.ticket.link(),
            target: self.target.clone(),
            approve: o.approve,
            can_type: o.can_type,
            manage: o.manage,
            desktop: o.desktop,
            ports: o.ports,
            ends_at: unix(*self.ends_at.lock().unwrap()),
            guests,
            problem: self.problem.lock().unwrap().clone(),
        }
    }

    /// The machine, for a guest. When it can't be reached, why goes to the
    /// sharer, and the guest hears only that (not this computer's paths or
    /// fixes that are the sharer's to make).
    async fn machine(&self) -> Result<Arc<Link>, String> {
        let result = self.source.link().await;
        let problem = result.as_ref().err().cloned();
        let news = {
            let mut p = self.problem.lock().unwrap();
            let news = *p != problem;
            *p = problem;
            news
        };
        if news {
            changed();
        }
        result.map_err(|e| {
            tracing::warn!("share: can't reach {}: {e}", self.target.machine_name);
            format!("{} can't be reached from {}'s computer right now.", self.target.machine_name, self.target.by)
        })
    }

    fn ends_in(&self) -> Duration {
        self.ends_at.lock().unwrap().duration_since(SystemTime::now()).unwrap_or(Duration::ZERO)
    }

    /// What's shared, as the guest hears it (`control`: this guest's own).
    fn info(&self, control: &Control) -> Value {
        let o = self.options();
        json!({
            "event": "share",
            "kind": if self.target.is_machine() { "machine" } else { "session" },
            "title": self.target.title,
            "agent": self.target.agent,
            "machine": self.target.machine_name,
            "by": self.target.by,
            "can_type": control.can_type,
            "desktop": o.desktop,
            "can_control": control.can_control,
            "can_manage": control.can_manage,
            // What can be started there (with `can_manage`).
            "agents": if self.target.is_machine() { self.source.agents() } else { Vec::new() },
            // A Mac's ⌘ stays ⌘ on its desktop.
            "mac": self.target.platform.starts_with("darwin"),
            "ports": o.ports,
            "ends_at": unix(*self.ends_at.lock().unwrap()),
        })
    }
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Default)]
struct Registry {
    shares: Mutex<HashMap<String, Arc<Share>>>,
    changed: OnceLock<watch::Sender<u64>>,
    next_guest: AtomicU64,
}

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(Registry::default)
}

fn changed() {
    let tx = registry().changed.get_or_init(|| watch::channel(0).0);
    tx.send_modify(|n| *n += 1);
}

/// Bumps whenever a share or a guest changes.
pub fn subscribe() -> watch::Receiver<u64> {
    registry().changed.get_or_init(|| watch::channel(0).0).subscribe()
}

/// Every share running.
pub fn list() -> Vec<ShareView> {
    let mut all: Vec<ShareView> = registry().shares.lock().unwrap().values().map(|s| s.view()).collect();
    all.sort_by(|a, b| a.target.machine_id.cmp(&b.target.machine_id).then(a.target.session_id.cmp(&b.target.session_id)).then(a.id.cmp(&b.id)));
    all
}

fn find(id: &str) -> Option<Arc<Share>> {
    registry().shares.lock().unwrap().get(id).cloned()
}

fn guest(share: &str, guest: u64) -> Option<Arc<Guest>> {
    find(share)?.guests.lock().unwrap().get(&guest).cloned()
}

/// Starts sharing a session or a machine; the view carries the link to give
/// out. Runs on the current Tokio runtime until stopped, expired, or the
/// app quits.
pub async fn start(server: &str, target: Target, options: Options, source: Arc<dyn Source>) -> Result<ShareView, String> {
    let (cert, fingerprint) = rtc::certificate()?;
    let ticket = Ticket::new(server, fingerprint)?;
    let server = signal::Server::new(&ticket.server);
    let key = signal::host_key();
    // The first check-in: the server answers, and keeps the room for us.
    server.check_in(&ticket.room(), &key).await.map_err(|e| e.to_string())?;
    let ttl = options.ttl.min(MAX_TTL);
    // Only a machine has a desktop, servers and sessions to start; starting
    // them is for people let in, who may type.
    let manage = options.manage && options.approve && options.can_type && target.is_machine();
    let options = if target.is_machine() { Options { ttl, manage, ..options } } else { Options { ttl, manage, desktop: Desktop::None, ports: Vec::new(), ..options } };
    let share = Arc::new(Share {
        ticket,
        server,
        key,
        connecting: std::sync::atomic::AtomicUsize::new(0),
        cert,
        target,
        source,
        options: Mutex::new(options),
        access: watch::channel(0).0,
        ends_at: Mutex::new(SystemTime::now() + ttl),
        guests: Mutex::new(HashMap::new()),
        stop: watch::channel(None).0,
        bad_proofs: Mutex::new(0),
        problem: Mutex::new(None),
    });
    registry().shares.lock().unwrap().insert(share.ticket.room(), share.clone());
    changed();
    tokio::spawn(serve(share.clone()));
    Ok(share.view())
}

/// Stops a share: its guests are let go and its link stops working.
pub fn stop(id: &str) {
    if let Some(s) = find(id) {
        s.stop.send_replace(Some("stopped"));
    }
}

/// Stops every share (the app is quitting).
pub fn stop_all() {
    for s in registry().shares.lock().unwrap().values() {
        s.stop.send_replace(Some("stopped"));
    }
}

/// Lets a waiting guest in, or not.
pub fn admit(share: &str, guest_id: u64, yes: bool) {
    if let Some(g) = guest(share, guest_id) {
        if let Some(tx) = g.decide.lock().unwrap().take() {
            let _ = tx.send(yes);
        }
    }
}

/// Lets a guest go.
pub fn remove(share: &str, guest_id: u64) {
    if let Some(g) = guest(share, guest_id) {
        if let Some(tx) = g.decide.lock().unwrap().take() {
            let _ = tx.send(false);
        }
        g.control.send_modify(|c| c.end = Some("removed"));
    }
}

/// Lets a guest type, or not (right away).
pub fn set_typing(share: &str, guest_id: u64, allowed: bool) {
    if let Some(g) = guest(share, guest_id) {
        // No typing, no starting sessions either.
        g.control.send_modify(|c| {
            c.can_type = allowed;
            c.can_manage &= allowed;
        });
        let mut v = g.view.lock().unwrap();
        v.can_type = allowed;
        v.can_manage &= allowed;
        drop(v);
        changed();
    }
}

/// Lets a guest start, end and restart sessions, or not (one who may type,
/// on a machine share).
pub fn set_managing(share: &str, guest_id: u64, allowed: bool) {
    let Some(s) = find(share) else { return };
    if let Some(g) = guest(share, guest_id) {
        let allowed = allowed && s.target.is_machine() && g.view.lock().unwrap().can_type;
        g.control.send_modify(|c| c.can_manage = allowed);
        g.view.lock().unwrap().can_manage = allowed;
        changed();
    }
}

/// Whether guests let in can start, end and restart sessions (those who may
/// type), for a machine share already going.
pub fn set_manage(id: &str, manage: bool) {
    let Some(s) = find(id) else { return };
    let manage = {
        let mut o = s.options.lock().unwrap();
        o.manage = manage && o.approve && o.can_type && s.target.is_machine();
        o.manage
    };
    for g in s.guests.lock().unwrap().values() {
        let mut v = g.view.lock().unwrap();
        if v.state == "in" {
            let allowed = manage && v.can_type;
            v.can_manage = allowed;
            g.control.send_modify(|c| c.can_manage = allowed);
        }
    }
    changed();
}

/// Lets a guest use the desktop's mouse and keyboard, or not (only while
/// the share lets guests control it at all).
pub fn set_control(share: &str, guest_id: u64, allowed: bool) {
    let Some(s) = find(share) else { return };
    let allowed = allowed && s.options().desktop == Desktop::Control;
    if let Some(g) = guest(share, guest_id) {
        g.control.send_modify(|c| c.can_control = allowed);
        g.view.lock().unwrap().can_control = allowed;
        changed();
    }
}

/// Who can join and whether guests can type, for a share already going:
/// new guests follow it, and guests in get typing on or off with it. Only
/// guests the user lets in can type; letting anyone in lets in whoever waits.
pub fn set_options(id: &str, approve: bool, can_type: bool) {
    let Some(s) = find(id) else { return };
    let can_type = can_type && approve;
    let manage = {
        let mut o = s.options.lock().unwrap();
        o.approve = approve;
        o.can_type = can_type;
        o.manage &= can_type;
        o.manage
    };
    for g in s.guests.lock().unwrap().values() {
        let waiting = g.view.lock().unwrap().state == "waiting";
        if waiting {
            if !approve && let Some(tx) = g.decide.lock().unwrap().take() {
                let _ = tx.send(true);
            }
            continue;
        }
        g.control.send_modify(|c| {
            c.can_type = can_type;
            c.can_manage = manage;
        });
        let mut v = g.view.lock().unwrap();
        v.can_type = can_type;
        v.can_manage = manage;
    }
    changed();
}

/// What a machine share's guests can do with its desktop, and which of its
/// servers they can open: guests in follow it right away. Using the desktop
/// is then given to each person (set_control).
pub fn set_access(id: &str, desktop: Desktop, ports: Vec<Port>) {
    let Some(s) = find(id) else { return };
    if !s.target.is_machine() {
        return;
    }
    {
        let mut o = s.options.lock().unwrap();
        o.desktop = desktop;
        o.ports = ports;
    }
    // Using the desktop is given person by person (set_control); a desktop
    // no longer under control takes it back from everyone.
    if desktop != Desktop::Control {
        for g in s.guests.lock().unwrap().values() {
            g.control.send_modify(|c| c.can_control = false);
            g.view.lock().unwrap().can_control = false;
        }
    }
    s.access.send_modify(|n| *n += 1);
    changed();
}

/// Ends a share this long from now (a week at most).
pub fn set_ends_in(id: &str, ttl: Duration) {
    if let Some(s) = find(id) {
        *s.ends_at.lock().unwrap() = SystemTime::now() + ttl.min(MAX_TTL);
        changed();
    }
}

/// Makes a share last longer.
pub fn extend(id: &str, by: Duration) {
    if let Some(s) = find(id) {
        let mut ends = s.ends_at.lock().unwrap();
        *ends = (*ends + by).min(SystemTime::now() + MAX_TTL);
        drop(ends);
        changed();
    }
}

/// Checks in at the share server for guests until the share ends.
async fn serve(share: Arc<Share>) {
    let mut stop = share.stop.subscribe();
    let room = share.ticket.room();
    let mut wait = CHECK_IN;
    let why = loop {
        tokio::select! {
            _ = stop.changed() => break stop.borrow().unwrap_or("stopped"),
            _ = tokio::time::sleep(wait.min(share.ends_in())) => {}
        }
        if share.ends_in() == Duration::ZERO {
            break "expired";
        }
        match share.server.check_in(&room, &share.key).await {
            Ok((ice, offers)) => {
                wait = CHECK_IN;
                for (id, sdp) in offers {
                    let busy = share.guests.lock().unwrap().len() + share.connecting.load(Ordering::Relaxed);
                    if busy >= MAX_GUESTS || *share.bad_proofs.lock().unwrap() >= MAX_BAD_PROOFS {
                        continue;
                    }
                    tokio::spawn(connect_guest(share.clone(), id, sdp, ice.clone()));
                }
            }
            // Away from the server (offline, asleep): guests already in stay,
            // new ones get in once it answers again.
            Err(e) => {
                tracing::debug!("share server: {e}");
                wait = (wait * 2).min(Duration::from_secs(30));
            }
        }
    };
    share.stop.send_replace(Some(why));
    for g in share.guests.lock().unwrap().values() {
        if let Some(tx) = g.decide.lock().unwrap().take() {
            let _ = tx.send(false);
        }
        g.control.send_modify(|c| c.end = Some(why));
    }
    registry().shares.lock().unwrap().remove(&room);
    changed();
    share.server.close(&room, &share.key).await;
}

/// Answers a guest's offer, then runs its connection.
async fn connect_guest(share: Arc<Share>, id: String, offer: String, ice: Value) {
    let Some(guest_fp) = rtc::fingerprint(&offer) else { return };
    share.connecting.fetch_add(1, Ordering::Relaxed);
    let connected = async {
        let (pc, mut events) = rtc::peer(&ice, Some(share.cert.clone())).await.ok()?;
        let answered = async {
            pc.set_remote_description(RTCSessionDescription::offer(offer).map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
            let answer = pc.create_answer(None).await.map_err(|e| e.to_string())?;
            pc.set_local_description(answer).await.map_err(|e| e.to_string())?;
            rtc::gathered(&pc, &mut events, &ice).await;
            let sdp = pc.local_description().await.ok_or("no answer")?.sdp;
            share.server.answer(&share.ticket.room(), &share.key, &id, &sdp).await.map_err(|e| e.to_string())
        }
        .await;
        let dc = match answered {
            Ok(()) => {
                let channel = async {
                    loop {
                        match events.recv().await? {
                            Event::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) => return None,
                            Event::Channel(dc) => return Some(dc),
                            _ => {}
                        }
                    }
                };
                tokio::time::timeout(Duration::from_secs(30), channel).await.ok().flatten()
            }
            Err(_) => None,
        };
        match dc {
            Some(dc) => Some((pc, dc, events)),
            None => {
                let _ = pc.close().await;
                None
            }
        }
    };
    let connected = connected.await;
    share.connecting.fetch_sub(1, Ordering::Relaxed);
    let Some((pc, dc, events)) = connected else { return };
    let host_fp = hex(&share.ticket.fingerprint);
    let dc_of_guest = dc.clone();
    run_guest(&share, &pc, dc, &host_fp, &guest_fp, events).await;
    rtc::forget(&dc_of_guest);
    let _ = pc.close().await;
}

/// A guest's connection, from its hello until it leaves or is let go.
async fn run_guest(share: &Arc<Share>, pc: &Arc<dyn PeerConnection>, dc: Arc<dyn DataChannel>, host_fp: &str, guest_fp: &str, mut events: mpsc::UnboundedReceiver<Event>) {
    // What the guest sends, read in one place.
    let (msg_tx, mut from_guest) = mpsc::unbounded_channel::<Vec<u8>>();
    let reader = dc.clone();
    tokio::spawn(async move {
        let mut pieces = rtc::Pieces::default();
        while let Some(m) = rtc::next_message(&reader).await {
            let Some(frame) = pieces.add(m) else { continue };
            if msg_tx.send(frame).is_err() {
                break;
            }
        }
    });

    // Hello, with the proof that it has the link.
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, from_guest.recv()).await {
        Ok(Some(m)) => m,
        _ => return,
    };
    let hello = match super::parse_frame(&hello) {
        Some((0, super::JSON, body)) => serde_json::from_slice::<Value>(body).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    if hello["op"] != "hello" || !share.ticket.check_proof(host_fp, guest_fp, hello["proof"].as_str().unwrap_or("")) {
        *share.bad_proofs.lock().unwrap() += 1;
        let _ = rtc::send(&dc, rtc::error_frame("This link doesn't work anymore.")).await;
        rtc::flush(&dc).await;
        return;
    }
    let options = share.options();
    let id = registry().next_guest.fetch_add(1, Ordering::Relaxed) + 1;
    let client = if hello["client"] == "browser" { "browser" } else { "app" };
    // Only someone the user let in types, or controls the desktop.
    // The desktop's mouse and keyboard: only for someone the user picks (set_control).
    let can_control = false;
    let can_manage = options.manage;
    let view = GuestView {
        id,
        name: clean_name(hello["name"].as_str().unwrap_or("")),
        code: share.ticket.code(guest_fp),
        client: client.into(),
        state: if options.approve { "waiting" } else { "in" }.into(),
        can_type: options.can_type,
        can_control,
        can_manage,
        path: String::new(),
    };
    let (decide_tx, decide_rx) = oneshot::channel();
    let guest = Arc::new(Guest {
        view: Mutex::new(view),
        decide: Mutex::new(options.approve.then_some(decide_tx)),
        control: watch::channel(Control { can_type: options.can_type, can_control, can_manage, end: None }).0,
    });
    share.guests.lock().unwrap().insert(id, guest.clone());
    changed();
    let path = pc.clone();
    let g = guest.clone();
    tokio::spawn(async move {
        if let Some(p) = connection_path(&path).await {
            g.view.lock().unwrap().path = p;
            changed();
        }
    });

    let result = admitted(share, &guest, &dc, decide_rx, options.approve, &mut from_guest, &mut events).await;
    if let Err(why) = result {
        let _ = rtc::send(&dc, rtc::json_frame(&json!({ "event": "ended", "why": why }))).await;
        rtc::flush(&dc).await;
    }
    share.guests.lock().unwrap().remove(&id);
    changed();
}

/// Lets the guest in (asking first when the share says so), then serves
/// what it asks for until something ends it: Err(why) to tell the guest.
async fn admitted(
    share: &Arc<Share>,
    guest: &Arc<Guest>,
    dc: &Arc<dyn DataChannel>,
    decide: oneshot::Receiver<bool>,
    approve: bool,
    from_guest: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    events: &mut mpsc::UnboundedReceiver<Event>,
) -> Result<(), &'static str> {
    let mut control = guest.control.subscribe();
    let first = *control.borrow();
    rtc::send(dc, rtc::json_frame(&share.info(&first))).await.map_err(|_| "gone")?;
    if approve {
        rtc::send(dc, rtc::json_frame(&json!({ "event": "waiting" }))).await.map_err(|_| "gone")?;
        let mut decide = decide;
        let yes = loop {
            tokio::select! {
                d = &mut decide => break d.unwrap_or(false),
                m = from_guest.recv() => if m.is_none() { return Ok(()) },
                _ = control.changed() => if let Some(why) = control.borrow().end { return Err(why) },
                e = events.recv() => if gone(e) { return Ok(()) },
            }
        };
        if !yes {
            let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "denied" }))).await;
            rtc::flush(dc).await;
            return Ok(());
        }
        guest.view.lock().unwrap().state = "in".into();
        changed();
    }
    let now = *control.borrow();
    rtc::send(dc, rtc::json_frame(&json!({ "event": "admitted", "can_type": now.can_type, "can_control": now.can_control, "can_manage": now.can_manage }))).await.map_err(|_| "gone")?;

    let mut open = Open::new(share, dc);
    let mut access = share.access.subscribe();
    // A machine's sessions, sent as they change, by a task of their own (the
    // machine may be slow to answer; keys shouldn't wait for it).
    let (refresh, refreshes) = mpsc::unbounded_channel::<()>();
    let feed = share.target.is_machine().then(|| tokio::spawn(sessions_feed(share.clone(), dc.clone(), refreshes)));
    let mut last = now;
    let result = loop {
        tokio::select! {
            m = from_guest.recv() => {
                let Some(m) = m else { break Ok(()) };
                let c = *control.borrow();
                match super::parse_frame(&m) {
                    Some((0, super::JSON, body)) => {
                        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                        if req["op"] == "bye" {
                            break Ok(());
                        }
                        if req["op"] == "sessions" {
                            let _ = refresh.send(());
                            continue;
                        }
                        if matches!(req["op"].as_str(), Some("start" | "end" | "restart")) {
                            // Answered when it's done, then the sessions as they are.
                            let (share, dc, refresh) = (share.clone(), dc.clone(), refresh.clone());
                            tokio::spawn(async move {
                                let reply = manage(&share, &req, &c).await;
                                let _ = rtc::send(&dc, rtc::json_frame(&reply)).await;
                                let _ = refresh.send(());
                            });
                            continue;
                        }
                        open.request(&req, &c);
                    }
                    Some((channel, kind, payload)) => open.pass(channel, kind, payload, &c),
                    None => {}
                }
            }
            o = open.opened.1.recv() => {
                // A channel the guest asked for, opened on the machine (or not).
                let Some(o) = o else { continue };
                open.landed(o).await;
            }
            p = open.ended.1.recv() => {
                // A channel ended on the machine's side, or the guest fell too
                // far behind to keep.
                let Some((channel, generation, gone)) = p else { continue };
                if gone {
                    break Ok(());
                }
                if open.channels.get(&channel).is_some_and(|j| j.generation == generation) {
                    open.channels.remove(&channel);
                }
            }
            _ = access.changed() => {
                // The desktop or the servers shared changed: what's open beyond
                // them closes, and the guest hears what it has now.
                let c = *control.borrow();
                open.prune().await;
                let o = share.options();
                let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "access", "desktop": o.desktop, "ports": o.ports, "can_control": c.can_control }))).await;
            }
            _ = control.changed() => {
                let c = *control.borrow();
                if let Some(why) = c.end {
                    break Err(why);
                }
                if c.can_type != last.can_type {
                    // Typing on or off: sessions attach again the other way
                    // (the session server enforces it too).
                    open.retype(c.can_type);
                    let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "typing", "allowed": c.can_type }))).await;
                }
                if c.can_control != last.can_control {
                    let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "control", "allowed": c.can_control }))).await;
                }
                if c.can_manage != last.can_manage {
                    let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "manage", "allowed": c.can_manage }))).await;
                }
                last = c;
            }
            e = events.recv() => if gone(e) { break Ok(()) },
        }
    };
    if let Some(feed) = feed {
        feed.abort();
    }
    open.close_all();
    result
}

/// A machine's sessions to a guest: now, whenever they change (checked every
/// few seconds), and right away when asked.
async fn sessions_feed(share: Arc<Share>, dc: Arc<dyn DataChannel>, mut refreshes: mpsc::UnboundedReceiver<()>) {
    let mut sent = Value::Null;
    let mut force = true;
    loop {
        if let Ok(list) = sessions(&share).await
            && (force || list != sent)
        {
            if rtc::send(&dc, rtc::json_frame(&json!({ "event": "sessions", "sessions": list }))).await.is_err() {
                return;
            }
            sent = list;
        }
        force = false;
        tokio::select! {
            _ = tokio::time::sleep(SESSIONS_EVERY) => {}
            r = refreshes.recv() => match r {
                Some(()) => force = true,
                None => return,
            },
        }
    }
}

/// A guest starting, ending or restarting a session: `{"op", "req", "agent"
/// | "session"}`, answered `{"event": "done", "req", "session"}` or
/// `{"event": "failed", "req", "error"}`. Only on a machine share, for a
/// guest let do it; only sessions that are there, agents installed there.
async fn manage(share: &Arc<Share>, req: &Value, c: &Control) -> Value {
    let id = req["req"].as_u64().unwrap_or(0);
    let done = async {
        if !share.target.is_machine() || !c.can_manage {
            return Err(format!("Ask {} to let you start and end sessions.", share.target.by));
        }
        let op = req["op"].as_str().unwrap_or("");
        if op == "start" {
            let agent = req["agent"].as_str().unwrap_or("shell");
            if agent != "shell" && !share.source.agents().iter().any(|a| a == agent) {
                return Err(format!("{agent} isn't installed on {}.", share.target.machine_name));
            }
            return share.source.start(agent).await;
        }
        let name = req["session"].as_str().unwrap_or("").to_string();
        let list = sessions(share).await?;
        if name.is_empty() || !list.as_array().into_iter().flatten().any(|s| s["name"] == name) {
            return Err("That session isn't there anymore.".into());
        }
        share.source.stop(&name, op == "restart").await.map(|()| name)
    };
    match done.await {
        Ok(session) => json!({ "event": "done", "req": id, "session": session }),
        Err(e) => json!({ "event": "failed", "req": id, "error": e }),
    }
}

/// The peer connection failed or closed.
fn gone(e: Option<Event>) -> bool {
    matches!(e, None | Some(Event::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)))
}

/// What a guest's channel is joined to.
#[derive(Clone, PartialEq)]
enum Kind {
    Session { name: String, can_type: bool },
    Desktop,
    Tcp(u16),
}

/// One of the guest's channels, joined to one of the machine's.
struct Joined {
    link: Arc<Link>,
    link_channel: u32,
    kind: Kind,
    /// Tells this joining from an earlier one on the same channel.
    generation: u64,
    pump: tokio::task::JoinHandle<()>,
}

impl Joined {
    fn close(self) {
        self.pump.abort();
        self.link.close_channel(self.link_channel);
    }
}

/// What a request on channel 0 got going: the reply, and the machine's
/// channel to join.
type Attached = (Value, Arc<Link>, u32, mpsc::UnboundedReceiver<(u8, Vec<u8>)>, Kind);

/// A channel's opening, done: which request it was (`ticket`), whether the
/// guest gets a reply (not for a session attaching again), and the result.
struct Opened {
    channel: u32,
    ticket: u64,
    reply: bool,
    result: Result<Attached, String>,
}

/// A guest's open channels, and those being opened.
struct Open {
    share: Arc<Share>,
    dc: Arc<dyn DataChannel>,
    channels: HashMap<u32, Joined>,
    /// Channels being opened, by the ticket of the latest request for each:
    /// an older one landing (or one closed meanwhile) is let go.
    pending: HashMap<u32, u64>,
    opened: (mpsc::UnboundedSender<Opened>, mpsc::UnboundedReceiver<Opened>),
    /// (channel, generation, the guest is gone) as pumps end.
    ended: (mpsc::UnboundedSender<(u32, u64, bool)>, mpsc::UnboundedReceiver<(u32, u64, bool)>),
    generation: u64,
}

impl Open {
    fn new(share: &Arc<Share>, dc: &Arc<dyn DataChannel>) -> Self {
        Open {
            share: share.clone(),
            dc: dc.clone(),
            channels: HashMap::new(),
            pending: HashMap::new(),
            opened: mpsc::unbounded_channel(),
            ended: mpsc::unbounded_channel(),
            generation: 0,
        }
    }

    /// A request on channel 0: `watch`, `desktop`, `tcp`, `close`. Opening
    /// asks the machine, so it runs beside the guest's loop; `landed` finishes it.
    fn request(&mut self, req: &Value, c: &Control) {
        let op = req["op"].as_str().unwrap_or("");
        let Some(channel) = req["channel"].as_u64().filter(|ch| (1..1 << 20).contains(ch)).map(|ch| ch as u32) else { return };
        if !matches!(op, "watch" | "desktop" | "tcp" | "close") {
            return;
        }
        // A channel holds one thing: asking again replaces it.
        self.close(channel);
        if op == "close" {
            return;
        }
        if self.channels.len() + self.pending.len() >= MAX_CHANNELS {
            let refused = json!({ "event": "refused", "channel": channel, "error": "Too much open at once. Close something first." });
            let dc = self.dc.clone();
            tokio::spawn(async move { rtc::send(&dc, rtc::json_frame(&refused)).await });
            return;
        }
        let share = self.share.clone();
        let (session, quality, port, can_type) = (req["session"].as_str().unwrap_or("").to_string(), req["quality"].as_str().unwrap_or("").to_string(), req["port"].as_u64().unwrap_or(0), c.can_type);
        let op = op.to_string();
        self.begin(channel, true, async move {
            match op.as_str() {
                "watch" => open_watch(&share, channel, &session, can_type).await,
                "desktop" => open_desktop(&share, channel, &quality).await,
                _ => open_tcp(&share, channel, port).await,
            }
        });
    }

    /// Opens a channel beside the loop.
    fn begin(&mut self, channel: u32, reply: bool, opening: impl std::future::Future<Output = Result<Attached, String>> + Send + 'static) {
        self.generation += 1;
        let ticket = self.generation;
        self.pending.insert(channel, ticket);
        let done = self.opened.0.clone();
        tokio::spawn(async move {
            let result = opening.await;
            let _ = done.send(Opened { channel, ticket, reply, result });
        });
    }

    /// A channel's opening is done: the reply first, then what it carries.
    async fn landed(&mut self, o: Opened) {
        if self.pending.get(&o.channel) != Some(&o.ticket) {
            // Closed or asked again meanwhile: not wanted anymore.
            if let Ok((_, link, link_channel, _, _)) = o.result {
                link.close_channel(link_channel);
            }
            return;
        }
        self.pending.remove(&o.channel);
        match o.result {
            Ok((reply, link, link_channel, frames, kind)) => {
                if o.reply {
                    let _ = rtc::send(&self.dc, rtc::json_frame(&reply)).await;
                }
                self.join(o.channel, link, link_channel, frames, kind);
            }
            Err(e) if o.reply => {
                let _ = rtc::send(&self.dc, rtc::json_frame(&json!({ "event": "refused", "channel": o.channel, "error": e }))).await;
            }
            // A session that couldn't attach again: the guest's channel ends.
            Err(_) => {
                let _ = rtc::send(&self.dc, super::frame(o.channel, super::CLOSE, &[])).await;
            }
        }
    }

    /// Lets go of a channel, open or being opened.
    fn close(&mut self, channel: u32) {
        self.pending.remove(&channel);
        if let Some(j) = self.channels.remove(&channel) {
            j.close();
        }
    }

    /// Joins a guest channel to a machine channel: what the machine sends of
    /// the kinds that belong there goes to the guest, then CLOSE.
    fn join(&mut self, channel: u32, link: Arc<Link>, link_channel: u32, frames: mpsc::UnboundedReceiver<(u8, Vec<u8>)>, kind: Kind) {
        self.generation += 1;
        let generation = self.generation;
        let (dc, ended) = (self.dc.clone(), self.ended.0.clone());
        let desktop = kind == Kind::Desktop;
        let passes: &'static [u8] = match kind {
            Kind::Session { .. } => &[super::DATA, super::SIZE, super::EXIT],
            Kind::Desktop => &[TILES, FRAME],
            Kind::Tcp(_) => &[super::DATA],
        };
        let pump = tokio::spawn(async move {
            let sent = if desktop { pump_desktop(&dc, channel, frames).await } else { pump(&dc, channel, frames, passes).await };
            let sent = match sent {
                Ok(()) => rtc::send(&dc, super::frame(channel, super::CLOSE, &[])).await,
                e => e,
            };
            let _ = ended.send((channel, generation, sent.is_err()));
        });
        self.channels.insert(channel, Joined { link, link_channel, kind, generation, pump });
    }

    /// A frame on one of the guest's channels: through only as allowed.
    fn pass(&mut self, channel: u32, kind: u8, payload: &[u8], c: &Control) {
        if kind == super::CLOSE {
            self.close(channel);
            return;
        }
        let Some(j) = self.channels.get(&channel) else { return };
        let through = match (&j.kind, kind) {
            (Kind::Session { can_type, .. }, super::DATA) => *can_type && c.can_type,
            (Kind::Desktop, INPUT) => c.can_control && self.share.options().desktop == Desktop::Control,
            (Kind::Tcp(_), super::DATA) => true,
            _ => false,
        };
        if through {
            j.link.send(j.link_channel, kind, payload);
        }
    }

    /// Sessions attach again with typing on or off (beside the loop).
    fn retype(&mut self, can_type: bool) {
        let sessions: Vec<(u32, String)> = self.channels.iter().filter_map(|(ch, j)| if let Kind::Session { name, .. } = &j.kind { Some((*ch, name.clone())) } else { None }).collect();
        for (channel, name) in sessions {
            self.close(channel);
            let share = self.share.clone();
            self.begin(channel, false, async move {
                let (link, link_channel, frames) = attach(&share, &name, can_type).await?;
                Ok((Value::Null, link, link_channel, frames, Kind::Session { name, can_type }))
            });
        }
    }

    /// Closes what the share no longer allows (the desktop, servers).
    async fn prune(&mut self) {
        let o = self.share.options();
        let gone: Vec<u32> = self
            .channels
            .iter()
            .filter(|(_, j)| match &j.kind {
                Kind::Desktop => o.desktop == Desktop::None,
                Kind::Tcp(port) => !o.ports.iter().any(|p| p.port == *port),
                Kind::Session { .. } => false,
            })
            .map(|(ch, _)| *ch)
            .collect();
        for channel in gone {
            self.close(channel);
            let _ = rtc::send(&self.dc, super::frame(channel, super::CLOSE, &[])).await;
        }
    }

    fn close_all(&mut self) {
        self.pending.clear();
        for (_, j) in self.channels.drain() {
            j.close();
        }
    }
}

/// A session: the shared one, or (a machine) one of its sessions.
async fn open_watch(share: &Arc<Share>, channel: u32, asked: &str, can_type: bool) -> Result<Attached, String> {
    let name = if share.target.is_machine() {
        let list = sessions(share).await?;
        let known = list.as_array().into_iter().flatten().any(|s| s["name"] == asked);
        if asked.is_empty() || !known {
            return Err("That session isn't there anymore.".into());
        }
        asked.to_string()
    } else {
        share.target.name.clone()
    };
    let (link, link_channel, frames) = attach(share, &name, can_type).await?;
    let reply = json!({ "event": "watching", "channel": channel, "session": name });
    Ok((reply, link, link_channel, frames, Kind::Session { name, can_type }))
}

async fn attach(share: &Arc<Share>, name: &str, can_type: bool) -> Result<(Arc<Link>, u32, mpsc::UnboundedReceiver<(u8, Vec<u8>)>), String> {
    let link = share.machine().await?;
    if !supports_sharing(link.version()) {
        return Err(format!("Muxin's agent on {} needs an update to share sessions.", share.target.machine_name));
    }
    let (link_channel, frames) = link.open_channel();
    let request = json!({ "op": "attach", "name": name, "channel": link_channel, "watch_only": !can_type, "sizes": true });
    let reply = match link.request(request, Duration::from_secs(15)).await {
        Ok(r) => r,
        Err(e) => {
            link.close_channel(link_channel);
            return Err(e);
        }
    };
    if reply["ok"].as_bool() != Some(true) {
        link.close_channel(link_channel);
        return Err(reply["error"].as_str().unwrap_or("The session couldn't be shown.").to_string());
    }
    Ok((link, link_channel, frames))
}

/// The machine's desktop, at one of the qualities a guest can pick.
async fn open_desktop(share: &Arc<Share>, channel: u32, quality: &str) -> Result<Attached, String> {
    if !share.target.is_machine() || share.options().desktop == Desktop::None {
        return Err("The desktop isn't shared.".into());
    }
    let link = share.machine().await?;
    let (link_channel, frames) = link.open_channel();
    let (quality, (fps, jpeg, max_width)) = desktop_quality(quality);
    let request = json!({ "op": "desktop", "channel": link_channel, "fps": fps, "quality": jpeg, "max_width": max_width, "tiles": true, "display": "" });
    let reply = match link.request(request, Duration::from_secs(20)).await {
        Ok(r) => r,
        Err(e) => {
            link.close_channel(link_channel);
            return Err(e);
        }
    };
    if reply["ok"].as_bool() != Some(true) {
        link.close_channel(link_channel);
        return Err(reply["error"].as_str().unwrap_or("The desktop couldn't be shown.").to_string());
    }
    let reply = json!({ "event": "desktop", "channel": channel, "width": reply["width"], "height": reply["height"], "quality": quality });
    Ok((reply, link, link_channel, frames, Kind::Desktop))
}

/// One of the servers the user picked, on the machine itself.
async fn open_tcp(share: &Arc<Share>, channel: u32, port: u64) -> Result<Attached, String> {
    let shared = share.options().ports.into_iter().find(|p| p.port as u64 == port).filter(|_| share.target.is_machine());
    let Some(shared) = shared else {
        return Err("That server isn't shared.".into());
    };
    let link = share.machine().await?;
    let (link_channel, frames) = link.open_channel();
    let request = json!({ "op": "tcp", "channel": link_channel, "port": port, "host": shared.host });
    let reply = match link.request(request, Duration::from_secs(15)).await {
        Ok(r) => r,
        Err(e) => {
            link.close_channel(link_channel);
            return Err(e);
        }
    };
    if reply["ok"].as_bool() != Some(true) {
        link.close_channel(link_channel);
        return Err(reply["error"].as_str().unwrap_or("That server didn't answer.").to_string());
    }
    Ok((json!({ "event": "tcp", "channel": channel, "port": port }), link, link_channel, frames, Kind::Tcp(port as u16)))
}

/// A machine channel's frames of the kinds that belong, to the guest's
/// channel, as they come (waiting while the guest catches up).
async fn pump(dc: &Arc<dyn DataChannel>, channel: u32, mut frames: mpsc::UnboundedReceiver<(u8, Vec<u8>)>, passes: &[u8]) -> Result<(), String> {
    while let Some((kind, bytes)) = frames.recv().await {
        if passes.contains(&kind) {
            rtc::send(dc, super::frame(channel, kind, &bytes)).await?;
        }
    }
    Ok(())
}

/// Bytes on their way to a guest before the desktop holds back.
const DESKTOP_BACKLOG: usize = 512 << 10;

/// The desktop to a guest. While its connection is behind, what changed
/// waits and only the newest of each part of the screen goes once it
/// catches up: a slow guest sees fewer frames, never an old screen or a
/// broken one.
async fn pump_desktop(dc: &Arc<dyn DataChannel>, channel: u32, mut frames: mpsc::UnboundedReceiver<(u8, Vec<u8>)>) -> Result<(), String> {
    let mut waiting = Tiles::default();
    loop {
        let next = if waiting.is_empty() {
            frames.recv().await
        } else {
            tokio::select! {
                f = frames.recv() => f,
                _ = tokio::time::sleep(Duration::from_millis(30)) => Some((0, Vec::new())),
            }
        };
        match next {
            None => return Ok(()),
            Some((TILES, bytes)) => {
                if !waiting.add(&bytes) {
                    continue;
                }
            }
            Some((FRAME, bytes)) => {
                rtc::send(dc, super::frame(channel, FRAME, &bytes)).await?;
                continue;
            }
            Some(_) => {}
        }
        if waiting.is_empty() || dc.outstanding_bytes().await.unwrap_or(0) > DESKTOP_BACKLOG {
            continue;
        }
        rtc::send(dc, super::frame(channel, TILES, &waiting.take())).await?;
    }
}

/// Screen patches waiting to go: `[width][height][count]` then each
/// `[x][y][w][h][len: u32][jpeg]` (agent/src/desktop.rs). A patch replaces
/// the ones before it that it covers.
#[derive(Default)]
struct Tiles {
    size: (u16, u16),
    patches: Vec<([u16; 4], Vec<u8>)>,
}

impl Tiles {
    fn is_empty(&self) -> bool {
        self.patches.is_empty()
    }

    /// Adds a TILES message's patches (false: it didn't read).
    fn add(&mut self, msg: &[u8]) -> bool {
        let u16_at = |o: usize| msg.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
        let (Some(w), Some(h), Some(count)) = (u16_at(0), u16_at(2), u16_at(4)) else { return false };
        let mut new = Vec::with_capacity(count as usize);
        let mut o = 6;
        for _ in 0..count {
            let (Some(x), Some(y), Some(pw), Some(ph)) = (u16_at(o), u16_at(o + 2), u16_at(o + 4), u16_at(o + 6)) else { return false };
            let Some(len) = msg.get(o + 8..o + 12).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize) else { return false };
            let Some(jpeg) = msg.get(o + 12..o + 12 + len) else { return false };
            new.push(([x, y, pw, ph], jpeg.to_vec()));
            o += 12 + len;
        }
        if self.size != (w, h) {
            self.patches.clear();
            self.size = (w, h);
        }
        let covers = |a: &[u16; 4], b: &[u16; 4]| a[0] <= b[0] && a[1] <= b[1] && a[0] as u32 + a[2] as u32 >= b[0] as u32 + b[2] as u32 && a[1] as u32 + a[3] as u32 >= b[1] as u32 + b[3] as u32;
        self.patches.retain(|(old, _)| !new.iter().any(|(r, _)| covers(r, old)));
        self.patches.extend(new);
        true
    }

    /// One TILES message of everything waiting.
    fn take(&mut self) -> Vec<u8> {
        let patches = std::mem::take(&mut self.patches);
        let mut msg = Vec::with_capacity(6 + patches.iter().map(|(_, j)| 12 + j.len()).sum::<usize>());
        for v in [self.size.0, self.size.1, patches.len() as u16] {
            msg.extend(v.to_be_bytes());
        }
        for (r, jpeg) in patches {
            for v in r {
                msg.extend(v.to_be_bytes());
            }
            msg.extend((jpeg.len() as u32).to_be_bytes());
            msg.extend(jpeg);
        }
        msg
    }
}

/// The desktop qualities a guest can pick, as (frames a second, JPEG
/// quality, widest frame): nothing heavier than "max". Medium unless asked.
pub const QUALITIES: [&str; 4] = ["low", "medium", "high", "max"];

fn desktop_quality(name: &str) -> (&'static str, (u32, u32, u32)) {
    match name {
        "low" => ("low", (10, 45, 1280)),
        "high" => ("high", (24, 75, 1920)),
        "max" => ("max", (30, 85, 2560)),
        _ => ("medium", (15, 60, 1600)),
    }
}

/// A machine's sessions, as a machine share's guests see them: no folders
/// or commands, just what to pick from.
async fn sessions(share: &Arc<Share>) -> Result<Value, String> {
    let link = share.machine().await?;
    let reply = link.request(json!({ "op": "list" }), Duration::from_secs(10)).await?;
    let now = chrono::Utc::now().timestamp();
    let list: Vec<Value> = reply["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            json!({
                "name": s["name"],
                "title": s["title"],
                "agent": s["agent"],
                "status": s["status"],
                "busy": s["last_output"].as_i64().is_some_and(|t| now - t < 6),
            })
        })
        .collect();
    Ok(Value::Array(list))
}

/// Agents from 0.2.9 attach watchers that can't type and report sizes.
fn supports_sharing(version: &str) -> bool {
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (0, 2, 9)
}

/// A guest's name as shown: printable, short, never empty.
fn clean_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    let name: String = name.chars().take(40).collect();
    if name.is_empty() { "Someone".into() } else { name }
}

/// "direct" or "relay": whether the connection goes through the TURN server.
async fn connection_path(pc: &Arc<dyn PeerConnection>) -> Option<String> {
    use ::rtc::peer_connection::transport::RTCIceCandidateType;
    use ::rtc::statistics::report::RTCStatsReportEntry;
    use ::rtc::statistics::stats::ice_candidate_pair::RTCStatsIceCandidatePairState;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let report = pc.get_stats(std::time::Instant::now(), webrtc::peer_connection::StatsSelector::None).await;
        let pair = report.iter().find_map(|e| match e {
            RTCStatsReportEntry::IceCandidatePair(p) if p.nominated && p.state == RTCStatsIceCandidatePairState::Succeeded => Some(p),
            _ => None,
        });
        let Some(pair) = pair else { continue };
        let relayed = [&pair.local_candidate_id, &pair.remote_candidate_id].iter().any(|id| {
            matches!(report.get(id), Some(RTCStatsReportEntry::LocalCandidate(c) | RTCStatsReportEntry::RemoteCandidate(c)) if c.candidate_type == RTCIceCandidateType::Relay)
        });
        return Some(if relayed { "relay" } else { "direct" }.into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Sam \u{7}\n Lee "), "Sam Lee");
        assert_eq!(clean_name(""), "Someone");
        assert_eq!(clean_name(&"x".repeat(100)).len(), 40);
    }

    fn tiles(size: (u16, u16), patches: &[([u16; 4], &[u8])]) -> Vec<u8> {
        let mut t = Tiles { size, patches: patches.iter().map(|(r, j)| (*r, j.to_vec())).collect() };
        t.take()
    }

    #[test]
    fn a_slow_guest_gets_the_newest_of_each_part() {
        let mut waiting = Tiles::default();
        assert!(waiting.add(&tiles((100, 50), &[([0, 0, 64, 32], b"a"), ([64, 0, 36, 32], b"b")])));
        // A later patch over the first one replaces it; the other stays.
        assert!(waiting.add(&tiles((100, 50), &[([0, 0, 64, 32], b"c")])));
        // One that only overlaps keeps both, in order.
        assert!(waiting.add(&tiles((100, 50), &[([32, 0, 40, 32], b"d")])));
        let kept: Vec<&[u8]> = waiting.patches.iter().map(|(_, j)| j.as_slice()).collect();
        assert_eq!(kept, [b"b".as_slice(), b"c", b"d"]);
        let mut again = Tiles::default();
        assert!(again.add(&waiting.take()));
        assert_eq!(again.patches.len(), 3);
        assert!(waiting.is_empty());
        // A new size starts over; a cut-short message doesn't read.
        assert!(again.add(&tiles((200, 100), &[([0, 0, 64, 32], b"e")])));
        assert_eq!(again.patches.len(), 1);
        let whole = tiles((200, 100), &[([0, 0, 64, 32], b"jpeg")]);
        assert!(!again.add(&whole[..whole.len() - 1]));
    }

    #[test]
    fn guests_pick_only_the_desktop_qualities_offered() {
        for q in QUALITIES {
            assert_eq!(desktop_quality(q).0, q);
        }
        assert_eq!(desktop_quality("").0, "medium");
        assert_eq!(desktop_quality("8k").1, desktop_quality("medium").1);
    }

    #[test]
    fn knows_which_agents_share() {
        assert!(supports_sharing("0.2.9"));
        assert!(supports_sharing("0.3.0"));
        assert!(!supports_sharing("0.2.8"));
        assert!(!supports_sharing(""));
    }
}
