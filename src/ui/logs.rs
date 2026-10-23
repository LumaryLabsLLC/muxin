//! A machine's logs, live: its system log, log files, Docker containers and
//! services (see fleet/logs.rs and the agent's logs.rs). The last lines,
//! then new ones as they're written, over the machine's link; a filter,
//! pause and follow. Embedded (a Compose project's page), it shows one log
//! with a chip for each of the project's services to narrow it.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Bounds, ClipboardItem, Context, Entity, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, FocusHandle, Focusable, FontWeight, ListHorizontalSizingBehavior, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use serde_json::json;

use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, icon, spinner};
use super::theme::{ActiveTheme, mono_font};
use crate::fleet::Engine;
use crate::fleet::link::Link;
use crate::fleet::logs::{LogSource, LogSources};

/// Lines kept; older ones go.
const KEEP: usize = 20_000;
/// Lines shown before following.
const FIRST_LINES: u64 = 500;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    System,
    Files,
    Docker,
    Services,
}

enum State {
    /// Nothing picked yet.
    Idle,
    Connecting,
    Live,
    Ended(String),
}

enum Msg {
    Live,
    Lines(String),
    Ended(String),
}

/// The log's channel on the machine's link; closed when dropped.
struct Remote {
    link: Arc<Link>,
    channel: u32,
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.link.close_channel(self.channel);
    }
}

pub struct LogsView {
    engine: Arc<Engine>,
    machine: String,
    sources: Option<Result<LogSources, String>>,
    tab: Tab,
    source: Option<LogSource>,
    state: State,
    lines: Vec<SharedString>,
    /// What came in while paused.
    held: Vec<SharedString>,
    /// A line still being written (no newline yet).
    partial: String,
    /// The lines the filter keeps (indexes into `lines`), and the longest of them.
    shown: Vec<usize>,
    longest: Option<usize>,
    paused: bool,
    follow: bool,
    /// The filter's text, lowercased (refiltered when it changes).
    filter_cache: String,
    filter: Entity<TextInput>,
    path: Entity<TextInput>,
    /// Narrows the list of logs on the left.
    find: Entity<TextInput>,
    /// Selected text: from where the mouse went down to where it is, as
    /// (row among the shown lines, character); and whether it's dragging.
    selection: Option<((usize, usize), (usize, usize))>,
    selecting: bool,
    /// Where the lines were drawn, and a character's width (the font is monospace).
    list_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    char_w: f32,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// Part of another page: one log, no list of logs to pick from.
    embedded: bool,
    /// A Compose project's services (embedded), and the one shown (None: all).
    services: Vec<String>,
    only: Option<String>,
    remote: Option<Arc<Remote>>,
    stream: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for LogsView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl LogsView {
    pub fn new(engine: Arc<Engine>, machine: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self::make(engine, machine, window, cx);
        view.load_sources(true, window, cx);
        view
    }

    /// One log inside another page: a Compose project's, with its services.
    pub fn embedded(engine: Arc<Engine>, machine: String, source: LogSource, services: Vec<String>, only: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self::make(engine, machine, window, cx);
        view.embedded = true;
        view.services = services;
        view.only = only;
        view.open(source, window, cx);
        view
    }

    /// Shows one service's lines (None: all of them).
    pub fn set_only(&mut self, only: Option<String>, cx: &mut Context<Self>) {
        if self.only != only {
            self.only = only;
            self.refilter();
            cx.notify();
        }
    }

    pub fn set_services(&mut self, services: Vec<String>, cx: &mut Context<Self>) {
        if self.services != services {
            self.services = services;
            cx.notify();
        }
    }

    fn make(engine: Arc<Engine>, machine: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx, "Filter"));
        let path = cx.new(|cx| TextInput::new(cx, "A file's path, then Return"));
        let find = cx.new(|cx| TextInput::new(cx, "Find"));
        let subs = vec![
            cx.subscribe_in(&filter, window, |_, _, e: &InputEvent, _, cx| {
                if *e == InputEvent::Changed {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&find, window, |_, _, e: &InputEvent, _, cx| {
                if *e == InputEvent::Changed {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&path, window, |this, input, e: &InputEvent, window, cx| {
                if *e == InputEvent::Submit {
                    let p = input.read(cx).text().trim().to_string();
                    if !p.is_empty() {
                        this.open(LogSource::file(&p), window, cx);
                    }
                }
            }),
        ];
        LogsView {
            engine,
            machine,
            sources: None,
            tab: Tab::System,
            source: None,
            state: State::Idle,
            lines: Vec::new(),
            held: Vec::new(),
            partial: String::new(),
            shown: Vec::new(),
            longest: None,
            paused: false,
            follow: true,
            filter_cache: String::new(),
            filter,
            path,
            find,
            selection: None,
            selecting: false,
            list_bounds: Rc::new(Cell::new(None)),
            char_w: 7.2,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            embedded: false,
            services: Vec::new(),
            only: None,
            remote: None,
            stream: None,
            _tasks: Vec::new(),
            _subscriptions: subs,
        }
    }

    /// The log shown, for the sidebar ("Everything", "nginx", ...).
    pub fn source_name(&self) -> Option<&str> {
        self.source.as_ref().map(|s| s.name.as_str())
    }

    /// Asks the machine what it has; the first time, opens its system log.
    fn load_sources(&mut self, first: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (engine, machine) = (self.engine.clone(), self.machine.clone());
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.log_sources(&machine).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| {
                if first && this.source.is_none() && let Ok(s) = &result {
                    let pick = s.system.first().map(|x| (Tab::System, x.clone())).or_else(|| s.files.iter().find(|f| f.readable != Some(false)).map(|f| (Tab::Files, f.clone())));
                    if let Some((tab, source)) = pick {
                        this.tab = tab;
                        this.open(source, window, cx);
                    }
                }
                this.sources = Some(result);
                cx.notify();
            });
        });
        self._tasks.push(task);
    }

