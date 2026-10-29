//! Alerts for Muxin for iPad. When an agent session here goes quiet after
//! working (it's waiting for you), rings the bell, or exits with an error,
//! `listen` tells each iPad that asked for it, even while that iPad's app is
//! in the background: the alert goes to a relay (muxin.dev), which hands it to
//! Apple's push service. It's encrypted with a key only that iPad has, so the
//! relay can't read it.
//!
//! An iPad asks with `{"op": "push", "set": {...}}` over its direct link. Only
//! a paired app can: its entry is kept by its certificate's fingerprint, in
//! ~/.muxen/direct/push.json, and goes once that app is unpaired.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};

use crate::{home_dir, now, proto, sys};

/// What an iPad can ask to hear about.
const KINDS: [&str; 3] = ["waiting", "bell", "exited"];
/// Output, then quiet this long: the agent worked (or asked something), and
/// now waits for you.
const QUIET: i64 = 6;
/// What's printed this soon after someone typed or resized is an echo or a
/// redraw, not work.
const ECHO: i64 = 2;
/// At most one alert per session this often, and `PER_HOUR` in all.
const PER_SESSION: i64 = 30;
const PER_HOUR: usize = 30;
const TIMEOUT: Duration = Duration::from_secs(10);

/// One iPad's alerts: where to send them, and the key to seal them with.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    relay: String,
    to: String,
    key: [u8; 32],
    /// This machine's id and name on that iPad, for the alert's words and the
    /// session it opens.
    machine_id: String,
    machine: String,
    kinds: Vec<String>,
}

impl Entry {
    fn parse(v: &Value) -> Result<Entry, String> {
        let text = |k: &str, max: usize| v[k].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= max).map(String::from);
        let relay = text("relay", 200).ok_or("alerts need the relay's address")?;
        if !relay_allowed(&relay) {
            return Err(format!("alerts go only to an https address, not {relay}"));
        }
        let to = text("to", 2000).ok_or("alerts need the iPad's id at the relay")?;
        let key: [u8; 32] = v["key"]
            .as_str()
            .and_then(|k| base64::engine::general_purpose::STANDARD.decode(k).ok())
            .and_then(|k| k.try_into().ok())
            .ok_or("alerts need a 32-byte key")?;
        let machine_id = text("machine_id", 200).ok_or("alerts need this machine's id on the iPad")?;
        let machine = text("machine", 200).unwrap_or_else(|| machine_id.clone());
        let kinds = match v["kinds"].as_array() {
            Some(a) => a.iter().filter_map(Value::as_str).filter(|k| KINDS.contains(k)).map(String::from).collect(),
            None => KINDS.iter().map(|k| k.to_string()).collect(),
        };
        Ok(Entry { relay: relay.trim_end_matches('/').to_string(), to, key, machine_id, machine, kinds })
    }

    fn to_json(&self) -> Value {
        json!({
            "relay": self.relay,
            "to": self.to,
            "key": base64::engine::general_purpose::STANDARD.encode(self.key),
            "machine_id": self.machine_id,
            "machine": self.machine,
            "kinds": self.kinds,
        })
    }
}

/// https anywhere; plain http only to this machine (a relay for tests).
fn relay_allowed(relay: &str) -> bool {
    match split_url(relay) {
        Some((true, ..)) => true,
        Some((false, host, ..)) => matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"),
        None => false,
    }
}

// MARK: The iPads that asked

fn path() -> PathBuf {
    home_dir().join("direct").join("push.json")
}

/// The entries, as `listen` keeps them (loaded once, saved on each change).
fn apps() -> &'static Mutex<HashMap<String, Entry>> {
    static APPS: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    APPS.get_or_init(|| Mutex::new(load()))
}

fn load() -> HashMap<String, Entry> {
    let text = std::fs::read_to_string(path()).unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    v.as_object().into_iter().flatten().filter_map(|(app, e)| Entry::parse(e).ok().map(|e| (app.clone(), e))).collect()
}

