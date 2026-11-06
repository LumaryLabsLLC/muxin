//! The session server. Each session is a PTY running the user's login shell
//! (with an agent CLI in it, or not). Output is parsed into a terminal state, so
//! a client attaching later gets the current screen, and streamed live to every
//! attached client. Sessions stay until stopped; an exited program leaves its
//! last screen up (like tmux's remain-on-exit) until it's restarted or stopped.
//!
//! Each session is also saved to disk as it goes (saved.rs), so a server
//! that stops (a crash, a reboot) brings them back when it starts again, with
//! their programs started fresh: an agent whose conversation is known
//! continues it (see `restore`).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};

use crate::saved::Store;
use crate::sys::{self, Stream};
use crate::{detect, now, proto, screen};

/// With no sessions and no connections for this long, the server exits.
const IDLE_EXIT: Duration = Duration::from_secs(120);
const MAX_EVENTS: usize = 200;
const SCROLLBACK: usize = 10_000;
/// A stalled client is dropped rather than holding up the session.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Input waiting for the program (chunks as they came): typing faster than it
/// reads waits here, with the session free (see `feed`).
const INPUT_QUEUE: usize = 256;
/// How often sessions are saved (what changed since), and the longest a
/// printing session's saved screen lags behind what it shows.
const SAVE_EVERY: Duration = Duration::from_secs(2);
const SCREEN_EVERY: i64 = 10;
/// Scrollback saved with a session's screen (what an attach sends, too).
const SAVED_HISTORY: usize = 2000;
/// A session's program that ended this recently may have been ended by the
/// machine shutting down, a moment before this server is: what's saved of
/// it changes only after this long, so it comes back as it was.
const GRACE: i64 = 3;

/// A color query's answer, once its color is known (see `answers`).
type ColorReply = Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>;

#[derive(Clone, Default)]
struct Listener {
    bells: Arc<AtomicUsize>,
    /// The terminal's answers to queries (cursor position, device attributes, ...).
    replies: Arc<Mutex<Vec<String>>>,
    /// Color queries (OSC 10, 11, 4): which color, and how to write the answer.
    colors: Arc<Mutex<Vec<(usize, ColorReply)>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Bell => {
                self.bells.fetch_add(1, Ordering::Relaxed);
            }
            Event::PtyWrite(s) => self.replies.lock().unwrap().push(s),
            Event::ColorRequest(index, format) => self.colors.lock().unwrap().push((index, format)),
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
    /// What it runs (the user's shell on its own when empty). After it came
    /// back from a restart, its agent's command on its own (`fresh`): the
    /// task it was started with isn't given again.
    run: String,
    /// Its agent's command without a task, for coming back without its conversation.
    fresh: String,
    created: i64,
    /// Saved to disk as it goes. A box's sessions aren't: a box pauses when
    /// its machine restarts, and its agent continues when it's resumed.
    saves: bool,
    inner: Mutex<Inner>,
}

struct Inner {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    /// The end of the last output, for a query split across two reads.
    tail: Vec<u8>,
    /// State escape codes (OSC 7501) in the output, across reads.
    escapes: crate::states::Escapes,
    /// Input for the program, written to its terminal by `feed`.
    writer: Option<SyncSender<Vec<u8>>>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    clients: Vec<Client>,
    cols: u16,
    rows: u16,
    exit: Option<i32>,
    last_output: i64,
    /// The last keys or size change from someone at the terminal: what the
    /// program prints right after is an echo or a redraw, not work (alerts).
    last_input: i64,
    /// Bumped on every (re)start so an old reader thread stops quietly.
    generation: u64,
    /// The command line running now: `run`, or what continues its agent's conversation.
    command: String,
    /// How it came back after the session server restarted ("resumed",
    /// "fresh", "shell" or "exited"), and when. Until it's restarted.
    restored: Option<(&'static str, i64)>,
    /// It printed since its screen was saved, last at `screen_saved`.
    unsaved: bool,
    screen_saved: i64,
    /// What it is changed since `session.json` was saved.
    meta_unsaved: bool,
    /// When its program exited.
    exited_at: i64,
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
        self.last_input = now();
        self.meta_unsaved = true;
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
    /// Where sessions are saved (none in tests that don't look).
    store: Option<Store>,
    /// Sessions whose program ended on its own, and when: what's saved of them goes after `GRACE`.
    closing: Mutex<Vec<(String, i64)>>,
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
    // One server at a time, even when two start at once: both would bring
    // the saved sessions back, each running their programs.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(crate::home_dir().join("serve.lock"));
    if let Ok(f) = &lock
        && matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock))
    {
        return 0; // another is starting
    }
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
        store: Some(Store::home()),
        closing: Mutex::new(Vec::new()),
    });
    // What the last server here had (clients wait meanwhile: it's listening).
    restore(&state, resume_on(), conversation_of);
    let saver = state.clone();
    thread::spawn(move || loop {
        thread::sleep(SAVE_EVERY);
        save(&saver, now());
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
        "last_input": i.last_input,
        "clients": i.clients.len(),
        "restored": i.restored.map(|r| r.0),
        "restored_at": i.restored.map(|r| r.1),
    })
}

