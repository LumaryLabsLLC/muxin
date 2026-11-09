//! Taking the keyboard shortcuts this computer keeps for itself (⌘Tab and
//! Mission Control's on a Mac, the Windows key and Alt+Tab on Windows, the
//! window manager's on Linux), while a machine's desktop has the keyboard:
//! so they go to the machine. GPUI has nothing for this, so each system is
//! asked in its own way, around GPUI:
//!
//! - a Mac: an event tap that hands the keys to Muxin itself. macOS allows
//!   one only to an app the user allowed under Accessibility.
//! - Windows: a low-level keyboard hook. What it takes never reaches GPUI,
//!   so those keys come through `Capture::keys`.
//! - Linux on X11: a keyboard grab from a connection of our own, each key
//!   passed on to Muxin's window.
//! - Linux on Wayland: the compositor is asked not to act on shortcuts while
//!   Muxin's window has the keyboard (keyboard-shortcuts-inhibit); it may
//!   ask the user first.
//!
//! Nothing here takes Ctrl+Alt+Del or Win+L on Windows, ⌘⌥Esc on a Mac, or
//! Ctrl+Alt+F1 to F12 on Linux: no program can.

#[cfg(all(target_os = "macos", feature = "gui"))]
mod mac;
#[cfg(all(target_os = "linux", feature = "gui"))]
mod wayland;
#[cfg(windows)]
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
mod windows;
#[cfg(all(target_os = "linux", feature = "gui"))]
mod x11;

/// Whether this computer's shortcuts can be taken.
#[cfg(feature = "gui")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ready {
    Yes,
    /// A Mac: once Muxin is allowed under Privacy & Security › Accessibility.
    NeedsPermission,
    /// Not on this system.
    #[cfg_attr(any(target_os = "macos", windows, target_os = "linux"), allow(dead_code))]
    No,
}

#[cfg(feature = "gui")]
pub fn ready() -> Ready {
    #[cfg(target_os = "macos")]
    {
        if mac::allowed(false) { Ready::Yes } else { Ready::NeedsPermission }
    }
    #[cfg(any(windows, target_os = "linux"))]
    {
        Ready::Yes
    }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        Ready::No
    }
}

/// A Mac: asks the user to allow Muxin under Accessibility. macOS shows its
/// own request once, with a button to System Settings; asked again, this
/// opens that page of System Settings itself.
#[cfg(feature = "gui")]
pub fn ask_permission() {
    #[cfg(target_os = "macos")]
    {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        static ASKED: AtomicBool = AtomicBool::new(false);
        if !ASKED.swap(true, Relaxed) {
            mac::allowed(true);
        } else if !mac::allowed(false) {
            crate::util::open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
        }
    }
}