fn save(apps: &HashMap<String, Entry>) {
    if apps.is_empty() {
        let _ = std::fs::remove_file(path());
        return;
    }
    let all: serde_json::Map<String, Value> = apps.iter().map(|(app, e)| (app.clone(), e.to_json())).collect();
    if let Err(e) = crate::direct::write_private(&path(), Value::Object(all).to_string().as_bytes()) {
        eprintln!("muxen-agent: couldn't save alerts: {e}");
    }
}

/// Drops the apps that aren't paired anymore. True when any were.
fn prune(apps: &mut HashMap<String, Entry>, trusted: &HashSet<String>) -> bool {
    let before = apps.len();
    apps.retain(|app, _| trusted.contains(app));
    apps.len() != before
}

/// `unpair` (which can run in another process): forgets that app's alerts,
/// or everyone's for "".
pub fn forget(client: &str) {
    let client = client.trim().to_lowercase();
    let mut apps = load();
    if client.is_empty() {
        apps.clear();
    } else {
        apps.remove(&client);
    }
    save(&apps);
}

/// `{"op": "push", ...}` from the app at the other end of a direct link:
/// `"set": {relay, to, key, machine_id, machine, kinds}` turns its alerts on,
/// `"clear": true` off, `"test": true` sends one now. Says what's set.
pub fn op(req: &Value, app: Option<&str>) -> Value {
    let Some(app) = app.map(str::to_lowercase) else {
        return json!({ "ok": false, "error": "Only a paired iPad can set alerts, over its direct connection." });
    };
    if req["clear"].as_bool() == Some(true) {
        let mut apps = apps().lock().unwrap();
        if apps.remove(&app).is_some() {
            save(&apps);
        }
    } else if let Some(set) = req.get("set").filter(|v| v.is_object()) {
        let entry = match Entry::parse(set) {
            Ok(e) => e,
            Err(e) => return json!({ "ok": false, "error": e }),
        };
        let mut apps = apps().lock().unwrap();
        apps.insert(app.clone(), entry);
        save(&apps);
    }
    let entry = apps().lock().unwrap().get(&app).cloned();
    if req["test"].as_bool() == Some(true) {
        let Some(e) = entry.as_ref() else {
            return json!({ "ok": false, "error": "Alerts aren't on for this iPad here." });
        };
        let alert = json!({ "kind": "test", "machine_id": e.machine_id, "machine": e.machine, "at": now() });
        if let Err(error) = send(&app, e, &alert, "test") {
            return json!({ "ok": false, "error": error });
        }
    }
    json!({
        "ok": true,
        "set": entry.is_some(),
        "kinds": entry.as_ref().map(|e| e.kinds.clone()),
        "relay": entry.as_ref().map(|e| e.relay.clone()),
    })
}

// MARK: Watching the sessions

/// What a session did, as far as alerts go.
#[derive(Debug, Default)]
struct Seen {
    last_output: i64,
    clients: u64,
    /// Output until then is the redraw for someone who just attached.
    redraw_until: i64,
    /// When it last printed something nobody typed or attached for (it's
    /// working, or asking): an alert once it's quiet.
    work: Option<i64>,
    /// What its screen showed last (see `signature`), for an agent.
    screen: Option<u64>,
    /// It started after the first look, so its first screen is news too.
    new: bool,
}

#[derive(Debug, PartialEq)]
struct Alert {
    kind: &'static str,
    session: String,
    agent: String,
    title: String,
    code: Option<i64>,
}

/// Turns the session server's lists, one a second, into alerts, by the Mac
/// app's rule (`new_notices`): an agent that worked, then went quiet, waits
/// for you; a bell rang; an agent exited with an error.
#[derive(Default)]
struct Watch {
    sessions: HashMap<String, Seen>,
    /// The newest event seen, and those seen at about that time; `None`
    /// until the first list, which only sets the baseline.
    mark: Option<i64>,
    events: HashSet<String>,
    sent: VecDeque<i64>,
    last_sent: HashMap<String, i64>,
}

