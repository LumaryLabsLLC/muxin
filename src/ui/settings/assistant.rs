//! Settings › AI: the assistant's chat and what answers in it. Whether the
//! chat is on at all (it's off until the user turns it on here), the
//! providers there are to answer with (programs on this computer, and
//! providers connected with an API key), which of their models the pickers
//! offer, what a new chat starts with (what answers, its model, effort and
//! speed, and whether it asks before it changes something), and the calls
//! the user said never to ask about again. Kept in
//! `[settings.chat]` in config.toml; API keys are kept apart, with Muxin's
//! other secrets (crate::chat::keys).
//!
//! None of this can be changed by the assistant itself: these settings
//! aren't among the tools it's given.

use gpui::{AnyElement, Context, Entity, SharedString, Stateful, Window, div, prelude::*, px};

use super::SettingsPage;
use crate::chat::brain::{self, catalog};
use crate::chat::gate::Mode;
use crate::chat::provider;
use crate::config::{ChatSettings, MANY_MODELS};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::kit::{self, Button, ButtonStyle, card, icon, section_title};
use crate::ui::theme::{Theme, mono_font};

/// Of a long list, the models shown before any is looked for: its first few.
const FEW: usize = 8;
/// How many more of them "Show more" adds.
const MORE: usize = 20;
/// The models shown of those a name narrows the list to.
const MOST_FOUND: usize = 20;
/// A provider's list is asked for again when it's looked at and this old:
/// what it lists changes, and so does which are used most.
const OLD: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// The room a row of options has, in pixels, before it goes on to another row.
const OPTIONS_WIDE: f32 = 540.;

/// What the page is doing about providers, and what it has to say.
pub(super) struct AssistantState {
    /// What the last thing did, or why it didn't: (went well, in words).
    note: Option<(bool, String)>,
    /// The provider whose key is being typed in, and the fields for it.
    connecting: Option<&'static str>,
    key: Entity<TextInput>,
    base: Entity<TextInput>,
    /// A provider is being asked (to connect, or for its models again).
    busy: bool,
    /// The brain whose models are listed, and what narrows a long list.
    listing: Option<&'static str>,
    filter: Entity<TextInput>,
    /// Models of a long list shown beyond its first few.
    more: usize,
    /// The providers being asked for their models again, quietly.
    asking: Vec<&'static str>,
}

impl AssistantState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SettingsPage>) -> (AssistantState, Vec<gpui::Subscription>) {
        let key = cx.new(|cx| TextInput::new(cx, "API key").mono().secret());
        let base = cx.new(|cx| TextInput::new(cx, "https://…/v1").mono());
        let filter = cx.new(|cx| TextInput::new(cx, "Find a model"));
        let subs = vec![
            cx.subscribe_in(&key, window, |this: &mut SettingsPage, _, e: &InputEvent, _, cx| {
                if *e == InputEvent::Submit {
                    this.connect_provider(cx);
                }
            }),
            cx.subscribe_in(&base, window, |this: &mut SettingsPage, _, e: &InputEvent, _, cx| {
                if *e == InputEvent::Submit {
                    this.connect_provider(cx);
                }
            }),
            // The list follows what's typed.
            cx.subscribe_in(&filter, window, |this: &mut SettingsPage, _, _: &InputEvent, _, cx| {
                this.assistant.more = 0;
                cx.notify();
            }),
        ];
        (AssistantState { note: None, connecting: None, key, base, busy: false, listing: None, filter, more: 0, asking: Vec::new() }, subs)
    }
}

/// A line under a title. Short: text in Settings doesn't wrap.
fn line(theme: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(11.5)).text_color(theme.text_placeholder).child(text.into())
}

/// One of a few to pick from, side by side: lit when it's the one picked.
fn option(theme: &Theme, id: impl Into<SharedString>, chosen: bool, label: impl Into<SharedString>) -> Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex_none()
        .h(px(26.))
        .px(px(10.))
        .flex()
        .items_center()
        .rounded(kit::corner(6.))
        .border_1()
        .border_color(if chosen { theme.accent } else { theme.border_variant })
        .bg(if chosen { theme.accent.opacity(0.10) } else { theme.canvas })
        .text_size(px(12.))
        .text_color(if chosen { theme.text } else { theme.text_muted })
        .cursor_pointer()
        .when(!chosen, |d| d.hover(|st| st.border_color(theme.border).text_color(theme.text)))
        .child(label.into())
}

