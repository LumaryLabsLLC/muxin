//! A machine's desktop, live: frames from its agent over the machine's link
//! (see agent/src/desktop.rs), and the mouse and keyboard sent back. X11
//! machines for now.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, SharedString, Corners, FocusHandle, Focusable, KeyBinding, KeyDownEvent, KeyUpEvent, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, NoAction, Pixels, Point, RenderImage, ScrollDelta, ScrollWheelEvent, Task, Window, canvas, div,
    prelude::*, px,
};
use serde_json::{Value, json};

use super::kit::{Button, ButtonStyle, icon, spinner};
use super::theme::ActiveTheme;
use crate::fleet::Engine;
use crate::fleet::desktop::{FRAME, INPUT, TILES, keysym};
use crate::fleet::link::Link;

const CONTEXT: &str = "Desktop";

/// Keys that would otherwise run Muxen's own shortcuts go to the machine.
pub fn bind_keys(cx: &mut App) {
    let mut keys = vec!["escape", "/", "tab", "shift-tab"];
    if cfg!(target_os = "macos") {
        keys.extend(["cmd-r", "cmd-t", "cmd-n", "cmd-f"]);
    } else {
        keys.extend(["ctrl-r", "ctrl-shift-r", "ctrl-1", "ctrl-2", "ctrl-,", "ctrl-f", "ctrl-j", "ctrl-n"]);
    }
    cx.bind_keys(keys.into_iter().map(|k| KeyBinding::new(k, NoAction, Some(CONTEXT))));
}

enum State {
    Connecting,
    Live,
    Ended(String),
}

enum Msg {
    /// The stream is up: its channel, frame size, and how it travels.
    Ready(Arc<Remote>, u32, u32, String),
    /// The screen picture changed (see `Canvas`).
    Changed,
    Ended(String),
}

/// The machine's screen as last received: patches land here as they arrive,
/// and the view takes a picture of it when it's ready to draw.
#[derive(Default)]
struct Canvas {
    w: u32,
    h: u32,
    /// BGRA, as GPUI draws images.
    data: Vec<u8>,
}

/// Frame widths to choose from; `NATIVE` is the screen's own size.
pub const NATIVE: u32 = 16384;
pub const RESOLUTIONS: [(&str, u32); 5] = [("720p", 1280), ("1080p", 1920), ("1440p", 2560), ("4K", 3840), ("Native", NATIVE)];

/// The desktop's channel on the machine's link.
struct Remote {
    link: Arc<Link>,
    channel: u32,
}

impl Remote {
    fn input(&self, event: Value) {
        self.link.send(self.channel, INPUT, &serde_json::to_vec(&event).unwrap_or_default());
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.link.close_channel(self.channel);
    }
}

pub struct DesktopView {
    engine: Arc<Engine>,
    machine: String,
    /// The frame width asked for (None: native on a local network, else 1080p).
    width: Option<u32>,
    /// What the stream ended up using, and how it travels ("Direct · Local network").
    chosen: u32,
    via: String,
    canvas: Arc<std::sync::Mutex<Canvas>>,
    /// A Changed message is on its way (so patches don't queue up draws).
    pending: Arc<std::sync::atomic::AtomicBool>,
    stream: Option<Task<()>>,
    state: State,
    remote: Option<Arc<Remote>>,
    frame: Option<Arc<RenderImage>>,
    /// The frame's size in pixels (what input coordinates refer to).
    size: (u32, u32),
    /// Where the frame was last drawn, for mapping the mouse.
    shown: Rc<Cell<Option<Bounds<Pixels>>>>,
    focus: FocusHandle,
    /// Modifier keys the machine has been told are down.
    held: Modifiers,
    last_move: Instant,
    scroll_rest: f32,
    frames: u64,
    /// Why the machine can't be reached ("Beelink is offline"), while it can't.
    offline: Option<String>,
    _tasks: Vec<Task<()>>,
}