/// A key the system would have kept, taken before GPUI saw it: its name as
/// `fleet::desktop::keysym` takes it, and whether it went down.
#[cfg(feature = "gui")]
pub type TakenKeys = futures::channel::mpsc::UnboundedReceiver<(&'static str, bool)>;

/// This computer's shortcuts are being taken for as long as this lives.
#[cfg(feature = "gui")]
pub struct Capture {
    #[cfg(target_os = "macos")]
    _mac: mac::Tap,
    #[cfg(target_os = "linux")]
    _linux: Linux,
    #[cfg(windows)]
    _windows: windows::Hook,
    #[cfg(windows)]
    keys: Option<TakenKeys>,
    started: std::time::Instant,
}

#[cfg(all(target_os = "linux", feature = "gui"))]
enum Linux {
    X11(#[allow(dead_code)] x11::Grab),
    Wayland(#[allow(dead_code)] wayland::Inhibit),
}

#[cfg(feature = "gui")]
impl Capture {
    /// Starts taking them for `window`, which has the keyboard now. None
    /// when the system says no.
    pub fn start(window: &gpui::Window) -> Option<Capture> {
        let _ = window;
        let started = std::time::Instant::now();
        #[cfg(target_os = "macos")]
        {
            Some(Capture { _mac: mac::Tap::start()?, started })
        }
        #[cfg(target_os = "linux")]
        {
            let linux = if gpui::guess_compositor() == "Wayland" { Linux::Wayland(wayland::Inhibit::start(window)?) } else { Linux::X11(x11::Grab::start()?) };
            Some(Capture { _linux: linux, started })
        }
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let handle = HasWindowHandle::window_handle(window).ok()?;
            let RawWindowHandle::Win32(h) = handle.as_raw() else { return None };
            let (tx, rx) = futures::channel::mpsc::unbounded();
            let hook = windows::Hook::start(h.hwnd.get(), move |key, down| {
                let _ = tx.unbounded_send((key, down));
            })?;
            Some(Capture { _windows: hook, keys: Some(rx), started })
        }
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        {
            let _ = started;
            None
        }
    }

    /// The keys taken before GPUI saw them (Windows; elsewhere they come
    /// through GPUI as any key). Once.
    pub fn keys(&mut self) -> Option<TakenKeys> {
        #[cfg(windows)]
        {
            self.keys.take()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Whether the window may look inactive because of the capture itself:
    /// on X11 the grab takes the keyboard from the window for a moment, and
    /// GPUI reports that as the window going inactive.
    pub fn settling(&self) -> bool {
        cfg!(target_os = "linux") && self.started.elapsed() < std::time::Duration::from_millis(400)
    }
}

/// Windows: the name of a key the hook takes (as `fleet::desktop::keysym`
/// takes it), by its virtual key.
#[cfg(any(windows, test))]
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn windows_key_name(vk: u32) -> Option<&'static str> {
    Some(match vk {
        // The left and right Windows keys.
        0x5B | 0x5C => "super",
        0x09 => "tab",
        0x1B => "escape",
        0x73 => "f4",
        0x20 => "space",
        _ => return None,
    })
}

/// Windows: whether a key going down is one Windows keeps for itself, with
/// Alt or Ctrl held or not: the Windows key (Start, and every Win+key),
/// Alt+Tab, Alt+Esc, Alt+F4, Alt+Space (the window's menu), Ctrl+Esc (Start)
/// and Ctrl+Shift+Esc (Task Manager).
#[cfg(any(windows, test))]
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn windows_takes(vk: u32, alt: bool, ctrl: bool) -> Option<&'static str> {
    let name = windows_key_name(vk)?;
    let taken = match name {
        "super" => true,
        "escape" => alt || ctrl,
        _ => alt,
    };
    taken.then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_takes_what_windows_keeps_and_nothing_else() {
        // The Windows keys, by themselves or with anything.
        assert_eq!(windows_takes(0x5B, false, false), Some("super"));
        assert_eq!(windows_takes(0x5C, false, true), Some("super"));
        // Alt+Tab, Alt+Esc, Alt+F4, Alt+Space, Ctrl+Esc, Ctrl+Alt+Tab.
        assert_eq!(windows_takes(0x09, true, false), Some("tab"));
        assert_eq!(windows_takes(0x1B, true, false), Some("escape"));
        assert_eq!(windows_takes(0x73, true, false), Some("f4"));
        assert_eq!(windows_takes(0x20, true, false), Some("space"));
        assert_eq!(windows_takes(0x1B, false, true), Some("escape"));
        assert_eq!(windows_takes(0x09, true, true), Some("tab"));
        // Tab, Esc, F4, Space and Ctrl+Tab are GPUI's as ever; so are letters with Alt.
        for (vk, alt, ctrl) in [(0x09, false, false), (0x1B, false, false), (0x73, false, false), (0x20, false, false), (0x09, false, true), (0x41, true, false), (0x41, false, true)] {
            assert_eq!(windows_takes(vk, alt, ctrl), None, "{vk:#x}");
        }
        // Every name is a key the agents know.
        for vk in [0x5B, 0x5C, 0x09, 0x1B, 0x73, 0x20] {
            assert!(crate::fleet::desktop::keysym(windows_key_name(vk).unwrap()).is_some());
        }
    }
}
