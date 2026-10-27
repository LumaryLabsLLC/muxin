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
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
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

/// A display, or one app's window, captured with GDI.
pub struct WinScreen {
    /// What's shown, in desktop coordinates: a display, or the visible part of
    /// an app's window.
    rect: RECT,
    /// The app's window, when that's what's shown.
    window: Option<AppWindow>,
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
    /// The bitmap's size: the display's, or the whole window's.
    bits_size: (i32, i32),
    frame: Vec<u8>,
    held: Vec<u16>,
    buttons: Vec<u64>,
}

/// An app's window shown on its own (`window:<id>`, an id from `windows`).
struct AppWindow {
    hwnd: HWND,
    /// The whole window on the desktop, as PrintWindow draws it: Windows 10
    /// and 11 add invisible borders around what shows, to resize it by.
    whole: RECT,
    /// What the app was last told (see `meta`): the pointer over the window,
    /// whether it's minimized; and its drag areas, from a thread that asks
    /// Windows for them now and then (`drag_areas`), until `stop`.
    cursor: &'static str,
    minimized: Option<bool>,
    drag: std::sync::Arc<std::sync::Mutex<Option<Vec<[i32; 4]>>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Where it was when a button went down: input maps from there until let
    /// go, so an app moving its own window doesn't run from the pointer.
    held_at: Option<(i32, i32)>,
    /// Its size (whole) and whether it was maximized before the app first
    /// resized it: so it's left again when the stream ends.
    restore: Option<(i32, i32, bool)>,
    /// Where it was (whole, top left) before a resize moved it to stay on its screen.
    moved_from: Option<(i32, i32)>,
}

impl AppWindow {
    fn new(hwnd: HWND, whole: RECT) -> AppWindow {
        let drag = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let id = hwnd as usize;
        let size = move || {
            let hwnd = id as HWND;
            (unsafe { IsWindow(hwnd) } != 0).then(|| window_bounds(hwnd).map_or((0, 0), |(_, r)| (r.right - r.left, r.bottom - r.top)))
        };
        let areas = move || {
            let hwnd = id as HWND;
            (unsafe { IsIconic(hwnd) } == 0).then(|| drag_areas(hwnd)).flatten()
        };
        crate::desktop::watch_drag_areas(size, areas, Duration::from_secs(3), drag.clone(), stop.clone());
        AppWindow { hwnd, whole, cursor: "", minimized: None, drag, stop, held_at: None, restore: None, moved_from: None }
    }
}

impl Drop for AppWindow {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Resized for its window in the app: back as it was.
        if let Some((w, h, maximized)) = self.restore.filter(|_| unsafe { IsWindow(self.hwnd) } != 0) {
            let (x, y, keep) = self.moved_from.map_or((0, 0, SWP_NOMOVE), |(x, y)| (x, y, 0));
            unsafe {
                match maximized {
                    true => ShowWindow(self.hwnd, SW_MAXIMIZE),
                    false => SetWindowPos(self.hwnd, std::ptr::null_mut(), x, y, w, h, keep | SWP_NOZORDER | SWP_NOACTIVATE),
                };
            }
        }
    }
}

