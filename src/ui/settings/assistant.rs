//! Settings › Chat: the assistant's chat. What answers in it, the ChatGPT
//! account it may use, whether it asks before it changes something, and the
//! calls the user said never to ask about again. Kept in `[settings.chat]`
//! in config.toml; the ChatGPT sign-in is kept apart (crate::chat::account).
//!
//! None of this can be changed by the assistant itself: these settings
//! aren't among the tools it's given.

use gpui::{AnyElement, Context, FontWeight, SharedString, Stateful, Task, Window, div, prelude::*, px};

use super::SettingsPage;
use crate::chat::account::{self, Account, Endpoints, USAGE_URL, Vault};
use crate::chat::brain::{self, responses};
use crate::chat::gate::Mode;
use crate::config::ChatSettings;
use crate::ui::kit::{self, Button, ButtonStyle, card, icon, section_title};
use crate::ui::theme::{Theme, mono_font};

/// What the page knows about the ChatGPT account, and what it's doing about it.
#[derive(Default)]
pub(super) struct AssistantState {
    /// The account signed in to (read when the tab opens, and after a
    /// sign-in or sign-out; not on every draw: it's in the Keychain).
    account: Option<Account>,
    /// A sign-in under way in the browser, or a sign-out.
    working: Option<Task<()>>,
    /// What the last one did, or why it didn't: (went well, in words).
    note: Option<(bool, String)>,
    /// The account's models, once asked for.
    models: Option<Result<Vec<responses::Model>, String>>,
    loading_models: Option<Task<()>>,
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
    fn vault(&self) -> Vault {
        Vault::of(&self.engine.paths().dir)
    }

    /// Reads the ChatGPT account (when the tab is opened).
    pub(super) fn load_assistant(&mut self, cx: &mut Context<Self>) {
        self.assistant.account = account::current(&self.vault());
        cx.notify();
    }

    /// Changes `[settings.chat]`.
    fn set_chat(&mut self, change: impl FnOnce(&mut ChatSettings), cx: &mut Context<Self>) {
        let mut settings = self.engine.settings();
        change(&mut settings.chat);
        if let Err(why) = self.engine.update_settings(settings) {
            self.assistant.note = Some((false, why));
        }
        cx.notify();
    }

