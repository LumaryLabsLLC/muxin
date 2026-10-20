//! Joining someone's share from Muxin: connect through the link's server,
//! check that the answer comes from the share's own certificate, prove the
//! link, then open what's shared on channels of our own, as on a machine's
//! link: a session, the desktop, a server (forwarded to a port here).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::{PeerConnection, RTCPeerConnectionState, RTCSessionDescription};

use super::host::{Desktop, Port};
use super::rtc::{self, Event};
use super::signal;
use super::ticket::{Ticket, hex};

/// What's shared, as its host describes it.
#[derive(Clone, Debug, Default)]
pub struct Info {
    /// A whole machine (its sessions, maybe its desktop and servers), not
    /// one session.
    pub whole_machine: bool,
    pub title: String,
    pub agent: String,
    pub machine: String,
    pub by: String,
    pub can_type: bool,
    pub desktop: Desktop,
    pub can_control: bool,
    /// May start, end and restart its sessions.
    pub can_manage: bool,
    /// The agent CLIs there, to start.
    pub agents: Vec<String>,
    pub ports: Vec<Port>,
    /// The machine is a Mac (its desktop's ⌘ stays ⌘).
    pub mac: bool,
    /// Unix seconds.
    pub ends_at: i64,
}

/// One of a shared machine's sessions.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub name: String,
    pub title: String,
    pub agent: String,
    /// "running" or "exited".
    pub status: String,
    /// Printed something in the last few seconds.
    pub busy: bool,
}

/// What happens, in order: the code to read out, what's shared, waiting,
/// let in (or not), then what changes while in. What's opened arrives on its
/// own channel (`Joined::open`).
#[derive(Debug)]
pub enum Update {
    Code(String),
    Info(Info),
    Waiting,
    Admitted { can_type: bool, can_control: bool, can_manage: bool },
    Denied,
    Typing(bool),
    Control(bool),
    /// Starting and ending sessions allowed, or not.
    Manage(bool),
    /// The desktop or the servers shared changed.
    Access { desktop: Desktop, ports: Vec<Port> },
    /// A shared machine's sessions, when they change.
    Sessions(Vec<Session>),
    /// The share ended or the connection dropped, and why.
    Ended(String),
}

/// A channel's frames: (kind, payload), then CLOSE.
pub type Frames = mpsc::UnboundedReceiver<(u8, Vec<u8>)>;

/// A server forwarded to a port on this computer.
struct Forward {
    port: u16,
    local: u16,
    task: tokio::task::JoinHandle<()>,
}

/// What a forwarded server sends waits while this much is on its way.
const QUEUED_MAX: usize = 8 << 20;

/// Frames to the host, in order, sent by one task. None is ever dropped
/// (a key, a mouse button let go); only a forwarded server's bytes wait while
/// too much is already queued.
#[derive(Clone)]
struct Out {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
}

impl Out {
    fn push(&self, frame: Vec<u8>) {
        self.queued.fetch_add(frame.len(), Ordering::Relaxed);
        let _ = self.tx.send(frame);
    }

