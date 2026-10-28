//! Pairing an iPad with this computer's machines, by a one-off link that
//! Muxin shows as a QR code: `muxin://pair?server=<server>#1.<ticket>`.
//!
//! The iPad reaches this app the way a share's guest does (mod.rs): through
//! the share server, then straight over WebRTC. It proves it has the link's
//! secret, and both screens show the same 6-digit code. Once the user allows
//! it here, each chosen machine's agent is asked to trust the iPad's own key
//! (its `pair` op, as when this app pairs), and the iPad gets what it needs
//! to reach those machines directly. The iPad's key never leaves it, and
//! nothing of this app's goes to it.
//!
//! iPad to Mac, on channel 0: `{"op": "pair", "v": 1, "name", "fingerprint"
//! (its key's, for the agents), "proof"}`, then `{"op": "done"}` once it has
//! the machines. Mac to iPad: `{"event": "waiting"}` (for the user to allow
//! it), `"pairing"` (`machines`: their names), `"paired"` (`machines`: each
//! one's `id`, `name`, `host` and `direct`; `failed`: `{name, error}`),
//! `"denied"`, `"ended"` (`why`), or `"error"`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::{BoxFuture, join_all};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::{PeerConnection, RTCCertificate, RTCPeerConnectionState, RTCSessionDescription};

use super::rtc::{self, Event};
use super::signal;
use super::ticket::{Ticket, hex};
use crate::fleet::direct;

/// How long a pairing link works.
const TTL: Duration = Duration::from_secs(10 * 60);
/// How often this app asks the server for an iPad's offer.
const CHECK_IN: Duration = Duration::from_secs(2);
const HELLO_TIMEOUT: Duration = Duration::from_secs(20);
/// Wrong proofs (someone guessing) before the link stops working.
const MAX_BAD_PROOFS: u32 = 5;
/// How long an iPad waits for the user to allow it.
const DECIDE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// A machine as the iPad gets it: its entry in config.toml, and how to reach
/// it directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub host: String,
    pub direct: direct::Paired,
}

/// A machine that couldn't trust the iPad, and why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Failed {
    pub name: String,
    pub error: String,
}

/// Has a machine's agent (by id) trust the iPad's key (its fingerprint):
/// the machine, for the iPad.
pub type Trust = Box<dyn Fn(String, String) -> BoxFuture<'static, Result<Machine, String>> + Send + Sync>;
/// The iPad paired: its name, key, and the machines that trust it (ids).
pub type Paired = Box<dyn Fn(String, String, Vec<String>) + Send + Sync>;

/// What pairing needs from the rest of the app.
pub struct Hooks {
    pub trust: Trust,
    pub paired: Paired,
}

/// Where a pairing is, for this app to show.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct View {
    /// The link, shown as a QR code.
    pub link: String,
    /// "waiting" (for an iPad to open the link), "asking" (one is here:
    /// allow it?), "pairing", "done", "denied", "failed" (`error`), or
    /// "ended" (stopped, or the link expired).
    pub state: String,
    /// The iPad's name and the code it shows, once it's here.
    pub device: Option<String>,
    pub code: Option<String>,
    /// The machines it gets, by name; those that trust it now; those that couldn't.
    pub machines: Vec<String>,
    pub paired: Vec<String>,
    pub failed: Vec<Failed>,
    pub error: Option<String>,
    /// When the link stops working (Unix seconds).
    pub expires_at: i64,
}

impl View {
    pub fn finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "denied" | "failed" | "ended")
    }
}

struct Pairing {
    ticket: Ticket,
    server: signal::Server,
    key: String,
    cert: RTCCertificate,
    /// Id and name of each machine the iPad gets.
    machines: Vec<(String, String)>,
    hooks: Hooks,
    view: watch::Sender<View>,
    decide: Mutex<Option<oneshot::Sender<bool>>>,
    stop: watch::Sender<bool>,
    /// An iPad is connected (one at a time).
    busy: AtomicBool,
    bad_proofs: AtomicU32,
}

impl Pairing {
    fn update(&self, f: impl FnOnce(&mut View)) {
        self.view.send_modify(f);
    }
}

