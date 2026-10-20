//! The session server. Each session is a PTY running the user's login shell
//! (with an agent CLI in it, or not). Output is parsed into a terminal state, so
//! a client attaching later gets the current screen, and streamed live to every
//! attached client. Sessions stay until stopped; an exited program leaves its
//! last screen up (like tmux's remain-on-exit) until it's restarted or stopped.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};

use crate::sys::{self, Stream};
use crate::{detect, now, proto, screen};

/// With no sessions and no connections for this long, the server exits.
const IDLE_EXIT: Duration = Duration::from_secs(120);
const MAX_EVENTS: usize = 200;
const SCROLLBACK: usize = 10_000;
/// A stalled client is dropped rather than holding up the session.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
struct Listener {
    bells: Arc<AtomicUsize>,
    /// The terminal's answers to queries (cursor position, colors, ...).
    replies: Arc<Mutex<Vec<String>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Bell => {
                self.bells.fetch_add(1, Ordering::Relaxed);
            }
            Event::PtyWrite(s) => self.replies.lock().unwrap().push(s),
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

struct Session {
    name: String,
    agent: String,
    title: String,
    cwd: PathBuf,
    run: String,
    created: i64,
    inner: Mutex<Inner>,
}

struct Inner {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    writer: Option<Box<dyn Write + Send>>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    clients: Vec<Client>,
    cols: u16,
    rows: u16,
    exit: Option<i32>,
    last_output: i64,
    /// Bumped on every (re)start so an old reader thread stops quietly.
    generation: u64,
}

/// An attached terminal.
struct Client {
    id: u64,
    stream: Stream,
    /// Only watches: what it sends is ignored (a shared session's guest).
    watch_only: bool,
    /// Wants SIZE frames, to draw at the session's size rather than set it.
    sizes: bool,
}

impl Inner {
    fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.clamp(10, 1000), rows.clamp(4, 500));
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.term.resize(Dims { cols: cols as usize, rows: rows as usize });
        if let Some(m) = &self.master {
            let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
        let size = self.size_frame();
        self.clients.retain_mut(|c| !c.sizes || proto::write_frame(&mut c.stream, proto::SIZE, &size).is_ok());
    }

    fn size_frame(&self) -> [u8; 4] {
        let (c, r) = (self.cols.to_be_bytes(), self.rows.to_be_bytes());
        [c[0], c[1], r[0], r[1]]
    }

    fn broadcast(&mut self, kind: u8, payload: &[u8]) {
        self.clients.retain_mut(|c| proto::write_frame(&mut c.stream, kind, payload).is_ok());
    }
}

struct State {
    sessions: Mutex<Vec<Arc<Session>>>,
    events: Mutex<VecDeque<Value>>,
    connections: AtomicUsize,
    next_client: AtomicU64,
    last_activity: Mutex<Instant>,
}

impl State {
    fn find(&self, name: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().iter().find(|s| s.name == name).cloned()
    }

    fn event(&self, kind: &str, session: &str, detail: String) {
        let mut events = self.events.lock().unwrap();
        events.push_back(json!({ "at": now(), "kind": kind, "session": session, "detail": detail }));
        while events.len() > MAX_EVENTS {
            events.pop_front();
        }
    }
}

pub fn serve() -> i32 {
    sys::ignore_hangup();
    if sys::connect().is_ok() {
        return 0; // already running
    }
    let listener = match sys::Listener::bind() {
        Ok(l) => Arc::new(l),
        Err(e) => {
            eprintln!("muxen-agent: can't listen for sessions: {e}");
            return 1;
        }
    };
    eprintln!("muxen-agent {} serving sessions", crate::VERSION);
    let state = Arc::new(State {
        sessions: Mutex::new(Vec::new()),
        events: Mutex::new(VecDeque::new()),
        connections: AtomicUsize::new(0),
        next_client: AtomicU64::new(1),
        last_activity: Mutex::new(Instant::now()),
    });

    // Exit once nothing has been running or connected for a while.
    let idle = state.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(10));
        let busy = !idle.sessions.lock().unwrap().is_empty() || idle.connections.load(Ordering::Relaxed) > 0;
        if busy {
            *idle.last_activity.lock().unwrap() = Instant::now();
        } else if idle.last_activity.lock().unwrap().elapsed() > IDLE_EXIT {
            sys::unlisten();
            std::process::exit(0);
        }
    });

    loop {
        let Ok(mut stream) = listener.accept() else { continue };
        let (state, listener) = (state.clone(), listener.clone());
        thread::spawn(move || {
            if !listener.admit(&mut stream) {
                return;
            }
            state.connections.fetch_add(1, Ordering::Relaxed);
            handle(&state, stream);
            state.connections.fetch_sub(1, Ordering::Relaxed);
            *state.last_activity.lock().unwrap() = Instant::now();
        });
    }
}

