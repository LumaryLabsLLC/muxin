//! Settings › Chat: the assistant's chat. Whether it's on at all (it's off
//! until the user turns it on here), what answers in it, whether it asks
//! before it changes something, and the calls the user said never to ask
//! about again. Kept in `[settings.chat]` in config.toml.
//!
//! None of this can be changed by the assistant itself: these settings
//! aren't among the tools it's given.

use gpui::{AnyElement, Context, SharedString, Stateful, div, prelude::*, px};

use super::SettingsPage;
use crate::chat::brain;
use crate::chat::gate::Mode;
use crate::config::ChatSettings;
use crate::ui::kit::{self, Button, ButtonStyle, card, section_title};
use crate::ui::theme::{Theme, mono_font};

/// What the page has to say about the last change.
#[derive(Default)]
pub(super) struct AssistantState {
    /// Why a setting couldn't be saved.
    note: Option<String>,
}

/// A line under a title. Short: text in Settings doesn't wrap.
fn line(theme: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(11.5)).text_color(theme.text_placeholder).child(text.into())
}

/// One of several to choose from: a round mark, a title, a word about it.
fn choice(theme: &Theme, id: impl Into<SharedString>, chosen: bool, enabled: bool, title: impl Into<SharedString>, about: impl Into<SharedString>) -> Stateful<gpui::Div> {
    let about: SharedString = about.into();
    div()
        .id(id.into())
        .flex()
        .items_center()
        .gap(px(10.))
        .min_h(px(30.))
        .when(enabled, |d| d.cursor_pointer())
        .child(
            div()
                .flex_none()
                .size(px(15.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(if chosen { theme.accent } else { theme.border })
                .bg(theme.canvas)
                .when(chosen, |d| d.child(div().size(px(7.)).rounded_full().bg(theme.accent))),
        )
        .child(div().flex_none().text_size(px(12.5)).text_color(if enabled { theme.text } else { theme.text_placeholder }).child(title.into()))
        .when(!about.is_empty(), |d| d.child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(about)))
}

impl SettingsPage {
    /// Changes `[settings.chat]`.
    fn set_chat(&mut self, change: impl FnOnce(&mut ChatSettings), cx: &mut Context<Self>) {
        let mut settings = self.engine.settings();
        change(&mut settings.chat);
        if let Err(why) = self.engine.update_settings(settings) {
            self.assistant.note = Some(why);
        }
        cx.notify();
    }

    pub(super) fn assistant_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let chat = self.engine.settings().chat;
        let page = div().flex().flex_col().gap(px(28.)).child(self.switch(theme, &chat)).children(self.assistant.note.clone().map(|why| div().text_size(px(11.5)).text_color(theme.error).child(why)));
        // The rest is about a chat that's on.
        if !chat.enabled {
            return page;
        }
        page.child(self.answers_with(theme, &chat, cx)).child(self.asking(theme, &chat, cx)).child(self.never_asked(theme, &chat, cx))
    }

    /// The chat itself: off until it's turned on here.
    fn switch(&self, theme: &Theme, chat: &ChatSettings) -> AnyElement {
        let engine = self.engine.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Chat", None))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(12.))
                    .gap(px(6.))
                    .child(kit::checkbox(theme, "chat-enabled", chat.enabled, "Ask Muxin in a chat", move |on, _, cx| {
                        let mut settings = engine.settings();
                        settings.chat.enabled = *on;
                        let _ = engine.update_settings(settings);
                        cx.refresh_windows();
                    }))
                    .child(line(theme, "An assistant that works across your machines with Muxin's own tools."))
                    .child(line(theme, "It asks before it changes anything."))
                    .child(line(theme, "It answers with Claude Code, Codex or Grok Build on this computer, on your own plan."))
                    .child(div().text_size(px(11.5)).text_color(theme.text_muted).child(if chat.enabled { "On. Chat is the first page in the sidebar." } else { "Off. Turn it on and Chat becomes the first page in the sidebar." })),
            )
            .into_any_element()
    }

    /// What answers in a new chat.
    fn answers_with(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let first = brain::available().first().map(|k| k.name);
        let mut rows = card(theme).px(px(14.)).py(px(8.)).child(
            choice(theme, "chat-brain-first", chat.brain.is_empty(), true, "The first there is", match first {
                Some(name) => format!("Now: {name}"),
                None => "Nothing to answer with yet".to_string(),
            })
            .on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.brain.clear(), cx))),
        );
        for kind in brain::KINDS.iter() {
            let here = brain::find(kind.program).is_some();
            let about = if here { "On this computer, on your own plan" } else { "Not on this computer" };
            let id = kind.id;
            let mut row = div().flex().items_center().gap(px(10.)).child(
                choice(theme, format!("chat-brain-{id}"), chat.brain == id, here, kind.name, about).when(here, |d| d.on_click(cx.listener(move |this, _, _, cx| this.set_chat(|chat| chat.brain = id.to_string(), cx)))),
            );
            if !here {
                let url = kind.get;
                row = row.child(div().flex_1()).child(Button::new(SharedString::from(format!("chat-get-{id}")), "Get it").icon("external-link").style(ButtonStyle::Ghost).on_click(cx.listener(move |_, _, _, cx| cx.open_url(url))));
            }
            rows = rows.child(row);
        }
        div().flex().flex_col().gap(px(10.)).child(section_title(theme, "Answers with", None)).child(rows).child(line(theme, "For new chats. A chat you go on with keeps what it started with.")).into_any_element()
    }

    /// Whether new chats ask before a tool changes something.
    fn asking(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let bypass = Mode::from_setting(&chat.mode) == Mode::Bypass;
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Mode", None))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(8.))
                    .child(choice(theme, "chat-mode-ask", !bypass, true, "Ask permissions", "Looking runs. Anything that changes something waits for your yes.").on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.mode.clear(), cx))))
                    .child(choice(theme, "chat-mode-bypass", bypass, true, "Bypass permissions", "Every tool runs at once, the ones that delete things too.").on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.mode = "bypass".into(), cx)))),
            )
            .child(line(theme, "For new chats. Each chat can be switched under the box you type in."))
            .into_any_element()
    }

    /// The calls answered "Yes, and don't ask again".
    fn never_asked(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let mut body = card(theme).overflow_hidden();
        if chat.allow.is_empty() {
            body = body.child(kit::empty_state(theme, "lock", "Nothing yet. Answering \u{201c}Yes, and don't ask again\u{201d} in a chat adds it here."));
        }
        for (n, rule) in chat.allow.iter().enumerate() {
            let remove = rule.clone();
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(36.))
                    .px(px(14.))
                    .when(n > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .child(div().flex_1().min_w(px(0.)).font_family(mono_font()).text_size(px(12.)).text_color(theme.text).child(rule.clone()))
                    .child(Button::icon_only(SharedString::from(format!("chat-rule-{n}")), "x").tooltip("Ask about this again").on_click(cx.listener(move |this, _, _, cx| {
                        let remove = remove.clone();
                        this.set_chat(move |chat| chat.allow.retain(|r| *r != remove), cx);
                    }))),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Runs without asking", Some(chat.allow.len()).filter(|n| *n > 0)))
            .child(body)
            .child(line(theme, "Calls that delete, stop running work or cost money are never on this list: they ask every time."))
            .into_any_element()
    }
}
