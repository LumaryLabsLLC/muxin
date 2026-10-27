//! A machine's desktop, live: frames from its agent over the machine's link
//! (see agent/src/desktop.rs), and the mouse and keyboard sent back. Or a
//! desktop someone shared with you, the same way over the share.

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

use super::kit::{self, Button, ButtonStyle, spinner};
use super::theme::ActiveTheme;
use crate::fleet::Engine;
use crate::fleet::desktop::{FRAME, INPUT, META, TILES, keysym};
use crate::fleet::link::Link;
use crate::share::guest;

const CONTEXT: &str = "Desktop";

/// Keys that would otherwise run Muxin's own shortcuts go to the machine.
pub fn bind_keys(cx: &mut App) {
    let mut keys: Vec<String> = ["escape", "/", "tab", "shift-tab"].map(String::from).to_vec();
    if cfg!(target_os = "macos") {
        keys.extend(["cmd-r", "cmd-t", "cmd-n", "cmd-f"].map(String::from));
    } else {
        keys.extend(super::shortcuts::ctrl_keys(cx));
    }
    cx.bind_keys(keys.iter().map(|k| KeyBinding::new(k, NoAction, Some(CONTEXT))));
}

enum State {
    Connecting,
    Live,
    Ended(String),
}

enum Msg {
    /// The stream is up: its channel, frame size, how it travels, a note, and
    /// what else the machine said about it.
    Ready(Arc<Remote>, u32, u32, String, Option<String>, Value),
    /// The screen picture changed (see `Canvas`).
    Changed,
    /// The stream ended: why, and whether it may well work again in a moment
    /// (the connection dropped, as opposed to the machine saying no).
    Ended(String, bool),
    /// Nobody's screen is there to show (nobody signed in): why.
    NoScreen(String),
    /// The monitor asked for is gone: showing the default one instead.
    Gone,
    /// The app's window shown was closed there: why.
    Closed(String),
    /// What changed about the app's window shown (see `apply_meta`).
    Meta(Value),
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

/// Tries at picking a dropped stream back up before saying it ended.
const RETRIES: u32 = 6;

/// A frame width meaning the screen's own size.
pub const NATIVE: u32 = 16384;

/// Monitor sizes with a short name, offered when the monitor has them.
const SIZES: [((u32, u32), &str); 4] = [((1280, 720), "720p"), ((1920, 1080), "1080p"), ((2560, 1440), "1440p"), ((3840, 2160), "4K")];

/// An app's window shown on its own (a `window:<id>` display), as its stream
/// describes it.
#[derive(Clone)]
pub struct AppShown {
    pub title: String,
    /// Frame pixels per point: how much its machine scales it up (2 at
    /// 200%), less how much the stream scales it down (on a slow network).
    pub scale: f32,
}

/// The desktop's channel: on the machine's link, or on a share.
enum Remote {
    Link(Arc<Link>, u32),
    Shared(Arc<guest::Joined>, u32),
}

impl Remote {
    fn input(&self, event: Value) {
        let bytes = serde_json::to_vec(&event).unwrap_or_default();
        match self {
            Remote::Link(link, channel) => link.send(*channel, INPUT, &bytes),
            Remote::Shared(joined, channel) => joined.send(*channel, INPUT, &bytes),
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        match self {
            Remote::Link(link, channel) => link.close_channel(*channel),
            Remote::Shared(joined, channel) => joined.close(*channel),
        }
    }
}

pub struct DesktopView {
    engine: Arc<Engine>,
    machine: String,
    /// Someone's shared desktop (no machine of ours), and whether we may use
    /// its mouse and keyboard.
    shared: Option<Arc<guest::Joined>>,
    can_control: bool,
    /// A shared desktop's quality ("low" to "max", see share::host).
    quality: &'static str,
    /// One of the machine's displays (None: the whole desktop, or a Mac's
    /// main display), or an app's window (`window:<id>`).
    display: Option<String>,
    /// That app's window, once its stream started; and the size last asked
    /// of it there (points), so a resize asks once.
    app: Option<AppShown>,
    asked: Option<(f32, f32)>,
    /// It was closed there.
    closed: bool,
    /// The pointer's shape over it, the parts of it that drag this window
    /// (frame pixels: x, y, w, h), whether it's minimized there, and whether
    /// this window is being dragged (that press's release isn't the app's).
    pointer: gpui::CursorStyle,
    drag: Vec<[f32; 4]>,
    minimized: bool,
    moving: bool,
    /// Until when an app's window says how to close it (its first few times).
    hint: Option<Instant>,
    /// The frame width asked for (None: native on a local network, else 1080p).
    width: Option<u32>,
    /// How the stream travels ("Direct · Local network").
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
    /// The area it was drawn in (to place things over the picture).
    area: Rc<Cell<Option<Bounds<Pixels>>>>,
    focus: FocusHandle,
    /// Modifier keys the machine has been told are down.
    held: Modifiers,
    /// Other keys (keysyms) and mouse buttons it has been told are down, let
    /// go when the view loses focus (else they'd stay held there).
    keys_down: Vec<u32>,
    buttons_down: Vec<u64>,
    last_move: Instant,
    scroll_rest: f32,
    frames: u64,
    /// Why the machine can't be reached ("Beelink is offline"), while it can't.
    offline: Option<String>,
    /// The machine is a Mac (⌘ stays ⌘).
    mac: bool,
    /// Its platform ("windows-x64"), for what its own remote desktop is called.
    platform: String,
    /// Nobody's screen is there (nobody signed in): its own remote desktop
    /// (RDP, VNC) is offered instead, which can sign someone in.
    no_screen: bool,
    /// Opening that, and what came of it (text, failed).
    opening: bool,
    opened: Option<(String, bool)>,
    /// What the machine says about the stream (e.g. view only).
    note: Option<String>,
    /// A monitor's size or scale being changed, and why the last change didn't happen.
    changing: bool,
    change_error: Option<String>,
    /// Picking a dropped stream back up: tries so far, and the next one.
    resuming: bool,
    retries: u32,
    retry: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl Focusable for DesktopView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DesktopView {
    pub fn new(engine: Arc<Engine>, machine_id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::make(engine, machine_id, None, None, "medium", window, cx)
    }

    /// Showing one of the machine's displays, or an app's window
    /// (`window:<id>`), from the start (not the default one first: two
    /// captures starting at once on a Mac can leave one never starting).
    pub fn showing(engine: Arc<Engine>, machine_id: String, display: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::make(engine, machine_id, None, display, "medium", window, cx)
    }

    /// A desktop someone shared with you: input goes there only while they
    /// let you control it (`set_control`).
    pub fn shared(engine: Arc<Engine>, joined: Arc<guest::Joined>, mac: bool, quality: &'static str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self::make(engine, String::new(), Some(joined), None, quality, window, cx);
        view.mac = mac;
        view
    }

    fn make(engine: Arc<Engine>, machine_id: String, shared: Option<Arc<guest::Joined>>, display: Option<String>, quality: &'static str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let can_control = shared.as_ref().is_none_or(|j| j.can_control());
        let mut view = DesktopView {
            engine,
            machine: machine_id,
            shared,
            can_control,
            quality,
            display: None,
            app: None,
            asked: None,
            closed: false,
            pointer: gpui::CursorStyle::Arrow,
            drag: Vec::new(),
            minimized: false,
            moving: false,
            hint: None,
            width: None,
            via: String::new(),
            canvas: Arc::new(std::sync::Mutex::new(Canvas::default())),
            pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stream: None,
            state: State::Connecting,
            remote: None,
            frame: None,
            size: (0, 0),
            shown: Rc::new(Cell::new(None)),
            area: Rc::new(Cell::new(None)),
            focus: cx.focus_handle(),
            held: Modifiers::default(),
            keys_down: Vec::new(),
            buttons_down: Vec::new(),
            last_move: Instant::now(),
            scroll_rest: 0.,
            frames: 0,
            offline: None,
            mac: false,
            platform: String::new(),
            no_screen: false,
            opening: false,
            opened: None,
            note: None,
            resuming: false,
            retries: 0,
            retry: None,
            changing: false,
            change_error: None,
            _tasks: Vec::new(),
            _subscriptions: Vec::new(),
        };
        view.platform = view.engine.current().machines.iter().find(|m| m.id == view.machine).map(|m| m.platform.clone()).unwrap_or_default();
        view.mac = view.platform.starts_with("darwin");
        view.display = display;
        view.start(window, cx);
        if view.shared.is_some() {
            let blur = cx.on_focus_out(&view.focus, window, |this, _, _, _| this.let_go());
            view._subscriptions = vec![blur];
            return view;
        }
        // Keys and buttons held when the view loses the keyboard (another
        // app, ⌘Q, a click elsewhere): let go on the machine too.
        let blur = cx.on_focus_out(&view.focus, window, |this, _, _, _| this.let_go());
        let away = cx.observe_window_activation(window, |this, window, _| {
            if !window.is_window_active() {
                this.let_go();
            } else if std::mem::take(&mut this.minimized) && this.shows_window() {
                // Its window here came back: so does the app's there.
                this.send(json!({ "t": "restore" }));
            }
        });
        view._subscriptions = vec![blur, away];
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
                        if m.is_disconnected() {
                            Some(format!("Disconnected from {}: connect to it to see its desktop", m.name))
                        } else {
                            (m.status == "offline").then(|| m.unreachable.as_ref().map(|u| u.title.clone()).unwrap_or_else(|| format!("Can't reach {}", m.name)))
                        }
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
            self.retries = 0;
            self.start(window, cx);
        }
        cx.notify();
    }

