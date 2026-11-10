//! The machine's screen, for Muxen's desktop view: frames out, mouse and
//! keyboard in. Linux desktops on X11, and Macs (ScreenCaptureKit, see
//! mac_screen.m).
//!
//! A link starts `muxen-agent screen` (on Linux with the desktop session's
//! DISPLAY and XAUTHORITY) and relays between it and a link channel:
//!
//! ```text
//! screen → app   HELLO  JSON {"width", "height", "screen_width", "screen_height"} (or {"error"})
//!                TILES  what changed, as JPEG patches (see `stream`)
//!                FRAME  [width: u16 BE][height: u16 BE][JPEG]  (whole frames, for screenshots)
//!                META   JSON, what changed about an app's window shown: {"cursor"}, {"drag"}, {"minimized"}
//! app → screen   INPUT  JSON {"t": "move" | "button" | "scroll" | "key" | "text" | "resize", ...}
//! ```
//!
//! Coordinates are in frame pixels; the screen may be scaled down to fit
//! `max_width`, and input is mapped back onto it.
//!
//! A "key" is {"keysym", "down"}, and "char": true when the keysym is the
//! character the sender's keyboard typed, Shift already counted (see `typed_char`).

#[cfg(target_os = "linux")]
use std::path::PathBuf;

use serde_json::Value;

pub const FRAME: u8 = 20;
pub const INPUT: u8 = 21;
pub const HELLO: u8 = 22;
/// screen → app: only what changed, as JPEG patches (see `stream`).
pub const TILES: u8 = 23;
/// screen → app: what changed about an app's window shown on its own (see `Screen::meta`).
pub const META: u8 = 24;

/// Why an app's window can't be shown: it's gone (its window in the app closes too).
pub const WINDOW_CLOSED: &str = "That window is closed.";

/// `muxen-agent windows`: the apps' windows on this desktop, front first,
/// each with the id to show it by (`window:<id>` as the display).
pub fn windows() -> Value {
    #[cfg(target_os = "linux")]
    return x11::windows();
    #[cfg(target_os = "macos")]
    return mac::windows();
    #[cfg(windows)]
    return crate::win_screen::windows();
}

/// The parts of a window's top that drag it, as rectangles in its own units
/// (x, y, w, h): `drags` is asked about a grid of points across its top
/// `band` (None when it can't say), and neighbouring answers merge.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn drag_grid(w: i32, h: i32, step: i32, band: i32, mut drags: impl FnMut(i32, i32) -> Option<bool>) -> Option<Vec<[i32; 4]>> {
    let mut areas: Vec<[i32; 4]> = Vec::new();
    for y in (step / 2..band.min(h)).step_by(step.max(1) as usize) {
        let mut from = None;
        for x in (step / 2..w + step).step_by(step.max(1) as usize) {
            let here = x < w && drags(x, y)?;
            match (here, from) {
                (true, None) => from = Some(x - step / 2),
                (false, Some(start)) => {
                    let (x0, x1) = (start, (x - step / 2).min(w));
                    // Under the same span of the row above: one taller area.
                    match areas.iter_mut().find(|a| a[0] == x0 && a[2] == x1 - x0 && a[1] + a[3] == y - step / 2) {
                        Some(a) => a[3] += step,
                        None => areas.push([x0, y - step / 2, x1 - x0, step]),
                    }
                    from = None;
                }
                _ => {}
            }
        }
    }
    Some(areas)
}

/// Where a window `w`×`h` at (`x`, `y`) goes to stay inside `area` (left,
/// top, right, bottom): moved left and up just enough, when it fits there.
#[cfg(any(windows, test))]
pub(crate) fn kept_inside((left, top, right, bottom): (i32, i32, i32, i32), (x, y): (i32, i32), (w, h): (i32, i32)) -> (i32, i32) {
    (x.min(right - w).max(left), y.min(bottom - h).max(top))
}

#[cfg(test)]
mod kept_inside_tests {
    #[test]
    fn moves_a_window_back_onto_its_screen() {
        let screen = (0, 0, 1280, 752);
        // Grown past the right edge: back left just enough; fine as it was: where it was.
        assert_eq!(super::kept_inside(screen, (700, 60), (1010, 633)), (270, 60));
        assert_eq!(super::kept_inside(screen, (100, 60), (1010, 633)), (100, 60));
        // Bigger than the screen: its top left stays on it.
        assert_eq!(super::kept_inside(screen, (300, 200), (1500, 900)), (0, 0));
    }
}

/// The character a "key" event's keysym stands for, as a UTF-16 unit. Not a
/// lowercase ASCII letter (that's its key: Shift comes on its own), a space or a
/// named key. It may not be on this keyboard, or not where the sender's has it.
pub(crate) fn typed_char(sym: u32) -> Option<u16> {
    matches!(sym, 0x21..=0x60 | 0x7b..=0x7e | 0xa1..=0xff).then_some(sym as u16)
}

/// What a key needs for the character at its `level` (0 by itself, 1 with
/// Shift, 2 with AltGr, 3 with both), beyond what's held down there.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct LevelHelp {
    lift_shift: bool,
    press_shift: bool,
    altgr: bool,
}

/// `exact`: the character is what counts ("char": true), so a held Shift comes
/// off a key that doesn't want it. Without it, Shift+2 is Shift and that key.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn level_help(level: u8, shift_held: bool, exact: bool) -> LevelHelp {
    let wants_shift = level & 1 == 1;
    LevelHelp { lift_shift: exact && shift_held && !wants_shift, press_shift: wants_shift && !shift_held, altgr: level >= 2 }
}

/// The button to press on Windows for one from the app (1 left, 2 middle, 3
/// right). SendInput's buttons are the mouse's own, which Windows swaps when
/// the right one is the primary there (Settings › Bluetooth & devices › Mouse).
/// The app's are already as its user's own mouse has them, so they're swapped
/// back first: else every left click was a right one there.
#[cfg(any(windows, test))]
pub(crate) fn win_button(b: u64, swapped: bool) -> u64 {
    match (b, swapped) {
        (1, true) => 3,
        (3, true) => 1,
        _ => b,
    }
}

#[cfg(test)]
#[test]
fn windows_with_the_right_button_primary_gets_the_buttons_swapped_back() {
    assert_eq!([1, 2, 3].map(|b| win_button(b, false)), [1, 2, 3]);
    assert_eq!([1, 2, 3].map(|b| win_button(b, true)), [3, 2, 1]);
}

/// How Windows types a "key" event's character.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq)]
pub(crate) enum WinPress {
    /// Its key, down until it's let go, as the keyboard's own would be.
    Key(u16),
    /// Its key pressed and let go, with Shift down (true) or up just for it: a shortcut.
    Tap(u16, bool),
    /// The character itself, which needs no key.
    Char,
    /// Nothing: a shortcut needs a key, and this keyboard has none for it.
    Nothing,
}

/// `scan`: `VkKeyScanExW`'s answer for the character. `dead`: its key waits for
/// the next one. `shortcut`: Ctrl, Alt or Win is held. `exact`: see `level_help`.
#[cfg(any(windows, test))]
pub(crate) fn win_press(scan: i16, dead: bool, shift_held: bool, shortcut: bool, exact: bool) -> WinPress {
    let char_or_nothing = if shortcut { WinPress::Nothing } else { WinPress::Char };
    // Ctrl and Alt in its high byte are AltGr: no key a shortcut can use, and
    // apps read the Alt in it as a shortcut when they're pressed by hand.
    if scan == -1 || scan as u16 & 0x600 != 0 {
        return char_or_nothing;
    }
    let (vk, wants_shift) = (scan as u16 & 0xff, scan as u16 & 0x100 != 0);
    if wants_shift == shift_held || (shift_held && !exact) {
        return if dead && !shortcut { WinPress::Char } else { WinPress::Key(vk) };
    }
    if shortcut { WinPress::Tap(vk, wants_shift) } else { WinPress::Char }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn what_stands_for_a_character() {
        for c in ['@', '/', '2', '`', '{', '~', '!', 'A', 'ö', 'Ö', '§', 'ß'] {
            assert_eq!(typed_char(c as u32), Some(c as u16), "{c}");
        }
        // Letters are keys, Shift comes on its own. So are space and named keys.
        for sym in ['a' as u32, 'z' as u32, 0x20, 0x7f, 0xa0, 0xff0d, 0xffe1, 0xffbe, 0x0100_0451] {
            assert_eq!(typed_char(sym), None, "{sym:#x}");
        }
    }

    fn help(lift_shift: bool, press_shift: bool, altgr: bool) -> LevelHelp {
        LevelHelp { lift_shift, press_shift, altgr }
    }

    #[test]
    fn levels_on_x11_with_the_character_as_what_counts() {
        // US Mac's Shift+2 on a US machine: Shift's down already, the key is enough.
        assert_eq!(level_help(1, true, true), LevelHelp::default());
        // German Mac's Shift+7 on a US machine: `/` is the key alone, so Shift comes off.
        assert_eq!(level_help(0, true, true), help(true, false, false));
        // US Mac's Shift+2 on a German machine: `@` is AltGr, without Shift.
        assert_eq!(level_help(2, true, true), help(true, false, true));
        assert_eq!(level_help(3, true, true), help(false, false, true));
        // A German Mac's `#` or `+`, no Shift there, on a US machine.
        assert_eq!(level_help(1, false, true), help(false, true, false));
        assert_eq!(level_help(0, false, true), LevelHelp::default());
        assert_eq!(level_help(3, false, true), help(false, true, true));
    }

    #[test]
    fn levels_on_x11_with_the_key_as_what_counts() {
        // `muxin desktop input` "shift+2", an iPad's Shift+2: Shift and that key.
        assert_eq!(level_help(0, true, false), LevelHelp::default());
        assert_eq!(level_help(1, true, false), LevelHelp::default());
        // "@" by itself, or an older app, which lets go of Shift first.
        assert_eq!(level_help(1, false, false), help(false, true, false));
        assert_eq!(level_help(2, false, false), help(false, false, true));
    }

    const VK_2: i16 = 0x32;
    const SHIFT: i16 = 0x100;
    const ALTGR: i16 = 0x600;

    #[test]
    fn windows_presses_the_key_when_shift_already_matches() {
        // US Mac's Shift+2 on a US machine, and a plain 2: real keys, held and repeating.
        assert_eq!(win_press(SHIFT | VK_2, false, true, false, true), WinPress::Key(0x32));
        assert_eq!(win_press(VK_2, false, false, false, true), WinPress::Key(0x32));
        // Ctrl+Shift+2 there.
        assert_eq!(win_press(SHIFT | VK_2, false, true, true, true), WinPress::Key(0x32));
    }

    #[test]
    fn windows_sends_the_character_when_the_keyboards_differ() {
        // German Mac's Shift+7 (`/`) on a US machine: Shift's down, the key wants it up.
        assert_eq!(win_press(0xbf, false, true, false, true), WinPress::Char);
        // A German Mac's `#`, no Shift there, on a US machine.
        assert_eq!(win_press(SHIFT | 0x33, false, false, false, true), WinPress::Char);
        // US Mac's Shift+2 (`@`) on a German machine: AltGr+Q.
        assert_eq!(win_press(ALTGR | 0x51, false, true, false, true), WinPress::Char);
        // Not on this keyboard, and a key that waits for the next one (German `^`).
        assert_eq!(win_press(-1, false, true, false, true), WinPress::Char);
        assert_eq!(win_press(0xdc, true, false, false, true), WinPress::Char);
    }

    #[test]
    fn windows_shortcuts_stay_keys_with_shift_corrected_just_for_them() {
        // German Mac's Ctrl+Shift+7 on a US machine is Ctrl+/.
        assert_eq!(win_press(0xbf, false, true, true, true), WinPress::Tap(0xbf, false));
        // A German Mac's Ctrl+# on a US machine is Ctrl+Shift+3.
        assert_eq!(win_press(SHIFT | 0x33, false, false, true, true), WinPress::Tap(0x33, true));
        // A dead key isn't waiting when it's a shortcut.
        assert_eq!(win_press(0xdc, true, false, true, true), WinPress::Key(0xdc));
        // No key to use: nothing, rather than typing `@` over a shortcut.
        assert_eq!(win_press(ALTGR | 0x51, false, true, true, true), WinPress::Nothing);
        assert_eq!(win_press(-1, false, false, true, true), WinPress::Nothing);
    }

    #[test]
    fn windows_keeps_shift_and_a_key_when_the_key_is_what_counts() {
        // `muxin desktop input` "shift+2": Shift and that key, whatever it types.
        assert_eq!(win_press(VK_2, false, true, false, false), WinPress::Key(0x32));
        // "@" by itself, or an older app, which lets go of Shift first.
        assert_eq!(win_press(SHIFT | VK_2, false, false, false, false), WinPress::Char);
        assert_eq!(win_press(SHIFT | VK_2, false, false, true, false), WinPress::Tap(0x32, true));
    }
}