fn handle(state: &Arc<State>, mut stream: Stream) {
    let Ok(req) = proto::read_line(&mut stream) else { return };
    let op = req["op"].as_str().unwrap_or("");
    if op == "attach" {
        attach(state, stream, &req);
        return;
    }
    let reply = match op {
        "ping" => Ok(json!({ "version": crate::VERSION })),
        "list" => Ok(list(state)),
        "start" => start(state, &req),
        "stop" => stop(state, &req),
        "restart" => restart(state, &req),
        "send" => send(state, &req),
        "read" => read(state, &req),
        other => Err(format!("unknown request \"{other}\"")),
    };
    let reply = match reply {
        Ok(mut v) => {
            v["ok"] = json!(true);
            v
        }
        Err(e) => json!({ "ok": false, "error": e }),
    };
    let _ = proto::write_line(&mut stream, &reply);
}

fn str_of<'a>(req: &'a Value, key: &str) -> &'a str {
    req[key].as_str().unwrap_or("")
}

fn session_json(s: &Session) -> Value {
    let i = s.inner.lock().unwrap();
    json!({
        "name": s.name,
        "agent": s.agent,
        "title": s.title,
        "cwd": s.cwd.display().to_string(),
        "created": s.created,
        "status": if i.exit.is_some() || i.child.is_none() { "exited" } else { "running" },
        "exit_code": i.exit,
        "last_output": i.last_output,
        "clients": i.clients.len(),
    })
}

fn list(state: &State) -> Value {
    let sessions: Vec<Value> = state.sessions.lock().unwrap().iter().map(|s| session_json(s)).collect();
    let events: Vec<Value> = state.events.lock().unwrap().iter().rev().take(60).rev().cloned().collect();
    json!({ "sessions": sessions, "events": events, "now": now() })
}

fn start(state: &Arc<State>, req: &Value) -> Result<Value, String> {
    let name = str_of(req, "name").to_string();
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return Err("a session needs a name without slashes".into());
    }
    if state.find(&name).is_some() {
        return Err(format!("there's already a session called {name}"));
    }
    let cwd = sys::expand_home(str_of(req, "cwd"));
    if !cwd.is_dir() {
        return Err(format!("{} isn't a folder on this machine.", cwd.display()));
    }
    let cols = req["cols"].as_u64().unwrap_or(120).clamp(10, 1000) as u16;
    let rows = req["rows"].as_u64().unwrap_or(40).clamp(4, 500) as u16;
    let listener = Listener::default();
    let config = Config { scrolling_history: SCROLLBACK, ..Config::default() };
    let session = Arc::new(Session {
        name,
        agent: str_of(req, "agent").to_string(),
        title: str_of(req, "title").to_string(),
        cwd,
        run: str_of(req, "run").to_string(),
        created: now(),
        inner: Mutex::new(Inner {
            term: Term::new(config, &Dims { cols: cols as usize, rows: rows as usize }, listener.clone()),
            parser: Processor::new(),
            listener,
            writer: None,
            master: None,
            child: None,
            clients: Vec::new(),
            cols,
            rows,
            exit: None,
            last_output: now(),
            generation: 0,
        }),
    });
    launch(state, &session).map_err(|e| format!("couldn't start it: {e}"))?;
    state.sessions.lock().unwrap().push(session.clone());
    Ok(json!({ "session": session_json(&session) }))
}