impl Watch {
    /// One look at the sessions. `screen` says what an agent session shows
    /// (`signature`), asked for only when it printed: what counts is its
    /// screen changing, so an idle agent whose screen only animates (Codex)
    /// is quiet. `None` (it couldn't say): the output alone counts.
    fn observe(&mut self, list: &Value, screen: impl Fn(&str) -> Option<u64>) -> Vec<Alert> {
        let now = list["now"].as_i64().unwrap_or_else(now);
        let sessions = list["sessions"].as_array().cloned().unwrap_or_default();
        let find = |name: &str| sessions.iter().find(|s| s["name"].as_str() == Some(name));
        let first = self.mark.is_none();
        let mut mark = self.mark.unwrap_or(0);
        let mut out = Vec::new();
        for e in list["events"].as_array().into_iter().flatten() {
            let at = e["at"].as_i64().unwrap_or(0);
            let (kind, session, detail) = (e["kind"].as_str().unwrap_or(""), e["session"].as_str().unwrap_or(""), e["detail"].as_str().unwrap_or(""));
            let id = format!("{at}|{kind}|{session}|{detail}");
            if at + 10 < mark || !self.events.insert(id) || first {
                mark = mark.max(at);
                continue;
            }
            mark = mark.max(at);
            let s = find(session);
            let (agent, title) = (s.and_then(|s| s["agent"].as_str()).unwrap_or(""), s.and_then(|s| s["title"].as_str()).unwrap_or(session));
            match kind {
                "bell" => out.push(Alert { kind: "bell", session: session.into(), agent: agent.into(), title: title.into(), code: None }),
                "exited" if !matches!(detail, "" | "0") => {
                    out.push(Alert { kind: "exited", session: session.into(), agent: agent.into(), title: title.into(), code: detail.parse().ok() })
                }
                _ => {}
            }
        }
        self.mark = Some(mark);
        self.events.retain(|id| id.split('|').next().and_then(|at| at.parse::<i64>().ok()).is_some_and(|at| at + 10 >= mark));

        for s in &sessions {
            let name = s["name"].as_str().unwrap_or("");
            let agent = s["agent"].as_str().unwrap_or("");
            let (last_output, last_input) = (s["last_output"].as_i64().unwrap_or(0), s["last_input"].as_i64().unwrap_or(0));
            let clients = s["clients"].as_u64().unwrap_or(0);
            let running = s["status"].as_str() == Some("running");
            let is_agent = !agent.is_empty() && agent != "shell";
            // One that started since the first look: what it printed so far counts.
            let seen = self.sessions.entry(name.to_string()).or_insert_with(|| Seen { last_output: if first { last_output } else { 0 }, clients, new: !first, ..Seen::default() });
            if clients != seen.clients {
                seen.clients = clients;
                seen.redraw_until = now + ECHO + 1;
            }
            let mut printed = last_output > seen.last_output;
            if printed && is_agent && let Some(shown) = screen(name) {
                // The first screen of one that was there already is only the baseline.
                printed = seen.screen.map_or(seen.new, |was| was != shown);
                seen.screen = Some(shown);
            }
            if printed && last_output > seen.redraw_until && last_output > last_input + ECHO {
                seen.work = Some(last_output);
            }
            seen.last_output = last_output;
            let Some(end) = seen.work else { continue };
            // Someone typed since: they're there already.
            if !running || last_input > end {
                seen.work = None;
            } else if now - end >= QUIET {
                seen.work = None;
                if is_agent {
                    let title = s["title"].as_str().unwrap_or(name);
                    out.push(Alert { kind: "waiting", session: name.into(), agent: agent.into(), title: title.into(), code: None });
                }
            }
        }
        self.sessions.retain(|name, _| find(name).is_some());
        if first { Vec::new() } else { out }
    }