impl WinScreen {
    /// `display`: a monitor's number from `displays`, "all", `window:<id>` for
    /// an app's window, or None for the main monitor.
    pub fn open(display: Option<&str>) -> Result<WinScreen, String> {
        dpi_aware();
        if let Some(id) = display.and_then(|d| d.strip_prefix("window:")) {
            return Self::open_window(id);
        }
        let ms = monitors();
        let rect = match display {
            Some("all") => virtual_screen(),
            Some(n) => ms.get(n.parse::<usize>().map_err(|_| format!("No display called {n}."))?.saturating_sub(1)).map(|m| m.1).ok_or_else(|| format!("No display called {n}."))?,
            None => ms.iter().find(|m| m.2).or(ms.first()).map(|m| m.1).unwrap_or_else(virtual_screen),
        };
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return Err("Windows has no screen here (nobody signed in?).".into());
        }
        Self::with(rect, None)
    }

    fn open_window(id: &str) -> Result<WinScreen, String> {
        let closed = || crate::desktop::WINDOW_CLOSED.to_string();
        let hwnd = id.parse::<usize>().map_err(|_| closed())? as HWND;
        if unsafe { IsWindow(hwnd) } == 0 {
            return Err(closed());
        }
        // Minimized, it draws nothing: bring it back.
        if unsafe { IsIconic(hwnd) } != 0 {
            unsafe { ShowWindow(hwnd, SW_RESTORE) };
            std::thread::sleep(Duration::from_millis(300));
        }
        let (whole, visible) = window_bounds(hwnd).ok_or_else(closed)?;
        if visible.right <= visible.left || visible.bottom <= visible.top {
            return Err("That window has no size.".into());
        }
        // (Not brought to the front: it's captured even when covered, and a
        // click brings it. A screenshot changes nothing there.)
        Self::with(visible, Some(AppWindow::new(hwnd, whole)))
    }

    fn with(rect: RECT, window: Option<AppWindow>) -> Result<WinScreen, String> {
        let area = window.as_ref().map_or(rect, |w| w.whole);
        let (w, h) = (area.right - area.left, area.bottom - area.top);
        unsafe {
            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                return Err("Can't reach the screen (nobody signed in to the desktop?).".into());
            }
            let mem_dc = CreateCompatibleDC(screen_dc);
            let Some((bitmap, bits)) = dib(mem_dc, w, h) else {
                DeleteDC(mem_dc);
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                return Err("Can't make room for the screen.".into());
            };
            let shown = ((rect.right - rect.left) * (rect.bottom - rect.top) * 4) as usize;
            Ok(WinScreen { rect, window, screen_dc, mem_dc, bitmap, bits, bits_size: (w, h), frame: vec![0; shown], held: Vec::new(), buttons: Vec::new() })
        }
    }

    pub fn size(&self) -> (usize, usize) {
        ((self.rect.right - self.rect.left) as usize, (self.rect.bottom - self.rect.top) as usize)
    }

    /// An app's window: its title and how much Windows scales it up (1.5 at
    /// 150%), for the app to size its own window by.
    pub fn info(&self) -> serde_json::Map<String, Value> {
        let mut info = serde_json::Map::new();
        if let Some(w) = &self.window {
            info.insert("title".into(), title_of(w.hwnd).into());
            info.insert("scale".into(), scale_of(w.hwnd).into());
        }
        info
    }

    /// An app's window: what changed for the app to know (see desktop.rs's `Screen::meta`).
    pub fn meta(&mut self) -> Option<Value> {
        let rect = self.rect;
        let w = self.window.as_mut()?;
        let mut changed = serde_json::Map::new();
        let minimized = unsafe { IsIconic(w.hwnd) } != 0;
        if w.minimized != Some(minimized) {
            w.minimized = Some(minimized);
            changed.insert("minimized".into(), minimized.into());
        }
        if let Some(kind) = pointer_over(rect).filter(|k| *k != w.cursor) {
            w.cursor = kind;
            changed.insert("cursor".into(), kind.into());
        }
        if let Some(drag) = w.drag.lock().unwrap().take() {
            changed.insert("drag".into(), json!(drag));
        }
        (!changed.is_empty()).then_some(Value::Object(changed))
    }

    /// Follows an app's window as it moves and changes size; true when its size changed.
    pub fn resized(&mut self) -> bool {
        let Some(hwnd) = self.window.as_ref().map(|w| w.hwnd) else { return false };
        if unsafe { IsIconic(hwnd) } != 0 {
            return false;
        }
        let Some((whole, visible)) = window_bounds(hwnd) else { return false };
        let (w, h) = (whole.right - whole.left, whole.bottom - whole.top);
        if visible.right <= visible.left || visible.bottom <= visible.top || w <= 0 || h <= 0 {
            return false;
        }
        if (w, h) != self.bits_size {
            let Some((bitmap, bits)) = (unsafe { dib(self.mem_dc, w, h) }) else { return false };
            unsafe { DeleteObject(self.bitmap as HGDIOBJ) };
            (self.bitmap, self.bits, self.bits_size) = (bitmap, bits, (w, h));
        }
        let before = self.size();
        self.rect = visible;
        if let Some(win) = self.window.as_mut() {
            win.whole = whole;
        }
        let (fw, fh) = self.size();
        if (fw, fh) == before {
            return false;
        }
        self.frame = vec![0; fw * fh * 4];
        true
    }

    /// The screen now, with the mouse pointer drawn in.
    pub fn grab(&mut self) -> Option<Option<&[u8]>> {
        if let Some(win) = &self.window {
            return self.grab_window(win.hwnd, win.whole);
        }
        let (w, h) = self.size();
        unsafe {
            // Fails while the secure desktop is up (a UAC prompt, the lock
            // screen): keep showing the last frame.
            if BitBlt(self.mem_dc, 0, 0, w as i32, h as i32, self.screen_dc, self.rect.left, self.rect.top, SRCCOPY | CAPTUREBLT) == 0 {
                return Some(None);
            }
            self.draw_pointer(self.rect.left, self.rect.top);
            GdiFlush();
            std::ptr::copy_nonoverlapping(self.bits, self.frame.as_mut_ptr(), self.frame.len());
        }
        Some(Some(&self.frame))
    }

    /// An app's window now. Uncovered, it's that part of the screen, so its
    /// menus and dialogs show too; under another app's window, the window alone.
    fn grab_window(&mut self, hwnd: HWND, whole: RECT) -> Option<Option<&[u8]>> {
        if unsafe { IsWindow(hwnd) } == 0 {
            return None;
        }
        if unsafe { IsIconic(hwnd) } != 0 {
            return Some(None);
        }
        let (fw, fh) = self.size();
        let (ix, iy) = ((self.rect.left - whole.left).max(0), (self.rect.top - whole.top).max(0));
        unsafe {
            // (Past a screen's edge the screen has nothing to copy: the window draws itself.)
            let drawn = if on_screen(self.rect) && !covered(hwnd) {
                BitBlt(self.mem_dc, ix, iy, fw as i32, fh as i32, self.screen_dc, self.rect.left, self.rect.top, SRCCOPY | CAPTUREBLT)
            } else {
                windows_sys::Win32::Storage::Xps::PrintWindow(hwnd, self.mem_dc, PW_RENDERFULLCONTENT)
            };
            if drawn == 0 {
                return Some(None);
            }
            // (No pointer drawn in: the app shows its own, shaped like this one; see `meta`.)
            GdiFlush();
            // The visible part, out of the whole window.
            let (bw, bh) = (self.bits_size.0 as usize, self.bits_size.1 as usize);
            let bits = std::slice::from_raw_parts(self.bits, bw * bh * 4);
            let (ix, iy) = (ix as usize, iy as usize);
            let w = fw.min(bw.saturating_sub(ix));
            for row in 0..fh.min(bh.saturating_sub(iy)) {
                let src = ((iy + row) * bw + ix) * 4;
                self.frame[row * fw * 4..(row * fw + w) * 4].copy_from_slice(&bits[src..src + w * 4]);
            }
        }
        Some(Some(&self.frame))
    }

    /// Draws the mouse pointer into the bitmap, whose top left is at (`left`, `top`) on the desktop.
    unsafe fn draw_pointer(&self, left: i32, top: i32) {
        unsafe {
            let mut ci: CURSORINFO = std::mem::zeroed();
            ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
            if GetCursorInfo(&mut ci) == 0 || ci.flags & CURSOR_SHOWING == 0 {
                return;
            }
            let p = ci.ptScreenPos;
            let mut ii: ICONINFO = std::mem::zeroed();
            if GetIconInfo(ci.hCursor, &mut ii) != 0 {
                DrawIconEx(self.mem_dc, p.x - left - ii.xHotspot as i32, p.y - top - ii.yHotspot as i32, ci.hCursor, 0, 0, 0, std::ptr::null_mut(), DI_NORMAL);
                if !ii.hbmMask.is_null() {
                    DeleteObject(ii.hbmMask as HGDIOBJ);
                }
                if !ii.hbmColor.is_null() {
                    DeleteObject(ii.hbmColor as HGDIOBJ);
                }
            }
        }
    }

    /// The desktop's coordinates, as SendInput wants them (0..65535 across all screens).
    fn absolute(&self, e: &Value, frame: (usize, usize)) -> (i32, i32) {
        let (w, h) = self.size();
        let (left, top) = self.window.as_ref().and_then(|w| w.held_at).unwrap_or((self.rect.left, self.rect.top));
        let x = left as f64 + e["x"].as_f64().unwrap_or(0.) * w as f64 / frame.0 as f64;
        let y = top as f64 + e["y"].as_f64().unwrap_or(0.) * h as f64 / frame.1 as f64;
        let v = virtual_screen();
        let nx = ((x - v.left as f64) * 65535. / ((v.right - v.left - 1).max(1)) as f64).round() as i32;
        let ny = ((y - v.top as f64) * 65535. / ((v.bottom - v.top - 1).max(1)) as f64).round() as i32;
        (nx, ny)
    }

    pub fn apply(&mut self, e: &Value, frame: (usize, usize)) {
        let kind = e["t"].as_str().unwrap_or("");
        if let Some(hwnd) = self.window.as_ref().map(|w| w.hwnd) {
            let down = e["down"].as_bool().unwrap_or(false);
            match kind {
                // Clicks, the wheel and keys are for this window: bring it to
                // the front first. Keys only go when it's there, never to another app.
                "button" if down => {
                    raise(hwnd);
                    let at = (self.rect.left, self.rect.top);
                    self.window.as_mut().map(|w| w.held_at.get_or_insert(at));
                }
                "scroll" => raise(hwnd),
                "key" | "text" if down || kind == "text" => {
                    raise(hwnd);
                    if !crate::desktop::comes_to_front(|| in_front(hwnd)) {
                        return;
                    }
                }
                "resize" => return self.resize_window(hwnd, e),
                // Its window in the app came back: so does it.
                "restore" => return raise(hwnd),
                _ => {}
            }
        }
        match kind {
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
                if let Some(w) = self.window.as_mut().filter(|_| self.buttons.is_empty()) {
                    w.held_at = None;
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
                // A character at a time, a moment apart: apps miss some in one
                // long burst (Windows 11's Notepad).
                for unit in e["s"].as_str().unwrap_or("").encode_utf16() {
                    let mut events = Vec::new();
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
                            // The key that types it, as a keyboard would, when it takes
                            // no Shift, Ctrl or Alt; else the character itself (apps
                            // read Shift as already up in a burst like this one).
                            let scan = unsafe { VkKeyScanW(u) };
                            if scan != -1 && u < 0x80 && scan as u16 & 0x700 == 0 {
                                let vk = (scan as u16) & 0xff;
                                events.push(key(vk, 0, 0));
                                events.push(key(vk, 0, KEYEVENTF_KEYUP));
                            } else {
                                events.push(key(0, u, KEYEVENTF_UNICODE));
                                events.push(key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                            }
                        }
                    }
                    send(&events);
                    std::thread::sleep(Duration::from_millis(8));
                }
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

    /// Resizes an app's window to `w`×`h` (in 100% pixels: Windows scales
    /// them up as it does the window), from the app's window being resized.
    fn resize_window(&mut self, hwnd: HWND, e: &Value) {
        let scale = scale_of(hwnd);
        let screen = virtual_screen();
        let want = |k: &str, min: f64, max: i32| (e[k].as_f64().unwrap_or(0.).max(min) * scale).round().min(max as f64) as i32;
        let (w, h) = (want("w", 240., screen.right - screen.left), want("h", 160., screen.bottom - screen.top));
        let Some((whole, visible)) = window_bounds(hwnd) else { return };
        if let Some(win) = self.window.as_mut().filter(|w| w.restore.is_none()) {
            win.restore = Some((whole.right - whole.left, whole.bottom - whole.top, unsafe { IsZoomed(hwnd) } != 0));
        }
        // A maximized window keeps its size: make it an ordinary one first.
        if unsafe { IsZoomed(hwnd) } != 0 {
            unsafe { ShowWindow(hwnd, SW_RESTORE) };
        }
        let (whole, visible) = window_bounds(hwnd).unwrap_or((whole, visible));
        // The invisible borders come on top of what shows.
        let (bw, bh) = ((whole.right - whole.left) - (visible.right - visible.left), (whole.bottom - whole.top) - (visible.bottom - visible.top));
        // Kept on its screen: past its edge the screen has nothing to show
        // (and apps drawn by DirectComposition don't draw themselves there either).
        let at = (visible.left, visible.top);
        let to = work_area(hwnd).map_or(at, |a| crate::desktop::kept_inside((a.left, a.top, a.right, a.bottom), at, (w, h)));
        let (dx, dy) = (visible.left - whole.left, visible.top - whole.top);
        let mut flags = SWP_NOZORDER | SWP_NOACTIVATE;
        if to == at {
            flags |= SWP_NOMOVE;
        } else if let Some(win) = self.window.as_mut() {
            win.moved_from.get_or_insert((whole.left, whole.top));
        }
        unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), to.0 - dx, to.1 - dy, w + bw, h + bh, flags) };
    }
}

