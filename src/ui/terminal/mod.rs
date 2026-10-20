//! An embedded terminal. alacritty_terminal does the emulation (and the PTY);
//! GPUI draws the grid. Agent sessions show here either over the machine's
//! live link (`remote`: bytes in and out, no local process) or by running
//! `ssh -tt <host> muxen-agent attach …` in a local PTY.

mod boxdraw;
mod colors;
mod ime;
mod keys;
mod render;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::Processor;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    App, ClipboardItem, Context, CursorStyle, EventEmitter, actions, FocusHandle, Focusable, KeyBinding, KeyDownEvent, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NoAction, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent,
    Task, Window, div, prelude::*,
};

use super::theme::{self as theme_mod, ActiveTheme, Appearance};
use colors::Palette;

/// Font size and line height, from the user's settings (config.toml).
#[derive(Clone, Copy)]
pub struct Metrics {
    pub size: f32,
    pub line_height: f32,
    pub blink: bool,
}

impl Metrics {
    pub fn current(cx: &App) -> Metrics {
        let a = cx.global::<Appearance>();
        Metrics { size: a.terminal_font_size, line_height: a.terminal_line_height, blink: a.terminal_cursor_blink }
    }
}

/// How long the cursor stays on, then off, while it blinks; and how long it
/// stays on after a key press (so it's there while you type).
const BLINK: Duration = Duration::from_millis(530);
const BLINK_REST: Duration = Duration::from_millis(600);

actions!(terminal, [IncreaseFontSize, DecreaseFontSize, ResetFontSize]);
const PAD_X: f32 = 12.;
const PAD_Y: f32 = 8.;
const CONTEXT: &str = "Terminal";

/// Keys the app binds without ⌘ that a terminal program needs, released in
/// terminals. On Linux and Windows the app's shortcuts use Ctrl, so those go
/// to the program too (the app's menu and ⌘ on macOS still work).
pub fn bind_keys(cx: &mut App) {
    let mut keys: Vec<String> = vec!["escape".into(), "/".into()];
    if !cfg!(target_os = "macos") {
        keys.extend(super::shortcuts::ctrl_keys(cx));
        keys.push("ctrl-q".into());
    }
    cx.bind_keys(keys.iter().map(|k| KeyBinding::new(k, NoAction, Some(CONTEXT))));
    cx.bind_keys([
        KeyBinding::new("secondary-=", IncreaseFontSize, Some(CONTEXT)),
        KeyBinding::new("secondary-+", IncreaseFontSize, Some(CONTEXT)),
        KeyBinding::new("secondary--", DecreaseFontSize, Some(CONTEXT)),
        KeyBinding::new("secondary-0", ResetFontSize, Some(CONTEXT)),
    ]);
}

/// The terminal font in use.
pub fn font_family(window: &Window) -> gpui::SharedString {
    render::family(window)
}

pub use render::{DEFAULT_FONT, monospace_families, set_font};