    /// Lets the mouse and keyboard through, or not (a shared desktop).
    pub fn set_control(&mut self, allowed: bool, cx: &mut Context<Self>) {
        if !allowed {
            self.let_go();
        }
        self.can_control = allowed;
        cx.notify();
    }

    /// A shared desktop's quality.
    pub fn quality(&self) -> &'static str {
        self.quality
    }

    /// Asks a shared desktop for another quality: a new stream, the picture
    /// kept until it comes.
    pub fn set_quality(&mut self, quality: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if quality != self.quality && self.shared.is_some() {
            self.quality = quality;
            self.retries = 0;
            self.start(window, cx);
            cx.notify();
        }
    }

    /// How the stream travels, e.g. "Direct · Local network".
    pub fn via(&self) -> &str {
        &self.via
    }

    /// The display shown (None: the default one).
    pub fn display(&self) -> Option<&str> {
        self.display.as_deref()
    }

    /// Whether it shows an app's window rather than a display.
    pub fn shows_window(&self) -> bool {
        self.display.as_deref().is_some_and(|d| d.starts_with("window:"))
    }

    /// The app's window shown, once its stream has started.
    pub fn app(&self) -> Option<&AppShown> {
        self.app.as_ref()
    }

    /// Whether the app's window shown was closed there.
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// Whether the picture is up (not connecting, reconnecting or failed).
    pub fn live(&self) -> bool {
        matches!(self.state, State::Live) && self.frame.is_some() && self.offline.is_none()
    }

    /// The size last asked of the app's window there (points).
    pub fn asked(&self) -> Option<(f32, f32)> {
        self.asked
    }

    /// What changed about the app's window shown: its pointer's shape, the
    /// parts of it that drag this window, whether it was minimized there.
    fn apply_meta(&mut self, meta: &Value, window: &mut Window) {
        if let Some(kind) = meta["cursor"].as_str() {
            self.pointer = pointer_style(kind);
        }
        if let Some(rects) = meta["drag"].as_array() {
            self.drag = rects
                .iter()
                .filter_map(|r| {
                    let r = r.as_array()?;
                    let n = |i: usize| r.get(i).and_then(Value::as_f64).map(|v| v as f32);
                    Some([n(0)?, n(1)?, n(2)?, n(3)?])
                })
                .collect();
        }
        if let Some(minimized) = meta["minimized"].as_bool() {
            self.minimized = minimized;
            if minimized && self.shows_window() {
                window.minimize_window();
            }
        }
    }

    /// Whether a press here drags this window: an empty part of the app's
    /// own title bar.
    fn drags_window(&self, p: Point<Pixels>) -> bool {
        self.shows_window()
            && self.can_control
            && self.at(p).is_some_and(|(x, y)| {
                let (x, y) = (x as f32, y as f32);
                self.drag.iter().any(|r| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3])
            })
    }

    /// The app's window's size in points (as big as it looks there).
    pub fn app_size(&self) -> Option<(f32, f32)> {
        let app = self.app.as_ref().filter(|_| self.size.0 > 0 && self.size.1 > 0)?;
        Some((self.size.0 as f32 / app.scale, self.size.1 as f32 / app.scale))
    }

