//! The agent's client commands: `ctl` (one JSON request, one JSON reply) and
//! `attach` (this terminal becomes a window onto a session).

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::{detect, home_dir, now, proto, socket_path};

/// Connects to the session server, starting it first when asked.
fn connect(start: bool) -> io::Result<UnixStream> {
    match UnixStream::connect(socket_path()) {
        Ok(s) => Ok(s),
        Err(e) if !start => Err(e),
        Err(_) => {
            spawn_server()?;
            let mut last = None;
            for _ in 0..100 {
                thread::sleep(Duration::from_millis(30));
                match UnixStream::connect(socket_path()) {
                    Ok(s) => return Ok(s),
                    Err(e) => last = Some(e),
                }
            }
            Err(last.unwrap_or_else(|| io::Error::other("the session server didn't start")))
        }
    }
}

/// Starts `serve` in the background, in its own session so it outlives this
/// ssh connection, logging to ~/.muxen/agent.log.
fn spawn_server() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(home_dir().join("agent.log"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).current_dir(home_dir());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

fn request(req: &Value, start: bool) -> Value {
    let mut stream = match connect(start) {
        Ok(s) => s,
        Err(e) if start => return json!({ "ok": false, "error": format!("couldn't reach the session server: {e}") }),
        Err(_) => return json!({ "ok": false, "error": "no sessions are running on this machine" }),
    };
    if let Err(e) = proto::write_line(&mut stream, req) {
        return json!({ "ok": false, "error": e.to_string() });
    }
    proto::read_line(&mut stream).unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }))
}

pub fn ctl() -> i32 {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    let req: Value = match serde_json::from_str(input.trim()) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", json!({ "ok": false, "error": format!("bad request: {e}") }));
            return 1;
        }
    };
    println!("{}", handle(&req));
    0
}

/// One request, as `ctl` or a link gets it.
pub fn handle(req: &Value) -> Value {
    let req = req.clone();
    match req["op"].as_str().unwrap_or("") {
        "discover" => discover(&req),
        // Checkouts and git work happen right here; no session server needed.
        "repos" => {
            let roots: Vec<String> = req["roots"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
            json!({ "ok": true, "repos": crate::repos::scan(&roots), "now": now() })
        }
        "git" => match crate::repos::act(req["path"].as_str().unwrap_or(""), req["action"].as_str().unwrap_or("")) {
            Ok(output) => json!({ "ok": true, "output": output }),
            Err(e) => json!({ "ok": false, "error": e }),
        },
        "start" => request(&req, true),
        _ => request(&req, false),
    }
}

/// Everything Muxen wants to know about this machine's sessions. Doesn't
/// start the server just to look.
fn discover(req: &Value) -> Value {
    let names: Vec<String> = req["agents"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    // Installed tools are looked up only when asked (it starts an interactive shell).
    let dirs = if names.is_empty() { Vec::new() } else { detect::path_dirs() };
    let agents = if names.is_empty() { Vec::new() } else { detect::agents(&names, &dirs) };
    let gh = if names.is_empty() { Value::Null } else { crate::repos::gh_status(&dirs) };
    let running = UnixStream::connect(socket_path()).is_ok();
    let list = if running { request(&json!({ "op": "list" }), false) } else { json!({}) };
    json!({
        "ok": true,
        "version": crate::VERSION,
        "agents": agents,
        "agents_checked": !names.is_empty(),
        "gh": gh,
        "server": running,
        "now": now(),
        "sessions": list.get("sessions").cloned().unwrap_or_else(|| json!([])),
        "events": list.get("events").cloned().unwrap_or_else(|| json!([])),
    })
}

/// Saves the terminal's settings and puts it in raw mode; restores on drop.
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable() -> RawMode {
        unsafe {
            if libc::isatty(0) == 0 {
                return RawMode(None);
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return RawMode(None);
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            RawMode(Some(saved))
        }
    }

    fn restore(&self) {
        if let Some(saved) = &self.0 {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, saved);
            }
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

fn window_size() -> Option<(u16, u16)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        (libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
    }
}

/// Bridges this terminal and a session until the session ends, the
/// connection drops, or (for people, not the app) Ctrl-] then d detaches.
pub fn attach(name: &str) -> i32 {
    let for_app = std::env::args().any(|a| a == "--app");
    let mut stream = match connect(false) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("muxen-agent: no sessions are running on this machine");
            return 1;
        }
    };
    let (cols, rows) = window_size().unwrap_or((120, 40));
    if proto::write_line(&mut stream, &json!({ "op": "attach", "name": name, "cols": cols, "rows": rows })).is_err() {
        return 1;
    }
    match proto::read_line(&mut stream) {
        Ok(v) if v["ok"].as_bool() == Some(true) => {}
        Ok(v) => {
            eprintln!("muxen-agent: {}", v["error"].as_str().unwrap_or("can't attach"));
            return 1;
        }
        Err(e) => {
            eprintln!("muxen-agent: {e}");
            return 1;
        }
    }

    let raw = Arc::new(RawMode::enable());
    let writer = Arc::new(Mutex::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return 1,
    }));

    // Session output to this terminal.
    let out_raw = raw.clone();
    let for_app_out = for_app;
    let mut reader = stream;
    let name_out = name.to_string();
    thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        loop {
            match proto::read_frame(&mut reader) {
                Ok((proto::DATA, bytes)) => {
                    if stdout.write_all(&bytes).and_then(|_| stdout.flush()).is_err() {
                        break;
                    }
                }
                Ok((proto::EXIT, _)) => {
                    out_raw.restore();
                    if !for_app_out {
                        let _ = write!(stdout, "\r\n[{name_out} ended]\r\n");
                        let _ = stdout.flush();
                    }
                    std::process::exit(0);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        out_raw.restore();
        std::process::exit(0);
    });

    // Follow this terminal's size.
    let resize = writer.clone();
    thread::spawn(move || {
        let mut last = (cols, rows);
        loop {
            thread::sleep(Duration::from_millis(150));
            if let Some(size) = window_size().filter(|s| *s != last) {
                last = size;
                let mut p = Vec::with_capacity(4);
                p.extend(size.0.to_be_bytes());
                p.extend(size.1.to_be_bytes());
                if proto::write_frame(&mut *resize.lock().unwrap(), proto::RESIZE, &p).is_err() {
                    break;
                }
            }
        }
    });

    // Keys to the session.
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 8192];
    let mut escape = false;
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut bytes = buf[..n].to_vec();
        if !for_app {
            // Ctrl-] then d detaches; Ctrl-] twice sends one.
            if escape {
                escape = false;
                if bytes.first() == Some(&b'd') {
                    break;
                }
            }
            if bytes.last() == Some(&0x1d) {
                bytes.pop();
                escape = true;
            }
        }
        if bytes.is_empty() {
            continue;
        }
        if proto::write_frame(&mut *writer.lock().unwrap(), proto::DATA, &bytes).is_err() {
            break;
        }
    }
    raw.restore();
    if !for_app {
        eprintln!("\r\n[detached from {name}]");
    }
    0
}