impl Focusable for DesktopView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DesktopView {
    pub fn new(engine: Arc<Engine>, machine_id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = DesktopView {
            engine,
            machine: machine_id,
            width: None,
            chosen: 0,
            via: String::new(),
            canvas: Arc::new(std::sync::Mutex::new(Canvas::default())),
            pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stream: None,
            state: State::Connecting,
            remote: None,
            frame: None,
            size: (0, 0),
            shown: Rc::new(Cell::new(None)),
            focus: cx.focus_handle(),
            held: Modifiers::default(),
            last_move: Instant::now(),
            scroll_rest: 0.,
            frames: 0,
            offline: None,
            _tasks: Vec::new(),
        };
        view.start(window, cx);
        // Follow the machine: say so when it goes offline, and pick the stream
        // back up when it returns.
        let mut rx = view.engine.subscribe();
        let id = view.machine.clone();
        let follow = cx.spawn_in(window, async move |this, cx| {
            let mut last: Option<Option<String>> = None;
            loop {
                let offline = {
                    let snap = rx.borrow_and_update();
                    snap.machines.iter().find(|m| m.id == id).and_then(|m| {
                        (m.status == "offline").then(|| m.unreachable.as_ref().map(|u| u.title.clone()).unwrap_or_else(|| format!("Can't reach {}", m.name)))
                    })
                };
                if last.as_ref() != Some(&offline) {
                    last = Some(offline.clone());
                    if this.update_in(cx, |this, window, cx| this.set_offline(offline, window, cx)).is_err() {
                        break;
                    }
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        });
        view._tasks.push(follow);
        view
    }

    fn set_offline(&mut self, offline: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let back = self.offline.is_some() && offline.is_none();
        self.offline = offline;
        if back && !matches!(self.state, State::Live) {
            self.start(window, cx);
        }
        cx.notify();
    }

    /// The frame width in use, for the resolution picker.
    pub fn resolution(&self) -> u32 {
        self.width.unwrap_or(self.chosen)
    }

    /// How the stream travels, e.g. "Direct · Local network".
    pub fn via(&self) -> &str {
        &self.via
    }

    /// Switches resolution: a new stream at that width.
    pub fn set_resolution(&mut self, width: u32, window: &mut Window, cx: &mut Context<Self>) {
        if Some(width) != self.width {
            self.width = Some(width);
            self.start(window, cx);
        }
    }

    /// (Re)starts the stream from the machine.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remote = None;
        self.state = State::Connecting;
        // Each stream has its own flag: an old stream's undelivered draw request
        // mustn't hold up the new one.
        self.pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Msg>();
        let (engine, machine_id, wanted) = (self.engine.clone(), self.machine.clone(), self.width);
        let (canvas, pending) = (self.canvas.clone(), self.pending.clone());
        let rt = engine.runtime().clone();
        rt.spawn(async move {
            let Some((spec, _)) = engine.machine(&machine_id) else {
                let _ = tx.unbounded_send(Msg::Ended("That machine is gone.".into()));
                return;
            };
            let Some(link) = crate::fleet::link::get(&spec).await else {
                let _ = tx.unbounded_send(Msg::Ended(format!("Can't reach the agent on {}.", spec.name)));
                return;
            };
            let network = link.network();
            // Full resolution on a local network; 1080p further away.
            let width = wanted.unwrap_or(if matches!(network, Some("Local network" | "This computer")) { NATIVE } else { 1920 });
            let via = [Some(if link.transport() == "direct" { "Direct" } else { "Over ssh" }), network].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            let (channel, mut frames) = link.open_channel();
            let request = json!({ "op": "desktop", "channel": channel, "fps": 30, "quality": 78, "max_width": width, "tiles": true });
            match link.request(request, Duration::from_secs(20)).await {
                Ok(r) if r["ok"].as_bool() == Some(true) => {
                    let (w, h) = (r["width"].as_u64().unwrap_or(0) as u32, r["height"].as_u64().unwrap_or(0) as u32);
                    let _ = tx.unbounded_send(Msg::Ready(Arc::new(Remote { link: link.clone(), channel }), w, h, format!("{via}|{width}")));
                }
                Ok(r) => {
                    link.close_channel(channel);
                    let _ = tx.unbounded_send(Msg::Ended(r["error"].as_str().unwrap_or("The machine can't show its desktop.").to_string()));
                    return;
                }
                Err(e) => {
                    link.close_channel(channel);
                    let _ = tx.unbounded_send(Msg::Ended(e));
                    return;
                }
            }
            while let Some((kind, bytes)) = frames.recv().await {
                if kind != TILES && kind != FRAME {
                    continue;
                }
                // Decoding is real work; keep it off the runtime's async threads.
                let canvas = canvas.clone();
                let changed = tokio::task::spawn_blocking(move || if kind == TILES { paint_tiles(&canvas, &bytes) } else { paint_frame(&canvas, &bytes) })
                    .await
                    .unwrap_or(false);
                // One draw request at a time; the view takes the newest picture.
                if changed && !pending.swap(true, std::sync::atomic::Ordering::AcqRel) && tx.unbounded_send(Msg::Changed).is_err() {
                    return;
                }
            }
            let _ = tx.unbounded_send(Msg::Ended("The desktop stream stopped.".into()));
        });

        let feed = cx.spawn_in(window, async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let alive = this.update_in(cx, |this, window, cx| {
                    for msg in batch {
                        match msg {
                            Msg::Ready(remote, w, h, via) => {
                                this.remote = Some(remote);
                                this.size = (w, h);
                                this.state = State::Live;
                                let (via, width) = via.split_once('|').unwrap_or((&via, "0"));
                                this.via = via.to_string();
                                this.chosen = width.parse().unwrap_or(0);
                            }
                            Msg::Changed => {
                                this.pending.store(false, std::sync::atomic::Ordering::Release);
                                let picture = {
                                    let c = this.canvas.lock().unwrap();
                                    image::RgbaImage::from_raw(c.w, c.h, c.data.clone())
                                };
                                if let Some(picture) = picture {
                                    this.frames += 1;
                                    let image = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(picture)]));
                                    // Only the newest picture matters; free the one it replaces.
                                    if let Some(old) = this.frame.replace(image) {
                                        cx.drop_image(old, Some(window));
                                    }
                                }
                            }
                            Msg::Ended(why) => {
                                this.state = State::Ended(why);
                                this.remote = None;
                            }
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        // Replacing the task stops the previous stream's feed.
        self.stream = Some(feed);
    }

    /// "Live", "Connecting…" or why it ended, for the page header.
    pub fn status(&self) -> String {
        if let (Some(why), false) = (&self.offline, matches!(self.state, State::Live)) {
            return why.clone();
        }
        match &self.state {
            State::Connecting => "Connecting…".into(),
            State::Live if self.frame.is_none() => "Waiting for the first frame…".into(),
            State::Live => format!("Live · {}×{}", self.size.0, self.size.1),
            State::Ended(why) => why.clone(),
        }
    }

    /// The mouse position in frame pixels, when it's over the frame.
    fn at(&self, p: Point<Pixels>) -> Option<(f64, f64)> {
        let r = self.shown.get()?;
        let (x, y) = (f32::from(p.x - r.origin.x), f32::from(p.y - r.origin.y));
        let (w, h) = (f32::from(r.size.width), f32::from(r.size.height));
        if x < 0. || y < 0. || x >= w || y >= h || w <= 0. || h <= 0. {
            return None;
        }
        Some(((x / w * self.size.0 as f32) as f64, (y / h * self.size.1 as f32) as f64))
    }

    fn send(&self, event: Value) {
        if let Some(r) = &self.remote {
            r.input(event);
        }
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
        // ~120 moves a second is plenty.
        if self.last_move.elapsed() < Duration::from_millis(8) {
            return;
        }
        if let Some((x, y)) = self.at(e.position) {
            self.last_move = Instant::now();
            self.send(json!({ "t": "move", "x": x, "y": y }));
        }
    }

    fn mouse_button(&mut self, button: MouseButton, position: Point<Pixels>, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        let b = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            _ => return,
        };
        if down {
            window.focus(&self.focus);
            cx.notify();
        }
        if let Some((x, y)) = self.at(position) {
            self.send(json!({ "t": "button", "b": b, "down": down, "x": x, "y": y }));
        } else if !down {
            // Let go outside the frame: still release it there.
            self.send(json!({ "t": "button", "b": b, "down": false, "x": 0, "y": 0 }));
        }
    }

    fn scroll(&mut self, e: &ScrollWheelEvent, _: &mut Window, _: &mut Context<Self>) {
        let Some((x, y)) = self.at(e.position) else { return };
        let lines = match e.delta {
            ScrollDelta::Lines(l) => l.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / 40.,
        };
        self.scroll_rest += lines;
        let notches = self.scroll_rest.trunc();
        if notches != 0. {
            self.scroll_rest -= notches;
            // Wheel up scrolls content up: X's button 4 (negative dy).
            self.send(json!({ "t": "scroll", "dy": -(notches as i64), "dx": 0, "x": x, "y": y }));
        }
    }

    /// Tells the machine about modifier keys pressed or let go.
    fn modifiers(&mut self, m: Modifiers) {
        // ⌘ works as Ctrl there (copy, paste, ... as on a Mac).
        let ctrl = |m: &Modifiers| m.control || m.platform;
        for (now, before, sym) in [(m.shift, self.held.shift, 0xffe1u32), (ctrl(&m), ctrl(&self.held), 0xffe3), (m.alt, self.held.alt, 0xffe9)] {
            if now != before {
                self.send(json!({ "t": "key", "keysym": sym, "down": now }));
            }
        }
        self.held = m;
    }

    fn key(&mut self, key: &str, down: bool, cx: &mut Context<Self>) {
        if let Some(sym) = keysym(key) {
            self.send(json!({ "t": "key", "keysym": sym, "down": down }));
            cx.stop_propagation();
        }
    }
}

/// Paints a TILES message into the canvas: `[w][h][count]`, then each
/// `[x][y][w][h][len: u32][jpeg]`. Patches decode side by side.
fn paint_tiles(canvas: &std::sync::Mutex<Canvas>, msg: &[u8]) -> bool {
    let u16_at = |o: usize| msg.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize);
    let (Some(fw), Some(fh), Some(n)) = (u16_at(0), u16_at(2), u16_at(4)) else { return false };
    let mut patches = Vec::with_capacity(n);
    let mut o = 6;
    for _ in 0..n {
        let (Some(x), Some(y), Some(w), Some(h)) = (u16_at(o), u16_at(o + 2), u16_at(o + 4), u16_at(o + 6)) else { return false };
        let Some(len) = msg.get(o + 8..o + 12).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize) else { return false };
        let Some(jpeg) = msg.get(o + 12..o + 12 + len) else { return false };
        patches.push((x, y, w, h, jpeg));
        o += 12 + len;
    }
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8).min(patches.len().max(1));
    let per = patches.len().div_ceil(workers).max(1);
    let decoded: Vec<(usize, usize, image::RgbaImage)> = std::thread::scope(|s| {
        let handles: Vec<_> = patches
            .chunks(per)
            .map(|chunk| {
                s.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|(x, y, _, _, jpeg)| {
                            let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?.into_rgba8();
                            Some((*x, *y, img))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    let mut c = canvas.lock().unwrap();
    if (c.w as usize, c.h as usize) != (fw, fh) {
        *c = Canvas { w: fw as u32, h: fh as u32, data: vec![0; fw * fh * 4] };
    }
    for (x, y, img) in decoded {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let rgba = img.as_raw();
        for row in 0..h.min(fh.saturating_sub(y)) {
            let width = w.min(fw.saturating_sub(x));
            let dst = ((y + row) * fw + x) * 4;
            let src = row * w * 4;
            for (d, s) in c.data[dst..dst + width * 4].chunks_exact_mut(4).zip(rgba[src..src + width * 4].chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], 255]);
            }
        }
    }
    true
}

/// Paints a whole-frame JPEG (`[w][h][jpeg]`) into the canvas.
fn paint_frame(canvas: &std::sync::Mutex<Canvas>, msg: &[u8]) -> bool {
    let Some(img) = msg.get(4..).and_then(|j| image::load_from_memory_with_format(j, image::ImageFormat::Jpeg).ok()) else { return false };
    let mut data = img.into_rgba8();
    let (w, h) = data.dimensions();
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    *canvas.lock().unwrap() = Canvas { w, h, data: data.into_raw() };
    true
}

impl Render for DesktopView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let frame = self.frame.clone();
        let size = self.size;
        let shown = self.shown.clone();
        let picture = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let Some(frame) = frame else { return };
                // Fit the frame inside, keeping its shape.
                let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
                let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                let s = (bw / w).min(bh / h);
                let (dw, dh) = (w * s, h * s);
                let rect = Bounds::new(
                    gpui::point(bounds.origin.x + px((bw - dw) / 2.), bounds.origin.y + px((bh - dh) / 2.)),
                    gpui::size(px(dw), px(dh)),
                );
                shown.set(Some(rect));
                let _ = window.paint_image(rect, Corners::default(), frame, 0, false);
            },
        )
        .size_full();
        let offline = self.offline.clone().filter(|_| !matches!(self.state, State::Live));
        let waiting = match (&self.state, offline) {
            (_, Some(why)) => Some(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .child(icon("power", 28., theme.text_placeholder))
                    .child(div().text_size(px(14.)).text_color(theme.text).child(why))
                    .child(div().text_size(px(12.5)).text_color(theme.text_muted).child("The desktop comes back here when the machine does.")),
            ),
            (State::Ended(why), None) => Some(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .child(icon("monitor", 28., theme.text_placeholder))
                    .child(div().max_w(px(460.)).text_center().text_size(px(13.)).text_color(theme.text_muted).child(why.clone()))
                    .child(Button::new("desktop-retry", "Try again").icon("refresh-cw").on_click(cx.listener(|this, _, window, cx| {
                        this.start(window, cx);
                        cx.notify();
                    }))),
            ),
            _ if self.frame.is_none() => Some(
                div().flex().items_center().gap(px(10.)).child(spinner("desktop-wait", 16., theme.text_muted)).child(
                    div().text_size(px(13.)).text_color(theme.text_muted).child(self.status()),
                ),
            ),
            _ => None,
        };
        div()
            .id("desktop")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .bg(gpui::black())
            .cursor_default()
            .child(picture)
            // Over the last picture, dimmed: it's no longer live.
            .children(waiting.map(|w| div().absolute().inset_0().flex().items_center().justify_center().when(self.frame.is_some(), |d| d.bg(gpui::black().opacity(0.72))).child(w)))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, w, cx| this.mouse_button(e.button, e.position, true, w, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, e: &MouseDownEvent, w, cx| this.mouse_button(e.button, e.position, true, w, cx)))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, e: &MouseDownEvent, w, cx| this.mouse_button(e.button, e.position, true, w, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, e: &MouseUpEvent, w, cx| this.mouse_button(e.button, e.position, false, w, cx)))
            .on_mouse_up(MouseButton::Right, cx.listener(|this, e: &MouseUpEvent, w, cx| this.mouse_button(e.button, e.position, false, w, cx)))
            .on_mouse_up(MouseButton::Middle, cx.listener(|this, e: &MouseUpEvent, w, cx| this.mouse_button(e.button, e.position, false, w, cx)))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_modifiers_changed(cx.listener(|this, e: &ModifiersChangedEvent, _, _| this.modifiers(e.modifiers)))
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                this.modifiers(e.keystroke.modifiers);
                this.key(&e.keystroke.key, true, cx)
            }))
            .on_key_up(cx.listener(|this, e: &KeyUpEvent, _, cx| this.key(&e.keystroke.key, false, cx)))
    }
}