    /// Whether an alert may go now: not too soon after the last for its
    /// session, and not too many an hour.
    fn allow(&mut self, alert: &Alert, now: i64) -> bool {
        while self.sent.front().is_some_and(|t| now - t >= 3600) {
            self.sent.pop_front();
        }
        if self.sent.len() >= PER_HOUR || self.last_sent.get(&alert.session).is_some_and(|t| now - t < PER_SESSION) {
            return false;
        }
        self.sent.push_back(now);
        self.last_sent.insert(alert.session.clone(), now);
        self.last_sent.retain(|_, t| now - *t < PER_SESSION);
        true
    }
}

/// `listen`'s alerts: looks at the sessions every second while any iPad
/// wants alerts, and sends what it finds.
pub fn start() {
    thread::spawn(|| {
        let mut watch = Watch::default();
        let mut pruned = Instant::now();
        loop {
            thread::sleep(Duration::from_secs(1));
            if pruned.elapsed() >= Duration::from_secs(5) {
                pruned = Instant::now();
                let mut apps = apps().lock().unwrap();
                if prune(&mut apps, &crate::direct::trusted()) {
                    save(&apps);
                }
            }
            let wanted = apps().lock().unwrap().clone();
            if wanted.is_empty() {
                watch = Watch::default();
                continue;
            }
            // No session server: no sessions. Don't start one just to look.
            let Ok(mut server) = sys::connect() else { continue };
            let Ok(list) = proto::write_line(&mut server, &json!({ "op": "list" })).and_then(|_| proto::read_line(&mut server)) else { continue };
            let alerts = watch.observe(&list, screen);
            if alerts.is_empty() {
                continue;
            }
            let titles = crate::client::read_titles();
            for alert in alerts {
                if !watch.allow(&alert, now()) {
                    continue;
                }
                let title = titles.get(&alert.session).and_then(Value::as_str).unwrap_or(&alert.title).to_string();
                for (app, e) in wanted.iter().filter(|(_, e)| e.kinds.iter().any(|k| k == alert.kind)) {
                    let payload = json!({
                        "kind": alert.kind,
                        "agent": alert.agent,
                        "title": title,
                        "session": alert.session,
                        "machine_id": e.machine_id,
                        "machine": e.machine,
                        "code": alert.code,
                        "at": now(),
                    });
                    let (app, e, collapse) = (app.clone(), e.clone(), alert.session.clone());
                    thread::spawn(move || {
                        if let Err(error) = send(&app, &e, &payload, &collapse) {
                            eprintln!("muxen-agent: an alert to {} didn't go: {error}", e.machine_id);
                        }
                    });
                }
            }
        }
    });
}

/// What an agent session's screen shows, from the session server, as a
/// signature: equal when only animation changed.
fn screen(name: &str) -> Option<u64> {
    let mut server = sys::connect().ok()?;
    proto::write_line(&mut server, &json!({ "op": "read", "name": name, "lines": 80 })).ok()?;
    let reply = proto::read_line(&mut server).ok()?;
    reply["text"].as_str().map(signature)
}

/// The text without what animates while an agent waits (Braille dots: Codex's
/// idle screen, spinners), and with each run of spaces as one, so dots moving
/// between words change nothing.
fn signature(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for line in text.lines() {
        let kept: String = line.chars().map(|c| if ('\u{2800}'..='\u{28ff}').contains(&c) { ' ' } else { c }).collect();
        kept.split_whitespace().collect::<Vec<_>>().join(" ").hash(&mut h);
    }
    h.finish()
}

// MARK: Sending

/// Seals an alert for one iPad and hands it to its relay. The relay saying
/// the iPad is gone (it was reset, or the app removed) drops its entry.
fn send(app: &str, e: &Entry, alert: &Value, collapse: &str) -> Result<(), String> {
    let sealed = seal(&e.key, alert.to_string().as_bytes())?;
    let body = json!({
        "to": e.to,
        "alert": base64::engine::general_purpose::STANDARD.encode(sealed),
        "collapse": collapse.chars().take(64).collect::<String>(),
    });
    let (status, reply) = post(&format!("{}/push/v1/send", e.relay), &body)?;
    let said = || serde_json::from_str::<Value>(reply.get(reply.find('{').unwrap_or(0)..reply.rfind('}').map_or(0, |i| i + 1)).unwrap_or("")).ok().and_then(|v| v["error"].as_str().map(String::from));
    match status {
        200..=299 => Ok(()),
        410 => {
            let mut apps = apps().lock().unwrap();
            if apps.get(app) == Some(e) {
                apps.remove(app);
                save(&apps);
            }
            Err("the relay says this iPad is gone".into())
        }
        code => Err(said().unwrap_or_else(|| format!("the relay answered {code}"))),
    }
}