    /// "Continue with ChatGPT": the sign-in, in the user's browser.
    fn chatgpt_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assistant.working.is_some() {
            return;
        }
        self.assistant.note = None;
        let vault = self.vault();
        let job = self.engine.runtime().spawn(async move { account::sign_in(&vault, &Endpoints::default(), |address| crate::util::open_url(address)).await });
        self.assistant.working = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = job.await.unwrap_or_else(|e| Err(format!("The sign-in stopped: {e}")));
            let _ = this.update(cx, |this, cx| {
                this.assistant.working = None;
                match outcome {
                    Ok(account) => {
                        this.assistant.note = (!account.plan).then(|| (false, "You signed in, but didn't allow Muxin to use your plan. Sign in again and allow it.".to_string()));
                        if account.plan && !account.welcomed {
                            // Said once, the first time (here rather than in the chat, since this is where they are).
                            this.assistant.note = Some((true, "You're using your ChatGPT plan: eligible requests in the chat use it.".into()));
                            account::welcomed(&this.vault());
                        }
                        this.assistant.account = Some(account);
                        this.assistant.models = None;
                    }
                    Err(why) => this.assistant.note = Some((false, why)),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Signs out of ChatGPT; with `forget`, also of the registration, to sign
    /// in with another account.
    fn chatgpt_sign_out(&mut self, forget: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.assistant.working.is_some() {
            return;
        }
        let vault = self.vault();
        let job = self.engine.runtime().spawn(async move { account::sign_out(&vault, &Endpoints::default(), forget).await });
        self.assistant.working = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = job.await.unwrap_or_else(|e| Err(format!("The sign-out stopped: {e}")));
            let _ = this.update(cx, |this, cx| {
                this.assistant.working = None;
                this.assistant.models = None;
                this.assistant.account = account::current(&this.vault());
                this.assistant.note = match outcome {
                    Ok(true) => None,
                    Ok(false) => Some((false, "Signed out here. ChatGPT couldn't be told: you can disconnect Muxin in ChatGPT's settings.".into())),
                    Err(why) => Some((false, why)),
                };
                // A chat can't answer with an account that's gone.
                if this.engine.settings().chat.brain == brain::CHATGPT {
                    this.set_chat(|chat| chat.brain.clear(), cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Asks the account for its models.
    fn load_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assistant.loading_models.is_some() {
            return;
        }
        let vault = self.vault();
        let job = self.engine.runtime().spawn(async move { responses::models(&vault, &Endpoints::default()).await });
        self.assistant.loading_models = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = job.await.unwrap_or_else(|e| Err(format!("Couldn't ask: {e}")));
            let _ = this.update(cx, |this, cx| {
                this.assistant.loading_models = None;
                this.assistant.models = Some(outcome);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn assistant_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let chat = self.engine.settings().chat;
        div()
            .flex()
            .flex_col()
            .gap(px(28.))
            .child(self.answers_with(theme, &chat, cx))
            .child(self.chatgpt_plan(theme, &chat, cx))
            .child(self.asking(theme, &chat, cx))
            .child(self.never_asked(theme, &chat, cx))
    }

    /// What answers in a new chat.
    fn answers_with(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let plan = self.assistant.account.as_ref().is_some_and(|a| a.plan);
        let first = brain::available(plan).first().map(|k| k.name);
        let mut rows = card(theme).px(px(14.)).py(px(8.)).child(
            choice(theme, "chat-brain-first", chat.brain.is_empty(), true, "The first there is", match first {
                Some(name) => format!("Now: {name}"),
                None => "Nothing to answer with yet".to_string(),
            })
            .on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.brain.clear(), cx))),
        );
        for kind in brain::KINDS.iter() {
            let (here, about) = match kind.program {
                Some(program) if brain::find(program).is_some() => (true, "On this computer, on your own plan".to_string()),
                Some(_) => (false, "Not on this computer".to_string()),
                None => match &self.assistant.account {
                    Some(account) if account.plan => (true, format!("Muxin's own, on your ChatGPT plan ({})", account.email)),
                    Some(_) => (false, "Signed in, but plan use wasn't allowed".to_string()),
                    None => (false, "Muxin's own, once you sign in below".to_string()),
                },
            };
            let id = kind.id;
            let mut row = div().flex().items_center().gap(px(10.)).child(
                choice(theme, format!("chat-brain-{id}"), chat.brain == id, here, kind.name, about).when(here, |d| d.on_click(cx.listener(move |this, _, _, cx| this.set_chat(|chat| chat.brain = id.to_string(), cx)))),
            );
            if !here && !kind.get.is_empty() {
                let url = kind.get;
                row = row.child(div().flex_1()).child(Button::new(SharedString::from(format!("chat-get-{id}")), "Get it").icon("external-link").style(ButtonStyle::Ghost).on_click(cx.listener(move |_, _, _, cx| cx.open_url(url))));
            }
            rows = rows.child(row);
        }
        div().flex().flex_col().gap(px(10.)).child(section_title(theme, "Answers with", None)).child(rows).child(line(theme, "For new chats. A chat you go on with keeps what it started with.")).into_any_element()
    }

    /// The ChatGPT account: signing in and out, and which model answers.
    fn chatgpt_plan(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let working = self.assistant.working.is_some();
        let sign_in = |id: &'static str, cx: &mut Context<Self>| {
            Button::new(id, "Continue with ChatGPT").style(ButtonStyle::Primary).loading(working).disabled(working).on_click(cx.listener(|this, _, window, cx| this.chatgpt_sign_in(window, cx)))
        };
        let mut body = card(theme).px(px(14.)).py(px(12.)).gap(px(8.));
        match &self.assistant.account {
            None => {
                body = body
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Use your ChatGPT plan"))
                    .child(line(theme, "Complete eligible AI requests in this app with usage included"))
                    .child(line(theme, "in your ChatGPT plan or credits balance. Nothing else to install."))
                    .child(div().flex().pt(px(4.)).child(sign_in("chatgpt-sign-in", cx)));
            }
            Some(account) => {
                let who = if account.email.is_empty() { "Signed in".to_string() } else { format!("Signed in as {}", account.email) };
                body = body.child(div().flex().items_center().gap(px(8.)).text_size(px(13.)).text_color(theme.text).child(icon(if account.plan { "circle-check" } else { "circle-alert" }, 14., if account.plan { theme.success } else { theme.warning })).child(who));
                body = body.child(line(theme, if account.plan { "Using ChatGPT plan: eligible requests in the chat use it." } else { "Plan use wasn't allowed, so the chat can't answer with it." }));
                let mut buttons = div().flex().flex_wrap().items_center().gap(px(8.)).pt(px(4.));
                if !account.plan {
                    buttons = buttons.child(sign_in("chatgpt-allow", cx));
                }
                buttons = buttons
                    .child(Button::new("chatgpt-usage", "Manage usage").icon("external-link").on_click(cx.listener(|_, _, _, cx| cx.open_url(USAGE_URL))))
                    .child(Button::new("chatgpt-sign-out", "Sign out").style(ButtonStyle::Ghost).loading(working).disabled(working).on_click(cx.listener(|this, _, window, cx| this.chatgpt_sign_out(false, window, cx))))
                    .child(
                        Button::new("chatgpt-forget", "Use another account")
                            .style(ButtonStyle::Ghost)
                            .disabled(working)
                            .tooltip("Signs out and forgets which account Muxin was registered for, so you can sign in with another")
                            .on_click(cx.listener(|this, _, window, cx| this.chatgpt_sign_out(true, window, cx))),
                    );
                body = body.child(buttons);
                if account.plan {
                    body = body.child(self.models(theme, chat, cx));
                }
            }
        }
        if let Some((ok, text)) = &self.assistant.note {
            body = body.child(div().text_size(px(11.5)).text_color(if *ok { theme.text_muted } else { theme.error }).child(text.clone()));
        }
        div().flex().flex_col().gap(px(10.)).child(section_title(theme, "Your ChatGPT plan", None)).child(body).into_any_element()
    }

    /// Which of the account's models answers.
    fn models(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let mut body = div().flex().flex_col().gap(px(2.)).pt(px(8.)).mt(px(4.)).border_t_1().border_color(theme.border_variant);
        let header = div().flex().items_center().gap(px(10.)).child(div().text_size(px(12.5)).text_color(theme.text).child("Model"));
        match &self.assistant.models {
            None => {
                let now = if chat.model.is_empty() { "The first your account lists".to_string() } else { chat.model.clone() };
                body = body.child(
                    header
                        .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(now))
                        .child(div().flex_1())
                        .child(Button::new("chatgpt-models", "Choose").style(ButtonStyle::Ghost).loading(self.assistant.loading_models.is_some()).on_click(cx.listener(|this, _, window, cx| this.load_models(window, cx)))),
                );
            }
            Some(Err(why)) => body = body.child(header).child(div().text_size(px(11.5)).text_color(theme.error).child(why.clone())),
            Some(Ok(list)) => {
                body = body.child(header).child(
                    choice(theme, "chatgpt-model-first", chat.model.is_empty(), true, "The first your account lists", list.first().map(|m| format!("Now: {}", m.name)).unwrap_or_default())
                        .on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.model.clear(), cx))),
                );
                for (n, model) in list.iter().enumerate() {
                    let slug = model.id.clone();
                    body = body.child(
                        choice(theme, format!("chatgpt-model-{n}"), chat.model == model.id, true, model.name.clone(), if model.name == model.id { String::new() } else { model.id.clone() })
                            .on_click(cx.listener(move |this, _, _, cx| this.set_chat(|chat| chat.model = slug.clone(), cx))),
                    );
                }
            }
        }
        body.into_any_element()
    }

    /// Whether new chats ask before a tool changes something.
    fn asking(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let bypass = Mode::from_setting(&chat.mode) == Mode::Bypass;
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Before it changes something", None))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(8.))
                    .child(choice(theme, "chat-mode-ask", !bypass, true, "Ask first", "Looking runs. Anything that changes something waits for your yes.").on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.mode.clear(), cx))))
                    .child(choice(theme, "chat-mode-bypass", bypass, true, "Don't ask", "Every tool runs at once, the ones that delete things too.").on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.mode = "bypass".into(), cx)))),
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