/// Status, network and resolution for a desktop, then `extra` controls (close,
/// pop out, full screen): the bar above every desktop view.
pub fn toolbar(view: &Entity<DesktopView>, extra: Vec<AnyElement>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (status, via, current) = {
        let v = view.read(cx);
        (v.status(), v.via().to_string(), v.resolution())
    };
    // 720p · 1080p · 1440p · 4K · Native
    let picker = div().flex().gap(px(2.)).p(px(2.)).rounded(px(7.)).bg(theme.element).children(RESOLUTIONS.iter().map(|(label, width)| {
        let (on, width, view) = (current == *width, *width, view.clone());
        div()
            .id(SharedString::from(format!("res-{label}")))
            .px(px(8.))
            .h(px(22.))
            .flex()
            .items_center()
            .rounded(px(5.))
            .cursor_pointer()
            .text_size(px(11.5))
            .text_color(if on { theme.text } else { theme.text_placeholder })
            .when(on, |d| d.bg(theme.element_selected))
            .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(*label)
            .on_click(move |_, window, cx| view.update(cx, |v, cx| v.set_resolution(width, window, cx)))
    }));
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(10.))
        .h(px(36.))
        .px(px(14.))
        .border_b_1()
        .border_color(theme.border_variant)
        .bg(theme.surface)
        // Status and network give way first when the bar is narrow.
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(icon("monitor", 14., theme.text_muted))
                .child(div().flex_none().text_size(px(12.5)).text_color(theme.text_muted).child(status))
                .when(!via.is_empty(), |d| {
                    d.child(div().flex_none().flex().items_center().gap(px(5.)).text_size(px(12.)).text_color(theme.text_placeholder).child(icon("network", 12., theme.text_placeholder)).child(via))
                }),
        )
        .child(picker.flex_none())
        .children(extra)
        .into_any_element()
}