/// ChaCha20-Poly1305 with a random nonce: the nonce, the ciphertext, then the
/// tag (what CryptoKit's `ChaChaPoly.SealedBox(combined:)` reads).
fn seal(key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>, String> {
    let mut nonce = [0u8; 12];
    SystemRandom::new().fill(&mut nonce).map_err(|_| "no random numbers")?;
    seal_with(key, nonce, plain)
}

fn seal_with(key: &[u8; 32], nonce: [u8; 12], plain: &[u8]) -> Result<Vec<u8>, String> {
    let key = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).map_err(|_| "bad key")?);
    let mut data = plain.to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data).map_err(|_| "couldn't seal the alert")?;
    let mut out = nonce.to_vec();
    out.extend(data);
    Ok(out)
}

/// `(https, host, port, path)` of `http(s)://host[:port][/path]`.
fn split_url(url: &str) -> Option<(bool, String, u16, String)> {
    let (https, rest) = match url.split_once("://")? {
        ("https", rest) => (true, rest),
        ("http", rest) => (false, rest),
        _ => return None,
    };
    let (authority, path) = rest.split_once('/').map_or((rest, String::from("/")), |(a, p)| (a, format!("/{p}")));
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']')?;
            (host.to_string(), after.strip_prefix(':').map(|p| p.parse().ok()).unwrap_or(Some(if https { 443 } else { 80 }))?)
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), if https { 443 } else { 80 }),
        },
    };
    (!host.is_empty()).then_some((https, host, port, path))
}

/// An HTTP/1.1 POST of JSON; the status and what came back.
fn post(url: &str, body: &Value) -> Result<(u16, String), String> {
    let (https, host, port, path) = split_url(url).ok_or_else(|| format!("{url} isn't an address"))?;
    let addr = (host.as_str(), port).to_socket_addrs().map_err(|e| format!("can't find {host}: {e}"))?.next().ok_or_else(|| format!("can't find {host}"))?;
    let tcp = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("can't reach {host}: {e}"))?;
    let _ = tcp.set_read_timeout(Some(TIMEOUT));
    let _ = tcp.set_write_timeout(Some(TIMEOUT));
    let body = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: muxen-agent/{}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        crate::VERSION,
        body.len()
    );
    let mut reply = Vec::new();
    if https {
        let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|_| format!("{host} isn't a host name"))?;
        let conn = rustls::ClientConnection::new(tls(), name).map_err(|e| e.to_string())?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        stream.write_all(request.as_bytes()).map_err(|e| format!("couldn't send to {host}: {e}"))?;
        read_reply(&mut stream, &mut reply);
    } else {
        let mut stream = tcp;
        stream.write_all(request.as_bytes()).map_err(|e| format!("couldn't send to {host}: {e}"))?;
        read_reply(&mut stream, &mut reply);
    }
    let text = String::from_utf8_lossy(&reply).to_string();
    let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| format!("{host} didn't answer"))?;
    Ok((status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default()))
}

/// Reads until the server closes (or stops early, which only cuts the body short).
fn read_reply(stream: &mut impl Read, into: &mut Vec<u8>) {
    let mut buf = [0u8; 8192];
    while into.len() < 64 << 10 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => into.extend_from_slice(&buf[..n]),
        }
    }
}

