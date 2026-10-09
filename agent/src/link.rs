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

use std::collections::HashMap;
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{client, detect, now, proto, socket_path};

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
    Session(UnixStream),
    /// The desktop: a `screen` process (its stdin takes input).
    Desktop(std::process::ChildStdin, std::process::Child),
    /// Files going out (a tar stream; ACKs let more through).
    Pack(Arc<crate::files::Credit>),
    /// Files coming in (fed to tar -x).
    Unpack(crate::files::Unpack),
    /// The clipboard: a `clip` process (its stdin takes what the app copied).
    Clip(std::process::ChildStdin, std::process::Child),
}

impl Sink {
    fn close(self) {
        match self {
            Sink::Session(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            Sink::Desktop(stdin, mut child) => {
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
            }
            Sink::Pack(credit) => credit.close(),
            Sink::Clip(stdin, mut child) => {
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
            }
            Sink::Unpack(u) => u.stop(),
        }
    }
}

type Attached = Arc<Mutex<HashMap<u32, Sink>>>;

pub fn run() -> i32 {
    serve(Box::new(io::stdin()), Box::new(io::stdout()))
}

/// Serves one link until the other side hangs up.
pub fn serve(mut input: Box<dyn Read + Send>, output: Box<dyn Write + Send>) -> i32 {
    let out: Out = Arc::new(Mutex::new(BufWriter::new(output)));
    let attached: Attached = Arc::new(Mutex::new(HashMap::new()));
    if send_json(&out, &json!({ "event": "hello", "version": crate::VERSION, "now": now() })).is_err() {
        return 1;
    }
    loop {
        let (channel, kind, payload) = match read_frame(&mut input) {
            Ok(f) => f,
            Err(_) => break,
        };
        if channel == CONTROL {
            if kind != JSON {
                continue;
            }
            let Ok(req) = serde_json::from_slice::<Value>(&payload) else { continue };
            let (out, attached) = (out.clone(), attached.clone());
            // Requests run side by side; a slow one doesn't hold up the rest.
            thread::spawn(move || {
                let id = req["id"].clone();
                let mut reply = handle(&req, &out, &attached);
                reply["id"] = id;
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
            Some(Sink::Desktop(stdin, _)) if kind == crate::desktop::INPUT => proto::write_frame(stdin, kind, &payload).and_then(|_| stdin.flush()),
            Some(Sink::Pack(credit)) if kind == crate::files::ACK => {
                credit.ack(payload.get(..8).map(|b| u64::from_be_bytes(b.try_into().unwrap_or_default())).unwrap_or(0));
                Ok(())
            }
            Some(Sink::Unpack(u)) if kind == proto::DATA => u.write(&payload),
            Some(Sink::Clip(stdin, _)) if kind == crate::clipboard::CLIP => proto::write_frame(stdin, kind, &payload).and_then(|_| stdin.flush()),
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

fn handle(req: &Value, out: &Out, attached: &Attached) -> Value {
    match req["op"].as_str().unwrap_or("") {
        "sh" => run_script(req),
        "attach" => attach(req, out, attached),
        "desktop" => desktop(req, out, attached),
        "files" => crate::files::list(req),
        "clipboard" => clipboard(req, out, attached),
        "pack" | "unpack" => files(req, out, attached),
        "watch" => {
            let out = out.clone();
            thread::spawn(move || watch(out));
            json!({ "ok": true })
        }
        "ping" => json!({ "ok": true, "version": crate::VERSION, "now": now() }),
        "addresses" => json!({ "ok": true, "addresses": crate::direct::lan_addresses(), "tailnet": crate::direct::tailnet_addresses(), "port": crate::direct::port() }),
        "pair" => crate::direct::pair(req),
        "unpair" => crate::direct::unpair(),
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
    let name = shell.rsplit('/').next().unwrap_or("");
    let program = if login && ["bash", "zsh", "sh", "ksh", "dash"].contains(&name) { shell.clone() } else { "/bin/sh".to_string() };
    let mut cmd = Command::new(program);
    cmd.args(if login { &["-l", "-s"][..] } else { &["-s"][..] }).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }
    // Its own process group, so a timeout takes down whatever it started.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't start a shell: {e}") }),
    };
    let pid = child.id() as i32;
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
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
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

/// Connects a channel to a session, as `muxen-agent attach` does for a terminal.
fn attach(req: &Value, out: &Out, attached: &Attached) -> Value {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "attach needs a channel" });
    }
    let mut stream = match UnixStream::connect(socket_path()) {
        Ok(s) => s,
        Err(_) => return json!({ "ok": false, "error": "no sessions are running on this machine" }),
    };
    let request = json!({ "op": "attach", "name": req["name"], "cols": req["cols"], "rows": req["rows"] });
    if let Err(e) = proto::write_line(&mut stream, &request) {
        return json!({ "ok": false, "error": e.to_string() });
    }
    match proto::read_line(&mut stream) {
        Ok(v) if v["ok"].as_bool() == Some(true) => {}
        Ok(v) => return json!({ "ok": false, "error": v["error"].as_str().unwrap_or("can't attach") }),
        Err(e) => return json!({ "ok": false, "error": e.to_string() }),
    }
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
    json!({ "ok": true })
}

/// Starts streaming the desktop on a channel: `{"op": "desktop", "channel",
/// "fps", "quality", "max_width"}`. Replies with the frame size.
fn desktop(req: &Value, out: &Out, attached: &Attached) -> Value {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "desktop needs a channel" });
    }
    let Some(env) = crate::desktop::session_env() else {
        return json!({ "ok": false, "error": "No graphical session found here (nobody logged in to a desktop?)." });
    };
    let Ok(exe) = std::env::current_exe() else { return json!({ "ok": false, "error": "can't find the agent" }) };
    let arg = |k: &str, d: u64| req[k].as_u64().unwrap_or(d).to_string();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::home_dir().join("screen.log"));
    let mut cmd = Command::new(exe);
    let mode = if req["tiles"].as_bool() == Some(true) { "tiles" } else { "frames" };
    cmd.arg("screen").args([arg("fps", 30), arg("quality", 75), arg("max_width", 1920), mode.to_string()]).envs(env).stdin(Stdio::piped()).stdout(Stdio::piped());
    match log {
        Ok(l) => cmd.stderr(l),
        Err(_) => cmd.stderr(Stdio::null()),
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't start capturing the screen: {e}") }),
    };
    let (Some(stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return json!({ "ok": false, "error": "couldn't start capturing the screen" });
    };
    // Its first word: the frame size, or why it can't.
    let info = match proto::read_frame(&mut stdout) {
        Ok((crate::desktop::HELLO, p)) => serde_json::from_slice::<Value>(&p).unwrap_or(Value::Null),
        _ => json!({ "error": "the screen capture stopped right away" }),
    };
    if let Some(e) = info["error"].as_str() {
        let _ = child.kill();
        let _ = child.wait();
        return json!({ "ok": false, "error": e });
    }
    attached.lock().unwrap().insert(channel, Sink::Desktop(stdin, child));
    let (out, attached) = (out.clone(), attached.clone());
    thread::spawn(move || {
        while let Ok((kind, frame)) = proto::read_frame(&mut stdout) {
            if send(&out, channel, kind, &frame).is_err() {
                break;
            }
        }
        if let Some(s) = attached.lock().unwrap().remove(&channel) {
            s.close();
            let _ = send(&out, channel, CLOSE, &[]);
        }
    });
    let mut reply = info;
    reply["ok"] = json!(true);
    reply
}