/// How wide an option with these words is, about.
fn option_width(label: &str) -> f32 {
    label.chars().count() as f32 * 6. + 22.
}

/// How many options go in each row, by their widths: as many as fit.
fn breaks(widths: &[f32]) -> Vec<usize> {
    let mut rows: Vec<usize> = Vec::new();
    let mut used = 0.;
    for wide in widths {
        match rows.last_mut() {
            Some(row) if used + wide <= OPTIONS_WIDE => *row += 1,
            _ => {
                rows.push(1);
                used = 0.;
            }
        }
        used += wide + 6.;
    }
    rows
}

/// Options in rows, as many to a row as fit. (Rows of its own making: GPUI
/// measures a wrapping row in a column of no set width as one long line.)
fn packed(options: Vec<(f32, AnyElement)>) -> gpui::Div {
    let widths: Vec<f32> = options.iter().map(|(wide, _)| *wide).collect();
    let mut options = options.into_iter().map(|(_, option)| option);
    div().flex().flex_col().gap(px(6.)).children(breaks(&widths).into_iter().map(|many| div().flex().gap(px(6.)).children(options.by_ref().take(many).collect::<Vec<_>>())))
}

/// What a long list of models shows: the ones offered, then some of the
/// rest, and how many of the rest aren't shown.
struct Shown<'a> {
    offered: Vec<&'a catalog::Model>,
    rest: Vec<&'a catalog::Model>,
    left: usize,
}

/// With nothing looked for: everything offered, and the first few of the
/// rest (`more` more of them, once asked for). With a name: the ones by
/// that name, the offered ones first.
fn shown<'a>(all: &'a [catalog::Model], offers: impl Fn(&catalog::Model) -> bool, wanted: &str, more: usize) -> Shown<'a> {
    let (on, off): (Vec<&catalog::Model>, Vec<&catalog::Model>) = all.iter().partition(|model| offers(model));
    let wanted = wanted.trim().to_lowercase();
    if wanted.is_empty() {
        let few = (FEW + more).min(off.len());
        return Shown { left: off.len() - few, rest: off.into_iter().take(few).collect(), offered: on };
    }
    let found: Vec<&catalog::Model> = on.into_iter().chain(off).filter(|m| m.id.to_lowercase().contains(&wanted) || m.name.to_lowercase().contains(&wanted)).collect();
    Shown { offered: Vec::new(), left: found.len().saturating_sub(MOST_FOUND), rest: found.into_iter().take(MOST_FOUND).collect() }
}

impl SettingsPage {
    /// Changes `[settings.chat]`.
    fn set_chat(&mut self, change: impl FnOnce(&mut ChatSettings), cx: &mut Context<Self>) {
        let mut settings = self.engine.settings();
        change(&mut settings.chat);
        if let Err(why) = self.engine.update_settings(settings) {
            self.assistant.note = Some((false, why));
        }
        cx.notify();
    }

    /// Shows a provider's key form, or a brain's list of models (a hidden
    /// `--view` hook, to look at them without a click).
    pub(in crate::ui) fn show_ai(&mut self, connecting: Option<&str>, listing: Option<&str>, cx: &mut Context<Self>) {
        let known = |id: Option<&str>| id.and_then(brain::kind).map(|kind| kind.id);
        if let Some(id) = known(connecting) {
            self.start_connecting(Some(id), cx);
        }
        if let Some(id) = known(listing) {
            self.assistant.listing = Some(id);
            self.refresh_if_old(id, cx);
        }
        cx.notify();
    }

    /// The AI tab is shown: the lists that are old are asked for again.
    pub(super) fn refresh_old_lists(&mut self, cx: &mut Context<Self>) {
        for kind in brain::available(&self.engine.settings().chat) {
            self.refresh_if_old(kind.id, cx);
        }
    }

