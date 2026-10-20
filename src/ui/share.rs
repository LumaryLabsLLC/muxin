//! Sharing in the window: the Share dialog (a session, or a whole machine),
//! letting guests in, and shares others gave you (joined from a link). The
//! sharing itself is crate::share.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{AnyElement, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};
use serde_json::json;
use tokio::sync::mpsc;

use super::desktop::DesktopView;
use super::dialogs::{DialogEvent, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, card, icon, section_title};
use super::root::{Modal, Route, Root};
use super::terminal::{RemoteIo, RemoteOutput, TerminalView};
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::Engine;
use crate::fleet::model::Machine;
use crate::fleet::ports::ListeningPort;
use crate::fleet::sessions::SessionInfo;
use crate::share::host::{Desktop, Port};
use crate::share::{guest, host};

/// How long a share lasts: what the dialog offers.
const DURATIONS: &[(&str, u64)] = &[("15 min", 15 * 60), ("1 hour", 3600), ("8 hours", 8 * 3600), ("1 day", 24 * 3600)];

/// Sessions of a shared machine kept open at once (switching back is instant).
const OPEN_SESSIONS: usize = 6;

/// "42 min", "3 hours".
pub(super) fn time_left(ends_at: i64) -> String {
    let left = ends_at - chrono::Utc::now().timestamp();
    if left <= 0 {
        "Ending".into()
    } else if left < 3600 {
        format!("{} min", (left / 60).max(1))
    } else {
        let h = (left + 1800) / 3600;
        format!("{h} hour{}", if h == 1 { "" } else { "s" })
    }
}

/// A share's key in the window: its session's id, or `machine:<id>`.
pub(super) fn share_key(t: &host::Target) -> String {
    if t.is_machine() { format!("machine:{}", t.machine_id) } else { t.session_id.clone() }
}

fn fact(theme: &Theme, glyph: &'static str, text: impl Into<SharedString>) -> impl IntoElement {
    div().flex().items_center().gap(px(10.)).text_size(px(13.)).text_color(theme.text).child(icon(glyph, 15., theme.text_muted)).child(text.into())
}

/// A choice among a few, as a small button (the durations, the desktop).
fn choice(theme: &Theme, id: (&'static str, usize), label: &'static str, picked: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(10.))
        .h(px(26.))
        .flex()
        .items_center()
        .rounded(px(6.))
        .border_1()
        .text_size(px(12.))
        .when(enabled, |d| d.cursor_pointer())
        .text_color(if enabled { theme.text } else { theme.text_placeholder })
        .border_color(if picked { theme.accent } else { theme.border_variant })
        .bg(if picked { theme.accent.opacity(0.12) } else { theme.element })
        .child(label)
}

/// A checkbox that can't be ticked yet (it needs another option first).
fn off_checkbox(theme: &Theme, label: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.5))
        .text_color(theme.text_placeholder)
        .child(div().size(px(15.)).rounded(px(4.)).border_1().border_color(theme.border_variant))
        .child(label)
        .into_any_element()
}

/// A server as the user knows it: its page's title, its container, its program.
fn server_name(p: &ListeningPort) -> String {
    p.title.clone().filter(|t| !t.trim().is_empty()).or_else(|| p.container.clone()).or_else(|| Some(p.process.clone()).filter(|x| !x.is_empty())).unwrap_or_else(|| format!("Port {}", p.port))
}

// ---------------------------------------------------------------------------
// Sharing

/// Sharing a session or a machine: one click makes the link and copies it
/// (you let each person in, they watch, for an hour); what they can do and
/// for how long change here afterwards, while it runs.
pub struct ShareDialog {
    engine: Arc<Engine>,
    target: host::Target,
    /// A machine whose desktop can be shown.
    has_desktop: bool,
    /// A machine's servers, to pick which to share; its other ports shown too.
    servers: Option<Result<Vec<ListeningPort>, String>>,
    all_ports: bool,
    /// The share, once made.
    share: Option<String>,
    creating: bool,
    copied: bool,
    error: Option<String>,
    focus: FocusHandle,
}

impl EventEmitter<DialogEvent> for ShareDialog {}

impl ShareDialog {
    /// The share this session or machine has, or a new one made right away.
    pub fn new(engine: Arc<Engine>, target: host::Target, has_desktop: bool, existing: Option<String>, cx: &mut Context<Self>) -> Self {
        let mut dialog = ShareDialog { engine, target, has_desktop, servers: None, all_ports: false, share: existing, creating: false, copied: false, error: None, focus: cx.focus_handle() };
        if dialog.share.is_none() {
            dialog.create(cx);
        }
        if dialog.target.is_machine() {
            dialog.load_servers(cx);
        }
        dialog
    }