fn current() -> &'static Mutex<Option<Arc<Pairing>>> {
    static CURRENT: OnceLock<Mutex<Option<Arc<Pairing>>>> = OnceLock::new();
    CURRENT.get_or_init(Default::default)
}

/// Starts pairing an iPad with `machines` (id and name), ending one that's
/// going on: what to show, as it changes.
pub async fn start(server: &str, machines: Vec<(String, String)>, hooks: Hooks) -> Result<watch::Receiver<View>, String> {
    stop();
    if machines.is_empty() {
        return Err("There's no machine to pair an iPad with yet. Add one first (this computer itself doesn't count).".into());
    }
    let (cert, fingerprint) = rtc::certificate()?;
    let ticket = Ticket::new(server, fingerprint)?;
    let server = signal::Server::new(&ticket.server);
    let key = signal::host_key();
    // The first check-in: the server keeps the room for us.
    server.check_in(&ticket.room(), &key).await.map_err(|e| e.to_string())?;
    let expires_at = SystemTime::now().checked_add(TTL).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
    let view = View {
        link: ticket.pair_link(),
        state: "waiting".into(),
        device: None,
        code: None,
        machines: machines.iter().map(|(_, n)| n.clone()).collect(),
        paired: Vec::new(),
        failed: Vec::new(),
        error: None,
        expires_at,
    };
    let (tx, rx) = watch::channel(view);
    let p = Arc::new(Pairing {
        ticket,
        server,
        key,
        cert,
        machines,
        hooks,
        view: tx,
        decide: Mutex::new(None),
        stop: watch::channel(false).0,
        busy: AtomicBool::new(false),
        bad_proofs: AtomicU32::new(0),
    });
    *current().lock().unwrap() = Some(p.clone());
    tokio::spawn(serve(p));
    Ok(rx)
}

/// The pairing going on, to watch.
pub fn watch() -> Option<watch::Receiver<View>> {
    current().lock().unwrap().as_ref().map(|p| p.view.subscribe())
}

/// Lets the iPad that's waiting pair, or not.
pub fn allow(yes: bool) {
    let p = current().lock().unwrap().clone();
    if let Some(tx) = p.and_then(|p| p.decide.lock().unwrap().take()) {
        let _ = tx.send(yes);
    }
}

/// Ends the pairing going on: its link stops working.
pub fn stop() {
    if let Some(p) = current().lock().unwrap().take() {
        p.stop.send_replace(true);
    }
}

/// Holds the room until an iPad pairs, the link expires, or it's stopped.
async fn serve(p: Arc<Pairing>) {
    let mut stop = p.stop.subscribe();
    let room = p.ticket.room();
    let deadline = tokio::time::Instant::now() + TTL;
    let mut wait = CHECK_IN;
    let why = loop {
        tokio::select! {
            _ = stop.changed() => break None,
            _ = tokio::time::sleep_until((tokio::time::Instant::now() + wait).min(deadline)) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            break Some("The link expired. Make a new one to pair an iPad.");
        }
        if p.bad_proofs.load(Ordering::Relaxed) >= MAX_BAD_PROOFS {
            break Some("Someone kept trying a wrong link, so this one stopped working. Make a new one.");
        }
        match p.server.check_in(&room, &p.key).await {
            Ok((ice, offers)) => {
                wait = CHECK_IN;
                if let Some((id, sdp)) = offers.into_iter().next()
                    && !p.busy.swap(true, Ordering::Relaxed)
                {
                    tokio::spawn(connect(p.clone(), id, sdp, ice));
                }
            }
            // Away from the server (offline, asleep): it gets the iPad once it answers again.
            Err(e) => {
                tracing::debug!("share server: {e}");
                wait = (wait * 2).min(Duration::from_secs(30));
            }
        }
    };
    // An iPad still waiting hears no.
    p.decide.lock().unwrap().take();
    p.stop.send_replace(true);
    p.update(|v| {
        if !v.finished() {
            v.state = "ended".into();
            v.error = why.map(str::to_string);
        }
    });
    {
        let mut slot = current().lock().unwrap();
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &p)) {
            *slot = None;
        }
    }
    p.server.close(&room, &p.key).await;
}

