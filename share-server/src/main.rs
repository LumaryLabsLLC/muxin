//! The share server: introduces the two sides of a Muxin share, and serves
//! the page that joins one from a browser. muxin.dev runs the same thing
//! (its own code, the same interface); this one is for running your own,
//! and for tests.
//!
//! It never sees what's shared. A share's link carries its secret after the
//! `#`, which browsers don't send; the page reads it and talks to the
//! sharing computer itself, over WebRTC. Here, each share is a room named
//! by its id: the sharing app checks in every couple of seconds with its
//! key and picks up the guests' offers; a guest leaves an offer and waits
//! for the answer. Both sides check each other end to end, so a server that
//! lies can stop a share from working but can't join one or listen in. The
//! interface is described in src/share/signal.rs.
//!
//! Configuration, from the environment:
//!
//! - `MUXIN_SHARE_LISTEN`: where to listen (`127.0.0.1:8787`). Put TLS in
//!   front (Caddy, nginx); browsers need https.
//! - `MUXIN_SHARE_STUN`: STUN servers, comma-separated (`stun:host:3478`).
//! - `MUXIN_SHARE_TURN`: TURN servers, comma-separated (`turn:host:3478`),
//!   with `MUXIN_SHARE_TURN_SECRET` (coturn's `static-auth-secret`): each
//!   side gets short-lived credentials for the relay.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use serde_json::{Value, json};

/// A room is open while its host checked in this recently.
const HOST_FRESH: Duration = Duration::from_secs(20);
/// Another key may take a room whose host has been gone this long.
const HOST_GONE: Duration = Duration::from_secs(60);
const ROOM_TTL: Duration = Duration::from_secs(15 * 60);
const OFFER_TTL: Duration = Duration::from_secs(120);
/// Offers waiting for one host.
const MAX_PENDING: usize = 32;
/// A description is a few KB.
const MAX_SDP: usize = 16 << 10;
/// How long TURN credentials last.
const TURN_TTL: Duration = Duration::from_secs(24 * 3600);

struct Room {
    /// SHA-256 of the host's key.
    host: String,
    seen: Instant,
    closed: bool,
    pending: Vec<String>,
}

struct Offer {
    room: String,
    sdp: String,
    answer: Option<String>,
    at: Instant,
}

#[derive(Default)]
struct Rooms {
    rooms: HashMap<String, Room>,
    offers: HashMap<String, Offer>,
}

struct App {
    state: Mutex<Rooms>,
    stun: Vec<String>,
    turn: Vec<String>,
    turn_secret: Option<String>,
}

type Shared = State<Arc<App>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let list = |k: &str| -> Vec<String> { std::env::var(k).unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect() };
    let app = Arc::new(App {
        state: Mutex::new(Rooms::default()),
        stun: list("MUXIN_SHARE_STUN"),
        turn: list("MUXIN_SHARE_TURN"),
        turn_secret: std::env::var("MUXIN_SHARE_TURN_SECRET").ok().filter(|s| !s.is_empty()),
    });
    if !app.turn.is_empty() && app.turn_secret.is_none() {
        tracing::warn!("MUXIN_SHARE_TURN is set without MUXIN_SHARE_TURN_SECRET: the relay won't be offered");
    }
    // Rooms and offers nobody came back for.
    let sweeping = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut st = sweeping.state.lock().unwrap();
            st.rooms.retain(|_, r| r.seen.elapsed() < ROOM_TTL);
            st.offers.retain(|_, o| o.at.elapsed() < OFFER_TTL);
        }
    });
    let addr: SocketAddr = std::env::var("MUXIN_SHARE_LISTEN").unwrap_or_else(|_| "127.0.0.1:8787".into()).parse().expect("MUXIN_SHARE_LISTEN: an address like 127.0.0.1:8787");
    let router = Router::new()
        .route("/", get(home))
        .route("/s", get(page))
        .route("/share/join.js", get(|| asset("join.js")))
        .route("/share/join.css", get(|| asset("join.css")))
        .route("/share/vendor/xterm.js", get(|| asset("vendor/xterm.js")))
        .route("/share/vendor/xterm.css", get(|| asset("vendor/xterm.css")))
        .route("/healthz", get(|| async { "ok" }))
        .route("/share/v1/rooms/{room}", get(room).delete(close))
        .route("/share/v1/rooms/{room}/host", post(check_in))
        .route("/share/v1/rooms/{room}/offers", post(offer))
        .route("/share/v1/rooms/{room}/offers/{id}", get(answer_to))
        .route("/share/v1/rooms/{room}/offers/{id}/answer", post(answer))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(addr).await.expect("can't listen");
    tracing::info!("muxin share server on http://{addr}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}

// ---------------------------------------------------------------------------
// The page

/// The page's files, built in.
fn file(name: &str) -> Option<(&'static [u8], &'static str)> {
    Some(match name {
        "index.html" => (include_bytes!("../web/index.html"), "text/html; charset=utf-8"),
        "join.js" => (include_bytes!("../web/join.js"), "text/javascript; charset=utf-8"),
        "join.css" => (include_bytes!("../web/join.css"), "text/css; charset=utf-8"),
        "vendor/xterm.js" => (include_bytes!("../web/vendor/xterm.js"), "text/javascript; charset=utf-8"),
        "vendor/xterm.css" => (include_bytes!("../web/vendor/xterm.css"), "text/css; charset=utf-8"),
        _ => return None,
    })
}

/// Headers the page gets: nothing from anywhere else, no framing, no
/// referrer. (xterm.js sets styles from script, hence 'unsafe-inline' for
/// styles only.)
fn secure(mut res: Response) -> Response {
    let h = res.headers_mut();
    let csp = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(csp));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("permissions-policy", HeaderValue::from_static("camera=(), microphone=(), geolocation=(), clipboard-read=()"));
    res
}