    /// An app's window has nothing of ours around it: its first few times, a
    /// moment saying how to close it.
    fn show_close_hint(&mut self, cx: &mut Context<Self>) {
        const TIMES: u32 = 3;
        if cx.global::<super::theme::Appearance>().close_hints >= TIMES {
            return;
        }
        super::theme::remember(cx, |a| a.close_hints += 1);
        let shown = Duration::from_secs(5);
        self.hint = Some(Instant::now() + shown);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(shown).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// How many screen pixels here each of the app window's pixels takes, when
    /// that's a whole number above one (it's at 100% there, on a Retina screen
    /// here): its picture is then enlarged that much, each pixel a sharp block,
    /// rather than smoothed into its neighbours as it's drawn.
    fn crisp(&self, window: &Window) -> Option<u32> {
        let app = self.app.as_ref().filter(|_| self.shows_window())?;
        let m = window.scale_factor() / app.scale;
        let whole = m.round();
        (whole >= 2. && (m - whole).abs() < 0.05).then_some(whole as u32)
    }

    /// Resizes the app's window there to fill the space it's drawn in here
    /// (after this window was resized).
    pub fn fit_app(&mut self) {
        let (Some(have), Some(area)) = (self.app_size(), self.area.get()) else { return };
        let want = (f32::from(area.size.width).round(), f32::from(area.size.height).round());
        let near = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 4. && (a.1 - b.1).abs() < 4.;
        if near(want, have) || self.asked.is_some_and(|a| near(a, want)) {
            return;
        }
        self.asked = Some(want);
        self.send(json!({ "t": "resize", "w": want.0, "h": want.1 }));
    }

    /// Shows another of the machine's displays: a new stream.
    pub fn set_display(&mut self, display: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if display != self.display {
            self.display = display;
            (self.app, self.asked) = (None, None);
            self.retries = 0;
            // Not the last monitor's picture while the new one comes.
            if let Some(old) = self.frame.take() {
                cx.drop_image(old, Some(window));
            }
            self.start(window, cx);
            cx.notify();
        }
    }

    /// Sets a monitor's size or scale on the machine; a new stream at its new
    /// size once it has one.
    pub fn change_display(&mut self, display: String, size: Option<(u32, u32)>, scale: Option<f64>, window: &mut Window, cx: &mut Context<Self>) {
        if self.changing {
            return;
        }
        self.changing = true;
        self.change_error = None;
        cx.notify();
        let (engine, machine) = (self.engine.clone(), self.machine.clone());
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.set_display(&machine, &display, size, scale).await }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.changing = false;
                match result {
                    Ok(()) if size.is_some() => {
                        this.retries = 0;
                        if let Some(old) = this.frame.take() {
                            cx.drop_image(old, Some(window));
                        }
                        this.start(window, cx);
                    }
                    Ok(()) => {}
                    Err(e) => this.change_error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the machine's own remote desktop (RDP, VNC) in this computer's
    /// app for it: what there is when nobody's screen is.
    fn open_remote(&mut self, cx: &mut Context<Self>) {
        self.opening = true;
        self.opened = None;
        let (engine, id) = (self.engine.clone(), self.machine.clone());
        let name = engine.current().machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.clone());
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.open_remote_desktop(&id, None).await }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.opening = false;
                this.opened = Some(match result {
                    Ok(r) => (r.summary(&name), false),
                    Err(e) => (e, true),
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// (Re)starts the stream from the machine.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remote = None;
        self.state = State::Connecting;
        self.no_screen = false;
        self.opened = None;
        // Each stream has its own flag: an old stream's undelivered draw request
        // mustn't hold up the new one.
        self.pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Msg>();
        let (engine, machine_id, wanted, mut display) = (self.engine.clone(), self.machine.clone(), self.width, self.display.clone().unwrap_or_default());
        let (canvas, pending) = (self.canvas.clone(), self.pending.clone());
        let rt = engine.runtime().clone();
        if let Some(joined) = self.shared.clone() {
            let (tx, quality) = (tx.clone(), self.quality);
            rt.spawn(async move {
                let (channel, reply, mut frames) = match joined.open(json!({ "op": "desktop", "quality": quality })).await {
                    Ok(opened) => opened,
                    Err(e) => {
                        let _ = tx.unbounded_send(Msg::Ended(e, false));
                        return;
                    }
                };
                let (w, h) = (reply["width"].as_u64().unwrap_or(0) as u32, reply["height"].as_u64().unwrap_or(0) as u32);
                let _ = tx.unbounded_send(Msg::Ready(Arc::new(Remote::Shared(joined, channel)), w, h, "Shared".into(), None, reply));
                while let Some((kind, bytes)) = frames.recv().await {
                    if kind != TILES && kind != FRAME {
                        continue;
                    }
                    let canvas = canvas.clone();
                    let changed = tokio::task::spawn_blocking(move || if kind == TILES { paint_tiles(&canvas, &bytes) } else { paint_frame(&canvas, &bytes) }).await.unwrap_or(false);
                    if changed && !pending.swap(true, std::sync::atomic::Ordering::AcqRel) && tx.unbounded_send(Msg::Changed).is_err() {
                        return;
                    }
                }
                let _ = tx.unbounded_send(Msg::Ended("They stopped sharing the desktop.".into(), false));
            });
        } else {
            rt.spawn(async move {
                let Some((spec, _)) = engine.machine(&machine_id) else {
                    let _ = tx.unbounded_send(Msg::Ended("That machine is gone.".into(), false));
                    return;
                };
                if engine.is_demo() {
                    let _ = tx.unbounded_send(Msg::Ended("The demo has no desktops to show.".into(), false));
                    return;
                }
                // Its monitors as they are now (one may have been added, removed
                // or resized since), for the picker; a gone one falls back to the default.
                // (An app's window isn't one of them.)
                let displays = match display.starts_with("window:") {
                    true => None,
                    false => engine.displays(&machine_id).await.ok(),
                };
                if let Some(displays) = displays {
                    if !display.is_empty() && display != "all" && !displays.iter().any(|d| d.id == display) {
                        display.clear();
                        let _ = tx.unbounded_send(Msg::Gone);
                    }
                    // The default: the main monitor (the agent's own default on
                    // Linux is the whole desktop).
                    if display.is_empty()
                        && displays.iter().any(|d| d.id == "all")
                        && let Some(main) = displays.iter().find(|d| d.main && d.id != "all")
                    {
                        display = main.id.clone();
                    }
                }
                let Some(link) = crate::fleet::link::get(&spec).await else {
                    let _ = tx.unbounded_send(Msg::Ended(format!("Can't reach the agent on {}.", spec.name), true));
                    return;
                };
                let network = link.network();
                // Full resolution on a local network; further away, 1080p (an app's
                // window: up to 4K wide, as a big one on a Retina Mac is).
                let far = if display.starts_with("window:") { 3840 } else { 1920 };
                let width = wanted.unwrap_or(if matches!(network, Some("Local network" | "This computer")) { NATIVE } else { far });
                let via = [Some(if link.transport() == "direct" { "Direct" } else { "Over ssh" }), network].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                let (channel, mut frames) = link.open_channel();
                let request = json!({ "op": "desktop", "channel": channel, "fps": 30, "quality": 78, "max_width": width, "tiles": true, "display": display });
                let (asked, what) = (Instant::now(), format!("{} {} (channel {channel}, {via})", spec.name, if display.is_empty() { "desktop" } else { display.as_str() }));
                tracing::info!("desktop {what}: asking");
                // (A Mac's window may take a second try at starting: see mac_screen.m.)
                let reply = link.request(request, Duration::from_secs(30)).await;
                match &reply {
                    Ok(r) if r["ok"].as_bool() == Some(true) => tracing::info!("desktop {what}: ready after {:?}", asked.elapsed()),
                    Ok(r) => tracing::info!("desktop {what}: refused after {:?}: {}", asked.elapsed(), r["error"]),
                    Err(e) => tracing::info!("desktop {what}: no answer after {:?}: {e}", asked.elapsed()),
                }
                match reply {
                    Ok(r) if r["ok"].as_bool() == Some(true) => {
                        let (w, h) = (r["width"].as_u64().unwrap_or(0) as u32, r["height"].as_u64().unwrap_or(0) as u32);
                        let note = r["note"].as_str().map(str::to_string);
                        let _ = tx.unbounded_send(Msg::Ready(Arc::new(Remote::Link(link.clone(), channel)), w, h, format!("{via}|{width}"), note, r));
                    }
                    Ok(r) => {
                        link.close_channel(channel);
                        let why = r["error"].as_str().unwrap_or("The machine can't show its desktop.").to_string();
                        let _ = tx.unbounded_send(match (r["no_session"].as_bool(), r["gone"].as_bool()) {
                            (Some(true), _) => Msg::NoScreen(why),
                            (_, Some(true)) => Msg::Closed(why),
                            _ => Msg::Ended(why, false),
                        });
                        return;
                    }
                    Err(e) => {
                        link.close_channel(channel);
                        let _ = tx.unbounded_send(Msg::Ended(e, true));
                        return;
                    }
                }
                let mut pictured = false;
                while let Some((kind, bytes)) = frames.recv().await {
                    if kind == META {
                        if let Ok(meta) = serde_json::from_slice::<Value>(&bytes)
                            && tx.unbounded_send(Msg::Meta(meta)).is_err()
                        {
                            tracing::info!("desktop {what}: closed here");
                            return;
                        }
                        continue;
                    }
                    if kind != TILES && kind != FRAME {
                        continue;
                    }
                    // Decoding is real work; keep it off the runtime's async threads.
                    let canvas = canvas.clone();
                    let changed = tokio::task::spawn_blocking(move || if kind == TILES { paint_tiles(&canvas, &bytes) } else { paint_frame(&canvas, &bytes) })
                        .await
                        .unwrap_or(false);
                    if changed && !std::mem::replace(&mut pictured, true) {
                        tracing::info!("desktop {what}: first picture after {:?}", asked.elapsed());
                    }
                    // One draw request at a time; the view takes the newest picture.
                    if changed && !pending.swap(true, std::sync::atomic::Ordering::AcqRel) && tx.unbounded_send(Msg::Changed).is_err() {
                        tracing::info!("desktop {what}: closed here");
                        return;
                    }
                }
                tracing::info!("desktop {what}: the machine ended it after {:?}", asked.elapsed());
                let _ = tx.unbounded_send(Msg::Ended("The desktop stream stopped.".into(), true));
            });
        }

        let feed = cx.spawn_in(window, async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let alive = this.update_in(cx, |this, window, cx| {
                    for msg in batch {
                        match msg {
                            Msg::Ready(remote, w, h, via, note, info) => {
                                this.note = note;
                                if this.shows_window() {
                                    let scale = info["scale"].as_f64().filter(|s| *s > 0.).unwrap_or(1.);
                                    let shrunk = w as f64 / info["screen_width"].as_f64().filter(|s| *s > 0.).unwrap_or(w as f64);
                                    this.app = Some(AppShown { title: info["title"].as_str().unwrap_or_default().to_string(), scale: (scale * shrunk) as f32 });
                                }
                                this.remote = Some(remote);
                                this.size = (w, h);
                                this.state = State::Live;
                                this.retries = 0;
                                this.resuming = false;
                                this.via = via.split_once('|').map_or(via.as_str(), |(v, _)| v).to_string();
                            }
                            Msg::Changed => {
                                this.pending.store(false, std::sync::atomic::Ordering::Release);
                                let picture = {
                                    let c = this.canvas.lock().unwrap();
                                    image::RgbaImage::from_raw(c.w, c.h, c.data.clone())
                                };
                                if let Some(mut picture) = picture {
                                    // An app's window changes size as it's resized there.
                                    this.size = picture.dimensions();
                                    this.frames += 1;
                                    if this.frames == 1 && this.shows_window() {
                                        this.show_close_hint(cx);
                                    }
                                    if let Some(m) = this.crisp(window) {
                                        picture = enlarge(picture, m);
                                    }
                                    let image = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(picture)]));
                                    // Only the newest picture matters; free the one it replaces.
                                    if let Some(old) = this.frame.replace(image) {
                                        cx.drop_image(old, Some(window));
                                    }
                                }
                            }
                            Msg::Gone => this.display = None,
                            Msg::Meta(meta) => this.apply_meta(&meta, window),
                            Msg::Closed(why) => {
                                this.remote = None;
                                this.resuming = false;
                                this.closed = true;
                                this.state = State::Ended(why);
                            }
                            Msg::NoScreen(why) => {
                                this.remote = None;
                                this.resuming = false;
                                this.no_screen = true;
                                this.state = State::Ended(why);
                            }
                            Msg::Ended(why, may_return) => {
                                this.remote = None;
                                // The connection dropped (another network, a
                                // moment's hiccup): pick the stream back up.
                                if may_return && this.offline.is_none() && this.retries < RETRIES {
                                    this.retries += 1;
                                    this.resuming = true;
                                    this.state = State::Connecting;
                                    let delay = Duration::from_millis(500 * this.retries as u64).min(Duration::from_secs(3));
                                    this.retry = Some(cx.spawn_in(window, async move |this, cx| {
                                        cx.background_executor().timer(delay).await;
                                        let _ = this.update_in(cx, |this, window, cx| {
                                            if this.resuming && this.offline.is_none() {
                                                this.start(window, cx);
                                            }
                                        });
                                    }));
                                } else {
                                    this.resuming = false;
                                    this.state = State::Ended(why);
                                }
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

    /// While the whole desktop shows, a label over each monitor's corner that
    /// opens that monitor (clicks elsewhere still go to the machine).
    fn monitor_labels(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.display.as_deref() != Some("all") || self.frame.is_none() {
            return Vec::new();
        }
        let displays = self.engine.current().machines.iter().find(|m| m.id == self.machine).map(|m| m.displays.clone()).unwrap_or_default();
        let Some(all) = displays.iter().find(|d| d.id == "all") else { return Vec::new() };
        let (Some(rect), Some(area)) = (self.shown.get(), self.area.get()) else { return Vec::new() };
        if all.width <= 0. {
            return Vec::new();
        }
        let theme = cx.theme();
        let scale = f32::from(rect.size.width) / all.width as f32;
        let (ox, oy) = (f32::from(rect.origin.x - area.origin.x), f32::from(rect.origin.y - area.origin.y));
        displays
            .iter()
            .filter(|d| d.id != "all")
            .map(|d| {
                let id = d.id.clone();
                div()
                    .id(SharedString::from(format!("mon-label-{}", d.id)))
                    .absolute()
                    .left(px(ox + d.x as f32 * scale + 8.))
                    .top(px(oy + d.y as f32 * scale + 8.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(24.))
                    .px(px(9.))
                    .rounded(crate::ui::kit::corner(7.))
                    .bg(gpui::black().opacity(0.62))
                    .border_1()
                    .border_color(gpui::white().opacity(0.18))
                    .text_size(px(12.))
                    .text_color(gpui::white())
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.accent))
                    .when(d.is_virtual, |el| el.child(kit::dot(theme.accent, 6.)))
                    .child(d.name.clone())
                    .child(kit::icon("external-link", 11., gpui::white().opacity(0.8)))
                    // The click is for the label, not the machine.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| this.set_display(Some(id.clone()), window, cx)))
                    .into_any_element()
            })
            .collect()
    }

    /// "Live", "Connecting…" or why it ended, for the page header.
    pub fn status(&self) -> String {
        if let (Some(why), false) = (&self.offline, matches!(self.state, State::Live)) {
            return why.clone();
        }
        if self.changing {
            return "Changing the monitor…".into();
        }
        if let Some(e) = &self.change_error {
            return e.clone();
        }
        match &self.state {
            State::Connecting if self.resuming => "Reconnecting…".into(),
            State::Connecting => "Connecting…".into(),
            State::Live if self.frame.is_none() => "Waiting for the first frame…".into(),
            State::Live => match &self.note {
                Some(n) => format!("Live · {}×{} · {n}", self.size.0, self.size.1),
                None => format!("Live · {}×{}", self.size.0, self.size.1),
            },
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
        if let Some(r) = self.remote.as_ref().filter(|_| self.can_control) {
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
        if !self.can_control {
            return;
        }
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
        self.buttons_down.retain(|x| *x != b);
        if down {
            self.buttons_down.push(b);
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
        // On a Mac ⌘ is ⌘; elsewhere it works as Ctrl (copy, paste, ... as on a Mac).
        let keys: [(bool, bool, u32); 4] = if self.mac {
            [(m.shift, self.held.shift, 0xffe1), (m.control, self.held.control, 0xffe3), (m.alt, self.held.alt, 0xffe9), (m.platform, self.held.platform, 0xffeb)]
        } else {
            let ctrl = |m: &Modifiers| m.control || m.platform;
            [(m.shift, self.held.shift, 0xffe1), (ctrl(&m), ctrl(&self.held), 0xffe3), (m.alt, self.held.alt, 0xffe9), (false, false, 0)]
        };
        for (now, before, sym) in keys {
            if now != before && sym != 0 {
                self.send(json!({ "t": "key", "keysym": sym, "down": now }));
            }
        }
        self.held = m;
    }

    fn key(&mut self, key: &str, down: bool, cx: &mut Context<Self>) {
        if !self.can_control {
            return;
        }
        if let Some(sym) = keysym(key) {
            self.send(json!({ "t": "key", "keysym": sym, "down": down }));
            self.keys_down.retain(|x| *x != sym);
            if down {
                self.keys_down.push(sym);
            }
            cx.stop_propagation();
        }
    }

    /// Lets go of every key, modifier and button the machine was told is down.
    fn let_go(&mut self) {
        for sym in std::mem::take(&mut self.keys_down) {
            self.send(json!({ "t": "key", "keysym": sym, "down": false }));
        }
        self.modifiers(Modifiers::default());
        for b in std::mem::take(&mut self.buttons_down) {
            self.send(json!({ "t": "button", "b": b, "down": false, "x": 0, "y": 0 }));
        }
    }
}

/// A picture `m` times as big, each pixel an m×m block (see `DesktopView::crisp`).
fn enlarge(picture: image::RgbaImage, m: u32) -> image::RgbaImage {
    let (w, h) = picture.dimensions();
    let src = picture.as_raw();
    let mut out = Vec::with_capacity((w * h * m * m * 4) as usize);
    let mut row = Vec::with_capacity((w * m * 4) as usize);
    for y in 0..h as usize {
        row.clear();
        for px in src[y * w as usize * 4..(y + 1) * w as usize * 4].as_chunks::<4>().0 {
            for _ in 0..m {
                row.extend_from_slice(px);
            }
        }
        for _ in 0..m {
            out.extend_from_slice(&row);
        }
    }
    image::RgbaImage::from_raw(w * m, h * m, out).unwrap_or(picture)
}

/// The pointer for a shape an app's window has there (see agent/src/desktop.rs).
fn pointer_style(kind: &str) -> gpui::CursorStyle {
    use gpui::CursorStyle::*;
    match kind {
        "text" => IBeam,
        "hand" => PointingHand,
        "ew" => ResizeLeftRight,
        "ns" => ResizeUpDown,
        "nwse" => ResizeUpLeftDownRight,
        "nesw" => ResizeUpRightDownLeft,
        "move" => ClosedHand,
        "no" => OperationNotAllowed,
        "crosshair" => Crosshair,
        _ => Arrow,
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
        let area = self.area.clone();
        let picture = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                area.set(Some(bounds));
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
                    .child(kit::color_tile("power", 44.))
                    .child(div().text_size(px(14.)).text_color(theme.text).child(why))
                    .child(div().text_size(px(12.5)).text_color(theme.text_muted).child("Comes back when it does.")),
            ),
            (State::Ended(why), None) => Some(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .child(kit::color_tile("monitor", 44.))
                    .child(div().max_w(px(460.)).text_center().text_size(px(13.)).text_color(theme.text_muted).child(why.clone()))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            // Nobody's screen: the machine's own remote desktop can sign someone in.
                            .when(self.no_screen, |d| {
                                d.child(
                                    Button::new("desktop-remote", crate::fleet::remote_desktop::open_label(&self.platform))
                                        .icon("external-link")
                                        .style(ButtonStyle::Primary)
                                        .disabled(self.opening)
                                        .on_click(cx.listener(|this, _, _, cx| this.open_remote(cx))),
                                )
                            })
                            .child(Button::new("desktop-retry", "Try again").icon("refresh-cw").on_click(cx.listener(|this, _, window, cx| {
                                this.retries = 0;
                                this.start(window, cx);
                                cx.notify();
                            })))
                            // An app's window has no title bar of its own until it shows.
                            .when(self.shows_window(), |d| d.child(Button::new("desktop-close-app", "Close").icon("x").on_click(|_, window, _| window.remove_window()))),
                    )
                    .children(self.opened.clone().map(|(text, failed)| {
                        div().max_w(px(460.)).text_center().text_size(px(12.5)).text_color(if failed { theme.error } else { theme.text_muted }).child(text)
                    })),
            ),
            (State::Connecting, None) if self.resuming && self.frame.is_some() => Some(
                div().flex().items_center().gap(px(10.)).child(spinner("desktop-resume", 16., theme.text_muted)).child(
                    div().text_size(px(13.)).text_color(theme.text_muted).child("Reconnecting…"),
                ),
            ),
            _ if self.frame.is_none() => Some(
                div().flex().items_center().gap(px(10.)).child(spinner("desktop-wait", 16., theme.text_muted)).child(
                    div().text_size(px(13.)).text_color(theme.text_muted).child(self.status()),
                ),
            ),
            _ => None,
        };
        // Showing every screen: each monitor's name over its corner opens it.
        let labels = self.monitor_labels(cx);
        let hint = self.hint.filter(|until| Instant::now() < *until && self.frame.is_some()).map(|_| {
            let keys = if cfg!(target_os = "macos") { "⌘W" } else { "Ctrl+Shift+W" };
            div().absolute().bottom(px(18.)).left_0().right_0().flex().justify_center().child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(30.))
                    .px(px(14.))
                    .rounded(crate::ui::kit::corner(15.))
                    .bg(gpui::black().opacity(0.72))
                    .border_1()
                    .border_color(gpui::white().opacity(0.14))
                    .text_size(px(12.5))
                    .text_color(gpui::white())
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(keys))
                    .child(div().text_color(gpui::white().opacity(0.8)).child("closes this window")),
            )
        });
        div()
            .id("desktop")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .bg(gpui::black())
            // An app's window: the pointer shaped as it is there.
            .cursor(if self.shows_window() { self.pointer } else { gpui::CursorStyle::Arrow })
            .child(picture)
            .children(labels)
            .children(hint)
            // Over the last picture, dimmed: it's no longer live.
            .children(waiting.map(|w| div().absolute().inset_0().flex().items_center().justify_center().when(self.frame.is_some(), |d| d.bg(gpui::black().opacity(0.72))).child(w)))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, w, cx| {
                    // A press on the app's own title bar drags this window, as
                    // a title bar here would (a double click zooms it).
                    this.moving = this.drags_window(e.position);
                    if this.moving {
                        super::title_bar_press(e, w);
                        return;
                    }
                    this.mouse_button(e.button, e.position, true, w, cx)
                }),
            )
            .on_mouse_down(MouseButton::Right, cx.listener(|this, e: &MouseDownEvent, w, cx| this.mouse_button(e.button, e.position, true, w, cx)))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, e: &MouseDownEvent, w, cx| this.mouse_button(e.button, e.position, true, w, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, e: &MouseUpEvent, w, cx| {
                    if !std::mem::take(&mut this.moving) {
                        this.mouse_button(e.button, e.position, false, w, cx)
                    }
                }),
            )
            .on_mouse_up(MouseButton::Right, cx.listener(|this, e: &MouseUpEvent, w, cx| this.mouse_button(e.button, e.position, false, w, cx)))
            .on_mouse_up(MouseButton::Middle, cx.listener(|this, e: &MouseUpEvent, w, cx| this.mouse_button(e.button, e.position, false, w, cx)))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_modifiers_changed(cx.listener(|this, e: &ModifiersChangedEvent, _, _| this.modifiers(e.modifiers)))
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                // ⌘W closes an app's window here, as any window here (Ctrl+Shift+W
                // off a Mac: Ctrl+W is the app's); the app goes on there.
                let m = e.keystroke.modifiers;
                let close = if cfg!(target_os = "macos") { m.platform && !m.shift } else { m.control && m.shift };
                if this.shows_window() && close && e.keystroke.key.eq_ignore_ascii_case("w") {
                    cx.stop_propagation();
                    // The machine was told those keys are down: not left so.
                    this.let_go();
                    window.remove_window();
                    return;
                }
                this.modifiers(e.keystroke.modifiers);
                this.key(&e.keystroke.key, true, cx)
            }))
            .on_key_up(cx.listener(|this, e: &KeyUpEvent, _, cx| this.key(&e.keystroke.key, false, cx)))
    }
}

