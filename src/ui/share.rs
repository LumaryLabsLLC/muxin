//! Sharing in the window: the Share dialog and the session header's button,
//! letting guests in, and sessions others shared with you (joined from a
//! link). The sharing itself is crate::share.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{AnyElement, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};
use tokio::sync::mpsc;

use super::dialogs::{DialogEvent, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, card, icon};
use super::root::{Modal, Route, Root};
use super::terminal::{RemoteIo, RemoteOutput, TerminalView};
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::Engine;
use crate::fleet::sessions::SessionInfo;
use crate::share::{guest, host};

/// How long a share lasts: what the dialog offers.
const DURATIONS: &[(&str, u64)] = &[("15 min", 15 * 60), ("1 hour", 3600), ("8 hours", 8 * 3600), ("1 day", 24 * 3600)];

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

fn fact(theme: &Theme, glyph: &'static str, text: impl Into<SharedString>) -> impl IntoElement {
    div().flex().items_center().gap(px(10.)).text_size(px(13.)).text_color(theme.text).child(icon(glyph, 15., theme.text_muted)).child(text.into())
}

// ---------------------------------------------------------------------------
// Sharing a session

/// Sharing a session: one click makes the link and copies it (you let each
/// person in, they watch, for an hour); what they can do and for how long
/// change here afterwards, while it runs.
pub struct ShareDialog {
    engine: Arc<Engine>,
    target: host::Target,
    /// The share, once made.
    share: Option<String>,
    creating: bool,
    copied: bool,
    error: Option<String>,
    focus: FocusHandle,
}

impl EventEmitter<DialogEvent> for ShareDialog {}