/// Starts (or restarts) the session's program in a new PTY.
fn launch(state: &Arc<State>, s: &Arc<Session>) -> anyhow::Result<()> {
    let (cols, rows) = {
        let i = s.inner.lock().unwrap();
        (i.cols, i.rows)
    };
    let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
    // The user's own shell as an interactive login shell, so the agent sees
    // the same PATH and setup as in their terminal (PowerShell on Windows,
    // with its profile).
    let shell = detect::user_shell();
    let mut cmd = CommandBuilder::new(&shell);
    if cfg!(windows) {
        cmd.arg("-NoLogo");
        if !s.run.is_empty() {
            cmd.args(["-Command", s.run.as_str()]);
        }
    } else {
        if s.run.is_empty() {
            cmd.arg("-l");
        } else {
            cmd.args(["-l", "-i", "-c", s.run.as_str()]);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("SHELL", &shell);
        // On a Mac the agent has no locale under launchd (direct connections)
        // or when an app opened from the Dock started it: Terminal.app sets
        // one itself. Without it, bash drops an é as it's typed and ls shows ?.
        #[cfg(target_os = "macos")]
        if let Some((name, value)) = default_locale(&cmd, has_c_utf8()) {
            cmd.env(name, value);
        }
    }
    cmd.cwd(&s.cwd);
    cmd.env("COLORTERM", "truecolor");
    cmd.env("MUXEN_SESSION", &s.name);
    cmd.env_remove("TMUX");
    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let generation = {
        let mut i = s.inner.lock().unwrap();
        i.generation += 1;
        i.writer = Some(writer);
        i.master = Some(pair.master);
        i.child = Some(child);
        i.exit = None;
        i.last_output = now();
        i.generation
    };
    let (state, s) = (state.clone(), s.clone());
    thread::spawn(move || pump(&state, &s, reader, generation));
    Ok(())
}

/// The locale a Mac session gets when the agent has none (no LC_ALL, LC_CTYPE
/// or LANG): LANG=C.UTF-8 where the Mac has it (macOS 27's /etc/zprofile sets
/// it too), else LC_CTYPE=UTF-8, since a Mac's UTF-8 is only a character set.
/// C.UTF-8 first: ssh passes it on to Linux machines, which don't know UTF-8.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn default_locale(cmd: &CommandBuilder, has_c_utf8: bool) -> Option<(&'static str, &'static str)> {
    if ["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|v| cmd.get_env(v).is_some_and(|x| !x.is_empty())) {
        return None;
    }
    Some(if has_c_utf8 { ("LANG", "C.UTF-8") } else { ("LC_CTYPE", "UTF-8") })
}

/// Whether this Mac's C library knows the C.UTF-8 locale.
#[cfg(target_os = "macos")]
fn has_c_utf8() -> bool {
    unsafe {
        let locale = libc::newlocale(libc::LC_ALL_MASK, c"C.UTF-8".as_ptr(), std::ptr::null_mut());
        if !locale.is_null() {
            libc::freelocale(locale);
        }
        !locale.is_null()
    }
}

/// Reads the program's output into the terminal state and out to clients.
fn pump(state: &State, s: &Session, mut reader: Box<dyn Read + Send>, generation: u64) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let bytes = &buf[..n];
        let mut guard = s.inner.lock().unwrap();
        if guard.generation != generation {
            return;
        }
        let i = &mut *guard;
        let bells = i.listener.bells.load(Ordering::Relaxed);
        i.parser.advance(&mut i.term, bytes);
        i.last_output = now();
        // Attached terminals answer queries themselves; without one that
        // can (watchers' answers are ignored), answer here so programs
        // waiting on a reply don't hang.
        let replies = std::mem::take(&mut *i.listener.replies.lock().unwrap());
        if i.clients.iter().all(|c| c.watch_only) {
            if let Some(w) = i.writer.as_mut() {
                for r in replies {
                    let _ = w.write_all(r.as_bytes());
                }
            }
        }
        i.broadcast(proto::DATA, bytes);
        let rang = i.listener.bells.load(Ordering::Relaxed) != bells;
        drop(guard);
        if rang {
            state.event("bell", &s.name, String::new());
        }
    }

    // The program closed the terminal: collect its exit status outside the lock.
    let child = {
        let mut i = s.inner.lock().unwrap();
        if i.generation != generation {
            return;
        }
        i.child.take()
    };
    let code = child.map(|mut c| wait_briefly(&mut c)).unwrap_or(-1);
    let mut i = s.inner.lock().unwrap();
    if i.generation != generation {
        return;
    }
    i.exit = Some(code);
    i.writer = None;
    i.master = None;
    // A shell that exits, or an agent that quits cleanly, was closed on
    // purpose: the session goes away like a closed terminal tab.
    if s.agent == "shell" || code == 0 {
        i.broadcast(proto::EXIT, &code.to_be_bytes());
        i.clients.clear();
        drop(i);
        state.sessions.lock().unwrap().retain(|x| x.name != s.name);
        state.event("closed", &s.name, code.to_string());
        return;
    }
    // A crash stays on screen, with a note, until it's restarted or stopped.
    let note = format!("\r\n\x1b[0;2m[exited with code {code}]\x1b[0m");
    let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
    parser.advance(&mut i.term, note.as_bytes());
    i.broadcast(proto::DATA, note.as_bytes());
    drop(i);
    state.event("exited", &s.name, code.to_string());
}

