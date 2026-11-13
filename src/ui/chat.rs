//! The assistant's chat: the app's first page. One conversation with a
//! model that sees and works with everything Muxin does (see crate::chat).
//! What it says, the tools it calls, and its questions before a call that
//! changes something, each with its answers as buttons.
//!
//! The chat itself runs in a task of its own (chat::live): this page shows
//! what that reports and sends it what the user types.

use std::sync::Arc;

use gpui::{AnyElement, ClipboardItem, Context, ElementId, Entity, FocusHandle, Focusable, FontWeight, ScrollHandle, ScrollWheelEvent, SharedString, Subscription, Task, Window, div, prelude::*, px};
use serde_json::Value;

use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, card, icon, spinner};
use super::markdown::markdown;
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::chat::gate::{Answer, Mode, Request};
use crate::chat::live::{Event, Live};
use crate::chat::risk::{self, Risk};
use crate::chat::store::{Item, Meta};
use crate::chat::{Options, Update, brain, term};
use crate::fleet::Engine;

/// The widest the conversation gets: longer lines are hard to read.
const MAX_WIDTH: f32 = 760.;
const PADDING: f32 = 28.;
/// Chats offered to go on with, on an empty page.
const RECENT: usize = 5;
/// How much of a tool's result is shown when its row is opened.
const SHOW_RESULT: usize = 3000;

/// What to ask on an empty page: the label, and what's sent.
const STARTERS: [(&str, &str); 3] = [
    ("What needs me?", "What needs me right now? Check the machines, the agent sessions and GitHub, and keep it short."),
    ("What are my agents doing?", "What are my agent sessions and boxes doing right now? One line each."),
    ("Is anything offline?", "Are any machines or runners offline? Say which, and why if you can tell."),
];

/// One thing in the conversation, as it's shown.
#[derive(Clone, Debug, PartialEq)]
enum Row {
    User(String),
    /// What the assistant said; `writing` while more of it is coming.
    Said { text: String, writing: bool },
    /// A tool it called, and what came back (text, whether it failed).
    Tool { id: String, name: String, args: Value, result: Option<(String, bool)>, open: bool },
    /// A call that waits for the user, or what they answered.
    Ask { request: Request, answer: Option<Answer> },
    /// A small line: what a turn used, a plan's limit.
    Note(String),
    Failed(String),
}