    pub fn first_focus(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        self.creating = true;
        self.error = None;
        cx.notify();
        let options = host::Options { approve: true, can_type: false, manage: false, desktop: Desktop::None, ports: Vec::new(), ttl: Duration::from_secs(3600) };
        let source = Arc::new(host::EngineSource { engine: self.engine.clone(), machine_id: self.target.machine_id.clone() });
        let (target, server) = (self.target.clone(), self.engine.settings().share_server());
        let job = self.engine.runtime().spawn(async move { host::start(&server, target, options, source).await });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.creating = false;
                match result {
                    Ok(view) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(view.link.clone()));
                        this.copied = true;
                        this.share = Some(view.id);
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// What's listening on the machine, sites first.
    fn load_servers(&mut self, cx: &mut Context<Self>) {
        let (engine, id) = (self.engine.clone(), self.target.machine_id.clone());
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.ports(&id).await }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string())).map(|mut list| {
                list.sort_by_key(|p| (!p.is_site(), p.port));
                list.dedup_by_key(|p| p.port);
                list
            });
            let _ = this.update(cx, |this, cx| {
                this.servers = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_making(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let title = format!("Share {}", self.target.title);
        match self.error.clone() {
            None => frame(theme, title, None)
                .child(div().flex().items_center().gap(px(10.)).text_size(px(13.)).text_color(theme.text_muted).child(kit::spinner("share-making", 14., theme.accent)).child("Making a link…"))
                .into_any_element(),
            Some(e) => frame(theme, title, None)
                .children(error_line(theme, &Some(e)))
                .child(footer(vec![
                    Button::new("share-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                    Button::new("share-retry", "Try again").style(ButtonStyle::Primary).on_click(cx.listener(|this, _, _, cx| this.create(cx))).into_any_element(),
                ]))
                .into_any_element(),
        }
    }

    /// Off, view or control: what guests get of the machine's screen.
    fn desktop_row(&self, theme: &Theme, view: &host::ShareView) -> AnyElement {
        let options = [(Desktop::None, "Off"), (Desktop::View, "View"), (Desktop::Control, "Control")];
        let choices = options.into_iter().enumerate().map(|(i, (d, label))| {
            // Control is for people you let in.
            let enabled = d != Desktop::Control || view.approve;
            let (id, ports) = (view.id.clone(), view.ports.clone());
            choice(theme, ("share-desktop", i), label, view.desktop == d, enabled).when(enabled, |el| el.on_click(move |_, _, _| host::set_access(&id, d, ports.clone())))
        });
        let (note, warn) = match view.desktop {
            Desktop::None => ("They don't see the screen.", false),
            Desktop::View => ("They see the screen, live. You can let someone control it.", false),
            Desktop::Control => ("You choose who can use its mouse and keyboard, as you.", true),
        };
        let note = if view.approve || view.desktop != Desktop::None { note } else { "Control is only for people you let in." };
        div()
            .flex()
            .flex_col()
            .gap(px(5.))
            .child(div().flex().items_center().gap(px(8.)).child(icon("monitor", 14., theme.text_muted)).child(div().flex_1().text_size(px(12.5)).child("Desktop")).children(choices))
            .child(div().pl(px(22.)).text_size(px(11.5)).text_color(if warn { theme.warning } else { theme.text_placeholder }).child(note))
            .into_any_element()
    }

    /// The machine's servers, each one to share or not: its web servers
    /// (and any picked), its other ports on request.
    fn servers_rows(&self, theme: &Theme, view: &host::ShareView, cx: &mut Context<Self>) -> AnyElement {
        let all_ports = self.all_ports;
        let shown = |p: &&ListeningPort| all_ports || p.is_site() || view.ports.iter().any(|x| x.port == p.port);
        let others = match &self.servers {
            Some(Ok(list)) => list.iter().filter(|p| !shown(p)).count(),
            _ => 0,
        };
        let list: AnyElement = match &self.servers {
            None => div().flex().items_center().gap(px(8.)).text_size(px(12.)).text_color(theme.text_muted).child(kit::spinner("share-servers", 12., theme.text_muted)).child("Looking for servers…").into_any_element(),
            Some(Err(e)) => div().text_size(px(12.)).text_color(theme.text_muted).child(e.clone()).into_any_element(),
            Some(Ok(list)) if list.is_empty() => div().text_size(px(12.)).text_color(theme.text_placeholder).child(format!("Nothing is listening on {} right now.", self.target.machine_name)).into_any_element(),
            Some(Ok(list)) if !list.iter().any(|p| shown(&p)) => div().text_size(px(12.)).text_color(theme.text_placeholder).child("No web servers there right now.").into_any_element(),
            Some(Ok(list)) => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .children(list.iter().filter(shown).take(24).map(|p| {
                    let on = view.ports.iter().any(|x| x.port == p.port);
                    let (id, desktop, ports) = (view.id.clone(), view.desktop, view.ports.clone());
                    let port = Port { port: p.port, name: server_name(p), host: p.host.clone() };
                    let label = format!("{} · {}", server_name(p), p.port);
                    kit::checkbox(theme, ("share-port", p.port as usize), on, label, move |now, _, _| {
                        let mut ports = ports.clone();
                        ports.retain(|x| x.port != port.port);
                        if *now {
                            ports.push(port.clone());
                        }
                        host::set_access(&id, desktop, ports);
                    })
                }))
                .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().items_center().gap(px(8.)).child(icon("network", 14., theme.text_muted)).child(div().text_size(px(12.5)).child("Servers")))
            .child(
                div()
                    .pl(px(22.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(list)
                    .when(others > 0 || all_ports, |d| {
                        d.child(
                            div()
                                .id("share-all-ports")
                                .text_size(px(12.))
                                .text_color(theme.text_accent)
                                .cursor_pointer()
                                .child(if all_ports { "Only web servers".to_string() } else { format!("Show {others} other port{}", if others == 1 { "" } else { "s" }) })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.all_ports = !this.all_ports;
                                    cx.notify()
                                })),
                        )
                    })
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child("They open in Muxin, at localhost on their computer.")),
            )
            .into_any_element()
    }

    fn render_share(&mut self, theme: &Theme, view: host::ShareView, cx: &mut Context<Self>) -> AnyElement {
        let link = view.link.clone();
        let machine = self.target.is_machine();
        let (id, id2, id3) = (view.id.clone(), view.id.clone(), view.id.clone());
        let (approve, can_type) = (view.approve, view.can_type);
        let left = view.ends_at - chrono::Utc::now().timestamp();
        let type_label = if machine { "They can type in sessions" } else { "They can type" };
        let typing_row = if approve {
            kit::checkbox(theme, "share-type", can_type, type_label, move |on, _, _| host::set_options(&id2, true, *on)).into_any_element()
        } else {
            off_checkbox(theme, type_label)
        };
        let intro = match (self.copied, machine) {
            (true, false) => "Link copied. Send it to anyone: it opens in their browser or in Muxin.".to_string(),
            (false, false) => "Send the link to anyone: it opens in their browser or in Muxin.".to_string(),
            (copied, true) => format!("{}They see {}'s sessions, in their browser or in Muxin.", if copied { "Link copied. " } else { "" }, self.target.machine_name),
        };
        let what = if machine { "its sessions" } else { "this session" };
        let problem = view.problem.clone().map(|p| format!("Guests can't reach {} right now: {p}", self.target.machine_name));
        frame(theme, format!("Share {}", self.target.title), Some(intro.into()))
            .w(px(540.))
            .children(error_line(theme, &problem))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .p(px(6.))
                    .pl(px(12.))
                    .rounded(px(9.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .child(div().w(px(380.)).truncate().font_family(mono_font()).text_size(px(12.5)).child(link.clone()))
                    .child(div().flex_1())
                    .child(Button::new("share-copy", if self.copied { "Copied" } else { "Copy" }).icon(if self.copied { "check" } else { "copy" }).style(ButtonStyle::Primary).on_click(cx.listener(
                        move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                            this.copied = true;
                            cx.notify()
                        },
                    ))),
            )
            .child(
                card(theme)
                    .p(px(14.))
                    .gap(px(12.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(kit::checkbox(theme, "share-approve", approve, "Ask me before letting someone in", move |on, _, _| host::set_options(&id, *on, *on && can_type)))
                            .child(div().pl(px(23.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(if approve {
                                "You see who's asking, and their code.".to_string()
                            } else {
                                format!("Anyone with the link can watch {what}.")
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(typing_row)
                            .child(div().pl(px(23.)).text_size(px(11.5)).text_color(if approve && can_type { theme.warning } else { theme.text_placeholder }).child(match (approve, machine) {
                                (true, false) => "Their keys go to this session, as you.",
                                (true, true) => "Their keys go to the session they open, as you.",
                                (false, _) => "Only for people you let in.",
                            })),
                    )
                    .when(machine, |d| {
                        let id = view.id.clone();
                        let can = approve && can_type;
                        let row = if can {
                            kit::checkbox(theme, "share-manage", view.manage, "They can start and end sessions", move |on, _, _| host::set_manage(&id, *on)).into_any_element()
                        } else {
                            off_checkbox(theme, "They can start and end sessions")
                        };
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .child(row)
                                .child(div().pl(px(23.)).text_size(px(11.5)).text_color(if view.manage { theme.warning } else { theme.text_placeholder }).child(if can {
                                    format!("A terminal there runs anything, on {} as you.", self.target.machine_name)
                                } else {
                                    "Only for people you let in who can type.".to_string()
                                })),
                        )
                    })
                    .when(machine && self.has_desktop, |d| d.child(self.desktop_row(theme, &view)))
                    .when(machine, |d| d.child(self.servers_rows(theme, &view, cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(icon("clock", 14., theme.text_muted))
                            .child(div().flex_1().text_size(px(12.5)).child(format!("Ends in {}", time_left(view.ends_at))))
                            .children(DURATIONS.iter().enumerate().map(|(i, (label, secs))| {
                                let (secs, id) = (*secs, id3.clone());
                                // The choice closest to what's left shows as picked.
                                let picked = DURATIONS.iter().min_by_key(|(_, d)| (*d as i64 - left).abs()).is_some_and(|(l, _)| l == label);
                                choice(theme, ("share-ttl", i), label, picked, true).on_click(move |_, _, _| host::set_ends_in(&id, Duration::from_secs(secs)))
                            })),
                    ),
            )
            .child(footer(vec![
                Button::new("share-stop", "Stop sharing")
                    .style(ButtonStyle::Danger)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        host::stop(&view.id);
                        cx.emit(DialogEvent::Dismissed)
                    }))
                    .into_any_element(),
                div().flex_1().into_any_element(),
                Button::new("share-done", "Done").style(ButtonStyle::Outlined).key_hint("esc").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
            ]))
            .into_any_element()
    }
}

impl Focusable for ShareDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ShareDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = self.share.as_ref().and_then(|id| host::list().into_iter().find(|s| &s.id == id));
        let body = match view {
            Some(view) => self.render_share(&theme, view, cx),
            // Stopped from elsewhere (the menu, its time ran out).
            None if self.share.is_some() => {
                cx.emit(DialogEvent::Dismissed);
                div().into_any_element()
            }
            None => self.render_making(&theme, cx),
        };
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, e: &gpui::KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" || e.keystroke.key == "enter" {
                    cx.emit(DialogEvent::Dismissed)
                }
            }))
            .child(body)
    }
}

// ---------------------------------------------------------------------------
// Joining one

/// A share joined and let in, on its way from the Join dialog to its page.
#[derive(Clone)]
pub struct JoinedHandle {
    joined: Arc<guest::Joined>,
    updates: Arc<Mutex<Option<mpsc::UnboundedReceiver<guest::Update>>>>,
    info: guest::Info,
    can_type: bool,
    can_control: bool,
    can_manage: bool,
}

enum Stage {
    Form,
    Connecting,
    /// Connected: the code to read out, what's shared, whether it waits.
    Joined { code: String, info: Option<guest::Info>, waiting: bool },
}

pub struct JoinDialog {
    engine: Arc<Engine>,
    link: Entity<TextInput>,
    name: Entity<TextInput>,
    stage: Stage,
    joined: Option<Arc<guest::Joined>>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for JoinDialog {}

impl JoinDialog {
    pub fn new(engine: Arc<Engine>, link: Option<String>, name: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let link = cx.new(|cx| TextInput::new(cx, "https://muxin.dev/s#1.…").with_text(link.unwrap_or_default()));
        let name = cx.new(|cx| TextInput::new(cx, "Your name").with_text(name).selected());
        let mut subs = Vec::new();
        for input in [&link, &name] {
            subs.push(cx.subscribe_in(input, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
                InputEvent::Submit => this.join(cx),
                InputEvent::Cancel => this.cancel(cx),
                InputEvent::Changed => {
                    this.error = None;
                    cx.notify()
                }
            }));
        }
        JoinDialog { engine, link, name, stage: Stage::Form, joined: None, error: None, _subscriptions: subs }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        if self.link.read(cx).text().trim().is_empty() { self.link.focus_handle(cx) } else { self.name.focus_handle(cx) }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(j) = self.joined.take() {
            j.leave();
        }
        cx.emit(DialogEvent::Dismissed);
    }