/// The certificates the web trusts (Mozilla's list, built in, so it's the
/// same on every machine).
fn tls() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("the ring provider does TLS 1.2 and 1.3")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Watch {
        /// A look without screens: the output alone counts.
        fn observe_plain(&mut self, list: &Value) -> Vec<Alert> {
            self.observe(list, |_| None)
        }
    }

    fn list(now: i64, sessions: Value, events: Value) -> Value {
        json!({ "now": now, "sessions": sessions, "events": events })
    }

    fn agent(last_output: i64, last_input: i64, clients: u64) -> Value {
        json!([{ "name": "claude-1", "agent": "claude", "title": "Checkout fix", "status": "running", "last_output": last_output, "last_input": last_input, "clients": clients }])
    }

    /// Feeds one list a second, from `from` to `to`, with output until `busy_until`.
    fn run(w: &mut Watch, from: i64, to: i64, busy_from: i64, busy_until: i64, input: i64, clients: u64) -> Vec<Alert> {
        let mut all = Vec::new();
        for t in from..=to {
            let out = if t >= busy_from { t.min(busy_until) } else { 0 };
            all.extend(w.observe_plain(&list(t, agent(out, input, clients), json!([]))));
        }
        all
    }

    #[test]
    fn an_agent_that_worked_then_went_quiet_is_waiting() {
        let mut w = Watch::default();
        let alerts = run(&mut w, 100, 120, 101, 108, 0, 0);
        assert_eq!(alerts.len(), 1);
        assert_eq!((alerts[0].kind, alerts[0].title.as_str(), alerts[0].agent.as_str()), ("waiting", "Checkout fix", "claude"));
        // It stays quiet: no more alerts.
        assert!(run(&mut w, 121, 140, 0, 0, 0, 0).is_empty());
    }

    #[test]
    fn a_question_right_away_is_waiting_too() {
        // A second of output (a question as it starts), then quiet.
        let alerts = run(&mut Watch::default(), 100, 120, 101, 102, 0, 0);
        assert_eq!(alerts.iter().map(|a| a.kind).collect::<Vec<_>>(), vec!["waiting"]);
    }

    #[test]
    fn typing_or_a_redraw_isnt_work() {
        // Typing: every bit of output follows a key.
        let mut w = Watch::default();
        let mut alerts = Vec::new();
        for t in 100..=120 {
            let out = if t <= 110 { t } else { 110 };
            let input = if t <= 110 { t } else { 110 };
            alerts.extend(w.observe_plain(&list(t, agent(out, input, 1), json!([]))));
        }
        assert!(alerts.is_empty());
        // Someone attaching: the redraw right after.
        let mut w = Watch::default();
        w.observe_plain(&list(100, agent(90, 0, 0), json!([])));
        let mut alerts = w.observe_plain(&list(101, agent(101, 0, 1), json!([])));
        alerts.extend(w.observe_plain(&list(102, agent(102, 0, 1), json!([]))));
        alerts.extend(run(&mut w, 103, 115, 0, 0, 0, 1));
        assert!(alerts.is_empty(), "{alerts:?}");
    }

    #[test]
    fn an_idle_screen_that_only_animates_is_quiet() {
        // Working: the screen changes every second until 110; after, only its dots move.
        let shows = |t: i64| if t <= 110 { format!("Working ({t}s)") } else { format!("› Ask Codex {}", ['⠁', '⠂', '⠄'][t as usize % 3]) };
        let mut w = Watch::default();
        let mut alerts = Vec::new();
        for t in 100..=125 {
            let sig = signature(&shows(t));
            alerts.extend(w.observe(&list(t, agent(t, 0, 0), json!([])), |_| Some(sig)));
        }
        assert_eq!(alerts.iter().map(|a| a.kind).collect::<Vec<_>>(), vec!["waiting"]);
        // One that was idle and animating before the first look stays quiet.
        let mut w = Watch::default();
        let mut alerts = Vec::new();
        for t in 200..=230 {
            let sig = signature(&shows(111 + t % 3));
            alerts.extend(w.observe(&list(t, agent(t, 0, 0), json!([])), |_| Some(sig)));
        }
        assert!(alerts.is_empty(), "{alerts:?}");
    }

    #[test]
    fn moving_dots_dont_change_the_screen() {
        assert_eq!(signature("› Ask Codex⡀  ⠈ to do  anything ⠂"), signature("› Ask Codex ⠁⠂  to do⠄anything"));
        assert_ne!(signature("Working (3s)"), signature("Working (4s)"));
    }

    #[test]
    fn a_new_session_that_asks_right_away_is_waiting() {
        let mut w = Watch::default();
        w.observe_plain(&list(100, json!([]), json!([])));
        // It started and asked within the second between two looks, then sat still.
        let sig = signature("Do you trust the contents of this directory?");
        let mut alerts = Vec::new();
        for t in 101..=110 {
            alerts.extend(w.observe(&list(t, agent(100, 0, 0), json!([])), |_| Some(sig)));
        }
        assert_eq!(alerts.iter().map(|a| a.kind).collect::<Vec<_>>(), vec!["waiting"]);
    }

    #[test]
    fn typing_after_the_work_means_someone_is_there() {
        let mut w = Watch::default();
        let mut alerts = run(&mut w, 100, 110, 101, 110, 0, 1);
        // The work ends at 110; they type at 112.
        for t in 111..=125 {
            alerts.extend(w.observe_plain(&list(t, agent(110, if t >= 112 { 112 } else { 0 }, 1), json!([]))));
        }
        assert!(alerts.is_empty(), "{alerts:?}");
    }

    #[test]
    fn shells_never_wait_but_bells_and_errors_tell() {
        let mut w = Watch::default();
        let shell = |out: i64| json!([{ "name": "shell-1", "agent": "shell", "title": "Shell", "status": "running", "last_output": out, "clients": 0 }]);
        let mut alerts = Vec::new();
        for t in 100..=120 {
            alerts.extend(w.observe_plain(&list(t, shell(t.min(110)), json!([]))));
        }
        assert!(alerts.is_empty());
        let events = json!([{ "at": 121, "kind": "bell", "session": "claude-1", "detail": "" }, { "at": 121, "kind": "exited", "session": "claude-1", "detail": "2" }, { "at": 121, "kind": "closed", "session": "shell-1", "detail": "0" }]);
        let alerts = w.observe_plain(&list(121, agent(0, 0, 0), events.clone()));
        assert_eq!(alerts.iter().map(|a| (a.kind, a.code)).collect::<Vec<_>>(), vec![("bell", None), ("exited", Some(2))]);
        // The same events again aren't new.
        assert!(w.observe_plain(&list(122, agent(0, 0, 0), events)).is_empty());
    }

    #[test]
    fn the_first_look_only_sets_the_baseline() {
        let mut w = Watch::default();
        let events = json!([{ "at": 99, "kind": "bell", "session": "claude-1", "detail": "" }]);
        assert!(w.observe_plain(&list(100, agent(99, 0, 0), events.clone())).is_empty());
        assert!(w.observe_plain(&list(101, agent(99, 0, 0), events)).is_empty());
    }

    #[test]
    fn alerts_are_rationed() {
        let mut w = Watch::default();
        let alert = |s: &str| Alert { kind: "bell", session: s.into(), agent: String::new(), title: String::new(), code: None };
        assert!(w.allow(&alert("a"), 1000));
        assert!(!w.allow(&alert("a"), 1010));
        assert!(w.allow(&alert("a"), 1031));
        for i in 0..PER_HOUR {
            w.allow(&alert(&format!("s{i}")), 1040);
        }
        assert!(!w.allow(&alert("new"), 1041));
        assert!(w.allow(&alert("new"), 1000 + 3600 + 41));
    }

    #[test]
    fn a_sealed_alert_opens_with_its_key_only() {
        let key = [7u8; 32];
        let sealed = seal(&key, b"{\"kind\":\"test\"}").unwrap();
        assert_eq!(sealed.len(), 12 + 15 + 16);
        let open = |key: &[u8; 32], sealed: &[u8]| {
            let k = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).unwrap());
            let mut data = sealed[12..].to_vec();
            k.open_in_place(Nonce::try_assume_unique_for_key(&sealed[..12]).unwrap(), Aad::empty(), &mut data).map(|p| p.to_vec())
        };
        assert_eq!(open(&key, &sealed).unwrap(), b"{\"kind\":\"test\"}");
        assert!(open(&[8u8; 32], &sealed).is_err());
        // The vector the iPad's test opens (MuxinFeatureTests).
        let fixed = seal_with(&[1u8; 32], [2u8; 12], b"{\"kind\":\"test\",\"machine\":\"Forge\"}").unwrap();
        assert_eq!(base64::engine::general_purpose::STANDARD.encode(&fixed), FIXED_VECTOR);
    }

    /// `seal_with([1; 32], [2; 12], {"kind":"test","machine":"Forge"})`.
    const FIXED_VECTOR: &str = "AgICAgICAgICAgICnT6ZR+PHyuBTWYyKp6nIC+HSD2XqxBjxLqZd0ofxIf6ulCLyUDn55TxAVl8zOKYf+Q==";

    #[test]
    fn entries_take_only_what_they_should() {
        let key = base64::engine::general_purpose::STANDARD.encode([3u8; 32]);
        let good = json!({ "relay": "https://muxin.dev/", "to": "abc", "key": key, "machine_id": "forge", "machine": "Forge", "kinds": ["waiting", "nope"] });
        let e = Entry::parse(&good).unwrap();
        assert_eq!((e.relay.as_str(), e.kinds.clone()), ("https://muxin.dev", vec!["waiting".to_string()]));
        assert_eq!(Entry::parse(&e.to_json()).unwrap(), e);
        for (k, v) in [("relay", json!("http://muxin.dev")), ("relay", json!("ftp://x")), ("key", json!("c2hvcnQ=")), ("to", json!(""))] {
            let mut bad = good.clone();
            bad[k] = v;
            assert!(Entry::parse(&bad).is_err(), "{k}");
        }
        let mut local = good.clone();
        local["relay"] = json!("http://127.0.0.1:8787");
        assert!(Entry::parse(&local).is_ok());
        assert_eq!(split_url("http://[::1]:99/x"), Some((false, "::1".into(), 99, "/x".into())));
    }

    #[test]
    fn unpaired_apps_lose_their_alerts() {
        let e = Entry { relay: "https://muxin.dev".into(), to: "x".into(), key: [0; 32], machine_id: "m".into(), machine: "M".into(), kinds: vec!["bell".into()] };
        let mut apps: HashMap<String, Entry> = [("a".to_string(), e.clone()), ("b".to_string(), e)].into_iter().collect();
        assert!(!prune(&mut apps, &["a".to_string(), "b".to_string()].into_iter().collect()));
        assert!(prune(&mut apps, &["b".to_string()].into_iter().collect()));
        assert_eq!(apps.keys().collect::<Vec<_>>(), vec!["b"]);
    }

    #[test]
    fn posts_json_and_reads_the_status() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let got = thread::spawn(move || {
            let (mut c, _) = server.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            let mut req = Vec::new();
            while !String::from_utf8_lossy(&req).contains("\"to\"") {
                let n = c.read(&mut buf).unwrap();
                req.extend_from_slice(&buf[..n]);
            }
            c.write_all(b"HTTP/1.1 410 Gone\r\nContent-Type: application/json\r\n\r\n{\"error\":\"gone\"}").unwrap();
            String::from_utf8_lossy(&req).to_string()
        });
        let (status, body) = post(&format!("http://{addr}/push/v1/send"), &json!({ "to": "x" })).unwrap();
        assert_eq!((status, body.as_str()), (410, "{\"error\":\"gone\"}"));
        let req = got.join().unwrap();
        assert!(req.starts_with("POST /push/v1/send HTTP/1.1\r\n"), "{req}");
        assert!(req.contains("Content-Type: application/json\r\n") && req.ends_with("{\"to\":\"x\"}"), "{req}");
    }
}
