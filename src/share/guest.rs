//! Joining someone's share from Muxin: connect through the link's server,
//! check that the answer comes from the share's own certificate, prove the
//! link, then watch (and type, when let) the session shared.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::{PeerConnection, RTCPeerConnectionState, RTCSessionDescription};

use super::rtc::{self, Event};
use super::signal;
use super::ticket::{Ticket, hex};

/// The channel the shared session arrives on.
const SESSION: u32 = 1;

/// What's shared, as its host describes it.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub title: String,
    pub agent: String,
    pub machine: String,
    pub by: String,
    pub can_type: bool,
    /// Unix seconds.
    pub ends_at: i64,
}

/// What happens, in order: the code to read out, what's shared, waiting,
/// let in (or not), then the session.
#[derive(Debug)]
pub enum Update {
    Code(String),
    Info(Info),
    Waiting,
    Admitted { can_type: bool },
    Denied,
    Typing(bool),
    Output(Vec<u8>),
    /// The session's size; draw at it.
    Size(u16, u16),
    /// The session ended (with its code).
    Exited(Option<i32>),
    /// The share ended or the connection dropped, and why.
    Ended(String),
}

/// A joined share: keys to send, and leaving.
pub struct Joined {
    dc: Arc<dyn DataChannel>,
    pc: Arc<dyn PeerConnection>,
    can_type: Arc<AtomicBool>,
    /// Where sending runs (keys can come from any thread, the window's included).
    runtime: tokio::runtime::Handle,
}

impl Joined {
    pub fn can_type(&self) -> bool {
        self.can_type.load(Ordering::Relaxed)
    }

    /// Keys for the session (dropped unless the host lets this guest type;
    /// the host drops them too).
    pub fn send_keys(&self, keys: &[u8]) {
        if !self.can_type() {
            return;
        }
        let dc = self.dc.clone();
        let frame = super::frame(SESSION, super::DATA, keys);
        self.runtime.spawn(async move {
            let _ = rtc::send(&dc, frame).await;
        });
    }

    pub fn leave(&self) {
        let (dc, pc) = (self.dc.clone(), self.pc.clone());
        self.runtime.spawn(async move {
            let _ = rtc::send(&dc, rtc::json_frame(&json!({ "op": "bye" }))).await;
            let _ = pc.close().await;
        });
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
    let joined = Arc::new(Joined { dc: dc.clone(), pc: pc.clone(), can_type: Arc::new(AtomicBool::new(false)), runtime: tokio::runtime::Handle::current() });
    let can_type = joined.can_type.clone();
    tokio::spawn(async move {
        let why = relay(&dc, &tx, &can_type, &mut events).await;
        let _ = tx.send(Update::Ended(why));
        let _ = pc.close().await;
    });
    Ok((joined, rx))
}

/// The host's messages and the session, until it ends: why.
async fn relay(dc: &Arc<dyn DataChannel>, tx: &mpsc::UnboundedSender<Update>, can_type: &AtomicBool, events: &mut mpsc::UnboundedReceiver<Event>) -> String {
    loop {
        let msg = tokio::select! {
            m = rtc::next_message(dc) => m,
            e = events.recv() => match e {
                None | Some(Event::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)) => None,
                Some(_) => continue,
            },
        };
        let Some(msg) = msg else { return "The connection dropped.".into() };
        let Some((channel, kind, body)) = super::parse_frame(&msg) else { continue };
        if channel == SESSION {
            let _ = match kind {
                super::DATA => tx.send(Update::Output(body.to_vec())),
                super::SIZE if body.len() == 4 => tx.send(Update::Size(u16::from_be_bytes([body[0], body[1]]), u16::from_be_bytes([body[2], body[3]]))),
                super::EXIT => tx.send(Update::Exited((body.len() == 4).then(|| i32::from_be_bytes([body[0], body[1], body[2], body[3]])))),
                super::CLOSE => return "The session ended.".into(),
                _ => Ok(()),
            };
            continue;
        }
        if channel != 0 || kind != super::JSON {
            continue;
        }
        let m: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        match m["event"].as_str().unwrap_or("") {
            "share" => {
                let info = Info {
                    title: text(&m["title"], 120),
                    agent: text(&m["agent"], 40),
                    machine: text(&m["machine"], 80),
                    by: text(&m["by"], 80),
                    can_type: m["can_type"].as_bool().unwrap_or(false),
                    ends_at: m["ends_at"].as_i64().unwrap_or(0),
                };
                let _ = tx.send(Update::Info(info));
            }
            "waiting" => {
                let _ = tx.send(Update::Waiting);
            }
            "admitted" => {
                let allowed = m["can_type"].as_bool().unwrap_or(false);
                can_type.store(allowed, Ordering::Relaxed);
                let _ = tx.send(Update::Admitted { can_type: allowed });
                let _ = rtc::send(dc, rtc::json_frame(&json!({ "op": "watch", "channel": SESSION }))).await;
            }
            "typing" => {
                let allowed = m["allowed"].as_bool().unwrap_or(false);
                can_type.store(allowed, Ordering::Relaxed);
                let _ = tx.send(Update::Typing(allowed));
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
            _ => {}
        }
    }
}

/// A string from the host, cut short and without control characters (it's
/// shown, so it can't carry terminal escapes or run on forever).
fn text(v: &Value, max: usize) -> String {
    v.as_str().unwrap_or("").chars().filter(|c| !c.is_control()).take(max).collect()
}
