//! Snippets: commands saved once (`[[settings.snippets]]` in config.toml),
//! in folders, run with one click: on the Run page on the machines picked,
//! from the palette, or typed into the focused terminal (its session page's
//! Snippets menu).

use std::sync::Arc;

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use super::dialogs::{ConfirmDialog, DialogEvent, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, card, section_title};
use super::root::{Modal, Root, Route};
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::config::{Snippet, snippet_folders};
use crate::fleet::Engine;

/// How a command of several lines shows in the one-line field: joined with "; ".
pub(super) fn one_line(command: &str) -> String {
    command.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ")
}

pub struct SnippetDialog {
    engine: Arc<Engine>,
    /// The name of the snippet being changed; None for a new one.
    editing: Option<String>,
    name: Entity<TextInput>,
    command: Entity<TextInput>,
    folder: Entity<TextInput>,
    /// The folders there are, to pick one.
    folders: Vec<String>,
    /// A command of several lines: how the field shows it, and the lines,
    /// kept as they are unless the field is changed.
    lines: Option<(String, String)>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for SnippetDialog {}

impl SnippetDialog {
    /// The dialog for `editing` (a snippet's name), or a new one with `command` in.
    pub fn new(engine: Arc<Engine>, editing: Option<String>, command: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = engine.settings();
        let existing = editing.as_deref().and_then(|n| settings.snippet(n)).cloned();
        let (name, cmd, folder) = match &existing {
            Some(s) => (s.name.clone(), s.command.clone(), s.folder.clone()),
            None => (String::new(), command.unwrap_or_default(), String::new()),
        };
        let lines = cmd.contains('\n').then(|| (one_line(&cmd), cmd.clone()));
        let shown = lines.as_ref().map(|(shown, _)| shown.clone()).unwrap_or(cmd);
        let name = cx.new(|cx| TextInput::new(cx, "e.g. Disk space").with_text(name));
        let command = cx.new(|cx| TextInput::new(cx, "e.g. df -h /").mono().with_text(shown));
        let folder = cx.new(|cx| TextInput::new(cx, "None").with_text(folder));
        let mut subs = Vec::new();
        for input in [&name, &command, &folder] {
            subs.push(cx.subscribe_in(input, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
                InputEvent::Submit => this.submit(cx),
                InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
                InputEvent::Changed => {
                    this.error = None;
                    cx.notify()
                }
            }));
        }
        let folders = snippet_folders(&settings.snippets).into_iter().filter(|f| !f.is_empty()).collect();
        SnippetDialog { engine, editing: existing.map(|s| s.name), name, command, folder, folders, lines, error: None, _subscriptions: subs }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        // A command typed on the Run page: it wants a name. Else the command first.
        if self.editing.is_none() && !self.command.read(cx).text().is_empty() { self.name.focus_handle(cx) } else { self.command.focus_handle(cx) }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let typed = self.command.read(cx).text().to_string();
        let command = match &self.lines {
            Some((shown, lines)) if *shown == typed => lines.clone(),
            _ => typed,
        };
        let snippet = Snippet { name: self.name.read(cx).text().to_string(), command, folder: self.folder.read(cx).text().to_string() };
        let mut settings = self.engine.settings();
        let result = settings.save_snippet(self.editing.as_deref(), snippet).and_then(|_| self.engine.update_settings(settings).map(|_| ()));
        match result {
            Ok(()) => cx.emit(DialogEvent::Dismissed),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }
}

impl Render for SnippetDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let title = if self.editing.is_some() { "Edit snippet" } else { "New snippet" };
        let picks = self.folders.iter().enumerate().map(|(i, f)| {
            let f2 = f.clone();
            div()
                .id(("snippet-folder", i))
                .px(px(8.))
                .h(px(22.))
                .flex()
                .items_center()
                .gap(px(4.))
                .rounded(kit::corner(6.))
                .bg(theme.element)
                .cursor_pointer()
                .hover(|st| st.bg(theme.element_hover))
                .text_size(px(11.5))
                .child(kit::icon("folder", 11., theme.text_muted))
                .child(f.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    let f = f2.clone();
                    this.folder.update(cx, |i, cx| i.set_text(f, cx));
                }))
        });
        frame(&theme, title, Some("A command you run often. Run it on the Run page, from the palette (type >), or in a terminal from its Snippets menu.".into()))
            .child(field(&theme, "Command", self.command.clone(), self.lines.as_ref().map(|_| "Its lines are joined with ; here, and kept as lines unless you change it.")))
            .child(field(&theme, "Name", self.name.clone(), Some("Empty: the command is its name.")))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(field(&theme, "Folder", self.folder.clone(), None))
                    .when(!self.folders.is_empty(), |d| d.child(div().flex().flex_wrap().gap(px(6.)).children(picks))),
            )
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                Button::new("sn-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                Button::new("sn-save", "Save").style(ButtonStyle::Primary).key_hint("↵").on_click(cx.listener(|this, _, _, cx| this.submit(cx))).into_any_element(),
            ]))
    }
}