/// Answers an iPad's offer, then pairs it (or not). The link keeps working
/// for another try when it leaves before the user decided.
async fn connect(p: Arc<Pairing>, id: String, offer: String, ice: Value) {
    if let Some(guest_fp) = rtc::fingerprint(&offer)
        && let Some((pc, dc, events)) = answer(&p, &id, offer, &ice).await
    {
        let host_fp = hex(&p.ticket.fingerprint);
        talk(&p, &dc, &host_fp, &guest_fp, events).await;
        rtc::flush(&dc).await;
        rtc::forget(&dc);
        let _ = pc.close().await;
    }
    p.busy.store(false, Ordering::Relaxed);
    if p.view.borrow().finished() {
        p.stop.send_replace(true);
    }
}

/// The connection to an iPad, once its data channel is there.
async fn answer(p: &Pairing, id: &str, offer: String, ice: &Value) -> Option<(Arc<dyn PeerConnection>, Arc<dyn DataChannel>, mpsc::UnboundedReceiver<Event>)> {
    let (pc, mut events) = rtc::peer(ice, Some(p.cert.clone())).await.ok()?;
    let answered = async {
        pc.set_remote_description(RTCSessionDescription::offer(offer).map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
        let answer = pc.create_answer(None).await.map_err(|e| e.to_string())?;
        pc.set_local_description(answer).await.map_err(|e| e.to_string())?;
        rtc::gathered(&pc, &mut events, ice).await;
        let sdp = pc.local_description().await.ok_or("no answer")?.sdp;
        p.server.answer(&p.ticket.room(), &p.key, id, &sdp).await.map_err(|e| e.to_string())
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
}

/// The iPad's hello, the user's answer, the machines.
async fn talk(p: &Arc<Pairing>, dc: &Arc<dyn DataChannel>, host_fp: &str, guest_fp: &str, mut events: mpsc::UnboundedReceiver<Event>) {
    let mut from_ipad = messages(dc);
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, from_ipad.recv()).await {
        Ok(Some(v)) => v,
        _ => return,
    };
    if hello["op"] != "pair" || !p.ticket.check_proof(host_fp, guest_fp, hello["proof"].as_str().unwrap_or("")) {
        p.bad_proofs.fetch_add(1, Ordering::Relaxed);
        let _ = rtc::send(dc, rtc::error_frame("That pairing link doesn't work anymore. Make a new one on your Mac.")).await;
        return;
    }
    let key = hello["fingerprint"].as_str().unwrap_or("").trim().to_lowercase();
    if key.len() != 64 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        let _ = rtc::send(dc, rtc::error_frame("The iPad didn't send its key.")).await;
        return;
    }
    let name = clean_name(hello["name"].as_str().unwrap_or(""));
    let code = p.ticket.code(guest_fp);
    let (tx, rx) = oneshot::channel();
    *p.decide.lock().unwrap() = Some(tx);
    p.update(|v| {
        v.state = "asking".into();
        v.device = Some(name.clone());
        v.code = Some(code);
    });
    let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "waiting" }))).await;

    // The user's answer; the iPad may give up first.
    let mut stop = p.stop.subscribe();
    let yes = tokio::select! {
        yes = rx => yes.unwrap_or(false),
        _ = tokio::time::sleep(DECIDE_TIMEOUT) => false,
        _ = stop.changed() => false,
        _ = left(&mut from_ipad, &mut events) => {
            p.decide.lock().unwrap().take();
            p.update(|v| {
                v.state = "waiting".into();
                v.device = None;
                v.code = None;
            });
            return;
        }
    };
    if !yes {
        p.update(|v| v.state = "denied".into());
        let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "denied" }))).await;
        return;
    }

    p.update(|v| v.state = "pairing".into());
    let names: Vec<&String> = p.machines.iter().map(|(_, n)| n).collect();
    let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "pairing", "machines": names }))).await;
    let tries = p.machines.iter().map(|(id, name)| {
        let trust = (p.hooks.trust)(id.clone(), key.clone());
        async move { (id.clone(), name.clone(), trust.await) }
    });
    let (mut machines, mut failed, mut ids) = (Vec::new(), Vec::new(), Vec::new());
    for (id, name, result) in join_all(tries).await {
        match result {
            Ok(m) => {
                ids.push(id);
                machines.push(m);
            }
            Err(error) => failed.push(Failed { name, error }),
        }
    }
    if !ids.is_empty() {
        (p.hooks.paired)(name, key, ids);
    }
    let paired: Vec<String> = machines.iter().map(|m| m.name.clone()).collect();
    let _ = rtc::send(dc, rtc::json_frame(&json!({ "event": "paired", "machines": machines, "failed": failed }))).await;
    p.update(|v| {
        v.state = if paired.is_empty() { "failed" } else { "done" }.into();
        if paired.is_empty() {
            v.error = Some("None of the machines could pair with the iPad.".into());
        }
        v.paired = paired;
        v.failed = failed;
    });
    // Until the iPad says it has them (or goes).
    let _ = tokio::time::timeout(Duration::from_secs(15), left(&mut from_ipad, &mut events)).await;
}