/// A top-down 32-bit bitmap of `w`×`h` for `dc`, selected into it, and its pixels.
unsafe fn dib(dc: HDC, w: i32, h: i32) -> Option<(HBITMAP, *mut u8)> {
    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if bitmap.is_null() || bits.is_null() {
            return None;
        }
        SelectObject(dc, bitmap as HGDIOBJ);
        Some((bitmap, bits as *mut u8))
    }
}

// --- Apps' windows -----------------------------------------------------------

/// A window's whole rectangle on the desktop, and the part of it that shows
/// (without the invisible borders), in real pixels.
fn window_bounds(hwnd: HWND) -> Option<(RECT, RECT)> {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    unsafe {
        let mut whole: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut whole) == 0 {
            return None;
        }
        let mut visible: RECT = std::mem::zeroed();
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut visible as *mut RECT as _, std::mem::size_of::<RECT>() as u32) != 0 {
            visible = whole;
        }
        Some((whole, visible))
    }
}

/// Whether the window is the one in front, or one of its own (a dialog, a
/// menu's window) is.
fn in_front(hwnd: HWND) -> bool {
    unsafe {
        let front = GetForegroundWindow();
        !front.is_null() && (front == hwnd || GetAncestor(front, GA_ROOTOWNER) == hwnd)
    }
}

