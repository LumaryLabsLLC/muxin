//! A Mac: an event tap on the keyboard. While a desktop has the keyboard,
//! keys pressed with ⌘ or ⌃ (and F1 to F12) are taken out of the system's
//! path, before the Dock or the window server acts on them (⌘Tab, ⌘Space,
//! Mission Control's ⌃← ⌃→ ⌃↑, screenshots), and put in Muxin's own event
//! queue instead, where they arrive like any key. Everything else is left
//! alone: plain typing and Option's characters go their usual way, input
//! methods included.
//!
//! macOS only lets an app the user allowed under Privacy & Security ›
//! Accessibility take keys this way (Input Monitoring only lets one listen).
//! If Muxin stops answering, macOS turns the tap off by itself and the keys
//! are the system's again.

#![allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering::Relaxed};

use objc::runtime::{BOOL, NO, Object};
use objc::{class, msg_send, sel, sel_impl};

type CGEventRef = *mut c_void;
type TapCallback = extern "C" fn(proxy: *mut c_void, kind: u32, event: CGEventRef, info: *mut c_void) -> CGEventRef;

/// A CoreFoundation structure only ever pointed at.
#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    static kAXTrustedCheckOptionPrompt: *const c_void;
    fn CGEventTapCreate(tap: u32, place: u32, options: u32, events: u64, callback: TapCallback, info: *mut c_void) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: *const c_void;
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    fn CFDictionaryCreate(allocator: *const c_void, keys: *const *const c_void, values: *const *const c_void, count: isize, key_callbacks: *const Opaque, value_callbacks: *const Opaque) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFMachPortCreateRunLoopSource(allocator: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
}

// CGEventType, and the same numbers as NSEventType.
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
// kCGSessionEventTap, kCGHeadInsertEventTap, kCGEventTapOptionDefault (it may change events).
const SESSION_TAP: u32 = 1;
const HEAD_INSERT: u32 = 0;
const MAY_CHANGE_EVENTS: u32 = 0;
/// kCGKeyboardEventKeycode.
const KEYCODE_FIELD: u32 = 9;
const CONTROL: u64 = 0x40000;
const COMMAND: u64 = 0x100000;

/// The tap, made once and kept (off while nothing holds it).
static TAP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// How many `Tap`s are alive: desktops with the keyboard.
static HOLDERS: AtomicUsize = AtomicUsize::new(0);
/// The keys (by keycode) whose press was taken: their release is too.
static DOWN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Whether Muxin is allowed under Accessibility; `ask` shows macOS's own
/// request when it isn't.
pub fn allowed(ask: bool) -> bool {
    unsafe {
        if !ask {
            return AXIsProcessTrusted() != 0;
        }
        let (keys, values) = ([kAXTrustedCheckOptionPrompt], [kCFBooleanTrue]);
        let options = CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        let allowed = AXIsProcessTrustedWithOptions(options) != 0;
        if !options.is_null() {
            CFRelease(options);
        }
        allowed
    }
}

/// Whether a key going down with these flags is taken: one the system may
/// keep for itself. Its shortcuts all have ⌘ or ⌃, or are function keys.
fn takes(code: u16, flags: u64) -> bool {
    const FUNCTION_KEYS: [u16; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];
    flags & (COMMAND | CONTROL) != 0 || FUNCTION_KEYS.contains(&code)
}

/// The keys are being taken while this lives. From the main thread only.
pub struct Tap(());

impl Tap {
    pub fn start() -> Option<Tap> {
        if !allowed(false) {
            return None;
        }
        let mut tap = TAP.load(Relaxed);
        if tap.is_null() {
            tap = unsafe { CGEventTapCreate(SESSION_TAP, HEAD_INSERT, MAY_CHANGE_EVENTS, (1 << KEY_DOWN) | (1 << KEY_UP), on_event, std::ptr::null_mut()) };
            if tap.is_null() {
                return None;
            }
            unsafe {
                // On the main thread's run loop: events are handed over from there.
                let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
                CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
                CGEventTapEnable(tap, false);
            }
            TAP.store(tap, Relaxed);
        }
        if HOLDERS.fetch_add(1, Relaxed) == 0 {
            unsafe { CGEventTapEnable(tap, true) };
        }
        Some(Tap(()))
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        if HOLDERS.fetch_sub(1, Relaxed) == 1 {
            unsafe { CGEventTapEnable(TAP.load(Relaxed), false) };
            for half in &DOWN {
                half.store(0, Relaxed);
            }
        }
    }
}

