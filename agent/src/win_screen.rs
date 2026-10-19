//! Windows' screen and clipboard, for the desktop view: GDI captures,
//! `SendInput` for the mouse and keyboard, the clipboard's Unicode text. And
//! getting there: sshd runs links in session 0, which has no desktop, so
//! programs that need the screen start in the signed-in user's own session
//! through a one-off scheduled task, and connect back on 127.0.0.1 with a
//! token (see `start_in_desktop` and `gui_run`).

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Physical pixels, not the scaled ones (a 150% display is its real size).
fn dpi_aware() {
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// The monitors: (name, rect, primary).
fn monitors() -> Vec<(String, RECT, bool)> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
        let out = unsafe { &mut *(data as *mut Vec<(String, RECT, bool)>) };
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(m, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) } != 0 {
            let name = String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').trim_start_matches(r"\\.\").to_string();
            out.push((name, info.monitorInfo.rcMonitor, info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0));
        }
        1
    }
    let mut out: Vec<(String, RECT, bool)> = Vec::new();
    unsafe { EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(each), &mut out as *mut _ as LPARAM) };
    out
}

fn virtual_screen() -> RECT {
    unsafe {
        let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        RECT { left: x, top: y, right: x + GetSystemMetrics(SM_CXVIRTUALSCREEN), bottom: y + GetSystemMetrics(SM_CYVIRTUALSCREEN) }
    }
}

/// `muxen-agent displays` on Windows: each monitor, and "all" when there are several.
pub fn displays() -> Value {
    dpi_aware();
    let ms = monitors();
    let mut out: Vec<Value> = ms
        .iter()
        .enumerate()
        .map(|(i, (name, r, primary))| json!({ "id": (i + 1).to_string(), "name": if *primary { format!("{name} (main)") } else { name.clone() }, "x": r.left, "y": r.top, "width": r.right - r.left, "height": r.bottom - r.top, "main": primary }))
        .collect();
    if ms.len() > 1 {
        let v = virtual_screen();
        out.push(json!({ "id": "all", "name": "All screens", "x": v.left, "y": v.top, "width": v.right - v.left, "height": v.bottom - v.top, "main": false }));
    }
    json!({ "ok": true, "displays": out })
}

/// A display, captured with GDI.
pub struct WinScreen {
    rect: RECT,
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
    frame: Vec<u8>,
    held: Vec<u16>,
    buttons: Vec<u64>,
}

