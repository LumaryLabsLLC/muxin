//! `muxen-agent clip`: this machine's clipboard, shared with the app. Frames
//! on stdin set it (text); frames on stdout say it changed (text copied here).
//! Linux: owns and watches the X clipboard (needs a desktop session). macOS:
//! pbcopy and pbpaste.

use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// A clipboard's text (the frame kind on the link and on stdin/stdout).
pub const CLIP: u8 = 40;
/// Bigger text isn't shared (X serves it in pieces, which this doesn't do).
pub const MAX: usize = 256 * 1024;

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
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (rx, changed);
        1
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

    pub fn run(rx: mpsc::Receiver<String>, changed: impl Fn(&str) -> bool) {
        // What's there now is old news; only changes from here on count.
        let mut last = paste().unwrap_or_default();
        loop {
            match rx.recv_timeout(Duration::from_millis(600)) {
                Ok(text) => {
                    copy(&text);
                    last = text;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if let Some(now) = paste() {
                if now != last && now.len() <= MAX {
                    if !changed(&now) {
                        return;
                    }
                    last = now;
                }
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
        let string: u32 = AtomEnum::STRING.into();
        conn.xfixes_query_version(5, 0).map_err(|e| e.to_string())?.reply().map_err(|e| format!("no XFixes: {e}"))?;
        conn.xfixes_select_selection_input(win, clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER).map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;

        // Text this window offers while it owns the clipboard, and the last
        // text either way (so nothing echoes back).
        let mut owned: Option<Vec<u8>> = None;
        let mut last = String::new();
        loop {
            while let Some(event) = conn.poll_for_event().map_err(|e| e.to_string())? {
                match event {
                    // Something else took the clipboard: ask it for the text.
                    Event::XfixesSelectionNotify(e) if e.owner != win && e.owner != NONE => {
                        let _ = conn.convert_selection(win, clipboard, utf8, prop, e.selection_timestamp);
                        let _ = conn.flush();
                    }
                    Event::SelectionNotify(e) if e.requestor == win && e.property != NONE => {
                        let Ok(cookie) = conn.get_property(true, win, prop, AtomEnum::ANY, 0, (MAX / 4) as u32 + 1) else { continue };
                        let Ok(reply) = cookie.reply() else { continue };
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