/// Whether another app's window lies over part of this one (its own menus,
/// dialogs and tooltips don't count, nor click-through overlays).
fn covered(hwnd: HWND) -> bool {
    let Some((_, mine)) = window_bounds(hwnd) else { return false };
    let process = |h: HWND| {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        pid
    };
    let own = process(hwnd);
    let mut above = unsafe { GetWindow(hwnd, GW_HWNDPREV) };
    while !above.is_null() {
        let ex = unsafe { GetWindowLongPtrW(above, GWL_EXSTYLE) } as u32;
        if unsafe { IsWindowVisible(above) } != 0 && ex & WS_EX_TRANSPARENT == 0 && !cloaked(above) && process(above) != own
            && let Some((_, r)) = window_bounds(above)
            && r.left < mine.right && mine.left < r.right && r.top < mine.bottom && mine.top < r.bottom
        {
            return true;
        }
        above = unsafe { GetWindow(above, GW_HWNDPREV) };
    }
    false
}

/// The part of the window's monitor that windows go on (without the taskbar).
fn work_area(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        (!monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0).then_some(info.rcWork)
    }
}

/// Whether all of `r` lies on the desktop's monitors (each corner on one).
fn on_screen(r: RECT) -> bool {
    use windows_sys::Win32::Foundation::POINT;
    [(r.left, r.top), (r.right - 1, r.top), (r.left, r.bottom - 1), (r.right - 1, r.bottom - 1)]
        .into_iter()
        .all(|(x, y)| !unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL) }.is_null())
}