/// The other side's JSON messages on channel 0, read in one place.
fn messages(dc: &Arc<dyn DataChannel>) -> mpsc::UnboundedReceiver<Value> {
    let (tx, rx) = mpsc::unbounded_channel();
    let dc = dc.clone();
    tokio::spawn(async move {
        let mut pieces = rtc::Pieces::default();
        while let Some(m) = rtc::next_message(&dc).await {
            let Some(frame) = pieces.add(m) else { continue };
            if let Some((0, super::JSON, body)) = super::parse_frame(&frame) {
                let v = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    rx
}

/// Resolves when the other side says it's done, or its connection goes.
async fn left(from: &mut mpsc::UnboundedReceiver<Value>, events: &mut mpsc::UnboundedReceiver<Event>) {
    loop {
        tokio::select! {
            m = from.recv() => match m {
                None => return,
                Some(v) if v["op"] == "done" || v["op"] == "bye" => return,
                Some(_) => {}
            },
            e = events.recv() => match e {
                None | Some(Event::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)) => return,
                Some(_) => {}
            },
        }
    }
}

fn clean_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(60).collect();
    let name = name.trim();
    if name.is_empty() { "iPad".into() } else { name.into() }
}

/// What the iPad hears while it pairs.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
pub enum Update {
    /// The code this iPad shows; the Mac shows the same.
    Code(String),
    /// Waiting for the user to allow it on the Mac.
    Waiting,
    /// Allowed: these machines are taking the iPad's key.
    Pairing(Vec<String>),
}

/// Pairs with the Mac behind `link`, as `name`, for this app's key: the
/// machines that trust it now, and those that couldn't. (The iPad's side;
/// muxinipad's core calls it.)
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
pub async fn join(link: &str, name: &str, updates: impl Fn(Update)) -> Result<(Vec<Machine>, Vec<Failed>), String> {
    let key = direct::app_fingerprint()?;
    let ticket = Ticket::parse(link)?;
    let server = signal::Server::new(&ticket.server);
    let room = ticket.room();
    let (open, ice) = server.room(&room).await.map_err(|e| e.to_string())?;
    if !open {
        return Err("That pairing link isn't open anymore. Make a new one on your Mac.".into());
    }
    let (pc, mut events) = rtc::peer(&ice, None).await?;
    let result = paired_with(&ticket, &server, &room, &ice, &pc, &mut events, &key, name, &updates).await;
    let _ = pc.close().await;
    result
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
async fn paired_with(
    ticket: &Ticket,
    server: &signal::Server,
    room: &str,
    ice: &Value,
    pc: &Arc<dyn PeerConnection>,
    events: &mut mpsc::UnboundedReceiver<Event>,
    key: &str,
    name: &str,
    updates: &impl Fn(Update),
) -> Result<(Vec<Machine>, Vec<Failed>), String> {
    const NO_MAC: &str = "Couldn't connect to your Mac. Check that Muxin is open there, then try again.";
    let dc = pc.create_data_channel(rtc::LABEL, None).await.map_err(|e| e.to_string())?;
    let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
    pc.set_local_description(offer).await.map_err(|e| e.to_string())?;
    rtc::gathered(pc, events, ice).await;
    let sdp = pc.local_description().await.ok_or("no offer")?.sdp;
    let my_fp = rtc::fingerprint(&sdp).ok_or("no certificate in the offer")?;
    let host_fp = hex(&ticket.fingerprint);
    let connecting = async {
        let id = server.offer(room, &sdp).await.map_err(|e| e.to_string())?;
        // The answer, once the Mac picks the offer up (it checks in every couple of seconds).
        let answer = loop {
            tokio::time::sleep(Duration::from_millis(700)).await;
            match server.answer_to(room, &id).await {
                Ok((Some(answer), _)) => break answer,
                Ok((None, true)) => {}
                Ok((None, false)) | Err(signal::Error::Gone(_)) => return Err("That pairing link isn't open anymore. Make a new one on your Mac.".to_string()),
                Err(e) => return Err(e.to_string()),
            }
        };
        // Only the Mac's own certificate: the server can't stand in.
        if rtc::fingerprint(&answer).as_deref() != Some(host_fp.as_str()) {
            return Err("The connection to your Mac couldn't be verified.".to_string());
        }
        pc.set_remote_description(RTCSessionDescription::answer(answer).map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
        if rtc::opened(&dc).await { Ok(()) } else { Err(NO_MAC.to_string()) }
    };
    tokio::time::timeout(Duration::from_secs(45), connecting).await.map_err(|_| NO_MAC.to_string())??;

    updates(Update::Code(ticket.code(&my_fp)));
    let hello = json!({ "op": "pair", "v": 1, "name": name, "fingerprint": key, "proof": ticket.proof(&host_fp, &my_fp) });
    rtc::send(&dc, rtc::json_frame(&hello)).await?;
    let mut from_mac = messages(&dc);
    let ended = "The connection to your Mac ended before it paired.";
    loop {
        let msg = tokio::select! {
            m = from_mac.recv() => m.ok_or(ended)?,
            e = events.recv() => match e {
                None | Some(Event::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)) => return Err(ended.into()),
                Some(_) => continue,
            },
        };
        match msg["event"].as_str().unwrap_or("") {
            "waiting" => updates(Update::Waiting),
            "pairing" => updates(Update::Pairing(strings(&msg["machines"]))),
            "paired" => {
                let machines: Vec<Machine> = serde_json::from_value(msg["machines"].clone()).map_err(|e| format!("Your Mac sent machines this app can't read: {e}"))?;
                let failed: Vec<Failed> = serde_json::from_value(msg["failed"].clone()).unwrap_or_default();
                let _ = rtc::send(&dc, rtc::json_frame(&json!({ "op": "done" }))).await;
                rtc::flush(&dc).await;
                rtc::forget(&dc);
                return Ok((machines, failed));
            }
            "denied" => return Err("Your Mac didn't allow this iPad.".into()),
            "error" => return Err(msg["error"].as_str().unwrap_or("Pairing didn't work.").to_string()),
            "ended" => return Err(msg["why"].as_str().unwrap_or(ended).to_string()),
            _ => {}
        }
    }
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// Adds paired machines to config.toml, and how to reach each one to
/// direct.json. One already there (by name) keeps its entry and id, so
/// pairing again only brings what's new. The machines' ids.
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
pub fn save(inventory: &Path, machines: &[Machine]) -> Result<Vec<String>, String> {
    let mut text = std::fs::read_to_string(inventory).unwrap_or_default();
    let have = crate::config::Inventory::parse(&text).map(|i| i.machines).unwrap_or_default();
    let mut ids = Vec::new();
    for m in machines {
        let id = match have.iter().find(|h| h.name.eq_ignore_ascii_case(&m.name)) {
            Some(h) => h.id.clone(),
            None => {
                let (updated, id) = crate::ops::inventory_edit::add_machine(&text, &m.name, &m.host)?;
                text = updated;
                id
            }
        };
        direct::save(&id, Some(m.direct.clone()))?;
        ids.push(id);
    }
    crate::config::write_atomic(inventory, text.as_bytes()).map_err(|e| format!("Couldn't save the machines: {e:#}"))?;
    Ok(ids)
}
