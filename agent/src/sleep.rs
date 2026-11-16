//! Knowing when this machine sleeps and wakes, so the apps linked to it are
//! told instead of finding out by waking it.
//!
//! A sleeping Mac with "Wake for network access" on wakes for a packet sent
//! to it, into a dark wake: awake for a moment with its display off, then
//! asleep again. An app that keeps its link up (or keeps checking) wakes it
//! over and over, sees it answer and calls it online. So:
//!
//! - Before it sleeps, every link says `{"event": "sleep"}`, and the app lets
//!   go of the machine.
//! - While it's asleep, dark wakes included, a link's hello and its pings say
//!   `"asleep": true`: an app that reaches it then knows to leave it alone.
//! - When it has fully woken, links say `{"event": "awake"}`, and `listen`
//!   tells the apps that asked (direct.rs: a wake notice).
//! - `{"op": "wake_self"}` wakes it for real, as a key press would.
//!
//! macOS only for now (IOKit's system power notices: mac_power.c). On other
//! systems the machine always counts as awake.
//!
//! `MUXEN_TEST_POWER=<file>` stands in for the system in tests: the file
//! says "asleep" or "awake".

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock, mpsc};
use std::thread;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// About to sleep: said before the machine stops.
    Sleep,
    /// Fully awake again.
    Awake,
}

/// How long the listeners get to say it's going to sleep, and how long what
/// they said gets to leave the machine (and the apps' goodbyes to arrive),
/// before the machine may sleep. macOS waits 30 seconds at most.
const TELL: Duration = Duration::from_secs(1);
const LEAVE: Duration = Duration::from_millis(500);

type Listener = Arc<dyn Fn(Change) + Send + Sync>;

struct Watch {
    asleep: AtomicBool,
    watching: AtomicBool,
    next: AtomicU64,
    listeners: Mutex<Vec<(u64, Listener)>>,
    /// An app was told of this sleep (`told_an_app`).
    told: AtomicBool,
    /// During this sleep: links made to the machine (each one woke it, or
    /// found it in a dark wake), and notices from the system that it woke
    /// which turned out to be for a dark wake. Said in the log when it wakes.
    reached: AtomicU64,
    dark_notices: AtomicU64,
    /// When this sleep began (seconds since 1970).
    since: AtomicU64,
}

fn watch() -> &'static Watch {
    static WATCH: OnceLock<Watch> = OnceLock::new();
    WATCH.get_or_init(|| Watch {
        asleep: AtomicBool::new(false),
        watching: AtomicBool::new(false),
        next: AtomicU64::new(1),
        listeners: Mutex::new(Vec::new()),
        told: AtomicBool::new(false),
        reached: AtomicU64::new(0),
        dark_notices: AtomicU64::new(0),
        since: AtomicU64::new(0),
    })
}

/// A link was made to this machine while it's asleep (counted for the log).
pub fn reached_asleep() {
    watch().reached.fetch_add(1, Ordering::Relaxed);
}

/// A listener says it told an app the machine is going to sleep: the sleep
/// then waits a moment for that to leave (and for the app's goodbye).
pub fn told_an_app() {
    watch().told.store(true, Ordering::SeqCst);
}

fn test_file() -> Option<PathBuf> {
    std::env::var_os("MUXEN_TEST_POWER").map(PathBuf::from)
}

/// Whether this build can tell (so an app knows a wake will be said).
pub fn supported() -> bool {
    cfg!(any(target_os = "macos", test)) || test_file().is_some()
}

/// Starts watching, once a process: `link` and `listen` do, since they talk
/// to apps for as long as they run.
pub fn start() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let w = watch();
        // This crate's tests say when (`test_change`), not the computer they run on.
        if cfg!(test) {
            w.watching.store(true, Ordering::SeqCst);
            return;
        }
        if let Some(file) = test_file() {
            let read = move || std::fs::read_to_string(&file).is_ok_and(|t| t.trim() == "asleep");
            w.asleep.store(read(), Ordering::SeqCst);
            w.watching.store(true, Ordering::SeqCst);
            thread::spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(50));
                    changed(if read() { Change::Sleep } else { Change::Awake });
                }
            });
            return;
        }
        #[cfg(target_os = "macos")]
        {
            // Started in a dark wake (an app's packet woke the Mac, and ssh
            // started this): asleep until it's fully awake.
            let dark = mac::fully_awake() == Some(false);
            w.asleep.store(dark, Ordering::SeqCst);
            if mac::watch() {
                w.watching.store(true, Ordering::SeqCst);
            }
            if dark {
                until_fully_awake();
            }
        }
    });
}