/// Whether Windows keeps the window out of sight (suspended Store apps, other
/// virtual desktops' windows).
fn cloaked(hwnd: HWND) -> bool {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    let mut cloaked = 0u32;
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as _, 4) == 0 && cloaked != 0 }
}

/// Brings an app's window to the front, uncovered, so clicks and keys reach it.
fn raise(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        let front = in_front(hwnd) || SetForegroundWindow(hwnd) != 0;
        if front && !covered(hwnd) {
            return;
        }
        // Windows lets only some programs pick the window in front. Above the
        // others it goes anyway, and a click on it then brings it in front.
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// The shape of the pointer, when it's over `rect`: "arrow", "text", "hand",
/// "ew", "ns", "nwse", "nesw", "move", "no", "crosshair", "wait".
fn pointer_over(rect: RECT) -> Option<&'static str> {
    static SHAPES: std::sync::OnceLock<Vec<(usize, &'static str)>> = std::sync::OnceLock::new();
    let shapes = SHAPES.get_or_init(|| {
        let shape = |id: windows_sys::core::PCWSTR| unsafe { LoadCursorW(std::ptr::null_mut(), id) } as usize;
        vec![
            (shape(IDC_ARROW), "arrow"),
            (shape(IDC_IBEAM), "text"),
            (shape(IDC_HAND), "hand"),
            (shape(IDC_SIZEWE), "ew"),
            (shape(IDC_SIZENS), "ns"),
            (shape(IDC_SIZENWSE), "nwse"),
            (shape(IDC_SIZENESW), "nesw"),
            (shape(IDC_SIZEALL), "move"),
            (shape(IDC_NO), "no"),
            (shape(IDC_CROSS), "crosshair"),
            (shape(IDC_WAIT), "wait"),
            (shape(IDC_APPSTARTING), "wait"),
        ]
    });
    let mut ci: CURSORINFO = unsafe { std::mem::zeroed() };
    ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
    if unsafe { GetCursorInfo(&mut ci) } == 0 {
        return None;
    }
    let p = ci.ptScreenPos;
    if p.x < rect.left || p.x >= rect.right || p.y < rect.top || p.y >= rect.bottom {
        return None;
    }
    // An app's own pointer: the arrow is the nearest thing.
    Some(shapes.iter().find(|(h, _)| *h == ci.hCursor as usize).map_or("arrow", |(_, k)| *k))
}

/// The parts of the window's top that drag it (its title bar, without its
/// buttons and tabs), as rectangles in its pixels: Windows is asked what's
/// at each point of a grid there, as it asks when the mouse goes down.
fn drag_areas(hwnd: HWND) -> Option<Vec<[i32; 4]>> {
    let (_, r) = window_bounds(hwnd)?;
    let scale = scale_of(hwnd);
    let caption = |x: i32, y: i32| {
        let at = (((r.top + y) as u32 & 0xffff) << 16 | ((r.left + x) as u32 & 0xffff)) as isize;
        let mut what = 0usize;
        let answered = unsafe { SendMessageTimeoutW(hwnd, WM_NCHITTEST, 0, at, SMTO_ABORTIFHUNG, 100, &mut what) } != 0;
        answered.then_some(what as u32 == HTCAPTION)
    };
    crate::desktop::drag_grid(r.right - r.left, r.bottom - r.top, ((6. * scale).round() as i32).max(4), (60. * scale) as i32, caption)
}

fn title_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// How much Windows scales the window up (1.0 at 100%, 1.5 at 150%).
fn scale_of(hwnd: HWND) -> f64 {
    match unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd) } {
        0 => 1.,
        dpi => dpi as f64 / 96.,
    }
}