/// Opens a display's menu: (machine, display, where), from a right-click on it.
pub type DisplayMenu = Rc<dyn Fn(&str, &str, gpui::Point<Pixels>, &mut Window, &mut App)>;

/// Status, network and resolution for a desktop, then `extra` controls (close,
/// pop out, full screen): the bar above every desktop view.
pub fn toolbar(view: &Entity<DesktopView>, extra: Vec<AnyElement>, cx: &App) -> AnyElement {
    toolbar_with_menu(view, extra, None, cx)
}

/// The toolbar, with `menu` opening a monitor's menu on right-click.
pub fn toolbar_with_menu(view: &Entity<DesktopView>, extra: Vec<AnyElement>, menu: Option<DisplayMenu>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (status, via, machine, shown, engine) = {
        let v = view.read(cx);
        (v.status(), v.via().to_string(), v.machine.clone(), v.display.clone(), v.engine.clone())
    };
    // Its monitors, when it has more than one: pick the one to show (none
    // for an app's window).
    let app_window = shown.as_deref().is_some_and(|d| d.starts_with("window:"));
    let displays = match app_window {
        true => Vec::new(),
        false => engine.current().machines.iter().find(|m| m.id == machine).map(|m| m.displays.clone()).unwrap_or_default(),
    };
    // Shown unless another is picked: the main monitor.
    let default = displays.iter().find(|d| d.main && d.id != "all").or_else(|| displays.iter().find(|d| d.id != "all")).map(|d| d.id.clone());
    let showing = shown.or(default);
    let monitors = (displays.len() > 1).then(|| {
        div().flex().gap(px(2.)).p(px(2.)).rounded(crate::ui::kit::corner(7.)).bg(theme.element).children(displays.iter().map(|d| {
            let (on, id, view) = (showing.as_deref() == Some(d.id.as_str()), d.id.clone(), view.clone());
            let right_click = menu.clone().filter(|_| d.id != "all").map(|menu| {
                let (machine, display) = (machine.clone(), d.id.clone());
                move |e: &MouseDownEvent, window: &mut Window, cx: &mut App| menu(&machine, &display, e.position, window, cx)
            });
            let name = if d.name.chars().count() > 18 { format!("{}…", d.name.chars().take(17).collect::<String>()) } else { d.name.clone() };
            div()
                .id(SharedString::from(format!("disp-{}", d.id)))
                .px(px(8.))
                .h(px(22.))
                .flex()
                .items_center()
                .gap(px(5.))
                .rounded(crate::ui::kit::corner(5.))
                .cursor_pointer()
                .text_size(px(11.5))
                .text_color(if on { theme.text } else { theme.text_placeholder })
                .when(on, |d| d.bg(theme.element_selected))
                .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                .when(d.is_virtual, |el| el.child(kit::dot(theme.accent, 5.)).tooltip(|window, cx| kit::Tooltip::view("Virtual monitor".into(), window, cx)))
                .child(name)
                .when_some(right_click, |el, f| el.on_mouse_down(MouseButton::Right, f))
                .on_click(move |_, window, cx| view.update(cx, |v, cx| v.set_display(Some(id.clone()), window, cx)))
        }))
    });
    // The monitor shown: its size (720p · 1080p · 1440p · 4K, the ones it
    // has) and scale (100% · 200%), set on the machine.
    let screen = showing.as_deref().filter(|id| *id != "all").and_then(|id| displays.iter().find(|d| d.id == id));
    let chip = |id: String, label: String, on: bool, tip: &'static str| {
        div()
            .id(SharedString::from(id))
            .px(px(8.))
            .h(px(22.))
            .flex()
            .items_center()
            .rounded(crate::ui::kit::corner(5.))
            .cursor_pointer()
            .text_size(px(11.5))
            .text_color(if on { theme.text } else { theme.text_placeholder })
            .when(on, |d| d.bg(theme.element_selected))
            .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .tooltip(move |window, cx| kit::Tooltip::view(tip.into(), window, cx))
            .child(label)
    };
    let group = |chips: Vec<gpui::Stateful<gpui::Div>>| div().flex_none().flex().gap(px(2.)).p(px(2.)).rounded(crate::ui::kit::corner(7.)).bg(theme.element).children(chips);
    let sizes = screen.and_then(|d| {
        let current = (d.width.round() as u32, d.height.round() as u32);
        let mut choices: Vec<((u32, u32), String)> = SIZES.iter().filter(|(size, _)| d.any_size || d.modes.contains(size)).map(|(size, label)| (*size, label.to_string())).collect();
        if !choices.iter().any(|(size, _)| *size == current) {
            choices.push((current, format!("{}×{}", current.0, current.1)));
            choices.sort_by_key(|((w, h), _)| w * h);
        }
        (choices.len() > 1).then(|| {
            group(
                choices
                    .into_iter()
                    .map(|(size, label)| {
                        let (view, id) = (view.clone(), d.id.clone());
                        chip(format!("size-{}x{}", size.0, size.1), label, size == current, "The monitor's resolution")
                            .on_click(move |_, window, cx| view.update(cx, |v, cx| v.change_display(id.clone(), Some(size), None, window, cx)))
                    })
                    .collect(),
            )
        })
    });
    let scales = screen.filter(|d| [1., 2.].iter().all(|s| d.scales.iter().any(|x| (x - s).abs() < 0.01))).map(|d| {
        group(
            [(1., "100%"), (2., "200%")]
                .into_iter()
                .map(|(scale, label)| {
                    let (view, id) = (view.clone(), d.id.clone());
                    chip(format!("scale-{label}"), label.to_string(), (d.scale - scale).abs() < 0.01, "How big things are (200%: twice as big, just as sharp)")
                        .on_click(move |_, window, cx| view.update(cx, |v, cx| v.change_display(id.clone(), None, Some(scale), window, cx)))
                })
                .collect(),
        )
    });
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
                .child(kit::color_icon(&theme, "monitor", 14., theme.text_muted))
                .child(div().flex_none().text_size(px(12.5)).text_color(theme.text_muted).child(status))
                .when(!via.is_empty(), |d| {
                    d.child(div().flex_none().flex().items_center().gap(px(5.)).text_size(px(12.)).text_color(theme.text_placeholder).child(kit::color_icon(&theme, "network", 12., theme.text_placeholder)).child(via))
                }),
        )
        .children(monitors.map(|m| m.flex_none()))
        .children(sizes)
        .children(scales)
        .children(extra)
        .into_any_element()
}