/// What's saved of a session (saved.rs: fields are only ever added).
fn meta_json(s: &Session, i: &Inner) -> Value {
    json!({
        "v": crate::saved::VERSION,
        "name": s.name,
        "agent": s.agent,
        "title": s.title,
        "cwd": s.cwd.display().to_string(),
        "run": s.run,
        "fresh": s.fresh,
        "created": s.created,
        "cols": i.cols,
        "rows": i.rows,
        "exit": i.exit,
        "restored": i.restored.map(|r| r.0),
        "restored_at": i.restored.map(|r| r.1),
        "saved": now(),
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
    let (agent, run) = (str_of(req, "agent").to_string(), str_of(req, "run").to_string());
    // Apps before this didn't say; for their agents, the command's first word is it.
    let fresh = req["fresh"].as_str().map(str::to_string).unwrap_or_else(|| fresh_line(&agent, &run));
    let session = new_session(state, name, agent, str_of(req, "title").to_string(), cwd, run, fresh, now(), cols, rows);
    launch(state, &session).map_err(|e| format!("couldn't start it: {e}"))?;
    state.sessions.lock().unwrap().push(session.clone());
    // Saved at once: a server that stops a moment later brings it back.
    if let (Some(store), true) = (&state.store, session.saves) {
        let mut i = session.inner.lock().unwrap();
        if store.write_meta(&session.name, &meta_json(&session, &i)).is_ok() {
            i.meta_unsaved = false;
        }
    }
    Ok(json!({ "session": session_json(&session) }))
}

#[allow(clippy::too_many_arguments)]
fn new_session(state: &State, name: String, agent: String, title: String, cwd: PathBuf, run: String, fresh: String, created: i64, cols: u16, rows: u16) -> Arc<Session> {
    let listener = Listener::default();
    let config = Config { scrolling_history: SCROLLBACK, ..Config::default() };
    Arc::new(Session {
        saves: state.store.is_some() && !name.starts_with("box-"),
        name,
        agent,
        title,
        cwd,
        fresh,
        created,
        inner: Mutex::new(Inner {
            term: Term::new(config, &Dims { cols: cols as usize, rows: rows as usize }, listener.clone()),
            parser: Processor::new(),
            tail: Vec::new(),
            escapes: Default::default(),
            listener,
            writer: None,
            master: None,
            child: None,
            clients: Vec::new(),
            cols,
            rows,
            exit: None,
            last_output: now(),
            last_input: 0,
            generation: 0,
            command: run.clone(),
            restored: None,
            unsaved: false,
            screen_saved: 0,
            meta_unsaved: true,
            exited_at: 0,
        }),
        run,
    })
}

/// Starts (or restarts) the session's program in a new PTY.
fn launch(state: &Arc<State>, s: &Arc<Session>) -> anyhow::Result<()> {
    let (cols, rows, run) = {
        let i = s.inner.lock().unwrap();
        (i.cols, i.rows, i.command.clone())
    };
    let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
    // The user's own shell as an interactive login shell, so the agent sees
    // the same PATH and setup as in their terminal (PowerShell on Windows,
    // with its profile).
    let shell = detect::user_shell();
    let mut cmd = CommandBuilder::new(&shell);
    if cfg!(windows) {
        cmd.arg("-NoLogo");
        if !run.is_empty() {
            cmd.args(["-Command", run.as_str()]);
        }
    } else {
        if run.is_empty() {
            cmd.arg("-l");
        } else {
            cmd.args(["-l", "-i", "-c", run.as_str()]);
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
    // Which session this is, for hooks and helpers (states.rs). MUXEN_ is
    // what older session servers set, and what Muxin's hooks look for.
    cmd.env("MUXEN_SESSION", &s.name);
    cmd.env("MUXIN_SESSION", &s.name);
    cmd.env_remove("TMUX");
    // Started from inside an agent (Muxin's CLI run by Claude Code, say), the
    // server has that agent's session in its environment: a session here isn't
    // its child (Claude Code wouldn't save its transcript).
    for (name, _) in std::env::vars_os() {
        if inherited_agent_var(&name.to_string_lossy()) {
            cmd.env_remove(name);
        }
    }
    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let (input, queued) = sync_channel(INPUT_QUEUE);
    thread::spawn(move || feed(writer, queued));
    let generation = {
        let mut i = s.inner.lock().unwrap();
        i.generation += 1;
        i.writer = Some(input);
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
        i.unsaved = true;
        // Attached terminals answer queries themselves; without one that
        // can (watchers' answers are ignored), answer here so programs
        // waiting on a reply don't hang.
        let replies = answers(&i.term, &i.listener, &i.tail, bytes);
        i.tail = bytes[bytes.len().saturating_sub(4)..].to_vec();
        // Never waiting here: this thread reads the program's output.
        if !replies.is_empty()
            && i.clients.iter().all(|c| c.watch_only)
            && let Some(w) = i.writer.as_ref()
        {
            let _ = w.try_send(replies.into_bytes());
        }
        i.broadcast(proto::DATA, bytes);
        let rang = i.listener.bells.load(Ordering::Relaxed) != bells;
        let said = i.escapes.feed(bytes);
        drop(guard);
        if rang {
            state.event("bell", &s.name, String::new());
        }
        // The program said what it's doing (the last word counts).
        if let Some(asked) = said.last() {
            crate::states::from_escape(&s.name, *asked);
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
    i.exited_at = now();
    i.meta_unsaved = true;
    i.writer = None;
    i.master = None;
    // A shell that exits, or an agent that quits cleanly, was closed on
    // purpose: the session goes away like a closed terminal tab. (What's
    // saved of it, and its agent's report, go a moment later: see `GRACE`.)
    if s.agent == "shell" || code == 0 {
        i.broadcast(proto::EXIT, &code.to_be_bytes());
        i.clients.clear();
        drop(i);
        state.sessions.lock().unwrap().retain(|x| x.name != s.name);
        state.closing.lock().unwrap().push((s.name.clone(), now()));
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

/// Writes a session's input to its program's terminal, in order, on a thread
/// of its own. A program that isn't reading its input then holds up only this
/// and whoever is typing, never the session: its output is still read (so it
/// can get on), and listing, reading and the other clients carry on. Ends when
/// the program does (the session drops its sender, or the write fails).
fn feed(mut terminal: Box<dyn Write + Send>, input: Receiver<Vec<u8>>) {
    for bytes in input {
        if terminal.write_all(&bytes).is_err() {
            break;
        }
    }
}

impl Session {
    /// Hands input to the program (see `feed`); waits only while its queue is
    /// full, with the session unlocked.
    fn input(&self, bytes: &[u8]) -> Result<(), String> {
        let gone = || format!("{} has exited", self.name);
        let writer = {
            let mut i = self.inner.lock().unwrap();
            i.last_input = now();
            i.writer.clone()
        }
        .ok_or_else(gone)?;
        writer.send(bytes.to_vec()).map_err(|_| gone())
    }
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
    crate::states::remove(name);
    if let Some(store) = &state.store {
        store.remove(name);
    }
    end_helpers(state, name);
    Ok(json!({}))
}

/// Ends the helpers a session started (helpers.rs), and theirs: it ended.
/// The ones started with `keep` go on.
fn end_helpers(state: &State, parent: &str) {
    let helpers = crate::helpers::all();
    if helpers.is_empty() {
        return;
    }
    if helpers.contains_key(parent) {
        crate::helpers::forget(parent);
    }
    for name in crate::helpers::ending_with(&helpers, parent) {
        if state.find(&name).is_some() {
            state.event("closed", &name, "its parent ended".into());
            let _ = stop(state, &json!({ "name": name }));
        } else {
            crate::helpers::forget(&name);
        }
    }
}

fn restart(state: &Arc<State>, req: &Value) -> Result<Value, String> {
    let name = str_of(req, "name");
    let s = state.find(name).ok_or_else(|| format!("no session called {name}"))?;
    // Its agent continues its conversation rather than starting the task again.
    let (command, how) = next_run(&s.agent, &s.run, conversation_of(name, &s.agent).as_deref(), resume_on());
    {
        let mut i = s.inner.lock().unwrap();
        i.generation += 1;
        kill(&mut i);
        // A clean screen for the new run, here and in attached terminals.
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        parser.advance(&mut i.term, b"\x1bc");
        i.broadcast(proto::DATA, b"\x1bc");
        i.escapes = Default::default();
        i.command = command;
        i.restored = None;
        i.meta_unsaved = true;
    }
    new_run(name, how == "resumed");
    launch(state, &s).map_err(|e| format!("couldn't restart it: {e}"))?;
    Ok(json!({ "session": session_json(&s) }))
}

/// A new run of a session's program: what the last one said it was doing is
/// over. Its conversation id stays when it continues that conversation; one
/// that starts a new conversation forgets it, so it isn't the one continued
/// next time.
fn new_run(name: &str, continues: bool) {
    if let Some(mut r) = crate::states::read(name) {
        r["state"] = serde_json::Value::Null;
        if let Some(o) = r.as_object_mut() {
            o.remove("pid");
            if !continues {
                o.remove("conversation");
            }
        }
        let _ = crate::states::replace(name, &r);
    }
}

/// Whether agents continue their conversations after a restart (Settings ›
/// Agents in the user's Muxins, kept here by settings.rs). On unless turned off.
fn resume_on() -> bool {
    crate::settings::on(&crate::settings::kept(), "resume", true)
}

/// The conversation a session's agent last reported (states.rs), when it was
/// that agent's own.
fn conversation_of(name: &str, agent: &str) -> Option<String> {
    let r = crate::states::read(name)?;
    let by = r["agent"].as_str().unwrap_or("");
    if !by.is_empty() && by != agent {
        return None;
    }
    r["conversation"].as_str().filter(|c| !c.is_empty()).map(str::to_string)
}

/// What a session runs when it comes back after a restart, or is restarted,
/// and which that is: its agent continuing its conversation ("resumed") when
/// that's known and `resume` allows it; else `run` ("fresh", or "shell"),
/// which after a restart is the agent on its own or a new shell.
fn next_run(agent: &str, run: &str, conversation: Option<&str>, resume: bool) -> (String, &'static str) {
    if agent == "shell" || agent.is_empty() {
        return (run.to_string(), "shell");
    }
    match conversation.filter(|_| resume).and_then(|c| resume_line(agent, c)) {
        Some(line) => (line, "resumed"),
        None => (run.to_string(), "fresh"),
    }
}

/// The command that continues an agent CLI's conversation, for the ones
/// whose hooks say which it is (hooks.rs): Claude Code, Codex, Copilot CLI,
/// Qwen Code and Kimi Code.
fn resume_line(agent: &str, conversation: &str) -> Option<String> {
    let id_like = !conversation.is_empty() && conversation.len() <= 100 && conversation.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    match agent {
        "claude" if id_like => Some(format!("claude --resume {conversation}")),
        "codex" if id_like => Some(format!("codex resume {conversation}")),
        "copilot" if id_like => Some(format!("copilot --resume={conversation}")),
        "qwen" if id_like => Some(format!("qwen --resume {conversation}")),
        "kimi" if id_like => Some(format!("kimi --session {conversation}")),
        _ => None,
    }
}

/// An agent's command on its own, from the command line it started with, for
/// apps that don't say (`fresh` in `start`): its first word, when that's the
/// agent's own name (`claude 'fix it'` → `claude`). None for a shell, or a
/// command that isn't its agent's (it comes back as the user's shell).
fn fresh_line(agent: &str, run: &str) -> String {
    match run.split_whitespace().next() {
        Some(word) if agent != "shell" && !agent.is_empty() && word.eq_ignore_ascii_case(agent) => word.to_string(),
        _ => String::new(),
    }
}

/// Saves what changed in each session (saved.rs), and forgets the ones that
/// closed a while ago. Cheap when nothing did: it only looks.
fn save(state: &State, now: i64) {
    let Some(store) = &state.store else { return };
    let sessions: Vec<Arc<Session>> = state.sessions.lock().unwrap().clone();
    for s in sessions.iter().filter(|s| s.saves) {
        let (meta, screen) = {
            let mut i = s.inner.lock().unwrap();
            // Its program just ended: perhaps the machine is shutting down.
            if i.exit.is_some() && now - i.exited_at < GRACE {
                continue;
            }
            let meta = std::mem::take(&mut i.meta_unsaved).then(|| meta_json(s, &i));
            let screen = (i.unsaved && now - i.screen_saved >= SCREEN_EVERY).then(|| {
                i.unsaved = false;
                i.screen_saved = now;
                screen::saved(&i.term, SAVED_HISTORY)
            });
            (meta, screen)
        };
        // Written outside the lock; tried again next time when they fail (a full disk).
        if let Some(bytes) = screen
            && store.write_screen(&s.name, &bytes).is_err()
        {
            s.inner.lock().unwrap().unsaved = true;
        }
        if let Some(meta) = meta
            && store.write_meta(&s.name, &meta).is_err()
        {
            s.inner.lock().unwrap().meta_unsaved = true;
        }
    }
    let due: Vec<String> = {
        let mut closing = state.closing.lock().unwrap();
        let (due, waiting): (Vec<_>, Vec<_>) = closing.drain(..).partition(|(_, at)| now - at >= GRACE);
        *closing = waiting;
        due.into_iter().map(|(name, _)| name).collect()
    };
    for name in due.iter().filter(|n| state.find(n).is_none()) {
        store.remove(name);
        crate::states::remove(name);
        // It closed (not a shutdown, or this server would be gone too): its helpers go with it.
        end_helpers(state, name);
    }
}

/// Brings back the sessions a session server here saved before it stopped
/// (this build or another's): the list, titles, folders and screens, with
/// their programs started again. An agent whose conversation is known
/// continues it when `resume` says so (Settings › Agents); others start
/// fresh, without the task they were first given, and a shell is a new
/// shell. One whose program had exited comes back as it was.
fn restore(state: &Arc<State>, resume: bool, conversation: impl Fn(&str, &str) -> Option<String>) {
    let Some(store) = state.store.clone() else { return };
    for meta in store.all() {
        let name = str_of(&meta, "name").to_string();
        if state.find(&name).is_some() {
            continue;
        }
        // A box's (no build saves those): it pauses instead.
        if name.starts_with("box-") {
            store.remove(&name);
            continue;
        }
        let agent = str_of(&meta, "agent").to_string();
        let fresh = meta["fresh"].as_str().map(str::to_string).unwrap_or_else(|| fresh_line(&agent, str_of(&meta, "run")));
        let run = if agent == "shell" || agent.is_empty() { String::new() } else { fresh.clone() };
        let cwd = sys::expand_home(str_of(&meta, "cwd"));
        let size = |key: &str, default: u64, max: u64| meta[key].as_u64().unwrap_or(default).clamp(4, max) as u16;
        let created = meta["created"].as_i64().unwrap_or_else(now);
        let session = new_session(state, name.clone(), agent.clone(), str_of(&meta, "title").to_string(), cwd.clone(), run.clone(), fresh, created, size("cols", 120, 1000).max(10), size("rows", 40, 500));
        let exited = meta["exit"].as_i64().map(|c| c as i32);
        let (command, how) = match exited {
            Some(_) => (String::new(), "exited"),
            None => next_run(&agent, &run, conversation(&name, &agent).as_deref(), resume),
        };
        let mut note = match how {
            "resumed" => "[After a restart: continuing the conversation]".to_string(),
            "fresh" => "[After a restart: a new conversation]".to_string(),
            "shell" => "[After a restart: a new shell]".to_string(),
            _ => String::new(),
        };
        if exited.is_none() && !cwd.is_dir() {
            note = format!("[After a restart: {} isn't there any more]", cwd.display());
        }
        {
            let mut i = session.inner.lock().unwrap();
            let mut screen = store.screen(&name);
            if !note.is_empty() {
                if !screen.is_empty() {
                    screen.extend_from_slice(b"\r\n");
                }
                screen.extend_from_slice(format!("\x1b[0;2m{note}\x1b[0m\r\n").as_bytes());
            }
            Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut i.term, &screen);
            i.command = command;
            i.restored = Some((how, now()));
            i.unsaved = true;
            if let Some(code) = exited {
                i.exit = Some(code);
                i.exited_at = meta["saved"].as_i64().unwrap_or(0);
                i.last_output = i.exited_at;
            }
        }
        if exited.is_none() {
            new_run(&name, how == "resumed");
            let failed = if cwd.is_dir() { launch(state, &session).err().map(|e| format!("[It didn't start again: {e}]")) } else { Some(String::new()) };
            if let Some(why) = failed {
                let mut i = session.inner.lock().unwrap();
                if !why.is_empty() {
                    Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut i.term, format!("\x1b[0;2m{why}\x1b[0m\r\n").as_bytes());
                }
                i.exit = Some(-1);
                i.restored = Some(("exited", now()));
            }
        }
        state.sessions.lock().unwrap().push(session);
    }
}

fn send(state: &State, req: &Value) -> Result<Value, String> {
    let name = str_of(req, "name");
    let s = state.find(name).ok_or_else(|| format!("no session called {name}"))?;
    let text = str_of(req, "text");
    let enter = req["enter"].as_bool().unwrap_or(true);
    s.input(text.as_bytes())?;
    if enter {
        // Enter on its own, a moment later: typed together, agents' input
        // boxes (Claude Code's) take it as part of a paste, a new line
        // rather than "send".
        if !text.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        s.input(b"\r")?;
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
                let _ = s.input(&bytes);
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

/// What the terminal says back to what the program just printed, in one
/// piece: the colors it asked for (OSC 10, 11, 4: the program's own if it set
/// them, else a dark terminal's), its name and version (XTVERSION, which the
/// terminal's parser leaves unanswered), then the parser's own answers (cursor
/// position, keyboard mode, device attributes). Device attributes come last:
/// programs (Gemini CLI, Neovim) ask for them last and take their answer as
/// "that's everything".
fn answers(term: &Term<Listener>, listener: &Listener, tail: &[u8], bytes: &[u8]) -> String {
    let mut out = String::new();
    for (index, format) in std::mem::take(&mut *listener.colors.lock().unwrap()) {
        out.push_str(&format(term.colors()[index].unwrap_or_else(|| default_color(index))));
    }
    if asks_version(tail, bytes) {
        out.push_str(&format!("\x1bP>|Muxin {}\x1b\\", env!("CARGO_PKG_VERSION")));
    }
    for r in std::mem::take(&mut *listener.replies.lock().unwrap()) {
        out.push_str(&r);
    }
    out
}

/// XTVERSION (`CSI > q` or `CSI > 0 q`), in this output or across its start.
fn asks_version(tail: &[u8], bytes: &[u8]) -> bool {
    let seen: Vec<u8> = tail.iter().chain(bytes.iter().take(5)).copied().collect();
    [&b"\x1b[>q"[..], &b"\x1b[>0q"[..]].iter().any(|q| bytes.windows(q.len()).any(|w| w == *q) || seen.windows(q.len()).any(|w| w == *q))
}

/// A dark terminal's colors, for a program that asks before setting any:
/// xterm's 256 (the 16 named, the 6×6×6 cube, the grays), then the
/// foreground, background and cursor (256, 257, 258; dim and bright ones after).
fn default_color(index: usize) -> Rgb {
    const NAMED: [(u8, u8, u8); 16] = [
        (0, 0, 0), (205, 0, 0), (0, 205, 0), (205, 205, 0), (0, 0, 238), (205, 0, 205), (0, 205, 205), (229, 229, 229),
        (127, 127, 127), (255, 0, 0), (0, 255, 0), (255, 255, 0), (92, 92, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255),
    ];
    let (r, g, b) = match index {
        0..=15 => NAMED[index],
        16..=231 => {
            let level = |n: usize| if n == 0 { 0 } else { (55 + n * 40) as u8 };
            let n = index - 16;
            (level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            (v, v, v)
        }
        257 => (0x15, 0x16, 0x1a),
        _ => (0xec, 0xec, 0xf1),
    };
    Rgb { r, g, b }
}

/// An agent's own marks of the session it runs in (Claude Code's, Codex's
/// sandbox), not the user's settings for it (CLAUDE_CONFIG_DIR, API keys).
fn inherited_agent_var(name: &str) -> bool {
    matches!(name, "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_EFFORT")
        || ["CLAUDE_CODE_", "CLAUDE_AGENT_SDK_", "CODEX_SANDBOX"].iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> Arc<State> {
        Arc::new(State {
            sessions: Mutex::new(Vec::new()),
            events: Mutex::new(VecDeque::new()),
            connections: AtomicUsize::new(0),
            next_client: AtomicU64::new(1),
            last_activity: Mutex::new(Instant::now()),
            store: None,
            closing: Mutex::new(Vec::new()),
        })
    }

    /// Asks `f` on another thread; whether it answered within `wait`.
    fn answers_within(wait: Duration, f: impl FnOnce() + Send + 'static) -> bool {
        let (done, answered) = std::sync::mpsc::channel();
        thread::spawn(move || {
            f();
            let _ = done.send(());
        });
        answered.recv_timeout(wait).is_ok()
    }

    /// A program that never reads its input, flooded with typing (a huge paste,
    /// say): the typing waits, but the session's lock is never held for it,
    /// so listing and reading it (and the other clients) carry on.
    #[cfg(unix)]
    #[test]
    fn typing_into_a_program_that_doesnt_read_never_locks_the_session() {
        let state = test_state();
        // Raw mode, like an agent's TUI: on Linux input then piles up instead
        // of being dropped (a Mac's terminal drops what it can't hold, so the
        // typing never waits there).
        let req = json!({ "name": "flood", "cwd": std::env::temp_dir().display().to_string(), "run": "stty raw -echo; echo ready; sleep 30" });
        start(&state, &req).expect("the session starts");
        let s = state.find("flood").unwrap();
        let ready = Instant::now();
        while !screen::text(&s.inner.lock().unwrap().term, 5).contains("ready") {
            assert!(ready.elapsed() < Duration::from_secs(20), "the session didn't start");
            thread::sleep(Duration::from_millis(50));
        }
        let typist = s.clone();
        thread::spawn(move || {
            let chunk = vec![b'x'; 4096];
            // Far more than a terminal's input buffer and the queue hold.
            for _ in 0..4096 {
                if typist.input(&chunk).is_err() {
                    break;
                }
            }
        });
        thread::sleep(Duration::from_millis(500));
        let st = state.clone();
        assert!(answers_within(Duration::from_secs(3), move || drop(list(&st))), "listing sessions hangs");
        let st = state.clone();
        assert!(answers_within(Duration::from_secs(3), move || drop(read(&st, &json!({ "name": "flood", "lines": 5 })))), "reading the session hangs");
        let st = state.clone();
        assert!(answers_within(Duration::from_secs(3), move || drop(stop(&st, &json!({ "name": "flood" })))), "stopping the session hangs");
    }

    fn saving_state(store: &Store) -> Arc<State> {
        let state = test_state();
        Arc::new(State { store: Some(store.clone()), ..Arc::try_unwrap(state).ok().unwrap() })
    }

    /// Waits for `text` on a session's screen.
    fn shows(s: &Session, text: &str) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if screen::text(&s.inner.lock().unwrap().term, 3000).contains(text) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// The session server dies (its programs with it), leaving what it saved.
    fn dies(state: &State) {
        for s in state.sessions.lock().unwrap().drain(..) {
            let mut i = s.inner.lock().unwrap();
            i.generation += 1;
            kill(&mut i);
        }
    }

    #[test]
    fn agents_continue_their_conversation_when_allowed_and_never_get_their_task_again() {
        assert_eq!(next_run("claude", "claude", Some("0b5e-41"), true), ("claude --resume 0b5e-41".to_string(), "resumed"));
        assert_eq!(next_run("codex", "codex", Some("019a_ff"), true), ("codex resume 019a_ff".to_string(), "resumed"));
        // Off, unknown, not an id, an agent that can't say: its own command.
        assert_eq!(next_run("claude", "claude", Some("0b5e-41"), false), ("claude".to_string(), "fresh"));
        assert_eq!(next_run("claude", "claude", None, true), ("claude".to_string(), "fresh"));
        assert_eq!(next_run("codex", "codex", Some("x; rm -rf ~"), true), ("codex".to_string(), "fresh"));
        assert_eq!(next_run("opencode", "opencode", Some("abc"), true), ("opencode".to_string(), "fresh"));
        // Ids as their hooks said them (tried with Copilot CLI 1.0, Qwen Code 0.26, Kimi Code 2.5).
        assert_eq!(next_run("copilot", "copilot", Some("320d40dc-1c11-4d16-bc64-b3a02c2fee33"), true).0, "copilot --resume=320d40dc-1c11-4d16-bc64-b3a02c2fee33");
        assert_eq!(next_run("qwen", "qwen", Some("3139440b-bae7"), true).0, "qwen --resume 3139440b-bae7");
        assert_eq!(next_run("kimi", "kimi", Some("session_671af367-6584"), true).0, "kimi --session session_671af367-6584");
        assert_eq!(next_run("droid", "droid", Some("abc"), true), ("droid".to_string(), "fresh"));
        assert_eq!(next_run("shell", "", Some("abc"), true), (String::new(), "shell"));
        // The command on its own, from what an older app started it with.
        assert_eq!(fresh_line("claude", "claude 'fix the login bug'"), "claude");
        assert_eq!(fresh_line("agy", "agy -i 'task'"), "agy");
        assert_eq!(fresh_line("codex", "codex"), "codex");
        assert_eq!(fresh_line("claude", "docker exec -it c claude"), "");
        assert_eq!(fresh_line("shell", "gh auth login"), "");
    }

    #[cfg(unix)]
    #[test]
    fn a_session_comes_back_after_its_server_stops_with_its_screen_and_without_its_task() {
        let store = crate::saved::scratch("restore");
        let dir = std::env::temp_dir().join(format!("muxen-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.canonicalize().unwrap().display().to_string();
        let a = saving_state(&store);
        // As an app starts an agent with its task: `run` has it, `fresh` doesn't.
        let req = json!({ "name": "codex-t1", "agent": "codex", "title": "Codex · t1", "cwd": cwd, "cols": 90, "rows": 20,
            "run": "echo first $((6*7)) run; exec sleep 30", "fresh": "echo fresh $((6*8)) run; exec sleep 30" });
        start(&a, &req).unwrap();
        let s = a.find("codex-t1").unwrap();
        assert!(shows(&s, "first 42 run"));
        // Saved at once, the screen once it printed.
        assert_eq!(store.all().len(), 1);
        save(&a, now());
        assert!(String::from_utf8_lossy(&store.screen("codex-t1")).contains("first 42 run"));
        a.find("codex-t1").unwrap().inner.lock().unwrap().resize(100, 24);
        save(&a, now());
        assert_eq!(store.all()[0]["cols"], 100);
        dies(&a);

        let b = saving_state(&store);
        restore(&b, false, |_, _| Some("abc-123".into()));
        let s = b.find("codex-t1").expect("it came back");
        assert_eq!((s.title.as_str(), s.cwd.display().to_string(), s.agent.as_str()), ("Codex · t1", cwd.clone(), "codex"));
        assert!(shows(&s, "fresh 48 run"), "its agent started again on its own");
        let text = screen::text(&s.inner.lock().unwrap().term, 3000);
        assert!(text.contains("first 42 run") && text.contains("[After a restart: a new conversation]"), "{text}");
        assert_eq!(text.matches("first 42 run").count(), 1, "the task isn't given again: {text}");
        let listed = session_json(&s);
        assert_eq!((listed["status"].as_str(), listed["restored"].as_str()), (Some("running"), Some("fresh")));
        let size = { let i = s.inner.lock().unwrap(); (i.cols, i.rows) };
        assert_eq!(size, (100, 24));
        // What's saved now runs its agent on its own, should it come back again.
        save(&b, now());
        assert_eq!(store.all()[0]["run"], "echo fresh $((6*8)) run; exec sleep 30");
        assert_eq!(store.all()[0]["restored"], "fresh");
        stop(&b, &json!({ "name": "codex-t1" })).unwrap();
        assert!(store.all().is_empty(), "stopping it forgets it");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[cfg(unix)]
    #[test]
    fn what_ends_as_the_machine_shuts_down_still_comes_back() {
        let store = crate::saved::scratch("grace");
        let a = saving_state(&store);
        let tmp = std::env::temp_dir().display().to_string();
        // A crash, and an agent that quit cleanly (closed on purpose).
        start(&a, &json!({ "name": "codex-crash", "agent": "codex", "cwd": tmp, "run": "sleep 0.3; exit 3" })).unwrap();
        start(&a, &json!({ "name": "codex-quit", "agent": "codex", "cwd": tmp, "run": "sleep 0.3; exit 0" })).unwrap();
        let ended = Instant::now();
        while a.find("codex-quit").is_some() || a.find("codex-crash").is_some_and(|s| s.inner.lock().unwrap().exit.is_none()) {
            assert!(ended.elapsed() < Duration::from_secs(20), "they didn't end");
            thread::sleep(Duration::from_millis(50));
        }
        // A moment later it's all as it was: the server may be next.
        save(&a, now());
        let saved = store.all();
        assert_eq!(saved.len(), 2);
        assert!(saved.iter().all(|m| m["exit"].is_null()), "{saved:?}");
        // After the grace, it was real.
        save(&a, now() + GRACE + 1);
        let saved = store.all();
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0]["name"].as_str(), saved[0]["exit"].as_i64()), (Some("codex-crash"), Some(3)));
        dies(&a);
        // A crashed one comes back as it was: exited, nothing started.
        let b = saving_state(&store);
        restore(&b, true, |_, _| None);
        let s = b.find("codex-crash").unwrap();
        let listed = session_json(&s);
        assert_eq!((listed["status"].as_str(), listed["exit_code"].as_i64(), listed["restored"].as_str()), (Some("exited"), Some(3), Some("exited")));
        assert!(s.inner.lock().unwrap().child.is_none());
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn a_folder_thats_gone_or_a_box_doesnt_start_anything() {
        let store = crate::saved::scratch("gone");
        store.write_meta("claude-gone", &json!({ "v": 1, "name": "claude-gone", "agent": "claude", "cwd": "/nonexistent/muxen-test", "run": "claude", "created": 5 })).unwrap();
        store.write_meta("box-7k2m9q", &json!({ "v": 1, "name": "box-7k2m9q", "agent": "claude", "cwd": "/tmp", "run": "docker start c", "created": 6 })).unwrap();
        let b = saving_state(&store);
        restore(&b, true, |_, _| Some("abc".into()));
        assert!(b.find("box-7k2m9q").is_none(), "a box pauses instead");
        assert_eq!(store.names(), vec!["claude-gone".to_string()]);
        let s = b.find("claude-gone").unwrap();
        let exit = s.inner.lock().unwrap().exit;
        assert_eq!(exit, Some(-1));
        let text = screen::text(&s.inner.lock().unwrap().term, 100);
        assert!(text.contains("isn't there any more"), "{text}");
        let _ = std::fs::remove_dir_all(store.root());
    }

    /// What the terminal answers to `printed`, as a session with no window would.
    fn answer(printed: &[u8]) -> String {
        let listener = Listener::default();
        let mut term = Term::new(Config::default(), &Dims { cols: 80, rows: 24 }, listener.clone());
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, printed);
        answers(&term, &listener, &[], printed)
    }

    #[test]
    fn gemini_clis_questions_are_all_answered_device_attributes_last() {
        // What Gemini CLI asks at start: keyboard mode, background color, name, device attributes.
        let got = answer(b"\x1b[?u\x1b]11;?\x1b\\\x1b[>q\x1b[c");
        assert!(got.starts_with("\x1b]11;rgb:1515/1616/1a1a"), "{got:?}");
        assert!(got.contains("\x1bP>|Muxin "), "{got:?}");
        // The kitty keyboard protocol is off in sessions, so that one has no answer, as in a terminal without it.
        assert!(!got.contains("u\x1b"), "{got:?}");
        assert!(got.ends_with("\x1b[?6c"), "{got:?}");
    }

    #[test]
    fn a_color_the_program_set_is_the_one_answered() {
        let got = answer(b"\x1b]11;#ffffff\x1b\\\x1b]11;?\x07");
        assert!(got.starts_with("\x1b]11;rgb:ffff/ffff/ffff"), "{got:?}");
        let fg = answer(b"\x1b]10;?\x07");
        assert!(fg.starts_with("\x1b]10;rgb:ecec/ecec/f1f1"), "{fg:?}");
    }

    #[test]
    fn a_version_query_split_across_reads_is_still_seen() {
        assert!(asks_version(b"xx\x1b[", b">qmore"));
        assert!(asks_version(b"", b"a\x1b[>0qb"));
        assert!(!asks_version(b"", b"\x1b[>c"));
        assert_eq!(answer(b"plain text\r\n"), "");
    }

    #[test]
    fn an_outer_agents_session_doesnt_reach_sessions() {
        for v in ["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_AGENT_SDK_VERSION", "CLAUDE_PID", "CODEX_SANDBOX_NETWORK_DISABLED"] {
            assert!(inherited_agent_var(v), "{v}");
        }
        for v in ["CLAUDE_CONFIG_DIR", "ANTHROPIC_API_KEY", "CODEX_HOME", "PATH"] {
            assert!(!inherited_agent_var(v), "{v}");
        }
    }

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