    /// Like `push`, once there's room (false: the connection is gone).
    async fn push_paced(&self, frame: Vec<u8>) -> bool {
        while self.queued.load(Ordering::Relaxed) > QUEUED_MAX {
            if self.tx.is_closed() {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.push(frame);
        !self.tx.is_closed()
    }
}

/// A joined share: open channels on it, send on them, leave.
pub struct Joined {
    out: Out,
    pc: Arc<dyn PeerConnection>,
    channels: Mutex<HashMap<u32, mpsc::UnboundedSender<(u8, Vec<u8>)>>>,
    /// Requests waiting for the host's answer, by channel.
    replies: Mutex<HashMap<u32, oneshot::Sender<Result<Value, String>>>>,
    /// Calls (start, end, restart) waiting for theirs, by number.
    calls: Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>,
    next: AtomicU32,
    can_type: AtomicBool,
    can_control: AtomicBool,
    can_manage: AtomicBool,
    forwards: Mutex<Vec<Forward>>,
    /// One server opened here at a time (a double click opens it once).
    forwarding: tokio::sync::Mutex<()>,
    /// Where sending runs (keys can come from any thread, the window's included).
    runtime: tokio::runtime::Handle,
}

impl Joined {
    pub fn can_type(&self) -> bool {
        self.can_type.load(Ordering::Relaxed)
    }

    pub fn can_control(&self) -> bool {
        self.can_control.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn can_manage(&self) -> bool {
        self.can_manage.load(Ordering::Relaxed)
    }

    /// Asks the host to start a session (`{"op": "start", "agent"}`), or end
    /// or restart one (`{"op": "end" | "restart", "session"}`): the
    /// session's name, or why not.
    pub async fn call(&self, mut req: Value) -> Result<String, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) as u64;
        let (tx, answer) = oneshot::channel();
        self.calls.lock().unwrap().insert(id, tx);
        req["req"] = json!(id);
        self.out.push(rtc::json_frame(&req));
        let answer = tokio::time::timeout(Duration::from_secs(60), answer).await;
        self.calls.lock().unwrap().remove(&id);
        match answer {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("The share ended.".into()),
            Err(_) => Err("No answer from the computer sharing it.".into()),
        }
    }

    /// A shared machine's sessions now (they also come when they change).
    pub fn refresh_sessions(&self) {
        self.out.push(rtc::json_frame(&json!({ "op": "sessions" })));
    }

    /// Asks for something on a channel of its own: `{"op": "watch",
    /// "session"}`, `{"op": "desktop"}` or `{"op": "tcp", "port"}`. The
    /// host's answer, and the channel's frames.
    pub async fn open(&self, mut req: Value) -> Result<(u32, Value, Frames), String> {
        let channel = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, frames) = mpsc::unbounded_channel();
        let (reply_tx, reply) = oneshot::channel();
        self.channels.lock().unwrap().insert(channel, tx);
        self.replies.lock().unwrap().insert(channel, reply_tx);
        req["channel"] = json!(channel);
        self.out.push(rtc::json_frame(&req));
        // The host may first update the machine's agent, then wait up to 20s
        // for a desktop to start: give it that and more.
        let result = match tokio::time::timeout(Duration::from_secs(90), reply).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("The share ended.".into()),
            Err(_) => Err("No answer from the computer sharing it.".into()),
        };
        match result {
            Ok(reply) => Ok((channel, reply, frames)),
            Err(e) => {
                self.replies.lock().unwrap().remove(&channel);
                self.close(channel);
                Err(e)
            }
        }
    }

    /// A frame on one of our channels (the host drops what it doesn't allow:
    /// keys unless we may type, the mouse unless we may control).
    pub fn send(&self, channel: u32, kind: u8, payload: &[u8]) {
        self.out.push(super::frame(channel, kind, payload));
    }

    /// Keys for a session (dropped here too unless we may type).
    pub fn send_keys(&self, channel: u32, keys: &[u8]) {
        if self.can_type() {
            self.send(channel, super::DATA, keys);
        }
    }

    pub fn close(&self, channel: u32) {
        if self.channels.lock().unwrap().remove(&channel).is_some() {
            self.send(channel, super::CLOSE, &[]);
        }
    }

    /// Opens one of the shared servers on this computer: its port here (the
    /// same as there when it's free). Only this computer can reach it.
    pub async fn forward(self: &Arc<Self>, port: u16) -> Result<u16, String> {
        let _one_at_a_time = self.forwarding.lock().await;
        if let Some(f) = self.forwards.lock().unwrap().iter().find(|f| f.port == port) {
            return Ok(f.local);
        }
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| format!("Couldn't open a port here: {e}"))?,
        };
        let local = listener.local_addr().map_err(|e| e.to_string())?.port();
        let me = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Some(me) = me.upgrade() else { return };
                tokio::spawn(async move { me.serve_tcp(stream, port).await });
            }
        });
        self.forwards.lock().unwrap().push(Forward { port, local, task });
        Ok(local)
    }

    /// Where a shared server is open here, when it is.
    #[cfg(test)]
    pub fn forwarded(&self, port: u16) -> Option<u16> {
        self.forwards.lock().unwrap().iter().find(|f| f.port == port).map(|f| f.local)
    }

    /// One connection to a forwarded server, over a channel of its own.
    async fn serve_tcp(self: Arc<Self>, stream: tokio::net::TcpStream, port: u16) {
        let Ok((channel, _, mut frames)) = self.open(json!({ "op": "tcp", "port": port })).await else { return };
        let _ = stream.set_nodelay(true);
        let (mut reader, mut writer) = stream.into_split();
        let out = self.out.clone();
        // What the program here sends; the reply keeps coming after it's done.
        let up = tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !out.push_paced(super::frame(channel, super::DATA, &buf[..n])).await {
                            break;
                        }
                    }
                }
            }
        });
        while let Some((kind, bytes)) = frames.recv().await {
            if kind == super::CLOSE || (kind == super::DATA && writer.write_all(&bytes).await.is_err()) {
                break;
            }
        }
        up.abort();
        self.close(channel);
    }

    /// Stops forwarding servers no longer shared.
    fn keep_forwards(&self, ports: &[Port]) {
        self.forwards.lock().unwrap().retain(|f| {
            let keep = ports.iter().any(|p| p.port == f.port);
            if !keep {
                f.task.abort();
            }
            keep
        });
    }

    pub fn leave(&self) {
        let (out, pc) = (self.out.clone(), self.pc.clone());
        self.runtime.spawn(async move {
            out.push(rtc::json_frame(&json!({ "op": "bye" })));
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = pc.close().await;
        });
        self.keep_forwards(&[]);
    }
}