/// A desktop in a window of its own (to put on another screen, or full
/// screen), or an app's window from a machine, as a window here.
pub struct DesktopWindow {
    view: Entity<DesktopView>,
    /// An app's window: nothing around it but its own title bar and frame.
    app: bool,
    /// An app's window: this window took its size once it came, and the
    /// size it last matched (points).
    sized: bool,
    shown: Option<(f32, f32)>,
    /// Whether macOS's own buttons are hidden (the app's title bar is showing).
    buttons_hidden: bool,
    /// Until when this window's size changes are its own doing (sizing it to
    /// the app, or macOS fitting that on the screen), not someone resizing it:
    /// those don't resize the app there.
    settling: Option<std::time::Instant>,
    /// Its size last seen: a change of place alone (a move) isn't a resize.
    last_size: Option<gpui::Size<Pixels>>,
    /// Resizing the app's window there to this one's new size, a moment after
    /// the resizing here stops.
    fit: Option<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl Render for DesktopWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.app {
            return div().relative().size_full().bg(gpui::black()).child(self.view.clone()).children(super::frame::resize_edges(window)).into_any_element();
        }
        let full = window.is_fullscreen();
        let button = Button::new("desktop-fullscreen", if full { "Exit full screen" } else { "Full screen" })
            .style(ButtonStyle::Ghost)
            .icon(if full { "minimize" } else { "maximize" })
            .on_click(|_, window, _| window.toggle_fullscreen())
            .into_any_element();
        let mut extra = vec![button];
        extra.extend(super::frame::window_buttons(window, cx));
        // When the app draws the frame (Linux on Wayland), the toolbar is the
        // title bar: it moves the window and opens the window menu.
        let title_bar = super::frame::ours(window);
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().canvas)
            .when(super::frame::bordered(window), |d| d.border_1().border_color(cx.theme().border))
            // Room for the traffic lights on macOS.
            .when(cfg!(target_os = "macos") && !full, |d| d.pt(px(28.)))
            .child(
                div()
                    .when(title_bar, |d| {
                        d.on_mouse_down(MouseButton::Left, |e: &MouseDownEvent, window, _| super::title_bar_press(e, window))
                            .on_mouse_down(MouseButton::Right, |e: &MouseDownEvent, window, _| super::frame::title_bar_menu(e, window))
                    })
                    .child(toolbar(&self.view, extra, cx)),
            )
            .child(div().flex_1().min_h(px(0.)).child(self.view.clone()))
            .children(super::frame::resize_edges(window))
            .into_any_element()
    }
}

