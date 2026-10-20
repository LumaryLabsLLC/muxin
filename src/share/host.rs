//! Hosting shares: each one waits for guests at the share server, checks
//! them, asks the user to let them in, then relays the one session shared.
//!
//! The gateway is the security boundary. A guest never speaks the machine's
//! link protocol: it may say `hello`, `watch` and `bye`, and send keys on
//! the channel it watches, which reach the session only when the user let
//! that guest type. Its size is never applied (the session keeps the
//! user's), and nothing else it asks for (another session, a shell, files)
//! exists here.

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

/// What's shared: one session.
#[derive(Clone, Debug, Serialize)]
pub struct Target {
    /// `<machine>/<session>`.
    pub session_id: String,
    pub machine_id: String,
    /// The session's name on the machine.
    pub name: String,
    pub title: String,
    /// "claude", "codex", "shell"...
    pub agent: String,
    pub machine_name: String,
    /// Who shares it, as guests see it.
    pub by: String,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Ask before letting each guest in.
    pub approve: bool,
    /// New guests may type.
    pub can_type: bool,
    pub ttl: Duration,
}

/// Where the shared session's machine is reached.
#[async_trait::async_trait]
pub trait Source: Send + Sync + 'static {
    async fn link(&self) -> Result<Arc<Link>, String>;
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
    /// Unix seconds.
    pub ends_at: i64,
    pub guests: Vec<GuestView>,
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
    ends_at: Mutex<SystemTime>,
    guests: Mutex<HashMap<u64, Arc<Guest>>>,
    stop: watch::Sender<Option<&'static str>>,
    bad_proofs: Mutex<u32>,
}

impl Share {
    fn view(&self) -> ShareView {
        let o = *self.options.lock().unwrap();
        let mut guests: Vec<GuestView> = self.guests.lock().unwrap().values().map(|g| g.view.lock().unwrap().clone()).collect();
        guests.sort_by_key(|g| g.id);
        ShareView {
            id: self.ticket.room(),
            link: self.ticket.link(),
            target: self.target.clone(),
            approve: o.approve,
            can_type: o.can_type,
            ends_at: unix(*self.ends_at.lock().unwrap()),
            guests,
        }
    }

    fn ends_in(&self) -> Duration {
        self.ends_at.lock().unwrap().duration_since(SystemTime::now()).unwrap_or(Duration::ZERO)
    }