impl Root {
    /// The New snippet dialog (with `command` in), or Edit for `editing`.
    pub(super) fn show_snippet_dialog(&mut self, editing: Option<String>, command: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| SnippetDialog::new(engine, editing, command, window, cx));
        self.show_modal(Modal::Snippet(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Asks, then deletes a snippet.
    pub(super) fn confirm_delete_snippet(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let detail = match self.engine.settings().snippet(&name) {
            Some(s) => format!("It's taken out of config.toml: {}", one_line(&s.command)),
            None => return,
        };
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Delete {name}?"),
                detail,
                "Delete snippet",
                None,
                move |_, engine| {
                    let mut settings = engine.settings();
                    settings.remove_snippet(&name)?;
                    engine.update_settings(settings).map(|_| None)
                },
                self.engine.clone(),
                window,
                cx,
            )
        });
        self.show_modal(Modal::Confirm(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Runs `command` on the Run page's machines picked.
    /// Its lines run as they are; the box above shows them on one line.
    pub(super) fn run_snippet_here(&mut self, command: &str, cx: &mut Context<Self>) {
        let text = one_line(command);
        self.run_input.update(cx, |i, cx| i.set_text(text, cx));
        self.run_command(command.trim().to_string(), cx);
    }

    /// Types `command` into a session's terminal and runs it. False when there's none to type into.
    pub(super) fn type_snippet(&mut self, session: &str, command: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(view) = self.terminals.get(session).cloned() else {
            self.notify_user("That session has no terminal here to type it into.", true, cx);
            return false;
        };
        let typed = view.update(cx, |t, cx| t.type_command(command, cx));
        if typed {
            window.focus(&view.focus_handle(cx));
        } else {
            self.notify_user("That session has ended.", true, cx);
        }
        typed
    }

    /// The session whose terminal a snippet goes into: the focused pane's, when one shows.
    pub(super) fn snippet_terminal(&self) -> Option<String> {
        match &self.route {
            Route::Session(id) if self.terminals.contains_key(id) => Some(id.clone()),
            _ => None,
        }
    }

    /// The Run page's snippets, by folder: run, edit, delete; a new one.
    pub(super) fn render_snippets(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let snippets = self.engine.settings().snippets;
        let new = Button::new("snippet-new", "New snippet").icon("plus").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, window, cx| this.show_snippet_dialog(None, None, window, cx)));
        let title = div().flex().items_center().gap(px(8.)).child(div().flex_1().child(section_title(theme, "Snippets", (!snippets.is_empty()).then_some(snippets.len())))).child(new);
        if snippets.is_empty() {
            return div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(title)
                .child(card(theme).child(kit::empty_state(theme, "code", "Save commands you run often. Run one here on the machines picked, from the palette, or in a terminal.")))
                .into_any_element();
        }
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut i = 0;
        for folder in snippet_folders(&snippets) {
            if !folder.is_empty() {
                rows.push(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(12.))
                        .pt(px(10.))
                        .pb(px(4.))
                        .text_size(px(11.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(kit::icon("folder", 12., theme.text_muted))
                        .child(folder.clone())
                        .into_any_element(),
                );
            }
            for s in snippets.iter().filter(|s| s.folder.eq_ignore_ascii_case(&folder)) {
                rows.push(self.snippet_row(theme, i, s, !folder.is_empty(), cx));
                i += 1;
            }
        }
        div().flex().flex_col().gap(px(8.)).child(title).child(card(theme).py(px(4.)).children(rows)).into_any_element()
    }

    fn snippet_row(&self, theme: &Theme, i: usize, s: &Snippet, in_folder: bool, cx: &mut Context<Self>) -> AnyElement {
        let (run, fill, edit, delete) = (s.command.clone(), s.command.clone(), s.name.clone(), s.name.clone());
        div()
            .id(("snippet", i))
            .flex()
            .items_center()
            .gap(px(8.))
            .pl(px(if in_folder { 20. } else { 8. }))
            .pr(px(8.))
            .py(px(3.))
            .hover(|st| st.bg(theme.element_hover))
            .child(Button::icon_only(("snippet-run", i), "play").tooltip("Run it on the machines picked").on_click(cx.listener(move |this, _, _, cx| this.run_snippet_here(&run, cx))))
            .child(
                div()
                    .id(("snippet-fill", i))
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .child(div().flex_none().max_w(px(220.)).truncate().text_size(px(13.)).child(s.name.clone()))
                    .child(div().flex_1().min_w(px(0.)).truncate().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_muted).child(one_line(&s.command)))
                    .tooltip({
                        let tip: SharedString = "Put it in the box above, to change it before it runs".into();
                        move |window, cx| kit::Tooltip::view(tip.clone(), window, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let text = one_line(&fill);
                        this.run_input.update(cx, |i, cx| i.set_text(text, cx));
                        window.focus(&this.run_input.read(cx).focus_handle(cx));
                    })),
            )
            .child(Button::icon_only(("snippet-edit", i), "pencil").tooltip("Edit").on_click(cx.listener(move |this, _, window, cx| this.show_snippet_dialog(Some(edit.clone()), None, window, cx))))
            .child(Button::icon_only(("snippet-delete", i), "trash-2").tooltip("Delete").on_click(cx.listener(move |this, _, window, cx| this.confirm_delete_snippet(delete.clone(), window, cx))))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::one_line;

    #[test]
    fn lines_show_on_one_line() {
        assert_eq!(one_line("df -h"), "df -h");
        assert_eq!(one_line("cd /srv/app\n  git pull\n\nmake"), "cd /srv/app; git pull; make");
    }
}