/// Whether the machine is asleep: about to sleep, sleeping, or in a dark wake.
pub fn asleep() -> bool {
    let w = watch();
    if w.watching.load(Ordering::SeqCst) {
        return w.asleep.load(Ordering::SeqCst);
    }
    // Not watching (a one-off `ctl`): what the system says now.
    if let Some(file) = test_file() {
        return std::fs::read_to_string(file).is_ok_and(|t| t.trim() == "asleep");
    }
    #[cfg(target_os = "macos")]
    {
        mac::fully_awake() == Some(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Adds what an app wants to know to a hello or a ping's reply: `"sleeps"`
/// (this agent says when the machine sleeps and wakes) and, while it's
/// asleep, `"asleep"` (and `"lid_closed"` for a laptop whose lid is).
pub fn describe(message: &mut Value) {
    if !supported() {
        return;
    }
    message["sleeps"] = json!(true);
    if asleep() {
        message["asleep"] = json!(true);
        if lid_closed() {
            message["lid_closed"] = json!(true);
        }
    }
}

/// Marks an answer given while the machine is asleep. Every answer on a link
/// has it then, so an app that missed the word that it was going to sleep
/// (or never got it) learns from whatever it asks next.
pub fn mark(reply: &mut Value) {
    if supported() && reply.is_object() && asleep() {
        reply["asleep"] = json!(true);
    }
}

/// A laptop with its lid closed: it can't stay awake without a display.
pub fn lid_closed() -> bool {
    #[cfg(target_os = "macos")]
    {
        test_file().is_none() && !cfg!(test) && mac::lid_closed()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Stops being told when dropped.
pub struct Listening(u64);

impl Drop for Listening {
    fn drop(&mut self) {
        watch().listeners.lock().unwrap().retain(|(id, _)| *id != self.0);
    }
}

/// Calls `f` when the machine is about to sleep and when it has fully woken.
/// A sleep waits for `f` to return, a second at most.
pub fn listen(f: impl Fn(Change) + Send + Sync + 'static) -> Listening {
    let w = watch();
    let id = w.next.fetch_add(1, Ordering::Relaxed);
    w.listeners.lock().unwrap().push((id, Arc::new(f)));
    Listening(id)
}

fn changed(to: Change) {
    let w = watch();
    let asleep = to == Change::Sleep;
    if w.asleep.swap(asleep, Ordering::SeqCst) == asleep {
        return;
    }
    // One line a sleep and one a wake, where `listen` keeps its log: what
    // happened in between tells whether something kept waking the machine.
    let now = crate::now().max(0) as u64;
    if asleep {
        w.since.store(now, Ordering::Relaxed);
        w.reached.store(0, Ordering::Relaxed);
        w.dark_notices.store(0, Ordering::Relaxed);
        eprintln!("{now} muxen-agent: going to sleep");
    } else {
        let slept = now.saturating_sub(w.since.load(Ordering::Relaxed));
        eprintln!("{now} muxen-agent: awake after {slept} s; reached {} times in its sleep; {} wake notices for a dark wake", w.reached.load(Ordering::Relaxed), w.dark_notices.load(Ordering::Relaxed));
    }
    let listeners: Vec<Listener> = w.listeners.lock().unwrap().iter().map(|(_, f)| f.clone()).collect();
    if listeners.is_empty() {
        return;
    }
    // On a thread of their own: a link stuck writing to an app that's gone
    // doesn't hold up the machine's sleep.
    w.told.store(false, Ordering::SeqCst);
    let (done, told) = mpsc::channel();
    thread::spawn(move || {
        for f in listeners {
            f(to);
        }
        let _ = done.send(());
    });
    if asleep {
        let _ = told.recv_timeout(TELL);
        // With no app linked, there's nothing to wait for.
        if w.told.load(Ordering::SeqCst) {
            thread::sleep(LEAVE);
        }
    }
}

/// The machine woke, maybe only into a dark wake: it's awake once its
/// display may come on. Looked at every second while it runs (it doesn't
/// while the machine sleeps), until then.
#[cfg(target_os = "macos")]
fn until_fully_awake() {
    static LOOKING: AtomicBool = AtomicBool::new(false);
    if LOOKING.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(|| {
        while watch().asleep.load(Ordering::SeqCst) {
            if mac::fully_awake() == Some(true) {
                changed(Change::Awake);
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
        LOOKING.store(false, Ordering::SeqCst);
    });
}

/// `{"op": "wake_self"}`: wakes this machine for real, as a key press would
/// (a dark wake becomes a full one and the display turns on), and says
/// whether it's awake now. A laptop with its lid closed and no display can't
/// be: `lid_closed` says so.
pub fn wake_self() -> Value {
    if !asleep() {
        return json!({ "ok": true, "awake": true });
    }
    #[cfg(target_os = "macos")]
    if test_file().is_none() && !cfg!(test) {
        if !mac::wake() {
            return json!({ "ok": false, "error": "macOS didn't take the wake." });
        }
        // A full wake takes a second or two.
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && mac::fully_awake() != Some(true) {
            thread::sleep(Duration::from_millis(200));
        }
        let awake = mac::fully_awake() == Some(true);
        if awake {
            // The apps hear it now, not when macOS's own notice comes.
            changed(Change::Awake);
        }
        return json!({ "ok": true, "awake": awake, "lid_closed": mac::lid_closed() });
    }
    json!({ "ok": true, "awake": !asleep() })
}

/// Tests that put the machine to sleep take turns: there's one watch a
/// process, and every link in it hears.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    start();
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub(crate) fn test_change(to: Change) {
    changed(to);
}

#[cfg(target_os = "macos")]
mod mac {
    use super::Change;

    unsafe extern "C" {
        fn muxen_power_watch(handler: extern "C" fn(i32)) -> i32;
        fn muxen_power_capabilities() -> i32;
        fn muxen_power_lid_closed() -> i32;
        fn muxen_power_wake() -> i32;
    }

    const CPU: i32 = 1;
    const GRAPHICS: i32 = 2;

    /// macOS's notices reach apps when the Mac leaves a full wake and when it
    /// reaches one again; a dark wake in between isn't said.
    extern "C" fn handler(event: i32) {
        match event {
            1 => super::changed(Change::Sleep),
            // Awake, unless this is a dark wake after all: then once it's a full one.
            2 if fully_awake() == Some(false) => {
                super::watch().dark_notices.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                super::until_fully_awake()
            }
            2 => super::changed(Change::Awake),
            _ => {}
        }
    }

    pub fn watch() -> bool {
        unsafe { muxen_power_watch(handler) == 1 }
    }

    /// A full wake (the display may be on), not a dark one. None when macOS
    /// doesn't say.
    pub fn fully_awake() -> Option<bool> {
        let capabilities = unsafe { muxen_power_capabilities() };
        (capabilities >= 0 && capabilities & CPU != 0).then_some(capabilities & GRAPHICS != 0)
    }

    pub fn lid_closed() -> bool {
        unsafe { muxen_power_lid_closed() == 1 }
    }

    pub fn wake() -> bool {
        unsafe { muxen_power_wake() == 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_sleep_and_wake_once_each() {
        let _turn = test_lock();
        assert!(supported() && !asleep());
        let (tx, heard) = mpsc::channel();
        let listening = listen(move |change| {
            let _ = tx.send(change);
        });
        let mut hello = json!({ "event": "hello" });
        describe(&mut hello);
        assert_eq!((hello["sleeps"].as_bool(), hello.get("asleep")), (Some(true), None));

        // Said before the machine may sleep: the listener has heard by the
        // time this returns. Not said again while it sleeps.
        test_change(Change::Sleep);
        assert_eq!(heard.try_recv(), Ok(Change::Sleep));
        assert!(asleep());
        test_change(Change::Sleep);
        assert!(heard.recv_timeout(Duration::from_millis(100)).is_err());
        let mut ping = json!({ "ok": true });
        describe(&mut ping);
        assert_eq!(ping["asleep"], true);
        // Asked to wake, it says where it stands (a test doesn't wake anything).
        assert_eq!(wake_self()["awake"], false);

        test_change(Change::Awake);
        assert_eq!(heard.recv_timeout(Duration::from_secs(5)), Ok(Change::Awake));
        assert!(!asleep());
        assert_eq!(wake_self()["awake"], true);

        // A listener let go hears nothing more.
        drop(listening);
        test_change(Change::Sleep);
        test_change(Change::Awake);
        assert!(heard.try_recv().is_err());
    }
}