/// A desktop in a window of its own (to put on another screen, or full screen).
pub struct DesktopWindow {
    view: Entity<DesktopView>,
}

impl Render for DesktopWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let full = window.is_fullscreen();
        let button = Button::new("desktop-fullscreen", if full { "Exit full screen" } else { "Full screen" })
            .style(ButtonStyle::Ghost)
            .icon(if full { "minimize" } else { "maximize" })
            .on_click(|_, window, _| window.toggle_fullscreen())
            .into_any_element();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().canvas)
            // Room for the traffic lights on macOS.
            .when(cfg!(target_os = "macos") && !full, |d| d.pt(px(28.)))
            .child(toolbar(&self.view, vec![button], cx))
            .child(div().flex_1().min_h(px(0.)).child(self.view.clone()))
    }
}

/// Opens a machine's desktop in its own window.
pub fn open_window(engine: Arc<Engine>, machine_id: String, name: String, cx: &mut App) {
    let bounds = Bounds::centered(None, gpui::size(px(1440.), px(900.)), cx);
    let options = gpui::WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions { title: Some(format!("{name} · Desktop").into()), appears_transparent: cfg!(target_os = "macos"), traffic_light_position: None }),
        window_min_size: Some(gpui::size(px(480.), px(320.))),
        app_id: Some("muxen".into()),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        super::theme::apply(window, cx);
        let view = cx.new(|cx| DesktopView::new(engine, machine_id, window, cx));
        window.focus(&view.focus_handle(cx));
        cx.new(|_| DesktopWindow { view })
    });
    if let Err(e) = opened {
        tracing::warn!("couldn't open the desktop window: {e:#}");
    }
}