    pub(super) fn join(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.stage, Stage::Form) {
            return;
        }
        let link = self.link.read(cx).text().trim().to_string();
        let name = self.name.read(cx).text().trim().to_string();
        if let Err(e) = crate::share::ticket::Ticket::parse(&link) {
            self.error = Some(e);
            cx.notify();
            return;
        }
        if name.is_empty() {
            self.error = Some("Enter your name first.".into());
            cx.notify();
            return;
        }
        let mut settings = self.engine.settings();
        if settings.share.name != name {
            settings.share.name = name.clone();
            let _ = self.engine.update_settings(settings);
        }
        self.stage = Stage::Connecting;
        self.error = None;
        cx.notify();
        let job = self.engine.runtime().spawn(async move { guest::join(&link, &name).await });
        cx.spawn(async move |this, cx| {
            let (joined, mut rx) = match job.await.unwrap_or_else(|e| Err(e.to_string())) {
                Ok(j) => j,
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        this.stage = Stage::Form;
                        this.error = Some(e);
                        cx.notify()
                    });
                    return;
                }
            };
            let _ = this.update(cx, |this, _| this.joined = Some(joined.clone()));
            while let Some(u) = rx.recv().await {
                let mut done = None;
                let alive = this.update(cx, |this, cx| {
                    match u {
                        guest::Update::Code(code) => this.stage = Stage::Joined { code, info: None, waiting: false },
                        guest::Update::Info(i) => {
                            if let Stage::Joined { info, .. } = &mut this.stage {
                                *info = Some(i);
                            }
                        }
                        guest::Update::Waiting => {
                            if let Stage::Joined { waiting, .. } = &mut this.stage {
                                *waiting = true;
                            }
                        }
                        guest::Update::Admitted { can_type, can_control, can_manage } => {
                            let info = match &this.stage {
                                Stage::Joined { info, .. } => info.clone().unwrap_or_default(),
                                _ => Default::default(),
                            };
                            done = Some((info, can_type, can_control, can_manage));
                        }
                        guest::Update::Denied => {
                            this.stage = Stage::Form;
                            this.joined = None;
                            this.error = Some("They didn't let you in.".into());
                        }
                        guest::Update::Ended(why) => {
                            this.stage = Stage::Form;
                            this.joined = None;
                            this.error = Some(why);
                        }
                        _ => {}
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    joined.leave();
                    return;
                }
                if let Some((info, can_type, can_control, can_manage)) = done {
                    let handle = JoinedHandle { joined: joined.clone(), updates: Arc::new(Mutex::new(Some(rx))), info, can_type, can_control, can_manage };
                    let _ = this.update(cx, |this, cx| {
                        this.joined = None;
                        cx.emit(DialogEvent::JoinedShare(handle))
                    });
                    return;
                }
            }
        })
        .detach();
    }

    /// What the guest will get, before going in.
    fn facts(theme: &Theme, i: &guest::Info) -> impl IntoElement {
        let mut facts = div().flex().flex_col().gap(px(8.));
        if i.whole_machine {
            facts = facts.child(fact(theme, if i.can_type { "keyboard" } else { "square-terminal" }, match (i.can_type, i.can_manage) {
                (_, true) => "Its sessions: you can watch, type, start and end them",
                (true, false) => "Its sessions: you can watch and type",
                _ => "Its sessions: you can watch",
            }));
            match i.desktop {
                Desktop::None => {}
                Desktop::View => facts = facts.child(fact(theme, "monitor", "Its screen: you can watch")),
                Desktop::Control if i.can_control => facts = facts.child(fact(theme, "monitor", "Its screen: you can watch and use it")),
                Desktop::Control => facts = facts.child(fact(theme, "monitor", "Its screen: you can watch, and use it when they let you")),
            }
            if !i.ports.is_empty() {
                let n = i.ports.len();
                facts = facts.child(fact(theme, "network", format!("{n} server{} to open here", if n == 1 { "" } else { "s" })));
            }
        } else {
            facts = facts.child(fact(theme, if i.can_type { "keyboard" } else { "eye" }, if i.can_type { "You can watch and type" } else { "You can watch" }));
        }
        facts.child(fact(theme, "clock", format!("Ends in {}", time_left(i.ends_at))))
    }
}

impl Render for JoinDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        match &self.stage {
            Stage::Form | Stage::Connecting => {
                let connecting = matches!(self.stage, Stage::Connecting);
                frame(&theme, "Join a share", Some("Paste the link someone sent you.".into()))
                    .child(field(&theme, "Link", self.link.clone(), None))
                    .child(field(&theme, "Your name", self.name.clone(), Some("They see it when you ask to join.")))
                    .children(error_line(&theme, &self.error))
                    .child(footer(vec![
                        Button::new("join-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, _, cx| this.cancel(cx))).into_any_element(),
                        Button::new("join-go", if connecting { "Connecting…" } else { "Join" })
                            .style(ButtonStyle::Primary)
                            .loading(connecting)
                            .key_hint("↵")
                            .on_click(cx.listener(|this, _, _, cx| this.join(cx)))
                            .into_any_element(),
                    ]))
                    .into_any_element()
            }
            Stage::Joined { code, info, waiting } => {
                let by = info.as_ref().map(|i| i.by.clone()).unwrap_or_else(|| "them".into());
                let title = info.as_ref().map(|i| if i.title.is_empty() { "a session".to_string() } else { i.title.clone() }).unwrap_or_else(|| "a share".into());
                let sub = info.as_ref().map(|i| if i.whole_machine { format!("A machine · shared by {}", i.by) } else { format!("{} on {} · shared by {}", agent_name(&i.agent), i.machine, i.by) });
                let (a, b) = code.split_at(3.min(code.len()));
                frame(&theme, format!("Join {title}"), sub.map(SharedString::from))
                    .children(info.as_ref().map(|i| Self::facts(&theme, i)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(4.))
                            .p(px(16.))
                            .rounded(px(10.))
                            .bg(theme.surface)
                            .border_1()
                            .border_color(theme.border)
                            .child(div().text_size(px(12.)).text_color(theme.text_muted).child("Your code"))
                            .child(div().font_family(mono_font()).text_size(px(30.)).font_weight(FontWeight::MEDIUM).child(format!("{a} {b}")))
                            .child(div().text_size(px(12.)).text_color(theme.text_muted).child(format!("Read it to {by} so they know it's you."))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(kit::spinner("join-wait", 12., theme.accent))
                            .child(div().flex_1().text_size(px(12.5)).text_color(theme.text_accent).child(if *waiting { format!("Waiting for {by} to let you in…") } else { "Asking to join…".into() }))
                            .child(Button::new("join-cancel-wait", "Cancel").style(ButtonStyle::Outlined).on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))),
                    )
                    .into_any_element()
            }
        }
    }
}

fn agent_name(agent: &str) -> &'static str {
    crate::fleet::sessions::agent(agent).map(|a| a.name).unwrap_or("A session")
}

/// Keys for a session open on a share (dropped unless its host lets us type).
struct GuestIo {
    joined: Arc<guest::Joined>,
    /// The session's channel, once open.
    channel: Arc<AtomicU32>,
}

impl RemoteIo for GuestIo {
    fn write(&self, bytes: &[u8]) {
        let channel = self.channel.load(Ordering::Relaxed);
        if channel != 0 {
            self.joined.send_keys(channel, bytes);
        }
    }

    fn resize(&self, _: u16, _: u16) {}

    fn close(&self) {}
}

/// A session open on a share.
struct OpenSession {
    /// Its name on the machine ("" for a shared session).
    name: String,
    terminal: Entity<TerminalView>,
    channel: Arc<AtomicU32>,
    /// Why it couldn't be shown.
    error: Option<String>,
}

/// What a share's page shows: a shared machine's own page, one of its
/// sessions (a shared session's is ""), or its desktop.
#[derive(Clone, Debug, PartialEq)]
pub enum Showing {
    Machine,
    Session(String),
    Desktop,
}

