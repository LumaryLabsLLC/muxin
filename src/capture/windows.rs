//! Windows: a low-level keyboard hook. While Muxin's window is the one in
//! front, the keys Windows keeps for itself (the Windows key, Alt+Tab,
//! Alt+Esc, Ctrl+Esc, Alt+F4, Alt+Space) are swallowed in the hook and handed
//! to `sink` instead. They never reach GPUI, and Windows never counts a
//! swallowed Windows key as held, so the desktop view presses them on the
//! machine itself.
//!
//! The hook has a thread of its own with a message loop: Windows drops a
//! hook that answers too slowly, and the app's own thread may be busy.
//! Ctrl+Alt+Del and Win+L never come this way: Windows keeps those from
//! every program.

use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering::Relaxed};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, MSG, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT,
    WM_SYSKEYDOWN,
};

type Sink = Box<dyn Fn(&'static str, bool) + Send>;

/// Muxin's window (its HWND) whose keys are taken, 0 for none.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static SINK: Mutex<Option<Sink>> = Mutex::new(None);
/// The keys (by virtual key) whose press was taken: their release is too.
static DOWN: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

const VK_CONTROL: i32 = 0x11;

/// The hook is in place while this lives.
pub struct Hook {
    thread: u32,
}

impl Hook {
    /// Takes those keys while `window` (an HWND) is in front, giving each to
    /// `sink` (from the hook's thread). None when another is in place, or
    /// Windows refuses.
    pub fn start(window: isize, sink: impl Fn(&'static str, bool) + Send + 'static) -> Option<Hook> {
        if WINDOW.compare_exchange(0, window, Relaxed, Relaxed).is_err() {
            return None;
        }
        *SINK.lock().unwrap() = Some(Box::new(sink));
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("keyboard hook".into()).spawn(move || unsafe {
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), GetModuleHandleW(std::ptr::null()), 0);
            let _ = tx.send((!hook.is_null()).then(|| GetCurrentThreadId()));
            if hook.is_null() {
                return;
            }
            // The hook is called from this loop, until WM_QUIT.
            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {}
            UnhookWindowsHookEx(hook);
        });
        match spawned.ok().and_then(|_| rx.recv().ok().flatten()) {
            Some(thread) => Some(Hook { thread }),
            None => {
                forget();
                None
            }
        }
    }
}

fn forget() {
    WINDOW.store(0, Relaxed);
    *SINK.lock().unwrap() = None;
    for part in &DOWN {
        part.store(0, Relaxed);
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        forget();
        unsafe { PostThreadMessageW(self.thread, WM_QUIT, 0, 0) };
    }
}

unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let vk = key.vkCode & 255;
        let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let (part, bit) = (&DOWN[(vk / 64) as usize], 1u64 << (vk % 64));
        let window = WINDOW.load(Relaxed);
        let name = if down {
            let in_front = window != 0 && unsafe { GetForegroundWindow() } as isize == window;
            let (alt, ctrl) = (key.flags & LLKHF_ALTDOWN != 0, unsafe { GetAsyncKeyState(VK_CONTROL) } < 0);
            in_front.then(|| super::windows_takes(vk, alt, ctrl)).flatten()
        } else {
            // Let go, whatever is held by now: the key that was taken going down.
            (part.load(Relaxed) & bit != 0).then(|| super::windows_key_name(vk)).flatten()
        };
        if let Some(name) = name {
            if down {
                part.fetch_or(bit, Relaxed);
            } else {
                part.fetch_and(!bit, Relaxed);
            }
            if let Some(sink) = SINK.lock().unwrap().as_ref() {
                sink(name, down);
            }
            // Swallowed: Windows never sees it.
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}
