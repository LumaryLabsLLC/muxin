//! Linux on X11: a keyboard grab. A window manager takes its shortcuts
//! (Alt+Tab, Super, Alt+F4) with grabs of its own on those keys; while a
//! client holds the whole keyboard they don't fire, and every key comes to
//! that client instead.
//!
//! GPUI's own connection can't be reached (its window gives no handle on
//! X11), so the grab is held by a connection of ours, which passes each key
//! on to Muxin's window as the event it would have been. GPUI reads those
//! like any key. The grab itself makes the X server tell the window it lost
//! the keyboard; a FocusIn sent after it tells GPUI it's back, so it keeps
//! reporting the modifier keys (see `Capture::settling`).
//!
//! If Muxin dies, the connection closes and the X server ends the grab.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt, EventMask, GrabMode, GrabStatus, Window};
use x11rb::rust_connection::RustConnection;

/// The keyboard is grabbed while this lives.
pub struct Grab {
    conn: Arc<RustConnection>,
    stop: Arc<AtomicBool>,
}

fn atom(conn: &RustConnection, name: &str) -> Option<u32> {
    conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok().map(|r| r.atom)
}

fn property(conn: &RustConnection, window: Window, name: &str, kind: AtomEnum) -> Option<u32> {
    let reply = conn.get_property(false, window, atom(conn, name)?, kind, 0, 1).ok()?.reply().ok()?;
    reply.value32()?.next()
}

/// Muxin's window with the keyboard: the one the window manager says is
/// active (or, with no window manager, the one with the input focus), when
/// it's this program's.
fn our_window(conn: &RustConnection, root: Window) -> Option<Window> {
    let active = property(conn, root, "_NET_ACTIVE_WINDOW", AtomEnum::WINDOW).filter(|w| *w != 0);
    let window = active.or_else(|| conn.get_input_focus().ok()?.reply().ok().map(|r| r.focus).filter(|w| *w > 1))?;
    // (A window that doesn't say whose it is: taken as ours, as Muxin's has the keyboard.)
    let ours = property(conn, window, "_NET_WM_PID", AtomEnum::CARDINAL).is_none_or(|pid| pid == std::process::id());
    ours.then_some(window)
}

impl Grab {
    /// Grabs the keyboard for Muxin's window, which has it now. None when
    /// the X server says no (another program holds it).
    pub fn start() -> Option<Grab> {
        let (conn, screen) = RustConnection::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let window = our_window(&conn, root)?;
        // The key or click that brought the keyboard here may still be the
        // window manager's for a moment: a few tries (the desktop view tries
        // again later too).
        let mut grabbed = false;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(15));
            }
            let reply = conn.grab_keyboard(false, window, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC).ok()?.reply().ok()?;
            if reply.status == GrabStatus::SUCCESS {
                grabbed = true;
                break;
            }
        }
        if !grabbed {
            return None;
        }
        // The window was just told it lost the keyboard to the grab: it's back.
        let back = xproto::FocusInEvent { response_type: xproto::FOCUS_IN_EVENT, detail: xproto::NotifyDetail::NONLINEAR, sequence: 0, event: window, mode: xproto::NotifyMode::NORMAL };
        conn.send_event(false, window, EventMask::NO_EVENT, back).ok()?;
        conn.flush().ok()?;
        let conn = Arc::new(conn);
        let stop = Arc::new(AtomicBool::new(false));
        let (reader, stopped) = (conn.clone(), stop.clone());
        std::thread::Builder::new().name("keyboard grab".into()).spawn(move || pass_keys_on(&reader, window, &stopped)).ok()?;
        Some(Grab { conn, stop })
    }
}

/// Each key the grab brings, on to Muxin's window (to the client that made
/// it, which is GPUI), until the grab ends.
fn pass_keys_on(conn: &RustConnection, window: Window, stop: &AtomicBool) {
    while !stop.load(Relaxed) {
        let mut any = false;
        loop {
            let sent = match conn.poll_for_event() {
                Ok(Some(Event::KeyPress(mut key))) => {
                    (key.event, key.child) = (window, x11rb::NONE);
                    conn.send_event(false, window, EventMask::NO_EVENT, key).is_ok()
                }
                Ok(Some(Event::KeyRelease(mut key))) => {
                    (key.event, key.child) = (window, x11rb::NONE);
                    conn.send_event(false, window, EventMask::NO_EVENT, key).is_ok()
                }
                Ok(Some(_)) => false,
                Ok(None) => break,
                Err(_) => return,
            };
            any |= sent;
        }
        if any {
            let _ = conn.flush();
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }
}