/// Hands terminal events from the PTY thread to the view.
#[derive(Clone)]
pub struct Listener(UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

#[derive(Clone, Copy)]
struct TermDims {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Where the grid was last drawn, for mapping the mouse to cells.
#[derive(Clone, Copy, Default)]
struct Geometry {
    origin: Point<Pixels>,
    cell_w: f32,
    line_h: f32,
    cols: usize,
    rows: usize,
}

/// Where a remote terminal's keys and size go (a session over the machine's link).
pub trait RemoteIo: Send + Sync {
    fn write(&self, bytes: &[u8]);
    fn resize(&self, cols: u16, rows: u16);
    /// Detach; the session keeps running.
    fn close(&self);
}

/// What arrives for a remote terminal.
pub enum RemoteOutput {
    Bytes(Vec<u8>),
    /// Draw at this size (cols, rows): someone else's session, shown as they size it.
    Resize(u16, u16),
    /// The session ended (with its code), or the connection did (None).
    Ended(Option<i32>),
}

/// What's behind the screen.
enum Backend {
    /// A local program in a PTY.
    Pty(EventLoopSender),
    Remote(Arc<dyn RemoteIo>),
    /// A replayed (demo) screen.
    None,
}

/// State shared between the view and the canvas that paints it.
struct Shared {
    term: Arc<FairMutex<Term<Listener>>>,
    backend: Backend,
    geometry: Mutex<Geometry>,
    /// A size set from elsewhere (a shared session's), instead of the space it has.
    fixed: Mutex<Option<(usize, usize)>>,
}

impl Shared {
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        match &self.backend {
            Backend::Pty(tx) => {
                let _ = tx.send(Msg::Input(bytes.into()));
            }
            Backend::Remote(io) => io.write(&bytes.into()),
            Backend::None => {}
        }
    }

    /// Draws at a size set from elsewhere from now on.
    fn fix_size(&self, cols: usize, rows: usize) {
        *self.fixed.lock().unwrap() = Some((cols, rows));
        let mut term = self.term.lock();
        if term.columns() != cols || term.screen_lines() != rows {
            term.resize(TermDims { cols, rows });
        }
    }

    /// Resizes the grid and tells the PTY, when the visible area changed.
    fn resize(&self, cols: usize, rows: usize, cell_w: f32, line_h: f32) {
        let mut term = self.term.lock();
        if term.columns() == cols && term.screen_lines() == rows {
            return;
        }
        term.resize(TermDims { cols, rows });
        drop(term);
        match &self.backend {
            Backend::Pty(tx) => {
                let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: cell_w as u16, cell_height: line_h as u16 };
                let _ = tx.send(Msg::Resize(size));
            }
            Backend::Remote(io) => io.resize(cols as u16, rows as u16),
            Backend::None => {}
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        match &self.backend {
            Backend::Pty(tx) => {
                let _ = tx.send(Msg::Shutdown);
            }
            Backend::Remote(io) => io.close(),
            Backend::None => {}
        }
    }
}

pub enum TerminalEvent {
    /// The program exited (for sessions: detached, or the session ended), with its code when known.
    Exited(Option<i32>),
}

pub struct TerminalView {
    shared: Arc<Shared>,
    focus: FocusHandle,
    exited: bool,
    /// Someone else's session, joined through a share: what this terminal
    /// would answer (queries) or take (a program's copy) stays here.
    guest: bool,
    /// Dragging a local text selection.
    selecting: bool,
    /// The button held in mouse-reporting mode, and the last cell reported.
    pressed: Option<(u8, (usize, usize))>,
    scroll_rest: f32,
    /// The blinking cursor's phase, and the last key press (which shows it).
    cursor_on: bool,
    typed_at: Instant,
    ime: ime::Composition,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn new_term(cols: usize, rows: usize) -> (Arc<FairMutex<Term<Listener>>>, Listener, futures::channel::mpsc::UnboundedReceiver<TermEvent>) {
    let (tx, rx) = unbounded();
    let listener = Listener(tx);
    let config = Config { scrolling_history: 10_000, ..Config::default() };
    let term = Term::new(config, &TermDims { cols, rows }, listener.clone());
    (Arc::new(FairMutex::new(term)), listener, rx)
}

impl TerminalView {
    /// Runs `program` in a new PTY.
    pub fn spawn(program: String, args: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Result<Self, String> {
        let (cols, rows) = (100, 30);
        let (term, listener, rx) = new_term(cols, rows);
        let env = HashMap::from([("TERM".to_string(), "xterm-256color".to_string()), ("COLORTERM".to_string(), "truecolor".to_string())]);
        // `..Default::default()` fills Windows-only fields.
        #[allow(clippy::needless_update)]
        let options = tty::Options { shell: Some(tty::Shell::new(program, args)), working_directory: None, drain_on_exit: true, env, ..Default::default() };
        let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: 8, cell_height: 17 };
        let pty = tty::new(&options, size, 0).map_err(|e| format!("couldn't open a terminal: {e}"))?;
        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false).map_err(|e| format!("couldn't start the terminal: {e}"))?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Self::with_term(term, Backend::Pty(sender), rx, window, cx))
    }

    /// A terminal fed from elsewhere (a session over the machine's link):
    /// `output` brings the screen, `io` takes keys and size.
    pub fn remote(
        io: Arc<dyn RemoteIo>,
        mut output: futures::channel::mpsc::UnboundedReceiver<RemoteOutput>,
        (cols, rows): (u16, u16),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (term, _listener, rx) = new_term(cols as usize, rows as usize);
        let mut view = Self::with_term(term.clone(), Backend::Remote(io), rx, window, cx);
        let feed = cx.spawn(async move |this, cx| {
            let mut parser: Processor = Processor::new();
            while let Some(first) = output.next().await {
                // Take everything queued, then draw once.
                let mut batch = vec![first];
                while let Ok(more) = output.try_recv() {
                    batch.push(more);
                }
                let mut ended = None;
                let mut size = None;
                {
                    let mut t = term.lock();
                    for item in batch {
                        match item {
                            RemoteOutput::Bytes(b) => parser.advance(&mut *t, &b),
                            RemoteOutput::Ended(code) => ended = Some(code),
                            RemoteOutput::Resize(cols, rows) => {
                                let dims = TermDims { cols: (cols as usize).clamp(2, 1000), rows: (rows as usize).clamp(1, 500) };
                                t.resize(dims);
                                size = Some((dims.cols, dims.rows));
                            }
                        }
                    }
                }
                if let Some((cols, rows)) = size {
                    let _ = this.update(cx, |this, _| this.shared.fix_size(cols, rows));
                }
                let alive = this.update(cx, |this, cx| match ended {
                    Some(code) => this.finish(code, cx),
                    None => cx.notify(),
                });
                if alive.is_err() || ended.is_some() {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| this.finish(None, cx));
        });
        view._tasks.push(feed);
        view
    }

    /// Someone else's session, joined through a share: drawn at their size
    /// (`Resize` updates), keys to `io` (which drops them unless the host
    /// lets this guest type).
    pub fn guest(io: Arc<dyn RemoteIo>, output: futures::channel::mpsc::UnboundedReceiver<RemoteOutput>, (cols, rows): (u16, u16), window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self::remote(io, output, (cols, rows), window, cx);
        view.guest = true;
        view.shared.fix_size(cols as usize, rows as usize);
        view
    }

    /// A terminal showing fixed output (the demo), with no program behind it.
    pub fn replay(bytes: &[u8], window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (term, _listener, rx) = new_term(110, 34);
        {
            let mut t = term.lock();
            let mut parser: Processor = Processor::new();
            parser.advance(&mut *t, bytes);
        }
        Self::with_term(term, Backend::None, rx, window, cx)
    }

    fn with_term(
        term: Arc<FairMutex<Term<Listener>>>,
        backend: Backend,
        mut rx: futures::channel::mpsc::UnboundedReceiver<TermEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let events = cx.spawn_in(window, async move |this, cx| {
            while let Some(first) = rx.next().await {
                // Output arrives in bursts; handle everything queued in one update.
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let alive = this.update_in(cx, |this: &mut Self, window, cx| {
                    for event in batch {
                        this.on_term_event(event, window, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let subscriptions = vec![
            cx.on_focus_in(&focus, window, |this, _, cx| this.report_focus(true, cx)),
            cx.on_focus_out(&focus, window, |this, _, _, cx| this.report_focus(false, cx)),
        ];
        // Blinks while focused in the active window; steady otherwise.
        let blink = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                let alive = this.update_in(cx, |this: &mut Self, window, cx| {
                    let blinking = Metrics::current(cx).blink && window.is_window_active() && this.focus.is_focused(window);
                    let on = !blinking || this.typed_at.elapsed() < BLINK_REST || !this.cursor_on;
                    if on != this.cursor_on {
                        this.cursor_on = on;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        TerminalView {
            shared: Arc::new(Shared { term, backend, geometry: Mutex::new(Geometry::default()), fixed: Mutex::new(None) }),
            focus,
            exited: false,
            guest: false,
            selecting: false,
            pressed: None,
            scroll_rest: 0.,
            cursor_on: true,
            typed_at: Instant::now(),
            ime: ime::Composition::default(),
            _tasks: vec![events, blink],
            _subscriptions: subscriptions,
        }
    }

    pub fn exited(&self) -> bool {
        self.exited
    }

    /// Columns and rows that fit `width`×`height` pixels, for sizing a new session.
    pub fn fit(width: f32, height: f32, window: &Window, cx: &App) -> (u16, u16) {
        let (w, h) = render::cell_size(window, Metrics::current(cx));
        (((width - 2. * PAD_X) / w).floor().clamp(40., 500.) as u16, ((height - 2. * PAD_Y) / h).floor().clamp(10., 200.) as u16)
    }

    fn mode(&self) -> TermMode {
        *self.shared.term.lock().mode()
    }

    fn on_term_event(&mut self, event: TermEvent, window: &Window, cx: &mut Context<Self>) {
        match event {
            TermEvent::Wakeup | TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => cx.notify(),
            // A guest's terminal answers nothing (the host's does) and takes
            // nothing a program there puts on the clipboard.
            TermEvent::PtyWrite(_) | TermEvent::ColorRequest(..) | TermEvent::TextAreaSizeRequest(_) | TermEvent::ClipboardStore(..) if self.guest => {}
            TermEvent::PtyWrite(text) => self.shared.write(text.into_bytes()),
            // A program's copy (OSC 52) only while you're in its terminal, not
            // from a session in the background; and it stays on this computer:
            // the shared clipboard doesn't pass it on to the machines.
            TermEvent::ClipboardStore(_, text) if window.is_window_active() && self.focus.is_focused(window) => {
                crate::fleet::clipboard::keep_here(&text);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            TermEvent::ColorRequest(index, format) => {
                let palette = Palette::new(&cx.theme());
                self.shared.write(format(palette.rgb(index)).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                let g = *self.shared.geometry.lock().unwrap();
                let size = WindowSize { num_lines: g.rows as u16, num_cols: g.cols as u16, cell_width: g.cell_w as u16, cell_height: g.line_h as u16 };
                self.shared.write(format(size).into_bytes());
            }
            TermEvent::ChildExit(status) => self.finish(status.code(), cx),
            TermEvent::Exit => self.finish(None, cx),
            // Never hand the clipboard to the program unasked (nor let one in
            // the background set it).
            TermEvent::ClipboardStore(..) | TermEvent::ClipboardLoad(..) | TermEvent::Title(_) | TermEvent::ResetTitle | TermEvent::Bell => {}
        }
    }

    fn finish(&mut self, code: Option<i32>, cx: &mut Context<Self>) {
        if !self.exited {
            self.exited = true;
            cx.emit(TerminalEvent::Exited(code));
            cx.notify();
        }
    }

    fn report_focus(&mut self, focused: bool, cx: &mut Context<Self>) {
        if !focused {
            self.ime.clear();
        }
        if self.mode().contains(TermMode::FOCUS_IN_OUT) {
            self.shared.write(if focused { b"\x1b[I".as_slice() } else { b"\x1b[O".as_slice() });
        }
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Keyboard

    fn prepare_input(&mut self, cx: &mut Context<Self>) {
        self.typed_at = Instant::now();
        self.cursor_on = true;
        let mut term = self.shared.term.lock();
        term.scroll_display(Scroll::Bottom);
        term.selection = None;
        drop(term);
        cx.notify();
    }

    fn write_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if self.exited || bytes.is_empty() {
            return;
        }
        self.ime.clear();
        self.prepare_input(cx);
        self.shared.write(bytes);
    }

    fn key_down(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.typed_at = Instant::now();
        if !self.cursor_on {
            self.cursor_on = true;
            cx.notify();
        }
        let ks = &e.keystroke;
        if ks.modifiers.platform || (!cfg!(target_os = "macos") && ks.modifiers.control && ks.modifiers.shift) {
            let handled = match ks.key.as_str() {
                "c" => self.copy(cx),
                "v" => self.paste(cx),
                _ => false,
            };
            if handled {
                cx.stop_propagation();
            }
            return;
        }
        if self.exited {
            return;
        }
        // Let the platform input method compose/commit printable text. Raw
        // key bytes here would swallow the first pinyin key or send it twice.
        if keys::is_text_input(ks) {
            return;
        }
        if let Some(bytes) = keys::to_bytes(ks, self.mode()) {
            self.write_input(bytes, cx);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let text = self.shared.term.lock().selection_to_string();
        match text.filter(|t| !t.is_empty()) {
            Some(t) => {
                cx.write_to_clipboard(ClipboardItem::new_string(t));
                true
            }
            None => false,
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else { return false };
        if !self.exited {
            self.write_input(keys::paste(&text, self.mode()), cx);
        }
        true
    }

    // -----------------------------------------------------------------------
    // Mouse

    fn cell(&self, pos: Point<Pixels>) -> Option<(usize, usize, bool)> {
        render::cell_at(&self.shared.geometry.lock().unwrap(), pos)
    }

    /// Programs that track the mouse (clicks, drags, wheel) get it,
    /// unless Shift is held, which selects text locally like other terminals.
    fn reporting(&self, mods: &Modifiers) -> Option<TermMode> {
        let mode = self.mode();
        (mode.intersects(TermMode::MOUSE_MODE) && !mods.shift && !self.exited).then_some(mode)
    }

    fn send_mouse(&self, mode: TermMode, code: u8, press: bool, (col, row): (usize, usize), mods: &Modifiers) {
        let code = code + if mods.shift { 4 } else { 0 } + if mods.alt { 8 } else { 0 } + if mods.control { 16 } else { 0 };
        let bytes = if mode.contains(TermMode::SGR_MOUSE) {
            format!("\x1b[<{code};{};{}{}", col + 1, row + 1, if press { 'M' } else { 'm' }).into_bytes()
        } else {
            let code = if press { code } else { 3 };
            let clamp = |v: usize| (32 + 1 + v).min(255) as u8;
            vec![0x1b, b'[', b'M', 32 + code, clamp(col), clamp(row)]
        };
        self.shared.write(bytes);
    }

    fn mouse_down(&mut self, button: MouseButton, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some((col, row, right)) = self.cell(e.position) else { return };
        if let Some(mode) = self.reporting(&e.modifiers) {
            let code = match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                _ => return,
            };
            self.pressed = Some((code, (col, row)));
            self.send_mouse(mode, code, true, (col, row), &e.modifiers);
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        let mut term = self.shared.term.lock();
        let point = GridPoint::new(Line(row as i32 - term.grid().display_offset() as i32), Column(col));
        let kind = match e.click_count {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        term.selection = Some(Selection::new(kind, point, if right { Side::Right } else { Side::Left }));
        drop(term);
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((col, row, right)) = self.cell(e.position) else { return };
        if let (Some((code, last)), Some(mode)) = (self.pressed, self.reporting(&e.modifiers)) {
            if last != (col, row) && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
                self.pressed = Some((code, (col, row)));
                self.send_mouse(mode, code + 32, true, (col, row), &e.modifiers);
            }
            return;
        }
        if self.selecting && e.pressed_button == Some(MouseButton::Left) {
            let mut term = self.shared.term.lock();
            let point = GridPoint::new(Line(row as i32 - term.grid().display_offset() as i32), Column(col));
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, if right { Side::Right } else { Side::Left });
            }
            drop(term);
            cx.notify();
        }
    }

    fn mouse_up(&mut self, e: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((code, cell)) = self.pressed.take() {
            if let Some(mode) = self.reporting(&e.modifiers) {
                let cell = self.cell(e.position).map(|(c, r, _)| (c, r)).unwrap_or(cell);
                self.send_mouse(mode, code, false, cell, &e.modifiers);
            }
            return;
        }
        if self.selecting {
            self.selecting = false;
            let mut term = self.shared.term.lock();
            if term.selection.as_ref().is_some_and(|s| s.is_empty()) {
                term.selection = None;
            }
            drop(term);
            cx.notify();
        }
    }

    fn scroll(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_h = self.shared.geometry.lock().unwrap().line_h.max(1.);
        let delta = match e.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / line_h,
        };
        self.scroll_rest += delta;
        let lines = self.scroll_rest.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_rest -= lines as f32;
        let up = lines > 0;
        if let Some(mode) = self.reporting(&e.modifiers) {
            if let Some((col, row, _)) = self.cell(e.position) {
                for _ in 0..lines.unsigned_abs().min(10) {
                    self.send_mouse(mode, if up { 64 } else { 65 }, true, (col, row), &e.modifiers);
                }
            }
            return;
        }
        let mode = self.mode();
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let key: &[u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.shared.write(key.repeat(lines.unsigned_abs() as usize));
            return;
        }
        self.shared.term.lock().scroll_display(Scroll::Delta(lines));
        cx.notify();
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::new(&cx.theme());
        let focused = self.focus.is_focused(window);
        div()
            .id("terminal")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .bg(palette.bg)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e, window, cx| this.mouse_down(MouseButton::Left, e, window, cx)))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, e, window, cx| this.mouse_down(MouseButton::Middle, e, window, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, e, window, cx| this.mouse_down(MouseButton::Right, e, window, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_action(cx.listener(|_, _: &IncreaseFontSize, _, cx| theme_mod::update_terminal(cx, |a| a.terminal_font_size += 1.)))
            .on_action(cx.listener(|_, _: &DecreaseFontSize, _, cx| theme_mod::update_terminal(cx, |a| a.terminal_font_size -= 1.)))
            .on_action(cx.listener(|_, _: &ResetFontSize, _, cx| theme_mod::update_terminal(cx, |a| a.terminal_font_size = theme_mod::TERMINAL_FONT_SIZE)))
            .child(render::grid(cx.entity(), palette, focused, self.cursor_on || !focused, Metrics::current(cx)))
    }
}