    /// Shows a log picked elsewhere (a service's, from the Services page).
    pub fn show_source(&mut self, source: LogSource, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = match source.kind.as_str() {
            "docker" | "compose" => Tab::Docker,
            "service" => Tab::Services,
            "file" => Tab::Files,
            _ => Tab::System,
        };
        self.open(source, window, cx);
    }

    /// Shows another log: its last lines, then what comes.
    fn open(&mut self, source: LogSource, window: &mut Window, cx: &mut Context<Self>) {
        self.remote = None;
        self.stream = None;
        self.lines.clear();
        self.held.clear();
        self.partial.clear();
        self.shown.clear();
        self.longest = None;
        self.selection = None;
        self.follow = true;
        self.paused = false;
        self.source = Some(source.clone());
        self.state = State::Connecting;
        cx.notify();
        if self.engine.is_demo() {
            self.state = State::Live;
            self.append(crate::fleet::logs::DEMO_LINES.join("\n") + "\n");
            return;
        }
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Msg>();
        let (engine, machine) = (self.engine.clone(), self.machine.clone());
        let (remote_tx, remote_rx) = std::sync::mpsc::channel::<Arc<Remote>>();
        engine.runtime().clone().spawn(async move {
            let Some((spec, probe)) = engine.machine(&machine) else {
                let _ = tx.unbounded_send(Msg::Ended("That machine is gone.".into()));
                return;
            };
            if let Err(e) = crate::fleet::sessions::agent::ensure(&spec, probe.as_ref()).await {
                let _ = tx.unbounded_send(Msg::Ended(e));
                return;
            }
            let Some(link) = crate::fleet::link::get(&spec).await else {
                let _ = tx.unbounded_send(Msg::Ended(format!("Can't reach the agent on {}.", spec.name)));
                return;
            };
            let (channel, mut frames) = link.open_channel();
            let _ = remote_tx.send(Arc::new(Remote { link: link.clone(), channel }));
            let request = json!({ "op": "logs", "channel": channel, "source": source.request(), "lines": FIRST_LINES });
            match link.request(request, Duration::from_secs(20)).await {
                Ok(r) if r["ok"].as_bool() == Some(true) => {
                    let _ = tx.unbounded_send(Msg::Live);
                }
                Ok(r) => {
                    let _ = tx.unbounded_send(Msg::Ended(r["error"].as_str().unwrap_or("The machine couldn't open that log.").to_string()));
                    return;
                }
                Err(e) => {
                    let _ = tx.unbounded_send(Msg::Ended(e));
                    return;
                }
            }
            while let Some((kind, bytes)) = frames.recv().await {
                if kind == crate::fleet::link::CLOSE {
                    break;
                }
                if tx.unbounded_send(Msg::Lines(String::from_utf8_lossy(&bytes).into_owned())).is_err() {
                    return;
                }
            }
            let _ = tx.unbounded_send(Msg::Ended("The log stopped.".into()));
        });
        self.stream = Some(cx.spawn_in(window, async move |this, cx| {
            use futures::StreamExt;
            while let Some(first) = rx.next().await {
                // Everything that's arrived: one redraw for a burst of lines.
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let alive = this.update(cx, |this, cx| {
                    if this.remote.is_none() {
                        this.remote = remote_rx.try_recv().ok();
                    }
                    let mut text = String::new();
                    for msg in batch {
                        match msg {
                            Msg::Live => this.state = State::Live,
                            Msg::Lines(t) => text.push_str(&t),
                            Msg::Ended(why) => this.state = State::Ended(why),
                        }
                    }
                    if !text.is_empty() {
                        this.append(text);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// New text: whole lines join the log (or wait while paused).
    fn append(&mut self, text: String) {
        self.partial.push_str(&text);
        let Some(end) = self.partial.rfind('\n') else { return };
        let rest = self.partial.split_off(end + 1);
        let done = std::mem::replace(&mut self.partial, rest);
        let new: Vec<SharedString> = done.lines().map(|l| SharedString::from(l.trim_end_matches('\r').to_string())).collect();
        if self.paused {
            self.held.extend(new);
            return;
        }
        self.push(new);
    }

    fn push(&mut self, new: Vec<SharedString>) {
        let start = self.lines.len();
        self.lines.extend(new);
        if self.lines.len() > KEEP {
            self.lines.drain(..self.lines.len() - KEEP);
            self.refilter();
        } else {
            let filter = self.filter_text();
            for i in start..self.lines.len() {
                if keeps(&self.lines[i], &filter, self.only.as_deref()) {
                    self.note_longest(i);
                    self.shown.push(i);
                }
            }
        }
        if self.follow && !self.shown.is_empty() {
            self.scroll.scroll_to_item(self.shown.len() - 1, ScrollStrategy::Top);
        }
    }

    fn note_longest(&mut self, i: usize) {
        let len = self.lines[i].len();
        if self.longest.is_none_or(|l| self.lines.get(self.shown.get(l).copied().unwrap_or(usize::MAX)).is_none_or(|x| x.len() < len)) {
            self.longest = Some(self.shown.len());
        }
    }

    fn filter_text(&self) -> String {
        self.filter_cache.clone()
    }

    fn refilter(&mut self) {
        let filter = self.filter_text();
        self.selection = None;
        self.shown.clear();
        self.longest = None;
        for i in 0..self.lines.len() {
            if keeps(&self.lines[i], &filter, self.only.as_deref()) {
                self.note_longest(i);
                self.shown.push(i);
            }
        }
        if self.follow && !self.shown.is_empty() {
            self.scroll.scroll_to_item(self.shown.len() - 1, ScrollStrategy::Top);
        }
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        self.paused = !self.paused;
        if !self.paused {
            let held = std::mem::take(&mut self.held);
            self.push(held);
        }
        cx.notify();
    }

    /// Copies the selected text, or every line shown when nothing is selected.
    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = match self.selected_text() {
            Some(t) => t,
            None => self.shown.iter().map(|i| self.lines[*i].as_ref()).collect::<Vec<_>>().join("\n"),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// The selection, start before end; None when nothing is selected.
    fn ordered(&self) -> Option<((usize, usize), (usize, usize))> {
        let (a, b) = self.selection?;
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        (from != to).then_some((from, to))
    }

    fn selected_text(&self) -> Option<String> {
        let ((r0, c0), (r1, c1)) = self.ordered()?;
        let mut out = Vec::new();
        for r in r0..=r1.min(self.shown.len().saturating_sub(1)) {
            let line = &self.lines[self.shown[r]];
            let from = if r == r0 { c0 } else { 0 };
            let to = if r == r1 { c1 } else { usize::MAX };
            out.push(chars(line, from, to).to_string());
        }
        Some(out.join("\n"))
    }

    /// The (row, character) under a point in the window.
    fn at(&self, p: Point<Pixels>) -> Option<(usize, usize)> {
        let bounds = self.list_bounds.get()?;
        if self.shown.is_empty() {
            return None;
        }
        let offset = self.scroll.0.borrow().base_handle.offset();
        let y = f32::from(p.y - bounds.origin.y - offset.y) - LIST_PAD_Y;
        let x = f32::from(p.x - bounds.origin.x - offset.x) - LINE_PAD_X;
        let row = ((y / LINE_H).floor().max(0.) as usize).min(self.shown.len() - 1);
        let len = self.lines[self.shown[row]].chars().count();
        let col = ((x / self.char_w).round().max(0.) as usize).min(len);
        Some((row, col))
    }

    fn mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(pos) = self.at(e.position) else { return };
        if e.click_count >= 2 {
            // The whole line.
            let len = self.lines[self.shown[pos.0]].chars().count();
            self.selection = Some(((pos.0, 0), (pos.0, len)));
            self.selecting = false;
        } else if e.modifiers.shift && let Some((anchor, _)) = self.selection {
            self.selection = Some((anchor, pos));
            self.selecting = true;
        } else {
            self.selection = Some((pos, pos));
            self.selecting = true;
        }
        // Picking text out of a log that keeps moving: hold it still.
        self.follow = false;
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || e.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let (Some(pos), Some((anchor, _))) = (self.at(e.position), self.selection) {
            self.selection = Some((anchor, pos));
            cx.notify();
        }
    }
}

/// A line's padding and height, and the list's top padding (for hitting text).
const LINE_PAD_X: f32 = 14.;
const LINE_H: f32 = 18.;
const LIST_PAD_Y: f32 = 6.;

/// Characters `from..to` of a line.
fn chars(line: &str, from: usize, to: usize) -> &str {
    let byte = |n: usize| line.char_indices().nth(n).map(|(i, _)| i).unwrap_or(line.len());
    let (a, b) = (byte(from), byte(to));
    &line[a.min(b)..b]
}

/// Whether a line holds the filter (any case), and is the service's when
/// one is picked.
fn keeps(line: &str, filter: &str, only: Option<&str>) -> bool {
    (filter.is_empty() || line.to_lowercase().contains(filter)) && only.is_none_or(|svc| service_of(line) == Some(svc))
}

/// The Compose service a line is from ("web-1    | ..." is web's).
fn service_of(line: &str) -> Option<&str> {
    let (left, _) = line.split_once('|')?;
    let name = left.trim();
    Some(name.rsplit_once('-').filter(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).map(|(s, _)| s).unwrap_or(name))
}

/// A service's color: by its place among the project's services, else by its name.
pub fn service_color(name: &str, services: &[String]) -> gpui::Hsla {
    const HUES: [f32; 8] = [0.58, 0.11, 0.47, 0.92, 0.76, 0.33, 0.02, 0.66];
    let i = services.iter().position(|s| s == name).unwrap_or_else(|| name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)));
    gpui::hsla(HUES[i % HUES.len()], 0.6, 0.66, 1.)
}

/// A line's color by what it says: errors, warnings, debug noise.
fn tone(line: &str, theme: &super::theme::Theme) -> gpui::Hsla {
    let l = line.to_ascii_lowercase();
    if ["error", "fatal", "panic", "crit", "fail", "emerg", "alert"].iter().any(|w| l.contains(w)) {
        theme.error
    } else if l.contains("warn") {
        theme.warning
    } else if l.contains("debug") || l.contains("trace") {
        theme.text_placeholder
    } else {
        theme.text_muted
    }
}

impl Render for LogsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // A character's width in the log's font, for picking text with the mouse.
        let ts = window.text_system();
        let font_id = ts.resolve_font(&gpui::font(mono_font()));
        self.char_w = ts.advance(font_id, px(12.), 'm').map(|s| f32::from(s.width)).unwrap_or(7.2);
        // The filter, as typed (refiltered when it changes; see the subscription).
        let typed = self.filter.read(cx).text().trim().to_lowercase();
        if typed != self.filter_cache {
            self.filter_cache = typed;
            self.refilter();
        }
        let sources = self.sources.as_ref().and_then(|s| s.as_ref().ok()).cloned().unwrap_or_default();
        let count = |tab: Tab| match tab {
            Tab::System => sources.system.len(),
            Tab::Files => sources.files.len(),
            Tab::Docker => sources.docker.len(),
            Tab::Services => sources.services.len(),
        };
        let tabs = div().flex_none().flex().gap(px(2.)).p(px(2.)).rounded(crate::ui::kit::corner(7.)).bg(theme.element).children(
            [(Tab::System, "System"), (Tab::Files, "Files"), (Tab::Docker, "Docker"), (Tab::Services, "Services")].into_iter().map(|(tab, label)| {
                let on = self.tab == tab;
                let n = count(tab);
                div()
                    .id(SharedString::from(format!("logs-tab-{label}")))
                    .px(px(9.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .rounded(crate::ui::kit::corner(5.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(if on { theme.text } else { theme.text_placeholder })
                    .when(on, |d| d.bg(theme.element_selected))
                    .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                    .child(label)
                    .when(n > 0, |d| d.child(div().text_size(px(10.5)).text_color(theme.text_placeholder).child(n.to_string())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
            }),
        );
        let status = match &self.state {
            State::Idle => "Pick a log".to_string(),
            State::Connecting => "Opening…".to_string(),
            State::Live if self.paused => format!("Paused · {} new", self.held.len()),
            State::Live => "Live".to_string(),
            State::Ended(why) => why.clone(),
        };
        // Embedded: a chip for each service (All, web, api, ...) and how it's going.
        let chips = self.embedded.then(|| {
            let mut all: Vec<(Option<String>, String)> = vec![(None, "All".to_string())];
            all.extend(self.services.iter().map(|s| (Some(s.clone()), s.clone())));
            let services = self.services.clone();
            div().flex_none().flex().gap(px(2.)).p(px(2.)).rounded(crate::ui::kit::corner(7.)).bg(theme.element).children(all.into_iter().map(|(svc, label)| {
                let on = self.only == svc;
                let color = svc.as_deref().map(|s| service_color(s, &services));
                div()
                    .id(SharedString::from(format!("logs-only-{label}")))
                    .px(px(9.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .rounded(crate::ui::kit::corner(5.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(if on { theme.text } else { theme.text_placeholder })
                    .when(on, |d| d.bg(theme.element_selected))
                    .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                    .children(color.map(|c| kit::dot(c, 7.)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_only(svc.clone(), cx)))
            }))
        });
        let toolbar = div()
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .min_h(px(44.))
            .py(px(6.))
            .px(px(14.))
            .border_b_1()
            .border_color(theme.border_variant)
            .when(!self.embedded, |d| d.child(tabs))
            .children(chips)
            .when(self.embedded, |d| d.child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(status.clone())))
            .child(div().flex_1())
            .child(div().flex_none().w(px(200.)).child(self.filter.clone()))
            .child(
                Button::new("logs-pause", if self.paused { "Resume" } else { "Pause" })
                    .icon(if self.paused { "play" } else { "pause" })
                    .style(ButtonStyle::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx))),
            )
            .child(
                Button::new("logs-follow", "Follow")
                    .icon("arrow-down")
                    .style(if self.follow { ButtonStyle::Outlined } else { ButtonStyle::Ghost })
                    .tooltip("Keep the newest line in view")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.follow = !this.follow;
                        if this.follow && !this.shown.is_empty() {
                            this.scroll.scroll_to_item(this.shown.len() - 1, ScrollStrategy::Top);
                        }
                        cx.notify();
                    })),
            )
            .child(Button::icon_only("logs-copy", "copy").tooltip("Copy the selection (⌘C), or every line shown").on_click(cx.listener(|this, _, _, cx| this.copy(cx))))
            .child(
                Button::icon_only("logs-clear", "trash-2").tooltip("Clear").on_click(cx.listener(|this, _, _, cx| {
                    this.lines.clear();
                    this.shown.clear();
                    this.longest = None;
                    cx.notify();
                })),
            );

        // What there is to open, for the tab.
        let (list, note): (Vec<LogSource>, Option<String>) = match self.tab {
            Tab::System => (sources.system.clone(), sources.system_note.clone()),
            Tab::Files => (sources.files.clone(), None),
            Tab::Docker => (sources.docker.clone(), sources.docker_note.clone().or_else(|| sources.docker.is_empty().then(|| "No containers here.".to_string()))),
            Tab::Services => (sources.services.clone(), sources.services_note.clone()),
        };
        let find = self.find.read(cx).text().trim().to_lowercase();
        let list: Vec<LogSource> = list.into_iter().filter(|x| find.is_empty() || x.name.to_lowercase().contains(&find) || x.detail.to_lowercase().contains(&find)).collect();
        // Docker: each Compose project, its containers under it, then the rest.
        let list: Vec<(LogSource, bool)> = if self.tab == Tab::Docker {
            let mut ordered = Vec::new();
            for p in list.iter().filter(|x| x.kind == "compose") {
                ordered.push((p.clone(), false));
                ordered.extend(list.iter().filter(|x| x.kind == "docker" && x.project == p.project).map(|x| (x.clone(), true)));
            }
            let grouped = |x: &LogSource| x.kind == "compose" || list.iter().any(|p| p.kind == "compose" && p.project.is_some() && p.project == x.project);
            ordered.extend(list.iter().filter(|x| !grouped(x)).map(|x| (x.clone(), false)));
            ordered
        } else {
            list.into_iter().map(|x| (x, false)).collect()
        };
        let current = self.source.clone();
        let picker = div()
            .id("logs-sources")
            .flex_none()
            .w(px(250.))
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme.border_variant)
            .flex()
            .flex_col()
            .p(px(6.))
            .gap(px(1.))
            .child(div().flex_none().p(px(4.)).child(div().w(px(230.)).child(self.find.clone())))
            .when(self.tab == Tab::Files, |d| d.child(div().flex_none().px(px(4.)).pb(px(4.)).child(div().w(px(230.)).child(self.path.clone()))))
            .children(match &self.sources {
                None => Some(div().flex().items_center().gap(px(8.)).p(px(10.)).text_size(px(12.)).text_color(theme.text_muted).child(spinner("logs-loading", 12., theme.text_muted)).child("Looking for logs…").into_any_element()),
                Some(Err(e)) => Some(div().p(px(10.)).text_size(px(12.)).text_color(theme.error).child(e.clone()).into_any_element()),
                Some(Ok(_)) => None,
            })
            .children(note.map(|n| div().p(px(8.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(n)))
            .when(list.is_empty() && !find.is_empty(), |d| d.child(div().p(px(8.)).text_size(px(11.5)).text_color(theme.text_placeholder).child("Nothing matches.")))
            .children(list.into_iter().enumerate().map(|(i, (s, nested))| {
                let on = current.as_ref().is_some_and(|c| c.kind == s.kind && c.target == s.target);
                let locked = s.readable == Some(false);
                let stopped = s.running == Some(false) || s.state.as_deref().is_some_and(|st| st != "running");
                // A service's or container's state, as a dot.
                let dot = match (s.state.as_deref(), s.running) {
                    (Some("failed"), _) => Some(theme.error),
                    (Some("running"), _) | (None, Some(true)) => Some(theme.success),
                    (Some(_), _) | (None, Some(false)) => Some(theme.text_placeholder),
                    _ => None,
                };
                let pick = s.clone();
                let compose = s.kind == "compose";
                let inset = if nested { 14. } else { 0. };
                div()
                    .id(SharedString::from(format!("logs-src-{i}")))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .ml(px(inset))
                    .when(nested, |d| d.border_l_1().border_color(theme.border_variant).rounded_l_none())
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(crate::ui::kit::corner(6.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(theme.element_selected))
                    .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.5))
                            .text_color(if locked || stopped { theme.text_placeholder } else { theme.text })
                            .children(dot.map(|c| kit::dot(c, 6.)))
                            .when(compose, |d| d.child(icon("layers", 12., theme.text_muted)))
                            .child(div().w(px(190. - inset - if compose { 18. } else { 0. })).truncate().when(compose, |d| d.font_weight(FontWeight::MEDIUM)).child(s.name.clone()))
                            .when(locked, |d| d.child(icon("lock", 11., theme.text_placeholder))),
                    )
                    .when(!s.detail.is_empty(), |d| d.child(div().w(px(220. - inset)).truncate().text_size(px(11.)).text_color(theme.text_placeholder).child(s.detail.clone())))
                    .on_click(cx.listener(move |this, _, window, cx| this.open(pick.clone(), window, cx)))
            }));

        // The lines.
        let shown = self.shown.clone();
        let lines = self.lines.clone();
        let colors = theme;
        // A Compose project's lines start with their service: in its color.
        let compose = self.source.as_ref().is_some_and(|s| s.kind == "compose");
        let services = self.services.clone();
        let body = if self.source.is_none() {
            div().flex_1().flex().items_center().justify_center().child(kit::empty_state(&theme, "scroll-text", "Pick a log on the left.")).into_any_element()
        } else {
            let selection = self.ordered();
            let selected_bg = theme.selection;
            let uniform = uniform_list("log-lines", shown.len(), move |range, _, _| {
                range
                    .map(|n| {
                        let line = lines[shown[n]].clone();
                        let color = tone(&line, &colors);
                        let row = div().flex().px(px(LINE_PAD_X)).h(px(LINE_H)).whitespace_nowrap().text_color(color);
                        // The selected part of this line, highlighted.
                        match selection.filter(|((r0, _), (r1, _))| (*r0..=*r1).contains(&n)) {
                            Some(((r0, c0), (r1, c1))) => {
                                let from = if n == r0 { c0 } else { 0 };
                                let to = if n == r1 { c1 } else { usize::MAX };
                                let (head, mid, tail) = (chars(&line, 0, from).to_string(), chars(&line, from, to).to_string(), chars(&line, to, usize::MAX).to_string());
                                // An empty line inside the selection still shows it.
                                let mid = if mid.is_empty() && n != r1 { " ".to_string() } else { mid };
                                row.child(head).child(div().bg(selected_bg).child(mid)).child(tail)
                            }
                            None if compose && let Some(bar) = line.find('|') => {
                                let color = service_of(&line).map(|s| service_color(s, &services)).unwrap_or(colors.text_muted);
                                row.child(div().text_color(color).child(line[..=bar].to_string())).child(line[bar + 1..].to_string())
                            }
                            None => row.child(line),
                        }
                    })
                    .collect()
            })
            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
            .with_width_from_item(self.longest)
            .track_scroll(self.scroll.clone())
            .size_full()
            .py(px(LIST_PAD_Y))
            .font_family(mono_font())
            .text_size(px(12.))
            .cursor_text();
            // Where it's drawn, for picking text; mouse to select.
            let bounds = self.list_bounds.clone();
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .child(gpui::canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {}).absolute().size_full())
                .child(uniform)
                .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                .on_mouse_move(cx.listener(Self::mouse_move))
                .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.selecting = false))
                .into_any_element()
        };
        let source_line = self.source.as_ref().map(|s| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(30.))
                .px(px(14.))
                .border_b_1()
                .border_color(theme.border_variant)
                .text_size(px(12.))
                .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.text).child(s.name.clone()))
                .child(div().text_color(theme.text_placeholder).child(status.clone()))
                .child(div().flex_1())
                .child(div().text_color(theme.text_placeholder).child(if self.shown.len() == self.lines.len() { format!("{} lines", self.lines.len()) } else { format!("{} of {} lines", self.shown.len(), self.lines.len()) }))
        });
        div()
            .id("logs-view")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                let m = e.keystroke.modifiers;
                if e.keystroke.key == "c" && (m.platform || m.control) {
                    this.copy(cx);
                    cx.stop_propagation();
                } else if e.keystroke.key == "escape" && this.selection.is_some() {
                    this.selection = None;
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.canvas)
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .when(!self.embedded, |d| d.child(picker))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .flex()
                            .flex_col()
                            .when(!self.embedded, |d| d.children(source_line))
                            // Scrolling up stops following; the Follow button turns it back on.
                            .on_scroll_wheel(cx.listener(|this, e: &gpui::ScrollWheelEvent, _, cx| {
                                let up = match e.delta {
                                    gpui::ScrollDelta::Lines(l) => l.y > 0.,
                                    gpui::ScrollDelta::Pixels(p) => f32::from(p.y) > 0.,
                                };
                                if up && this.follow {
                                    this.follow = false;
                                    cx.notify();
                                }
                            }))
                            .child(body),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{chars, keeps, service_of};

    #[test]
    fn knows_a_lines_service() {
        assert_eq!(service_of("web-1    | 2026-10-12T16:11:56Z start"), Some("web"));
        assert_eq!(service_of("my-api-12 | up"), Some("my-api"));
        assert_eq!(service_of("cache | up"), Some("cache"));
        assert_eq!(service_of("no bar"), None);
        assert!(keeps("web-1 | GET / 200", "get", Some("web")));
        assert!(!keeps("api-1 | GET / 200", "", Some("web")));
    }

    #[test]
    fn cuts_by_character() {
        assert_eq!(chars("hello world", 0, 5), "hello");
        assert_eq!(chars("hello world", 6, usize::MAX), "world");
        assert_eq!(chars("héllo", 1, 3), "él");
        assert_eq!(chars("abc", 5, 9), "");
        assert_eq!(chars("abc", 2, 1), "");
    }
}