pub struct ChatView {
    engine: Arc<Engine>,
    input: Entity<TextInput>,
    /// The chat that's open, once something was said or an earlier one was opened.
    live: Option<Live>,
    meta: Option<Meta>,
    rows: Vec<Row>,
    /// A turn is running.
    busy: bool,
    mode: Mode,
    /// Why no chat can be opened (no brain on this computer), in words.
    trouble: Option<String>,
    /// The page's width, from the window (text only wraps in a width it knows).
    width: f32,
    scroll: ScrollHandle,
    /// Keep the newest line in view (until the user scrolls up).
    stick: bool,
    listening: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl Focusable for ChatView {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl ChatView {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new(cx, "Ask about your machines, or ask for something to be done").bare());
        let subs = vec![cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::Submit => this.submit(window, cx),
            // The send button follows whether there's something to send.
            InputEvent::Changed => cx.notify(),
            InputEvent::Cancel => {}
        })];
        let mode = Mode::from_setting(&engine.settings().chat.mode);
        let mut view = ChatView { engine, input, live: None, meta: None, rows: Vec::new(), busy: false, mode, trouble: None, width: 800., scroll: ScrollHandle::new(), stick: true, listening: None, _subs: subs };
        // The chat used last opens with the page.
        if let Some(last) = crate::chat::store(&view.engine).list().into_iter().next() {
            view.open(Some(last.id), window, cx);
        }
        view
    }

    /// The page's width, set by the window's layout before each draw.
    pub fn set_width(&mut self, width: f32) {
        self.width = width;
    }

    /// Opens a chat from before (`Some(id)`) or a new one. False when it
    /// couldn't; `trouble` says why.
    fn open(&mut self, chat: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.close();
        match Live::open(&self.engine, Options { chat, brain: None, mode: Some(self.mode) }) {
            Ok((live, meta, thread, mut events)) => {
                self.rows = rows_from(thread);
                self.meta = Some(meta);
                self.live = Some(live);
                self.stick = true;
                self.listening = Some(cx.spawn_in(window, async move |this, cx| {
                    while let Some(first) = events.recv().await {
                        // Everything that's arrived: one redraw for a burst of words.
                        let mut batch = vec![first];
                        while let Ok(more) = events.try_recv() {
                            batch.push(more);
                        }
                        let alive = this.update(cx, |this, cx| {
                            for event in batch {
                                this.heard(event);
                            }
                            cx.notify();
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                }));
                cx.notify();
                true
            }
            Err(why) => {
                self.trouble = Some(why);
                cx.notify();
                false
            }
        }
    }

    /// Closes the chat that's open (it's kept, to go on with later).
    fn close(&mut self) {
        self.live = None;
        self.listening = None;
        self.meta = None;
        self.rows.clear();
        self.busy = false;
        self.trouble = None;
    }

    fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close();
        window.focus(&self.input.focus_handle(cx));
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.say(text, window, cx);
        self.input.update(cx, |input, cx| input.set_text("", cx));
    }

    /// Says `text` to the assistant, in the open chat or a new one.
    pub(super) fn say(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.live.is_none() && !self.open(None, window, cx) {
            return;
        }
        if let Some(live) = &self.live {
            live.send(text);
            self.stick = true;
        }
    }

    fn stop(&mut self) {
        if let Some(live) = &self.live {
            live.stop();
        }
    }

    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            Mode::Ask => Mode::Bypass,
            Mode::Bypass => Mode::Ask,
        };
        if let Some(live) = &self.live {
            live.gate().set_mode(self.mode);
        }
        cx.notify();
    }

    /// The user's answer to a waiting call.
    fn answer(&mut self, id: u64, answer: Answer, cx: &mut Context<Self>) {
        if let Some(live) = &self.live {
            live.gate().answer(id, answer.clone());
        }
        for row in &mut self.rows {
            if let Row::Ask { request, answer: given } = row
                && request.id == id
            {
                *given = Some(answer.clone());
            }
        }
        cx.notify();
    }

    /// Something the running chat reported.
    fn heard(&mut self, event: Event) {
        match event {
            Event::Started(text) => {
                self.rows.push(Row::User(text));
                self.busy = true;
                self.stick = true;
            }
            Event::Update(update) => apply(&mut self.rows, update),
            Event::Ended(outcome) => {
                self.busy = false;
                settle(&mut self.rows);
                match outcome {
                    Ok(()) => {}
                    Err(why) if why == "Stopped." => self.rows.push(Row::Note("Stopped.".into())),
                    Err(why) => self.rows.push(Row::Failed(why)),
                }
            }
            Event::Meta(meta) => self.meta = Some(meta),
        }
    }
}

/// Adds what happened in a turn to the rows shown.
fn apply(rows: &mut Vec<Row>, update: Update) {
    match update {
        Update::Delta(piece) => match rows.last_mut() {
            Some(Row::Said { text, writing: true }) => text.push_str(&piece),
            _ => rows.push(Row::Said { text: piece, writing: true }),
        },
        Update::Text(whole) => match rows.last_mut() {
            // Its pieces were shown as they came: the whole replaces them.
            Some(Row::Said { text, writing }) if *writing => {
                *text = whole;
                *writing = false;
            }
            _ if whole.trim().is_empty() => {}
            _ => rows.push(Row::Said { text: whole, writing: false }),
        },
        Update::Tool { id, name, args } => {
            settle(rows);
            push_tool(rows, id, name, args);
        }
        Update::Result { id, text, is_error } => {
            for row in rows.iter_mut().rev() {
                if let Row::Tool { id: call, result, .. } = row
                    && *call == id
                {
                    *result = Some((kept(text), is_error));
                    break;
                }
            }
        }
        Update::Ask(request) => {
            settle(rows);
            rows.push(Row::Ask { request, answer: None });
        }
        Update::Usage { input, output, cost, .. } => {
            let cost = cost.map(|c| format!(" · ${c:.2} at API prices")).unwrap_or_default();
            rows.push(Row::Note(format!("{} tokens in, {} out{cost}", term::count(input), term::count(output))));
        }
        Update::Limit(text) => rows.push(Row::Note(text)),
    }
}

