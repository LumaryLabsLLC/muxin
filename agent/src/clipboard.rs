//! `muxen-agent clip`: this machine's clipboard, shared with the app. Frames
//! on stdin set it (text); frames on stdout say it changed (text copied here).
//! Linux: owns and watches the X clipboard (needs a desktop session). macOS:
//! pbcopy and pbpaste. A password manager's copies aren't sent (each system
//! has its own way of marking them). A copy made here that hasn't been sent
//! yet wins over text arriving from the app at the same time: the app's text
//! isn't pasted over it, and the copy goes out instead.

use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// A clipboard's text (the frame kind on the link and on stdin/stdout).
pub const CLIP: u8 = 40;
/// Bigger text isn't shared (X serves it in pieces, which this doesn't do).
pub const MAX: usize = 256 * 1024;

/// Whether a copy's `x-kde-passwordManagerHint` says it's a password, as
/// KeePassXC and other password managers on Linux mark theirs.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn secret(hint: &[u8]) -> bool {
    hint.trim_ascii() == b"secret"
}

pub fn run() -> i32 {
    // What the app sets, as it arrives.
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        while let Ok((kind, payload)) = crate::proto::read_frame(&mut stdin) {
            if kind == CLIP && tx.send(String::from_utf8_lossy(&payload).into_owned()).is_err() {
                break;
            }
        }
    });
    let changed = |text: &str| -> bool {
        let mut out = io::stdout().lock();
        crate::proto::write_frame(&mut out, CLIP, text.as_bytes()).and_then(|_| out.flush()).is_ok()
    };
    #[cfg(target_os = "linux")]
    {
        match x11::run(rx, changed) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("clipboard: {e}");
                1
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        mac::run(rx, changed);
        0
    }
    #[cfg(windows)]
    {
        crate::win_screen::clip(rx, changed);
        0
    }
}

/// A clipboard that numbers its changes: a Mac's pasteboard, Windows'.
#[cfg(any(target_os = "macos", windows, test))]
pub trait Board {
    /// The number of its last change, and whether a password manager marked
    /// what's there private.
    fn state(&mut self) -> (i64, bool);
    /// Its text, if it has some (Windows: not a password manager's).
    fn text(&mut self) -> Option<String>;
    /// Puts text there; whether that worked.
    fn set(&mut self, text: &str) -> bool;
}