/// The name of the program a window belongs to ("notepad"). A Store app's
/// window is ApplicationFrameHost's, holding the app's own window: that one's.
fn program_of(hwnd: HWND) -> String {
    let pid_of = |h: HWND| {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        pid
    };
    let host = pid_of(hwnd);
    let name = process_name(host);
    if !name.eq_ignore_ascii_case("ApplicationFrameHost") {
        return name;
    }
    unsafe extern "system" fn each(child: HWND, data: LPARAM) -> i32 {
        let (host, found) = unsafe { &mut *(data as *mut (u32, u32)) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(child, &mut pid) };
        if pid != *host {
            *found = pid;
            return 0;
        }
        1
    }
    let mut search = (host, 0u32);
    unsafe { EnumChildWindows(hwnd, Some(each), &mut search as *mut (u32, u32) as LPARAM) };
    match search.1 {
        0 => name,
        app => process_name(app),
    }
}

/// A process's program name ("notepad"), or "" when Windows won't say.
fn process_name(pid: u32) -> String {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) != 0;
        CloseHandle(process);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// A window Alt+Tab would list: shown, with a title, not a tool window or a
/// window another owns (dialogs, menus), and not hidden away by Windows
/// (suspended Store apps are "cloaked").
fn is_app_window(hwnd: HWND) -> bool {
    unsafe {
        if IsWindowVisible(hwnd) == 0 || hwnd == GetShellWindow() {
            return false;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_APPWINDOW == 0 && (ex & WS_EX_TOOLWINDOW != 0 || !GetWindow(hwnd, GW_OWNER).is_null()) {
            return false;
        }
    }
    !cloaked(hwnd) && !title_of(hwnd).trim().is_empty()
}

/// `muxen-agent windows` on Windows: the apps' windows on the desktop, front
/// first, each with the id to show it by (`window:<id>`).
pub fn windows() -> Value {
    dpi_aware();
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> i32 {
        unsafe { (*(data as *mut Vec<HWND>)).push(hwnd) };
        1
    }
    let mut all: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(each), &mut all as *mut Vec<HWND> as LPARAM) };
    let list: Vec<Value> = all
        .into_iter()
        .filter(|h| is_app_window(*h))
        .map(|h| {
            let r = window_bounds(h).map_or(RECT { left: 0, top: 0, right: 0, bottom: 0 }, |(_, visible)| visible);
            json!({
                "id": (h as usize).to_string(),
                "title": title_of(h),
                "app": program_of(h),
                "width": r.right - r.left,
                "height": r.bottom - r.top,
                "minimized": unsafe { IsIconic(h) } != 0,
                "front": in_front(h),
            })
        })
        .collect();
    json!({ "ok": true, "windows": list })
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
/// (`screen`, `clip`, `displays`, `windows`) with each connection as the program's stdin
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
            if !matches!(args.first().map(String::as_str), Some("screen" | "clip" | "displays" | "windows" | "version")) {
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