/// What's kept of a tool's result for its row: as much as opening it shows.
fn kept(text: String) -> String {
    if text.len() <= SHOW_RESULT {
        return text;
    }
    let mut cut = SHOW_RESULT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…", &text[..cut])
}

/// Adds a tool's row: before the question about that very call, when the
/// question was heard first (the gate asks before some brains say they called).
fn push_tool(rows: &mut Vec<Row>, id: String, name: String, args: Value) {
    let same = |row: Option<&Row>| match row {
        Some(Row::Ask { request, .. }) => request.tool == name && request.args == args,
        Some(Row::Tool { name: other, args: others, .. }) => *other == name && *others == args,
        _ => false,
    };
    // A question about this call, with no row of the call above it yet.
    let asked_first = matches!(rows.last(), Some(Row::Ask { .. })) && same(rows.last()) && !(rows.len() >= 2 && matches!(rows[rows.len() - 2], Row::Tool { .. }) && same(rows.get(rows.len() - 2)));
    let at = if asked_first { rows.len() - 1 } else { rows.len() };
    rows.insert(at, Row::Tool { id, name, args, result: None, open: false });
}

/// Nothing more is coming for the text being written.
fn settle(rows: &mut [Row]) {
    if let Some(Row::Said { writing, .. }) = rows.last_mut() {
        *writing = false;
    }
}

/// A chat's kept thread as rows, for one opened again.
fn rows_from(thread: Vec<Item>) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for item in thread {
        match item {
            Item::User { text, .. } => rows.push(Row::User(text)),
            Item::Text { text, .. } => rows.push(Row::Said { text, writing: false }),
            Item::Tool { id, name, args, .. } => push_tool(&mut rows, id, name, args),
            Item::Result { id, text, is_error, .. } => {
                for row in rows.iter_mut().rev() {
                    if let Row::Tool { id: call, result, .. } = row
                        && *call == id
                    {
                        *result = Some((kept(text), is_error));
                        break;
                    }
                }
            }
            Item::Ask { id, tool, args, risk, .. } => {
                let rule = risk::rule(&tool, &args);
                rows.push(Row::Ask { request: Request { id, tool, args, risk, rule }, answer: None });
            }
            Item::Answer { id, answer, note, .. } => {
                let said = match answer.as_str() {
                    "yes" => Answer::Yes,
                    "always" => Answer::Always,
                    _ => Answer::No(note),
                };
                for row in rows.iter_mut().rev() {
                    if let Row::Ask { request, answer } = row
                        && request.id == id
                        && answer.is_none()
                    {
                        *answer = Some(said);
                        break;
                    }
                }
            }
            Item::Usage { .. } => {}
            Item::Error { text, .. } if text == "Stopped." => rows.push(Row::Note(text)),
            Item::Error { text, .. } => rows.push(Row::Failed(text)),
        }
    }
    // From a chat that closed mid-turn: nothing is waiting any more.
    for row in &mut rows {
        match row {
            Row::Ask { answer, .. } if answer.is_none() => *answer = Some(Answer::No(String::new())),
            Row::Tool { result, .. } if result.is_none() => *result = Some((String::new(), false)),
            _ => {}
        }
    }
    rows
}

/// The rows that are a turn's last words (its answer, not what it said on
/// the way): the last thing said before the user spoke again, or before the
/// end, unless that turn is still running.
fn last_words(rows: &[Row], busy: bool) -> Vec<usize> {
    let mut found = Vec::new();
    let mut last: Option<usize> = None;
    for (n, row) in rows.iter().enumerate() {
        match row {
            Row::User(_) => found.extend(last.take()),
            Row::Said { .. } => last = Some(n),
            _ => {}
        }
    }
    if !busy {
        found.extend(last);
    }
    found
}