/// Joins the share behind `link` as `name`. Runs on the current Tokio
/// runtime; updates arrive until the share ends or `leave`.
pub async fn join(link: &str, name: &str) -> Result<(Arc<Joined>, mpsc::UnboundedReceiver<Update>), String> {
    let ticket = Ticket::parse(link)?;
    let server = signal::Server::new(&ticket.server);
    let room = ticket.room();
    let (open, ice) = server.room(&room).await.map_err(|e| e.to_string())?;
    if !open {
        return Err("That share isn't open right now. It may have ended.".into());
    }
    let (pc, mut events) = rtc::peer(&ice, None).await?;
    let dc = pc.create_data_channel(rtc::LABEL, None).await.map_err(|e| e.to_string())?;
    let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
    pc.set_local_description(offer).await.map_err(|e| e.to_string())?;
    rtc::gathered(&pc, &mut events, &ice).await;
    let sdp = pc.local_description().await.ok_or("no offer")?.sdp;
    let my_fp = rtc::fingerprint(&sdp).ok_or("no certificate in the offer")?;
    let host_fp = hex(&ticket.fingerprint);

    let connecting = async {
        let id = server.offer(&room, &sdp).await.map_err(|e| e.to_string())?;
        // The answer, once the sharing computer picks the offer up (it
        // checks in every couple of seconds).
        let answer = loop {
            tokio::time::sleep(Duration::from_millis(700)).await;
            match server.answer_to(&room, &id).await {
                Ok((Some(answer), _)) => break answer,
                Ok((None, true)) => {}
                Ok((None, false)) | Err(signal::Error::Gone(_)) => return Err("That share has ended.".to_string()),
                Err(e) => return Err(e.to_string()),
            }
        };
        // Only the share's own certificate: the server can't stand in.
        if rtc::fingerprint(&answer).as_deref() != Some(host_fp.as_str()) {
            return Err("That share's connection couldn't be verified.".to_string());
        }
        pc.set_remote_description(RTCSessionDescription::answer(answer).map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
        tokio::select! {
            open = rtc::opened(&dc) => if open { Ok(()) } else { Err("Couldn't connect to the computer sharing it.".to_string()) },
            failed = async {
                loop {
                    match events.recv().await {
                        Some(Event::State(RTCPeerConnectionState::Failed)) | None => return,
                        Some(_) => {}
                    }
                }
            } => {
                let () = failed;
                Err("Couldn't connect to the computer sharing it.".to_string())
            }
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(45), connecting).await.unwrap_or_else(|_| Err("Couldn't connect to the computer sharing it.".into()));
    if let Err(e) = result {
        let _ = pc.close().await;
        return Err(e);
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let _ = tx.send(Update::Code(ticket.code(&my_fp)));
    let hello = json!({ "op": "hello", "v": 1, "name": name, "client": "app", "proof": ticket.proof(&host_fp, &my_fp) });
    rtc::send(&dc, rtc::json_frame(&hello)).await?;
    let (to_host, mut outgoing) = mpsc::unbounded_channel::<Vec<u8>>();
    let out = Out { tx: to_host, queued: Arc::new(AtomicUsize::new(0)) };
    let (writer, queued) = (dc.clone(), out.queued.clone());
    tokio::spawn(async move {
        while let Some(f) = outgoing.recv().await {
            let n = f.len();
            let sent = rtc::send(&writer, f).await;
            queued.fetch_sub(n, Ordering::Relaxed);
            if sent.is_err() {
                break;
            }
        }
    });
    let joined = Arc::new(Joined {
        out,
        pc: pc.clone(),
        channels: Mutex::new(HashMap::new()),
        replies: Mutex::new(HashMap::new()),
        calls: Mutex::new(HashMap::new()),
        next: AtomicU32::new(1),
        can_type: AtomicBool::new(false),
        can_control: AtomicBool::new(false),
        can_manage: AtomicBool::new(false),
        forwards: Mutex::new(Vec::new()),
        forwarding: tokio::sync::Mutex::new(()),
        runtime: tokio::runtime::Handle::current(),
    });
    let j = joined.clone();
    tokio::spawn(async move {
        let why = relay(&dc, &tx, &j, &mut events).await;
        // Everything open ends with it.
        j.channels.lock().unwrap().clear();
        for (_, r) in j.replies.lock().unwrap().drain() {
            let _ = r.send(Err(why.clone()));
        }
        for (_, r) in j.calls.lock().unwrap().drain() {
            let _ = r.send(Err(why.clone()));
        }
        j.keep_forwards(&[]);
        let _ = tx.send(Update::Ended(why));
        rtc::forget(&dc);
        let _ = pc.close().await;
    });
    Ok((joined, rx))
}

/// The host's messages and the channels' frames, until it ends: why.
async fn relay(dc: &Arc<dyn DataChannel>, tx: &mpsc::UnboundedSender<Update>, joined: &Joined, events: &mut mpsc::UnboundedReceiver<Event>) -> String {
    let mut pieces = rtc::Pieces::default();
    loop {
        let msg = tokio::select! {
            m = rtc::next_message(dc) => m,
            e = events.recv() => match e {
                None | Some(Event::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)) => None,
                Some(_) => continue,
            },
        };
        let Some(msg) = msg else { return "The connection dropped.".into() };
        let Some(msg) = pieces.add(msg) else { continue };
        let Some((channel, kind, body)) = super::parse_frame(&msg) else { continue };
        if channel != 0 {
            let mut channels = joined.channels.lock().unwrap();
            if let Some(sink) = channels.get(&channel) {
                let _ = sink.send((kind, body.to_vec()));
                if kind == super::CLOSE {
                    channels.remove(&channel);
                }
            }
            continue;
        }
        if kind != super::JSON {
            continue;
        }
        let m: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let update = match m["event"].as_str().unwrap_or("") {
            "share" => {
                let info = Info {
                    whole_machine: m["kind"] == "machine",
                    title: text(&m["title"], 120),
                    agent: text(&m["agent"], 40),
                    machine: text(&m["machine"], 80),
                    by: text(&m["by"], 80),
                    can_type: m["can_type"].as_bool().unwrap_or(false),
                    desktop: desktop(&m["desktop"]),
                    can_control: m["can_control"].as_bool().unwrap_or(false),
                    can_manage: m["can_manage"].as_bool().unwrap_or(false),
                    agents: m["agents"].as_array().into_iter().flatten().take(16).map(|a| text(a, 20)).filter(|a| !a.is_empty()).collect(),
                    ports: ports(&m["ports"]),
                    mac: m["mac"].as_bool().unwrap_or(false),
                    ends_at: m["ends_at"].as_i64().unwrap_or(0),
                };
                Update::Info(info)
            }
            "waiting" => Update::Waiting,
            "admitted" => {
                let flag = |k: &str| m[k].as_bool().unwrap_or(false);
                let (can_type, can_control, can_manage) = (flag("can_type"), flag("can_control"), flag("can_manage"));
                joined.can_type.store(can_type, Ordering::Relaxed);
                joined.can_control.store(can_control, Ordering::Relaxed);
                joined.can_manage.store(can_manage, Ordering::Relaxed);
                Update::Admitted { can_type, can_control, can_manage }
            }
            "manage" => {
                let allowed = m["allowed"].as_bool().unwrap_or(false);
                joined.can_manage.store(allowed, Ordering::Relaxed);
                Update::Manage(allowed)
            }
            // Answers to `call`.
            "done" | "failed" => {
                let id = m["req"].as_u64().unwrap_or(0);
                if let Some(r) = joined.calls.lock().unwrap().remove(&id) {
                    let _ = r.send(if m["event"] == "done" { Ok(text(&m["session"], 120)) } else { Err(text(&m["error"], 200)) });
                }
                continue;
            }
            "typing" => {
                let allowed = m["allowed"].as_bool().unwrap_or(false);
                joined.can_type.store(allowed, Ordering::Relaxed);
                Update::Typing(allowed)
            }
            "control" => {
                let allowed = m["allowed"].as_bool().unwrap_or(false);
                joined.can_control.store(allowed, Ordering::Relaxed);
                Update::Control(allowed)
            }
            "access" => {
                let ports = ports(&m["ports"]);
                joined.keep_forwards(&ports);
                let can_control = m["can_control"].as_bool().unwrap_or(false);
                if joined.can_control.swap(can_control, Ordering::Relaxed) != can_control {
                    let _ = tx.send(Update::Control(can_control));
                }
                Update::Access { desktop: desktop(&m["desktop"]), ports }
            }
            "sessions" => {
                let list = m["sessions"].as_array().into_iter().flatten().take(200);
                Update::Sessions(
                    list.map(|s| Session {
                        name: text(&s["name"], 120),
                        title: text(&s["title"], 120),
                        agent: text(&s["agent"], 40),
                        status: text(&s["status"], 20),
                        busy: s["busy"].as_bool().unwrap_or(false),
                    })
                    .collect(),
                )
            }
            // Answers to `open`.
            "watching" | "desktop" | "tcp" | "refused" => {
                let channel = m["channel"].as_u64().unwrap_or(0) as u32;
                if let Some(r) = joined.replies.lock().unwrap().remove(&channel) {
                    let _ = r.send(if m["event"] == "refused" { Err(text(&m["error"], 200)) } else { Ok(m) });
                }
                continue;
            }
            "denied" => {
                let _ = tx.send(Update::Denied);
                return "They didn't let you in.".into();
            }
            "ended" => {
                return match m["why"].as_str().unwrap_or("") {
                    "expired" => "The share ended.",
                    "removed" => "You were removed from the share.",
                    _ => "They stopped sharing.",
                }
                .into();
            }
            "error" => return text(&m["error"], 200),
            _ => continue,
        };
        let _ = tx.send(update);
    }
}

fn desktop(v: &Value) -> Desktop {
    match v.as_str() {
        Some("view") => Desktop::View,
        Some("control") => Desktop::Control,
        _ => Desktop::None,
    }
}

fn ports(v: &Value) -> Vec<Port> {
    let list = v.as_array().into_iter().flatten().take(32);
    list.filter_map(|p| Some(Port { port: p["port"].as_u64().filter(|n| (1..=65535).contains(n))? as u16, name: text(&p["name"], 60), host: String::new() })).collect()
}

/// A string from the host, cut short and without control characters (it's
/// shown, so it can't carry terminal escapes or run on forever).
fn text(v: &Value, max: usize) -> String {
    v.as_str().unwrap_or("").chars().filter(|c| !c.is_control()).take(max).collect()
}
