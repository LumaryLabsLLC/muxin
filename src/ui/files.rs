//! Copying files between machines: a dialog with the machine the files are
//! on (left) and the one they go to (right), each a folder you can browse.

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, ScrollHandle, SharedString, Subscription, Window, div, prelude::*,
    px,
};

use super::dialogs::DialogEvent;
use super::format;
use super::github::Load;
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, icon};
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::Engine;
use crate::fleet::files::{CopyRequest, Listing};

#[derive(Clone, Copy, PartialEq)]
enum Side {
    From,
    To,
}

/// One side: a machine and a folder on it.
struct Pane {
    machine: String,
    path: Entity<TextInput>,
    listing: Load<Listing>,
    /// Ignores a listing that arrives after the pane moved on.
    asked: u64,
    scroll: ScrollHandle,
}

pub struct CopyDialog {
    engine: Arc<Engine>,
    /// (id, name, online) of the machines that can be browsed.
    machines: Vec<(String, String, bool)>,
    from: Pane,
    to: Pane,
    /// Names in the From folder to copy.
    selected: BTreeSet<String>,
    show_hidden: bool,
    error: Option<String>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for CopyDialog {}

impl Focusable for CopyDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Where the dialog starts: a machine and folder for each side, and what's
/// already picked (files dropped from Finder).
pub struct CopyStart {
    pub from: String,
    pub from_dir: String,
    pub selected: Vec<String>,
    pub to: String,
    pub to_dir: String,
}

impl CopyDialog {
    pub fn new(engine: Arc<Engine>, start: CopyStart, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snap = engine.current();
        let mut machines: Vec<(String, String, bool)> = snap
            .machines
            .iter()
            .filter(|m| !m.host.is_empty())
            .map(|m| (m.id.clone(), m.name.clone(), !m.is_down()))
            .collect();
        // This computer first.
        machines.sort_by_key(|(id, _, _)| !snap.machines.iter().any(|m| &m.id == id && m.local));
        let input = |cx: &mut Context<Self>, text: &str| cx.new(|cx| TextInput::new(cx, "~/folder").mono().with_text(text.to_string()));
        let (from_path, to_path) = (input(cx, &start.from_dir), input(cx, &start.to_dir));
        let on_path = |side: Side| {
            move |this: &mut Self, _: &Entity<TextInput>, e: &InputEvent, _: &mut Window, cx: &mut Context<Self>| match e {
                InputEvent::Submit => {
                    let path = this.pane(side).path.read(cx).text().trim().to_string();
                    this.load(side, path, cx);
                }
                InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
                InputEvent::Changed => {}
            }
        };
        let subs = vec![cx.subscribe_in(&from_path, window, on_path(Side::From)), cx.subscribe_in(&to_path, window, on_path(Side::To))];
        let pane = |machine: String, path: Entity<TextInput>| Pane { machine, path, listing: Load::Loading, asked: 0, scroll: ScrollHandle::new() };
        let mut dialog = CopyDialog {
            engine,
            machines,
            from: pane(start.from, from_path),
            to: pane(start.to, to_path),
            selected: start.selected.into_iter().collect(),
            show_hidden: false,
            error: None,
            focus: cx.focus_handle(),
            _subscriptions: subs,
        };
        dialog.load(Side::From, start.from_dir, cx);
        dialog.load(Side::To, start.to_dir, cx);
        dialog
    }

    pub fn first_focus(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }

    fn pane(&self, side: Side) -> &Pane {
        match side {
            Side::From => &self.from,
            Side::To => &self.to,
        }
    }

    fn pane_mut(&mut self, side: Side) -> &mut Pane {
        match side {
            Side::From => &mut self.from,
            Side::To => &mut self.to,
        }
    }

