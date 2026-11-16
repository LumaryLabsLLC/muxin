//! `muxen-agent link`: one long-lived connection between Muxen and
//! this machine, so the app doesn't pay for a new ssh connection on every
//! request. Today it runs over ssh's stdin/stdout; the same protocol can run
//! over any byte stream.
//!
//! Everything shares it, each on its own channel:
//!
//! ```text
//! frame = [channel: u32 BE][kind: u8][len: u32 BE][payload]
//! ```
//!
//! Channel 0 carries JSON (kind `JSON`): requests from the app
//! (`{"id": 7, "op": "...", ...}`), replies (`{"id": 7, "ok": true, ...}`) and
//! events this side pushes (`{"event": "..."}`). Every other channel is a
//! terminal attached to a session, carrying the session server's own frame
//! kinds (DATA, RESIZE, EXIT) until either side sends CLOSE.
//!
//! Over a direct connection (QUIC) every stream the app opens is a link of
//! its own: the machine's link, and one for each terminal it shows (a
//! "lane"), so a busy terminal doesn't hold up the others. `{"op":
//! "priority", "priority": n}` puts this side of a stream in line: higher
//! goes first (the app uses 2 for the terminal it's typing into, 1 for the
//! machine's link, 0 for the rest).
//!
//! A link also says when the machine sleeps and wakes (sleep.rs): its hello
//! and its pings' replies have `"sleeps": true` where the agent can tell,
//! and `"asleep": true` while the machine is asleep (a dark wake included;
//! every other answer has `"asleep": true` then, too);
//! `{"event": "sleep"}` comes before it sleeps and `{"event": "awake"}` once
//! it has fully woken. An app whose ping names a port of its own
//! (`"wake_notice": 53312`) over a direct link is told there when the
//! machine wakes, with no link left (direct.rs); the reply's `wake_notice`
//! is the address that goes to, and the `sleep` event says it again. When
//! the agent can't reach that port (it tries when asked), both say
//! `wake_notice_error` instead, and the app looks for itself.