/// A share someone gave you, open here: one session, or a machine's.
pub struct JoinedShare {
    pub id: String,
    pub info: guest::Info,
    joined: Arc<guest::Joined>,
    pub can_type: bool,
    pub can_control: bool,
    /// May start, end and restart its sessions.
    pub can_manage: bool,
    /// A shared machine's sessions.
    sessions: Vec<guest::Session>,
    open: Vec<OpenSession>,
    showing: Showing,
    /// Its screen while it shows (closed when you look elsewhere, so it isn't
    /// streamed for nothing), and the quality picked for it.
    desktop: Option<Entity<DesktopView>>,
    quality: &'static str,
    /// Servers opened here: their port here, or why not.
    servers: HashMap<u16, Result<u16, String>>,
    /// Why it ended, once it has.
    pub ended: Option<String>,
}

impl Root {
    /// The Share dialog for a session or (`machine:<id>`) a machine: its link
    /// when it's shared already.
    pub(super) fn show_share(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let by = self.snapshot.github.login.clone().filter(|l| !l.is_empty()).or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "Someone".into());
        let (target, has_desktop) = if let Some(mid) = key.strip_prefix("machine:") {
            let Some(m) = self.snapshot.machines.iter().find(|m| m.id == mid) else { return };
            let target = host::Target {
                session_id: String::new(),
                machine_id: m.id.clone(),
                name: String::new(),
                title: m.name.clone(),
                agent: String::new(),
                machine_name: m.name.clone(),
                platform: m.platform.clone(),
                by,
            };
            (target, super::format::has_desktop(m))
        } else {
            let Some(info) = self.snapshot.sessions.iter().find(|x| x.id == key).cloned() else { return };
            let machine = self.snapshot.machines.iter().find(|m| m.id == info.machine_id);
            let target = host::Target {
                session_id: info.id.clone(),
                machine_id: info.machine_id.clone(),
                name: info.id.split_once('/').map(|(_, n)| n.to_string()).unwrap_or_default(),
                title: info.title.clone(),
                agent: info.agent.clone(),
                machine_name: machine.map(|m| m.name.clone()).unwrap_or_else(|| info.machine_id.clone()),
                platform: machine.map(|m| m.platform.clone()).unwrap_or_default(),
                by,
            };
            (target, false)
        };
        let existing = self.share_of(key).map(|s| s.id.clone());
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| ShareDialog::new(engine, target, has_desktop, existing, cx));
        self.show_modal(Modal::Share(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// The share of a session, or (`machine:<id>`) of a machine.
    pub(super) fn share_of(&self, key: &str) -> Option<&host::ShareView> {
        self.shares.iter().find(|s| share_key(&s.target) == key)
    }

    /// Shares changed (a guest knocked, came in, left): show it, and say
    /// who's asking to join when they first do.
    pub(super) fn shares_changed(&mut self, cx: &mut Context<Self>) {
        let before = std::mem::replace(&mut self.shares, host::list());
        // The machine stopped being reachable for guests: say why, once.
        let news = self.shares.iter().find_map(|s| {
            let was = before.iter().find(|b| b.id == s.id).and_then(|b| b.problem.as_ref());
            s.problem.as_ref().filter(|p| was != Some(*p)).map(|p| format!("Guests can't reach {}: {p}", s.target.machine_name))
        });
        if let Some(news) = news {
            self.notify_user(news, true, cx);
        }
        let mut knocks = Vec::new();
        for s in &self.shares {
            for g in s.guests.iter().filter(|g| g.state == "waiting") {
                if self.knocks_seen.insert((s.id.clone(), g.id)) {
                    knocks.push(format!("{} wants to join {}", g.name, s.target.title));
                }
            }
        }
        if let Some(k) = knocks.pop() {
            self.notify_user(k, false, cx);
        }
        cx.notify();
    }

    /// A Share button, or what's going on with the share: `key` is a
    /// session's id or `machine:<id>`.
    fn shared_button(&self, key: &str, id: &'static str, tooltip: &'static str, disabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let key = key.to_string();
        let Some(share) = self.share_of(&key) else {
            return Button::new(id, "Share").icon("link").tooltip(tooltip).disabled(disabled).on_click(cx.listener(move |this, _, window, cx| this.show_share(&key, window, cx))).into_any_element();
        };
        let waiting = share.guests.iter().filter(|g| g.state == "waiting").count();
        let inside: Vec<&host::GuestView> = share.guests.iter().filter(|g| g.state == "in").collect();
        let typing = inside.iter().any(|g| g.can_type || g.can_control);
        let what = match (inside.len(), waiting) {
            (_, w) if w > 0 => format!("{w} waiting"),
            (0, _) => "no one yet".to_string(),
            (1, _) => format!("{} {}", inside[0].name, if inside[0].can_control { "in control" } else if typing { "typing" } else { "watching" }),
            (n, _) => format!("{n} watching"),
        };
        Button::new(SharedString::from(format!("{id}-on")), format!("Shared · {what}"))
            .icon(if typing { "keyboard" } else { "eye" })
            .style(ButtonStyle::Outlined)
            .tooltip("Who's here, the link, and stopping")
            .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| this.open_menu(super::menu::MenuTarget::Share(key.clone()), e.position(), cx)))
            .into_any_element()
    }

    /// The session header's Share button, or what's going on with its share.
    pub(super) fn share_button(&self, info: &SessionInfo, cx: &mut Context<Self>) -> AnyElement {
        self.shared_button(&info.id, "session-share", "Share this session by link", false, cx)
    }

    /// The machine page's Share button: its sessions, and its desktop and
    /// servers if you choose.
    pub(super) fn machine_share_button(&self, m: &Machine, disabled: bool, cx: &mut Context<Self>) -> AnyElement {
        self.shared_button(&format!("machine:{}", m.id), "m-share", "Share this machine by link", disabled, cx)
    }

    /// Over the page: each guest waiting to be let in, whatever is showing.
    pub(super) fn knock_cards(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let mut cards = Vec::new();
        for share in &self.shares {
            for g in share.guests.iter().filter(|g| g.state == "waiting") {
                let (share_id, guest_id) = (share.id.clone(), g.id);
                let share_id2 = share_id.clone();
                let (a, b) = g.code.split_at(3.min(g.code.len()));
                let how = if g.client == "browser" { "In a browser" } else { "In Muxin" };
                let wants = match (share.target.is_machine(), share.can_type) {
                    (true, _) => format!("wants to use {}", share.target.title),
                    (false, true) => format!("wants to join {}", share.target.title),
                    (false, false) => format!("wants to watch {}", share.target.title),
                };
                cards.push(
                    div()
                        .mx(px(20.))
                        .mt(px(12.))
                        .flex()
                        .items_center()
                        .gap(px(14.))
                        .p(px(14.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme.accent)
                        .bg(theme.elevated)
                        .shadow_lg()
                        .child(
                            div()
                                .size(px(36.))
                                .flex_none()
                                .rounded_full()
                                .bg(theme.accent.opacity(0.25))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(g.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.))
                                .gap(px(2.))
                                .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).child(format!("{} {wants}", g.name)))
                                .child(div().text_size(px(12.)).text_color(theme.text_muted).child(format!("{how} · ask {} to read their code", g.name))),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_end()
                                .px(px(8.))
                                .child(div().text_size(px(11.5)).text_color(theme.text_muted).child("Their code"))
                                .child(div().font_family(mono_font()).text_size(px(22.)).font_weight(FontWeight::MEDIUM).child(format!("{a} {b}"))),
                        )
                        .child(Button::new(SharedString::from(format!("knock-deny-{guest_id}")), "Deny").style(ButtonStyle::Outlined).on_click(cx.listener(move |_, _, _, _| host::admit(&share_id, guest_id, false))))
                        .child(Button::new(SharedString::from(format!("knock-in-{guest_id}")), "Let in").style(ButtonStyle::Primary).on_click(cx.listener(move |_, _, _, _| host::admit(&share_id2, guest_id, true))))
                        .into_any_element(),
                );
            }
        }
        cards
    }

    /// The Join dialog (from a link opened, or the command palette).
    pub(super) fn show_join(&mut self, link: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Entity<JoinDialog> {
        let engine = self.engine.clone();
        let name = Some(self.engine.settings().share.name).filter(|n| !n.is_empty()).or_else(|| self.snapshot.github.login.clone()).unwrap_or_default();
        let dialog = cx.new(|cx| JoinDialog::new(engine, link, name, window, cx));
        self.show_modal(Modal::Join(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
        dialog
    }

    fn joined_mut(&mut self, id: &str) -> Option<&mut JoinedShare> {
        self.joined.iter_mut().find(|j| j.id == id)
    }

    /// A share joined and let in: its page, kept up to date from now on.
    pub(super) fn open_joined(&mut self, handle: &JoinedHandle, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut updates) = handle.updates.lock().unwrap().take() else { return };
        self.joined_count += 1;
        let id = format!("shared-{}", self.joined_count);
        self.joined.push(JoinedShare {
            id: id.clone(),
            info: handle.info.clone(),
            joined: handle.joined.clone(),
            can_type: handle.can_type,
            can_control: handle.can_control,
            can_manage: handle.can_manage,
            sessions: Vec::new(),
            open: Vec::new(),
            showing: if handle.info.whole_machine { Showing::Machine } else { Showing::Session(String::new()) },
            desktop: None,
            quality: "medium",
            servers: HashMap::new(),
            ended: None,
        });
        if !handle.info.whole_machine {
            self.open_shared_session(&id, String::new(), window, cx);
        }
        let sid = id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Some(u) = updates.recv().await {
                let alive = this.update_in(cx, |this, _window, cx| {
                    let Some(j) = this.joined_mut(&sid) else { return false };
                    match u {
                        guest::Update::Typing(allowed) => {
                            j.can_type = allowed;
                            j.can_manage &= allowed;
                            this.notify_user(if allowed { "You can type now." } else { "Typing is off. You're watching." }, false, cx);
                        }
                        guest::Update::Control(allowed) => {
                            j.can_control = allowed;
                            if let Some(d) = j.desktop.clone() {
                                d.update(cx, |d, cx| d.set_control(allowed, cx));
                            }
                            if j.info.desktop != Desktop::None {
                                this.notify_user(if allowed { "You can use their desktop now." } else { "You're watching their desktop." }, false, cx);
                            }
                        }
                        guest::Update::Manage(allowed) => {
                            j.can_manage = allowed;
                            this.notify_user(if allowed { "You can start and end sessions now." } else { "Starting and ending sessions is off." }, false, cx);
                        }
                        guest::Update::Access { desktop, ports } => {
                            j.info.desktop = desktop;
                            j.servers.retain(|port, _| ports.iter().any(|p| p.port == *port));
                            j.info.ports = ports;
                            if desktop == Desktop::None {
                                j.desktop = None;
                                if j.showing == Showing::Desktop {
                                    j.showing = Showing::Machine;
                                }
                            }
                        }
                        guest::Update::Sessions(list) => j.sessions = list,
                        guest::Update::Ended(why) => {
                            j.ended = Some(why);
                            cx.notify();
                            return false;
                        }
                        _ => {}
                    }
                    cx.notify();
                    true
                });
                if !alive.unwrap_or(false) {
                    return;
                }
            }
        });
        self.track(task);
        self.navigate(Route::Shared(id), window, cx);
    }

    /// Shows a share's machine page, one of its sessions or its desktop.
    pub(super) fn show_shared(&mut self, id: &str, showing: Showing, window: &mut Window, cx: &mut Context<Self>) {
        // Leaving the screen stops its stream (it starts again when shown).
        if showing != Showing::Desktop
            && let Some(j) = self.joined_mut(id)
        {
            j.desktop = None;
        }
        match showing {
            Showing::Session(name) => self.open_shared_session(id, name, window, cx),
            Showing::Desktop => self.show_shared_desktop(id, window, cx),
            Showing::Machine => {
                if let Some(j) = self.joined_mut(id) {
                    j.showing = Showing::Machine;
                }
            }
        }
        if self.route != Route::Shared(id.to_string()) {
            self.navigate(Route::Shared(id.to_string()), window, cx);
        }
        cx.notify();
    }

    /// Shows one of the share's sessions, opening it when it isn't yet.
    fn open_shared_session(&mut self, id: &str, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let runtime = self.engine.runtime().clone();
        let Some(j) = self.joined_mut(id) else { return };
        j.showing = Showing::Session(name.clone());
        if j.open.iter().any(|o| o.name == name) {
            cx.notify();
            return;
        }
        // The one looked at longest ago makes room.
        if j.open.len() >= OPEN_SESSIONS {
            let old = j.open.remove(0);
            j.joined.close(old.channel.load(Ordering::Relaxed));
        }
        let channel = Arc::new(AtomicU32::new(0));
        let (tx, output) = futures::channel::mpsc::unbounded();
        let io: Arc<dyn RemoteIo> = Arc::new(GuestIo { joined: j.joined.clone(), channel: channel.clone() });
        let terminal = cx.new(|cx| TerminalView::guest(io, output, (100, 30), window, cx));
        j.open.push(OpenSession { name: name.clone(), terminal, channel: channel.clone(), error: None });
        let joined = j.joined.clone();
        let session = name.clone();
        let opening = runtime.spawn(async move { joined.open(json!({ "op": "watch", "session": session })).await });
        let sid = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let mut frames = match opening.await.unwrap_or_else(|e| Err(e.to_string())) {
                Ok((ch, _, frames)) => {
                    channel.store(ch, Ordering::Relaxed);
                    frames
                }
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(o) = this.joined_mut(&sid).and_then(|j| j.open.iter_mut().find(|o| o.name == name)) {
                            o.error = Some(e);
                        }
                        cx.notify()
                    });
                    return;
                }
            };
            while let Some((kind, bytes)) = frames.recv().await {
                match kind {
                    crate::share::DATA => {
                        if tx.unbounded_send(RemoteOutput::Bytes(bytes)).is_err() {
                            return;
                        }
                    }
                    crate::share::SIZE if bytes.len() == 4 => {
                        let _ = tx.unbounded_send(RemoteOutput::Resize(u16::from_be_bytes([bytes[0], bytes[1]]), u16::from_be_bytes([bytes[2], bytes[3]])));
                        let _ = this.update(cx, |_, cx| cx.notify());
                    }
                    crate::share::EXIT => {
                        let code = (bytes.len() == 4).then(|| i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
                        let _ = tx.unbounded_send(RemoteOutput::Ended(code));
                    }
                    crate::share::CLOSE => break,
                    _ => {}
                }
            }
            let _ = tx.unbounded_send(RemoteOutput::Ended(None));
        });
        self.track(task);
        cx.notify();
    }

    /// Shows the shared machine's desktop.
    fn show_shared_desktop(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let Some(j) = self.joined_mut(id) else { return };
        if j.desktop.is_none() {
            let (joined, mac, quality) = (j.joined.clone(), j.info.mac, j.quality);
            j.desktop = Some(cx.new(|cx| DesktopView::shared(engine, joined, mac, quality, window, cx)));
        }
        j.showing = Showing::Desktop;
        cx.notify();
    }

    /// The quality of a shared desktop shown here.
    pub(super) fn set_shared_quality(&mut self, id: &str, quality: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(j) = self.joined_mut(id) else { return };
        j.quality = quality;
        if let Some(d) = j.desktop.clone() {
            d.update(cx, |d, cx| d.set_quality(quality, window, cx));
        }
    }

    /// Opens one of the shared servers here, in the browser.
    pub(super) fn open_shared_server(&mut self, id: &str, port: u16, cx: &mut Context<Self>) {
        let runtime = self.engine.runtime().clone();
        let Some(j) = self.joined_mut(id) else { return };
        if let Some(Ok(local)) = j.servers.get(&port) {
            cx.open_url(&format!("http://localhost:{local}"));
            return;
        }
        let joined = j.joined.clone();
        let job = runtime.spawn(async move { joined.forward(port).await });
        let sid = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                if let Ok(local) = &result {
                    cx.open_url(&format!("http://localhost:{local}"));
                }
                if let Some(j) = this.joined_mut(&sid) {
                    j.servers.insert(port, result);
                }
                cx.notify();
            });
        });
        self.track(task);
    }

    /// Lets go of a share someone gave you.
    pub(super) fn leave_joined(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.joined.iter().position(|j| j.id == id) {
            let j = self.joined.remove(i);
            j.joined.leave();
        }
        if self.route == Route::Shared(id.to_string()) {
            self.navigate(Route::Overview, window, cx);
        }
        cx.notify();
    }


    /// A share someone gave you, drawn like your own: a machine's page, a
    /// session's, a desktop's.
    pub(super) fn render_joined(&mut self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(j) = self.joined.iter().find(|j| j.id == id) else {
            return div().size_full().flex().items_center().justify_center().text_color(theme.text_muted).child("That share is closed.").into_any_element();
        };
        if let Some(why) = j.ended.clone() {
            let leave = j.id.clone();
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    card(&theme)
                        .items_center()
                        .gap(px(10.))
                        .p(px(22.))
                        .max_w(px(420.))
                        .child(kit::color_tile("eye", 36.))
                        .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(why))
                        .child(Button::new("joined-close", "Close").icon("x").on_click(cx.listener(move |this, _, window, cx| this.leave_joined(&leave, window, cx)))),
                )
                .into_any_element();
        }
        match j.showing.clone() {
            Showing::Machine => self.joined_machine_page(id, cx),
            Showing::Session(name) => self.joined_session_page(id, &name, cx),
            Showing::Desktop => self.joined_desktop_page(id, cx),
        }
    }

    /// What this guest may do, in a few words ("Watching", "Typing").
    fn joined_mode(j: &JoinedShare) -> &'static str {
        if j.can_type { "Typing" } else { "Watching" }
    }

    /// One of a shared machine's sessions, or the session shared: like a
    /// session of your own, its terminal filling the page.
    fn joined_session_page(&mut self, id: &str, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(j) = self.joined.iter().find(|j| j.id == id) else { return div().into_any_element() };
        let session = j.sessions.iter().find(|s| s.name == name).cloned();
        let agent = session.as_ref().map(|s| s.agent.clone()).unwrap_or_else(|| j.info.agent.clone());
        let title = match &session {
            Some(s) if !s.title.is_empty() => s.title.clone(),
            Some(s) => agent_name(&s.agent).to_string(),
            None if !j.info.title.is_empty() && !j.info.whole_machine => j.info.title.clone(),
            None => agent_name(&agent).to_string(),
        };
        let machine = if j.info.whole_machine { j.info.title.clone() } else { j.info.machine.clone() };
        let (icon_name, (from, to)) = kit::agent_look(&agent);
        let status = match &session {
            Some(s) if s.status == "exited" => kit::pill("Exited", theme.text_placeholder).into_any_element(),
            Some(s) if s.busy => div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(theme.text_accent)
                .child(kit::spinner("joined-busy", 12., theme.accent))
                .child("Working")
                .into_any_element(),
            Some(s) => div().text_size(px(12.)).text_color(theme.text_placeholder).child(if s.agent == "shell" { "Idle" } else { "Waiting for you" }).into_any_element(),
            None => div().into_any_element(),
        };
        let can_type = j.can_type;
        let by = j.info.by.clone();
        let whole = j.info.whole_machine;
        let desktop = j.info.desktop != Desktop::None;
        let (sid, sid2) = (j.id.clone(), j.id.clone());
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .px(px(20.))
            .py(px(12.))
            .border_b_1()
            .border_color(theme.border_variant)
            .child(kit::tile(icon_name, from, to, 32.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title))
                    .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(format!("{} · {machine} · shared by {by}", agent_name(&agent)))),
            )
            .child(status)
            .child(
                div()
                    .id("joined-mode")
                    .child(kit::pill(Self::joined_mode(j), theme.accent))
                    .tooltip(move |window, cx| kit::Tooltip::view(if can_type { "Your keys go to their session, as them.".into() } else { format!("You're watching. Only {by} can type.").into() }, window, cx)),
            )
            .when(whole && desktop, |d| {
                d.child(Button::new("joined-desktop", "Desktop").icon("monitor").style(ButtonStyle::Ghost).on_click(cx.listener(move |this, _, window, cx| this.show_shared(&sid, Showing::Desktop, window, cx))))
            })
            .when(whole && j.can_manage && session.is_some(), |d| {
                let (sid, name) = (j.id.clone(), name.to_string());
                let exited = session.as_ref().is_some_and(|s| s.status == "exited");
                let (label, glyph, action) = if exited { ("Restart", "rotate-ccw", JoinedAction::Restart(name)) } else { ("End", "power", JoinedAction::End(name)) };
                d.child(Button::new("joined-end", label).icon(glyph).style(ButtonStyle::Ghost).on_click(cx.listener(move |this, _, window, cx| this.run_joined(&sid, action.clone(), window, cx))))
            })
            .when(!whole, |d| d.child(Button::new("joined-leave", "Leave").icon("x").style(ButtonStyle::Ghost).on_click(cx.listener(move |this, _, window, cx| this.leave_joined(&sid2, window, cx)))));
        let body = match j.open.iter().find(|o| o.name == name) {
            Some(o) if o.error.is_some() => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .text_color(theme.text_muted)
                .child(o.error.clone().unwrap_or_default())
                .into_any_element(),
            Some(o) => div().relative().flex_1().min_h(px(0.)).flex().child(o.terminal.clone()).into_any_element(),
            None => div().flex_1().into_any_element(),
        };
        div().size_full().flex().flex_col().child(header).child(body).into_any_element()
    }

    /// A shared machine's desktop, like your own machines' Desktop page.
    fn joined_desktop_page(&mut self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(j) = self.joined.iter().find(|j| j.id == id) else { return div().into_any_element() };
        let Some(d) = j.desktop.clone() else {
            return div().size_full().flex().items_center().justify_center().text_color(theme.text_muted).child("The desktop isn't shared anymore.").into_any_element();
        };
        let current = d.read(cx).quality();
        let qualities = host::QUALITIES.iter().enumerate().map(|(i, q)| {
            let (sid, q) = (j.id.clone(), *q);
            choice(&theme, ("desktop-quality", i), quality_label(q), current == q, true).on_click(cx.listener(move |this, _, window, cx| this.set_shared_quality(&sid, q, window, cx)))
        });
        let can_control = j.can_control;
        let by = j.info.by.clone();
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .px(px(20.))
            .py(px(12.))
            .border_b_1()
            .border_color(theme.border_variant)
            .child(kit::color_tile("monitor", 32.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Desktop"))
                    .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(format!("{} · shared by {} · {}", j.info.title, j.info.by, d.read(cx).status()))),
            )
            .child(div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(theme.text_muted).child("Quality").children(qualities))
            .child(
                div()
                    .id("joined-control")
                    .child(kit::pill(if can_control { "In control" } else { "View only" }, theme.accent))
                    .tooltip(move |window, cx| kit::Tooltip::view(if can_control { "Your mouse and keys work on their screen.".into() } else { format!("You're watching. Only {by} can use it.").into() }, window, cx)),
            );
        div().size_full().flex().flex_col().child(header).child(div().flex_1().min_h(px(0.)).child(d)).into_any_element()
    }

    /// A shared machine's page, like your own machines': what's shared and
    /// what you can do with it.
    fn joined_machine_page(&mut self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(j) = self.joined.iter().find(|j| j.id == id) else { return div().into_any_element() };
        let sid = j.id.clone();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if j.can_manage {
            let sid = sid.clone();
            buttons.push(Button::new("jm-terminal", "New terminal").icon("square-terminal").style(ButtonStyle::Primary).on_click(cx.listener(move |this, _, window, cx| this.run_joined(&sid, JoinedAction::Start("shell"), window, cx))).into_any_element());
        }
        if j.info.desktop != Desktop::None {
            let sid = sid.clone();
            buttons.push(Button::new("jm-desktop", "Show desktop").icon("monitor").style(if j.can_manage { ButtonStyle::Outlined } else { ButtonStyle::Primary }).on_click(cx.listener(move |this, _, window, cx| this.show_shared(&sid, Showing::Desktop, window, cx))).into_any_element());
        }
        {
            let sid = sid.clone();
            buttons.push(Button::new("jm-leave", "Leave").icon("x").on_click(cx.listener(move |this, _, window, cx| this.leave_joined(&sid, window, cx))).into_any_element());
        }
        let header = div()
            .flex()
            .flex_wrap()
            .items_start()
            .justify_between()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .child(kit::color_tile("monitor", 40.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(div().flex().items_center().gap(px(10.)).child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(j.info.title.clone())).child(kit::pill("Shared with you", theme.accent)))
                            .child(div().text_size(px(12.)).text_color(theme.text_muted).child(format!("Shared by {} · ends in {}", j.info.by, time_left(j.info.ends_at)))),
                    ),
            )
            .child(div().flex().gap(px(6.)).children(buttons));

        // What you can do here, as the sharer set it.
        let can = |glyph: &'static str, text: String| div().flex().items_center().gap(px(8.)).text_size(px(12.5)).text_color(theme.text).child(icon(glyph, 14., theme.text_muted)).child(text);
        let mut allowed = div().flex().flex_col().gap(px(8.)).p(px(14.));
        allowed = allowed.child(can(if j.can_type { "keyboard" } else { "eye" }, if j.can_type { "Watch and type in its sessions".into() } else { format!("Watch its sessions (only {} types)", j.info.by) }));
        if j.can_manage {
            allowed = allowed.child(can("square-terminal", "Start, end and restart its sessions".into()));
        }
        match j.info.desktop {
            Desktop::None => {}
            Desktop::View => allowed = allowed.child(can("monitor", "Watch its screen".into())),
            Desktop::Control if j.can_control => allowed = allowed.child(can("monitor", "Watch and use its screen".into())),
            Desktop::Control => allowed = allowed.child(can("monitor", format!("Watch its screen, and use it when {} lets you", j.info.by))),
        }
        if !j.info.ports.is_empty() {
            let n = j.info.ports.len();
            allowed = allowed.child(can("network", format!("Open {n} of its servers here")));
        }

        // Its sessions, like a machine's.
        let sessions: Vec<AnyElement> = j
            .sessions
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let (icon_name, (from, to)) = kit::agent_look(&x.agent);
                let title = if x.title.is_empty() { agent_name(&x.agent).to_string() } else { x.title.clone() };
                let state = if x.status == "exited" {
                    div().text_size(px(12.)).text_color(theme.text_placeholder).child("Exited").into_any_element()
                } else if x.busy {
                    div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(theme.text_accent).child(kit::spinner(("jm-spin", i), 12., theme.accent)).child("Working").into_any_element()
                } else {
                    div().text_size(px(12.)).text_color(theme.text_placeholder).child(if x.agent == "shell" { "Idle" } else { "Waiting for you" }).into_any_element()
                };
                let (sid, name) = (sid.clone(), x.name.clone());
                div()
                    .id(("jm-session", i))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(10.))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.ghost_hover))
                    .child(kit::tile(icon_name, from, to, 26.))
                    .child(div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(1.)).child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.text).child(title)).child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(agent_name(&x.agent))))
                    .child(state)
                    .on_click(cx.listener(move |this, _, window, cx| this.show_shared(&sid, Showing::Session(name.clone()), window, cx)))
                    .into_any_element()
            })
            .collect();
        let sessions = if sessions.is_empty() {
            card(&theme).p(px(16.)).flex_row().items_center().gap(px(12.)).child(kit::color_tile("square-terminal", 28.)).child(div().text_size(px(12.5)).text_color(theme.text_muted).child(format!("No sessions are running on {} right now. New ones show up here.", j.info.title))).into_any_element()
        } else {
            card(&theme).children(sessions).into_any_element()
        };

        // Its servers, to open here.
        let servers: Vec<AnyElement> = j
            .info
            .ports
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let state = match j.servers.get(&p.port) {
                    Some(Ok(local)) => format!("Open here at localhost:{local}"),
                    Some(Err(e)) => e.clone(),
                    None => format!("Port {} on {}", p.port, j.info.title),
                };
                let (sid, port) = (sid.clone(), p.port);
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(10.))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .child(kit::color_tile("network", 26.))
                    .child(div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(1.)).child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child(if p.name.is_empty() { format!("Port {}", p.port) } else { p.name.clone() })).child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(state)))
                    .child(Button::new(("jm-open", i), "Open").icon("external-link").on_click(cx.listener(move |this, _, _, cx| this.open_shared_server(&sid, port, cx))))
                    .into_any_element()
            })
            .collect();

        super::overview::page("shared-page", &self.page_scroll)
            .child(header)
            .child(div().flex().flex_col().gap(px(10.)).child(section_title(&theme, "What you can do", None)).child(card(&theme).child(allowed)))
            .child(div().flex().flex_col().gap(px(10.)).child(section_title(&theme, "Agent sessions", Some(j.sessions.len()))).child(sessions))
            .when(!servers.is_empty(), |d| d.child(div().flex().flex_col().gap(px(10.)).child(section_title(&theme, "Servers", Some(servers.len()))).child(card(&theme).children(servers))))
            .into_any_element()
    }

    /// The sidebar's "Shared with you": each share like one of your machines
    /// (or sessions), a shared machine's sessions, desktop and servers under it.
    pub(super) fn joined_rows(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        for j in &self.joined {
            let here = self.route == Route::Shared(j.id.clone());
            let key = format!("shared:{}", j.id);
            let open = !self.collapsed.contains(&key);
            rows.push(self.joined_row(theme, j, here && (j.showing == Showing::Machine || !j.info.whole_machine), open, cx));
            if !j.info.whole_machine || !open || j.ended.is_some() {
                continue;
            }
            for s in &j.sessions {
                let selected = here && j.showing == Showing::Session(s.name.clone());
                let (icon_name, (from, to)) = kit::agent_look(&s.agent);
                let title = if s.title.is_empty() { agent_name(&s.agent).to_string() } else { s.title.clone() };
                let exited = s.status == "exited";
                let (sid, name) = (j.id.clone(), s.name.clone());
                let (msid, mname) = (j.id.clone(), s.name.clone());
                rows.push(
                    nested_row(theme, SharedString::from(format!("side-js-{}-{}", j.id, s.name)), selected)
                        .child(kit::tile(icon_name, from, to, 16.))
                        .child(div().w(px((self.row_width() - 110.).max(40.))).truncate().text_size(px(12.5)).text_color(if exited { theme.text_placeholder } else if selected { theme.text } else { theme.text_muted }).child(title))
                        .child(div().flex_1())
                        .when(s.busy && !exited, |d| d.child(kit::spinner(SharedString::from(format!("side-js-spin-{}-{}", j.id, s.name)), 11., theme.accent)))
                        .when(exited, |d| d.child(div().text_size(px(11.)).text_color(theme.text_placeholder).child("exited")))
                        .on_mouse_down(gpui::MouseButton::Right, super::menu::on_right_click(move || super::menu::MenuTarget::JoinedSession(msid.clone(), mname.clone()), cx))
                        .on_click(cx.listener(move |this, _, window, cx| this.show_shared(&sid, Showing::Session(name.clone()), window, cx)))
                        .into_any_element(),
                );
            }
            if j.info.desktop != Desktop::None {
                let selected = here && j.showing == Showing::Desktop;
                let (sid, msid) = (j.id.clone(), j.id.clone());
                rows.push(
                    nested_row(theme, SharedString::from(format!("side-jd-{}", j.id)), selected)
                        .child(kit::color_tile("monitor", 16.))
                        .child(div().flex_1().text_size(px(12.5)).text_color(if selected { theme.text } else { theme.text_muted }).child("Desktop"))
                        .child(div().text_size(px(10.5)).text_color(theme.text_placeholder).child(if j.can_control { "control" } else { "view" }))
                        .on_mouse_down(gpui::MouseButton::Right, super::menu::on_right_click(move || super::menu::MenuTarget::JoinedDesktop(msid.clone()), cx))
                        .on_click(cx.listener(move |this, _, window, cx| this.show_shared(&sid, Showing::Desktop, window, cx)))
                        .into_any_element(),
                );
            }
            for p in &j.info.ports {
                let (sid, port) = (j.id.clone(), p.port);
                let local = j.servers.get(&p.port).and_then(|r| r.as_ref().ok()).copied();
                rows.push(
                    nested_row(theme, SharedString::from(format!("side-jp-{}-{}", j.id, p.port)), false)
                        .child(icon("network", 14., theme.text_muted))
                        .child(div().w(px((self.row_width() - 130.).max(40.))).truncate().text_size(px(12.5)).text_color(theme.text_muted).child(if p.name.is_empty() { format!("Port {}", p.port) } else { p.name.clone() }))
                        .child(div().flex_1())
                        .child(div().font_family(mono_font()).text_size(px(10.5)).text_color(theme.text_placeholder).child(match local {
                            Some(l) => format!(":{l}"),
                            None => format!(":{}", p.port),
                        }))
                        .tooltip(|window, cx| kit::Tooltip::view("Open it here, in your browser".into(), window, cx))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_shared_server(&sid, port, cx)))
                        .into_any_element(),
                );
            }
        }
        rows
    }

    /// A share in the sidebar, shaped like a machine row: its tile with an eye
    /// in the corner, who shared it and what you can do; a machine's opens and
    /// closes what's under it.
    fn joined_row(&self, theme: &Theme, j: &JoinedShare, selected: bool, open: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = j.id.clone();
        let (tile, title) = if j.info.whole_machine {
            (kit::color_tile("monitor", 30.), j.info.title.clone())
        } else {
            let (icon_name, (from, to)) = kit::agent_look(&j.info.agent);
            (kit::tile(icon_name, from, to, 30.), if j.info.title.is_empty() { agent_name(&j.info.agent).to_string() } else { j.info.title.clone() })
        };
        let doing = if j.ended.is_some() {
            "ended"
        } else if j.can_control && j.info.desktop == Desktop::Control {
            "in control"
        } else if j.can_type {
            "typing"
        } else {
            "watching"
        };
        let toggle = j.info.whole_machine && j.ended.is_none();
        // Row chrome: margins, padding, tile, gap, and the toggle.
        let text_w = (self.row_width() - 20. - 16. - 30. - 10. - if toggle { 32. } else { 0. }).max(40.);
        let (menu_id, key) = (j.id.clone(), format!("shared:{}", j.id));
        div()
            .id(SharedString::from(format!("side-j-{}", j.id)))
            .group("joined-row")
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(44.))
            .mx(px(10.))
            .px(px(8.))
            .rounded(px(9.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(
                div().relative().flex_none().child(tile).child(
                    div()
                        .absolute()
                        .right(px(-4.))
                        .bottom(px(-4.))
                        .size(px(15.))
                        .rounded_full()
                        .bg(theme.sidebar)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(if j.ended.is_some() { "x" } else { "eye" }, 10., if j.ended.is_some() { theme.text_placeholder } else { theme.text_accent })),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .gap(px(1.))
                    .child(div().w(px(text_w)).truncate().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.text).child(title))
                    .child(div().w(px(text_w)).truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!("from {} · {doing}", j.info.by))),
            )
            .when(toggle, |d| {
                d.child(
                    div()
                        .id(SharedString::from(format!("side-j-toggle-{}", j.id)))
                        .flex_none()
                        .p(px(3.))
                        .rounded(px(5.))
                        .invisible()
                        .group_hover("joined-row", |st| st.visible())
                        .hover(|st| st.bg(theme.ghost_hover))
                        .child(icon(if open { "chevron-down" } else { "chevron-right" }, 13., theme.text_placeholder))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if !this.collapsed.remove(&key) {
                                this.collapsed.insert(key.clone());
                            }
                            cx.notify();
                        })),
                )
            })
            .on_mouse_down(gpui::MouseButton::Right, super::menu::on_right_click(move || super::menu::MenuTarget::Joined(menu_id.clone()), cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                let showing = match this.joined.iter().find(|j| j.id == id) {
                    Some(j) if j.info.whole_machine => Showing::Machine,
                    _ => Showing::Session(String::new()),
                };
                this.show_shared(&id, showing, window, cx)
            }))
            .into_any_element()
    }

    /// A share's right-click menu: like your own machine's and sessions',
    /// with what this guest isn't let do dimmed.
    pub(super) fn joined_menu(&self, target: &super::menu::MenuTarget) -> Vec<JoinedItem> {
        use super::menu::MenuTarget;
        let id = match target {
            MenuTarget::Joined(id) | MenuTarget::JoinedSession(id, _) | MenuTarget::JoinedDesktop(id) => id,
            _ => return Vec::new(),
        };
        let Some(j) = self.joined.iter().find(|j| &j.id == id) else { return Vec::new() };
        let live = j.ended.is_none();
        let item = |label: String, glyph: &'static str, action: JoinedAction| JoinedItem::Do { label, glyph, action, danger: false, picked: false };
        // Starting and ending sessions: dimmed unless let.
        let managed = |label: String, glyph: &'static str, action: JoinedAction| if j.can_manage && live { item(label, glyph, action) } else { JoinedItem::Off { label, glyph } };
        let mut items = Vec::new();
        match target {
            MenuTarget::JoinedSession(_, name) => {
                let exited = j.sessions.iter().any(|s| &s.name == name && s.status == "exited");
                items.push(item("Open".into(), "square-terminal", JoinedAction::Show(Showing::Session(name.clone()))));
                items.push(JoinedItem::Separator);
                if exited {
                    items.push(managed("Restart".into(), "rotate-ccw", JoinedAction::Restart(name.clone())));
                }
                items.push(managed("End session".into(), "power", JoinedAction::End(name.clone())));
            }
            MenuTarget::JoinedDesktop(_) => {
                items.push(item("Open".into(), "monitor", JoinedAction::Show(Showing::Desktop)));
                items.push(JoinedItem::Separator);
                items.push(JoinedItem::Note("Quality".into()));
                let current = j.quality;
                for q in host::QUALITIES {
                    items.push(JoinedItem::Do { label: quality_label(q).into(), glyph: "", action: JoinedAction::Quality(q), danger: false, picked: q == current });
                }
            }
            _ if j.info.whole_machine => {
                items.push(managed("New terminal".into(), "square-terminal", JoinedAction::Start("shell")));
                for a in crate::fleet::sessions::AGENTS.iter().filter(|a| a.id != "shell" && j.info.agents.iter().any(|i| i == a.id)) {
                    items.push(match managed(format!("New {} session", a.name), "", JoinedAction::Start(a.id)) {
                        JoinedItem::Do { label, action, .. } => JoinedItem::Agent { label, agent: a.id, action: Some(action) },
                        _ => JoinedItem::Agent { label: format!("New {} session", a.name), agent: a.id, action: None },
                    });
                }
                if j.info.desktop != Desktop::None {
                    items.push(item("Show desktop".into(), "monitor", JoinedAction::Show(Showing::Desktop)));
                }
                for p in &j.info.ports {
                    let name = if p.name.is_empty() { format!("port {}", p.port) } else { p.name.clone() };
                    items.push(item(format!("Open {name} here"), "external-link", JoinedAction::Server(p.port)));
                }
                items.push(JoinedItem::Separator);
                items.push(item("Open".into(), "monitor", JoinedAction::Show(Showing::Machine)));
                items.push(item("Refresh".into(), "refresh-cw", JoinedAction::Refresh));
                if live && !j.can_manage {
                    items.push(JoinedItem::Note(format!("Ask {} to let you start sessions.", j.info.by)));
                }
            }
            _ => items.push(item("Open".into(), "square-terminal", JoinedAction::Show(Showing::Session(String::new())))),
        }
        items.push(JoinedItem::Separator);
        items.push(JoinedItem::Do { label: if live { "Leave share".into() } else { "Close".into() }, glyph: "x", action: JoinedAction::Leave, danger: true, picked: false });
        items
    }

    /// Asks the sharing computer to start, end or restart a session; a
    /// session started opens.
    fn shared_call(&mut self, id: &str, req: serde_json::Value, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(joined) = self.joined.iter().find(|j| j.id == id).map(|j| j.joined.clone()) else { return };
        let job = self.engine.runtime().spawn(async move { joined.call(req).await });
        let sid = id.to_string();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(name) if open => this.show_shared(&sid, Showing::Session(name), window, cx),
                Ok(_) => {
                    if let Some(j) = this.joined.iter().find(|j| j.id == sid) {
                        j.joined.refresh_sessions();
                    }
                }
                Err(e) => this.notify_user(e, true, cx),
            });
        });
        self.track(task);
    }

    /// Runs one of a share's menu items.
    pub(super) fn run_joined(&mut self, id: &str, action: JoinedAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            JoinedAction::Show(showing) => self.show_shared(id, showing, window, cx),
            JoinedAction::Quality(q) => {
                self.show_shared(id, Showing::Desktop, window, cx);
                self.set_shared_quality(id, q, window, cx);
            }
            JoinedAction::Server(port) => self.open_shared_server(id, port, cx),
            JoinedAction::Start(agent) => self.shared_call(id, json!({ "op": "start", "agent": agent }), true, window, cx),
            JoinedAction::End(name) => {
                // Not showing a session that's gone.
                if let Some(j) = self.joined_mut(id)
                    && j.showing == Showing::Session(name.clone())
                {
                    j.showing = Showing::Machine;
                }
                self.shared_call(id, json!({ "op": "end", "session": name }), false, window, cx)
            }
            JoinedAction::Restart(name) => self.shared_call(id, json!({ "op": "restart", "session": name }), true, window, cx),
            JoinedAction::Refresh => {
                if let Some(j) = self.joined.iter().find(|j| j.id == id) {
                    j.joined.refresh_sessions();
                }
            }
            JoinedAction::Leave => self.leave_joined(id, window, cx),
        }
    }
}