impl WinScreen {
    /// `display`: a monitor's number from `displays`, "all", or None for the main one.
    pub fn open(display: Option<&str>) -> Result<WinScreen, String> {
        dpi_aware();
        let ms = monitors();
        let rect = match display {
            Some("all") => virtual_screen(),
            Some(n) => ms.get(n.parse::<usize>().map_err(|_| format!("No display called {n}."))?.saturating_sub(1)).map(|m| m.1).ok_or_else(|| format!("No display called {n}."))?,
            None => ms.iter().find(|m| m.2).or(ms.first()).map(|m| m.1).unwrap_or_else(virtual_screen),
        };
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        if w <= 0 || h <= 0 {
            return Err("Windows has no screen here (nobody signed in?).".into());
        }
        unsafe {
            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                return Err("Can't reach the screen (nobody signed in to the desktop?).".into());
            }
            let mem_dc = CreateCompatibleDC(screen_dc);
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                return Err("Can't make room for the screen.".into());
            }
            SelectObject(mem_dc, bitmap as HGDIOBJ);
            Ok(WinScreen { rect, screen_dc, mem_dc, bitmap, bits: bits as *mut u8, frame: vec![0; (w * h * 4) as usize], held: Vec::new(), buttons: Vec::new() })
        }
    }

    pub fn size(&self) -> (usize, usize) {
        ((self.rect.right - self.rect.left) as usize, (self.rect.bottom - self.rect.top) as usize)
    }

    /// The screen now, with the mouse pointer drawn in.
    pub fn grab(&mut self) -> Option<Option<&[u8]>> {
        let (w, h) = self.size();
        unsafe {
            // Fails while the secure desktop is up (a UAC prompt, the lock
            // screen): keep showing the last frame.
            if BitBlt(self.mem_dc, 0, 0, w as i32, h as i32, self.screen_dc, self.rect.left, self.rect.top, SRCCOPY | CAPTUREBLT) == 0 {
                return Some(None);
            }
            let mut ci: CURSORINFO = std::mem::zeroed();
            ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
            if GetCursorInfo(&mut ci) != 0 && ci.flags & CURSOR_SHOWING != 0 {
                let mut ii: ICONINFO = std::mem::zeroed();
                if GetIconInfo(ci.hCursor, &mut ii) != 0 {
                    let (x, y) = (ci.ptScreenPos.x - self.rect.left - ii.xHotspot as i32, ci.ptScreenPos.y - self.rect.top - ii.yHotspot as i32);
                    DrawIconEx(self.mem_dc, x, y, ci.hCursor, 0, 0, 0, std::ptr::null_mut(), DI_NORMAL);
                    if !ii.hbmMask.is_null() {
                        DeleteObject(ii.hbmMask as HGDIOBJ);
                    }
                    if !ii.hbmColor.is_null() {
                        DeleteObject(ii.hbmColor as HGDIOBJ);
                    }
                }
            }
            GdiFlush();
            std::ptr::copy_nonoverlapping(self.bits, self.frame.as_mut_ptr(), self.frame.len());
        }
        Some(Some(&self.frame))
    }

    /// The desktop's coordinates, as SendInput wants them (0..65535 across all screens).
    fn absolute(&self, e: &Value, frame: (usize, usize)) -> (i32, i32) {
        let (w, h) = self.size();
        let x = self.rect.left as f64 + e["x"].as_f64().unwrap_or(0.) * w as f64 / frame.0 as f64;
        let y = self.rect.top as f64 + e["y"].as_f64().unwrap_or(0.) * h as f64 / frame.1 as f64;
        let v = virtual_screen();
        let nx = ((x - v.left as f64) * 65535. / ((v.right - v.left - 1).max(1)) as f64).round() as i32;
        let ny = ((y - v.top as f64) * 65535. / ((v.bottom - v.top - 1).max(1)) as f64).round() as i32;
        (nx, ny)
    }

    pub fn apply(&mut self, e: &Value, frame: (usize, usize)) {
        match e["t"].as_str().unwrap_or("") {
            "move" => {
                let (x, y) = self.absolute(e, frame);
                send(&[mouse(x, y, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
            }
            "button" => {
                let (x, y) = self.absolute(e, frame);
                let b = e["b"].as_u64().unwrap_or(1);
                let down = e["down"].as_bool().unwrap_or(false);
                let flag = match (b, down) {
                    (1, true) => MOUSEEVENTF_LEFTDOWN,
                    (1, false) => MOUSEEVENTF_LEFTUP,
                    (2, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (2, false) => MOUSEEVENTF_MIDDLEUP,
                    (3, true) => MOUSEEVENTF_RIGHTDOWN,
                    (3, false) => MOUSEEVENTF_RIGHTUP,
                    _ => return,
                };
                let at = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                // A release from the app's "let go of everything" carries no position.
                if down || e["x"].as_f64().unwrap_or(0.) != 0. || e["y"].as_f64().unwrap_or(0.) != 0. {
                    send(&[mouse(x, y, 0, at), mouse(0, 0, 0, flag)]);
                } else {
                    send(&[mouse(0, 0, 0, flag)]);
                }
                self.buttons.retain(|x| *x != b);
                if down {
                    self.buttons.push(b);
                }
            }
            "scroll" => {
                let (x, y) = self.absolute(e, frame);
                let at = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                let (dy, dx) = (e["dy"].as_i64().unwrap_or(0).clamp(-20, 20), e["dx"].as_i64().unwrap_or(0).clamp(-20, 20));
                let mut events = vec![mouse(x, y, 0, at)];
                // Down is positive here, negative for Windows' wheel.
                if dy != 0 {
                    events.push(mouse(0, 0, (-dy * WHEEL_DELTA as i64) as i32, MOUSEEVENTF_WHEEL));
                }
                if dx != 0 {
                    events.push(mouse(0, 0, (dx * WHEEL_DELTA as i64) as i32, MOUSEEVENTF_HWHEEL));
                }
                send(&events);
            }
            "text" => {
                let mut events = Vec::new();
                for unit in e["s"].as_str().unwrap_or("").encode_utf16() {
                    match unit {
                        0x0a | 0x0d => {
                            events.push(key(VK_RETURN, 0, 0));
                            events.push(key(VK_RETURN, 0, KEYEVENTF_KEYUP));
                        }
                        0x09 => {
                            events.push(key(VK_TAB, 0, 0));
                            events.push(key(VK_TAB, 0, KEYEVENTF_KEYUP));
                        }
                        u => {
                            // The key that types it (with Shift, Ctrl, Alt as the layout
                            // needs), as a keyboard would; else the character itself.
                            let scan = unsafe { VkKeyScanW(u) };
                            if scan != -1 && u < 0x80 {
                                let vk = (scan as u16) & 0xff;
                                let mods: Vec<VIRTUAL_KEY> = [(0x100, VK_SHIFT), (0x200, VK_CONTROL), (0x400, VK_MENU)].iter().filter(|(bit, _)| scan as u16 & bit != 0).map(|(_, k)| *k).collect();
                                events.extend(mods.iter().map(|m| key(*m, 0, 0)));
                                events.push(key(vk, 0, 0));
                                events.push(key(vk, 0, KEYEVENTF_KEYUP));
                                events.extend(mods.iter().rev().map(|m| key(*m, 0, KEYEVENTF_KEYUP)));
                            } else {
                                events.push(key(0, u, KEYEVENTF_UNICODE));
                                events.push(key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                            }
                        }
                    }
                }
                send(&events);
            }
            "key" => {
                let sym = e["keysym"].as_u64().unwrap_or(0) as u32;
                let down = e["down"].as_bool().unwrap_or(false);
                let Some((vk, extended)) = vk_of(sym) else { return };
                let mut flags = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                send(&[key(vk, 0, flags)]);
                self.held.retain(|x| *x != vk);
                if down {
                    self.held.push(vk);
                }
            }
            _ => {}
        }
    }
}

impl Drop for WinScreen {
    fn drop(&mut self) {
        // Nothing stays held down when the view goes away.
        let keys: Vec<INPUT> = self.held.iter().map(|vk| key(*vk, 0, KEYEVENTF_KEYUP)).collect();
        send(&keys);
        for b in self.buttons.clone() {
            let flag = match b {
                1 => MOUSEEVENTF_LEFTUP,
                2 => MOUSEEVENTF_MIDDLEUP,
                _ => MOUSEEVENTF_RIGHTUP,
            };
            send(&[mouse(0, 0, 0, flag)]);
        }
        unsafe {
            DeleteObject(self.bitmap as HGDIOBJ);
            DeleteDC(self.mem_dc);
            ReleaseDC(std::ptr::null_mut(), self.screen_dc);
        }
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn send(events: &[INPUT]) {
    if !events.is_empty() {
        unsafe { SendInput(events.len() as u32, events.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    }
}

/// An X keysym's Windows virtual key, and whether it's an extended key.
fn vk_of(sym: u32) -> Option<(VIRTUAL_KEY, bool)> {
    Some(match sym {
        0xff08 => (VK_BACK, false),
        0xff09 => (VK_TAB, false),
        0xff0d => (VK_RETURN, false),
        0xff1b => (VK_ESCAPE, false),
        0x20 => (VK_SPACE, false),
        0xffff => (VK_DELETE, true),
        0xff63 => (VK_INSERT, true),
        0xff50 => (VK_HOME, true),
        0xff57 => (VK_END, true),
        0xff55 => (VK_PRIOR, true),
        0xff56 => (VK_NEXT, true),
        0xff51 => (VK_LEFT, true),
        0xff52 => (VK_UP, true),
        0xff53 => (VK_RIGHT, true),
        0xff54 => (VK_DOWN, true),
        0xffe1 => (VK_LSHIFT, false),
        0xffe2 => (VK_RSHIFT, false),
        0xffe3 => (VK_LCONTROL, false),
        0xffe4 => (VK_RCONTROL, true),
        0xffe9 => (VK_LMENU, false),
        0xffea => (VK_RMENU, true),
        0xffeb | 0xffe7 => (VK_LWIN, true),
        0xffec | 0xffe8 => (VK_RWIN, true),
        0xffbe..=0xffc9 => (VK_F1 + (sym - 0xffbe) as u16, false),
        _ => {
            let c = char::from_u32(sym)?;
            if c.is_ascii_alphabetic() {
                (c.to_ascii_uppercase() as u16, false)
            } else if c.is_ascii_digit() {
                (c as u16, false)
            } else {
                // Punctuation: the key it's on, in this keyboard's layout (Shift comes on its own).
                let mut units = [0u16; 2];
                let n = c.encode_utf16(&mut units).len();
                if n != 1 {
                    return None;
                }
                let scan = unsafe { VkKeyScanW(units[0]) };
                if scan == -1 {
                    return None;
                }
                ((scan as u16) & 0xff, false)
            }
        }
    })
}

// --- The clipboard -----------------------------------------------------------

const CF_UNICODETEXT: u32 = 13;

/// The clipboard's text, if it has any and isn't a password manager's: they
/// keep their copies out of clipboard history and off other devices, with
/// `ExcludeClipboardContentFromMonitorProcessing`, or with
/// `CanIncludeInClipboardHistory` or `CanUploadToCloudClipboard` set to 0.
fn clipboard_text() -> Option<String> {
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 || OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = if private_copy() { std::ptr::null_mut() } else { GetClipboardData(CF_UNICODETEXT) };
        let text = if h.is_null() {
            None
        } else {
            let p = GlobalLock(h as _) as *const u16;
            if p.is_null() {
                None
            } else {
                let mut n = 0;
                while *p.add(n) != 0 {
                    n += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                GlobalUnlock(h as _);
                Some(s)
            }
        };
        CloseClipboard();
        text
    }
}

/// Whether what's on the clipboard (open) is marked private (see `clipboard_text`).
fn private_copy() -> bool {
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;
    let format = |name: &str| {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
    };
    let excluded = format("ExcludeClipboardContentFromMonitorProcessing");
    if excluded != 0 && unsafe { IsClipboardFormatAvailable(excluded) } != 0 {
        return true;
    }
    // These two hold a DWORD: 0 keeps the copy out.
    ["CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"].into_iter().any(|name| {
        let f = format(name);
        let h = if f == 0 { std::ptr::null_mut() } else { unsafe { GetClipboardData(f) } };
        if h.is_null() {
            return false;
        }
        unsafe {
            let p = GlobalLock(h) as *const u32;
            if p.is_null() {
                return false;
            }
            let zero = GlobalSize(h) >= 4 && p.read_unaligned() == 0;
            GlobalUnlock(h);
            zero
        }
    })
}

fn set_clipboard_text(text: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
        let ok = !h.is_null() && {
            let p = GlobalLock(h) as *mut u16;
            !p.is_null() && {
                std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
                GlobalUnlock(h);
                !SetClipboardData(CF_UNICODETEXT, h as _).is_null()
            }
        };
        CloseClipboard();
        ok
    }
}

/// Windows' clipboard, for `muxen-agent clip`: Windows numbers its changes.
struct Clipboard;

impl super::clipboard::Board for Clipboard {
    /// (A password manager's copy shows as no text: see `clipboard_text`.)
    fn state(&mut self) -> (i64, bool) {
        (unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() } as i64, false)
    }

    fn text(&mut self) -> Option<String> {
        clipboard_text()
    }

    fn set(&mut self, text: &str) -> bool {
        set_clipboard_text(text)
    }
}

/// `muxen-agent clip` on Windows: what's copied here goes out; what the app
/// sends is put on the clipboard (see clipboard.rs). A look every few hundred
/// milliseconds sees each change.
pub fn clip(rx: std::sync::mpsc::Receiver<String>, changed: impl Fn(&str) -> bool) {
    super::clipboard::share(&mut Clipboard, rx, Duration::from_millis(300), changed);
}

// --- Running in the signed-in user's desktop session ------------------------

/// Whether this process runs in the session on the screen (not sshd's session 0).
pub fn in_desktop_session() -> bool {
    use windows_sys::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session) != 0 && session != 0 && session == WTSGetActiveConsoleSessionId() }
}

/// A signed-in Windows session: its user's name, and whether it's on a
/// screen (connected) rather than disconnected.
struct SignedIn {
    user: String,
    connected: bool,
}

/// Who is signed in to a desktop here, at the screen or through Remote
/// Desktop, from Windows' list of sessions. None when Windows won't say.
fn signed_in_users() -> Option<Vec<SignedIn>> {
    use windows_sys::Win32::System::RemoteDesktop::*;
    let query = |id: u32, class: WTS_INFO_CLASS| -> String {
        let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
        let mut bytes = 0u32;
        if unsafe { WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, id, class, &mut buf, &mut bytes) } == 0 || buf.is_null() {
            return String::new();
        }
        let units = unsafe { std::slice::from_raw_parts(buf, bytes as usize / 2) };
        let text = String::from_utf16_lossy(units.split(|u| *u == 0).next().unwrap_or_default());
        unsafe { WTSFreeMemory(buf as _) };
        text
    };
    let mut list: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    if unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut list, &mut count) } == 0 || list.is_null() {
        return None;
    }
    let sessions = unsafe { std::slice::from_raw_parts(list, count as usize) };
    // Session 0 is services'; the sign-in screen's session is only connected.
    let users: Vec<SignedIn> = sessions
        .iter()
        .filter(|s| s.SessionId != 0 && (s.State == WTSActive || s.State == WTSDisconnected))
        .map(|s| SignedIn { user: query(s.SessionId, WTSUserName), connected: s.State == WTSActive })
        .collect();
    unsafe { WTSFreeMemory(list as _) };
    // A signed-in session whose user Windows won't name: don't guess.
    (!users.iter().any(|u| u.user.is_empty())).then_some(users)
}

/// Why there's no desktop session with a screen to start in, when Windows
/// says so: nobody signed in, only someone else (a scheduled task only runs
/// in its own user's session), or this user's session is disconnected (its
/// Remote Desktop window closed: Windows draws nothing for it).
pub fn no_desktop_session() -> Option<String> {
    let users = signed_in_users()?;
    // By name, as the task names its user (`$env:USERNAME`); sshd's
    // USERDOMAIN says WORKGROUP rather than the computer's name.
    let me = std::env::var("USERNAME").unwrap_or_default();
    if me.is_empty() {
        return None;
    }
    let own: Vec<&SignedIn> = users.iter().filter(|u| u.user.eq_ignore_ascii_case(&me)).collect();
    if own.iter().any(|u| u.connected) {
        return None;
    }
    if !own.is_empty() {
        return Some(format!("{me} is signed in to Windows here, but that session is disconnected (its Remote Desktop window was closed), so there's no screen to show. Open it with Remote Desktop again."));
    }
    Some(match users.first() {
        Some(other) => format!("{} is signed in to Windows here, not {me}. Muxin shows the screen of the account it connects as.", other.user),
        None => "Nobody is signed in to Windows here, so there's no screen to show. Sign in first, at the machine or with Remote Desktop.".into(),
    })
}

/// Where the desktop session's helper listens: "port token".
fn gui_port_file() -> std::path::PathBuf {
    crate::home_dir().join("gui.port")
}

/// Asks the helper in the desktop session to run `muxen-agent <args>`; its
/// stdin and stdout, as the connection.
fn ask_gui_server(args: &[String]) -> io::Result<TcpStream> {
    let text = std::fs::read_to_string(gui_port_file())?;
    let (port, token) = text.trim().split_once(' ').ok_or_else(|| io::Error::other("gui.port is damaged"))?;
    let port: u16 = port.parse().map_err(|_| io::Error::other("gui.port is damaged"))?;
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.write_all(format!("{token}\n{}\n", json!({ "run": args })).as_bytes())?;
    Ok(s)
}

/// Starts `muxen-agent <args>` in the signed-in user's desktop session; its
/// stdin and stdout, as a connection. A helper there (`gui-serve`) runs them;
/// the first time, a one-off scheduled task starts it (through a windowless
/// console, so nothing flashes on their screen), and it stays a while.
pub fn start_in_desktop(args: &[String]) -> Result<TcpStream, String> {
    if let Ok(s) = ask_gui_server(args) {
        return Ok(s);
    }
    // The task would never start: say so now, not after waiting for it.
    if let Some(why) = no_desktop_session() {
        return Err(why);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let task = format!("Muxen desktop {}", std::process::id());
    let q = crate::win::quote;
    let script = format!(
        "$action = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument {argument}\n\
         $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited\n\
         $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)\n\
         Register-ScheduledTask -TaskName {task} -Action $action -Principal $principal -Settings $settings -Force | Out-Null\n\
         Start-ScheduledTask -TaskName {task}\n",
        argument = q(&format!("--headless \"{}\" gui-serve", exe.display())),
        task = q(&task)
    );
    let started = crate::win::ps(&script);
    // It's up within a few seconds when someone is signed in.
    let deadline = Instant::now() + Duration::from_secs(12);
    let result = match started {
        Err(e) => Err(format!("couldn't start a helper in the desktop session: {e}")),
        Ok(_) => loop {
            match ask_gui_server(args) {
                Ok(s) => break Ok(s),
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(150)),
                Err(_) => break Err("Windows didn't start Muxin's helper in your desktop session, so there's no screen to show.".to_string()),
            }
        },
    };
    let _ = crate::win::ps(&format!("Unregister-ScheduledTask -TaskName {} -Confirm:$false -ErrorAction SilentlyContinue", q(&task)));
    result
}

/// With nothing running for this long, the helper goes.
const GUI_IDLE: Duration = Duration::from_secs(30 * 60);

/// `muxen-agent gui-serve`, in the desktop session: runs what links ask for
/// (`screen`, `clip`, `displays`) with each connection as the program's stdin
/// and stdout. A connection sends the token from gui.port, then
/// `{"run": [args]}`.
pub fn gui_serve() -> i32 {
    use ring::rand::SecureRandom;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    if ask_gui_server(&["version".to_string()]).is_ok() {
        return 0; // one's already here
    }
    let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)) else { return 1 };
    let Ok(port) = listener.local_addr().map(|a| a.port()) else { return 1 };
    let mut bytes = [0u8; 24];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        return 1;
    }
    let token: Arc<String> = Arc::new(bytes.iter().map(|b| format!("{b:02x}")).collect());
    let tmp = gui_port_file().with_extension(format!("port.{}", std::process::id()));
    if std::fs::write(&tmp, format!("{port} {token}")).and_then(|_| std::fs::rename(&tmp, gui_port_file())).is_err() {
        return 1;
    }
    let active = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(std::sync::Mutex::new(Instant::now()));
    {
        let (active, last) = (active.clone(), last.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(30));
            if active.load(Ordering::Relaxed) == 0 && last.lock().unwrap().elapsed() > GUI_IDLE {
                let _ = std::fs::remove_file(gui_port_file());
                std::process::exit(0);
            }
        });
    }
    for conn in listener.incoming().flatten() {
        let (token, active, last) = (token.clone(), active.clone(), last.clone());
        std::thread::spawn(move || {
            let mut conn = conn;
            let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
            let line = |c: &mut TcpStream| -> Option<String> {
                let mut out = Vec::new();
                let mut byte = [0u8; 1];
                while out.len() < 64 * 1024 {
                    match c.read(&mut byte) {
                        Ok(1) if byte[0] == b'\n' => return String::from_utf8(out).ok(),
                        Ok(1) => out.push(byte[0]),
                        _ => return None,
                    }
                }
                None
            };
            if line(&mut conn).as_deref() != Some(token.as_str()) {
                return;
            }
            let Some(req) = line(&mut conn).and_then(|l| serde_json::from_str::<Value>(&l).ok()) else { return };
            let args: Vec<String> = req["run"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            // Only the programs that need the desktop session.
            if !matches!(args.first().map(String::as_str), Some("screen" | "clip" | "displays" | "version")) {
                return;
            }
            let _ = conn.set_read_timeout(None);
            active.fetch_add(1, Ordering::Relaxed);
            serve_program(conn, &args);
            active.fetch_sub(1, Ordering::Relaxed);
            *last.lock().unwrap() = Instant::now();
        });
    }
    0
}