/// Shares a clipboard that numbers its changes: every `every` (or as soon as
/// the app sends something) it looks for a copy made here, and pastes what
/// the app sent unless a newer copy was made here or it's there already.
#[cfg(any(target_os = "macos", windows, test))]
pub fn share(board: &mut impl Board, rx: mpsc::Receiver<String>, every: Duration, changed: impl Fn(&str) -> bool) {
    let mut sharing = Sharing::new(board);
    loop {
        let from_app = match rx.recv_timeout(every) {
            Ok(text) => Some(text),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if let Some(text) = sharing.turn(board, from_app)
            && !changed(&text)
        {
            return;
        }
    }
}

/// What `share` knows about the clipboard.
#[cfg(any(target_os = "macos", windows, test))]
struct Sharing {
    /// The number of the last change looked at.
    seen: i64,
    /// What's there as of that change, when it's text (not a password manager's).
    holds: Option<String>,
    /// The text last sent to the app or pasted from it (so nothing echoes back).
    last: String,
}

#[cfg(any(target_os = "macos", windows, test))]
impl Sharing {
    fn new(board: &mut impl Board) -> Sharing {
        // What's there now is old news; only copies from here on count.
        let (seen, private) = board.state();
        Sharing { seen, holds: if private { None } else { board.text() }, last: String::new() }
    }

    /// A look at the clipboard, with the text the app sent meanwhile: that's
    /// pasted unless a newer copy was made here. Returns a copy made here to
    /// send.
    fn turn(&mut self, board: &mut impl Board, from_app: Option<String>) -> Option<String> {
        let (newer, copied) = self.look(board);
        if let Some(text) = from_app
            && !newer
        {
            self.paste(board, text);
        }
        copied
    }

    /// Looks for a copy made here since the last look: whether there's one
    /// (newer than anything the app sends now), and its text when that's to
    /// be sent. A password manager's copy, one that isn't text, and the same
    /// text again aren't sent.
    fn look(&mut self, board: &mut impl Board) -> (bool, Option<String>) {
        let (count, private) = board.state();
        if count == self.seen {
            return (false, None);
        }
        let text = if private { None } else { board.text() };
        // Copied again while reading: that copy is looked at next time.
        if board.state().0 != count {
            return (true, None);
        }
        self.seen = count;
        self.holds = text.clone();
        match text {
            Some(t) if t == self.last => (false, None),
            Some(t) if !t.is_empty() && t.len() <= MAX => {
                self.last = t.clone();
                (true, Some(t))
            }
            _ => (true, None),
        }
    }

    /// Pastes text from the app, unless it's there already.
    fn paste(&mut self, board: &mut impl Board, text: String) {
        if self.holds.as_ref() != Some(&text) && board.set(&text) {
            self.seen = board.state().0;
            self.holds = Some(text.clone());
        }
        self.last = text;
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use std::process::{Command, Stdio};

    unsafe extern "C" {
        fn muxen_pasteboard(concealed: *mut i32) -> i64;
    }

    /// pbcopy or pbpaste, reading and writing UTF-8. Without a locale (as
    /// under launchd, for direct connections) they'd take it for Mac Roman.
    pub(super) fn pb(program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_remove("LC_ALL").env("LC_CTYPE", "UTF-8").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        cmd
    }

    struct Pasteboard;

    impl Board for Pasteboard {
        /// The change count, and whether a password manager marked what's
        /// there private (see mac_screen.m).
        fn state(&mut self) -> (i64, bool) {
            let mut concealed = 0;
            let count = unsafe { muxen_pasteboard(&mut concealed) };
            (count, concealed != 0)
        }

        fn text(&mut self) -> Option<String> {
            let out = pb("pbpaste").stdout(Stdio::piped()).output().ok()?;
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        }

        fn set(&mut self, text: &str) -> bool {
            let Ok(mut child) = pb("pbcopy").stdin(Stdio::piped()).spawn() else { return false };
            let wrote = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
            child.wait().is_ok_and(|s| s.success()) && wrote
        }
    }

    pub fn run(rx: mpsc::Receiver<String>, changed: impl Fn(&str) -> bool) {
        share(&mut Pasteboard, rx, Duration::from_millis(600), changed);
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use super::*;
    use std::time::Instant;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xfixes::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::*;
    use x11rb::wrapper::ConnectionExt as _;

    const NONE: u32 = 0;

    pub fn run(rx: mpsc::Receiver<String>, changed: impl Fn(&str) -> bool) -> Result<(), String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| e.to_string())?;
        let root = conn.setup().roots[screen_num].root;
        let win = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(0, win, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new()).map_err(|e| e.to_string())?;
        let atom = |name: &str| -> Result<u32, String> { Ok(conn.intern_atom(false, name.as_bytes()).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom) };
        let (clipboard, utf8, targets, text_atom, plain, prop, incr) =
            (atom("CLIPBOARD")?, atom("UTF8_STRING")?, atom("TARGETS")?, atom("TEXT")?, atom("text/plain;charset=utf-8")?, atom("MUXEN_CLIP")?, atom("INCR")?);
        let hint = atom("x-kde-passwordManagerHint")?;
        let string: u32 = AtomEnum::STRING.into();
        conn.xfixes_query_version(5, 0).map_err(|e| e.to_string())?.reply().map_err(|e| format!("no XFixes: {e}"))?;
        conn.xfixes_select_selection_input(win, clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER).map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;

        // Text this window offers while it owns the clipboard, and the last
        // text either way (so nothing echoes back).
        let mut owned: Option<Vec<u8>> = None;
        let mut last = String::new();
        // The clipboard's text when that's known: ours, or read from the
        // program that copied it.
        let mut holds: Option<String> = None;
        // Since when a copy made here is being read (its targets, then its text).
        let mut reading: Option<Instant> = None;
        // When the copy being asked about was made (answers about an older one are dropped).
        let mut asked_at = x11rb::CURRENT_TIME;
        let ask = |target: u32, at: u32| {
            let _ = conn.convert_selection(win, clipboard, target, prop, at);
            let _ = conn.flush();
        };
        loop {
            while let Some(event) = conn.poll_for_event().map_err(|e| e.to_string())? {
                match event {
                    // Something else took the clipboard: ask what it offers
                    // first, since a password manager marks its copies. (No
                    // owner: the program that had it quit; it's empty.)
                    Event::XfixesSelectionNotify(e) if e.owner != win => {
                        holds = None;
                        reading = None;
                        if e.owner != NONE {
                            asked_at = e.selection_timestamp;
                            reading = Some(Instant::now());
                            ask(targets, asked_at);
                        }
                    }
                    Event::SelectionNotify(e) if reading.is_some() && e.requestor == win && (e.time == asked_at || e.time == x11rb::CURRENT_TIME) => {
                        if e.property == NONE {
                            // It doesn't list what it offers: just the text.
                            if e.target == targets {
                                ask(utf8, asked_at);
                            } else {
                                reading = None;
                            }
                            continue;
                        }
                        let reply = conn.get_property(true, win, prop, AtomEnum::ANY, 0, (MAX / 4) as u32 + 1).ok().and_then(|cookie| cookie.reply().ok());
                        let Some(reply) = reply else {
                            reading = None;
                            continue;
                        };
                        if e.target == targets {
                            let offered: Vec<u32> = reply.value32().map(|atoms| atoms.collect()).unwrap_or_default();
                            ask(if offered.contains(&hint) { hint } else { utf8 }, asked_at);
                            continue;
                        }
                        if e.target == hint {
                            // A password manager's copy stays on this machine.
                            if super::secret(&reply.value) {
                                reading = None;
                            } else {
                                ask(utf8, asked_at);
                            }
                            continue;
                        }
                        reading = None;
                        if reply.type_ == incr || reply.value.len() > MAX {
                            continue;
                        }
                        let text = String::from_utf8_lossy(&reply.value).into_owned();
                        holds = Some(text.clone());
                        if text != last && !text.is_empty() {
                            if !changed(&text) {
                                return Ok(());
                            }
                            last = text;
                        }
                    }
                    // A program pastes what this window offers.
                    Event::SelectionRequest(e) => {
                        let mut property = if e.property == NONE { e.target } else { e.property };
                        match &owned {
                            _ if e.target == targets => {
                                let _ = conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &[targets, utf8, string, text_atom, plain]);
                            }
                            Some(bytes) if [utf8, string, text_atom, plain].contains(&e.target) => {
                                let ty = if e.target == string { string } else { utf8 };
                                let _ = conn.change_property8(PropMode::REPLACE, e.requestor, property, ty, bytes);
                            }
                            _ => property = NONE,
                        }
                        let notify = SelectionNotifyEvent {
                            response_type: SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: e.time,
                            requestor: e.requestor,
                            selection: e.selection,
                            target: e.target,
                            property,
                        };
                        let _ = conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify);
                        let _ = conn.flush();
                    }
                    Event::SelectionClear(e) if e.owner == win => owned = None,
                    _ => {}
                }
            }
            loop {
                match rx.try_recv() {
                    // A copy made here that's still being read is newer: it
                    // goes out instead (a program that doesn't answer gets a second).
                    Ok(_) if reading.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) => {}
                    Ok(text) => {
                        reading = None;
                        if holds.as_ref() != Some(&text) {
                            owned = Some(text.clone().into_bytes());
                            holds = Some(text.clone());
                            let _ = conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME);
                            let _ = conn.flush();
                        }
                        last = text;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    // The app let go.
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_managers_copy_says_secret() {
        assert!(super::secret(b"secret"));
        assert!(super::secret(b"secret\n"));
        assert!(!super::secret(b""));
        assert!(!super::secret(b"public"));
    }

    /// A clipboard in memory that numbers its changes, like a Mac's.
    #[derive(Default)]
    struct Fake {
        count: i64,
        text: Option<String>,
        private: bool,
        /// Texts pasted from the app.
        pasted: Vec<String>,
    }

    impl Fake {
        /// A copy made on this machine.
        fn copy(&mut self, text: Option<&str>, private: bool) {
            self.count += 1;
            self.text = text.map(str::to_string);
            self.private = private;
        }
    }

    impl Board for Fake {
        fn state(&mut self) -> (i64, bool) {
            (self.count, self.private)
        }
        fn text(&mut self) -> Option<String> {
            self.text.clone()
        }
        fn set(&mut self, text: &str) -> bool {
            self.pasted.push(text.to_string());
            self.copy(Some(text), false);
            true
        }
    }

    fn turn(sharing: &mut Sharing, board: &mut Fake, from_app: &str) -> Option<String> {
        sharing.turn(board, Some(from_app.to_string()))
    }

    #[test]
    fn a_newer_copy_here_wins_over_the_apps_text() {
        let mut board = Fake::default();
        board.copy(Some("old news"), false);
        let mut sharing = Sharing::new(&mut board);
        // Nothing copied here since the last look: the app's text is pasted,
        // and doesn't come back.
        assert_eq!(turn(&mut sharing, &mut board, "from the app"), None);
        assert_eq!(board.text.as_deref(), Some("from the app"));
        assert_eq!(sharing.turn(&mut board, None), None);
        // Copied here after that, before the app's next text arrived: the
        // copy goes out and stays here.
        board.copy(Some("copied here"), false);
        assert_eq!(turn(&mut sharing, &mut board, "from the app, later"), Some("copied here".into()));
        assert_eq!(board.text.as_deref(), Some("copied here"));
        // The app's text after that is pasted as usual.
        assert_eq!(turn(&mut sharing, &mut board, "newest"), None);
        assert_eq!(board.text.as_deref(), Some("newest"));
        assert_eq!(board.pasted, ["from the app", "newest"]);
    }

    #[test]
    fn a_copy_that_stays_here_isnt_pasted_over_either() {
        for (text, private) in [(Some("hunter2"), true), (None, false)] {
            let mut board = Fake::default();
            let mut sharing = Sharing::new(&mut board);
            // A password manager's copy, or an image: not sent, not replaced.
            board.copy(text, private);
            assert_eq!(turn(&mut sharing, &mut board, "from the app"), None);
            assert_eq!((board.text.as_deref(), board.pasted.len()), (text, 0));
            // Once it's been looked at, the app's next text replaces it.
            assert_eq!(turn(&mut sharing, &mut board, "from the app"), None);
            assert_eq!(board.text.as_deref(), Some("from the app"));
        }
    }

    #[test]
    fn the_same_text_copied_again_doesnt_count() {
        let mut board = Fake::default();
        let mut sharing = Sharing::new(&mut board);
        turn(&mut sharing, &mut board, "from the app");
        // Copied again here (or a clipboard manager took it over): it isn't
        // sent back, and doesn't keep the app's next text out.
        board.copy(Some("from the app"), false);
        assert_eq!(turn(&mut sharing, &mut board, "next"), None);
        assert_eq!(board.text.as_deref(), Some("next"));
    }

    #[test]
    fn text_thats_there_already_isnt_pasted_again() {
        let mut board = Fake::default();
        board.copy(Some("same"), false);
        let mut sharing = Sharing::new(&mut board);
        let count = board.count;
        assert_eq!(turn(&mut sharing, &mut board, "same"), None);
        assert_eq!((board.count, board.pasted.len()), (count, 0));
        // And copying it here afterwards isn't news for the app.
        board.copy(Some("same"), false);
        assert_eq!(sharing.turn(&mut board, None), None);
    }

    #[test]
    fn a_copy_made_while_reading_is_looked_at_next_time() {
        /// Copies "second" here while the first copy's text is being read.
        struct Racing(Fake);
        impl Board for Racing {
            fn state(&mut self) -> (i64, bool) {
                self.0.state()
            }
            fn text(&mut self) -> Option<String> {
                let first = self.0.text();
                if first.as_deref() == Some("first") {
                    self.0.copy(Some("second"), false);
                }
                first
            }
            fn set(&mut self, text: &str) -> bool {
                self.0.set(text)
            }
        }
        let mut board = Racing(Fake::default());
        let mut sharing = Sharing::new(&mut board);
        board.0.copy(Some("first"), false);
        // Something newer is here, so the app's text waits; the next look sends it.
        assert_eq!(sharing.turn(&mut board, Some("from the app".into())), None);
        assert_eq!(sharing.turn(&mut board, None), Some("second".into()));
        assert!(board.0.pasted.is_empty());
    }

    #[test]
    fn sharing_pastes_what_the_app_sends_until_it_lets_go() {
        let mut board = Fake::default();
        let (to_agent, from_app) = mpsc::channel();
        for text in ["one", "two"] {
            to_agent.send(text.to_string()).unwrap();
        }
        drop(to_agent);
        share(&mut board, from_app, Duration::from_secs(5), |_| panic!("nothing was copied here"));
        assert_eq!(board.pasted, ["one", "two"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pbcopy_and_pbpaste_use_utf8() {
        let cmd = mac::pb("pbcopy");
        let envs: Vec<_> = cmd.get_envs().map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))).collect();
        assert!(envs.contains(&("LC_CTYPE".into(), Some("UTF-8".into()))));
        assert!(envs.contains(&("LC_ALL".into(), None)));
    }
}
