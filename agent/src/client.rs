//! The agent's client commands: `ctl` (one JSON request, one JSON reply) and
//! `attach` (this terminal becomes a window onto a session).

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::sys::{self, Stream};
use crate::{detect, home_dir, now, proto};

/// Connects to the session server, starting it first when asked.
pub fn connect(start: bool) -> io::Result<Stream> {
    match sys::connect() {
        Ok(s) => Ok(s),
        Err(e) if !start => Err(e),
        Err(_) => {
            // In the background, so it outlives this ssh connection (logging to ~/.muxen/agent.log).
            sys::spawn_background(&["serve"], home_dir().join("agent.log"))?;
            let mut last = None;
            // Windows takes a moment longer (the server starts through WMI).
            for _ in 0..200 {
                thread::sleep(Duration::from_millis(30));
                match sys::connect() {
                    Ok(s) => return Ok(s),
                    Err(e) => last = Some(e),
                }
            }
            Err(last.unwrap_or_else(|| io::Error::other("the session server didn't start")))
        }
    }
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
        // What the work in each folder changed (sessions' folders).
        "git_info" => {
            let paths: Vec<&str> = req["paths"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            json!({ "ok": true, "folders": paths.iter().map(|p| crate::repos::info(p)).collect::<Vec<_>>() })
        }
        "git" => match crate::repos::act(req["path"].as_str().unwrap_or(""), req["action"].as_str().unwrap_or("")) {
            Ok(output) => json!({ "ok": true, "output": output }),
            Err(e) => json!({ "ok": false, "error": e }),
        },
        "start" => request(&req, true),
        "rename" => rename(&req),
        #[cfg(unix)]
        "displays" => crate::displays::list(),
        #[cfg(unix)]
        "display_create" => crate::displays::create(&req),
        #[cfg(unix)]
        "display_remove" => crate::displays::remove(&req),
        #[cfg(unix)]
        "display_rename" => crate::displays::rename(&req),
        #[cfg(unix)]
        "display_set" => crate::displays::set(&req),
        #[cfg(windows)]
        "displays" => windows_displays(),
        #[cfg(windows)]
        "display_create" | "display_remove" | "display_rename" | "display_set" => json!({ "ok": false, "error": "Muxen can't add or change monitors on Windows yet." }),
        // Apps' windows, to show one on its own (`window:<id>` as a display).
        #[cfg(windows)]
        "windows" => match crate::win_screen::in_desktop_session() {
            true => crate::win_screen::windows(),
            false => in_desktop(&["windows"], "Windows didn't list its windows."),
        },
        #[cfg(unix)]
        "windows" => crate::displays::in_session(&["windows".to_string()]).unwrap_or_else(|e| json!({ "ok": false, "error": e })),
        "log_sources" => crate::logs::sources(),
        "logs_read" => crate::logs::read(&req),
        "services" => crate::services::list(&req),
        "service_action" => crate::services::act(&req),
        "compose_new" => crate::services::compose_new(&req),
        "compose_describe" => crate::services::compose_describe(&req),
        "ports" => crate::ports::list(),
        "listening" => crate::ports::listening(),
        "usage" => crate::usage::usage(&req),
        "processes" => crate::procs::list(),
        "gpus" => json!({ "ok": true, "gpus": crate::procs::gpus() }),
        "process_signal" => crate::procs::signal(&req),
        "wake" => crate::wake::wake(&req),
        "schedules" => crate::schedules::list(),
        "schedule_save" => crate::schedules::save(&req),
        "schedule_delete" => crate::schedules::delete(&req),
        "schedule_run" => crate::schedules::run_now(&req),
        "schedule_log" => crate::schedules::log(&req),
        "boxes" => crate::boxes::list(),
        "box_save" => crate::boxes::save(&req),
        "box_resume" => crate::boxes::resume(&req),
        "box_pause" => crate::boxes::pause(&req),
        "box_delete" => crate::boxes::delete(&req),
        "box_shell" => crate::boxes::shell(&req),
        "box_switch" => crate::boxes::switch(&req),
        "handoff" => crate::handoff::note(&req),
        "carry_pack" => crate::carry::pack(&req),
        "carry_unpack" => crate::carry::unpack(&req),
        "carry_drop" => crate::carry::drop(&req),
        _ => request(&req, false),
    }
}

/// Windows' monitors, as the signed-in desktop sees them (sshd's session has none).
#[cfg(windows)]
fn windows_displays() -> Value {
    if crate::win_screen::in_desktop_session() {
        return crate::win_screen::displays();
    }
    in_desktop(&["displays"], "Windows didn't list its monitors.")
}