async fn home() -> Response {
    secure((StatusCode::OK, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "Muxin share server. Open a share link to join.\n").into_response())
}

async fn page() -> Response {
    let (body, kind) = file("index.html").expect("built in");
    secure(([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, "no-store")], body).into_response())
}

async fn asset(name: &'static str) -> Response {
    match file(name) {
        Some((body, kind)) => secure(([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, "public, max-age=300")], body).into_response()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Introductions

fn reply(status: StatusCode, body: Value) -> Response {
    let mut res = (status, axum::Json(body)).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn error(status: StatusCode, why: &str) -> Response {
    reply(status, json!({ "error": why }))
}

/// A room id: a share's 16-byte id in base64url.
fn valid_room(room: &str) -> bool {
    room.len() == 22 && room.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The host's key, from `X-Muxin-Host`, as kept (its SHA-256).
fn host_key(headers: &HeaderMap) -> Option<String> {
    let key = headers.get("x-muxin-host")?.to_str().ok()?;
    if key.len() < 32 || key.len() > 128 {
        return None;
    }
    Some(ring::digest::digest(&ring::digest::SHA256, key.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

/// A description from the request, when it looks like one.
fn sdp(body: &Value) -> Option<String> {
    let sdp = body["sdp"].as_str()?;
    (sdp.len() <= MAX_SDP && sdp.starts_with("v=0")).then(|| sdp.to_string())
}

fn open(r: &Room) -> bool {
    !r.closed && r.seen.elapsed() < HOST_FRESH
}

async fn check_in(State(app): Shared, Path(room): Path<String>, headers: HeaderMap) -> Response {
    if !valid_room(&room) {
        return error(StatusCode::NOT_FOUND, "That isn't a share.");
    }
    let Some(key) = host_key(&headers) else { return error(StatusCode::UNAUTHORIZED, "Say which computer this is.") };
    let offers = {
        let mut st = app.state.lock().unwrap();
        let st = &mut *st;
        let r = st.rooms.entry(room.clone()).or_insert_with(|| Room { host: key.clone(), seen: Instant::now(), closed: false, pending: Vec::new() });
        if r.host != key {
            if !r.closed && r.seen.elapsed() < HOST_GONE {
                return error(StatusCode::CONFLICT, "That share is open on another computer.");
            }
            *r = Room { host: key, seen: Instant::now(), closed: false, pending: Vec::new() };
        }
        r.seen = Instant::now();
        r.closed = false;
        let ids = std::mem::take(&mut r.pending);
        ids.into_iter().filter_map(|id| st.offers.get(&id).map(|o| json!({ "id": id, "sdp": o.sdp }))).collect::<Vec<_>>()
    };
    reply(StatusCode::OK, json!({ "ice": app.ice(&room), "offers": offers }))
}

async fn answer(State(app): Shared, Path((room, id)): Path<(String, String)>, headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> Response {
    let Some(key) = host_key(&headers) else { return error(StatusCode::UNAUTHORIZED, "Say which computer this is.") };
    let Some(sdp) = sdp(&body) else { return error(StatusCode::UNPROCESSABLE_ENTITY, "That isn't a description.") };
    let mut st = app.state.lock().unwrap();
    if st.rooms.get(&room).is_none_or(|r| r.host != key) {
        return error(StatusCode::FORBIDDEN, "That isn't your share.");
    }
    match st.offers.get_mut(&id).filter(|o| o.room == room) {
        Some(o) => {
            o.answer = Some(sdp);
            reply(StatusCode::OK, json!({ "ok": true }))
        }
        None => error(StatusCode::NOT_FOUND, "That offer is gone."),
    }
}

async fn close(State(app): Shared, Path(room): Path<String>, headers: HeaderMap) -> Response {
    let Some(key) = host_key(&headers) else { return error(StatusCode::UNAUTHORIZED, "Say which computer this is.") };
    let mut st = app.state.lock().unwrap();
    match st.rooms.get_mut(&room) {
        Some(r) if r.host == key => {
            r.closed = true;
            r.pending.clear();
            reply(StatusCode::OK, json!({ "ok": true }))
        }
        _ => error(StatusCode::NOT_FOUND, "That share isn't open."),
    }
}

async fn room(State(app): Shared, Path(room): Path<String>) -> Response {
    if !valid_room(&room) {
        return error(StatusCode::NOT_FOUND, "That isn't a share.");
    }
    let is_open = app.state.lock().unwrap().rooms.get(&room).is_some_and(open);
    reply(StatusCode::OK, json!({ "open": is_open, "ice": app.ice(&room) }))
}

async fn offer(State(app): Shared, Path(room): Path<String>, axum::Json(body): axum::Json<Value>) -> Response {
    let Some(sdp) = sdp(&body) else { return error(StatusCode::UNPROCESSABLE_ENTITY, "That isn't a description.") };
    let id = {
        let mut bytes = [0u8; 12];
        let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    };
    let mut st = app.state.lock().unwrap();
    let st = &mut *st;
    let Some(r) = st.rooms.get_mut(&room).filter(|r| open(r)) else {
        return error(StatusCode::NOT_FOUND, "That share isn't open right now. It may have ended.");
    };
    if r.pending.len() >= MAX_PENDING {
        return error(StatusCode::TOO_MANY_REQUESTS, "Too many people are joining this share right now. Try again in a minute.");
    }
    r.pending.push(id.clone());
    st.offers.insert(id.clone(), Offer { room, sdp, answer: None, at: Instant::now() });
    reply(StatusCode::CREATED, json!({ "id": id }))
}

async fn answer_to(State(app): Shared, Path((room, id)): Path<(String, String)>) -> Response {
    if !valid_id(&id) {
        return error(StatusCode::NOT_FOUND, "That offer is gone.");
    }
    let st = app.state.lock().unwrap();
    let is_open = st.rooms.get(&room).is_some_and(open);
    match st.offers.get(&id).filter(|o| o.room == room) {
        Some(o) => reply(StatusCode::OK, json!({ "answer": o.answer, "open": is_open })),
        None => error(StatusCode::NOT_FOUND, "That offer is gone."),
    }
}

impl App {
    /// The STUN and TURN servers to give a side, with TURN credentials of
    /// its own (coturn's REST scheme: `expiry:room`, HMAC-SHA1 of it).
    fn ice(&self, room: &str) -> Value {
        let mut ice = Vec::new();
        if !self.stun.is_empty() {
            ice.push(json!({ "urls": self.stun }));
        }
        if let (false, Some(secret)) = (self.turn.is_empty(), &self.turn_secret) {
            let expiry = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) + TURN_TTL.as_secs();
            let username = format!("{expiry}:{}", &room[..8.min(room.len())]);
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret.as_bytes());
            let credential = base64::engine::general_purpose::STANDARD.encode(ring::hmac::sign(&key, username.as_bytes()).as_ref());
            ice.push(json!({ "urls": self.turn, "username": username, "credential": credential }));
        }
        Value::Array(ice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooms_are_share_ids() {
        assert!(valid_room("AQEBAQEBAQEBAQEBAQEBAQ"));
        assert!(!valid_room("short"));
        assert!(!valid_room("AQEBAQEBAQEBAQEBAQEB/Q"));
        assert!(valid_id("AQEBAQEBAQEBAQEB"));
        assert!(!valid_id("../../etc/passwd"));
    }

    #[test]
    fn turn_credentials_follow_coturn() {
        let app = App {
            state: Mutex::new(Rooms::default()),
            stun: vec!["stun:s.example.com:3478".into()],
            turn: vec!["turn:t.example.com:3478".into()],
            turn_secret: Some("secret".into()),
        };
        let ice = app.ice("AQEBAQEBAQEBAQEBAQEBAQ");
        assert_eq!(ice[0]["urls"][0], "stun:s.example.com:3478");
        let user = ice[1]["username"].as_str().unwrap();
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, b"secret");
        let want = base64::engine::general_purpose::STANDARD.encode(ring::hmac::sign(&key, user.as_bytes()).as_ref());
        assert_eq!(ice[1]["credential"], want);
        assert!(user.ends_with(":AQEBAQEB"));
    }
}
