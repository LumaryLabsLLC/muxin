//! Settings › Prompts: what each session action (commit, open a pull
//! request, review, ...) types into an agent. Any can be changed; Muxin's
//! own comes back with Reset. Kept in `[settings.prompts]` in config.toml.

use gpui::{AnyElement, Context, Entity, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use super::SettingsPage;
use crate::fleet::sessions::actions::{ACTIONS, SessionAction};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::kit::{Button, ButtonStyle, card, icon, section_title};
use crate::ui::theme::{Theme, mono_font};

/// The prompt being changed: which action, its field, and what watches it.
pub(super) struct PromptEdit {
    action: &'static str,
    input: Entity<TextInput>,
    _subscription: Subscription,
}

impl SettingsPage {
    /// The prompt an action types in: the user's, else Muxin's.
    fn prompt_of(&self, a: &SessionAction) -> (String, bool) {
        match self.engine.settings().prompts.get(a.id).filter(|p| !p.trim().is_empty()) {
            Some(p) => (p.clone(), true),
            None => (a.prompt.to_string(), false),
        }
    }

    fn edit_prompt(&mut self, a: &'static SessionAction, window: &mut Window, cx: &mut Context<Self>) {
        let (text, _) = self.prompt_of(a);
        let input = cx.new(|cx| TextInput::new(cx, a.prompt).with_text(text));
        let subscription = cx.subscribe_in(&input, window, |this, _, e: &InputEvent, _, cx| match e {
            InputEvent::Submit => this.save_prompt(cx),
            InputEvent::Cancel => {
                this.prompt_edit = None;
                cx.notify()
            }
            InputEvent::Changed => cx.notify(),
        });
        window.focus(&input.focus_handle(cx));
        self.prompt_edit = Some(PromptEdit { action: a.id, input, _subscription: subscription });
        cx.notify();
    }

    /// Starts changing an action's prompt, by its id.
    pub(crate) fn edit_prompt_id(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(a) = crate::fleet::sessions::actions::action(id) {
            self.edit_prompt(a, window, cx);
        }
    }

    /// Keeps what's in the field; Muxin's own (or an empty field) is kept as no change.
    fn save_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.prompt_edit.take() else { return };
        let text = edit.input.read(cx).text().trim().to_string();
        self.set_prompt(edit.action, Some(text), cx);
    }

    fn set_prompt(&mut self, action: &'static str, text: Option<String>, cx: &mut Context<Self>) {
        let own = crate::fleet::sessions::actions::action(action).map(|a| a.prompt).unwrap_or_default();
        let mut s = self.engine.settings();
        match text.filter(|t| !t.is_empty() && t != own) {
            Some(t) => s.prompts.insert(action.to_string(), t),
            None => s.prompts.remove(action),
        };
        self.save_error = self.engine.update_settings(s).err();
        cx.notify();
    }

    pub(super) fn prompts_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = ACTIONS.iter().enumerate().map(|(i, a)| self.prompt_row(theme, i, a, cx)).collect();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Session actions", None))
            .child(
                div().text_size(px(12.5)).text_color(theme.text_muted).child(
                    "Ask an agent to commit, open a pull request, review its work and more, from a session's Actions menu. Each one types a prompt into the agent. Change any of them here: {branch}, {base} and {pull} are filled in with the session's branch, the branch it started from and its pull request.",
                ),
            )
            .child(card(theme).children(rows))
    }

    fn prompt_row(&self, theme: &Theme, i: usize, a: &'static SessionAction, cx: &mut Context<Self>) -> AnyElement {
        let (text, changed) = self.prompt_of(a);
        let editing = self.prompt_edit.as_ref().filter(|e| e.action == a.id);
        let header = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(icon(a.icon, 14., theme.text_muted))
            .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.text).child(a.label))
            .when(changed, |d| d.child(div().text_size(px(11.)).text_color(theme.text_accent).child("changed")))
            .child(div().flex_1())
            .when(editing.is_none(), |d| {
                d.when(changed, |d| {
                    d.child(Button::new(SharedString::from(format!("prompt-reset-{}", a.id)), "Reset").style(ButtonStyle::Ghost).tooltip("Go back to Muxin's prompt").on_click(cx.listener(move |this, _, _, cx| this.set_prompt(a.id, None, cx))))
                })
                .child(Button::new(SharedString::from(format!("prompt-edit-{}", a.id)), "Edit").icon("pencil").on_click(cx.listener(move |this, _, window, cx| this.edit_prompt(a, window, cx))))
            })
            .when(editing.is_some(), |d| {
                d.child(Button::new("prompt-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, _, cx| {
                    this.prompt_edit = None;
                    cx.notify()
                })))
                .child(Button::new("prompt-save", "Save").style(ButtonStyle::Primary).on_click(cx.listener(|this, _, _, cx| this.save_prompt(cx))))
            });
        // Editing: the field, and the whole prompt wrapped below it as it's typed.
        let body = match editing {
            Some(e) => {
                let typed = e.input.read(cx).text().to_string();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(e.input.clone())
                    .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(if typed.trim().is_empty() { format!("Empty uses Muxin's: {}", a.prompt) } else { typed }))
                    .into_any_element()
            }
            None => div().text_size(px(12.5)).line_height(px(18.)).text_color(theme.text_muted).child(text).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .py(px(12.))
            .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
            .child(header)
            .child(body)
            .child(div().font_family(mono_font()).text_size(px(10.5)).text_color(theme.text_placeholder).child(format!("muxin session do <session> {}", a.id)))
            .into_any_element()
    }
}