use std::collections::HashMap;
use std::io::{self, BufWriter, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::sys::{self, Stream};
use crate::{client, detect, now, proto};

/// Channel 0: requests, replies and events.
pub const CONTROL: u32 = 0;
/// A JSON message (on channel 0).
pub const JSON: u8 = 10;
/// Either side: this channel is finished.
pub const CLOSE: u8 = 11;

const MAX_FRAME: usize = 64 << 20;
/// Output kept per stream of a script; more is cut.
const MAX_OUTPUT: usize = 32 << 20;

type Out = Arc<Mutex<BufWriter<Box<dyn Write + Send>>>>;

fn send(out: &Out, channel: u32, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut w = out.lock().unwrap();
    w.write_all(&channel.to_be_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn send_json(out: &Out, value: &Value) -> io::Result<()> {
    send(out, CONTROL, JSON, &serde_json::to_vec(value).unwrap_or_default())
}

fn read_frame(r: &mut impl Read) -> io::Result<(u32, u8, Vec<u8>)> {
    let mut head = [0u8; 9];
    r.read_exact(&mut head)?;
    let channel = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    let len = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((channel, head[4], payload))
}

/// What a channel is connected to.
enum Sink {
    /// A terminal attached to a session (the session server connection).
    Session(Stream),
    /// The desktop: a `screen` program (its stdin takes input).
    Desktop(Program),
    /// Files going out (a tar stream; ACKs let more through).
    Pack(Arc<crate::files::Credit>),
    /// Files coming in (fed to tar -x).
    Unpack(crate::files::Unpack),
    /// The clipboard: a `clip` program (its stdin takes what the app copied).
    Clip(Program),
    /// A log being followed (journalctl, tail -F, docker logs -f, log stream).
    Logs(std::process::Child),
    /// A TCP connection to a port here (a server forwarded to the app's
    /// computer): what the app sends goes to its writer thread, so a slow
    /// server doesn't hold up the link.
    Tcp { to: mpsc::Sender<Vec<u8>>, stream: std::net::TcpStream },
}

impl Sink {
    fn close(self) {
        match self {
            Sink::Session(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            Sink::Desktop(p) | Sink::Clip(p) => p.close(),
            Sink::Pack(credit) => credit.close(),

            Sink::Unpack(u) => u.stop(),
            Sink::Tcp { to, stream } => {
                // What's queued still goes out (then the writer ends its half).
                drop(to);
                let _ = stream.shutdown(std::net::Shutdown::Read);
            }
            Sink::Logs(mut child) => {
                // Its whole process group (`log stream` runs under a shell).
                sys::kill_tree(child.id(), true);
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

type Attached = Arc<Mutex<HashMap<u32, Sink>>>;

/// A program run for a channel (`screen`, `clip`): here, or in the logged-in
/// session through the background service (see gui.rs), or on Windows
/// through a scheduled task (see win_screen.rs).
struct Program {
    input: Box<dyn Write + Send>,
    stop: Stop,
    /// A Mac's screen capture: it ends by itself, maybe later (see desktop.rs's
    /// `captures`), and isn't killed for taking its time.
    patient: bool,
}

enum Stop {
    Child(std::process::Child),
    #[cfg(unix)]
    Socket(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    Socket(std::net::TcpStream),
}

impl Program {
    /// Starts `muxen-agent <args>`; returns it and what it prints.
    fn start(args: &[String], env: Vec<(String, String)>) -> Result<(Program, Box<dyn Read + Send>), String> {
        #[cfg(unix)]
        let elsewhere = crate::gui::needed().then(|| crate::gui::connect(args));
        #[cfg(windows)]
        let elsewhere = (!crate::win_screen::in_desktop_session()).then(|| crate::win_screen::start_in_desktop(args));
        if let Some(stream) = elsewhere {
            let stream = stream?;
            let reader = stream.try_clone().map_err(|e| e.to_string())?;
            let writer = stream.try_clone().map_err(|e| e.to_string())?;
            return Ok((Program { input: Box::new(writer), stop: Stop::Socket(stream), patient: false }, Box::new(reader)));
        }
        let exe = std::env::current_exe().map_err(|_| "can't find the agent".to_string())?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::home_dir().join("screen.log"));
        let mut cmd = Command::new(exe);
        cmd.args(args).envs(env).stdin(Stdio::piped()).stdout(Stdio::piped());
        sys::no_window(&mut cmd);
        match log {
            Ok(l) => cmd.stderr(l),
            Err(_) => cmd.stderr(Stdio::null()),
        };
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err("couldn't start it".into());
        };
        let patient = cfg!(target_os = "macos") && args.first().map(String::as_str) == Some("screen");
        Ok((Program { input: Box::new(stdin), stop: Stop::Child(child), patient }, Box::new(stdout)))
    }

    fn close(self) {
        drop(self.input);
        match self.stop {
            // Its input closed, it ends by itself, letting go of any key or
            // button it holds down there (a ⌘ held to close its window, say).
            // Killed only if it hasn't ended in a moment.
            Stop::Child(mut child) => {
                let grace = if self.patient { 3700 } else { 2 };
                thread::spawn(move || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(grace);
                    while std::time::Instant::now() < deadline {
                        if matches!(child.try_wait(), Ok(Some(_))) {
                            return;
                        }
                        thread::sleep(std::time::Duration::from_millis(50));
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                });
            }
            Stop::Socket(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

pub fn run() -> i32 {
    serve(Box::new(io::stdin()), Box::new(io::stdout()))
}

/// Serves one link until the other side hangs up.
pub fn serve(input: Box<dyn Read + Send>, output: Box<dyn Write + Send>) -> i32 {
    serve_for(input, output, Peer::default(), None)
}

/// Where a direct link's stream goes in line (see `priority` above).
pub type Priority = Arc<std::sync::atomic::AtomicI32>;

/// The paired app a direct link serves (over ssh there's no telling which
/// app it is): its certificate fingerprint, and where it connected from.
#[derive(Clone, Debug, Default)]
pub struct Peer {
    pub app: Option<String>,
    pub addr: Option<std::net::SocketAddr>,
}

/// The same, for a paired app (a direct link), on a stream whose priority
/// the app may set.
pub fn serve_for(mut input: Box<dyn Read + Send>, output: Box<dyn Write + Send>, peer: Peer, priority: Option<Priority>) -> i32 {
    let out: Out = Arc::new(Mutex::new(BufWriter::new(output)));
    let attached: Attached = Arc::new(Mutex::new(HashMap::new()));
    crate::sleep::start();
    let mut hello = json!({ "event": "hello", "version": crate::VERSION, "now": now() });
    crate::sleep::describe(&mut hello);
    if hello["asleep"].as_bool() == Some(true) {
        crate::sleep::reached_asleep();
    }
    if send_json(&out, &hello).is_err() {
        return 1;
    }
    // Before the machine sleeps, and once it has fully woken, the app hears.
    let _listening = {
        let (out, peer) = (out.clone(), peer.clone());
        crate::sleep::listen(move |change| {
            let event = match change {
                crate::sleep::Change::Sleep => {
                    let mut e = json!({ "event": "sleep" });
                    if crate::sleep::lid_closed() {
                        e["lid_closed"] = json!(true);
                    }
                    if let Some(to) = crate::direct::notice_for(&peer) {
                        e["wake_notice"] = json!(to.to_string());
                    } else if let Some(why) = crate::direct::notice_refused(&peer) {
                        e["wake_notice_error"] = json!(why);
                    }
                    e
                }
                crate::sleep::Change::Awake => json!({ "event": "awake" }),
            };
            if send_json(&out, &event).is_ok() && change == crate::sleep::Change::Sleep {
                crate::sleep::told_an_app();
            }
        })
    };
    loop {
        let (channel, kind, mut payload) = match read_frame(&mut input) {
            Ok(f) => f,
            Err(_) => break,
        };
        if channel == CONTROL {
            if kind != JSON {
                continue;
            }
            let Ok(req) = serde_json::from_slice::<Value>(&payload) else { continue };
            if req["op"] == "priority" {
                let reply = match &priority {
                    Some(p) => {
                        p.store(req["priority"].as_i64().unwrap_or(0).clamp(-100, 100) as i32, std::sync::atomic::Ordering::Relaxed);
                        json!({ "ok": true })
                    }
                    None => json!({ "ok": false, "error": "only direct links have priorities" }),
                };
                let mut reply = reply;
                reply["id"] = req["id"].clone();
                let _ = send_json(&out, &reply);
                continue;
            }
            let (out, attached, peer) = (out.clone(), attached.clone(), peer.clone());
            // Requests run side by side; a slow one doesn't hold up the rest.
            thread::spawn(move || {
                let id = req["id"].clone();
                let mut reply = handle(&req, &out, &attached, &peer);
                reply["id"] = id;
                crate::sleep::mark(&mut reply);
                let _ = send_json(&out, &reply);
            });
            continue;
        }
        // A terminal's input or size, desktop input, or the app letting go.
        let mut map = attached.lock().unwrap();
        if kind == CLOSE {
            if let Some(s) = map.remove(&channel) {
                s.close();
            }
            continue;
        }
        let written = match map.get_mut(&channel) {
            Some(Sink::Session(s)) if kind == proto::DATA || kind == proto::RESIZE => proto::write_frame(s, kind, &payload),
            Some(Sink::Desktop(p)) if kind == crate::desktop::INPUT => crate::gui::write(&mut p.input, kind, &payload),
            Some(Sink::Pack(credit)) if kind == crate::files::ACK => {
                credit.ack(payload.get(..8).map(|b| u64::from_be_bytes(b.try_into().unwrap_or_default())).unwrap_or(0));
                Ok(())
            }
            Some(Sink::Unpack(u)) if kind == proto::DATA => u.write(&payload),
            Some(Sink::Tcp { to, .. }) if kind == proto::DATA => to.send(std::mem::take(&mut payload)).map_err(|_| io::Error::other("closed")),
            Some(Sink::Clip(p)) if kind == crate::clipboard::CLIP => crate::gui::write(&mut p.input, kind, &payload),
            Some(Sink::Unpack(u)) if kind == crate::files::END => {
                u.end();
                Ok(())
            }
            _ => Ok(()),
        };
        if written.is_err() {
            if let Some(s) = map.remove(&channel) {
                s.close();
            }
        }
    }
    // The app went away; sessions keep running in the session server.
    for (_, s) in attached.lock().unwrap().drain() {
        s.close();
    }
    0
}

fn handle(req: &Value, out: &Out, attached: &Attached, peer: &Peer) -> Value {
    match req["op"].as_str().unwrap_or("") {
        "sh" => run_script(req),
        "attach" => attach(req, out, attached),
        "desktop" => desktop(req, out, attached),
        "files" => crate::files::list(req),
        "clipboard" => clipboard(req, out, attached),
        "logs" => logs(req, out, attached),
        "tcp" => tcp(req, out, attached),
        "pack" | "unpack" => files(req, out, attached),
        "watch" => {
            let out = out.clone();
            thread::spawn(move || watch(out));
            json!({ "ok": true })
        }
        "ping" => {
            let mut reply = json!({ "ok": true, "version": crate::VERSION, "now": now() });
            crate::sleep::describe(&mut reply);
            // An app that wants to hear when this machine wakes, on that port:
            // where its notice goes, or why none can.
            if let Some(port) = req["wake_notice"].as_u64().and_then(|p| u16::try_from(p).ok()) {
                match crate::direct::wants_notice(peer, port) {
                    Some(Ok(to)) => reply["wake_notice"] = json!(to.to_string()),
                    Some(Err(why)) => reply["wake_notice_error"] = json!(why),
                    None => {}
                }
            }
            reply
        }
        "addresses" => json!({ "ok": true, "addresses": crate::direct::lan_addresses(), "tailnet": crate::direct::tailnet_addresses(), "port": crate::direct::port() }),
        "pair" => crate::direct::pair(req),
        "trust" => crate::direct::trust(req),
        "unpair" => crate::direct::unpair(req),
        "push" => crate::push::op(req, peer.app.as_deref()),
        // Everything `muxen-agent ctl` does.
        _ => client::handle(req),
    }
}

/// Runs a script in the user's login shell (Homebrew, nvm and friends on PATH),
/// as ssh would: the script on stdin, both outputs and the exit status back.
/// `"login": false` runs a plain `sh` instead (machine checks).
fn run_script(req: &Value) -> Value {
    let script = req["script"].as_str().unwrap_or("").to_string();
    let timeout = Duration::from_secs(req["timeout"].as_u64().unwrap_or(120).clamp(1, 3600));
    let login = req["login"].as_bool().unwrap_or(true);
    let shell = detect::user_shell();
    let mut cmd = if cfg!(windows) {
        // PowerShell reads the whole script, then runs it (with the user's profile unless `login` is off).
        let mut c = Command::new(&shell);
        if !login {
            c.arg("-NoProfile");
        }
        c.args(["-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", "[Console]::In.ReadToEnd() | Invoke-Expression"]);
        c
    } else {
        let name = shell.rsplit('/').next().unwrap_or("");
        let program = if login && ["bash", "zsh", "sh", "ksh", "dash"].contains(&name) { shell.clone() } else { "/bin/sh".to_string() };
        let mut c = Command::new(program);
        c.args(if login { &["-l", "-s"][..] } else { &["-s"][..] });
        c
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).current_dir(sys::user_home());
    // Its own process group, so a timeout takes down whatever it started.
    sys::own_group(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't start a shell: {e}") }),
    };
    let pid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        thread::spawn(move || {
            let _ = stdin.write_all(script.as_bytes());
        });
    }
    let collect = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let mut chunk = [0u8; 16 * 1024];
                while let Ok(n) = p.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    if buf.len() < MAX_OUTPUT {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let stdout = collect(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = collect(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(5)),
            _ => {
                timed_out = true;
                sys::kill_tree(pid, false);
                let _ = child.wait();
                break None;
            }
        }
    };
    // Something it left running may hold the pipes; don't wait on it for long.
    let grace = Duration::from_secs(2);
    let text = |rx: mpsc::Receiver<Vec<u8>>| String::from_utf8_lossy(&rx.recv_timeout(grace).unwrap_or_default()).into_owned();
    json!({ "ok": true, "status": status, "stdout": text(stdout), "stderr": text(stderr), "timed_out": timed_out })
}

/// Connects a channel to a session, as `muxen-agent attach` does for a
/// terminal (`watch_only` and `sizes` as the session server takes them).
fn attach(req: &Value, out: &Out, attached: &Attached) -> Value {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "attach needs a channel" });
    }
    let mut stream = match sys::connect() {
        Ok(s) => s,
        Err(_) => return json!({ "ok": false, "error": "no sessions are running on this machine" }),
    };
    let request = json!({ "op": "attach", "name": req["name"], "cols": req["cols"], "rows": req["rows"], "watch_only": req["watch_only"], "sizes": req["sizes"] });
    if let Err(e) = proto::write_line(&mut stream, &request) {
        return json!({ "ok": false, "error": e.to_string() });
    }
    let size = match proto::read_line(&mut stream) {
        Ok(v) if v["ok"].as_bool() == Some(true) => json!({ "cols": v["cols"], "rows": v["rows"] }),
        Ok(v) => return json!({ "ok": false, "error": v["error"].as_str().unwrap_or("can't attach") }),
        Err(e) => return json!({ "ok": false, "error": e.to_string() }),
    };
    let Ok(mut reader) = stream.try_clone() else { return json!({ "ok": false, "error": "can't attach" }) };
    attached.lock().unwrap().insert(channel, Sink::Session(stream));
    // The session's output, and its end, to the app.
    let (out, attached) = (out.clone(), attached.clone());
    thread::spawn(move || {
        loop {
            match proto::read_frame(&mut reader) {
                Ok((kind, bytes)) => {
                    if send(&out, channel, kind, &bytes).is_err() {
                        break;
                    }
                    if kind == proto::EXIT {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        if attached.lock().unwrap().remove(&channel).is_some() {
            let _ = send(&out, channel, CLOSE, &[]);
        }
    });
    json!({ "ok": true, "cols": size["cols"], "rows": size["rows"] })
}

/// Starts streaming the desktop on a channel: `{"op": "desktop", "channel",
/// "fps", "quality", "max_width", "display"}` (see displays.rs; empty: the
/// whole desktop or the main display). Replies with the frame size.
fn desktop(req: &Value, out: &Out, attached: &Attached) -> Value {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "desktop needs a channel" });
    }
    // Nobody's screen to show: `no_session` lets the app offer the machine's
    // own remote desktop (RDP, VNC) instead, which can sign someone in.
    let Some(env) = crate::desktop::session_env() else {
        return json!({ "ok": false, "error": "No graphical session found here (nobody logged in to a desktop?).", "no_session": true });
    };
    if let Some(why) = crate::desktop::no_session() {
        return json!({ "ok": false, "error": why, "no_session": true });
    }
    let arg = |k: &str, d: u64| req[k].as_u64().unwrap_or(d).to_string();
    let mode = if req["tiles"].as_bool() == Some(true) { "tiles" } else { "frames" };
    let display = req["display"].as_str().unwrap_or("").to_string();
    let args = vec!["screen".to_string(), arg("fps", 30), arg("quality", 75), arg("max_width", 1920), mode.to_string(), display];
    let (program, mut frames) = match Program::start(&args, env) {
        Ok(p) => p,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't start capturing the screen: {e}") }),
    };
    // Its first word: the frame size, or why it can't.
    let info = crate::gui::read_hello(&mut frames);
    if let Some(e) = info["error"].as_str() {
        program.close();
        return json!({ "ok": false, "error": e, "gone": info["gone"].as_bool() == Some(true) });
    }
    attached.lock().unwrap().insert(channel, Sink::Desktop(program));
    relay(out, attached, channel, frames);
    let mut reply = info;
    reply["ok"] = json!(true);
    reply
}

/// What a channel's program prints goes to the app, frame by frame, until it
/// stops; then the channel closes.
fn relay(out: &Out, attached: &Attached, channel: u32, mut from: Box<dyn Read + Send>) {
    let (out, attached) = (out.clone(), attached.clone());
    thread::spawn(move || {
        while let Ok((kind, frame)) = proto::read_frame(&mut from) {
            if send(&out, channel, kind, &frame).is_err() {
                // Read to its end all the same: a program stuck writing never ends.
                let _ = io::copy(&mut from, &mut io::sink());
                break;
            }
        }
        if let Some(s) = attached.lock().unwrap().remove(&channel) {
            s.close();
            let _ = send(&out, channel, CLOSE, &[]);
        }
    });
}

/// Follows a log on a channel: `{"op": "logs", "channel", "source",
/// "lines"}` (see logs.rs). Its last lines, then new ones, go to the app as
/// DATA, whole lines at a time, until it stops or the app closes the channel.
fn logs(req: &Value, out: &Out, attached: &Attached) -> Value {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "a log needs a channel" });
    }
    let (program, args) = match crate::logs::command(&req["source"], crate::logs::initial_lines(req), true) {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let mut cmd = Command::new(&program);
    cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    sys::own_group(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't run {program}: {e}") }),
    };
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    attached.lock().unwrap().insert(channel, Sink::Logs(child));
    // Both outputs (docker writes a container's stderr to its own), a whole
    // line at a time so the two don't cut into each other's lines. One
    // thread reads, another sends: a Compose project's last lines come one
    // container after another, so they're put in time order first.
    let sort_backlog = req["source"]["kind"] == "compose";
    let pump = |pipe: Box<dyn Read + Send>, last: bool, sort_backlog: bool| {
        let (out, attached) = (out.clone(), attached.clone());
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut pipe = pipe;
            let mut pending: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 16 * 1024];
            loop {
                let n = pipe.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&chunk[..n]);
                if let Some(end) = pending.iter().rposition(|b| *b == b'\n') && tx.send(pending.drain(..=end).collect()).is_err() {
                    return;
                }
            }
            if !pending.is_empty() {
                pending.push(b'\n');
                let _ = tx.send(pending);
            }
        });
        thread::spawn(move || {
            // The app can't be reached: stop the program.
            let deliver = |lines: &[u8]| {
                let sent = send(&out, channel, proto::DATA, lines).is_ok();
                if !sent && let Some(s) = attached.lock().unwrap().remove(&channel) {
                    s.close();
                }
                sent
            };
            if sort_backlog {
                // The backlog: what comes before the first pause.
                let mut backlog: Vec<u8> = Vec::new();
                let started = Instant::now();
                let mut wait = Duration::from_secs(5);
                while let Ok(lines) = rx.recv_timeout(wait) {
                    backlog.extend(lines);
                    wait = Duration::from_millis(400);
                    if started.elapsed() > Duration::from_secs(4) {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&backlog).into_owned();
                let sorted = crate::logs::by_time(text.lines().collect());
                if !sorted.is_empty() && !deliver(format!("{}\n", sorted.join("\n")).as_bytes()) {
                    return;
                }
            }
            for lines in rx {
                if !deliver(&lines) {
                    return;
                }
            }
            // Stdout ended: the program stopped.
            if last && let Some(s) = attached.lock().unwrap().remove(&channel) {
                s.close();
                let _ = send(&out, channel, CLOSE, &[]);
            }
        });
    };
    if let Some(e) = stderr {
        pump(Box::new(e), false, false);
    }
    if let Some(o) = stdout {
        pump(Box::new(o), true, sort_backlog);
    }
    json!({ "ok": true })
}

/// `{"op": "tcp", "channel", "port", "host"?}`: connects to a port here (or
/// at `host`, 127.0.0.1 by default) and joins it to the channel: a server on
/// this machine, reached from the app's computer.
fn tcp(req: &Value, out: &Out, attached: &Attached) -> Value {
    use std::net::{IpAddr, Ipv6Addr, Shutdown, SocketAddr, TcpStream};
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "a connection needs a channel" });
    }
    let Some(port) = req["port"].as_u64().filter(|p| (1..=65535).contains(p)).map(|p| p as u16) else {
        return json!({ "ok": false, "error": "Which port?" });
    };
    let host = req["host"].as_str().filter(|h| !h.is_empty()).unwrap_or("127.0.0.1");
    let Ok(ip) = host.parse::<IpAddr>() else {
        return json!({ "ok": false, "error": format!("\"{host}\" isn't an address.") });
    };
    // A server listening only on IPv6's localhost answers there.
    let mut tries = vec![SocketAddr::new(ip, port)];
    if host == "127.0.0.1" {
        tries.push(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port));
    }
    let Some(stream) = tries.iter().find_map(|a| TcpStream::connect_timeout(a, Duration::from_secs(5)).ok()) else {
        return json!({ "ok": false, "error": format!("Nothing is listening on port {port} there.") });
    };
    let _ = stream.set_nodelay(true);
    let (Ok(mut reader), Ok(mut writer)) = (stream.try_clone(), stream.try_clone()) else {
        return json!({ "ok": false, "error": "couldn't use the connection" });
    };
    let (to, from) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        for chunk in from {
            if writer.write_all(&chunk).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(Shutdown::Write);
    });
    attached.lock().unwrap().insert(channel, Sink::Tcp { to, stream });
    let (out, attached) = (out.clone(), attached.clone());
    thread::spawn(move || {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if send(&out, channel, proto::DATA, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        // The server hung up (or the app can't be reached): tell the app.
        if let Some(s) = attached.lock().unwrap().remove(&channel) {
            s.close();
            let _ = send(&out, channel, CLOSE, &[]);
        }
    });
    json!({ "ok": true })
}