    /// Asks a connected provider for its models again when the list kept is
    /// old. Quietly: nobody asked, so nothing is said if it doesn't answer.
    fn refresh_if_old(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let connected = brain::kind(id).is_some_and(|kind| kind.api().is_some()) && self.engine.settings().chat.providers.contains_key(id);
        let old = catalog::age(&self.engine.paths().dir, id).is_none_or(|age| age > OLD);
        // A demo's providers aren't anyone's: nothing is asked of them.
        if !connected || !old || self.engine.is_demo() || self.assistant.busy || self.assistant.asking.contains(&id) {
            return;
        }
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { provider::refresh(&engine, id).await }
        });
        self.assistant.asking.push(id);
        cx.spawn(async move |this, cx| {
            let _ = job.await;
            let _ = this.update(cx, |this, cx| {
                this.assistant.asking.retain(|asked| *asked != id);
                cx.notify();
            });
        })
        .detach();
    }

    /// What `brain`'s new chats start with: this model (None: its own choice).
    fn start_chats_with(&mut self, brain_id: &'static str, model: Option<String>, cx: &mut Context<Self>) {
        let models = provider::offered(&self.engine, brain_id);
        let found = model.as_ref().and_then(|id| catalog::find(&models, id));
        let efforts: Vec<String> = found.map(|m| m.efforts.iter().map(|e| e.id.clone()).collect()).unwrap_or_default();
        let fast = found.is_some_and(|m| m.fast);
        self.set_chat(
            move |chat| {
                chat.start_with(brain_id, model.as_deref(), &efforts, fast);
            },
            cx,
        );
    }

    /// Opens the form for a provider's key (or closes it).
    fn start_connecting(&mut self, id: Option<&'static str>, cx: &mut Context<Self>) {
        self.assistant.connecting = id;
        self.assistant.note = None;
        for field in [&self.assistant.key, &self.assistant.base] {
            field.update(cx, |field, cx| field.set_text("", cx));
        }
        cx.notify();
    }

    /// Connects the provider whose form is open, with the key typed in it.
    /// The provider is asked for its models first: nothing is kept unless it answers.
    fn connect_provider(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.assistant.connecting else { return };
        if self.assistant.busy {
            return;
        }
        let (key, base) = (self.assistant.key.read(cx).text().to_string(), self.assistant.base.read(cx).text().to_string());
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { provider::connect(&engine, id, &key, &base).await }
        });
        self.assistant.busy = true;
        self.assistant.note = None;
        cx.spawn(async move |this, cx| {
            let outcome = job.await.unwrap_or_else(|e| Err(format!("Connecting stopped: {e}")));
            let _ = this.update(cx, |this, cx| {
                this.assistant.busy = false;
                match outcome {
                    Ok(models) => {
                        let name = brain::kind(id).map(|k| k.name).unwrap_or(id);
                        let offered = provider::offered(&this.engine, id).len();
                        let said = if offered == 0 && !models.is_empty() { format!("Connected {name}. It lists {} models: choose which to offer, under Models.", models.len()) } else { format!("Connected {name}, with {} models.", models.len()) };
                        this.start_connecting(None, cx);
                        this.assistant.note = Some((true, said));
                        // Its models, right there to choose from.
                        this.assistant.listing = Some(id);
                    }
                    Err(why) => this.assistant.note = Some((false, why)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Asks a connected provider for its models again.
    fn refresh_provider(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.assistant.busy {
            return;
        }
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { provider::refresh(&engine, id).await }
        });
        self.assistant.busy = true;
        cx.spawn(async move |this, cx| {
            let outcome = job.await.unwrap_or_else(|e| Err(format!("Asking stopped: {e}")));
            let _ = this.update(cx, |this, cx| {
                this.assistant.busy = false;
                let name = brain::kind(id).map(|k| k.name).unwrap_or(id);
                this.assistant.note = Some(match outcome {
                    Ok(models) => (true, format!("{name} lists {} models now.", models.len())),
                    Err(why) => (false, why),
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn disconnect_provider(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let name = brain::kind(id).map(|k| k.name).unwrap_or(id);
        self.assistant.note = Some(match provider::disconnect(&self.engine, id) {
            Ok(()) => (true, format!("Disconnected {name}. Its key is forgotten.")),
            Err(why) => (false, why),
        });
        if self.assistant.listing == Some(id) {
            self.assistant.listing = None;
        }
        cx.notify();
    }

    pub(super) fn assistant_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let chat = self.engine.settings().chat;
        let page = div().flex().flex_col().gap(px(28.)).child(self.switch(theme, &chat));
        // The rest is about a chat that's on.
        if !chat.enabled {
            return page;
        }
        page.child(self.providers(theme, &chat, cx)).child(self.models(theme, &chat, cx)).child(self.new_chats(theme, &chat, cx)).child(self.never_asked(theme, &chat, cx))
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
                    .child(div().text_size(px(11.5)).text_color(theme.text_muted).child(if chat.enabled { "On. Chat is the first page in the sidebar." } else { "Off. Turn it on and Chat becomes the first page in the sidebar." })),
            )
            .into_any_element()
    }

    /// A provider's row: its mark, its name, how it's reached, and what follows.
    fn provider_row(&self, theme: &Theme, kind: &'static brain::Kind, first: bool, about: String) -> gpui::Div {
        let how = if kind.program().is_some() { "Program" } else { "API key" };
        let mark = match kind.id {
            "anthropic" => "claude",
            "openai" => "codex",
            id => id,
        };
        let (name, (from, to)) = kit::agent_look(mark);
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .min_h(px(44.))
            .px(px(14.))
            .when(!first, |d| d.border_t_1().border_color(theme.border_variant))
            .child(kit::tile(name, from, to, 20.))
            .child(div().flex_none().text_size(px(13.)).text_color(theme.text).child(kind.name))
            .child(div().flex_none().px(px(6.)).py(px(1.)).rounded(kit::corner(4.)).bg(theme.element_hover).text_size(px(10.5)).text_color(theme.text_muted).child(how))
            .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(about))
    }

    /// What there is to answer with: connected, and what could be.
    fn providers(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.assistant.busy;
        let is_connected = |kind: &brain::Kind| match kind.via {
            brain::Via::Program(program) => brain::find(program).is_some(),
            brain::Via::Api(_) => chat.providers.contains_key(kind.id),
        };
        let mut connected = card(theme).overflow_hidden();
        let mut more = card(theme).overflow_hidden();
        let (mut have, mut could) = (0, 0);
        for kind in brain::KINDS.iter() {
            let id = kind.id;
            if is_connected(kind) {
                let about = match chat.providers.get(id) {
                    None => "On this computer, on your own plan".to_string(),
                    Some(set) if set.base.is_empty() => "Paid by the token".to_string(),
                    Some(set) => format!("At {}", set.base),
                };
                let mut row = self.provider_row(theme, kind, have == 0, about);
                if kind.api().is_some() {
                    row = row
                        .child(Button::icon_only(SharedString::from(format!("ai-refresh-{id}")), "refresh-cw").tooltip("Ask for its models again").disabled(busy).on_click(cx.listener(move |this, _, _, cx| this.refresh_provider(id, cx))))
                        .child(Button::new(SharedString::from(format!("ai-disconnect-{id}")), "Disconnect").style(ButtonStyle::Ghost).disabled(busy).on_click(cx.listener(move |this, _, _, cx| this.disconnect_provider(id, cx))));
                }
                connected = connected.child(row);
                have += 1;
                continue;
            }
            let about = match kind.via {
                brain::Via::Program(_) => "Not on this computer".to_string(),
                brain::Via::Api(_) if id == brain::CUSTOM => "Any provider that speaks chat completions, at its address".to_string(),
                brain::Via::Api(_) => "With an API key, paid by the token".to_string(),
            };
            let mut row = self.provider_row(theme, kind, could == 0, about);
            row = match kind.via {
                brain::Via::Program(_) => {
                    let url = kind.get;
                    row.child(Button::new(SharedString::from(format!("ai-get-{id}")), "Get it").icon("external-link").style(ButtonStyle::Ghost).on_click(cx.listener(move |_, _, _, cx| cx.open_url(url))))
                }
                brain::Via::Api(_) => row.child(Button::new(SharedString::from(format!("ai-connect-{id}")), "Connect").icon("plus").on_click(cx.listener(move |this, _, _, cx| this.start_connecting(Some(id), cx)))),
            };
            more = more.child(row);
            // Its key is typed right under it.
            if self.assistant.connecting == Some(id) {
                more = more.child(self.connect_form(theme, kind, cx));
            }
            could += 1;
        }
        let mut section = div().flex().flex_col().gap(px(10.)).child(section_title(theme, "Providers", None));
        if have > 0 {
            section = section.child(kit::label(theme, "Connected")).child(connected);
        }
        if could > 0 {
            section = section.child(kit::label(theme, "More providers")).child(more);
        }
        if let Some((ok, text)) = &self.assistant.note {
            section = section.child(div().text_size(px(11.5)).text_color(if *ok { theme.text_muted } else { theme.error }).child(text.clone()));
        }
        section.child(line(theme, "A program answers on its own plan. A key is kept in the Keychain, and paid by the token.")).into_any_element()
    }

    /// The form for a provider's key, and its address when that's the user's to give.
    fn connect_form(&self, theme: &Theme, kind: &'static brain::Kind, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.assistant.busy;
        let own = kind.api().map(|api| api.base).unwrap_or_default();
        let mut form = div().flex().flex_col().gap(px(8.)).px(px(14.)).pb(px(12.)).child(div().flex().items_center().gap(px(10.)).child(div().w(px(64.)).flex_none().text_size(px(12.)).text_color(theme.text_muted).child("API key")).child(div().flex_1().child(self.assistant.key.clone())));
        // Its own address unless another is given: a gateway in front of it, say.
        form = form.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(div().w(px(64.)).flex_none().text_size(px(12.)).text_color(theme.text_muted).child("Address"))
                .child(div().flex_1().child(self.assistant.base.clone())),
        );
        form = form.child(line(theme, if own.is_empty() { "Where it's reached, like https://example.com/v1.".to_string() } else { format!("Leave the address empty for {own}.") }));
        let mut buttons = div().flex().items_center().gap(px(8.)).child(Button::new("ai-connect-go", if busy { "Connecting…" } else { "Connect" }).style(ButtonStyle::Primary).loading(busy).disabled(busy).on_click(cx.listener(|this, _, _, cx| this.connect_provider(cx)))).child(
            Button::new("ai-connect-cancel", "Cancel").style(ButtonStyle::Ghost).disabled(busy).on_click(cx.listener(|this, _, _, cx| this.start_connecting(None, cx))),
        );
        if !kind.get.is_empty() {
            let url = kind.get;
            buttons = buttons.child(div().flex_1()).child(Button::new("ai-connect-key", "Get a key").icon("external-link").style(ButtonStyle::Ghost).on_click(cx.listener(move |_, _, _, cx| cx.open_url(url))));
        }
        form.child(buttons).into_any_element()
    }

    /// Which models the pickers offer, a brain at a time.
    fn models(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let dir = self.engine.paths().dir.clone();
        let mut groups = card(theme).overflow_hidden();
        let brains = brain::available(chat);
        for (n, kind) in brains.iter().enumerate() {
            let id = kind.id;
            let all = catalog::known(&dir, id);
            let listed = all.len();
            let offered = all.iter().filter(|m| chat.offers(id, &m.id, listed)).count();
            let open = self.assistant.listing == Some(id);
            let count = match listed {
                0 => "It doesn't say which it has".to_string(),
                _ => format!("{offered} of {listed} offered"),
            };
            groups = groups.child(
                div()
                    .id(SharedString::from(format!("ai-models-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(38.))
                    .px(px(14.))
                    .cursor_pointer()
                    .when(n > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .hover(|st| st.bg(theme.ghost_hover))
                    .child(icon(if open { "chevron-down" } else { "chevron-right" }, 13., theme.text_placeholder))
                    .child(div().flex_none().text_size(px(13.)).text_color(theme.text).child(kind.name))
                    .child(div().flex_1())
                    .child(div().flex_none().text_size(px(11.5)).text_color(theme.text_placeholder).child(count))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.assistant.listing = if this.assistant.listing == Some(id) { None } else { Some(id) };
                        this.assistant.filter.update(cx, |field, cx| field.set_text("", cx));
                        this.assistant.more = 0;
                        if this.assistant.listing.is_some() {
                            this.refresh_if_old(id, cx);
                        }
                        cx.notify();
                    })),
            );
            if !open || listed == 0 {
                continue;
            }
            let offers = |model: &catalog::Model| chat.offers(id, &model.id, listed);
            // A model's line: a box to check, its name, and its id when that's something else.
            let row = |model: &catalog::Model| -> AnyElement {
                let engine = self.engine.clone();
                let model_id = model.id.clone();
                let check = kit::checkbox(theme, SharedString::from(format!("ai-model-{id}-{}", model.id)), offers(model), model.name.clone(), move |on, _, cx| {
                    let mut settings = engine.settings();
                    settings.chat.offer(id, &model_id, *on);
                    let _ = engine.update_settings(settings);
                    cx.refresh_windows();
                });
                let line = div().flex().items_center().gap(px(10.)).child(check);
                if model.name == model.id { line } else { line.child(div().font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(model.id.clone())) }.into_any_element()
            };
            let mut rows = div().flex().flex_col().gap(px(4.)).px(px(14.)).pb(px(12.)).pl(px(35.));
            if listed <= MANY_MODELS {
                // A short list is all there.
                rows = rows.children(all.iter().map(row));
                groups = groups.child(rows);
                continue;
            }
            // A long one shows what's offered and its first few (the most used,
            // from a provider that says). The rest is found by name.
            rows = rows.child(div().pb(px(4.)).child(self.assistant.filter.clone()));
            let wanted = self.assistant.filter.read(cx).text().trim().to_string();
            let Shown { offered, rest, left } = shown(&all, offers, &wanted, self.assistant.more);
            if !wanted.is_empty() {
                rows = rows.children(rest.iter().map(|model| row(model)));
                if left > 0 {
                    rows = rows.child(line(theme, format!("{left} more by that name. Type more of it.")));
                }
                if rest.is_empty() {
                    rows = rows.child(line(theme, "None by that name."));
                }
                groups = groups.child(rows);
                continue;
            }
            if !offered.is_empty() {
                rows = rows.child(kit::label(theme, "Offered").pt(px(2.))).children(offered.iter().map(|model| row(model)));
            }
            rows = rows.child(kit::label(theme, if id == "openrouter" { "Most popular" } else { "More of its models" }).pt(px(6.))).children(rest.iter().map(|model| row(model)));
            if left > 0 {
                rows = rows.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .pt(px(4.))
                        .child(Button::new(SharedString::from(format!("ai-models-more-{id}")), "Show more").style(ButtonStyle::Outlined).on_click(cx.listener(|this, _, _, cx| {
                            this.assistant.more += MORE;
                            cx.notify();
                        })))
                        .child(line(theme, format!("{left} more. Type a name to find one."))),
                );
            }
            groups = groups.child(rows);
        }
        let mut section = div().flex().flex_col().gap(px(10.)).child(section_title(theme, "Models", None));
        section = if brains.is_empty() { section.child(card(theme).child(kit::empty_state(theme, "bot", "Nothing to answer with yet. Connect a provider above."))) } else { section.child(groups) };
        section.child(line(theme, "Which models the menus under the chat's box offer. A long list offers only what you check.")).into_any_element()
    }

    /// What a new chat starts with: what answers, its model, how hard it
    /// thinks and how fast it's run, and whether it asks before it changes
    /// something. The menus under the chat's box pick the same things.
    fn new_chats(&self, theme: &Theme, chat: &ChatSettings, cx: &mut Context<Self>) -> AnyElement {
        let named = |name: &'static str, options: gpui::Div| {
            div().flex().items_start().gap(px(12.)).child(div().w(px(84.)).flex_none().h(px(26.)).flex().items_center().text_size(px(12.)).text_color(theme.text_muted).child(name)).child(options)
        };
        let said = |text: String| div().h(px(26.)).flex().items_center().text_size(px(11.5)).text_color(theme.text_placeholder).child(text);
        let usable = brain::available(chat);
        // The one set, else the first there is.
        let now = usable.iter().find(|kind| kind.id == chat.brain).or(usable.first()).copied();
        let mut body = card(theme).px(px(14.)).py(px(12.)).gap(px(10.));

        let answers = usable
            .iter()
            .map(|kind| {
                let id = kind.id;
                let pick = option(theme, format!("chat-brain-{id}"), now.is_some_and(|now| now.id == id), kind.name).on_click(cx.listener(move |this, _, _, cx| this.set_chat(|chat| chat.brain = id.to_string(), cx)));
                (option_width(kind.name), pick.into_any_element())
            })
            .collect::<Vec<_>>();
        body = body.child(named("Answers with", if answers.is_empty() { said("Nothing to answer with yet. Connect a provider above.".into()) } else { packed(answers) }));

        if let Some(kind) = now {
            let id = kind.id;
            let models = provider::offered(&self.engine, id);
            let (picked, effort) = chat.pick(id);
            // A program has a choice of its own; a provider is asked for a model by name.
            let own = kind.program().is_some();
            let model = picked.as_deref().and_then(|picked| catalog::find(&models, picked)).or(models.first());
            let mut options = Vec::new();
            if own {
                let pick = option(theme, "chat-model-own", picked.is_none(), "Default").on_click(cx.listener(move |this, _, _, cx| this.start_chats_with(id, None, cx)));
                options.push((option_width("Default"), pick.into_any_element()));
            }
            for listed in &models {
                let chosen = if own { picked.as_deref() == Some(listed.id.as_str()) } else { model.is_some_and(|model| model.id == listed.id) };
                let model_id = listed.id.clone();
                let pick = option(theme, format!("chat-model-{}", listed.id), chosen, listed.name.clone()).on_click(cx.listener(move |this, _, _, cx| this.start_chats_with(id, Some(model_id.clone()), cx)));
                options.push((option_width(&listed.name), pick.into_any_element()));
            }
            body = body.child(named(
                "Model",
                match (models.is_empty(), own) {
                    (true, true) => said(format!("{} answers with its own choice.", kind.name)),
                    (true, false) => said(format!("None of {}'s models is offered yet. Check some under Models.", kind.name)),
                    _ => packed(options),
                },
            ));

            if let Some(model) = model.filter(|model| !model.efforts.is_empty()) {
                let effort = effort.filter(|effort| model.efforts.iter().any(|e| e.id == *effort));
                let own = match model.effort.as_deref().and_then(|id| model.efforts.iter().find(|e| e.id == id)) {
                    Some(usual) => format!("Default ({})", usual.name),
                    None => "Default".to_string(),
                };
                let set = |to: Option<String>| {
                    cx.listener(move |this: &mut Self, _: &gpui::ClickEvent, _, cx| {
                        let to = to.clone().unwrap_or_default();
                        this.set_chat(move |chat| chat.picked.entry(id.to_string()).or_default().effort = to, cx)
                    })
                };
                let mut options = vec![(option_width(&own), option(theme, "chat-effort-own", effort.is_none(), own).on_click(set(None)).into_any_element())];
                for level in &model.efforts {
                    let pick = option(theme, format!("chat-effort-{}", level.id), effort.as_deref() == Some(level.id.as_str()), level.name.clone()).on_click(set(Some(level.id.clone())));
                    options.push((option_width(&level.name), pick.into_any_element()));
                }
                body = body.child(named("Effort", packed(options)));
            }

            if model.is_some_and(|model| model.fast) {
                let fast = chat.speed(id).as_deref() == Some("fast");
                let set = |to: &'static str| cx.listener(move |this: &mut Self, _: &gpui::ClickEvent, _, cx| this.set_chat(move |chat| chat.picked.entry(id.to_string()).or_default().speed = to.to_string(), cx));
                let options = vec![
                    (option_width("Standard"), option(theme, "chat-speed-standard", !fast, "Standard").on_click(set("")).into_any_element()),
                    (option_width("Fast"), option(theme, "chat-speed-fast", fast, "Fast").on_click(set("fast")).into_any_element()),
                ];
                body = body.child(named("Speed", packed(options).child(line(theme, "Fast answers sooner, and costs more."))));
            }
        }

        let bypass = Mode::from_setting(&chat.mode) == Mode::Bypass;
        let modes = vec![
            (option_width("Ask permissions"), option(theme, "chat-mode-ask", !bypass, "Ask permissions").on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.mode.clear(), cx))).into_any_element()),
            (option_width("Bypass permissions"), option(theme, "chat-mode-bypass", bypass, "Bypass permissions").on_click(cx.listener(|this, _, _, cx| this.set_chat(|chat| chat.mode = "bypass".into(), cx))).into_any_element()),
        ];
        let means = if bypass { "Every tool runs at once, the ones that delete things too." } else { "Looking runs. Anything that changes something waits for your yes." };
        body = body.child(named("Permissions", packed(modes).child(line(theme, means))));

        div().flex().flex_col().gap(px(10.)).child(section_title(theme, "New chats", None)).child(body).child(line(theme, "What a new chat starts with. The menus under the chat's box pick the same, for the chat you're in.")).into_any_element()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn models(many: usize) -> Vec<catalog::Model> {
        (0..many).map(|n| catalog::Model { id: format!("maker/model-{n}"), name: format!("Maker: Model {n}"), efforts: Vec::new(), effort: None, fast: false }).collect()
    }

    #[test]
    fn a_long_list_shows_what_is_offered_and_its_first_few() {
        let all = models(316);
        let ids = |list: &[&catalog::Model]| list.iter().map(|m| m.id.trim_start_matches("maker/model-").to_string()).collect::<Vec<_>>().join(" ");
        // Nothing offered yet: the first few, in the list's own order (the most used first).
        let none = shown(&all, |_| false, "", 0);
        assert_eq!((ids(&none.offered), ids(&none.rest), none.left), (String::new(), "0 1 2 3 4 5 6 7".to_string(), 308));
        // What's offered is all there, wherever it is in the list, and isn't among the rest.
        let some = shown(&all, |m| m.id.ends_with("-3") || m.id.ends_with("-200"), "  ", 0);
        assert_eq!((ids(&some.offered), ids(&some.rest), some.left), ("3 200".to_string(), "0 1 2 4 5 6 7 8".to_string(), 306));
        // "Show more" adds to the few, as far as the list goes.
        assert_eq!((shown(&all, |_| false, "", MORE).rest.len(), shown(&all, |_| false, "", MORE).left), (28, 288));
        assert_eq!((shown(&all, |_| false, "", 1000).rest.len(), shown(&all, |_| false, "", 1000).left), (316, 0));
    }

    #[test]
    fn a_name_finds_models_anywhere_in_the_list_the_offered_ones_first() {
        let all = models(316);
        let found = shown(&all, |m| m.id.ends_with("-31"), "MODEL 31", 0);
        // By name or by id, whatever the case: 31, 310 to 315.
        assert_eq!(found.rest.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["maker/model-31", "maker/model-310", "maker/model-311", "maker/model-312", "maker/model-313", "maker/model-314", "maker/model-315"]);
        assert!(found.offered.is_empty() && found.left == 0);
        let offered_first = shown(&all, |m| m.id.ends_with("-315"), "model-31", 0);
        assert_eq!(offered_first.rest[0].id, "maker/model-315");
        // Too many by that name: some of them, and how many more.
        let many = shown(&all, |_| false, "model", 0);
        assert_eq!((many.rest.len(), many.left), (MOST_FOUND, 316 - MOST_FOUND));
        assert!(shown(&all, |_| false, "nothing like it", 0).rest.is_empty());
    }

    #[test]
    fn options_fill_a_row_and_go_on_to_the_next() {
        assert!(breaks(&[]).is_empty());
        assert_eq!(breaks(&[100., 100., 100.]), [3]);
        // 200 wide each, 6 between: two to a row of 540.
        assert_eq!(breaks(&[200.; 5]), [2, 2, 1]);
        // One wider than a row has a row to itself.
        assert_eq!(breaks(&[100., 700., 100., 100.]), [1, 1, 2]);
        assert_eq!(option_width("Fast"), 46.);
    }
}