/// A line of a share's right-click menu.
pub enum JoinedItem {
    Do { label: String, glyph: &'static str, action: JoinedAction, danger: bool, picked: bool },
    /// Starting one of the machine's agent CLIs (dimmed without an action).
    Agent { label: String, agent: &'static str, action: Option<JoinedAction> },
    /// Not let: shown dimmed.
    Off { label: String, glyph: &'static str },
    Note(String),
    Separator,
}

/// What a share's menu item does.
#[derive(Clone, Debug)]
pub enum JoinedAction {
    Show(Showing),
    Quality(&'static str),
    Server(u16),
    /// Start a session with this agent CLI ("shell" for a terminal).
    Start(&'static str),
    End(String),
    Restart(String),
    Refresh,
    Leave,
}

/// "Low", "Medium", "High", "Max".
fn quality_label(q: &str) -> &'static str {
    match q {
        "low" => "Low",
        "high" => "High",
        "max" => "Max",
        _ => "Medium",
    }
}

/// A row under a share in the sidebar, like a machine's sessions.
fn nested_row(theme: &Theme, id: SharedString, selected: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(26.))
        .mx(px(10.))
        .pl(px(22.))
        .pr(px(8.))
        .rounded(px(7.))
        .cursor_pointer()
        .when(selected, |d| d.bg(theme.element_selected))
        .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
}
