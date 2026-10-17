//! `muxen-agent clip`: this machine's clipboard, shared with the app. Frames
//! on stdin set it (text); frames on stdout say it changed (text copied here).
//! Linux: owns and watches the X clipboard (needs a desktop session). macOS:
//! pbcopy and pbpaste. A password manager's copies aren't sent (each system
//! has its own way of marking them).

use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
#[cfg(unix)]
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

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use std::process::{Command, Stdio};

    fn paste() -> Option<String> {
        let out = Command::new("pbpaste").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn copy(text: &str) {
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }

    unsafe extern "C" {
        fn muxen_pasteboard(concealed: *mut i32) -> i64;
    }

    /// The pasteboard's change count, and whether a password manager marked
    /// what's there private (see mac_screen.m).
    fn pasteboard() -> (i64, bool) {
        let mut concealed = 0;
        let count = unsafe { muxen_pasteboard(&mut concealed) };
        (count, concealed != 0)
    }

    pub fn run(rx: mpsc::Receiver<String>, changed: impl Fn(&str) -> bool) {
        // What's there now is old news; only copies from here on count.
        let mut seen = pasteboard().0;
        loop {
            match rx.recv_timeout(Duration::from_millis(600)) {
                Ok(text) => {
                    copy(&text);
                    seen = pasteboard().0;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let (count, private) = pasteboard();
            if count == seen {
                continue;
            }
            // A password manager's copy stays on this Mac.
            let text = if private { None } else { paste() };
            // Copied again while reading: that copy is looked at next time.
            if pasteboard().0 != count {
                continue;
            }
            seen = count;
            if let Some(now) = text.filter(|t| !t.is_empty() && t.len() <= MAX)
                && !changed(&now)
            {
                return;
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use super::*;
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
                    // first, since a password manager marks its copies.
                    Event::XfixesSelectionNotify(e) if e.owner != win && e.owner != NONE => {
                        asked_at = e.selection_timestamp;
                        ask(targets, asked_at);
                    }
                    Event::SelectionNotify(e) if e.requestor == win && (e.time == asked_at || e.time == x11rb::CURRENT_TIME) => {
                        if e.property == NONE {
                            // It doesn't list what it offers: just the text.
                            if e.target == targets {
                                ask(utf8, asked_at);
                            }
                            continue;
                        }
                        let Ok(cookie) = conn.get_property(true, win, prop, AtomEnum::ANY, 0, (MAX / 4) as u32 + 1) else { continue };
                        let Ok(reply) = cookie.reply() else { continue };
                        if e.target == targets {
                            let offered: Vec<u32> = reply.value32().map(|atoms| atoms.collect()).unwrap_or_default();
                            ask(if offered.contains(&hint) { hint } else { utf8 }, asked_at);
                            continue;
                        }
                        if e.target == hint {
                            // A password manager's copy stays on this machine.
                            if !super::secret(&reply.value) {
                                ask(utf8, asked_at);
                            }
                            continue;
                        }
                        if reply.type_ == incr || reply.value.len() > MAX {
                            continue;
                        }
                        let text = String::from_utf8_lossy(&reply.value).into_owned();
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
                    Ok(text) => {
                        owned = Some(text.clone().into_bytes());
                        last = text;
                        let _ = conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME);
                        let _ = conn.flush();
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
    #[test]
    fn a_password_managers_copy_says_secret() {
        assert!(super::secret(b"secret"));
        assert!(super::secret(b"secret\n"));
        assert!(!super::secret(b""));
        assert!(!super::secret(b"public"));
    }
}