impl ShareDialog {
    /// The share this session has, or a new one made right away.
    pub fn new(engine: Arc<Engine>, target: host::Target, existing: Option<String>, cx: &mut Context<Self>) -> Self {
        let mut dialog = ShareDialog { engine, target, share: existing, creating: false, copied: false, error: None, focus: cx.focus_handle() };
        if dialog.share.is_none() {
            dialog.create(cx);
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
        let options = host::Options { approve: true, can_type: false, ttl: Duration::from_secs(3600) };
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

    fn render_share(&mut self, theme: &Theme, view: host::ShareView, cx: &mut Context<Self>) -> AnyElement {
        let link = view.link.clone();
        let (id, id2, id3) = (view.id.clone(), view.id.clone(), view.id.clone());
        let (approve, can_type) = (view.approve, view.can_type);
        let left = view.ends_at - chrono::Utc::now().timestamp();
        let typing_row = if approve {
            kit::checkbox(theme, "share-type", can_type, "They can type", move |on, _, _| host::set_options(&id2, true, *on)).into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.5))
                .text_color(theme.text_placeholder)
                .child(div().size(px(15.)).rounded(px(4.)).border_1().border_color(theme.border_variant))
                .child("They can type")
                .into_any_element()
        };
        frame(theme, format!("Share {}", self.target.title), Some(if self.copied { "Link copied. Send it to anyone: it opens in their browser or in Muxin." } else { "Send the link to anyone: it opens in their browser or in Muxin." }.into()))
            .w(px(540.))
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
                            .child(div().pl(px(23.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(if approve { "You see who's asking, and their code." } else { "Anyone with the link can watch." })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(typing_row)
                            .child(div().pl(px(23.)).text_size(px(11.5)).text_color(if approve && can_type { theme.warning } else { theme.text_placeholder }).child(if approve {
                                "Their keys go to this session, as you."
                            } else {
                                "Only for people you let in."
                            })),
                    )
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
                                div()
                                    .id(("share-ttl", i))
                                    .px(px(10.))
                                    .h(px(26.))
                                    .flex()
                                    .items_center()
                                    .rounded(px(6.))
                                    .border_1()
                                    .cursor_pointer()
                                    .text_size(px(12.))
                                    .border_color(if picked { theme.accent } else { theme.border_variant })
                                    .bg(if picked { theme.accent.opacity(0.12) } else { theme.element })
                                    .child(*label)
                                    .on_click(move |_, _, _| host::set_ends_in(&id, Duration::from_secs(secs)))
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

    fn join(&mut self, cx: &mut Context<Self>) {
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
                        guest::Update::Admitted { can_type } => {
                            let info = match &this.stage {
                                Stage::Joined { info, .. } => info.clone().unwrap_or_default(),
                                _ => Default::default(),
                            };
                            done = Some((info, can_type));
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
                if let Some((info, can_type)) = done {
                    let handle = JoinedHandle { joined: joined.clone(), updates: Arc::new(Mutex::new(Some(rx))), info, can_type };
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
}

impl Render for JoinDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        match &self.stage {
            Stage::Form | Stage::Connecting => {
                let connecting = matches!(self.stage, Stage::Connecting);
                frame(&theme, "Join a shared session", Some("Paste the link someone sent you.".into()))
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
                let title = info.as_ref().map(|i| if i.title.is_empty() { "a session".to_string() } else { i.title.clone() }).unwrap_or_else(|| "a session".into());
                let sub = info.as_ref().map(|i| format!("{} on {} · shared by {}", agent_name(&i.agent), i.machine, i.by));
                let (a, b) = code.split_at(3.min(code.len()));
                frame(&theme, format!("Join {title}"), sub.map(SharedString::from))
                    .children(info.as_ref().map(|i| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(fact(&theme, if i.can_type { "keyboard" } else { "eye" }, if i.can_type { "You can watch and type" } else { "You can watch" }))
                            .child(fact(&theme, "clock", format!("Ends in {}", time_left(i.ends_at))))
                    }))
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

/// Keys for a joined session (dropped unless its host lets us type).
struct GuestIo(Arc<guest::Joined>);

impl RemoteIo for GuestIo {
    fn write(&self, bytes: &[u8]) {
        self.0.send_keys(bytes);
    }

    fn resize(&self, _: u16, _: u16) {}

    fn close(&self) {}
}

/// A session someone shared with you, open here.
pub struct JoinedSession {
    pub id: String,
    pub info: guest::Info,
    joined: Arc<guest::Joined>,
    pub terminal: Entity<TerminalView>,
    pub can_type: bool,
    /// Why it ended, once it has.
    pub ended: Option<String>,
}

impl Root {
    /// The Share dialog for a session: its link when it's shared already.
    pub(super) fn show_share(&mut self, session_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(info) = self.snapshot.sessions.iter().find(|x| x.id == session_id).cloned() else { return };
        let machine_name = self.snapshot.machines.iter().find(|m| m.id == info.machine_id).map(|m| m.name.clone()).unwrap_or_else(|| info.machine_id.clone());
        let by = self.snapshot.github.login.clone().filter(|l| !l.is_empty()).or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "Someone".into());
        let target = host::Target {
            session_id: info.id.clone(),
            machine_id: info.machine_id.clone(),
            name: info.id.split_once('/').map(|(_, n)| n.to_string()).unwrap_or_default(),
            title: info.title.clone(),
            agent: info.agent.clone(),
            machine_name,
            by,
        };
        let existing = self.share_of(session_id).map(|s| s.id.clone());
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| ShareDialog::new(engine, target, existing, cx));
        self.show_modal(Modal::Share(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    pub(super) fn share_of(&self, session_id: &str) -> Option<&host::ShareView> {
        self.shares.iter().find(|s| s.target.session_id == session_id)
    }

    /// Shares changed (a guest knocked, came in, left): show it, and say
    /// who's asking to join when they first do.
    pub(super) fn shares_changed(&mut self, cx: &mut Context<Self>) {
        self.shares = host::list();
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

    /// The session header's Share button, or what's going on with its share.
    pub(super) fn share_button(&self, info: &SessionInfo, cx: &mut Context<Self>) -> AnyElement {
        let sid = info.id.clone();
        let Some(share) = self.share_of(&info.id) else {
            return Button::new("session-share", "Share")
                .icon("link")
                .tooltip("Share this session by link")
                .on_click(cx.listener(move |this, _, window, cx| this.show_share(&sid, window, cx)))
                .into_any_element();
        };
        let waiting = share.guests.iter().filter(|g| g.state == "waiting").count();
        let inside: Vec<&host::GuestView> = share.guests.iter().filter(|g| g.state == "in").collect();
        let typing = inside.iter().any(|g| g.can_type);
        let what = match (inside.len(), waiting) {
            (_, w) if w > 0 => format!("{w} waiting"),
            (0, _) => "no one yet".to_string(),
            (1, _) => format!("{} {}", inside[0].name, if typing { "typing" } else { "watching" }),
            (n, _) => format!("{n} watching"),
        };
        Button::new("session-shared", format!("Shared · {what}"))
            .icon(if typing { "keyboard" } else { "eye" })
            .style(ButtonStyle::Outlined)
            .tooltip("Who's here, the link, and stopping")
            .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| this.open_menu(super::menu::MenuTarget::Share(sid.clone()), e.position(), cx)))
            .into_any_element()
    }

    /// Above a shared session's terminal: each guest waiting to be let in.
    pub(super) fn knock_cards(&self, session_id: &str, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let Some(share) = self.share_of(session_id) else { return Vec::new() };
        share
            .guests
            .iter()
            .filter(|g| g.state == "waiting")
            .map(|g| {
                let (share_id, guest_id) = (share.id.clone(), g.id);
                let share_id2 = share_id.clone();
                let (a, b) = g.code.split_at(3.min(g.code.len()));
                let how = if g.client == "browser" { "In a browser" } else { "In Muxin" };
                let wants = if share.can_type { "wants to join" } else { "wants to watch" };
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
                    .into_any_element()
            })
            .collect()
    }

    /// The Join dialog (from a link opened, or the command palette).
    pub(super) fn show_join(&mut self, link: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let name = Some(self.engine.settings().share.name).filter(|n| !n.is_empty()).or_else(|| self.snapshot.github.login.clone()).unwrap_or_default();
        let dialog = cx.new(|cx| JoinDialog::new(engine, link, name, window, cx));
        self.show_modal(Modal::Join(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// A share joined and let in: its terminal, fed from now on, and its page.
    pub(super) fn open_joined(&mut self, handle: &JoinedHandle, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut updates) = handle.updates.lock().unwrap().take() else { return };
        self.joined_count += 1;
        let id = format!("shared-{}", self.joined_count);
        let (tx, output) = futures::channel::mpsc::unbounded();
        let io: Arc<dyn RemoteIo> = Arc::new(GuestIo(handle.joined.clone()));
        let terminal = cx.new(|cx| TerminalView::guest(io, output, (100, 30), window, cx));
        self.joined.push(JoinedSession { id: id.clone(), info: handle.info.clone(), joined: handle.joined.clone(), terminal, can_type: handle.can_type, ended: None });
        let sid = id.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Some(u) = updates.recv().await {
                let mut fed = true;
                match u {
                    guest::Update::Output(b) => fed = tx.unbounded_send(RemoteOutput::Bytes(b)).is_ok(),
                    guest::Update::Size(c, r) => {
                        let _ = tx.unbounded_send(RemoteOutput::Resize(c, r));
                        let _ = this.update(cx, |_, cx| cx.notify());
                    }
                    guest::Update::Typing(allowed) => {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(j) = this.joined.iter_mut().find(|j| j.id == sid) {
                                j.can_type = allowed;
                            }
                            this.notify_user(if allowed { "You can type now." } else { "Typing is off. You're watching." }, false, cx);
                        });
                    }
                    guest::Update::Exited(code) => fed = tx.unbounded_send(RemoteOutput::Ended(code)).is_ok(),
                    guest::Update::Ended(why) => {
                        let _ = tx.unbounded_send(RemoteOutput::Ended(None));
                        let _ = this.update(cx, |this, cx| {
                            if let Some(j) = this.joined.iter_mut().find(|j| j.id == sid) {
                                j.ended = Some(why);
                            }
                            cx.notify();
                        });
                        return;
                    }
                    _ => {}
                }
                if !fed {
                    break;
                }
            }
        });
        self.track(task);
        self.navigate(Route::Shared(id), window, cx);
    }

    /// Lets go of a session shared with you.
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

    /// A session someone shared with you.
    pub(super) fn render_joined(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(j) = self.joined.iter().find(|j| j.id == id) else {
            return div().size_full().flex().items_center().justify_center().text_color(theme.text_muted).child("That share is closed.").into_any_element();
        };
        let (icon_name, (from, to)) = kit::agent_look(&j.info.agent);
        let title = if j.info.title.is_empty() { agent_name(&j.info.agent).to_string() } else { j.info.title.clone() };
        let leave = j.id.clone();
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
                    .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(format!("{} · {} · shared by {}", agent_name(&j.info.agent), j.info.machine, j.info.by))),
            )
            .when(j.ended.is_none(), |d| {
                d.child(kit::pill(if j.can_type { "Typing" } else { "Watching" }, theme.accent))
                    .child(div().text_size(px(12.)).text_color(theme.text_muted).child(format!("{} left", time_left(j.info.ends_at))))
            })
            .child(Button::new("joined-leave", if j.ended.is_some() { "Close" } else { "Leave" }).icon("x").style(ButtonStyle::Outlined).on_click(cx.listener(move |this, _, window, cx| this.leave_joined(&leave, window, cx))));
        let body = match &j.ended {
            Some(why) => div()
                .flex_1()
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
                        .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(why.clone())),
                )
                .into_any_element(),
            None => {
                let (cols, rows) = j.terminal.read(cx).fixed_size().unwrap_or((100, 30));
                let (w, h) = TerminalView::pixel_size(cols, rows, window, cx);
                let note = if j.can_type { "You can type. Keys go to their session.".to_string() } else { format!("You're watching. Only {} can type.", j.info.by) };
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(16.))
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_hidden()
                            .child(div().w(px(w)).h(px(h)).rounded(px(8.)).overflow_hidden().border_1().border_color(theme.border_variant).child(j.terminal.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(12.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .bg(theme.surface)
                            .text_size(px(12.))
                            .text_color(theme.text_muted)
                            .child(icon(if j.can_type { "keyboard" } else { "lock" }, 13., theme.text_muted))
                            .child(div().flex_1().child(note))
                            .child(format!("Shown at {}'s size", j.info.by)),
                    )
                    .into_any_element()
            }
        };
        div().size_full().flex().flex_col().child(header).child(body).into_any_element()
    }

    /// The sidebar's "Shared with you" rows.
    pub(super) fn joined_rows(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.joined
            .iter()
            .map(|j| {
                let id = j.id.clone();
                let selected = self.route == Route::Shared(j.id.clone());
                let (icon_name, (from, to)) = kit::agent_look(&j.info.agent);
                let title = if j.info.title.is_empty() { agent_name(&j.info.agent).to_string() } else { j.info.title.clone() };
                let text_w = (self.sidebar_width - 20. - 22. - 8. - 16. - 8. - 24.).max(40.);
                div()
                    .id(SharedString::from(format!("side-j-{}", j.id)))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(30.))
                    .mx(px(10.))
                    .pl(px(10.))
                    .pr(px(8.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.element_selected))
                    .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                    .child(kit::tile(icon_name, from, to, 16.))
                    .child(
                        div()
                            .w(px(text_w))
                            .flex()
                            .flex_col()
                            .child(div().w(px(text_w)).truncate().text_size(px(12.5)).text_color(if selected { theme.text } else { theme.text_muted }).child(title))
                            .child(div().w(px(text_w)).truncate().text_size(px(11.)).text_color(theme.text_placeholder).child(format!("from {}", j.info.by))),
                    )
                    .child(div().flex_1())
                    .child(icon(if j.ended.is_some() { "x" } else { "eye" }, 12., theme.text_accent))
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Shared(id.clone()), window, cx)))
                    .into_any_element()
            })
            .collect()
    }
}