    fn info(&self, can_type: bool) -> Value {
        json!({
            "event": "share",
            "title": self.target.title,
            "agent": self.target.agent,
            "machine": self.target.machine_name,
            "by": self.target.by,
            "can_type": can_type,
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
    all.sort_by(|a, b| a.target.session_id.cmp(&b.target.session_id).then(a.id.cmp(&b.id)));
    all
}

fn find(id: &str) -> Option<Arc<Share>> {
    registry().shares.lock().unwrap().get(id).cloned()
}

fn guest(share: &str, guest: u64) -> Option<Arc<Guest>> {
    find(share)?.guests.lock().unwrap().get(&guest).cloned()
}

/// Starts sharing a session; the view carries the link to give out. Runs
/// on the current Tokio runtime until stopped, expired, or the app quits.
pub async fn start(server: &str, target: Target, options: Options, source: Arc<dyn Source>) -> Result<ShareView, String> {
    let (cert, fingerprint) = rtc::certificate()?;
    let ticket = Ticket::new(server, fingerprint)?;
    let server = signal::Server::new(&ticket.server);
    let key = signal::host_key();
    // The first check-in: the server answers, and keeps the room for us.
    server.check_in(&ticket.room(), &key).await.map_err(|e| e.to_string())?;
    let ttl = options.ttl.min(MAX_TTL);
    let share = Arc::new(Share {
        ticket,
        server,
        key,
        connecting: std::sync::atomic::AtomicUsize::new(0),
        cert,
        target,
        source,
        options: Mutex::new(Options { ttl, ..options }),
        ends_at: Mutex::new(SystemTime::now() + ttl),
        guests: Mutex::new(HashMap::new()),
        stop: watch::channel(None).0,
        bad_proofs: Mutex::new(0),
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
        g.control.send_modify(|c| c.can_type = allowed);
        g.view.lock().unwrap().can_type = allowed;
        changed();
    }
}

/// Who can join and whether guests can type, for a share already going:
/// new guests follow it, and guests in get typing on or off with it. Only
/// guests the user lets in can type; letting anyone in lets in whoever waits.
pub fn set_options(id: &str, approve: bool, can_type: bool) {
    let Some(s) = find(id) else { return };
    let can_type = can_type && approve;
    {
        let mut o = s.options.lock().unwrap();
        o.approve = approve;
        o.can_type = can_type;
    }
    for g in s.guests.lock().unwrap().values() {
        let waiting = g.view.lock().unwrap().state == "waiting";
        if waiting {
            if !approve && let Some(tx) = g.decide.lock().unwrap().take() {
                let _ = tx.send(true);
            }
            continue;
        }
        g.control.send_modify(|c| c.can_type = can_type);
        g.view.lock().unwrap().can_type = can_type;
    }
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
    run_guest(&share, &pc, dc, &host_fp, &guest_fp, events).await;
    let _ = pc.close().await;
}

/// A guest's connection, from its hello until it leaves or is let go.
async fn run_guest(share: &Arc<Share>, pc: &Arc<dyn PeerConnection>, dc: Arc<dyn DataChannel>, host_fp: &str, guest_fp: &str, mut events: mpsc::UnboundedReceiver<Event>) {
    // What the guest sends, read in one place.
    let (msg_tx, mut from_guest) = mpsc::unbounded_channel::<Vec<u8>>();
    let reader = dc.clone();
    tokio::spawn(async move {
        while let Some(m) = rtc::next_message(&reader).await {
            if msg_tx.send(m).is_err() {
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
    let options = *share.options.lock().unwrap();
    let id = registry().next_guest.fetch_add(1, Ordering::Relaxed) + 1;
    let client = if hello["client"] == "browser" { "browser" } else { "app" };
    let view = GuestView {
        id,
        name: clean_name(hello["name"].as_str().unwrap_or("")),
        code: share.ticket.code(guest_fp),
        client: client.into(),
        state: if options.approve { "waiting" } else { "in" }.into(),
        can_type: options.can_type,
        path: String::new(),
    };
    let (decide_tx, decide_rx) = oneshot::channel();
    let guest = Arc::new(Guest {
        view: Mutex::new(view),
        decide: Mutex::new(options.approve.then_some(decide_tx)),
        control: watch::channel(Control { can_type: options.can_type, end: None }).0,
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

    let result = admitted(share, &guest, &dc, decide_rx, options, &mut from_guest, &mut events).await;
    if let Err(why) = result {
        let _ = rtc::send(&dc, rtc::json_frame(&json!({ "event": "ended", "why": why }))).await;
        rtc::flush(&dc).await;
    }
    share.guests.lock().unwrap().remove(&id);
    changed();
}

/// Lets the guest in (asking first when the share says so), then relays
/// the session until something ends it: Err(why) to tell the guest.
async fn admitted(
    share: &Arc<Share>,
    guest: &Arc<Guest>,
    dc: &Arc<dyn DataChannel>,
    decide: oneshot::Receiver<bool>,
    options: Options,
    from_guest: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    events: &mut mpsc::UnboundedReceiver<Event>,
) -> Result<(), &'static str> {
    let mut control = guest.control.subscribe();
    rtc::send(dc, rtc::json_frame(&share.info(options.can_type))).await.map_err(|_| "gone")?;
    if options.approve {
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
    let can_type = control.borrow().can_type;
    rtc::send(dc, rtc::json_frame(&json!({ "event": "admitted", "can_type": can_type }))).await.map_err(|_| "gone")?;

    // The session, once the guest asks for it.
    let mut watching: Option<Watching> = None;
    let result = loop {
        let session = async {
            match watching.as_mut() {
                Some(w) => w.frames.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            m = from_guest.recv() => {
                let Some(m) = m else { break Ok(()) };
                match super::parse_frame(&m) {
                    Some((0, super::JSON, body)) => {
                        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                        match req["op"].as_str().unwrap_or("") {
                            "watch" => {
                                let channel = req["channel"].as_u64().filter(|c| (1..1 << 20).contains(c)).unwrap_or(1) as u32;
                                if let Some(w) = watching.take() {
                                    w.close();
                                }
                                let can_type = control.borrow().can_type;
                                match watch_session(share, channel, can_type).await {
                                    Ok(w) => watching = Some(w),
                                    Err(e) => {
                                        let _ = rtc::send(dc, rtc::error_frame(&e)).await;
                                    }
                                }
                            }
                            "bye" => break Ok(()),
                            _ => {}
                        }
                    }
                    // Keys, only from a guest the user lets type.
                    Some((channel, super::DATA, keys)) => {
                        if let Some(w) = watching.as_ref().filter(|w| w.guest_channel == channel && w.can_type) {
                            w.link.send(w.link_channel, crate::fleet::link::DATA, keys);
                        }
                    }
                    _ => {}
                }
            }
            f = session => {
                let w = watching.as_ref().expect("watching");
                match f {
                    Some((kind, bytes)) if [super::DATA, super::SIZE, super::EXIT].contains(&kind) => {
                        if rtc::send(dc, super::frame(w.guest_channel, kind, &bytes)).await.is_err() {
                            break Ok(());
                        }
                    }
                    Some(_) => {}
                    None => {
                        let channel = w.guest_channel;
                        watching = None;
                        let _ = rtc::send(dc, super::frame(channel, super::CLOSE, &[])).await;
                    }
                }
            }
            _ = control.changed() => {
                let c = *control.borrow();
                if let Some(why) = c.end {
                    break Err(why);
                }
                if watching.as_ref().is_some_and(|w| w.can_type != c.can_type) {
                    // Typing on or off: attach again the other way (the
                    // session server enforces it too).
                    let channel = watching.as_ref().map(|w| w.guest_channel).unwrap_or(1);
                    if let Some(w) = watching.take() {
                        w.close();
                    }
                    watching = watch_session(share, channel, c.can_type).await.ok();
                }
                let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "typing", "allowed": c.can_type }))).await;
            }
            e = events.recv() => if gone(e) { break Ok(()) },
        }
    };
    if let Some(w) = watching {
        w.close();
    }
    result
}

/// The peer connection failed or closed.
fn gone(e: Option<Event>) -> bool {
    matches!(e, None | Some(Event::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)))
}

/// The shared session, attached for one guest.
struct Watching {
    link: Arc<Link>,
    link_channel: u32,
    guest_channel: u32,
    can_type: bool,
    frames: mpsc::UnboundedReceiver<(u8, Vec<u8>)>,
}

impl Watching {
    fn close(self) {
        self.link.close_channel(self.link_channel);
    }
}

async fn watch_session(share: &Arc<Share>, guest_channel: u32, can_type: bool) -> Result<Watching, String> {
    let link = share.source.link().await?;
    if !supports_sharing(link.version()) {
        return Err(format!("Muxin's agent on {} needs an update to share sessions.", share.target.machine_name));
    }
    let (link_channel, frames) = link.open_channel();
    let request = json!({ "op": "attach", "name": share.target.name, "channel": link_channel, "watch_only": !can_type, "sizes": true });
    let reply = link.request(request, Duration::from_secs(15)).await?;
    if reply["ok"].as_bool() != Some(true) {
        link.close_channel(link_channel);
        return Err(reply["error"].as_str().unwrap_or("The session couldn't be shown.").to_string());
    }
    Ok(Watching { link, link_channel, guest_channel, can_type, frames })
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

    #[test]
    fn knows_which_agents_share() {
        assert!(supports_sharing("0.2.9"));
        assert!(supports_sharing("0.3.0"));
        assert!(!supports_sharing("0.2.8"));
        assert!(!supports_sharing(""));
    }
}