    /// Lists a folder on the side's machine.
    fn load(&mut self, side: Side, path: String, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        pane.asked += 1;
        let (asked, machine) = (pane.asked, pane.machine.clone());
        // What was picked belongs to the folder being left.
        let keep = side == Side::From && matches!(&pane.listing, Load::Ready(l) if l.path == path || display(l) == path);
        pane.listing = Load::Loading;
        if side == Side::From && !keep && asked > 1 {
            self.selected.clear();
        }
        self.error = None;
        cx.notify();
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.list_files(&machine, &path).await }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                if this.pane(side).asked != asked {
                    return;
                }
                if let Ok(l) = &result {
                    let shown = display(l);
                    this.pane(side).path.update(cx, |p, cx| p.set_text(shown, cx));
                }
                this.pane_mut(side).listing = match result {
                    Ok(l) => Load::Ready(l),
                    Err(e) => Load::Failed(e),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn current_path(&self, side: Side) -> Option<String> {
        match &self.pane(side).listing {
            Load::Ready(l) => Some(l.path.clone()),
            _ => None,
        }
    }

    fn open(&mut self, side: Side, name: &str, cx: &mut Context<Self>) {
        if let Some(p) = self.current_path(side) {
            let next = if p.ends_with('/') { format!("{p}{name}") } else { format!("{p}/{name}") };
            self.load(side, next, cx);
        }
    }

    fn up(&mut self, side: Side, cx: &mut Context<Self>) {
        if let Some(p) = self.current_path(side) {
            let parent = match p.trim_end_matches('/').rsplit_once('/') {
                Some(("", _)) | None => "/".to_string(),
                Some((dir, _)) => dir.to_string(),
            };
            self.load(side, parent, cx);
        }
    }

    fn set_machine(&mut self, side: Side, id: String, cx: &mut Context<Self>) {
        if self.pane(side).machine == id {
            return;
        }
        self.pane_mut(side).machine = id;
        if side == Side::From {
            self.selected.clear();
        }
        self.load(side, "~".into(), cx);
    }

    /// The other way round: To becomes From.
    fn swap(&mut self, cx: &mut Context<Self>) {
        let (from_dir, to_dir) = (self.current_path(Side::From).unwrap_or_else(|| "~".into()), self.current_path(Side::To).unwrap_or_else(|| "~".into()));
        std::mem::swap(&mut self.from.machine, &mut self.to.machine);
        self.selected.clear();
        self.load(Side::From, to_dir, cx);
        self.load(Side::To, from_dir, cx);
    }

    fn toggle(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.selected.remove(name) {
            self.selected.insert(name.to_string());
        }
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let (Some(dir), Some(to_dir)) = (self.current_path(Side::From), self.current_path(Side::To)) else {
            self.error = Some("Wait for both folders to open.".into());
            cx.notify();
            return;
        };
        let req = CopyRequest {
            from: self.from.machine.clone(),
            dir,
            names: self.selected.iter().cloned().collect(),
            to: self.to.machine.clone(),
            to_dir,
        };
        match self.engine.copy_files(req) {
            Ok(op) => cx.emit(DialogEvent::OperationStarted(op)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn pane_view(&self, theme: &Theme, side: Side, cx: &mut Context<Self>) -> AnyElement {
        let pane = self.pane(side);
        let tag = if side == Side::From { "from" } else { "to" };
        let chips: Vec<AnyElement> = self.machines.iter().map(|(id, name, online)| {
            let on = *id == pane.machine;
            let id2 = id.clone();
            div()
                .id(SharedString::from(format!("cp-{tag}-m-{id}")))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(24.))
                .px(px(9.))
                .rounded(px(6.))
                .border_1()
                .cursor_pointer()
                .text_size(px(12.))
                .border_color(if on { theme.accent } else { theme.border_variant })
                .bg(if on { theme.accent.opacity(0.12) } else { theme.element })
                .child(kit::dot(if *online { theme.success } else { theme.text_placeholder }, 6.))
                .child(name.clone())
                .on_click(cx.listener(move |this, _, _, cx| this.set_machine(side, id2.clone(), cx)))
                .into_any_element()
        }).collect();
        let body: AnyElement = match &pane.listing {
            Load::Loading => div().p(px(14.)).flex().items_center().gap(px(8.)).text_size(px(12.5)).text_color(theme.text_muted).child(kit::spinner(SharedString::from(format!("cp-{tag}-spin")), 12., theme.text_muted)).child("Opening…").into_any_element(),
            Load::Failed(e) => div().p(px(14.)).flex().gap(px(8.)).text_size(px(12.5)).text_color(theme.text_muted).child(icon("circle-alert", 14., theme.warning)).child(div().flex_1().min_w(px(0.)).child(e.clone())).into_any_element(),
            Load::Ready(l) => {
                let shown: Vec<&crate::fleet::files::FileEntry> = l.entries.iter().filter(|e| self.show_hidden || !e.name.starts_with('.')).collect();
                if shown.is_empty() {
                    div().p(px(14.)).text_size(px(12.5)).text_color(theme.text_placeholder).child("Nothing here.").into_any_element()
                } else {
                    let rows: Vec<AnyElement> = shown.into_iter().enumerate().map(|(i, e)| self.row(theme, side, e, i, cx)).collect();
                    div().flex().flex_col().children(rows).into_any_element()
                }
            }
        };
        let up_id = SharedString::from(format!("cp-{tag}-up"));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .gap(px(8.))
            .child(div().text_size(px(12.)).text_color(theme.text_muted).child(if side == Side::From { "From" } else { "To" }))
            .child(div().flex().flex_wrap().gap(px(5.)).children(chips))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(Button::icon_only(up_id, "arrow-up").tooltip("Up a folder").on_click(cx.listener(move |this, _, _, cx| this.up(side, cx))))
                    .child(div().flex_1().min_w(px(0.)).child(pane.path.clone())),
            )
            .child(
                div()
                    .id(SharedString::from(format!("cp-{tag}-list")))
                    .h(px(300.))
                    .overflow_y_scroll()
                    .track_scroll(&pane.scroll)
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.border_variant)
                    .bg(theme.canvas)
                    .child(body),
            )
            .into_any_element()
    }

    fn row(&self, theme: &Theme, side: Side, e: &crate::fleet::files::FileEntry, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let tag = if side == Side::From { "from" } else { "to" };
        let picked = side == Side::From && self.selected.contains(&e.name);
        let name = e.name.clone();
        let meta = match (e.folder, e.size) {
            (true, _) => String::new(),
            (false, Some(s)) => format::bytes(s),
            _ => String::new(),
        };
        // In the To pane only folders matter: it's where the files go.
        let dim = side == Side::To && !e.folder;
        let row = div()
            .id(SharedString::from(format!("cp-{tag}-r-{i}")))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .h(px(28.))
            .when(picked, |d| d.bg(theme.accent.opacity(0.12)))
            .when(!dim, |d| d.cursor_pointer().hover(|st| st.bg(theme.ghost_hover)))
            .when(side == Side::From, |d| {
                let name = name.clone();
                d.child(
                    div()
                        .id(SharedString::from(format!("cp-pick-{i}")))
                        .size(px(15.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(if picked { theme.accent } else { theme.border })
                        .bg(if picked { theme.accent } else { theme.canvas })
                        .when(picked, |d| d.child(icon("check", 11., theme.accent_text)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle(&name, cx)
                        })),
                )
            })
            .child(kit::color_icon(&theme, if e.folder { "folder" } else { "file" }, 14., theme.text_placeholder))
            .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(12.5)).text_color(if dim { theme.text_placeholder } else { theme.text }).child(e.name.clone()))
            .child(div().flex_none().font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(meta));
        // A folder opens; a file (on the left) is picked.
        if e.folder {
            row.on_click(cx.listener(move |this, _, _, cx| this.open(side, &name, cx))).into_any_element()
        } else if side == Side::From {
            row.on_click(cx.listener(move |this, _, _, cx| this.toggle(&name, cx))).into_any_element()
        } else {
            row.into_any_element()
        }
    }
}

/// A folder as people say it: ~/x for the home folder.
/// The listing's folder, as ~/... when it's in the home folder.
pub(super) fn display(l: &Listing) -> String {
    tilde(&l.path, &l.home)
}

/// `path`, as ~/... when it's in `home`.
pub(super) fn tilde(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

impl Render for CopyDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let to_name = self.machines.iter().find(|(id, _, _)| *id == self.to.machine).map(|(_, n, _)| n.clone()).unwrap_or_default();
        let to_shown = match &self.to.listing {
            Load::Ready(l) => Some(display(l)),
            _ => None,
        };
        // What's picked: how much, and what it would replace there.
        let sizes: u64 = match &self.from.listing {
            Load::Ready(l) => l.entries.iter().filter(|e| self.selected.contains(&e.name)).filter_map(|e| e.size).sum(),
            _ => 0,
        };
        let replaces: Vec<String> = match &self.to.listing {
            Load::Ready(l) => l.entries.iter().filter(|e| self.selected.contains(&e.name)).map(|e| e.name.clone()).collect(),
            _ => Vec::new(),
        };
        let same_place = self.from.machine == self.to.machine && self.current_path(Side::From).is_some() && self.current_path(Side::From) == self.current_path(Side::To);
        let ready = !self.selected.is_empty() && to_shown.is_some() && self.current_path(Side::From).is_some() && !same_place;
        let summary = match self.selected.len() {
            0 => "Pick files or folders on the left.".to_string(),
            n => format!("{n} selected{}", if sizes > 0 { format!(" · {} in files", format::bytes(sizes)) } else { String::new() }),
        };
        let copy_label = match &to_shown {
            Some(p) => format!("Copy to {to_name} {p}"),
            None => "Copy".into(),
        };
        let hidden = self.show_hidden;
        div()
            .id("copy-dialog")
            .track_focus(&self.focus)
            .w(px(860.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(18.))
            .rounded(px(10.))
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .text_color(theme.text)
            .on_key_down(cx.listener(|_, e: &gpui::KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cx.emit(DialogEvent::Dismissed)
                }
            }))
            .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Copy files"))
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(self.pane_view(&theme, Side::From, cx))
                    .child(
                        div().flex().flex_col().justify_center().child(
                            Button::icon_only("cp-swap", "arrow-right").tooltip("Swap: copy the other way").on_click(cx.listener(|this, _, _, cx| this.swap(cx))),
                        ),
                    )
                    .child(self.pane_view(&theme, Side::To, cx)),
            )
            .children((!replaces.is_empty()).then(|| {
                div().flex().items_center().gap(px(8.)).text_size(px(12.)).text_color(theme.warning).child(icon("triangle-alert", 13., theme.warning)).child(format!(
                    "Replaces {} already there.",
                    match replaces.as_slice() {
                        [one] => one.clone(),
                        many => format!("{} items", many.len()),
                    }
                ))
            }))
            .children(same_place.then(|| div().text_size(px(12.)).text_color(theme.text_muted).child("That's the same folder; pick another one to copy into.")))
            .children(self.error.clone().map(|e| div().flex().gap(px(8.)).text_size(px(12.5)).text_color(theme.error).child(icon("circle-x", 13., theme.error)).child(e)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(kit::checkbox(&theme, "cp-hidden", hidden, "Hidden files", {
                        let this = cx.entity().downgrade();
                        move |on, _, cx| {
                            let on = *on;
                            let _ = this.update(cx, |this, cx| {
                                this.show_hidden = on;
                                cx.notify()
                            });
                        }
                    }))
                    .child(div().flex_1().text_size(px(12.)).text_color(theme.text_placeholder).child(summary))
                    .child(Button::new("cp-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))))
                    .child(Button::new("cp-go", copy_label).style(ButtonStyle::Primary).disabled(!ready).on_click(cx.listener(|this, _, _, cx| this.submit(cx)))),
            )
    }
}