/// Shares this machine's clipboard on the request's channel (see clipboard.rs):
/// text copied here goes to the app, text the app sends is pasted here.
fn clipboard(req: &Value, out: &Out, attached: &Attached) -> Value {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "the clipboard needs a channel" });
    }
    let Some(env) = crate::desktop::session_env() else {
        return json!({ "ok": false, "error": "No desktop session here, so no clipboard." });
    };
    let (program, from) = match Program::start(&["clip".to_string()], env) {
        Ok(p) => p,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't start sharing the clipboard: {e}") }),
    };
    attached.lock().unwrap().insert(channel, Sink::Clip(program));
    relay(out, attached, channel, from);
    json!({ "ok": true })
}

/// Starts one end of a copy on the request's channel (see files.rs).
fn files(req: &Value, out: &Out, attached: &Attached) -> Value {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "a copy needs a channel" });
    }
    let o = out.clone();
    let send: crate::files::Send = Arc::new(move |c, kind, payload| send(&o, c, kind, payload));
    let started = if req["op"] == "pack" {
        crate::files::pack(req, send).map(|(reply, credit)| (reply, Sink::Pack(credit)))
    } else {
        crate::files::unpack(req, send).map(|(reply, u)| (reply, Sink::Unpack(u)))
    };
    match started {
        Ok((reply, sink)) => {
            attached.lock().unwrap().insert(channel, sink);
            reply
        }
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

/// Tells the app when sessions change (one starts or ends, starts or stops
/// printing, rings the bell), so it doesn't have to keep asking.
fn watch(out: Out) {
    let mut last = String::new();
    loop {
        let now = now();
        let signature = match sys::connect() {
            Err(_) => "no server".to_string(),
            Ok(mut s) => {
                let mut list = proto::write_line(&mut s, &json!({ "op": "list" })).and_then(|_| proto::read_line(&mut s)).unwrap_or(Value::Null);
                // A hook saying it's done or needs you is news right away.
                if let Some(sessions) = list.get_mut("sessions") {
                    crate::states::annotate(sessions, &crate::states::all(), now);
                }
                let sessions: Vec<String> = list["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|x| {
                        let busy = x["last_output"].as_i64().is_some_and(|t| now - t < 6);
                        format!("{}:{}:{}:{}:{}:{}", x["name"], x["status"], x["exit_code"], x["title"], busy, x["state"])
                    })
                    .collect();
                let events = list["events"].as_array().map(|e| (e.len(), e.last().cloned())).unwrap_or_default();
                format!("{}|{}|{:?}", sessions.join(","), events.0, events.1)
            }
        };
        // Claude's desktop app's sessions (apps.rs), with or without a server.
        let apps: Vec<String> = crate::apps::listed(&crate::states::all(), now).iter().map(|x| format!("{}:{}:{}", x["name"], x["title"], x["state"])).collect();
        let signature = format!("{signature}|{}", apps.join(","));
        if signature != last {
            last = signature;
            if send_json(&out, &json!({ "event": "sessions" })).is_err() {
                return;
            }
        }
        thread::sleep(Duration::from_millis(300));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = channel.to_be_bytes().to_vec();
        f.push(kind);
        f.extend((payload.len() as u32).to_be_bytes());
        f.extend(payload);
        f
    }

    #[cfg(unix)]
    #[test]
    fn answers_requests_and_runs_scripts() {
        use std::os::unix::net::UnixStream;
        let _turn = crate::sleep::test_lock();
        let (mut app, agent) = UnixStream::pair().unwrap();
        let agent_out = agent.try_clone().unwrap();
        let server = thread::spawn(move || serve(Box::new(agent), Box::new(agent_out)));
        let next = |app: &mut UnixStream| {
            let (channel, kind, payload) = read_frame(app).unwrap();
            assert_eq!((channel, kind), (CONTROL, JSON));
            serde_json::from_slice::<Value>(&payload).unwrap()
        };
        assert_eq!(next(&mut app)["event"], "hello");
        app.write_all(&frame(CONTROL, JSON, br#"{"id":1,"op":"ping"}"#)).unwrap();
        let script = json!({ "id": 2, "op": "sh", "login": false, "script": "echo out; echo err >&2; exit 4" });
        app.write_all(&frame(CONTROL, JSON, script.to_string().as_bytes())).unwrap();
        let mut replies: Vec<Value> = (0..2).map(|_| next(&mut app)).collect();
        replies.sort_by_key(|r| r["id"].as_u64());
        assert_eq!(replies[0]["version"], crate::VERSION);
        assert_eq!((replies[1]["stdout"].as_str(), replies[1]["stderr"].as_str(), replies[1]["status"].as_i64()), (Some("out\n"), Some("err\n"), Some(4)));
        drop(app);
        assert_eq!(server.join().unwrap(), 0);
    }

    /// An app before 0.5.7 passed on everything the other machine's tar
    /// wrote: after the end of the archive, the zeros tar fills its last
    /// record with. A tar that leaves at the end of the archive without
    /// reading on (macOS's) is gone when they come, and the write to it
    /// fails. The files are all there by then, and that's what the app hears.
    #[cfg(unix)]
    #[test]
    fn zeros_after_the_end_of_an_archive_dont_fail_a_copy() {
        let base = std::env::temp_dir().join(format!("muxen-link-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (src, dst) = (base.join("src"), base.join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("data.bin"), vec![7u8; 300_000]).unwrap();
        let tar = Command::new("tar").env("COPYFILE_DISABLE", "1").arg("-C").arg(&src).args(["-cf", "-", "./data.bin"]).output().unwrap();
        assert!(tar.status.success());
        // The archive to its end (two empty blocks), and the zeros that fill
        // its last record of 10240 bytes (BusyBox's tar leaves them out).
        let end = crate::files::entries_end(&tar.stdout) + 1024;
        let (archive, zeros) = (&tar.stdout[..end], vec![0u8; 10240 - end % 10240]);

        // Two pipes, as over ssh: each end has one owner, and closing the
        // app's ends the link.
        let (agent_in, mut app) = io::pipe().unwrap();
        let (mut from_agent, agent_out) = io::pipe().unwrap();
        let server = thread::spawn(move || serve(Box::new(agent_in), Box::new(agent_out)));
        // What the agent sends, as it comes.
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(f) = read_frame(&mut from_agent) {
                if tx.send(f).is_err() {
                    break;
                }
            }
        });
        let patience = Duration::from_secs(60);
        let json_of = |f: (u32, u8, Vec<u8>)| serde_json::from_slice::<Value>(&f.2).unwrap();
        assert_eq!(json_of(frames.recv_timeout(patience).unwrap())["event"], "hello");
        let req = json!({ "id": 1, "op": "unpack", "channel": 7, "dir": dst.display().to_string() });
        app.write_all(&frame(CONTROL, JSON, req.to_string().as_bytes())).unwrap();
        let reply = json_of(frames.recv_timeout(patience).unwrap());
        assert_eq!(reply["ok"], true, "{reply}");

        // All of the archive is taken.
        app.write_all(&frame(7, proto::DATA, archive)).unwrap();
        let (channel, kind, taken) = frames.recv_timeout(patience).unwrap();
        assert_eq!((channel, kind, taken), (7, crate::files::ACK, (end as u64).to_be_bytes().to_vec()));
        // macOS's tar has left by now, and its result comes unasked. GNU tar
        // waits for the rest of its record and BusyBox's for the end of its
        // input: they take the zeros.
        let mut result = match frames.recv_timeout(Duration::from_secs(1)) {
            Ok((7, crate::files::END, result)) => Some(result),
            _ => None,
        };
        app.write_all(&frame(7, proto::DATA, &zeros)).unwrap();
        app.write_all(&frame(7, crate::files::END, &[])).unwrap();
        while result.is_none() {
            if let (7, crate::files::END, r) = frames.recv_timeout(patience).expect("the copy's result") {
                result = Some(r);
            }
        }
        let result = serde_json::from_slice::<Value>(&result.unwrap()).unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert!(std::fs::read(dst.join("data.bin")).unwrap() == vec![7u8; 300_000], "data.bin arrives whole");
        drop(app);
        assert_eq!(server.join().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A direct link's stream takes the priority the app asks for; one that
    /// never asks (an app before 0.14) is served as before, and over ssh
    /// (no stream of its own) the app is told there's none.
    #[cfg(unix)]
    #[test]
    fn a_direct_stream_takes_a_priority() {
        use std::os::unix::net::UnixStream;
        use std::sync::atomic::Ordering;
        let _turn = crate::sleep::test_lock();
        let next = |app: &mut UnixStream| {
            let (channel, kind, payload) = read_frame(app).unwrap();
            assert_eq!((channel, kind), (CONTROL, JSON));
            serde_json::from_slice::<Value>(&payload).unwrap()
        };
        for direct in [true, false] {
            let (mut app, agent) = UnixStream::pair().unwrap();
            let agent_out = agent.try_clone().unwrap();
            let priority: Priority = Default::default();
            let given = direct.then(|| priority.clone());
            let server = thread::spawn(move || serve_for(Box::new(agent), Box::new(agent_out), Peer::default(), given));
            assert_eq!(next(&mut app)["event"], "hello");
            // An app that never asks.
            app.write_all(&frame(CONTROL, JSON, br#"{"id":1,"op":"ping"}"#)).unwrap();
            assert_eq!((next(&mut app)["id"].as_u64(), priority.load(Ordering::Relaxed)), (Some(1), 0));
            app.write_all(&frame(CONTROL, JSON, br#"{"id":2,"op":"priority","priority":2}"#)).unwrap();
            let r = next(&mut app);
            assert_eq!((r["id"].as_u64(), r["ok"].as_bool()), (Some(2), Some(direct)), "{r}");
            assert_eq!(priority.load(Ordering::Relaxed), if direct { 2 } else { 0 });
            app.write_all(&frame(CONTROL, JSON, br#"{"id":3,"op":"priority","priority":-7}"#)).unwrap();
            assert_eq!(next(&mut app)["id"], 3);
            assert_eq!(priority.load(Ordering::Relaxed), if direct { -7 } else { 0 });
            drop(app);
            assert_eq!(server.join().unwrap(), 0);
        }
    }

    /// A link says when the machine is about to sleep and when it has woken.
    /// While it sleeps, pings say so, and so does the hello of a link made
    /// then (an app's packet woke the machine for a moment). An app before
    /// all this hears two events it doesn't know, and is served as ever.
    #[cfg(unix)]
    #[test]
    fn says_when_the_machine_sleeps_and_wakes() {
        use crate::sleep::{Change, test_change};
        use std::os::unix::net::UnixStream;
        let _turn = crate::sleep::test_lock();
        let next = |app: &mut UnixStream| {
            let (channel, kind, payload) = read_frame(app).unwrap();
            assert_eq!((channel, kind), (CONTROL, JSON));
            serde_json::from_slice::<Value>(&payload).unwrap()
        };
        let link = || {
            let (app, agent) = UnixStream::pair().unwrap();
            let agent_out = agent.try_clone().unwrap();
            (app, thread::spawn(move || serve(Box::new(agent), Box::new(agent_out))))
        };
        let (mut app, server) = link();
        let hello = next(&mut app);
        assert_eq!((hello["event"].as_str(), hello["sleeps"].as_bool(), hello.get("asleep")), (Some("hello"), Some(true), None));
        // Asking for a wake notice over ssh: there's nowhere to send one.
        app.write_all(&frame(CONTROL, JSON, br#"{"id":1,"op":"ping","wake_notice":53312}"#)).unwrap();
        let pong = next(&mut app);
        assert_eq!((pong["id"].as_u64(), pong["ok"].as_bool(), pong.get("asleep"), pong.get("wake_notice")), (Some(1), Some(true), None, None));

        // Said before the machine may sleep: it's on the wire when this returns.
        test_change(Change::Sleep);
        let event = next(&mut app);
        assert_eq!((event["event"].as_str(), event.get("wake_notice")), (Some("sleep"), None));
        app.write_all(&frame(CONTROL, JSON, br#"{"id":2,"op":"ping"}"#)).unwrap();
        let pong = next(&mut app);
        assert_eq!((pong["id"].as_u64(), pong["asleep"].as_bool()), (Some(2), Some(true)));
        // A link made while it sleeps.
        let (mut late, late_server) = link();
        let hello = next(&mut late);
        assert_eq!((hello["event"].as_str(), hello["asleep"].as_bool()), (Some("hello"), Some(true)));
        // It still answers: an app from before this goes on as it did. And
        // every answer says it's asleep, for an app that missed the word.
        late.write_all(&frame(CONTROL, JSON, br#"{"id":1,"op":"sh","login":false,"script":"echo up"}"#)).unwrap();
        let ran = next(&mut late);
        assert_eq!((ran["stdout"].as_str(), ran["asleep"].as_bool()), (Some("up\n"), Some(true)));

        test_change(Change::Awake);
        assert_eq!(next(&mut app)["event"], "awake");
        assert_eq!(next(&mut late)["event"], "awake");
        app.write_all(&frame(CONTROL, JSON, br#"{"id":3,"op":"ping"}"#)).unwrap();
        assert_eq!(next(&mut app).get("asleep"), None);
        drop((app, late));
        assert_eq!((server.join().unwrap(), late_server.join().unwrap()), (0, 0));
    }
}