fn wait_briefly(child: &mut Box<dyn Child + Send + Sync>) -> i32 {
    for _ in 0..40 {
        if let Ok(Some(status)) = child.try_wait() {
            return status.exit_code() as i32;
        }
        thread::sleep(Duration::from_millis(50));
    }
    // Still running without its terminal: end it.
    let _ = child.kill();
    child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1)
}

fn kill(i: &mut Inner) {
    if let Some(mut c) = i.child.take() {
        let _ = c.kill();
    }
    i.writer = None;
    i.master = None;
}

fn stop(state: &State, req: &Value) -> Result<Value, String> {
    let name = str_of(req, "name");
    let s = state.find(name).ok_or_else(|| format!("no session called {name}"))?;
    {
        let mut i = s.inner.lock().unwrap();
        i.generation += 1;
        kill(&mut i);
        i.broadcast(proto::EXIT, &0i32.to_be_bytes());
        i.clients.clear();
    }
    state.sessions.lock().unwrap().retain(|x| x.name != name);
    Ok(json!({}))
}

fn restart(state: &Arc<State>, req: &Value) -> Result<Value, String> {
    let name = str_of(req, "name");
    let s = state.find(name).ok_or_else(|| format!("no session called {name}"))?;
    {
        let mut i = s.inner.lock().unwrap();
        i.generation += 1;
        kill(&mut i);
        // A clean screen for the new run, here and in attached terminals.
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        parser.advance(&mut i.term, b"\x1bc");
        i.broadcast(proto::DATA, b"\x1bc");
    }
    launch(state, &s).map_err(|e| format!("couldn't restart it: {e}"))?;
    Ok(json!({ "session": session_json(&s) }))
}