/// Shares this machine's clipboard on the request's channel (see clipboard.rs):
/// text copied here goes to the app, text the app sends is pasted here.
fn clipboard(req: &Value, out: &Out, attached: &Attached) -> Value {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    if channel == CONTROL {
        return json!({ "ok": false, "error": "the clipboard needs a channel" });
    }
    let Ok(exe) = std::env::current_exe() else { return json!({ "ok": false, "error": "can't find the agent" }) };
    let mut cmd = Command::new(exe);
    cmd.arg("clip").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if cfg!(target_os = "linux") {
        match crate::desktop::session_env() {
            Some(env) => {
                cmd.envs(env);
            }
            None => return json!({ "ok": false, "error": "No desktop session here, so no clipboard." }),
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": format!("couldn't start sharing the clipboard: {e}") }),
    };
    let (Some(stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return json!({ "ok": false, "error": "couldn't start sharing the clipboard" });
    };
    attached.lock().unwrap().insert(channel, Sink::Clip(stdin, child));
    let (out, attached) = (out.clone(), attached.clone());
    thread::spawn(move || {
        while let Ok((kind, text)) = proto::read_frame(&mut stdout) {
            if send(&out, channel, kind, &text).is_err() {
                break;
            }
        }
        if let Some(s) = attached.lock().unwrap().remove(&channel) {
            s.close();
            let _ = send(&out, channel, CLOSE, &[]);
        }
    });
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
        let signature = match UnixStream::connect(socket_path()) {
            Err(_) => "no server".to_string(),
            Ok(mut s) => {
                let list = proto::write_line(&mut s, &json!({ "op": "list" })).and_then(|_| proto::read_line(&mut s)).unwrap_or(Value::Null);
                let sessions: Vec<String> = list["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|x| {
                        let busy = x["last_output"].as_i64().is_some_and(|t| now - t < 6);
                        format!("{}:{}:{}:{}:{}", x["name"], x["status"], x["exit_code"], x["title"], busy)
                    })
                    .collect();
                let events = list["events"].as_array().map(|e| (e.len(), e.last().cloned())).unwrap_or_default();
                format!("{}|{}|{:?}", sessions.join(","), events.0, events.1)
            }
        };
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

    #[test]
    fn answers_requests_and_runs_scripts() {
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
}