fn row_id(name: &'static str, n: usize) -> ElementId {
    ElementId::NamedInteger(SharedString::new_static(name), n as u64)
}

impl ChatView {
    fn said(&self, theme: &Theme, n: usize, text: &str, copyable: bool, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let copy = text.to_string();
        div()
            .w(px(width))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(markdown(theme, text, width, format!("chat-said-{n}"), ""))
            .when(copyable && !text.trim().is_empty(), |d| {
                d.child(div().flex().child(Button::icon_only(row_id("chat-copy", n), "copy").tooltip("Copy").on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))))))
            })
            .into_any_element()
    }

    fn tool(&self, theme: &Theme, n: usize, name: &str, args: &Value, result: &Option<(String, bool)>, open: bool, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let failed = result.as_ref().filter(|(_, is_error)| *is_error).and_then(|(text, _)| term::failure(text));
        let mut body = div().w(px(width)).flex().flex_col().gap(px(6.)).child(
            div()
                .id(row_id("chat-tool", n))
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(24.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(icon(if open { "chevron-down" } else { "chevron-right" }, 12., theme.text_placeholder))
                .child(div().flex_1().min_w(px(0.)).overflow_hidden().whitespace_nowrap().font_family(mono_font()).child(term::brief(name, args)))
                .when(result.is_none(), |d| d.child(spinner(row_id("chat-tool-spin", n), 12., theme.text_placeholder)))
                .when(failed.is_some(), |d| d.child(icon("circle-x", 12., theme.error)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(Row::Tool { open, .. }) = this.rows.get_mut(n) {
                        *open = !*open;
                        this.stick = false;
                        cx.notify();
                    }
                })),
        );
        if let Some(why) = &failed {
            body = body.child(div().pl(px(20.)).text_size(px(12.)).text_color(theme.error).child(why.clone()));
        }
        if open {
            let mut lines = risk::describe(name, args);
            lines.remove(0);
            if let Some((text, _)) = result
                && !text.trim().is_empty()
            {
                lines.push(String::new());
                lines.extend(text.lines().map(str::to_string));
            }
            if lines.is_empty() {
                lines.push("No arguments.".into());
            }
            body = body.child(
                div()
                    .ml(px(20.))
                    .w(px(width - 20.))
                    .p(px(10.))
                    .rounded(kit::corner(6.))
                    .bg(theme.element)
                    .font_family(mono_font())
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .overflow_hidden()
                    .children(lines.into_iter().map(|l| div().child(if l.is_empty() { " ".to_string() } else { l }))),
            );
        }
        body.into_any_element()
    }

    /// A call that waits for the user: what it is, and the answers as buttons.
    /// Once answered, one small line saying what they said.
    fn ask(&self, theme: &Theme, n: usize, request: &Request, answer: &Option<Answer>, width: f32, cx: &mut Context<Self>) -> AnyElement {
        if let Some(answer) = answer {
            let (glyph, color, said) = match answer {
                Answer::Yes => ("check", theme.success, "You said yes."),
                Answer::Always => ("check", theme.success, "You said yes, and not to ask again."),
                Answer::No(_) => ("x", theme.text_placeholder, "You said no, so it didn't run."),
            };
            return div().w(px(width)).pl(px(20.)).flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(theme.text_muted).child(icon(glyph, 12., color)).child(said).into_any_element();
        }
        let danger = request.risk == Risk::Danger;
        let edge = if danger { theme.error } else { theme.warning };
        let mut lines = risk::describe(&request.tool, &request.args);
        let tool = lines.remove(0);
        let mut body = card(theme).w(px(width)).p(px(14.)).gap(px(10.)).border_color(edge.opacity(0.6)).child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(icon(if danger { "triangle-alert" } else { "lock" }, 14., edge))
                .child(if danger { "Run this? It can't simply be undone." } else { "Run this?" }),
        );
        body = body.child(
            div()
                .p(px(10.))
                .rounded(kit::corner(6.))
                .bg(theme.element)
                .font_family(mono_font())
                .text_size(px(12.))
                .text_color(theme.text)
                .overflow_hidden()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(tool))
                .children(lines.into_iter().map(|l| div().text_color(theme.text_muted).child(l))),
        );
        let id = request.id;
        let mut buttons = div().flex().items_center().gap(px(8.)).child(Button::new(row_id("chat-yes", n), "Yes").style(ButtonStyle::Primary).on_click(cx.listener(move |this, _, _, cx| this.answer(id, Answer::Yes, cx))));
        if let Some(rule) = &request.rule {
            buttons = buttons.child(
                Button::new(row_id("chat-always", n), "Yes, and don't ask again")
                    .tooltip(format!("Runs {rule} without asking from now on"))
                    .on_click(cx.listener(move |this, _, _, cx| this.answer(id, Answer::Always, cx))),
            );
        }
        body.child(buttons.child(Button::new(row_id("chat-no", n), "No").style(ButtonStyle::Ghost).on_click(cx.listener(move |this, _, _, cx| this.answer(id, Answer::No(String::new()), cx))))).into_any_element()
    }

    /// An empty page: what to ask, and chats to go on with.
    fn start(&self, theme: &Theme, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let mut body = div().w(px(width)).pt(px(48.)).flex().flex_col().gap(px(18.)).child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Ask Muxin"))
                .child(div().w(px(width)).text_size(px(13.)).text_color(theme.text_muted).child(match self.mode {
                    Mode::Ask => "It works across all your machines, and asks before it changes anything.",
                    Mode::Bypass => "It works across all your machines, without asking first.",
                })),
        );
        let mut starters = div().flex().flex_wrap().gap(px(8.));
        for (n, (label, text)) in STARTERS.into_iter().enumerate() {
            starters = starters.child(Button::new(row_id("chat-starter", n), label).on_click(cx.listener(move |this, _, window, cx| this.say(text.to_string(), window, cx))));
        }
        body = body.child(starters);

        let recent: Vec<Meta> = crate::chat::store(&self.engine).list().into_iter().filter(|m| !m.title.is_empty()).take(RECENT).collect();
        if !recent.is_empty() {
            let now = crate::chat::store::now();
            let mut list = card(theme).w(px(width)).overflow_hidden();
            for (n, meta) in recent.into_iter().enumerate() {
                let id = meta.id.clone();
                list = list.child(
                    div()
                        .id(row_id("chat-recent", n))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .h(px(34.))
                        .px(px(12.))
                        .cursor_pointer()
                        .text_size(px(13.))
                        .text_color(theme.text)
                        .when(n > 0, |d| d.border_t_1().border_color(theme.border_variant))
                        .hover(|st| st.bg(theme.ghost_hover))
                        .child(icon("message-square", 13., theme.text_placeholder))
                        .child(div().flex_1().min_w(px(0.)).overflow_hidden().whitespace_nowrap().child(meta.title.clone()))
                        .child(div().flex_none().text_size(px(11.)).text_color(theme.text_placeholder).child(format!("{} ago", crate::util::short_duration((now - meta.updated).max(0) as u64))))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open(Some(id.clone()), window, cx);
                        })),
                );
            }
            body = body.child(div().flex().flex_col().gap(px(8.)).child(kit::label(theme, "Go on with")).child(list));
        }
        body.into_any_element()
    }

    /// No brain on this computer: what's needed, and where to get it.
    fn needs(&self, theme: &Theme, why: &str, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let mut buttons = div().flex().items_center().gap(px(8.));
        for (n, kind) in brain::KINDS.iter().enumerate() {
            let url = kind.get;
            buttons = buttons.child(Button::new(row_id("chat-get", n), format!("Get {}", kind.name)).icon("external-link").on_click(cx.listener(move |_, _, _, cx| cx.open_url(url))));
        }
        buttons = buttons.child(Button::new("chat-retry", "Try again").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, _, cx| {
            this.trouble = None;
            cx.notify();
        })));
        card(theme)
            .w(px(width))
            .mt(px(48.))
            .p(px(16.))
            .gap(px(12.))
            .child(div().flex().items_center().gap(px(8.)).text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(icon("circle-alert", 15., theme.warning)).child("The assistant can't start yet"))
            .child(div().w(px(width - 32.)).text_size(px(13.)).text_color(theme.text_muted).child(why.to_string()))
            .child(buttons)
            .into_any_element()
    }

    fn composer(&self, theme: &Theme, width: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.input.focus_handle(cx).is_focused(window);
        let has_text = !self.input.read(cx).text().trim().is_empty();
        let bypass = self.mode == Mode::Bypass;
        let name = self.meta.as_ref().and_then(|m| brain::kind(&m.brain)).or_else(|| brain::installed().into_iter().next()).map(|k| k.name);
        let action = if self.busy && !has_text {
            Button::icon_only("chat-stop", "square").style(ButtonStyle::Outlined).tooltip("Stop").on_click(cx.listener(|this, _, _, _| this.stop()))
        } else {
            Button::icon_only("chat-send", "arrow-up").style(ButtonStyle::Primary).disabled(!has_text).tooltip("Send").on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };
        div()
            .w(px(width))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(44.))
                    .pl(px(14.))
                    .pr(px(8.))
                    .rounded(kit::corner(10.))
                    .border_1()
                    .border_color(if bypass { theme.warning.opacity(0.7) } else if focused { theme.border_focused } else { theme.border })
                    .bg(theme.surface)
                    .child(div().flex_1().min_w(px(0.)).child(self.input.clone()))
                    .child(action),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        Button::new("chat-mode", if bypass { "Doesn't ask first" } else { "Asks first" })
                            .icon(if bypass { "triangle-alert" } else { "lock" })
                            .style(ButtonStyle::Ghost)
                            .tooltip(if bypass { "Every tool runs without asking, the ones that delete things too. Click to have it ask." } else { "It asks before a tool changes anything. Click to let it run without asking." })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_mode(cx))),
                    )
                    .children(name.map(|name| div().px(px(6.)).text_size(px(12.)).text_color(theme.text_placeholder).child(name)))
                    .child(div().flex_1())
                    .when(self.live.is_some(), |d| d.child(Button::new("chat-new", "New chat").icon("plus").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))))),
            )
            .into_any_element()
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let width = (self.width - PADDING * 2.).clamp(280., MAX_WIDTH);
        if self.stick {
            self.scroll.scroll_to_bottom();
        }

        let mut thread = div().w(px(width)).flex().flex_col().gap(px(14.));
        if let Some(why) = self.trouble.clone() {
            thread = thread.child(self.needs(&theme, &why, width, cx));
        } else if self.rows.is_empty() {
            thread = thread.child(self.start(&theme, width, cx));
        }
        let rows = self.rows.clone();
        let answers = last_words(&rows, self.busy);
        for (n, row) in rows.iter().enumerate() {
            thread = thread.child(match row {
                Row::User(text) => div()
                    .w(px(width))
                    .px(px(12.))
                    .py(px(9.))
                    .rounded(kit::corner(10.))
                    .bg(theme.element)
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(div().w(px(width - 24.)).child(text.clone()))
                    .into_any_element(),
                Row::Said { text, .. } => self.said(&theme, n, text, answers.contains(&n), width, cx),
                Row::Tool { name, args, result, open, .. } => self.tool(&theme, n, name, args, result, *open, width, cx),
                Row::Ask { request, answer } => self.ask(&theme, n, request, answer, width, cx),
                Row::Note(text) => div().w(px(width)).text_size(px(11.5)).text_color(theme.text_placeholder).child(text.clone()).into_any_element(),
                Row::Failed(why) => div()
                    .w(px(width))
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .child(div().flex_none().h(px(18.)).flex().items_center().child(icon("circle-alert", 14., theme.error)))
                    .child(div().flex_1().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text).child(why.clone()))
                    .into_any_element(),
            });
        }
        // Thinking, with nothing on screen to show for it yet.
        let quiet = !matches!(rows.last(), Some(Row::Said { writing: true, .. }) | Some(Row::Tool { result: None, .. }));
        let waits_for_you = rows.iter().any(|row| matches!(row, Row::Ask { answer: None, .. }));
        if self.busy && quiet && !waits_for_you {
            thread = thread.child(div().w(px(width)).flex().items_center().gap(px(8.)).text_size(px(12.)).text_color(theme.text_placeholder).child(spinner("chat-working", 12., theme.text_placeholder)).child("Working…"));
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("chat-thread")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                        // Scrolling up lets go of the newest line; back at the bottom, it's followed again.
                        let up = event.delta.pixel_delta(window.line_height()).y > px(0.);
                        let at_bottom = -this.scroll.offset().y + px(40.) >= this.scroll.max_offset().height;
                        let stick = !up && at_bottom;
                        if stick != this.stick {
                            this.stick = stick;
                            cx.notify();
                        }
                    }))
                    .flex()
                    .flex_col()
                    .items_center()
                    .px(px(PADDING))
                    .pt(px(22.))
                    .pb(px(20.))
                    .child(thread),
            )
            .child(div().flex_none().flex().flex_col().items_center().px(px(PADDING)).pb(px(18.)).child(self.composer(&theme, width, window, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_turn_becomes_rows_as_it_comes() {
        let mut rows = vec![Row::User("restart beelink-01".into())];
        let restart = json!({"runner_id": "beelink/beelink-01", "action": "restart"});
        for update in [
            Update::Delta("I'll ".into()),
            Update::Delta("restart it.".into()),
            Update::Text("I'll restart it.".into()),
            Update::Tool { id: "t1".into(), name: "runner_action".into(), args: restart.clone() },
            Update::Ask(Request { id: 7, tool: "runner_action".into(), args: restart.clone(), risk: Risk::Change, rule: Some("runner_action:restart".into()) }),
            Update::Result { id: "t1".into(), text: "restarted".into(), is_error: false },
            // A brain that sends no pieces: its text arrives whole.
            Update::Text("Done.".into()),
            Update::Text("  ".into()),
            Update::Usage { input: 12_340, output: 41, cached: 9000, cost: Some(0.05) },
        ] {
            apply(&mut rows, update);
        }
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[1], Row::Said { text: "I'll restart it.".into(), writing: false });
        assert!(matches!(&rows[2], Row::Tool { name, result: Some((text, false)), .. } if name == "runner_action" && text == "restarted"));
        assert!(matches!(&rows[3], Row::Ask { request, answer: None } if request.id == 7));
        assert_eq!(rows[4], Row::Said { text: "Done.".into(), writing: false });
        assert_eq!(rows[5], Row::Note("12.3k tokens in, 41 out · $0.05 at API prices".into()));
    }

    #[test]
    fn a_tools_row_comes_before_the_question_about_it() {
        let save = json!({"name": "Uptime", "command": "uptime"});
        let ask = |id| Request { id, tool: "snippet_save".into(), args: json!({"name": "Uptime", "command": "uptime"}), risk: Risk::Change, rule: Some("snippet_save".into()) };
        // The question was heard first (Codex): the tool's row still goes above it.
        let mut rows = vec![Row::User("save it".into())];
        apply(&mut rows, Update::Ask(ask(1)));
        apply(&mut rows, Update::Tool { id: "t1".into(), name: "snippet_save".into(), args: save.clone() });
        assert!(matches!((&rows[1], &rows[2]), (Row::Tool { .. }, Row::Ask { .. })), "{rows:?}");
        // The tool first (Claude Code): as it came. And a question about another call stays where it is.
        apply(&mut rows, Update::Tool { id: "t2".into(), name: "snippet_save".into(), args: save });
        apply(&mut rows, Update::Ask(ask(2)));
        apply(&mut rows, Update::Tool { id: "t3".into(), name: "status".into(), args: json!({}) });
        let kinds: Vec<&str> = rows.iter().map(|r| match r { Row::User(_) => "user", Row::Tool { .. } => "tool", Row::Ask { .. } => "ask", _ => "other" }).collect();
        assert_eq!(kinds, ["user", "tool", "ask", "tool", "ask", "tool"]);
    }

    #[test]
    fn copy_sits_under_a_turns_last_words_only() {
        let said = |text: &str| Row::Said { text: text.into(), writing: false };
        let rows = vec![
            Row::User("one".into()),
            said("I'll look."),
            Row::Tool { id: "t".into(), name: "status".into(), args: json!({}), result: None, open: false },
            said("Nine machines."),
            Row::Note("1 tokens in".into()),
            Row::User("two".into()),
            said("Checking…"),
        ];
        // The second turn is still running: its words so far aren't its answer yet.
        assert_eq!(last_words(&rows, true), [3]);
        assert_eq!(last_words(&rows, false), [3, 6]);
        assert!(last_words(&[], false).is_empty());
    }

    #[test]
    fn a_long_result_is_kept_only_as_far_as_its_shown() {
        let mut rows = vec![Row::Tool { id: "t".into(), name: "logs_read".into(), args: json!({}), result: None, open: false }];
        apply(&mut rows, Update::Result { id: "t".into(), text: "é".repeat(SHOW_RESULT), is_error: false });
        let Row::Tool { result: Some((text, _)), .. } = &rows[0] else { panic!("a result") };
        assert!(text.len() <= SHOW_RESULT + 4 && text.ends_with('…'), "{}", text.len());
        assert_eq!(kept("short".into()), "short");
    }

    #[test]
    fn text_cut_off_by_a_tool_call_stops_being_written() {
        let mut rows = Vec::new();
        apply(&mut rows, Update::Delta("Let me look".into()));
        apply(&mut rows, Update::Tool { id: "t".into(), name: "status".into(), args: json!({}) });
        apply(&mut rows, Update::Delta("Nine machines.".into()));
        assert_eq!(rows[0], Row::Said { text: "Let me look".into(), writing: false });
        assert_eq!(rows[2], Row::Said { text: "Nine machines.".into(), writing: true });
        settle(&mut rows);
        assert_eq!(rows[2], Row::Said { text: "Nine machines.".into(), writing: false });
    }

    #[test]
    fn a_kept_thread_opens_with_nothing_left_waiting() {
        let restart = json!({"runner_id": "a/b", "action": "restart"});
        let rows = rows_from(vec![
            Item::User { at: 1, text: "restart it".into() },
            Item::Tool { at: 2, id: "t1".into(), name: "runner_action".into(), args: restart.clone() },
            Item::Ask { at: 2, id: 1, tool: "runner_action".into(), args: restart.clone(), risk: Risk::Change },
            Item::Answer { at: 3, id: 1, answer: "no".into(), note: "mid-job".into() },
            Item::Result { at: 3, id: "t1".into(), text: "declined".into(), is_error: true },
            Item::Text { at: 4, text: "Left it alone.".into() },
            Item::Usage { at: 4, input: 1, output: 1, cached: 0, cost: None },
            Item::User { at: 5, text: "ok, do it".into() },
            // The app quit while these two waited.
            Item::Tool { at: 6, id: "t2".into(), name: "runner_action".into(), args: restart.clone() },
            Item::Ask { at: 6, id: 2, tool: "runner_action".into(), args: restart, risk: Risk::Change },
            Item::Error { at: 7, text: "Stopped.".into() },
            Item::Error { at: 8, text: "Claude Code ended before it answered.".into() },
        ]);
        assert!(matches!(&rows[2], Row::Ask { request, answer: Some(Answer::No(note)) } if request.rule.as_deref() == Some("runner_action:restart") && note == "mid-job"));
        assert!(matches!(&rows[1], Row::Tool { result: Some((_, true)), .. }));
        assert!(matches!(&rows[5], Row::Tool { result: Some(_), .. }));
        assert!(matches!(&rows[6], Row::Ask { answer: Some(Answer::No(_)), .. }));
        assert_eq!(rows[7], Row::Note("Stopped.".into()));
        assert_eq!(rows[8], Row::Failed("Claude Code ended before it answered.".into()));
        assert_eq!(rows.len(), 9);
    }
}
