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
//! app → screen   INPUT  JSON {"t": "move" | "button" | "scroll" | "key" | "text", ...}
//! ```
//!
//! Coordinates are in frame pixels; the screen may be scaled down to fit
//! `max_width`, and input is mapped back onto it.

#[cfg(target_os = "linux")]
use std::path::PathBuf;

use serde_json::Value;

pub const FRAME: u8 = 20;
pub const INPUT: u8 = 21;
pub const HELLO: u8 = 22;
/// screen → app: only what changed, as JPEG patches (see `stream`).
pub const TILES: u8 = 23;

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
            Err(e) => {
                hello(&serde_json::json!({ "error": e }));
                1
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let id = match display.map(|d| d.parse::<u32>()) {
            None => 0,
            Some(Ok(id)) => id,
            Some(Err(_)) => {
                hello(&serde_json::json!({ "error": format!("No display called {}.", display.unwrap_or_default()) }));
                return 1;
            }
        };
        match mac::Mac::open(fps, max_width, id) {
            Ok(screen) => stream(screen, fps, quality as u8, max_width, tiles),
            Err(e) => {
                hello(&serde_json::json!({ "error": e }));
                1
            }
        }
    }
    #[cfg(windows)]
    {
        match crate::win_screen::WinScreen::open(display) {
            Ok(screen) => stream(screen, fps, quality as u8, max_width, tiles),
            Err(e) => {
                hello(&serde_json::json!({ "error": e }));
                1
            }
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
}

/// Streams a screen: `tiles`, only what changed as JPEG strips (the live
/// view); otherwise the whole frame as one JPEG whenever anything changed
/// (screenshots). Input arrives on stdin as it happens.
fn stream(mut screen: impl Screen, fps: u32, quality: u8, max_width: u32, tiles: bool) -> i32 {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use frames::{Patch, Scaler, TILE, encode};

    let (sw, sh) = screen.size();
    let fw = (max_width as usize).min(screen.max_frame()).min(sw).max(1);
    let fh = (sh * fw / sw).max(1);
    let mut info = screen.info();
    for (k, v) in [("width", fw), ("height", fh), ("screen_width", sw), ("screen_height", sh)] {
        info.insert(k.into(), v.into());
    }
    hello(&Value::Object(info));
    let scaler = Scaler::new(sw, sh, fw, fh);
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
    let mut stdout = std::io::stdout().lock();
    loop {
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
            Err(mpsc::RecvTimeoutError::Disconnected) => return 0,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        next = Instant::now() + tick;
        let src = match screen.grab() {
            None => return 1,
            Some(None) => continue,
            Some(Some(src)) => src,
        };
        if src.len() < sw * sh * 4 {
            return 1;
        }

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
            continue;
        }
        let sent = if tiles {
            // [width][height][count] then each [x][y][w][h][len: u32][jpeg]
            let mut msg = Vec::with_capacity(6 + patches.iter().map(|p| 12 + p.jpeg.len()).sum::<usize>());
            for v in [fw as u16, fh as u16, patches.len() as u16] {
                msg.extend(v.to_be_bytes());
            }
            for p in &patches {
                for v in [p.x, p.y, p.w, p.h] {
                    msg.extend(v.to_be_bytes());
                }
                msg.extend((p.jpeg.len() as u32).to_be_bytes());
                msg.extend(&p.jpeg);
            }
            crate::proto::write_frame(&mut stdout, TILES, &msg)
        } else {
            let Some(jpeg) = encode(&cur, fw * 4, 0, 0, fw, fh, quality) else { continue };
            let mut msg = Vec::with_capacity(4 + jpeg.len());
            msg.extend((fw as u16).to_be_bytes());
            msg.extend((fh as u16).to_be_bytes());
            msg.extend(jpeg);
            crate::proto::write_frame(&mut stdout, FRAME, &msg)
        };
        if sent.and_then(|_| stdout.flush()).is_err() {
            return 0;
        }
        // What the app has now, to compare the next frame with.
        match &mut prev {
            Some(p) => std::mem::swap(p, &mut cur),
            None => prev = Some(cur.clone()),
        }
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::collections::HashMap;

    use serde_json::Value;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, ImageFormat, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
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
        /// keysym → (keycode, needs shift)
        keys: HashMap<u32, (u8, bool)>,
        /// The monitor shown (None: all of them), and when its place was last checked.
        monitor: Option<String>,
        checked: std::time::Instant,
        /// Keys and buttons pressed and not yet let go: let go when the stream
        /// ends, so none stays held (a key held down freezes the desktop's input).
        pressed: Vec<u8>,
        buttons: Vec<u8>,
    }

    impl Drop for X11 {
        fn drop(&mut self) {
            for code in self.pressed.drain(..) {
                let _ = self.conn.xtest_fake_input(KEY_RELEASE_EVENT, code, 0, self.root, 0, 0, 0);
            }
            for b in self.buttons.drain(..) {
                let _ = self.conn.xtest_fake_input(BUTTON_RELEASE_EVENT, b, 0, self.root, 0, 0, 0);
            }
            let _ = self.conn.flush();
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
        /// The desktop, or one monitor of it (a RandR monitor's name; "all": everything).
        pub fn open(monitor: Option<&str>) -> Result<X11, String> {
            let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("Can't reach the desktop ({e}). Is someone logged in to a graphical session?"))?;
            let root = conn.setup().roots[screen_num].root;
            let monitor = monitor.filter(|m| *m != "all").map(str::to_string);
            let (ox, oy, sw, sh) = area(&conn, root, monitor.as_deref())?;
            let shm = attach_shm(&conn, sw * sh * 4);
            let mut keys: HashMap<u32, (u8, bool)> = HashMap::new();
            let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
            if let Ok(Ok(map)) = conn.get_keyboard_mapping(min, max - min + 1).map(|c| c.reply()) {
                let per = map.keysyms_per_keycode.max(1) as usize;
                for (i, syms) in map.keysyms.chunks(per).enumerate() {
                    for (level, sym) in syms.iter().take(2).enumerate() {
                        if *sym != 0 {
                            keys.entry(*sym).or_insert((min + i as u8, level == 1));
                        }
                    }
                }
            }
            Ok(X11 { conn, root, ox, oy, sw, sh, shm, scratch: Vec::new(), keys, monitor, checked: std::time::Instant::now(), pressed: Vec::new(), buttons: Vec::new() })
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

        fn apply(&mut self, e: &Value, frame: (usize, usize)) {
            let (conn, root, keys) = (&self.conn, self.root, &self.keys);
            let scale = (self.sw as f64 / frame.0 as f64, self.sh as f64 / frame.1 as f64);
            let (ox, oy) = (self.ox, self.oy);
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
                    // Typing, for agents: each character's key, with Shift when it needs it.
                    let shift = keys.get(&0xffe1).map(|(c, _)| *c);
                    for ch in e["s"].as_str().unwrap_or("").chars() {
                        let sym = match ch {
                            '\n' => 0xff0d,
                            '\t' => 0xff09,
                            c => c as u32,
                        };
                        let Some((code, needs_shift)) = keys.get(&sym).copied() else { continue };
                        let shifted = needs_shift.then_some(shift).flatten();
                        if let Some(s) = shifted {
                            fake(KEY_PRESS_EVENT, s, 0, 0);
                        }
                        fake(KEY_PRESS_EVENT, code, 0, 0);
                        fake(KEY_RELEASE_EVENT, code, 0, 0);
                        if let Some(s) = shifted {
                            fake(KEY_RELEASE_EVENT, s, 0, 0);
                        }
                        let _ = conn.flush();
                    }
                }
                "key" => {
                    let sym = e["keysym"].as_u64().unwrap_or(0) as u32;
                    // Letters arrive lowercase; shift comes as its own key.
                    let found = keys.get(&sym).or_else(|| char::from_u32(sym).map(|c| c.to_ascii_lowercase() as u32).and_then(|s| keys.get(&s)));
                    if let Some(&(code, _)) = found {
                        let down = e["down"].as_bool().unwrap_or(false);
                        fake(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, code, 0, 0);
                        self.pressed.retain(|x| *x != code);
                        if down {
                            self.pressed.push(code);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use serde_json::Value;

    type FrameFn = extern "C" fn(*mut std::ffi::c_void, *const u8, usize, usize, usize);

    unsafe extern "C" {
        fn muxen_display(id: u32, pixels_w: *mut usize, pixels_h: *mut usize, points_w: *mut f64, points_h: *mut f64, x: *mut f64, y: *mut f64);
        fn muxen_capture_allowed(ask: i32) -> i32;
        fn muxen_input_allowed(ask: i32) -> i32;
        fn muxen_capture_start(id: u32, fps: i32, width: usize, height: usize, f: FrameFn, ctx: *mut std::ffi::c_void, err: *mut u8, errlen: usize) -> i32;
        fn muxen_capture_stopped() -> i32;
        fn muxen_mouse(kind: u32, x: f64, y: f64, button: u32, clicks: i64);
        fn muxen_scroll(dy: i32, dx: i32);
        fn muxen_key(keycode: u16, down: i32, flags: u64);
        fn muxen_text(utf16: *const u16, len: usize);
    }

    /// The newest frame from ScreenCaptureKit, and whether it's new.
    #[derive(Default)]
    struct Latest {
        pixels: Vec<u8>,
        fresh: bool,
    }

    extern "C" fn on_frame(ctx: *mut std::ffi::c_void, bgra: *const u8, width: usize, height: usize, stride: usize) {
        let latest = unsafe { &*(ctx as *const Mutex<Latest>) };
        let mut l = latest.lock().unwrap();
        l.pixels.resize(width * height * 4, 0);
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
        /// Keys pressed and not yet let go, and where the pointer was: let go
        /// when the stream ends, so none stays held.
        pressed: Vec<u16>,
        at: (f64, f64),
    }

    impl Drop for Mac {
        fn drop(&mut self) {
            for code in self.pressed.drain(..) {
                unsafe { muxen_key(code, 0, 0) };
            }
            let (x, y) = self.at;
            for (i, up) in [LEFT_UP, RIGHT_UP, OTHER_UP].into_iter().enumerate() {
                if self.held[i] {
                    unsafe { muxen_mouse(up, x, y, i as u32, 1) };
                }
            }
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
                return Err(format!("On {}, allow muxen-agent under Privacy & Security › Screen Recording, then try again.", computer_name()));
            }
            let input = unsafe { muxen_input_allowed(1) } == 1;
            let fw = (max_width as usize).min(pw).max(1);
            let fh = (ph * fw / pw).max(1);
            let latest = Arc::new(Mutex::new(Latest::default()));
            let mut err = [0u8; 512];
            // ScreenCaptureKit keeps calling with this pointer while the process lives.
            let ctx = Arc::into_raw(latest.clone()) as *mut std::ffi::c_void;
            let started = unsafe { muxen_capture_start(id, fps as i32, fw, fh, on_frame, ctx, err.as_mut_ptr(), err.len()) };
            if started != 0 {
                let why = String::from_utf8_lossy(&err[..err.iter().position(|b| *b == 0).unwrap_or(err.len())]).into_owned();
                return Err(format!("Can't capture the screen: {why}"));
            }
            Ok(Mac { fw, fh, points: (ptw, pth), origin: (ox, oy), latest, frame: Vec::new(), input, held: [false; 3], flags: 0, last_click: None, pressed: Vec::new(), at: (ox, oy) })
        }

        fn point(&self, e: &Value, frame: (usize, usize)) -> (f64, f64) {
            let x = e["x"].as_f64().unwrap_or(0.) * self.points.0 / frame.0 as f64;
            let y = e["y"].as_f64().unwrap_or(0.) * self.points.1 / frame.1 as f64;
            (self.origin.0 + x, self.origin.1 + y)
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
            if unsafe { muxen_capture_stopped() } == 1 {
                return None;
            }
            let mut l = self.latest.lock().unwrap();
            if !l.fresh || l.pixels.len() < self.fw * self.fh * 4 {
                return Some(None);
            }
            l.fresh = false;
            std::mem::swap(&mut self.frame, &mut l.pixels);
            drop(l);
            Some(Some(self.frame.as_slice()))
        }

        fn info(&self) -> serde_json::Map<String, Value> {
            let mut m = serde_json::Map::new();
            if !self.input {
                m.insert("input".into(), false.into());
                m.insert("note".into(), format!("view only: allow muxen-agent under Accessibility on {}", computer_name()).into());
            }
            m
        }

        fn apply(&mut self, e: &Value, frame: (usize, usize)) {
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
                    if let Some(code) = keycode(sym) {
                        unsafe { muxen_key(code, down as i32, self.flags) };
                        self.pressed.retain(|x| *x != code);
                        if down {
                            self.pressed.push(code);
                        }
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

    /// An X keysym's macOS virtual keycode (a US keyboard's layout).
    fn keycode(sym: u32) -> Option<u16> {
        Some(match sym {
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
            _ => {
                let c = char::from_u32(sym)?.to_ascii_lowercase();
                match c {
                    'a' => 0, 's' => 1, 'd' => 2, 'f' => 3, 'h' => 4, 'g' => 5, 'z' => 6, 'x' => 7, 'c' => 8, 'v' => 9,
                    'b' => 11, 'q' => 12, 'w' => 13, 'e' => 14, 'r' => 15, 'y' => 16, 't' => 17,
                    '1' | '!' => 18, '2' | '@' => 19, '3' | '#' => 20, '4' | '$' => 21, '6' | '^' => 22, '5' | '%' => 23,
                    '=' | '+' => 24, '9' | '(' => 25, '7' | '&' => 26, '-' | '_' => 27, '8' | '*' => 28, '0' | ')' => 29,
                    ']' | '}' => 30, 'o' => 31, 'u' => 32, '[' | '{' => 33, 'i' => 34, 'p' => 35, 'l' => 37, 'j' => 38,
                    '\'' | '"' => 39, 'k' => 40, ';' | ':' => 41, '\\' | '|' => 42, ',' | '<' => 43, '/' | '?' => 44,
                    'n' => 45, 'm' => 46, '.' | '>' => 47, '`' | '~' => 50,
                    _ => return None,
                }
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_keys() {
            assert_eq!(keycode('a' as u32), Some(0));
            assert_eq!(keycode('A' as u32), Some(0));
            assert_eq!(keycode(0xff0d), Some(36));
            assert_eq!(keycode(0xffbe), Some(122));
            assert_eq!(keycode('!' as u32), Some(18));
            assert_eq!(modifier_flag(0xffeb), Some(0x100000));
        }
    }
}