/// Keeps `found` up to date with a window's drag areas (`areas`), on a
/// thread of its own: once its `size` settles after a change, and `every`
/// while it doesn't (tabs open and close), until `stop`.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn watch_drag_areas(
    size: impl Fn() -> Option<(i32, i32)> + Send + 'static,
    areas: impl Fn() -> Option<Vec<[i32; 4]>> + Send + 'static,
    every: std::time::Duration,
    found: std::sync::Arc<std::sync::Mutex<Option<Vec<[i32; 4]>>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    std::thread::spawn(move || {
        let (mut last, mut was, mut asked) = (None, None, Instant::now() - Duration::from_secs(60));
        while !stop.load(Ordering::Relaxed) {
            let Some(now) = size() else { break };
            let settled = was == Some(now);
            was = Some(now);
            // A new size: ask again once it settles.
            if !settled {
                last = None;
            } else if last.is_none() || asked.elapsed() >= every {
                asked = Instant::now();
                if let Some(a) = areas().filter(|a| last.as_ref() != Some(a)) {
                    *found.lock().unwrap() = Some(a.clone());
                    last = Some(a);
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

#[cfg(all(test, any(windows, target_os = "macos")))]
mod drag_tests {
    #[test]
    fn merges_a_title_bar_into_areas() {
        // A 100×40 window whose top 20 drags, but for a button at x 60..80.
        let areas = super::drag_grid(100, 40, 10, 40, |x, y| Some(y < 20 && !(60..80).contains(&x))).unwrap();
        assert_eq!(areas, vec![[0, 0, 60, 20], [80, 0, 20, 20]]);
        // An app that doesn't answer: no areas at all, rather than wrong ones.
        assert_eq!(super::drag_grid(100, 40, 10, 40, |_, _| None), None);
    }
}

/// Whether an app's window is in front, or comes there in a moment (`front`
/// asked every 20 ms, up to 300): a key typed just as it's brought there
/// waits for it rather than being dropped.
pub(crate) fn comes_to_front(mut front: impl FnMut() -> bool) -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
    loop {
        if front() {
            return true;
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// HELLO for a screen that couldn't open: why (and whether it's an app's window that's gone).
fn refused(e: String) -> i32 {
    hello(&serde_json::json!({ "gone": e == WINDOW_CLOSED, "error": e }));
    1
}

/// DISPLAY and XAUTHORITY of this user's graphical session: our own when set,
/// else borrowed from a process in that session.
#[cfg(target_os = "linux")]
pub fn session_env() -> Option<Vec<(String, String)>> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(display) = get("DISPLAY") {
        let mut env = vec![("DISPLAY".to_string(), display)];
        if let Some(auth) = get("XAUTHORITY") {
            env.push(("XAUTHORITY".to_string(), auth));
        }
        return Some(env);
    }
    let uid = unsafe { libc::getuid() };
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())) {
            continue;
        }
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&path).map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let Ok(raw) = std::fs::read(path.join("environ")) else { continue };
        let vars: Vec<(String, String)> = raw
            .split(|b| *b == 0)
            .filter_map(|kv| std::str::from_utf8(kv).ok()?.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .filter(|(k, _)| k == "DISPLAY" || k == "XAUTHORITY")
            .collect();
        if vars.iter().any(|(k, v)| k == "DISPLAY" && !v.is_empty()) {
            return Some(vars);
        }
    }
    let home = std::env::var("HOME").map(PathBuf::from).ok()?;
    let auth = home.join(".Xauthority");
    auth.exists().then(|| vec![("DISPLAY".into(), ":0".into()), ("XAUTHORITY".into(), auth.display().to_string())])
}

#[cfg(not(target_os = "linux"))]
pub fn session_env() -> Option<Vec<(String, String)>> {
    // A Mac's or Windows' screen needs nothing from the environment.
    Some(Vec::new())
}

/// Why this user's screen isn't there to show, when that's so: nobody is
/// signed in at the screen, or someone else is. Asked before starting
/// anything in the desktop session, so the answer is quick.
#[cfg(target_os = "macos")]
pub fn no_session() -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    // The console belongs to whoever's session is on the screen; root's at the login window.
    let owner = std::fs::metadata("/dev/console").ok()?.uid();
    let me = unsafe { libc::getuid() };
    if owner == me {
        return None;
    }
    if owner == 0 {
        return Some("Nobody is signed in to this Mac, so there's no screen to show. Sign in first, at the Mac or with Screen Sharing.".into());
    }
    let name = |uid: u32| unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_name.is_null() {
            return format!("user {uid}");
        }
        std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
    };
    Some(format!("{} is using this Mac's screen, not {}. Muxin shows the screen of the account it connects as.", name(owner), name(me)))
}

#[cfg(windows)]
pub fn no_session() -> Option<String> {
    if crate::win_screen::in_desktop_session() {
        return None;
    }
    crate::win_screen::no_desktop_session()
}

#[cfg(target_os = "linux")]
pub fn no_session() -> Option<String> {
    // `session_env` says it: no DISPLAY to borrow.
    None
}

/// `muxen-agent screen [fps] [quality] [max_width] [tiles|frames] [display]`:
/// streams a screen on stdout and applies input from stdin until either
/// closes. `display` is one from `displays` (see displays.rs); without it, the
/// whole desktop (Linux) or the main display (a Mac).
pub fn run(args: &[String]) -> i32 {
    let num = |i: usize, default: u32| args.get(i).and_then(|a| a.parse::<u32>().ok()).unwrap_or(default);
    let (fps, quality, max_width) = (num(0, 30).clamp(1, 60), num(1, 75).clamp(20, 95), num(2, 1920).max(320));
    let tiles = args.get(3).map(String::as_str) == Some("tiles");
    let display = args.get(4).map(String::as_str).filter(|d| !d.is_empty() && *d != "main");
    #[cfg(target_os = "linux")]
    {
        match x11::X11::open(display) {
            Ok(screen) => stream(screen, fps, quality as u8, max_width, tiles),
            Err(e) => refused(e),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(id) = display.and_then(|d| d.strip_prefix("window:")) {
            return match mac::Mac::open_window(fps, max_width, id) {
                Ok(screen) => stream(screen, fps, quality as u8, max_width, tiles),
                Err(e) => refused(e),
            };
        }
        let id = match display.map(|d| d.parse::<u32>()) {
            None => 0,
            Some(Ok(id)) => id,
            Some(Err(_)) => return refused(format!("No display called {}.", display.unwrap_or_default())),
        };
        match mac::Mac::open(fps, max_width, id) {
            Ok(screen) => stream(screen, fps, quality as u8, max_width, tiles),
            Err(e) => refused(e),
        }
    }
    #[cfg(windows)]
    {
        match crate::win_screen::WinScreen::open(display) {
            Ok(screen) => stream(screen, fps, quality as u8, max_width, tiles),
            Err(e) => refused(e),
        }
    }
}

fn hello(value: &Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = crate::proto::write_frame(&mut out, HELLO, &serde_json::to_vec(value).unwrap_or_default());
    let _ = out.flush();
}

/// A screen to stream: frames of it, and input applied to it.
trait Screen {
    /// Its size in the pixels `grab` returns.
    fn size(&self) -> (usize, usize);
    /// The largest frame width it can serve (its own size, or what it was set up for).
    fn max_frame(&self) -> usize {
        self.size().0
    }
    /// The screen now (BGRA or BGRX, width × 4 bytes a row), or None when it's
    /// gone. `Some(None)`: nothing changed since the last one.
    fn grab(&mut self) -> Option<Option<&[u8]>>;
    /// Applies an input event; coordinates are in pixels of a `frame`-sized frame.
    fn apply(&mut self, e: &Value, frame: (usize, usize));
    /// After a batch of input.
    fn flush(&mut self) {}
    /// What the app should know in HELLO besides the sizes (e.g. "input": false).
    fn info(&self) -> serde_json::Map<String, Value> {
        serde_json::Map::new()
    }
    /// Before each frame: whether its size changed since the last (an app's
    /// window being resized). Frames then come at the new size.
    fn resized(&mut self) -> bool {
        false
    }
    /// What changed about an app's window since the last call, for the app to
    /// look and behave like it: the pointer over it ({"cursor": "text"}), the
    /// parts of it that drag the window ({"drag": [[x, y, w, h], ...]}, in its
    /// pixels), {"minimized": true}.
    fn meta(&mut self) -> Option<Value> {
        None
    }
    /// The app is done with it: what's left to do before it goes (a window
    /// it resized goes back; a Mac's capture waits for this Mac's others, see
    /// mac.rs's `linger`).
    fn linger(&mut self) {}
}

mod frames {
    /// How the screen maps onto the frame: frame pixel x covers source columns
    /// `xs[x]..xs[x + 1]` (and the same for rows).
    pub struct Scaler {
        sw: usize,
        fw: usize,
        xs: Vec<usize>,
        ys: Vec<usize>,
    }

    impl Scaler {
        pub fn new(sw: usize, sh: usize, fw: usize, fh: usize) -> Scaler {
            let edges = |src: usize, out: usize| (0..=out).map(|i| (i * src / out).min(src)).collect::<Vec<_>>();
            Scaler { sw, fw, xs: edges(sw, fw), ys: edges(sh, fh) }
        }

        /// Frame rows `y0..y1` from the screen into `out` (BGRA, `fw * 4` a row).
        pub fn rows(&self, src: &[u8], y0: usize, y1: usize, out: &mut [u8]) {
            let (sw, fw) = (self.sw, self.fw);
            if sw == fw {
                for y in y0..y1 {
                    let o = (y - y0) * fw * 4;
                    out[o..o + fw * 4].copy_from_slice(&src[y * sw * 4..(y + 1) * sw * 4]);
                }
                return;
            }
            // Average each frame pixel's block of screen pixels.
            for y in y0..y1 {
                let (sy0, sy1) = (self.ys[y], self.ys[y + 1].max(self.ys[y] + 1));
                for x in 0..fw {
                    let (sx0, sx1) = (self.xs[x], self.xs[x + 1].max(self.xs[x] + 1));
                    let mut sum = [0u32; 3];
                    for sy in sy0..sy1 {
                        let row = sy * sw * 4;
                        for sx in sx0..sx1 {
                            let p = row + sx * 4;
                            sum[0] += src[p] as u32;
                            sum[1] += src[p + 1] as u32;
                            sum[2] += src[p + 2] as u32;
                        }
                    }
                    let n = ((sy1 - sy0) * (sx1 - sx0)) as u32;
                    let o = (y - y0) * fw * 4 + x * 4;
                    out[o] = (sum[0] / n) as u8;
                    out[o + 1] = (sum[1] / n) as u8;
                    out[o + 2] = (sum[2] / n) as u8;
                    out[o + 3] = 255;
                }
            }
        }
    }

    /// A changed area and its JPEG.
    pub struct Patch {
        pub x: u16,
        pub y: u16,
        pub w: u16,
        pub h: u16,
        pub jpeg: Vec<u8>,
    }

    pub fn encode(buf: &[u8], stride: usize, x: usize, y: usize, w: usize, h: usize, quality: u8) -> Option<Vec<u8>> {
        let mut rect = Vec::with_capacity(w * h * 4);
        for row in y..y + h {
            let o = row * stride + x * 4;
            rect.extend_from_slice(&buf[o..o + w * 4]);
        }
        let mut jpeg = Vec::with_capacity(w * h / 4);
        jpeg_encoder::Encoder::new(&mut jpeg, quality).encode(&rect, w as u16, h as u16, jpeg_encoder::ColorType::Bgra).ok()?;
        Some(jpeg)
    }

    pub const TILE: usize = 64;
}

/// Windows: GDI captures and SendInput (see win_screen.rs).
#[cfg(windows)]
impl Screen for crate::win_screen::WinScreen {
    fn size(&self) -> (usize, usize) {
        crate::win_screen::WinScreen::size(self)
    }
    fn grab(&mut self) -> Option<Option<&[u8]>> {
        crate::win_screen::WinScreen::grab(self)
    }
    fn apply(&mut self, e: &Value, frame: (usize, usize)) {
        crate::win_screen::WinScreen::apply(self, e, frame)
    }
    fn info(&self) -> serde_json::Map<String, Value> {
        crate::win_screen::WinScreen::info(self)
    }
    fn resized(&mut self) -> bool {
        crate::win_screen::WinScreen::resized(self)
    }
    fn meta(&mut self) -> Option<Value> {
        crate::win_screen::WinScreen::meta(self)
    }
}

/// Streams a screen: `tiles`, only what changed as JPEG strips (the live
/// view); otherwise the whole frame as one JPEG whenever anything changed
/// (screenshots). Input arrives on stdin as it happens.
fn stream(mut screen: impl Screen, fps: u32, quality: u8, max_width: u32, tiles: bool) -> i32 {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use frames::{Patch, Scaler, TILE, encode};


    let (mut sw, mut sh) = screen.size();
    let frame_size = |sw: usize, sh: usize, max_frame: usize| {
        let fw = (max_width as usize).min(max_frame).min(sw).max(1);
        (fw, (sh * fw / sw.max(1)).max(1))
    };
    let (mut fw, mut fh) = frame_size(sw, sh, screen.max_frame());
    let mut info = screen.info();
    for (k, v) in [("width", fw), ("height", fh), ("screen_width", sw), ("screen_height", sh)] {
        info.insert(k.into(), v.into());
    }
    hello(&Value::Object(info));
    let mut scaler = Scaler::new(sw, sh, fw, fh);
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16);

    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Ok((kind, payload)) = crate::proto::read_frame(&mut stdin) {
            if kind == INPUT {
                if let Ok(v) = serde_json::from_slice::<Value>(&payload) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let tick = Duration::from_millis(1000 / fps as u64);
    let mut next = Instant::now();
    let mut cur = vec![0u8; fw * fh * 4];
    let mut prev: Option<Vec<u8>> = None;
    // Tiles sent soft (at `quality`), sent again sharp once the picture has
    // been still a moment: text ends up crisp without every frame costing that.
    let refine = tiles && quality < SHARP;
    let mut soft = vec![false; fw.div_ceil(TILE) * fh.div_ceil(TILE)];
    let mut changed_at = Instant::now();
    let mut stdout = std::io::stdout().lock();
    let code = loop {
        // Input first, until it's time for a frame; a frame soon after input
        // shows its effect without waiting a whole tick.
        match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(event) => {
                screen.apply(&event, (fw, fh));
                while let Ok(more) = rx.try_recv() {
                    screen.apply(&more, (fw, fh));
                }
                screen.flush();
                next = next.min(Instant::now() + Duration::from_millis(12));
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break 0,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        next = Instant::now() + tick;
        // A new size: everything is sent again at it (each message says its size).
        if screen.resized() {
            (sw, sh) = screen.size();
            (fw, fh) = frame_size(sw, sh, screen.max_frame());
            scaler = Scaler::new(sw, sh, fw, fh);
            cur = vec![0u8; fw * fh * 4];
            prev = None;
            soft = vec![false; fw.div_ceil(TILE) * fh.div_ceil(TILE)];
        }
        if let Some(mut meta) = screen.meta() {
            // Its drag areas, in frame pixels like everything else.
            if let Some(rects) = meta.get_mut("drag").and_then(Value::as_array_mut) {
                for r in rects.iter_mut().filter_map(Value::as_array_mut) {
                    for (i, v) in r.iter_mut().enumerate() {
                        let (to, from) = if i % 2 == 0 { (fw, sw) } else { (fh, sh) };
                        *v = Value::from(v.as_f64().unwrap_or(0.) * to as f64 / from.max(1) as f64);
                    }
                }
            }
            if crate::proto::write_frame(&mut stdout, META, &serde_json::to_vec(&meta).unwrap_or_default()).and_then(|_| stdout.flush()).is_err() {
                break 0;
            }
        }
        let src = match screen.grab() {
            None => break 1,
            Some(None) => None,
            Some(Some(src)) if src.len() < sw * sh * 4 => break 1,
            Some(Some(src)) => Some(src),
        };
        let Some(src) = src else {
            if refine && !send_sharp(&mut stdout, prev.as_deref(), (fw, fh), &mut soft, changed_at, workers) {
                break 0;
            }
            continue;
        };

        // Bands of tile rows, one per worker: scale, compare, encode what changed.
        let band_rows = fh.div_ceil(TILE).div_ceil(workers) * TILE;
        let previous = prev.as_deref();
        let patches: Vec<Patch> = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for (band, out) in cur.chunks_mut(band_rows * fw * 4).enumerate() {
                let (scaler, previous) = (&scaler, previous);
                handles.push(s.spawn(move || {
                    let y0 = band * band_rows;
                    let y1 = (y0 + band_rows).min(fh);
                    scaler.rows(src, y0, y1, out);
                    let mut found = Vec::new();
                    let mut ty = y0;
                    while ty < y1 {
                        let th = TILE.min(y1 - ty);
                        // Changed tiles in this row, merged into runs.
                        let changed = |tx: usize| {
                            let Some(p) = previous else { return true };
                            let tw = TILE.min(fw - tx);
                            (ty..ty + th).any(|y| {
                                let o = y * fw * 4 + tx * 4;
                                let l = (y - y0) * fw * 4 + tx * 4;
                                out[l..l + tw * 4] != p[o..o + tw * 4]
                            })
                        };
                        let mut tx = 0;
                        while tx < fw {
                            if !changed(tx) {
                                tx += TILE;
                                continue;
                            }
                            let start = tx;
                            while tx < fw && changed(tx) {
                                tx += TILE;
                            }
                            let w = tx.min(fw) - start;
                            if let Some(jpeg) = encode(out, fw * 4, start, ty - y0, w, th, quality) {
                                found.push(Patch { x: start as u16, y: ty as u16, w: w as u16, h: th as u16, jpeg });
                            }
                        }
                        ty += th;
                    }
                    found
                }));
            }
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        if patches.is_empty() {
            if refine && !send_sharp(&mut stdout, prev.as_deref(), (fw, fh), &mut soft, changed_at, workers) {
                break 0;
            }
            continue;
        }
        changed_at = Instant::now();
        let sent = if tiles {
            if refine {
                let cols = fw.div_ceil(TILE);
                for p in &patches {
                    let (tx0, ty0) = (p.x as usize / TILE, p.y as usize / TILE);
                    for ty in ty0..(p.y as usize + p.h as usize).div_ceil(TILE) {
                        for tx in tx0..(p.x as usize + p.w as usize).div_ceil(TILE) {
                            soft[ty * cols + tx] = true;
                        }
                    }
                }
            }
            crate::proto::write_frame(&mut stdout, TILES, &tiles_message(fw, fh, &patches))
        } else {
            let Some(jpeg) = encode(&cur, fw * 4, 0, 0, fw, fh, quality) else { continue };
            let mut msg = Vec::with_capacity(4 + jpeg.len());
            msg.extend((fw as u16).to_be_bytes());
            msg.extend((fh as u16).to_be_bytes());
            msg.extend(jpeg);
            crate::proto::write_frame(&mut stdout, FRAME, &msg)
        };
        if sent.and_then(|_| stdout.flush()).is_err() {
            break 0;
        }
        // What the app has now, to compare the next frame with.
        match &mut prev {
            Some(p) => std::mem::swap(p, &mut cur),
            None => prev = Some(cur.clone()),
        }
    };
    ended(&mut screen, code)
}

/// The stream is over, however it ended (the app closed it, the window
/// closed, the capture stopped): the app is told first (its end of the pipe
/// closes), then what's left is done (`linger`: a window goes back to its
/// size; a Mac's capture waits for this Mac's others, whichever way it ended).
fn ended(screen: &mut impl Screen, code: i32) -> i32 {
    #[cfg(unix)]
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        if null >= 0 {
            libc::dup2(null, 1);
            libc::close(null);
        }
    }
    screen.linger();
    code
}

/// A TILES message: `[width][height][count]`, then each `[x][y][w][h][len: u32][jpeg]`.
fn tiles_message(fw: usize, fh: usize, patches: &[frames::Patch]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(6 + patches.iter().map(|p| 12 + p.jpeg.len()).sum::<usize>());
    for v in [fw as u16, fh as u16, patches.len() as u16] {
        msg.extend(v.to_be_bytes());
    }
    for p in patches {
        for v in [p.x, p.y, p.w, p.h] {
            msg.extend(v.to_be_bytes());
        }
        msg.extend((p.jpeg.len() as u32).to_be_bytes());
        msg.extend(&p.jpeg);
    }
    msg
}

/// Once the picture has been still a moment, the tiles last sent soft go
/// again sharp. False when the app is gone.
fn send_sharp(out: &mut impl std::io::Write, frame: Option<&[u8]>, (fw, fh): (usize, usize), soft: &mut [bool], changed_at: std::time::Instant, workers: usize) -> bool {
    use frames::{Patch, TILE, encode};
    let Some(frame) = frame.filter(|_| changed_at.elapsed() >= std::time::Duration::from_millis(300) && soft.iter().any(|s| *s)) else { return true };
    let cols = fw.div_ceil(TILE);
    let rows: Vec<usize> = (0..fh.div_ceil(TILE)).filter(|ty| soft[ty * cols..(ty + 1) * cols].iter().any(|s| *s)).collect();
    let soft_ref = &*soft;
    let patches: Vec<Patch> = std::thread::scope(|s| {
        let handles: Vec<_> = rows
            .chunks(rows.len().div_ceil(workers).max(1))
            .map(|chunk| {
                s.spawn(move || {
                    let mut found = Vec::new();
                    for &ty in chunk {
                        let (y, h) = (ty * TILE, TILE.min(fh - ty * TILE));
                        let mut tx = 0;
                        while tx < cols {
                            if !soft_ref[ty * cols + tx] {
                                tx += 1;
                                continue;
                            }
                            let start = tx;
                            while tx < cols && soft_ref[ty * cols + tx] {
                                tx += 1;
                            }
                            let (x, w) = (start * TILE, (tx * TILE).min(fw) - start * TILE);
                            if let Some(jpeg) = encode(frame, fw * 4, x, y, w, h, SHARP) {
                                found.push(Patch { x: x as u16, y: y as u16, w: w as u16, h: h as u16, jpeg });
                            }
                        }
                    }
                    found
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    soft.fill(false);
    patches.is_empty() || crate::proto::write_frame(out, TILES, &tiles_message(fw, fh, &patches)).and_then(|_| out.flush()).is_ok()
}

/// The quality what was sent soft is sent again at (with full colour detail).
const SHARP: u8 = 95;

#[cfg(target_os = "linux")]
mod x11 {
    use std::collections::HashMap;

    use serde_json::{Value, json};
    use x11rb::connection::Connection;
    use x11rb::protocol::composite::ConnectionExt as _;
    use x11rb::protocol::xfixes::ConnectionExt as _;
    use x11rb::protocol::xproto::{
        AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, ImageFormat, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
        MOTION_NOTIFY_EVENT, MapState,
    };
    use x11rb::protocol::xtest::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    /// An X11 desktop: frames through shared memory with the X server when it
    /// allows (no copying the screen through the socket), else GetImage; input
    /// through XTEST.
    pub struct X11 {
        conn: RustConnection,
        root: u32,
        /// The part of the desktop shown: one monitor's area, or all of it.
        ox: i16,
        oy: i16,
        sw: usize,
        sh: usize,
        shm: Option<(u32, *mut u8, usize)>,
        scratch: Vec<u8>,
        /// Unused keycodes given a keysym, for characters the keyboard doesn't
        /// have, oldest first (see `spare_key`). Taken back when the stream ends.
        spare: Vec<(u8, u32)>,
        /// The monitor shown (None: all of them), and when its place was last checked.
        monitor: Option<String>,
        checked: std::time::Instant,
        /// Keys (keycode, the keysym that pressed it) and buttons pressed and not yet
        /// let go: let go when the stream ends, so none stays held (a key held
        /// down freezes the desktop's input).
        pressed: Vec<(u8, u32)>,
        buttons: Vec<u8>,
        /// An app's window shown on its own, and the whole desktop's size.
        window: Option<Win>,
        desktop: (usize, usize),
    }

    /// An app's window shown on its own (`window:<id>`, a client window from `windows`).
    struct Win {
        client: u32,
        /// The desktop's child holding it (the window manager's frame, or itself).
        frame: u32,
        atoms: Atoms,
        /// Whether the X server can give a window's own picture (Composite), for one that's covered.
        composite: bool,
        /// The window manager's frame around it (left, right, top, bottom), and
        /// what's added around the client in all (a GTK window's shadow taken off).
        frame_extents: [i32; 4],
        added: [i32; 4],
        /// Where it was when a button went down: input maps from there until
        /// let go, so an app moving its own window doesn't run from the pointer.
        held_at: Option<(i16, i16)>,
        /// What the app was last told (see `meta`).
        cursor: &'static str,
        minimized: Option<bool>,
        drag: Option<Vec<[i32; 4]>>,
        ticks: u32,
        /// How much the desktop scales things up (2 at 200%).
        scale: f64,
        /// Its client's size and whether it was maximized before the app first
        /// resized it: so it's left again when the stream ends.
        restore: Option<(u32, u32, bool)>,
    }

    /// How much the desktop scales things up: its `Xft.dpi` (192 at 200%) over 96.
    fn desktop_scale(conn: &RustConnection, root: u32) -> f64 {
        let resources = conn.get_property(false, root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16).ok().and_then(|c| c.reply().ok());
        let text = resources.map(|r| String::from_utf8_lossy(&r.value).into_owned()).unwrap_or_default();
        text.lines().find_map(|l| l.strip_prefix("Xft.dpi:")).and_then(|v| v.trim().parse::<f64>().ok()).filter(|d| *d > 0.).map_or(1., |dpi| dpi / 96.)
    }

    /// The window manager's atoms window mode reads and sends.
    struct Atoms {
        stacking: u32,
        clients: u32,
        active: u32,
        name: u32,
        utf8: u32,
        pid: u32,
        state: u32,
        hidden: u32,
        skip_taskbar: u32,
        max_h: u32,
        max_v: u32,
        kind: u32,
        normal: u32,
        dialog: u32,
        extents: u32,
        gtk_extents: u32,
    }

    fn atoms(conn: &RustConnection) -> Result<Atoms, String> {
        let names: [&[u8]; 16] = [
            b"_NET_CLIENT_LIST_STACKING",
            b"_NET_CLIENT_LIST",
            b"_NET_ACTIVE_WINDOW",
            b"_NET_WM_NAME",
            b"UTF8_STRING",
            b"_NET_WM_PID",
            b"_NET_WM_STATE",
            b"_NET_WM_STATE_HIDDEN",
            b"_NET_WM_STATE_SKIP_TASKBAR",
            b"_NET_WM_STATE_MAXIMIZED_HORZ",
            b"_NET_WM_STATE_MAXIMIZED_VERT",
            b"_NET_WM_WINDOW_TYPE",
            b"_NET_WM_WINDOW_TYPE_NORMAL",
            b"_NET_WM_WINDOW_TYPE_DIALOG",
            b"_NET_FRAME_EXTENTS",
            b"_GTK_FRAME_EXTENTS",
        ];
        let cookies = names.iter().map(|n| conn.intern_atom(false, n)).collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        let a = cookies.into_iter().map(|c| c.reply().map(|r| r.atom)).collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        Ok(Atoms {
            stacking: a[0],
            clients: a[1],
            active: a[2],
            name: a[3],
            utf8: a[4],
            pid: a[5],
            state: a[6],
            hidden: a[7],
            skip_taskbar: a[8],
            max_h: a[9],
            max_v: a[10],
            kind: a[11],
            normal: a[12],
            dialog: a[13],
            extents: a[14],
            gtk_extents: a[15],
        })
    }

    /// A window's property, as 32-bit values (empty when it hasn't one).
    fn prop32(conn: &RustConnection, window: u32, atom: u32) -> Vec<u32> {
        conn.get_property(false, window, atom, AtomEnum::ANY, 0, 4096).ok().and_then(|c| c.reply().ok()).and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default()
    }

    fn title_of(conn: &RustConnection, a: &Atoms, window: u32) -> String {
        let text = |atom: u32, kind: u32| {
            let r = conn.get_property(false, window, atom, kind, 0, 1024).ok()?.reply().ok()?;
            Some(String::from_utf8_lossy(&r.value).into_owned()).filter(|t| !t.is_empty())
        };
        text(a.name, a.utf8).or_else(|| text(AtomEnum::WM_NAME.into(), AtomEnum::STRING.into())).unwrap_or_default()
    }

    /// The name of the program a window belongs to ("firefox"): its
    /// executable's (its `comm` stops at 15 characters, and some programs,
    /// like Chrome, rewrite their command line).
    fn program_of(conn: &RustConnection, a: &Atoms, window: u32) -> String {
        let Some(pid) = prop32(conn, window, a.pid).first().copied() else { return String::new() };
        let named = |p: &std::path::Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty());
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|p| named(&p))
            .or_else(|| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|c| c.trim().to_string()))
            .unwrap_or_default()
    }

    /// Where a window shows on the desktop, its window manager's frame
    /// included (and a GTK window's shadow left out); the frame's sizes; and
    /// what's added around the client in all.
    fn bounds(conn: &RustConnection, a: &Atoms, window: u32, root: u32) -> Option<((i16, i16, usize, usize), [i32; 4], [i32; 4])> {
        let at = conn.translate_coordinates(window, root, 0, 0).ok()?.reply().ok()?;
        let g = conn.get_geometry(window).ok()?.reply().ok()?;
        let four = |atom: u32| {
            let v = prop32(conn, window, atom);
            std::array::from_fn::<i32, 4, _>(|i| v.get(i).copied().unwrap_or(0) as i32)
        };
        let (frame, gtk) = (four(a.extents), four(a.gtk_extents));
        let added: [i32; 4] = std::array::from_fn(|i| frame[i] - gtk[i]);
        let (x, y) = (at.dst_x as i32 - added[0], at.dst_y as i32 - added[2]);
        let (w, h) = (g.width as i32 + added[0] + added[1], g.height as i32 + added[2] + added[3]);
        (w > 0 && h > 0).then_some(((x as i16, y as i16, w as usize, h as usize), frame, added))
    }

    /// The desktop's child that holds a window (its window manager's frame, or itself).
    fn frame_of(conn: &RustConnection, root: u32, window: u32) -> u32 {
        let mut w = window;
        for _ in 0..16 {
            match conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
                Some(t) if t.parent != root && t.parent != 0 => w = t.parent,
                _ => break,
            }
        }
        w
    }

    /// A window a taskbar would list: a normal one or a dialog, with a title.
    fn is_app(conn: &RustConnection, a: &Atoms, window: u32) -> bool {
        let kinds = prop32(conn, window, a.kind);
        if !kinds.is_empty() && !kinds.iter().any(|k| *k == a.normal || *k == a.dialog) {
            return false;
        }
        // One that says it's an app's window needs no title (Ghostty's is
        // blank until its shell names it); one that doesn't say, does.
        !prop32(conn, window, a.state).contains(&a.skip_taskbar) && (!kinds.is_empty() || !title_of(conn, a, window).trim().is_empty())
    }

    /// What to call a window: its title, or its program's name while it has none.
    fn name_of(conn: &RustConnection, a: &Atoms, window: u32) -> String {
        let title = title_of(conn, a, window);
        match title.trim().is_empty() {
            true => program_of(conn, a, window),
            false => title,
        }
    }

    /// `muxen-agent windows` on Linux: the apps' windows its window manager
    /// lists, front first.
    pub fn windows() -> Value {
        let Ok((conn, n)) = x11rb::connect(None) else {
            return json!({ "ok": false, "error": "Can't reach the desktop. Is someone logged in to a graphical session?" });
        };
        let root = conn.setup().roots[n].root;
        let a = match atoms(&conn) {
            Ok(a) => a,
            Err(e) => return json!({ "ok": false, "error": e }),
        };
        let mut list = prop32(&conn, root, a.stacking);
        if list.is_empty() {
            list = prop32(&conn, root, a.clients);
        }
        if list.is_empty() {
            return json!({ "ok": false, "error": "This desktop's window manager doesn't list its windows." });
        }
        // Bottom to top: front first.
        list.reverse();
        let active = prop32(&conn, root, a.active).first().copied().unwrap_or(0);
        let out: Vec<Value> = list
            .into_iter()
            .filter(|w| is_app(&conn, &a, *w))
            .map(|w| {
                let size = bounds(&conn, &a, w, root).map_or((0, 0), |(r, _, _)| (r.2, r.3));
                json!({
                    "id": w.to_string(),
                    "title": name_of(&conn, &a, w),
                    "app": program_of(&conn, &a, w),
                    "width": size.0,
                    "height": size.1,
                    "minimized": prop32(&conn, w, a.state).contains(&a.hidden),
                    "front": w == active,
                })
            })
            .collect();
        json!({ "ok": true, "windows": out })
    }

    /// Asks the window manager to bring a window to the front (and back from minimized).
    fn activate(conn: &RustConnection, root: u32, a: &Atoms, window: u32) {
        // Source 2: a pager, which window managers obey without question.
        let event = ClientMessageEvent::new(32, window, a.active, [2u32, 0, 0, 0, 0]);
        let _ = conn.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, event);
        let _ = conn.flush();
    }

    /// Whether the window is the active one, or one of its dialogs is.
    fn in_front(conn: &RustConnection, root: u32, a: &Atoms, window: u32) -> bool {
        let Some(active) = prop32(conn, root, a.active).first().copied() else { return false };
        active == window || prop32(conn, active, AtomEnum::WM_TRANSIENT_FOR.into()).first() == Some(&window)
    }

    /// Whether another window lies over part of `rect` (menus and tooltips,
    /// which aren't managed, don't count).
    fn covered(conn: &RustConnection, root: u32, frame: u32, (x, y, w, h): (i16, i16, usize, usize)) -> bool {
        let Some(tree) = conn.query_tree(root).ok().and_then(|c| c.reply().ok()) else { return false };
        let Some(at) = tree.children.iter().position(|c| *c == frame) else { return false };
        let above = &tree.children[at + 1..];
        let attrs: Vec<_> = above.iter().map(|c| conn.get_window_attributes(*c).ok()).collect();
        let geoms: Vec<_> = above.iter().map(|c| conn.get_geometry(*c).ok()).collect();
        attrs.into_iter().zip(geoms).any(|(a, g)| {
            let (Some(a), Some(g)) = (a.and_then(|c| c.reply().ok()), g.and_then(|c| c.reply().ok())) else { return false };
            let (gx, gy, gw, gh) = (g.x as i32, g.y as i32, g.width as i32, g.height as i32);
            a.map_state == MapState::VIEWABLE
                && !a.override_redirect
                && gx < x as i32 + w as i32
                && (x as i32) < gx + gw
                && gy < y as i32 + h as i32
                && (y as i32) < gy + gh
        })
    }

    /// The shape of the pointer, when it's over `rect` (see win_screen.rs's `pointer_over`).
    fn pointer_over(conn: &RustConnection, root: u32, (x, y, w, h): (i16, i16, usize, usize)) -> Option<&'static str> {
        let p = conn.query_pointer(root).ok()?.reply().ok()?;
        let (px, py) = (p.root_x as i32, p.root_y as i32);
        if px < x as i32 || py < y as i32 || px >= x as i32 + w as i32 || py >= y as i32 + h as i32 {
            return None;
        }
        let cursor = conn.xfixes_get_cursor_image_and_name().ok()?.reply().ok()?;
        Some(match String::from_utf8_lossy(&cursor.name).as_ref() {
            "xterm" | "text" | "ibeam" => "text",
            "hand1" | "hand2" | "pointer" | "pointing_hand" => "hand",
            "sb_h_double_arrow" | "h_double_arrow" | "ew-resize" | "col-resize" | "left_side" | "right_side" | "e-resize" | "w-resize" => "ew",
            "sb_v_double_arrow" | "v_double_arrow" | "ns-resize" | "row-resize" | "top_side" | "bottom_side" | "n-resize" | "s-resize" => "ns",
            "bottom_right_corner" | "top_left_corner" | "nwse-resize" | "se-resize" | "nw-resize" | "size_fdiag" => "nwse",
            "bottom_left_corner" | "top_right_corner" | "nesw-resize" | "sw-resize" | "ne-resize" | "size_bdiag" => "nesw",
            "fleur" | "move" | "all-scroll" | "grabbing" | "dnd-move" => "move",
            "not-allowed" | "crossed_circle" | "no-drop" | "forbidden" => "no",
            "crosshair" | "cross" | "tcross" => "crosshair",
            "watch" | "wait" | "progress" | "left_ptr_watch" => "wait",
            _ => "arrow",
        })
    }

    /// The part of the window manager's title bar that drags the window: its
    /// middle, clear of the buttons at either end (themes put them on either
    /// side). None for a window that draws its own (a GTK header bar).
    fn title_bar(frame: [i32; 4], width: usize) -> Vec<[i32; 4]> {
        let top = frame[2];
        let (left, right) = (frame[0] + top * 7 / 2, frame[1] + top * 7 / 2);
        match top > 0 && (width as i32) > left + right {
            true => vec![[left, 0, width as i32 - left - right, top]],
            false => Vec::new(),
        }
    }

    /// What it takes to type on the desktop's keyboard, as it is right now.
    #[derive(Default)]
    struct Typing {
        /// keysym → (keycode, level): 0 the key alone, 1 with Shift, 2 with
        /// AltGr, 3 with both. The lowest level that has it.
        keys: HashMap<u32, (u8, u8)>,
        /// Keycodes with nothing on them, the highest first.
        unused: Vec<u8>,
    }

    impl Typing {
        /// From the core keyboard mapping: `per` keysyms for each key from `min`.
        /// A key's columns are its levels 0 and 1, a second layout's two, then levels 2 and 3.
        fn from_mapping(min: u8, per: usize, syms: &[u32]) -> Self {
            let mut typing = Typing::default();
            let keys: Vec<(u8, &[u32])> = syms.chunks(per.max(1)).enumerate().map(|(i, syms)| (min.saturating_add(i as u8), syms)).collect();
            for (level, column) in [(0, 0), (1, 1), (2, 4), (3, 5)] {
                for (code, syms) in &keys {
                    if let Some(sym) = syms.get(column).filter(|sym| **sym != 0) {
                        typing.keys.entry(*sym).or_insert((*code, level));
                    }
                }
            }
            typing.unused = keys.iter().rev().filter(|(_, syms)| syms.iter().all(|sym| *sym == 0)).map(|(code, _)| *code).collect();
            typing
        }

        fn read(conn: &RustConnection) -> Self {
            let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
            match conn.get_keyboard_mapping(min, max - min + 1).map(|c| c.reply()) {
                Ok(Ok(map)) => Self::from_mapping(min, map.keysyms_per_keycode as usize, &map.keysyms),
                _ => Self::default(),
            }
        }
    }

    /// The keysym for typing a character.
    fn char_keysym(c: char) -> u32 {
        match c {
            '\n' => 0xff0d,
            '\t' => 0xff09,
            // Only the Euro's old keysym matches what keyboards have it as.
            '€' => 0x20ac,
            c if matches!(c as u32, 0x20..=0x7e | 0xa0..=0xff) => c as u32,
            c => 0x0100_0000 + c as u32,
        }
    }

    /// A key for a character this keyboard doesn't have: an unused keycode,
    /// given its keysym (or the one given longest ago, when none is left). It
    /// keeps it, so typing that character again doesn't make every program on
    /// the desktop read the keyboard's mapping again.
    fn spare_key(conn: &RustConnection, spare: &mut Vec<(u8, u32)>, unused: &[u8], sym: u32) -> Option<u8> {
        let code = match unused.iter().find(|code| !spare.iter().any(|(c, _)| c == *code)) {
            Some(code) => *code,
            None if !spare.is_empty() => spare.remove(0).0,
            None => return None,
        };
        conn.change_keyboard_mapping(1, code, 2, &[sym, sym]).ok()?;
        spare.push((code, sym));
        // Programs read the new mapping when they hear of it, which comes
        // before the key does, but give them a moment to.
        conn.get_input_focus().ok()?.reply().ok()?;
        std::thread::sleep(std::time::Duration::from_millis(30));
        Some(code)
    }

    impl Drop for X11 {
        fn drop(&mut self) {
            for (code, _) in self.pressed.drain(..) {
                let _ = self.conn.xtest_fake_input(KEY_RELEASE_EVENT, code, 0, self.root, 0, 0, 0);
            }
            for b in self.buttons.drain(..) {
                let _ = self.conn.xtest_fake_input(BUTTON_RELEASE_EVENT, b, 0, self.root, 0, 0, 0);
            }
            let keep_up = !self.spare.is_empty();
            for (code, _) in self.spare.drain(..) {
                let _ = self.conn.change_keyboard_mapping(1, code, 2, &[0, 0]);
            }
            let _ = self.conn.flush();
            // The server drops what it hasn't done when a program quits.
            if keep_up {
                let _ = self.conn.get_input_focus().map(|c| c.reply());
            }
        }
    }

    /// Turns on the driver's TearFree on the screens it can (AMD's and
    /// Intel's drivers), when it isn't: with it off or "auto", a compositor's
    /// frames can be flipped onto the screen straight from its own buffers,
    /// so the screen the X server hands out (what's captured) stops changing
    /// while nobody captures it. The next stream then starts from that old
    /// picture, and parts of it stay old until they change again. With
    /// TearFree on, the driver draws every frame into that screen itself.
    /// Changing it repaints the whole screen once; it stays on (it also stops
    /// tearing), so later streams don't repaint again.
    fn tear_free(conn: &RustConnection, root: u32) {
        use x11rb::protocol::randr::{Connection as State, ConnectionExt as _};
        use x11rb::protocol::xproto::PropMode;
        let atom = |name: &[u8]| conn.intern_atom(true, name).ok().and_then(|c| c.reply().ok()).map(|r| r.atom).filter(|a| *a != 0);
        // No such property: not a driver that has it.
        let (Some(prop), Some(on)) = (atom(b"TearFree"), atom(b"on")) else { return };
        let Some(res) = conn.randr_get_screen_resources_current(root).ok().and_then(|c| c.reply().ok()) else { return };
        let mut changed = false;
        for output in res.outputs {
            let Some(info) = conn.randr_get_output_info(output, res.config_timestamp).ok().and_then(|c| c.reply().ok()) else { continue };
            if info.connection != State::CONNECTED || info.crtc == 0 {
                continue;
            }
            let Some(now) = conn.randr_get_output_property(output, prop, AtomEnum::ANY, 0, 1, false, false).ok().and_then(|c| c.reply().ok()) else { continue };
            if now.format != 32 || now.data.len() < 4 {
                continue;
            }
            if u32::from_ne_bytes([now.data[0], now.data[1], now.data[2], now.data[3]]) == on {
                continue;
            }
            if conn.randr_change_output_property(output, prop, AtomEnum::ATOM.into(), 32, PropMode::REPLACE, 1, &on.to_ne_bytes()).ok().and_then(|c| c.check().ok()).is_some() {
                eprintln!("screen: turned TearFree on for {} (its picture went stale between streams)", String::from_utf8_lossy(&info.name));
                changed = true;
            }
        }
        // The repaint it brings, before the first frame.
        if changed {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    /// Where a monitor is on the desktop (None: the whole desktop), right now.
    fn area(conn: &RustConnection, root: u32, monitor: Option<&str>) -> Result<(i16, i16, usize, usize), String> {
        use x11rb::protocol::randr::ConnectionExt as _;
        let Some(name) = monitor else {
            let g = conn.get_geometry(root).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
            return Ok((0, 0, g.width as usize, g.height as usize));
        };
        let monitors = conn.randr_get_monitors(root, true).ok().and_then(|c| c.reply().ok()).map(|r| r.monitors).unwrap_or_default();
        let found = monitors.into_iter().find(|m| conn.get_atom_name(m.name).ok().and_then(|c| c.reply().ok()).is_some_and(|a| a.name == name.as_bytes()));
        let m = found.ok_or_else(|| format!("No display called {name} here (it may have been turned off)."))?;
        Ok((m.x, m.y, m.width as usize, m.height as usize))
    }

    impl X11 {
        /// The desktop, one monitor of it (a RandR monitor's name; "all":
        /// everything), or an app's window (`window:<id>`).
        pub fn open(monitor: Option<&str>) -> Result<X11, String> {
            let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("Can't reach the desktop ({e}). Is someone logged in to a graphical session?"))?;
            let root = conn.setup().roots[screen_num].root;
            tear_free(&conn, root);
            let (_, _, dw, dh) = area(&conn, root, None)?;
            if let Some(id) = monitor.and_then(|m| m.strip_prefix("window:")) {
                return Self::open_window(conn, root, (dw, dh), id);
            }
            let monitor = monitor.filter(|m| *m != "all").map(str::to_string);
            let (ox, oy, sw, sh) = area(&conn, root, monitor.as_deref())?;
            let shm = attach_shm(&conn, sw * sh * 4);
            Ok(X11 { conn, root, ox, oy, sw, sh, shm, scratch: Vec::new(), spare: Vec::new(), monitor, checked: std::time::Instant::now(), pressed: Vec::new(), buttons: Vec::new(), window: None, desktop: (dw, dh) })
        }

        fn open_window(conn: RustConnection, root: u32, desktop: (usize, usize), id: &str) -> Result<X11, String> {
            let closed = || super::WINDOW_CLOSED.to_string();
            let client: u32 = id.parse().map_err(|_| closed())?;
            let atoms = atoms(&conn)?;
            conn.get_geometry(client).ok().and_then(|c| c.reply().ok()).ok_or_else(closed)?;
            // Minimized, it isn't drawn: bring it back.
            if prop32(&conn, client, atoms.state).contains(&atoms.hidden) {
                activate(&conn, root, &atoms, client);
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            let ((ox, oy, sw, sh), frame_extents, added) = bounds(&conn, &atoms, client, root).ok_or_else(closed)?;
            let composite = conn.composite_query_version(0, 4).ok().and_then(|c| c.reply().ok()).is_some();
            let _ = conn.xfixes_query_version(5, 0).map(|c| c.reply());
            let scale = desktop_scale(&conn, root);
            let win = Win { client, frame: frame_of(&conn, root, client), atoms, composite, frame_extents, added, held_at: None, cursor: "", minimized: None, drag: None, ticks: 0, scale, restore: None };
            let shm = attach_shm(&conn, desktop.0 * desktop.1 * 4);
            Ok(X11 { conn, root, ox, oy, sw, sh, shm, scratch: Vec::new(), spare: Vec::new(), monitor: None, checked: std::time::Instant::now(), pressed: Vec::new(), buttons: Vec::new(), window: Some(win), desktop })
        }

        /// Part of a drawable as BGRA: through shared memory when it fits, else the socket.
        fn capture(&mut self, drawable: u32, x: i16, y: i16, w: usize, h: usize) -> Option<&[u8]> {
            use x11rb::protocol::shm::ConnectionExt as _;
            if let Some((seg, addr, size)) = self.shm.filter(|s| s.2 >= w * h * 4) {
                if self.conn.shm_get_image(drawable, x, y, w as u16, h as u16, !0, ImageFormat::Z_PIXMAP.into(), seg, 0).ok().and_then(|c| c.reply().ok()).is_some() {
                    return Some(unsafe { std::slice::from_raw_parts(addr, (w * h * 4).min(size)) });
                }
            }
            self.scratch = self.conn.get_image(ImageFormat::Z_PIXMAP, drawable, x, y, w as u16, h as u16, !0).ok()?.reply().ok()?.data;
            Some(self.scratch.as_slice())
        }

        /// An app's window now: uncovered and on screen, that part of the
        /// screen (its menus show too); else its own picture, from Composite.
        fn grab_window(&mut self) -> Option<Option<&[u8]>> {
            let win = self.window.as_ref()?;
            let (client, frame, composite) = (win.client, win.frame, win.composite);
            self.conn.get_geometry(client).ok()?.reply().ok()?;
            if prop32(&self.conn, client, win.atoms.state).contains(&win.atoms.hidden) {
                return Some(None);
            }
            let rect = (self.ox, self.oy, self.sw, self.sh);
            let on_screen = rect.0 >= 0 && rect.1 >= 0 && rect.0 as usize + rect.2 <= self.desktop.0 && rect.1 as usize + rect.3 <= self.desktop.1;
            if on_screen && !covered(&self.conn, self.root, frame, rect) {
                let root = self.root;
                return Some(self.capture(root, rect.0, rect.1, rect.2, rect.3));
            }
            if !composite {
                return Some(None);
            }
            let fg = self.conn.get_geometry(frame).ok()?.reply().ok()?;
            let pixmap = self.conn.generate_id().ok()?;
            if self.conn.composite_name_window_pixmap(frame, pixmap).ok().and_then(|c| c.check().ok()).is_none() {
                return Some(None);
            }
            let image = self.conn.get_image(ImageFormat::Z_PIXMAP, pixmap, rect.0 - fg.x, rect.1 - fg.y, rect.2 as u16, rect.3 as u16, !0).ok().and_then(|c| c.reply().ok());
            let _ = self.conn.free_pixmap(pixmap);
            match image {
                Some(i) if i.data.len() >= rect.2 * rect.3 * 4 => {
                    self.scratch = i.data;
                    Some(Some(self.scratch.as_slice()))
                }
                _ => Some(None),
            }
        }

        /// Brings an app's window to the front, uncovered, so clicks and keys reach it.
        fn raise(&self, win: &Win) {
            let rect = (self.ox, self.oy, self.sw, self.sh);
            if in_front(&self.conn, self.root, &win.atoms, win.client) && !covered(&self.conn, self.root, win.frame, rect) {
                return;
            }
            activate(&self.conn, self.root, &win.atoms, win.client);
            // The window manager does it in a moment.
            std::thread::sleep(std::time::Duration::from_millis(40));
        }

        /// Resizes an app's window so what shows is `w`×`h` (points: the
        /// desktop's scale makes them pixels).
        fn resize_window(&self, win: &mut Win, e: &Value) {
            if win.restore.is_none()
                && let Some(g) = self.conn.get_geometry(win.client).ok().and_then(|c| c.reply().ok())
            {
                let state = prop32(&self.conn, win.client, win.atoms.state);
                win.restore = Some((g.width as u32, g.height as u32, state.contains(&win.atoms.max_h) && state.contains(&win.atoms.max_v)));
            }
            let want = |k: &str, min: f64, max: usize| (e[k].as_f64().unwrap_or(0.).max(min) * win.scale).min(max as f64).round() as i32;
            let (w, h) = (want("w", 240., self.desktop.0), want("h", 160., self.desktop.1));
            let a = &win.atoms;
            // A maximized window keeps its size: make it an ordinary one first.
            let state = ClientMessageEvent::new(32, win.client, a.state, [0u32, a.max_h, a.max_v, 2, 0]);
            let _ = self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, state);
            let (cw, ch) = (w - win.added[0] - win.added[1], h - win.added[2] - win.added[3]);
            if cw > 0 && ch > 0 {
                let _ = self.conn.configure_window(win.client, &ConfigureWindowAux::new().width(cw as u32).height(ch as u32));
            }
            let _ = self.conn.flush();
        }
    }

    fn attach_shm(conn: &impl Connection, size: usize) -> Option<(u32, *mut u8, usize)> {
        use x11rb::protocol::shm::ConnectionExt as _;
        conn.shm_query_version().ok()?.reply().ok()?;
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            return None;
        }
        let addr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        // Freed once both sides let go.
        let attached = (addr as isize != -1)
            .then(|| conn.generate_id().ok())
            .flatten()
            .filter(|seg| conn.shm_attach(*seg, id as u32, false).ok().and_then(|c| c.check().ok()).is_some());
        unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        attached.map(|seg| (seg, addr as *mut u8, size))
    }

    impl super::Screen for X11 {
        fn size(&self) -> (usize, usize) {
            (self.sw, self.sh)
        }

        fn grab(&mut self) -> Option<Option<&[u8]>> {
            use x11rb::protocol::shm::ConnectionExt as _;
            if self.window.is_some() {
                return self.grab_window();
            }
            // Monitors get resized and moved: follow one that moved; one that
            // changed size (or went) ends the stream, for one at its new size.
            if self.checked.elapsed() >= std::time::Duration::from_secs(1) {
                self.checked = std::time::Instant::now();
                let (ox, oy, sw, sh) = area(&self.conn, self.root, self.monitor.as_deref()).ok()?;
                if (sw, sh) != (self.sw, self.sh) {
                    eprintln!("screen: the monitor is now {sw}x{sh}");
                    return None;
                }
                (self.ox, self.oy) = (ox, oy);
            }
            let (w, h) = (self.sw as u16, self.sh as u16);
            if let Some((seg, addr, size)) = self.shm {
                if self.conn.shm_get_image(self.root, self.ox, self.oy, w, h, !0, ImageFormat::Z_PIXMAP.into(), seg, 0).ok().and_then(|c| c.reply().ok()).is_some() {
                    return Some(Some(unsafe { std::slice::from_raw_parts(addr, size) }));
                }
                // Shared memory stopped working: copy through the socket from now on.
                eprintln!("screen: shared-memory capture failed; using GetImage");
                self.shm = None;
            }
            self.scratch = self.conn.get_image(ImageFormat::Z_PIXMAP, self.root, self.ox, self.oy, w, h, !0).ok()?.reply().ok()?.data;
            Some(Some(self.scratch.as_slice()))
        }

        fn flush(&mut self) {
            let _ = self.conn.flush();
        }

        fn info(&self) -> serde_json::Map<String, Value> {
            let mut info = serde_json::Map::new();
            if let Some(w) = &self.window {
                info.insert("title".into(), name_of(&self.conn, &w.atoms, w.client).into());
                info.insert("scale".into(), w.scale.into());
            }
            info
        }

        /// An app's window resized for its window in the app: back as it was,
        /// while still connected (a window manager can drop a request from a
        /// client that goes right after sending it).
        fn linger(&mut self) {
            let Some(win) = self.window.as_mut() else { return };
            let Some((w, h, maximized)) = win.restore.take() else { return };
            eprintln!("screen: window {}: back to {w}x{h}", win.client);
            let _ = self.conn.configure_window(win.client, &ConfigureWindowAux::new().width(w).height(h));
            if maximized {
                let a = &win.atoms;
                let state = ClientMessageEvent::new(32, win.client, a.state, [1u32, a.max_h, a.max_v, 2, 0]);
                let _ = self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, state);
            }
            // The window manager does it in a moment.
            let _ = self.conn.get_input_focus().map(|c| c.reply());
            std::thread::sleep(std::time::Duration::from_millis(300));
        }

        fn resized(&mut self) -> bool {
            let Some(win) = self.window.as_mut() else { return false };
            let Some(((x, y, w, h), frame_extents, added)) = bounds(&self.conn, &win.atoms, win.client, self.root) else { return false };
            (win.frame_extents, win.added) = (frame_extents, added);
            (self.ox, self.oy) = (x, y);
            if (w, h) == (self.sw, self.sh) {
                return false;
            }
            (self.sw, self.sh) = (w, h);
            win.frame = frame_of(&self.conn, self.root, win.client);
            true
        }

        fn meta(&mut self) -> Option<Value> {
            let rect = (self.ox, self.oy, self.sw, self.sh);
            let win = self.window.as_mut()?;
            let mut changed = serde_json::Map::new();
            let minimized = prop32(&self.conn, win.client, win.atoms.state).contains(&win.atoms.hidden);
            if win.minimized != Some(minimized) {
                win.minimized = Some(minimized);
                changed.insert("minimized".into(), minimized.into());
            }
            // The pointer's shape every few frames: it costs a round trip.
            win.ticks += 1;
            if win.ticks % 3 == 0
                && let Some(kind) = pointer_over(&self.conn, self.root, rect).filter(|k| *k != win.cursor)
            {
                win.cursor = kind;
                changed.insert("cursor".into(), kind.into());
            }
            let drag = title_bar(win.frame_extents, rect.2);
            if win.drag.as_ref() != Some(&drag) {
                changed.insert("drag".into(), json!(drag));
                win.drag = Some(drag);
            }
            (!changed.is_empty()).then_some(Value::Object(changed))
        }

        fn apply(&mut self, e: &Value, frame: (usize, usize)) {
            let kind = e["t"].as_str().unwrap_or("");
            let down = e["down"].as_bool().unwrap_or(false);
            if let Some(mut win) = self.window.take() {
                // Clicks, the wheel and keys are for this window: bring it to
                // the front first. Keys only go when it's there, never elsewhere.
                let go = match kind {
                    "button" if down => {
                        self.raise(&win);
                        true
                    }
                    "scroll" => {
                        self.raise(&win);
                        true
                    }
                    "key" | "text" if down || kind == "text" => {
                        self.raise(&win);
                        super::comes_to_front(|| in_front(&self.conn, self.root, &win.atoms, win.client))
                    }
                    "resize" => {
                        self.resize_window(&mut win, e);
                        false
                    }
                    "restore" => {
                        activate(&self.conn, self.root, &win.atoms, win.client);
                        false
                    }
                    _ => true,
                };
                self.window = Some(win);
                if !go {
                    return;
                }
            }
            // While a button is held, input maps from where the window was when it went down.
            if let Some(win) = self.window.as_mut() {
                if kind == "button" && down {
                    win.held_at.get_or_insert((self.ox, self.oy));
                }
            }
            let held_at = self.window.as_ref().and_then(|w| w.held_at);
            self.apply_input(e, frame, held_at.unwrap_or((self.ox, self.oy)));
            if let Some(win) = self.window.as_mut().filter(|_| kind == "button" && !down && self.buttons.is_empty()) {
                win.held_at = None;
            }
        }
    }

    impl X11 {
        /// Input on the desktop, with the shown area's top left at (`ox`, `oy`).
        fn apply_input(&mut self, e: &Value, frame: (usize, usize), (ox, oy): (i16, i16)) {
            let (conn, root) = (&self.conn, self.root);
            let scale = (self.sw as f64 / frame.0 as f64, self.sh as f64 / frame.1 as f64);
            let at = |e: &Value| ((e["x"].as_f64().unwrap_or(0.) * scale.0) as i16 + ox, (e["y"].as_f64().unwrap_or(0.) * scale.1) as i16 + oy);
            let fake = |kind: u8, detail: u8, x: i16, y: i16| {
                let _ = conn.xtest_fake_input(kind, detail, 0, root, x, y, 0);
            };
            match e["t"].as_str().unwrap_or("") {
                "move" => {
                    let (x, y) = at(e);
                    fake(MOTION_NOTIFY_EVENT, 0, x, y);
                }
                "button" => {
                    let (x, y) = at(e);
                    fake(MOTION_NOTIFY_EVENT, 0, x, y);
                    let b = e["b"].as_u64().unwrap_or(1).clamp(1, 9) as u8;
                    let down = e["down"].as_bool().unwrap_or(false);
                    fake(if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, b, 0, 0);
                    self.buttons.retain(|x| *x != b);
                    if down {
                        self.buttons.push(b);
                    }
                }
                "scroll" => {
                    let (x, y) = at(e);
                    fake(MOTION_NOTIFY_EVENT, 0, x, y);
                    // Wheel buttons: 4 up, 5 down, 6 left, 7 right; one click per notch.
                    for (amount, less, more) in [(e["dy"].as_i64().unwrap_or(0), 4u8, 5u8), (e["dx"].as_i64().unwrap_or(0), 6, 7)] {
                        let button = if amount < 0 { less } else { more };
                        for _ in 0..amount.unsigned_abs().min(20) {
                            fake(BUTTON_PRESS_EVENT, button, 0, 0);
                            fake(BUTTON_RELEASE_EVENT, button, 0, 0);
                        }
                    }
                }
                "text" => {
                    // Typing, for agents and Option's characters: each one's key on
                    // this keyboard, with the Shift and AltGr it needs, else a spare key.
                    let typing = Typing::read(conn);
                    let modifier = |sym: u32| typing.keys.get(&sym).map(|(code, _)| *code);
                    let (shift, altgr) = (modifier(0xffe1), modifier(0xfe03));
                    for ch in e["s"].as_str().unwrap_or("").chars() {
                        let sym = char_keysym(ch);
                        let key = self.spare.iter().find(|(_, s)| *s == sym).map(|(code, _)| (*code, 0));
                        let key = key.or_else(|| typing.keys.get(&sym).copied().filter(|(_, level)| *level < 2 || altgr.is_some()));
                        let Some((code, level)) = key.or_else(|| spare_key(conn, &mut self.spare, &typing.unused, sym).map(|code| (code, 0))) else { continue };
                        let held: Vec<u8> = [(level & 1 == 1).then_some(shift).flatten(), (level >= 2).then_some(altgr).flatten()].into_iter().flatten().collect();
                        for m in &held {
                            fake(KEY_PRESS_EVENT, *m, 0, 0);
                        }
                        fake(KEY_PRESS_EVENT, code, 0, 0);
                        fake(KEY_RELEASE_EVENT, code, 0, 0);
                        for m in held.iter().rev() {
                            fake(KEY_RELEASE_EVENT, *m, 0, 0);
                        }
                        let _ = conn.flush();
                    }
                }
                "key" => {
                    let sym = e["keysym"].as_u64().unwrap_or(0) as u32;
                    let typed = super::typed_char(sym).is_some();
                    if !e["down"].as_bool().unwrap_or(false) {
                        // The key it pressed. A character it typed with help is let go already.
                        let code = match self.pressed.iter().position(|(_, s)| *s == sym) {
                            Some(i) => Some(self.pressed.remove(i).0),
                            None if typed => None,
                            None => Typing::read(conn).keys.get(&sym).map(|(code, _)| *code),
                        };
                        if let Some(code) = code {
                            fake(KEY_RELEASE_EVENT, code, 0, 0);
                        }
                        return;
                    }
                    let typing = Typing::read(conn);
                    let modifier = |sym: u32| typing.keys.get(&sym).map(|(code, _)| *code);
                    let (shift, altgr) = (modifier(0xffe1), modifier(0xfe03));
                    let found = typing.keys.get(&sym).copied().filter(|(_, level)| *level < 2 || altgr.is_some());
                    let found = found.or_else(|| typed.then(|| spare_key(conn, &mut self.spare, &typing.unused, sym)).flatten().map(|code| (code, 0)));
                    let Some((code, level)) = found else { return };
                    let shifts: Vec<u8> = self.pressed.iter().filter(|(_, s)| matches!(s, 0xffe1 | 0xffe2)).map(|(code, _)| *code).collect();
                    // A spare key has the character on both levels: Shift can stay as it is.
                    let help = if self.spare.iter().any(|(_, s)| *s == sym) { Default::default() } else { super::level_help(level, !shifts.is_empty(), e["char"].as_bool().unwrap_or(false)) };
                    if help == super::LevelHelp::default() {
                        // Really held, so the desktop repeats it as it does any key.
                        fake(KEY_PRESS_EVENT, code, 0, 0);
                        self.pressed.retain(|(c, _)| *c != code);
                        self.pressed.push((code, sym));
                    } else {
                        // Pressed and let go at once, or the desktop would repeat it
                        // without its help (`@` then `2222`), and the next key typed
                        // before this one is let go would get it (`@` then `A`). The
                        // sender's own repeats type it again.
                        let lifted: &[u8] = if help.lift_shift { &shifts } else { &[] };
                        let added: Vec<u8> = [help.press_shift.then_some(shift).flatten(), help.altgr.then_some(altgr).flatten()].into_iter().flatten().collect();
                        for s in lifted {
                            fake(KEY_RELEASE_EVENT, *s, 0, 0);
                        }
                        for m in &added {
                            fake(KEY_PRESS_EVENT, *m, 0, 0);
                        }
                        fake(KEY_PRESS_EVENT, code, 0, 0);
                        fake(KEY_RELEASE_EVENT, code, 0, 0);
                        for m in added.iter().rev() {
                            fake(KEY_RELEASE_EVENT, *m, 0, 0);
                        }
                        for s in lifted {
                            fake(KEY_PRESS_EVENT, *s, 0, 0);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const NONE: u32 = 0;
        const AT: u32 = 0x40;
        const OMEGA: u32 = 0x07d9;
        const BRACKET: u32 = 0x5b;
        const BRACE: u32 = 0x7b;
        const LEVEL3_SHIFT: u32 = 0xfe03;

        /// As `xmodmap -pke` shows them on a German keyboard, 7 columns a key,
        /// from keycode 24: q, 7's, 8, one with nothing on it, AltGr.
        fn german() -> Typing {
            #[rustfmt::skip]
            let syms = [
                'q' as u32, 'Q' as u32, 'q' as u32, 'Q' as u32, AT, OMEGA, AT,
                '7' as u32, '/' as u32, '7' as u32, '/' as u32, BRACE, 0x2c6, BRACE,
                '8' as u32, '(' as u32, '8' as u32, '(' as u32, BRACKET, 0xad3, BRACKET,
                NONE, NONE, NONE, NONE, NONE, NONE, NONE,
                LEVEL3_SHIFT, NONE, LEVEL3_SHIFT, NONE, NONE, NONE, NONE,
            ];
            Typing::from_mapping(24, 7, &syms)
        }

        #[test]
        fn finds_what_altgr_types_on_a_german_keyboard() {
            let typing = german();
            assert_eq!(typing.keys[&('q' as u32)], (24, 0));
            assert_eq!(typing.keys[&('Q' as u32)], (24, 1));
            assert_eq!(typing.keys[&AT], (24, 2));
            assert_eq!(typing.keys[&OMEGA], (24, 3));
            assert_eq!(typing.keys[&BRACE], (25, 2));
            assert_eq!(typing.keys[&BRACKET], (26, 2));
            assert_eq!(typing.keys[&LEVEL3_SHIFT], (28, 0));
        }

        #[test]
        fn a_second_layouts_columns_are_not_levels() {
            let typing = Typing::from_mapping(10, 4, &['a' as u32, 'A' as u32, 0x6c1, 0x6e1]);
            assert_eq!(typing.keys[&('A' as u32)], (10, 1));
            assert!(!typing.keys.contains_key(&0x6c1));
        }

        #[test]
        fn the_lowest_level_wins_whichever_key_has_it() {
            // `<` with AltGr on one key, by itself on a later one.
            let typing = Typing::from_mapping(10, 6, &['z' as u32, NONE, NONE, NONE, '<' as u32, NONE, '<' as u32, '>' as u32, NONE, NONE, NONE, NONE]);
            assert_eq!(typing.keys[&('<' as u32)], (11, 0));
        }

        #[test]
        fn unused_keycodes_are_those_with_nothing_on_them_highest_first() {
            assert_eq!(german().unused, vec![27]);
            let typing = Typing::from_mapping(8, 2, &[NONE, NONE, 'a' as u32, NONE, NONE, NONE]);
            assert_eq!(typing.unused, vec![10, 8]);
        }

        #[test]
        fn a_short_mapping_is_fine() {
            let typing = Typing::from_mapping(8, 2, &['a' as u32, 'A' as u32]);
            assert_eq!(typing.keys[&('A' as u32)], (8, 1));
            assert!(typing.unused.is_empty());
            assert!(Typing::from_mapping(8, 0, &[]).keys.is_empty());
        }

        #[test]
        fn keysyms_of_characters() {
            assert_eq!(char_keysym('@'), 0x40);
            assert_eq!(char_keysym('\n'), 0xff0d);
            assert_eq!(char_keysym('\t'), 0xff09);
            // Latin-1 is its code point, the rest Unicode keysyms; the Euro's is its own.
            assert_eq!(char_keysym('«'), 0xab);
            assert_eq!(char_keysym('€'), 0x20ac);
            assert_eq!(char_keysym('ƒ'), 0x0100_0192);
            assert_eq!(char_keysym('ñ'), 0xf1);
            assert_eq!(char_keysym('😀'), 0x0101_f600);
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use serde_json::{Value, json};

    type FrameFn = extern "C" fn(*mut std::ffi::c_void, *const u8, usize, usize, usize);

    unsafe extern "C" {
        fn muxen_display(id: u32, pixels_w: *mut usize, pixels_h: *mut usize, points_w: *mut f64, points_h: *mut f64, x: *mut f64, y: *mut f64);
        fn muxen_capture_allowed(ask: i32) -> i32;
        fn muxen_input_allowed(ask: i32) -> i32;
        fn muxen_capture_start(id: u32, fps: i32, width: usize, height: usize, f: FrameFn, ctx: *mut std::ffi::c_void, err: *mut u8, errlen: usize) -> i32;
        fn muxen_capture_stopped() -> i32;
        fn muxen_capture_stop();
        fn muxen_capture_idle();
        fn muxen_mouse(kind: u32, x: f64, y: f64, button: u32, clicks: i64);
        fn muxen_scroll(dy: i32, dx: i32);
        fn muxen_key(keycode: u16, down: i32, flags: u64);
        fn muxen_text(utf16: *const u16, len: usize);
        fn muxen_key_layout(keys: *mut [u16; 4], count: usize) -> i32;
        fn muxen_capture_window_start(id: u32, fps: i32, width: usize, height: usize, f: FrameFn, ctx: *mut std::ffi::c_void, err: *mut u8, errlen: usize) -> i32;
        fn muxen_capture_follow(x: f64, y: f64, w: f64, h: f64, pw: usize, ph: usize);
        fn muxen_windows_json(buf: *mut u8, len: usize) -> usize;
        fn muxen_window_info(id: u32, x: *mut f64, y: *mut f64, w: *mut f64, h: *mut f64, pid: *mut i32, on_screen: *mut i32) -> i32;
        fn muxen_window_title(id: u32, buf: *mut u8, len: usize) -> usize;
        fn muxen_app_in_front(pid: i32) -> i32;
        fn muxen_window_raise(id: u32, pid: i32);
        fn muxen_window_resize(id: u32, pid: i32, w: f64, h: f64);
        fn muxen_drags_at(pid: i32, x: f64, y: f64) -> i32;
        fn muxen_cursor_kind() -> *const std::ffi::c_char;
        fn muxen_scale_at(x: f64, y: f64) -> f64;
        fn muxen_pointer(x: *mut f64, y: *mut f64);
    }

    /// x, y, width, height, in points.
    type Rect = (f64, f64, f64, f64);

    /// One window's place and size, its app, and whether it's on screen (not
    /// minimized or hidden); None when it's gone.
    fn window_info(id: u32) -> Option<(Rect, i32, bool)> {
        let (mut x, mut y, mut w, mut h, mut pid, mut on) = (0., 0., 0., 0., 0, 0);
        (unsafe { muxen_window_info(id, &mut x, &mut y, &mut w, &mut h, &mut pid, &mut on) } == 1).then_some(((x, y, w, h), pid, on == 1))
    }

    /// `muxen-agent windows` on a Mac: the apps' windows on screen, front first.
    pub fn windows() -> Value {
        let mut buf = vec![0u8; 256 * 1024];
        let n = unsafe { muxen_windows_json(buf.as_mut_ptr(), buf.len()) };
        if n >= buf.len() {
            buf = vec![0u8; n + 1];
            unsafe { muxen_windows_json(buf.as_mut_ptr(), buf.len()) };
        }
        let list: Vec<Value> = serde_json::from_slice(&buf[..n.min(buf.len())]).unwrap_or_default();
        let mut front_seen = false;
        let out: Vec<Value> = list
            .into_iter()
            .map(|w| {
                let scale = w["scale"].as_f64().unwrap_or(1.);
                // Front first: the first window of the app in front is the window in front.
                let front = !front_seen && unsafe { muxen_app_in_front(w["pid"].as_i64().unwrap_or(0) as i32) } == 1;
                front_seen |= front;
                json!({
                    "id": w["id"].as_u64().unwrap_or(0).to_string(),
                    "title": w["title"],
                    "app": w["app"],
                    "width": (w["width"].as_f64().unwrap_or(0.) * scale).round() as i64,
                    "height": (w["height"].as_f64().unwrap_or(0.) * scale).round() as i64,
                    "minimized": false,
                    "front": front,
                })
            })
            .collect();
        json!({ "ok": true, "windows": out })
    }

    /// The newest frame from ScreenCaptureKit, and whether it's new.
    #[derive(Default)]
    struct Latest {
        pixels: Vec<u8>,
        size: (usize, usize),
        fresh: bool,
    }

    extern "C" fn on_frame(ctx: *mut std::ffi::c_void, bgra: *const u8, width: usize, height: usize, stride: usize) {
        let latest = unsafe { &*(ctx as *const Mutex<Latest>) };
        let mut l = latest.lock().unwrap();
        l.pixels.resize(width * height * 4, 0);
        l.size = (width, height);
        let src = unsafe { std::slice::from_raw_parts(bgra, stride * height) };
        for y in 0..height {
            l.pixels[y * width * 4..(y + 1) * width * 4].copy_from_slice(&src[y * stride..y * stride + width * 4]);
        }
        l.fresh = true;
    }

    /// One of a Mac's displays: captured at the frame's size by ScreenCaptureKit,
    /// input through CoreGraphics events.
    pub struct Mac {
        /// The frame size ScreenCaptureKit delivers.
        fw: usize,
        fh: usize,
        /// The display in points (input coordinates), and where it sits on the desktop.
        points: (f64, f64),
        origin: (f64, f64),
        latest: Arc<Mutex<Latest>>,
        frame: Vec<u8>,
        input: bool,
        /// Buttons held (for drags), modifier flags held, and the last click (for double clicks).
        held: [bool; 3],
        flags: u64,
        last_click: Option<(Instant, u32, f64, f64, i64)>,
        /// Keys pressed and not yet let go (keycode, the keysym that pressed it),
        /// and where the pointer was: let go when the stream ends, so none stays held.
        pressed: Vec<(u16, u32)>,
        at: (f64, f64),
        /// An app's window shown on its own, and the widest frame asked for.
        window: Option<MacWin>,
        max_width: usize,
    }

    /// An app's window shown on its own (`window:<id>`, a CGWindowID from `windows`).
    struct MacWin {
        id: u32,
        pid: i32,
        /// Pixels per point on its display.
        scale: f64,
        /// Where it was when a button went down: input maps from there until let go.
        held_at: Option<(f64, f64)>,
        /// What the app was last told (see `meta`), and the drag areas (in
        /// points) from a thread that asks the app for them now and then.
        cursor: &'static str,
        minimized: Option<bool>,
        drag: Arc<Mutex<Option<Vec<[i32; 4]>>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        ticks: u32,
        /// Its size (points) before the app first resized it: so it's left
        /// again when the stream ends.
        restore: Option<(f64, f64)>,
    }

    impl Drop for MacWin {
        fn drop(&mut self) {
            eprintln!("screen: window {}: ending", self.id);
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            // Resized for its window in the app: back as it was.
            if let Some((w, h)) = self.restore {
                unsafe { muxen_window_resize(self.id, self.pid, w, h) };
            }
        }
    }

    /// Runs `start` once no other capture is starting on this Mac: of two
    /// started at the same moment, ScreenCaptureKit can leave one never
    /// starting (a desktop and an app's window opened together, say).
    fn one_start_at_a_time<T>(start: impl FnOnce() -> T) -> T {
        use std::os::fd::AsRawFd;
        let lock = std::fs::OpenOptions::new().create(true).append(true).open(crate::home_dir().join("capture.lock")).ok();
        if let Some(f) = &lock {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        }
        // (Let go when `lock` closes.)
        start()
    }

    /// Frame size for a window of `points` at `scale`, no wider than `max_width`.
    fn window_frame((w, h): (f64, f64), scale: f64, max_width: usize) -> (usize, usize) {
        let (pw, ph) = ((w * scale).round().max(1.) as usize, (h * scale).round().max(1.) as usize);
        let fw = max_width.min(pw).max(1);
        (fw, (ph * fw / pw).max(1))
    }

    impl Drop for Mac {
        fn drop(&mut self) {
            // ScreenCaptureKit is told it's over, not left to find out.
            unsafe { muxen_capture_stop() };
            captures::forget();
            self.let_go();
        }
    }

    /// This Mac's screen captures, a file each in `~/.muxen/captures`, named
    /// by pid: "on" while it streams, "idle" while it lingers. On a Mac, once
    /// a capture ends while another goes on, no new one starts until all have
    /// ended (ScreenCaptureKit, macOS 26-27): so none ends while others stream.
    mod captures {
        use std::path::PathBuf;

        fn dir() -> PathBuf {
            crate::home_dir().join("captures")
        }

        pub fn mark(state: &str) {
            let _ = std::fs::create_dir_all(dir());
            let _ = std::fs::write(dir().join(std::process::id().to_string()), state);
        }

        pub fn forget() {
            let _ = std::fs::remove_file(dir().join(std::process::id().to_string()));
        }

        /// Whether another capture here is streaming (gone ones are forgotten).
        pub fn others_streaming() -> bool {
            let me = std::process::id();
            let Ok(entries) = std::fs::read_dir(dir()) else { return false };
            entries.flatten().any(|e| {
                let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()).filter(|p| *p != me) else { return false };
                if unsafe { libc::kill(pid as i32, 0) } != 0 {
                    let _ = std::fs::remove_file(e.path());
                    return false;
                }
                std::fs::read_to_string(e.path()).is_ok_and(|s| s == "on")
            })
        }
    }

    impl Mac {
        /// Display `id` (0: the main one).
        pub fn open(fps: u32, max_width: u32, id: u32) -> Result<Mac, String> {
            let (mut pw, mut ph, mut ptw, mut pth, mut ox, mut oy) = (0usize, 0usize, 0f64, 0f64, 0f64, 0f64);
            unsafe { muxen_display(id, &mut pw, &mut ph, &mut ptw, &mut pth, &mut ox, &mut oy) };
            if pw == 0 || ph == 0 {
                return Err("No display here.".into());
            }
            if unsafe { muxen_capture_allowed(1) } == 0 {
                return Err(format!("On {}, allow {} under Privacy & Security › Screen Recording, then try again.", computer_name(), listed_as()));
            }
            let input = unsafe { muxen_input_allowed(1) } == 1;
            let fw = (max_width as usize).min(pw).max(1);
            let fh = (ph * fw / pw).max(1);
            let latest = Arc::new(Mutex::new(Latest::default()));
            let mut err = [0u8; 512];
            // ScreenCaptureKit keeps calling with this pointer while the process lives.
            let ctx = Arc::into_raw(latest.clone()) as *mut std::ffi::c_void;
            let started = one_start_at_a_time(|| unsafe { muxen_capture_start(id, fps as i32, fw, fh, on_frame, ctx, err.as_mut_ptr(), err.len()) });
            if started != 0 {
                let why = String::from_utf8_lossy(&err[..err.iter().position(|b| *b == 0).unwrap_or(err.len())]).into_owned();
                return Err(format!("Can't capture the screen: {why}"));
            }
            captures::mark("on");
            Ok(Mac { fw, fh, points: (ptw, pth), origin: (ox, oy), latest, frame: Vec::new(), input, held: [false; 3], flags: 0, last_click: None, pressed: Vec::new(), at: (ox, oy), window: None, max_width: max_width as usize })
        }

        /// An app's window on its own (`id` from `windows`), captured even
        /// while covered, without the pointer (the app shows its own).
        pub fn open_window(fps: u32, max_width: u32, id: &str) -> Result<Mac, String> {
            let closed = || super::WINDOW_CLOSED.to_string();
            let id: u32 = id.parse().map_err(|_| closed())?;
            let (_, pid, on_screen) = window_info(id).ok_or_else(closed)?;
            if unsafe { muxen_capture_allowed(1) } == 0 {
                return Err(format!("On {}, allow {} under Privacy & Security › Screen Recording, then try again.", computer_name(), listed_as()));
            }
            let input = unsafe { muxen_input_allowed(1) } == 1;
            // Minimized or hidden, it isn't drawn: bring it back.
            if !on_screen {
                unsafe { muxen_window_raise(id, pid) };
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let ((x, y, w, h), _, _) = window_info(id).ok_or_else(closed)?;
            let scale = unsafe { muxen_scale_at(x + w / 2., y + h / 2.) }.max(1.);
            let (fw, fh) = window_frame((w, h), scale, max_width as usize);
            let latest = Arc::new(Mutex::new(Latest::default()));
            let mut err = [0u8; 512];
            let ctx = Arc::into_raw(latest.clone()) as *mut std::ffi::c_void;
            let asked = Instant::now();
            eprintln!("screen: window {id} ({fw}x{fh}): starting");
            let started = one_start_at_a_time(|| unsafe { muxen_capture_window_start(id, fps as i32, fw, fh, on_frame, ctx, err.as_mut_ptr(), err.len()) });
            eprintln!("screen: window {id}: start returned {started} after {:?}", asked.elapsed());
            match started {
                0 => {}
                2 => return Err(closed()),
                _ => {
                    let why = String::from_utf8_lossy(&err[..err.iter().position(|b| *b == 0).unwrap_or(err.len())]).into_owned();
                    return Err(format!("Can't capture the window: {why}"));
                }
            }
            let drag = Arc::new(Mutex::new(None));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let size = move || window_info(id).map(|((_, _, w, h), _, _)| (w.round() as i32, h.round() as i32));
            let areas = move || {
                let ((x, y, w, h), _, on) = window_info(id)?;
                let drags = |px: i32, py: i32| match unsafe { muxen_drags_at(pid, x + px as f64, y + py as f64) } {
                    -1 => None,
                    n => Some(n == 1),
                };
                // The top 52 points: a title bar, or a toolbar under or beside it.
                on.then(|| super::drag_grid(w.round() as i32, h.round() as i32, 8, 52, drags)).flatten()
            };
            // Asking an app about each point takes a while: now and then.
            super::watch_drag_areas(size, areas, std::time::Duration::from_secs(10), drag.clone(), stop.clone());
            let window = MacWin { id, pid, scale, held_at: None, cursor: "", minimized: None, drag, stop, ticks: 0, restore: None };
            captures::mark("on");
            Ok(Mac {
                fw,
                fh,
                points: (w, h),
                origin: (x, y),
                latest,
                frame: Vec::new(),
                input,
                held: [false; 3],
                flags: 0,
                last_click: None,
                pressed: Vec::new(),
                at: (x, y),
                window: Some(window),
                max_width: max_width as usize,
            })
        }

        fn point(&self, e: &Value, frame: (usize, usize)) -> (f64, f64) {
            let x = e["x"].as_f64().unwrap_or(0.) * self.points.0 / frame.0 as f64;
            let y = e["y"].as_f64().unwrap_or(0.) * self.points.1 / frame.1 as f64;
            // While a button is held on an app's window, from where it was when it went down.
            let origin = self.window.as_ref().and_then(|w| w.held_at).unwrap_or(self.origin);
            (origin.0 + x, origin.1 + y)
        }

        /// Lets go of every key and button it pressed there.
        fn let_go(&mut self) {
            for (code, _) in self.pressed.drain(..) {
                unsafe { muxen_key(code, 0, key_flags(code, 0)) };
            }
            let (x, y) = self.at;
            for (i, up) in [LEFT_UP, RIGHT_UP, OTHER_UP].into_iter().enumerate() {
                if std::mem::take(&mut self.held[i]) {
                    unsafe { muxen_mouse(up, x, y, i as u32, 1) };
                }
            }
        }

        /// Brings an app's window to the front, so clicks and keys reach it.
        fn raise(&self, win: &MacWin) {
            if unsafe { muxen_app_in_front(win.pid) } == 0 {
                unsafe { muxen_window_raise(win.id, win.pid) };
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }
    }

    // CGEventType values.
    const LEFT_DOWN: u32 = 1;
    const LEFT_UP: u32 = 2;
    const RIGHT_DOWN: u32 = 3;
    const RIGHT_UP: u32 = 4;
    const MOVED: u32 = 5;
    const LEFT_DRAGGED: u32 = 6;
    const RIGHT_DRAGGED: u32 = 7;
    const OTHER_DOWN: u32 = 25;
    const OTHER_UP: u32 = 26;
    const OTHER_DRAGGED: u32 = 27;

    impl super::Screen for Mac {
        fn size(&self) -> (usize, usize) {
            (self.fw, self.fh)
        }

        fn grab(&mut self) -> Option<Option<&[u8]>> {
            if let Some(win) = &self.window {
                // Gone: the stream ends. Minimized or hidden: nothing new.
                let (_, _, on_screen) = window_info(win.id)?;
                if !on_screen {
                    return Some(None);
                }
            }
            if unsafe { muxen_capture_stopped() } == 1 {
                return None;
            }
            let mut l = self.latest.lock().unwrap();
            // (After a resize, frames at the old size still arrive for a moment.)
            if !l.fresh || l.pixels.len() < self.fw * self.fh * 4 || (self.window.is_some() && l.size != (self.fw, self.fh)) {
                return Some(None);
            }
            l.fresh = false;
            std::mem::swap(&mut self.frame, &mut l.pixels);
            drop(l);
            Some(Some(self.frame.as_slice()))
        }

        /// Done with: its keys let go and its window back as it was now; then,
        /// while another capture here streams, it stays (a tiny frame a second)
        /// rather than end (see `captures`). An hour at most.
        fn linger(&mut self) {
            self.let_go();
            if let Some(win) = self.window.as_mut() {
                win.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some((w, h)) = win.restore.take() {
                    unsafe { muxen_window_resize(win.id, win.pid, w, h) };
                }
            }
            if !captures::others_streaming() {
                return;
            }
            captures::mark("idle");
            unsafe { muxen_capture_idle() };
            let since = Instant::now();
            while captures::others_streaming() && since.elapsed() < std::time::Duration::from_secs(3600) {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }

        fn resized(&mut self) -> bool {
            let Some(win) = &self.window else { return false };
            let Some(((x, y, w, h), _, true)) = window_info(win.id) else { return false };
            if ((x, y), (w, h)) == (self.origin, self.points) {
                return false;
            }
            (self.origin, self.points) = ((x, y), (w, h));
            let (fw, fh) = window_frame((w, h), win.scale, self.max_width);
            // (Captured through its display, the capture follows it as it moves too.)
            unsafe { muxen_capture_follow(x, y, w, h, fw, fh) };
            if (fw, fh) == (self.fw, self.fh) {
                return false;
            }
            (self.fw, self.fh) = (fw, fh);
            true
        }

        fn meta(&mut self) -> Option<Value> {
            let (origin, points, per_point) = (self.origin, self.points, self.fw as f64 / self.points.0.max(1.));
            let win = self.window.as_mut()?;
            let mut changed = serde_json::Map::new();
            let minimized = !window_info(win.id).is_some_and(|(_, _, on)| on);
            if win.minimized != Some(minimized) {
                win.minimized = Some(minimized);
                changed.insert("minimized".into(), minimized.into());
            }
            win.ticks += 1;
            if win.ticks % 3 == 0 {
                let (mut px, mut py) = (0., 0.);
                unsafe { muxen_pointer(&mut px, &mut py) };
                let over = px >= origin.0 && py >= origin.1 && px < origin.0 + points.0 && py < origin.1 + points.1;
                let kind = unsafe { std::ffi::CStr::from_ptr(muxen_cursor_kind()) }.to_str().unwrap_or("arrow");
                let kind: &'static str = ["text", "hand", "ew", "ns", "crosshair", "move"].into_iter().find(|k| *k == kind).unwrap_or("arrow");
                if over && kind != win.cursor {
                    win.cursor = kind;
                    changed.insert("cursor".into(), kind.into());
                }
            }
            // Its drag areas, from points to the screen's pixels.
            if let Some(drag) = win.drag.lock().unwrap().take() {
                let px: Vec<[f64; 4]> = drag.iter().map(|r| r.map(|v| v as f64 * per_point)).collect();
                changed.insert("drag".into(), json!(px));
            }
            (!changed.is_empty()).then_some(Value::Object(changed))
        }

        fn info(&self) -> serde_json::Map<String, Value> {
            let mut m = serde_json::Map::new();
            if let Some(win) = &self.window {
                let mut buf = [0u8; 512];
                let n = unsafe { muxen_window_title(win.id, buf.as_mut_ptr(), buf.len()) }.min(buf.len());
                m.insert("title".into(), String::from_utf8_lossy(&buf[..n]).into_owned().into());
                m.insert("scale".into(), (self.fw as f64 / self.points.0.max(1.)).into());
            }
            if !self.input {
                m.insert("input".into(), false.into());
                m.insert("note".into(), format!("view only: allow {} under Accessibility on {}", listed_as(), computer_name()).into());
            }
            m
        }

        fn apply(&mut self, e: &Value, frame: (usize, usize)) {
            let kind = e["t"].as_str().unwrap_or("");
            let down = e["down"].as_bool().unwrap_or(false);
            if let Some(mut win) = self.window.take() {
                // Clicks, the wheel and keys are for this window: its app comes
                // to the front first. Keys only go when it's there, never elsewhere.
                let go = match kind {
                    "button" if down => {
                        self.raise(&win);
                        true
                    }
                    "scroll" => {
                        self.raise(&win);
                        true
                    }
                    "key" | "text" if down || kind == "text" => {
                        self.raise(&win);
                        super::comes_to_front(|| unsafe { muxen_app_in_front(win.pid) } == 1)
                    }
                    "resize" => {
                        let (w, h) = (e["w"].as_f64().unwrap_or(0.).max(200.), e["h"].as_f64().unwrap_or(0.).max(120.));
                        if win.restore.is_none() {
                            win.restore = Some(self.points);
                        }
                        unsafe { muxen_window_resize(win.id, win.pid, w, h) };
                        false
                    }
                    "restore" => {
                        unsafe { muxen_window_raise(win.id, win.pid) };
                        false
                    }
                    _ => true,
                };
                if kind == "button" && down {
                    win.held_at.get_or_insert(self.origin);
                }
                self.window = Some(win);
                if !go {
                    return;
                }
            }
            self.apply_input(e, frame);
            if let Some(win) = self.window.as_mut().filter(|_| kind == "button" && !down && !self.held.iter().any(|h| *h)) {
                win.held_at = None;
            }
        }
    }

    impl Mac {
        fn apply_input(&mut self, e: &Value, frame: (usize, usize)) {
            match e["t"].as_str().unwrap_or("") {
                "move" => {
                    let (x, y) = self.point(e, frame);
                    self.at = (x, y);
                    let kind = if self.held[0] {
                        LEFT_DRAGGED
                    } else if self.held[1] {
                        RIGHT_DRAGGED
                    } else if self.held[2] {
                        OTHER_DRAGGED
                    } else {
                        MOVED
                    };
                    unsafe { muxen_mouse(kind, x, y, 0, 0) };
                }
                "button" => {
                    let (x, y) = self.point(e, frame);
                    let down = e["down"].as_bool().unwrap_or(false);
                    // X11 buttons: 1 left, 2 middle, 3 right.
                    let (index, button, kinds) = match e["b"].as_u64().unwrap_or(1) {
                        3 => (1, 1, (RIGHT_DOWN, RIGHT_UP)),
                        2 => (2, 2, (OTHER_DOWN, OTHER_UP)),
                        _ => (0, 0, (LEFT_DOWN, LEFT_UP)),
                    };
                    // Double clicks: the same button again, quickly, nearby.
                    let clicks = if down {
                        let n = match self.last_click {
                            Some((at, b, lx, ly, n)) if b == button && at.elapsed().as_millis() < 500 && (lx - x).abs() < 6. && (ly - y).abs() < 6. => n + 1,
                            _ => 1,
                        };
                        self.last_click = Some((Instant::now(), button, x, y, n));
                        n
                    } else {
                        self.last_click.map(|c| c.4).unwrap_or(1)
                    };
                    self.held[index] = down;
                    self.at = (x, y);
                    unsafe { muxen_mouse(if down { kinds.0 } else { kinds.1 }, x, y, button, clicks) };
                }
                "scroll" => {
                    let (x, y) = self.point(e, frame);
                    unsafe { muxen_mouse(MOVED, x, y, 0, 0) };
                    let (dy, dx) = (e["dy"].as_i64().unwrap_or(0).clamp(-20, 20) as i32, e["dx"].as_i64().unwrap_or(0).clamp(-20, 20) as i32);
                    unsafe { muxen_scroll(dy, dx) };
                }
                "text" => {
                    let utf16: Vec<u16> = e["s"].as_str().unwrap_or("").encode_utf16().collect();
                    if !utf16.is_empty() {
                        unsafe { muxen_text(utf16.as_ptr(), utf16.len()) };
                    }
                }
                "key" => {
                    let sym = e["keysym"].as_u64().unwrap_or(0) as u32;
                    let down = e["down"].as_bool().unwrap_or(false);
                    if let Some(flag) = modifier_flag(sym) {
                        if down {
                            self.flags |= flag;
                        } else {
                            self.flags &= !flag;
                        }
                    }
                    if !down {
                        // The key it pressed, as the layout may have changed since, or
                        // the one it names (`@` pressed, `2` let go). A character typed
                        // as text has no key to let go.
                        let pressed = self.pressed.iter().find(|(_, s)| *s == sym).map(|(code, _)| *code);
                        let code = pressed.or_else(|| {
                            let (code, _) = keycode(sym, self.flags, &Layout::current())?;
                            (super::typed_char(sym).is_none() || self.pressed.iter().any(|(c, _)| *c == code)).then_some(code)
                        });
                        if let Some(code) = code {
                            unsafe { muxen_key(code, 0, key_flags(code, self.flags)) };
                            self.pressed.retain(|(c, _)| *c != code);
                        }
                        return;
                    }
                    match mac_press(sym, self.flags, e["char"].as_bool().unwrap_or(false), &Layout::current()) {
                        MacPress::Key(code, flags) => {
                            unsafe { muxen_key(code, 1, key_flags(code, flags)) };
                            self.pressed.retain(|(c, _)| *c != code);
                            self.pressed.push((code, sym));
                        }
                        MacPress::Text => {
                            let unit = text_unit(sym, self.flags);
                            unsafe { muxen_text(&unit, 1) };
                        }
                        MacPress::Nothing => {}
                    }
                }
                _ => {}
            }
        }
    }

    /// What the Mac is called (System Settings › General › About).
    fn computer_name() -> String {
        std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "this Mac".into())
    }

    /// The name System Settings › Privacy & Security lists this agent under:
    /// Muxin Agent when it runs from its app (Muxin 0.10.0 and later), else
    /// the plain program's own name.
    fn listed_as() -> &'static str {
        let exe = std::env::current_exe().and_then(std::fs::canonicalize).unwrap_or_default();
        listed_name(&exe)
    }

    /// `~/.muxen/bin/muxen-agent` is a link: `exe` is where it leads.
    fn listed_name(exe: &std::path::Path) -> &'static str {
        if exe.to_string_lossy().contains("Muxin Agent.app/Contents/MacOS/") { "Muxin Agent" } else { "muxen-agent" }
    }

    /// The CGEventFlags bit of a modifier keysym.
    fn modifier_flag(sym: u32) -> Option<u64> {
        Some(match sym {
            0xffe1 | 0xffe2 => 0x20000,  // shift
            0xffe3 | 0xffe4 => 0x40000,  // control
            0xffe9 | 0xffea => 0x80000,  // option
            0xffeb | 0xffec | 0xffe7 => 0x100000, // command
            _ => return None,
        })
    }

    const FN: u64 = 0x800000;
    const KEYPAD: u64 = 0x200000;

    /// The flags a key's events carry: the modifiers held, and what a real
    /// keyboard adds for the key itself. Fn for the arrows, the keys above
    /// them (Home, End, Page Up, Page Down, Delete) and F1 to F12, and the
    /// keypad's for the arrows. Mission Control's shortcuts are defined with
    /// Fn, so the Dock doesn't take ⌃←, ⌃→ or ⌃↑ without it.
    fn key_flags(code: u16, flags: u64) -> u64 {
        match code {
            123..=126 => flags | FN | KEYPAD,
            96..=101 | 103 | 109 | 111 | 115..=122 => flags | FN,
            _ => flags,
        }
    }

    #[derive(Debug, PartialEq)]
    enum MacPress {
        /// A key, with these flags.
        Key(u16, u64),
        /// The character itself, whatever this Mac's layout.
        Text,
        Nothing,
    }

    const SHIFT: u64 = 0x20000;
    const COMMAND: u64 = 0x100000;
    /// Held, a key is a shortcut rather than a character typed.
    const SHORTCUT: u64 = 0x40000 | 0x80000 | COMMAND;

    /// What a Mac's keys type on a keyboard layout, by keycode: by itself, with
    /// Shift, with ⌘, with ⌘⇧. 0: nothing, or a dead key, which types nothing yet.
    struct Layout([[u16; 4]; 128]);

    impl Layout {
        /// This Mac's layout now, read for each key as it can change at any time.
        /// A US keyboard's when it can't say.
        fn current() -> Layout {
            let mut keys = [[0; 4]; 128];
            if unsafe { muxen_key_layout(keys.as_mut_ptr(), keys.len()) } == 1 { Layout(keys) } else { Layout::us() }
        }

        /// From what each of those four types, key after key: keycodes 0 to 50,
        /// without Return (36), Tab (48) and Space (49).
        fn from_levels(levels: [&str; 4]) -> Layout {
            let mut keys = [[0; 4]; 128];
            for (level, typed) in levels.iter().enumerate() {
                let codes = (0..=50).filter(|code| !matches!(code, 36 | 48 | 49));
                for (code, c) in codes.zip(typed.chars()) {
                    keys[code][level] = c as u16;
                }
            }
            Layout(keys)
        }

        /// A US keyboard, which has no key 10.
        fn us() -> Layout {
            const ALONE: &str = "asdfhgzxcv\0bqweryt123465=97-80]ou[iplj'k;\\,/nm.`";
            const WITH_SHIFT: &str = "ASDFHGZXCV\0BQWERYT!@#$^%+(&_*)}OU{IPLJ\"K:|<?NM>~";
            Layout::from_levels([ALONE, WITH_SHIFT, ALONE, ALONE])
        }

        /// The key for a character, and whether it wants Shift for it. Typing, what
        /// the keys type. A shortcut can use more: with ⌘ held many layouts go by
        /// other keys (Latin on a Russian one, QWERTY on "Dvorak - QWERTY ⌘"), so
        /// that comes first then, and dead keys don't wait.
        fn key_for(&self, c: u16, flags: u64) -> Option<(u16, bool)> {
            let levels: &[usize] = if flags & COMMAND != 0 {
                &[2, 0, 1, 3]
            } else if flags & SHORTCUT != 0 {
                &[0, 1, 2, 3]
            } else {
                &[0, 1]
            };
            levels.iter().find_map(|&level| {
                let code = self.0.iter().position(|key| key[level] == c)?;
                Some((code as u16, level % 2 == 1))
            })
        }
    }

    /// How to press a "key" event's keysym. A character goes as its key on this
    /// Mac's layout when Shift is as that key wants it (apps that read key codes
    /// get a real one), or when a shortcut needs a key, with Shift corrected; else
    /// as text, which needs no key. So does what the layout has only behind Option
    /// (`@` on a German one), as Windows does with AltGr: apps read Option as a
    /// shortcut, and terminals as Meta. `exact`: see `level_help`.
    fn mac_press(sym: u32, flags: u64, exact: bool, layout: &Layout) -> MacPress {
        let key = keycode(sym, flags, layout);
        let shortcut = flags & SHORTCUT != 0;
        if super::typed_char(sym).is_none() {
            return match key {
                Some((code, _)) => MacPress::Key(code, flags),
                // A letter this layout has no key for.
                None if !shortcut && (0x61..=0x7a).contains(&sym) => MacPress::Text,
                None => MacPress::Nothing,
            };
        }
        let shift_held = flags & SHIFT != 0;
        if shortcut {
            return key.map_or(MacPress::Nothing, |(code, wants_shift)| {
                let flags = if wants_shift { flags | SHIFT } else if exact { flags & !SHIFT } else { flags };
                MacPress::Key(code, flags)
            });
        }
        match key {
            Some((code, wants_shift)) if wants_shift == shift_held || (shift_held && !exact) => MacPress::Key(code, flags),
            _ => MacPress::Text,
        }
    }

    /// The character `MacPress::Text` types. A letter's keysym doesn't say Shift.
    fn text_unit(sym: u32, flags: u64) -> u16 {
        if flags & SHIFT != 0 { (sym as u8).to_ascii_uppercase() as u16 } else { sym as u16 }
    }

    /// An X keysym's macOS virtual keycode, and whether it wants Shift. Named keys
    /// are the same on every layout; a character's is where this layout has it.
    fn keycode(sym: u32, flags: u64, layout: &Layout) -> Option<(u16, bool)> {
        let code = match sym {
            0xff0d => 36,          // return
            0xff09 => 48,          // tab
            0x20 => 49,            // space
            0xff08 => 51,          // backspace
            0xff1b => 53,          // escape
            0xffff => 117,         // forward delete
            0xff50 => 115,         // home
            0xff57 => 119,         // end
            0xff55 => 116,         // page up
            0xff56 => 121,         // page down
            0xff51 => 123,         // left
            0xff53 => 124,         // right
            0xff54 => 125,         // down
            0xff52 => 126,         // up
            0xffe1 | 0xffe2 => 56, // shift
            0xffe3 | 0xffe4 => 59, // control
            0xffe9 | 0xffea => 58, // option
            0xffeb | 0xffec | 0xffe7 => 55, // command
            0xffbe..=0xffc9 => [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111][(sym - 0xffbe) as usize], // F1-F12
            _ => return layout.key_for(u16::try_from(sym).ok().filter(|c| *c != 0)?, flags),
        };
        Some((code, false))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn permissions_name_the_app_the_agent_runs_from() {
            let app = std::path::Path::new("/Users/me/.muxen/agent/c79755dff29f2948/Muxin Agent.app/Contents/MacOS/muxen-agent");
            assert_eq!(listed_name(app), "Muxin Agent");
            assert_eq!(listed_name(std::path::Path::new("/Users/me/.muxen/bin/muxen-agent")), "muxen-agent");
            assert_eq!(listed_name(std::path::Path::new("")), "muxen-agent");
        }

        // On a US keyboard, unless a test says otherwise.
        fn mac_press(sym: u32, flags: u64, exact: bool) -> MacPress {
            super::mac_press(sym, flags, exact, &Layout::us())
        }

        fn keycode(sym: u32) -> Option<u16> {
            super::keycode(sym, 0, &Layout::us()).map(|(code, _)| code)
        }

        #[test]
        fn arrows_and_function_keys_carry_fn_as_from_a_keyboard() {
            const CONTROL: u64 = 0x40000;
            // ⌃→ (next Space) and ⌃↑ (Mission Control): Control, Fn and the keypad's.
            assert_eq!(key_flags(keycode(0xff53).unwrap(), CONTROL), 0xa40000);
            assert_eq!(key_flags(keycode(0xff52).unwrap(), CONTROL), 0xa40000);
            // Let go with nothing held, they still say which key it was.
            assert_eq!(key_flags(keycode(0xff51).unwrap(), 0), FN | KEYPAD);
            // Home, End, Page Up, Page Down, Delete and F1 to F12: Fn alone.
            for sym in [0xff50, 0xff57, 0xff55, 0xff56, 0xffff].into_iter().chain(0xffbe..=0xffc9) {
                assert_eq!(key_flags(keycode(sym).unwrap(), 0), FN, "{sym:#x}");
            }
            // Letters, Return, Tab, Space, Escape and the modifiers themselves: as held.
            for sym in ['a' as u32, 0xff0d, 0xff09, 0x20, 0xff1b, 0xff08, 0xffe3, 0xffeb] {
                assert_eq!(key_flags(keycode(sym).unwrap(), COMMAND), COMMAND, "{sym:#x}");
            }
        }

        #[test]
        fn maps_keys() {
            assert_eq!(keycode('a' as u32), Some(0));
            assert_eq!(keycode('A' as u32), Some(0));
            assert_eq!(keycode(0xff0d), Some(36));
            assert_eq!(keycode(0xffbe), Some(122));
            assert_eq!(keycode('!' as u32), Some(18));
            assert_eq!(modifier_flag(0xffeb), Some(0x100000));
        }

        const SHIFT: u64 = 0x20000;
        const CTRL: u64 = 0x40000;
        const CMD: u64 = 0x100000;

        #[test]
        fn a_character_is_its_key_when_shift_matches_a_us_keyboard() {
            // Shift+2 from a US Mac, and a plain 2: real keys, for apps that read key codes.
            assert_eq!(mac_press('@' as u32, SHIFT, true), MacPress::Key(19, SHIFT));
            assert_eq!(mac_press('2' as u32, 0, true), MacPress::Key(19, 0));
            assert_eq!(mac_press('/' as u32, 0, true), MacPress::Key(44, 0));
        }

        #[test]
        fn a_character_is_text_when_a_us_keyboard_wouldnt_type_it_that_way() {
            // Shift+7 from a German Mac: Shift's down, a US `/` has none.
            assert_eq!(mac_press('/' as u32, SHIFT, true), MacPress::Text);
            // `#` from a German Mac, no Shift there.
            assert_eq!(mac_press('#' as u32, 0, true), MacPress::Text);
            // Not on a US keyboard at all.
            for c in ['ö', 'Ö', '§'] {
                assert_eq!(mac_press(c as u32, SHIFT, true), MacPress::Text, "{c}");
                assert_eq!(mac_press(c as u32, 0, true), MacPress::Text, "{c}");
            }
        }

        #[test]
        fn shortcuts_keep_their_keys_with_shift_as_a_us_keyboard_has_it() {
            // ⌘⇧3 and ⌘⇧] from a US Mac.
            assert_eq!(mac_press('#' as u32, CMD | SHIFT, true), MacPress::Key(20, CMD | SHIFT));
            assert_eq!(mac_press('}' as u32, CMD | SHIFT, true), MacPress::Key(30, CMD | SHIFT));
            // ⌘⇧7 from a German Mac is ⌘/.
            assert_eq!(mac_press('/' as u32, CMD | SHIFT, true), MacPress::Key(44, CMD));
            // ⌃# from a German Mac, no Shift there.
            assert_eq!(mac_press('#' as u32, CTRL, true), MacPress::Key(20, CTRL | SHIFT));
            // Not on a US keyboard: nothing, rather than typing it over a shortcut.
            assert_eq!(mac_press('§' as u32, CMD | SHIFT, true), MacPress::Nothing);
        }

        #[test]
        fn letters_and_named_keys_are_keys() {
            assert_eq!(mac_press('a' as u32, SHIFT, false), MacPress::Key(0, SHIFT));
            assert_eq!(mac_press(0xff0d, SHIFT, false), MacPress::Key(36, SHIFT));
            assert_eq!(mac_press(0xffe1, SHIFT, false), MacPress::Key(56, SHIFT));
        }

        #[test]
        fn without_char_shift_and_a_key_stay_that() {
            // `muxin desktop input` "shift+2" and "2".
            assert_eq!(mac_press('2' as u32, SHIFT, false), MacPress::Key(19, SHIFT));
            assert_eq!(mac_press('2' as u32, 0, false), MacPress::Key(19, 0));
            // "@" by itself, or an older app, which lets go of Shift first.
            assert_eq!(mac_press('@' as u32, 0, false), MacPress::Text);
            assert_eq!(mac_press('@' as u32, SHIFT, false), MacPress::Key(19, SHIFT));
        }

        // Layouts as `muxen_read_keys` reads them from macOS's own data.
        fn german() -> Layout {
            Layout::from_levels([
                "asdfhgyxcv\0bqwerzt123465\097ß80+ouüipljäkö#,-nm.<",
                "ASDFHGYXCV°BQWERZT!\"§$&%\0)/?(=*OUÜIPLJÄKÖ';_NM:>",
                "asdfhgyxcv^bqwerzt123465´97ß80+ouüipljäkö#,-nm.<",
                "asdfhgyxcv°bqwerzt123465`97/80*ouüipljäkö^;_nm:>",
            ])
        }

        fn russian() -> Layout {
            Layout::from_levels([
                "фыварпячсм>ийцукне123465=97-80ъщгхшздоэлжёб/тью]",
                "ФЫВАРПЯЧСМ<ИЙЦУКНЕ!\"№%,:+(._;)ЪЩГХШЗДОЭЛЖЁБ?ТЬЮ[",
                "asdfhgzxcv§bqweryt123465=97-80]ou[iplj'k;\\,/nm.`",
                "ASDFHGZXCV±BQWERYT!@#$^%+(&_*)}OU{IPLJ\"K:|<?NM>~",
            ])
        }

        fn dvorak_qwerty_command() -> Layout {
            Layout::from_levels([
                "aoeudi;qjk\0x',.pfy123465]97[80=rg/clnh-ts\\wzbmv`",
                "AOEUDI:QJK\0X\"<>PFY!@#$^%}(&{*)+RG?CLNH_TS|WZBMV~",
                "asdfhgzxcv\0bqweryt123465=97-80]ou[iplj'k;\\,/nm.`",
                "asdfhgzxcv\0bqweryt!@#$^%+(&_*)}ou{iplj\"k:|<?nm>~",
            ])
        }

        fn on_german(c: char, flags: u64) -> MacPress {
            super::mac_press(c as u32, flags, true, &german())
        }

        #[test]
        fn levels_reach_the_last_key() {
            for layout in [Layout::us(), german(), russian(), dvorak_qwerty_command()] {
                for level in 0..4 {
                    assert_ne!(layout.0[50][level], 0, "level {level}");
                }
            }
        }

        #[test]
        fn a_german_macs_letters_are_where_it_has_them() {
            // Not swapped, typed or in a shortcut.
            for flags in [0, SHIFT, CMD, CMD | SHIFT] {
                assert_eq!(super::mac_press('z' as u32, flags, false, &german()), MacPress::Key(16, flags));
                assert_eq!(super::mac_press('y' as u32, flags, false, &german()), MacPress::Key(6, flags));
            }
            assert_eq!(super::keycode(0xff0d, 0, &german()), Some((36, false)));
            assert_eq!(super::keycode(0x20, 0, &german()), Some((49, false)));
        }

        #[test]
        fn a_german_macs_characters_are_its_keys_when_shift_matches() {
            for (c, code, flags) in [
                ('"', 19, SHIFT),
                ('§', 20, SHIFT),
                ('/', 26, SHIFT),
                ('=', 29, SHIFT),
                ('?', 27, SHIFT),
                ('*', 30, SHIFT),
                ('ß', 27, 0),
                ('+', 30, 0),
                ('ö', 41, 0),
                ('Ö', 41, SHIFT),
                ('ü', 33, 0),
                ('ä', 39, 0),
                ('#', 42, 0),
                ('\'', 42, SHIFT),
                ('-', 44, 0),
                ('_', 44, SHIFT),
                ('<', 50, 0),
                ('>', 50, SHIFT),
                ('°', 10, SHIFT),
            ] {
                assert_eq!(on_german(c, flags), MacPress::Key(code, flags), "{c}");
            }
        }

        #[test]
        fn a_german_mac_types_as_text_what_a_key_wouldnt_type() {
            // Shift+2 from a US Mac: `@` is behind Option here, and this key with Shift is `"`.
            assert_eq!(on_german('@', SHIFT), MacPress::Text);
            // Behind Option too.
            for c in ['[', ']', '{', '}', '|', '\\', '~'] {
                assert_eq!(on_german(c, 0), MacPress::Text, "{c}");
            }
            // `/` from a US Mac, no Shift there: here it wants it.
            assert_eq!(on_german('/', 0), MacPress::Text);
            // Dead keys would wait for the next one.
            for (c, flags) in [('^', 0), ('´', 0), ('`', SHIFT)] {
                assert_eq!(on_german(c, flags), MacPress::Text, "{c}");
            }
        }

        #[test]
        fn a_german_macs_shortcuts_keep_their_keys() {
            // ⌘⇧3 from a German Mac.
            assert_eq!(on_german('§', CMD | SHIFT), MacPress::Key(20, CMD | SHIFT));
            // ⌘/ from a US Mac, and ⌘⇧7 from a German one, the same on a German Mac.
            assert_eq!(on_german('/', CMD), MacPress::Key(26, CMD | SHIFT));
            assert_eq!(on_german('/', CMD | SHIFT), MacPress::Key(26, CMD | SHIFT));
            // Shift comes off a key that doesn't want it.
            assert_eq!(on_german('#', CMD | SHIFT), MacPress::Key(42, CMD));
            // Dead keys don't wait with ⌘, or Control.
            assert_eq!(on_german('^', CMD), MacPress::Key(10, CMD));
            assert_eq!(on_german('´', CMD), MacPress::Key(24, CMD));
            assert_eq!(on_german('`', CMD | SHIFT), MacPress::Key(24, CMD | SHIFT));
            assert_eq!(on_german('^', CTRL), MacPress::Key(10, CTRL));
            // Behind Option: nothing, rather than typing it over a shortcut.
            assert_eq!(on_german('@', CMD), MacPress::Nothing);
        }

        #[test]
        fn a_german_mac_without_char_keeps_shift_and_a_key() {
            let press = |c: char, flags| super::mac_press(c as u32, flags, false, &german());
            // `muxin desktop input` "shift+2", whatever that types here, and "2".
            assert_eq!(press('2', SHIFT), MacPress::Key(19, SHIFT));
            assert_eq!(press('2', 0), MacPress::Key(19, 0));
            // "@" by itself.
            assert_eq!(press('@', 0), MacPress::Text);
            assert_eq!(press('@', SHIFT), MacPress::Text);
        }

        #[test]
        fn a_layout_without_latin_letters_types_them_and_has_them_for_shortcuts() {
            let press = |c: char, flags| super::mac_press(c as u32, flags, false, &russian());
            assert_eq!(press('a', 0), MacPress::Text);
            assert_eq!(text_unit('a' as u32, SHIFT), 'A' as u16);
            assert_eq!(text_unit('a' as u32, 0), 'a' as u16);
            assert_eq!(text_unit('/' as u32, SHIFT), '/' as u16);
            assert_eq!(text_unit('§' as u32, SHIFT), '§' as u16);
            // With ⌘ this layout goes by Latin letters, and they're there for Control too.
            assert_eq!(press('c', CMD), MacPress::Key(8, CMD));
            assert_eq!(press('c', CTRL), MacPress::Key(8, CTRL));
            assert_eq!(press('2', 0), MacPress::Key(19, 0));
        }

        #[test]
        fn a_layout_with_other_keys_for_command_uses_them_only_with_it() {
            let press = |c: char, flags| super::mac_press(c as u32, flags, false, &dvorak_qwerty_command());
            assert_eq!(press('c', 0), MacPress::Key(34, 0));
            assert_eq!(press('c', CTRL), MacPress::Key(34, CTRL));
            assert_eq!(press('c', CMD), MacPress::Key(8, CMD));
        }
    }
}