/// What `muxen-agent <args>` prints, run in the signed-in desktop session.
#[cfg(windows)]
fn in_desktop(args: &[&str], failed: &str) -> Value {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let mut stream = match crate::win_screen::start_in_desktop(&args) {
        Ok(s) => s,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut text = String::new();
    let _ = stream.read_to_string(&mut text);
    serde_json::from_str(text.trim()).unwrap_or_else(|_| json!({ "ok": false, "error": failed }))
}

/// Titles the user gave sessions, by session name (`~/.muxen/titles.json`).
/// Kept here rather than in the session server, which goes on running the
/// build it started with across agent updates, so renaming works with any.
fn titles_path() -> std::path::PathBuf {
    home_dir().join("titles.json")
}

pub(crate) fn read_titles() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(titles_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn write_titles(titles: &serde_json::Map<String, Value>) -> io::Result<()> {
    let path = titles_path();
    if titles.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, Value::Object(titles.clone()).to_string())?;
    std::fs::rename(&tmp, &path)
}

/// Shows the titles the user gave in the server's session list.
fn apply_titles(sessions: &mut Value, titles: &serde_json::Map<String, Value>) {
    for s in sessions.as_array_mut().into_iter().flatten() {
        if let Some(title) = s["name"].as_str().and_then(|n| titles.get(n)).cloned() {
            s["title"] = title;
        }
    }
}

/// Forgets the titles of sessions that are gone. True when any were.
fn prune_titles(sessions: &Value, titles: &mut serde_json::Map<String, Value>) -> bool {
    let names: Vec<&str> = sessions.as_array().into_iter().flatten().filter_map(|s| s["name"].as_str()).collect();
    let before = titles.len();
    titles.retain(|name, _| names.contains(&name.as_str()));
    titles.len() != before
}

/// `{"op": "rename", "name": "shell-ab12", "title": "API server"}`. An empty
/// title goes back to the one the session started with.
fn rename(req: &Value) -> Value {
    let name = req["name"].as_str().unwrap_or("");
    let title = req["title"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    let list = request(&json!({ "op": "list" }), false);
    let exists = list["sessions"].as_array().is_some_and(|a| a.iter().any(|s| s["name"].as_str() == Some(name)));
    if name.is_empty() || !exists {
        return json!({ "ok": false, "error": format!("there's no session called {name}") });
    }
    let mut titles = read_titles();
    if title.is_empty() {
        titles.remove(name);
    } else {
        titles.insert(name.to_string(), Value::String(title));
    }
    match write_titles(&titles) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": format!("couldn't save the title: {e}") }),
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
    let running = sys::connect().is_ok();
    let list = if running { request(&json!({ "op": "list" }), false) } else { json!({}) };
    let mut sessions = list.get("sessions").cloned().unwrap_or_else(|| json!([]));
    let boxes = crate::boxes::list_with(sessions.as_array().map(Vec::as_slice).unwrap_or_default());
    let mut titles = read_titles();
    if !titles.is_empty() {
        apply_titles(&mut sessions, &titles);
        // Only a real answer says which sessions are gone (no server: all of them).
        let answered = !running || list["ok"].as_bool() == Some(true);
        if answered && prune_titles(&sessions, &mut titles) {
            let _ = write_titles(&titles);
        }
    }
    json!({
        "ok": true,
        "version": crate::VERSION,
        "agents": agents,
        "agents_checked": !names.is_empty(),
        "gh": gh,
        "server": running,
        "now": now(),
        "sessions": sessions,
        "boxes": boxes,
        "events": list.get("events").cloned().unwrap_or_else(|| json!([])),
    })
}

/// Saves the terminal's settings and puts it in raw mode; restores on drop.
#[cfg(unix)]
struct RawMode(Option<libc::termios>);

#[cfg(unix)]
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

/// Windows: the console's modes, saved; keys go through one by one as VT
/// sequences, and output's escape sequences are drawn.
#[cfg(windows)]
struct RawMode(Option<(u32, u32)>);

#[cfg(windows)]
impl RawMode {
    fn enable() -> RawMode {
        use windows_sys::Win32::System::Console::*;
        unsafe {
            let (input, output) = (GetStdHandle(STD_INPUT_HANDLE), GetStdHandle(STD_OUTPUT_HANDLE));
            let (mut in_mode, mut out_mode) = (0u32, 0u32);
            if GetConsoleMode(input, &mut in_mode) == 0 {
                return RawMode(None);
            }
            GetConsoleMode(output, &mut out_mode);
            SetConsoleMode(input, (in_mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT);
            SetConsoleMode(output, out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            RawMode(Some((in_mode, out_mode)))
        }
    }

    fn restore(&self) {
        use windows_sys::Win32::System::Console::*;
        if let Some((in_mode, out_mode)) = self.0 {
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), in_mode);
                SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), out_mode);
            }
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(unix)]
fn window_size() -> Option<(u16, u16)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        (libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
    }
}

#[cfg(windows)]
fn window_size() -> Option<(u16, u16)> {
    use windows_sys::Win32::System::Console::*;
    unsafe {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
            return None;
        }
        let w = info.srWindow;
        let (cols, rows) = ((w.Right - w.Left + 1) as u16, (w.Bottom - w.Top + 1) as u16);
        (cols > 0).then_some((cols, rows))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_titles_show_and_gone_sessions_forget_theirs() {
        let mut sessions = json!([{ "name": "shell-ab12", "title": "Shell · home" }, { "name": "claude-3f9a", "title": "Claude Code · api" }]);
        let mut titles = serde_json::Map::new();
        titles.insert("shell-ab12".into(), json!("API server"));
        titles.insert("codex-0000".into(), json!("Gone"));
        apply_titles(&mut sessions, &titles);
        assert_eq!(sessions[0]["title"], "API server");
        assert_eq!(sessions[1]["title"], "Claude Code · api");
        assert!(prune_titles(&sessions, &mut titles));
        assert_eq!(titles.keys().collect::<Vec<_>>(), ["shell-ab12"]);
        assert!(!prune_titles(&sessions, &mut titles));
    }
}