extern "C" fn on_event(_proxy: *mut c_void, kind: u32, event: CGEventRef, _info: *mut c_void) -> CGEventRef {
    if kind == TAP_DISABLED_BY_TIMEOUT || kind == TAP_DISABLED_BY_USER_INPUT {
        // macOS turned it off (Muxin was slow to answer): on again while it's wanted.
        let tap = TAP.load(Relaxed);
        if HOLDERS.load(Relaxed) > 0 && !tap.is_null() {
            unsafe { CGEventTapEnable(tap, true) };
        }
        return event;
    }
    if HOLDERS.load(Relaxed) == 0 || (kind != KEY_DOWN && kind != KEY_UP) {
        return event;
    }
    let code = (unsafe { CGEventGetIntegerValueField(event, KEYCODE_FIELD) } as u64) & 127;
    let (half, bit) = (&DOWN[(code / 64) as usize], 1u64 << (code % 64));
    let take = match kind {
        KEY_DOWN => takes(code as u16, unsafe { CGEventGetFlags(event) }),
        _ => half.load(Relaxed) & bit != 0,
    };
    if !take || !unsafe { hand_over(event) } {
        return event;
    }
    if kind == KEY_DOWN {
        half.fetch_or(bit, Relaxed);
    } else {
        half.fetch_and(!bit, Relaxed);
    }
    // Taken: the system never sees it.
    std::ptr::null_mut()
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}

/// Puts the key in Muxin's own event queue, for its window with the keyboard:
/// from there it's a key like any other (never handled right here, in the
/// middle of whatever the run loop was doing). False when Muxin isn't the app
/// in front, or has no such window: the key is left to the system then.
unsafe fn hand_over(event: CGEventRef) -> bool {
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let active: BOOL = msg_send![app, isActive];
        let window: *mut Object = msg_send![app, keyWindow];
        if active == NO || window.is_null() {
            return false;
        }
        let from: *mut Object = msg_send![class!(NSEvent), eventWithCGEvent: event];
        if from.is_null() {
            return false;
        }
        // The same key, addressed to that window (as taken, it has none).
        let characters: *mut Object = msg_send![from, characters];
        let plain: *mut Object = msg_send![from, charactersIgnoringModifiers];
        let mut key = from;
        if !characters.is_null() && !plain.is_null() {
            let kind: usize = msg_send![from, type];
            let flags: usize = msg_send![from, modifierFlags];
            let time: f64 = msg_send![from, timestamp];
            let number: isize = msg_send![window, windowNumber];
            let repeat: BOOL = msg_send![from, isARepeat];
            let code: u16 = msg_send![from, keyCode];
            let nil: *mut Object = std::ptr::null_mut();
            let made: *mut Object = msg_send![class!(NSEvent), keyEventWithType: kind
                location: NSPoint { x: 0., y: 0. }
                modifierFlags: flags
                timestamp: time
                windowNumber: number
                context: nil
                characters: characters
                charactersIgnoringModifiers: plain
                isARepeat: repeat
                keyCode: code];
            if !made.is_null() {
                key = made;
            }
        }
        let _: () = msg_send![app, postEvent: key atStart: NO];
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asking_whether_muxin_is_allowed_shows_nothing_and_answers() {
        // Either answer is fine here: it's macOS's own, with no request shown.
        let allowed = allowed(false);
        assert_eq!(allowed, super::allowed(false));
    }

    #[test]
    fn takes_what_the_system_may_keep_and_leaves_typing_alone() {
        const SHIFT: u64 = 0x20000;
        const OPTION: u64 = 0x80000;
        // ⌘Tab, ⌘Space, ⌃→, ⌃↑, ⌘⇧4, ⌘Q, ⌃C.
        for (code, flags) in [(48, COMMAND), (49, COMMAND), (124, CONTROL), (126, CONTROL), (21, COMMAND | SHIFT), (12, COMMAND), (8, CONTROL)] {
            assert!(takes(code, flags), "{code} {flags:#x}");
        }
        // F3, F11 and F12, with nothing held.
        for code in [99, 103, 111] {
            assert!(takes(code, 0), "{code}");
        }
        // Letters, Shift+letters, Option's characters, Return, the arrows: Muxin gets them as ever.
        for (code, flags) in [(0, 0), (0, SHIFT), (37, OPTION), (36, 0), (123, 0), (48, 0), (53, 0), (49, SHIFT | OPTION)] {
            assert!(!takes(code, flags), "{code} {flags:#x}");
        }
    }
}