/// Opens a machine's desktop in its own window.
pub fn open_window(engine: Arc<Engine>, machine_id: String, display: Option<String>, name: String, cx: &mut App) {
    let bounds = Bounds::centered(None, gpui::size(px(1440.), px(900.)), cx);
    let title = format!("{name} · Desktop");
    let options = gpui::WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions { title: Some(title.clone().into()), appears_transparent: cfg!(target_os = "macos"), traffic_light_position: None }),
        window_min_size: Some(gpui::size(px(480.), px(320.))),
        app_id: Some("muxin".into()),
        window_decorations: super::frame::requested(),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        // (Wayland gets no title from the options.)
        window.set_window_title(&title);
        super::theme::apply(window, cx);
        let view = cx.new(|cx| DesktopView::showing(engine, machine_id, display, window, cx));
        window.focus(&view.focus_handle(cx));
        cx.new(|_| DesktopWindow { view, app: false, sized: false, shown: None, buttons_hidden: false, settling: None, last_size: None, fit: None, _subscriptions: Vec::new() })
    });
    if let Err(e) = opened {
        tracing::warn!("couldn't open the desktop window: {e:#}");
    }
}

/// Hides macOS's close, minimize and zoom buttons (the app's own title bar
/// has its own), or shows them again (while it isn't showing: something to
/// close the window with).
#[allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare
fn hide_window_buttons(window: &Window, hidden: bool) {
    #[cfg(target_os = "macos")]
    unsafe {
        use objc::runtime::{NO, Object, YES};
        use objc::{msg_send, sel, sel_impl};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
        let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
        let view = h.ns_view.as_ptr() as *mut Object;
        let ns_window: *mut Object = msg_send![view, window];
        if ns_window.is_null() {
            return;
        }
        // NSWindowCloseButton, NSWindowMiniaturizeButton, NSWindowZoomButton.
        for kind in 0u64..3 {
            let button: *mut Object = msg_send![ns_window, standardWindowButton: kind];
            if !button.is_null() {
                let _: () = msg_send![button, setHidden: if hidden { YES } else { NO }];
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (window, hidden);
}

/// `size` (points) made to fit this window's screen, keeping its shape: an
/// app's window bigger than the screen here shows smaller, rather than cut off
/// with bars beside it. (The screen less room for the menu bar and Dock.)
fn fit_on_screen(window: &Window, cx: &App, (w, h): (f32, f32)) -> (f32, f32) {
    let Some(screen) = window.display(cx).map(|d| d.bounds()) else { return (w.round(), h.round()) };
    let (room_w, room_h) = (f32::from(screen.size.width) * 0.95, f32::from(screen.size.height) - 90.);
    let k = (room_w / w.max(1.)).min(room_h / h.max(1.)).min(1.);
    ((w * k).round(), (h * k).round())
}

/// Opens an app's window from a machine as a window of its own here, as big
/// as it is there, with nothing around it but its own title bar: dragging
/// that moves this window, and resizing either one resizes the other.
pub fn open_app_window(engine: Arc<Engine>, machine_id: String, app: crate::fleet::desktop::AppWindowInfo, machine_name: String, cx: &mut App) {
    // Its size there (until its stream says how much Windows scales it up).
    let guess = gpui::size(px((app.width as f32).clamp(480., 1600.)), px((app.height as f32).clamp(320., 1000.)));
    let bounds = Bounds::centered(None, guess, cx);
    let title = format!("{} · {machine_name}", app.title);
    let options = gpui::WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions { title: Some(title.clone().into()), appears_transparent: cfg!(target_os = "macos"), traffic_light_position: None }),
        window_min_size: Some(gpui::size(px(320.), px(240.))),
        app_id: Some("muxin".into()),
        window_decorations: super::frame::requested(),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        window.set_window_title(&title);
        super::theme::apply(window, cx);
        let view = cx.new(|cx| DesktopView::showing(engine, machine_id, Some(app.display()), window, cx));
        window.focus(&view.focus_handle(cx));
        cx.new(|cx| {
            // Once its stream says how big it is and what it's called now: that,
            // here. Later, when it changes size there by itself (maximized, or
            // kept from getting as small as asked): the same here.
            let started = cx.observe_in(&view, window, move |this: &mut DesktopWindow, view, window, cx| {
                let v = view.read(cx);
                // Closed there: closed here.
                if v.closed() {
                    window.remove_window();
                    return;
                }
                // macOS's own buttons only while the app isn't showing (connecting, failed).
                if this.buttons_hidden != v.live() {
                    this.buttons_hidden = v.live();
                    hide_window_buttons(window, this.buttons_hidden);
                }
                let (Some((w, h)), Some(app)) = (v.app_size(), v.app()) else { return };
                let near = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 4. && (a.1 - b.1).abs() < 4.;
                let follow = match (this.sized, this.shown) {
                    (false, _) => {
                        if !app.title.is_empty() {
                            window.set_window_title(&format!("{} · {machine_name}", app.title));
                        }
                        true
                    }
                    (true, Some(shown)) => !near(shown, (w, h)) && !v.asked().is_some_and(|a| near(a, (w, h))) && !window.is_fullscreen(),
                    (true, None) => false,
                };
                (this.sized, this.shown) = (true, Some((w, h)));
                if follow {
                    this.settling = Some(std::time::Instant::now() + Duration::from_millis(800));
                    let (w, h) = fit_on_screen(window, cx, (w, h));
                    window.resize(gpui::size(px(w), px(h)));
                }
            });
            // Resized here: the same there, once the resizing stops.
            let resized = cx.observe_window_bounds(window, |this: &mut DesktopWindow, window, cx| {
                let size = window.bounds().size;
                let moved = this.last_size.replace(size) == Some(size);
                if moved || !this.sized || window.is_fullscreen() || this.settling.is_some_and(|t| std::time::Instant::now() < t) {
                    return;
                }
                let view = this.view.clone();
                this.fit = Some(cx.spawn_in(window, async move |_, cx| {
                    cx.background_executor().timer(Duration::from_millis(250)).await;
                    let _ = view.update(cx, |v, _| v.fit_app());
                }));
            });
            DesktopWindow { view, app: true, sized: false, shown: None, buttons_hidden: false, settling: None, last_size: None, fit: None, _subscriptions: vec![started, resized] }
        })
    });
    if let Err(e) = opened {
        tracing::warn!("couldn't open the app's window: {e:#}");
    }
}