fn send(state: &State, req: &Value) -> Result<Value, String> {
    let name = str_of(req, "name");
    let s = state.find(name).ok_or_else(|| format!("no session called {name}"))?;
    let text = str_of(req, "text");
    let enter = req["enter"].as_bool().unwrap_or(true);
    {
        let mut i = s.inner.lock().unwrap();
        let w = i.writer.as_mut().ok_or_else(|| format!("{name} has exited"))?;
        w.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    if enter {
        // Enter on its own, a moment later: typed together, agents' input
        // boxes (Claude Code's) take it as part of a paste, a new line
        // rather than "send".
        if !text.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        let mut i = s.inner.lock().unwrap();
        let w = i.writer.as_mut().ok_or_else(|| format!("{name} has exited"))?;
        w.write_all(b"\r").map_err(|e| e.to_string())?;
    }
    Ok(json!({}))
}

fn read(state: &State, req: &Value) -> Result<Value, String> {
    let name = str_of(req, "name");
    let s = state.find(name).ok_or_else(|| format!("no session called {name}"))?;
    let lines = req["lines"].as_u64().unwrap_or(200) as usize;
    let i = s.inner.lock().unwrap();
    Ok(json!({ "text": screen::text(&i.term, lines) }))
}

/// Connects a client terminal: resize to it, send the current screen, then
/// stream both ways until it goes away. `"watch_only": true` attaches one
/// whose keys and size are ignored (it keeps the session's size), and
/// `"sizes": true` sends it the size as it changes (SIZE frames).
fn attach(state: &State, mut stream: Stream, req: &Value) {
    let name = str_of(req, "name");
    let Some(s) = state.find(name) else {
        let _ = proto::write_line(&mut stream, &json!({ "ok": false, "error": format!("no session called {name}") }));
        return;
    };
    let id = state.next_client.fetch_add(1, Ordering::Relaxed);
    let watch_only = req["watch_only"].as_bool() == Some(true);
    let sizes = req["sizes"].as_bool() == Some(true);
    {
        let mut i = s.inner.lock().unwrap();
        // The newest terminal sets the size, like tmux's window-size latest.
        if !watch_only {
            let cols = req["cols"].as_u64().unwrap_or(i.cols as u64) as u16;
            let rows = req["rows"].as_u64().unwrap_or(i.rows as u64) as u16;
            i.resize(cols, rows);
        }
        if proto::write_line(&mut stream, &json!({ "ok": true, "cols": i.cols, "rows": i.rows })).is_err() {
            return;
        }
        if sizes && proto::write_frame(&mut stream, proto::SIZE, &i.size_frame()).is_err() {
            return;
        }
        let redraw = screen::redraw(&i.term);
        if proto::write_frame(&mut stream, proto::DATA, &redraw).is_err() {
            return;
        }
        let Ok(out) = stream.try_clone() else { return };
        let _ = out.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        i.clients.push(Client { id, stream: out, watch_only, sizes });
    }
    loop {
        match proto::read_frame(&mut stream) {
            Ok(_) if watch_only => {}
            Ok((proto::DATA, bytes)) => {
                let mut i = s.inner.lock().unwrap();
                if let Some(w) = i.writer.as_mut() {
                    let _ = w.write_all(&bytes);
                }
            }
            Ok((proto::RESIZE, p)) if p.len() == 4 => {
                let cols = u16::from_be_bytes([p[0], p[1]]);
                let rows = u16::from_be_bytes([p[2], p[3]]);
                s.inner.lock().unwrap().resize(cols, rows);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    s.inner.lock().unwrap().clients.retain(|c| c.id != id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session's command with only these of the locale variables.
    fn with_locale(vars: &[(&str, &str)]) -> CommandBuilder {
        let mut cmd = CommandBuilder::new("sh");
        for v in ["LC_ALL", "LC_CTYPE", "LANG"] {
            cmd.env_remove(v);
        }
        for (k, v) in vars {
            cmd.env(k, v);
        }
        cmd
    }

    #[test]
    fn mac_sessions_get_a_utf8_locale_unless_one_is_set() {
        // launchd's environment, as `listen` has for direct connections.
        assert_eq!(default_locale(&with_locale(&[]), true), Some(("LANG", "C.UTF-8")));
        assert_eq!(default_locale(&with_locale(&[]), false), Some(("LC_CTYPE", "UTF-8")));
        // An empty LANG is none, and LC_MESSAGES alone doesn't pick a character set.
        assert_eq!(default_locale(&with_locale(&[("LANG", ""), ("LC_MESSAGES", "fr_FR.UTF-8")]), true), Some(("LANG", "C.UTF-8")));
        // A locale the agent was given (ssh brings the terminal's) stays, even C.
        for set in [("LANG", "en_US.UTF-8"), ("LC_CTYPE", "UTF-8"), ("LC_ALL", "C")] {
            assert_eq!(default_locale(&with_locale(&[set]), true), None, "{set:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_mac_knows_the_locale_it_would_give() {
        let (name, value) = default_locale(&with_locale(&[]), has_c_utf8()).unwrap();
        // LANG sets every category, LC_CTYPE only the character set.
        let mask = if name == "LANG" { libc::LC_ALL_MASK } else { libc::LC_CTYPE_MASK };
        let value = std::ffi::CString::new(value).unwrap();
        let locale = unsafe { libc::newlocale(mask, value.as_ptr(), std::ptr::null_mut()) };
        assert!(!locale.is_null(), "{name}={value:?}");
        unsafe { libc::freelocale(locale) };
    }
}