/// Runs `muxen-agent <args>` with the connection as its stdin and stdout, until either ends.
fn serve_program(mut conn: TcpStream, args: &[String]) {
    let Ok(exe) = std::env::current_exe() else { return };
    let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::home_dir().join("screen.log"));
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped());
    match log {
        Ok(l) => cmd.stderr(l),
        Err(_) => cmd.stderr(Stdio::null()),
    };
    crate::sys::no_window(&mut cmd);
    let Ok(mut child) = cmd.spawn() else { return };
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return;
    };
    let Ok(mut from_link) = conn.try_clone() else {
        let _ = child.kill();
        return;
    };
    // The link side's end shows here as the end of its input.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = io::copy(&mut from_link, &mut stdin);
        let _ = done_tx.send(());
    });
    let _ = io::copy(&mut stdout, &mut conn);
    let _ = child.kill();
    let _ = child.wait();
    // Close our side properly and let the other read everything first: a
    // socket closed abruptly is reset, and a reset loses whatever the other
    // side hadn't read yet.
    let _ = conn.shutdown(std::net::Shutdown::Write);
    let _ = done_rx.recv_timeout(Duration::from_secs(10));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_keys() {
        assert_eq!(vk_of('a' as u32), Some((0x41, false)));
        assert_eq!(vk_of('A' as u32), Some((0x41, false)));
        assert_eq!(vk_of(0xff0d), Some((VK_RETURN, false)));
        assert_eq!(vk_of(0xff51), Some((VK_LEFT, true)));
        assert_eq!(vk_of(0xffbe), Some((VK_F1, false)));
    }
}
