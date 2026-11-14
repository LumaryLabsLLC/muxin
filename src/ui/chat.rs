//! The assistant's chat: the app's first page. One conversation with a
//! model that sees and works with everything Muxin does (see crate::chat).
//! What it says, the tools it calls, and its questions before a call that
//! changes something, each with its answers as buttons.
//!
//! The chat itself runs in a task of its own (chat::live): this page shows
//! what that reports and sends it what the user types.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AnyElement, ClickEvent, ClipboardItem, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, MouseButton, MouseDownEvent, Pixels, Point, ScrollHandle, ScrollWheelEvent, SharedString, Subscription, Task, Window, div, prelude::*, px};
use serde_json::Value;

use super::input::{InputEvent, ListKey, PastedOther, TextInput};
use super::kit::{self, Button, ButtonStyle, card, icon, spinner};
use super::markdown::markdown;
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::chat::attach;
use crate::chat::brain::catalog::{self, Model};
use crate::chat::gate::{Answer, Mode, Request};
use crate::chat::live::{Event, Live};
use crate::chat::risk::{self, Risk};
use crate::chat::store::{Item, Meta};
use crate::chat::{Options, Update, brain, command, mention, term};
use crate::fleet::Engine;

/// The widest the conversation gets: longer lines are hard to read.
const MAX_WIDTH: f32 = 760.;
const PADDING: f32 = 28.;
/// Chats offered to go on with, on an empty page.
const RECENT: usize = 5;
/// With "Show all": as many as a page can still hold comfortably.
const MOST_LISTED: usize = 200;
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
    /// The files that went with what the user said, by name.
    Files(Vec<String>),
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

/// What the page asks its window for: a menu where the user clicked, to
/// pick what answers (`effort`: how hard it thinks).
pub enum ChatEvent {
    Menu { effort: bool, position: Point<Pixels> },
    /// The menu that says whether it asks before a tool changes something.
    ModeMenu { position: Point<Pixels> },
    /// A chat's own menu (rename it, delete it). `above`: it opens upward.
    ChatMenu { id: String, position: Point<Pixels>, above: bool },
    /// The dialog that names this chat; the one that asks before deleting it.
    Rename(String),
    Delete(String),
}

/// A row of the list above the box, while a "/" command or an "@" mention
/// is being typed.
enum Offer {
    Command(&'static command::Command),
    Thing(mention::Thing),
}

/// That list: its rows, what's typed so far ("@fo", "/mo"), and where in
/// the text it is (what picking a row replaces).
struct Offers {
    range: std::ops::Range<usize>,
    typed: String,
    rows: Vec<Offer>,
}

/// Rows of it shown at once, and looked through at most.
const OFFER_ROWS: usize = 8;
const MOST_OFFERED: usize = 60;

/// One line of a picker: what picking it sets (None: the brain's, or the
/// model's, own choice), and whether it's the one in use.
pub(super) struct Choice {
    pub id: Option<String>,
    pub name: String,
    pub picked: bool,
}

/// A brain in the model picker, with its models.
pub(super) struct BrainChoice {
    pub id: &'static str,
    pub name: &'static str,
    /// It's what answers now.
    pub current: bool,
    /// The model it answers with, or would, when one is picked.
    pub model: Option<String>,
    pub models: Vec<Choice>,
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
    /// What answers in the next new chat, when the user chose (else the first there is).
    brain_choice: Option<String>,
    /// The model answering in the open chat, once its brain said.
    model: Option<String>,
    /// The files that go with the next message, and what's said under the
    /// box: why a file can't be attached, or a command couldn't be done.
    attached: Vec<PathBuf>,
    under_box: Option<String>,
    /// The list above the box (see `Offers`): the row that's picked, and
    /// what was typed when the user closed it with Escape, so it stays
    /// closed until they type on.
    list_at: usize,
    list_closed: Option<String>,
    /// The empty page lists every chat, not only the latest few.
    all_chats: bool,
    /// Each program's models, as that program lists them on this computer
    /// (read when the page opens and when it's shown again, not on every draw).
    catalog: Vec<(&'static str, Vec<Model>)>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ChatEvent> for ChatView {}

impl Focusable for ChatView {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl ChatView {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Several lines: it grows as far as eight rows, then scrolls. Enter sends, Shift+Enter starts a new line.
        let input = cx.new(|cx| TextInput::new(cx, "Ask Muxin. @ to mention, / for commands").bare().lines(8));
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Submit => this.submit(window, cx),
                // The send button follows whether there's something to send,
                // and the list above the box what's being typed.
                InputEvent::Changed => {
                    this.list_at = 0;
                    cx.notify();
                }
                InputEvent::Cancel => {}
            }),
            // The keys that list takes while it's open.
            cx.subscribe_in(&input, window, |this, _, key: &ListKey, window, cx| this.list_key(*key, window, cx)),
            // It follows the cursor too, not only the text.
            cx.observe(&input, |this, _, cx| this.sync_list(cx)),
            // A picture pasted where text goes is attached instead.
            cx.subscribe_in(&input, window, |this, _, _: &PastedOther, _, cx| this.attach_pasted(cx)),
        ];
        let mode = Mode::from_setting(&engine.settings().chat.mode);
        let mut view = ChatView {
            engine,
            input,
            live: None,
            meta: None,
            rows: Vec::new(),
            busy: false,
            mode,
            trouble: None,
            width: 800.,
            scroll: ScrollHandle::new(),
            stick: true,
            listening: None,
            brain_choice: None,
            model: None,
            attached: Vec::new(),
            under_box: None,
            list_at: 0,
            list_closed: None,
            all_chats: false,
            catalog: Vec::new(),
            _subs: subs,
        };
        view.load_models();
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
        // A chat from before goes on with its own brain; a new one takes the one chosen.
        let brain = if chat.is_none() { self.brain_choice.clone() } else { None };
        match Live::open(&self.engine, Options { chat, brain, mode: Some(self.mode), ..Default::default() }) {
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

    /// The open chat's title, once it has one.
    pub(super) fn title(&self) -> Option<String> {
        self.meta.as_ref().filter(|_| self.live.is_some()).map(|meta| meta.title.clone()).filter(|title| !title.is_empty())
    }

    /// The open chat as it is on disk now, after a dialog that may have
    /// renamed or deleted it: its new title, or the page lets go of it (and
    /// what was running in it ends).
    pub(super) fn refresh_open(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = &mut self.meta {
            match crate::chat::store(&self.engine).get(&open.id) {
                Some(kept) => open.title = kept.title,
                None => self.close(),
            }
        }
        cx.notify();
    }

    /// Closes the chat that's open (it's kept, to go on with later).
    fn close(&mut self) {
        self.live = None;
        self.listening = None;
        self.meta = None;
        self.rows.clear();
        self.busy = false;
        self.trouble = None;
        self.model = None;
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
        // A command is done, not said.
        if let Some((command, rest)) = command::parse(&text).filter(|(command, _)| command.app) {
            self.input.update(cx, |input, cx| input.set_text("", cx));
            return self.run_command(command, rest, window, cx);
        }
        let files = std::mem::take(&mut self.attached);
        self.under_box = None;
        self.say_with(text, files, window, cx);
        self.input.update(cx, |input, cx| input.set_text("", cx));
    }

    /// What's offered for what's being typed where the cursor is: commands
    /// after a "/" that starts the message, things to mention after an "@".
    fn offers(&self, cx: &gpui::App) -> Option<Offers> {
        let input = self.input.read(cx);
        let (text, cursor) = (input.text(), input.cursor());
        let (range, typed, rows): (_, _, Vec<Offer>) = if let Some(so_far) = command::typing(text, cursor) {
            // A pick replaces the whole word, wherever the cursor is in it.
            let end = text.find(char::is_whitespace).unwrap_or(text.len());
            (0..end, format!("/{so_far}"), command::matching(so_far, true).into_iter().map(Offer::Command).collect())
        } else if let Some((at, so_far)) = mention::typing(text, cursor) {
            let all = mention::things(&self.engine.current());
            let rows = mention::matching(&all, so_far).into_iter().take(MOST_OFFERED).cloned().map(Offer::Thing).collect();
            (at..mention::end(text, cursor), format!("@{so_far}"), rows)
        } else {
            return None;
        };
        (!rows.is_empty() && self.list_closed.as_deref() != Some(typed.as_str())).then_some(Offers { range, typed, rows })
    }

    /// Tells the field whether the list is open (its keys go to the list
    /// then), after the text or the cursor changed.
    fn sync_list(&mut self, cx: &mut Context<Self>) {
        let open = self.offers(cx).is_some();
        self.input.update(cx, |input, _| input.set_listing(open));
        cx.notify();
    }

    /// A key for the list: up and down move in it (round and round), Enter
    /// or Tab takes the row that's picked, Escape closes it.
    fn list_key(&mut self, key: ListKey, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(offers) = self.offers(cx) {
            let rows = offers.rows.len();
            let at = self.list_at.min(rows - 1);
            match key {
                ListKey::Up => self.list_at = (at + rows - 1) % rows,
                ListKey::Down => self.list_at = (at + 1) % rows,
                ListKey::Close => self.list_closed = Some(offers.typed),
                ListKey::Take => self.take_offer(at, window, cx),
            }
        }
        self.sync_list(cx);
    }

    /// Takes a row of the list: a mention goes into the text by its id, a
    /// command is done at once.
    fn take_offer(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(offers) = self.offers(cx) else { return };
        let text = self.input.read(cx).text().to_string();
        match offers.rows.into_iter().nth(row) {
            Some(Offer::Thing(thing)) => {
                // A space after it, to type on, unless one is there already.
                let space = if text[offers.range.end..].starts_with(char::is_whitespace) { "" } else { " " };
                self.input.update(cx, |input, cx| input.replace(offers.range, &format!("@{}{space}", thing.id), cx));
            }
            Some(Offer::Command(command)) => {
                let rest = text[offers.range.end..].trim().to_string();
                self.input.update(cx, |input, cx| input.set_text("", cx));
                self.run_command(command, &rest, window, cx);
            }
            None => {}
        }
        self.list_at = 0;
        window.focus(&self.input.focus_handle(cx));
        self.sync_list(cx);
    }

    /// Does a command (see crate::chat::command), with what followed its name.
    fn run_command(&mut self, command: &'static command::Command, rest: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.under_box = None;
        let open = self.meta.as_ref().filter(|_| self.has_chat()).map(|meta| meta.id.clone());
        match (command.name, rest, open) {
            ("new", ..) => self.new_chat(window, cx),
            ("chats", ..) => {
                self.all_chats = true;
                self.new_chat(window, cx);
            }
            ("model", "", _) => cx.emit(ChatEvent::Menu { effort: false, position: self.picker_anchor(false, window) }),
            ("model", said, _) => self.model_said(said, window, cx),
            ("effort", "", _) => match self.effort_label() {
                Some(_) => cx.emit(ChatEvent::Menu { effort: true, position: self.picker_anchor(true, window) }),
                None => self.under_box = Some("This model doesn't take an effort.".into()),
            },
            ("effort", said, _) => self.effort_said(said, cx),
            ("ask", ..) => self.set_bypass(false, cx),
            ("bypass", ..) => self.set_bypass(true, cx),
            ("attach", "", _) => self.pick_files(window, cx),
            ("attach", named, _) => self.attach(vec![command::path(named)], cx),
            ("stop", ..) => self.stop(),
            ("rename" | "delete", _, None) => self.under_box = Some("There's no chat here yet.".into()),
            ("delete", _, Some(id)) => cx.emit(ChatEvent::Delete(id)),
            ("rename", "", Some(id)) => cx.emit(ChatEvent::Rename(id)),
            ("rename", title, Some(id)) => match crate::chat::store(&self.engine).rename(&id, title) {
                Ok(_) => self.refresh_open(cx),
                Err(why) => self.under_box = Some(format!("Couldn't rename it: {why}")),
            },
            _ => {}
        }
        cx.notify();
    }

    /// `/model <said>`: a program by its name ("codex", "Grok Build"), or a
    /// model by its own: one of the answering program's first, then another's.
    fn model_said(&mut self, said: &str, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = said.trim().to_lowercase();
        let here = brain::available();
        let current = self.brain().map(|kind| kind.id);
        if let Some(kind) = here.iter().find(|kind| kind.id == wanted || kind.name.to_lowercase() == wanted) {
            return self.pick(kind.id, None, window, cx);
        }
        if let (Some(current), "default") = (current, wanted.as_str()) {
            return self.pick(current, None, window, cx);
        }
        let ordered = here.iter().filter(|kind| Some(kind.id) == current).chain(here.iter().filter(|kind| Some(kind.id) != current));
        let found = ordered.into_iter().find_map(|kind| catalog::named(self.models_of(kind.id), said).map(|model| (kind.id, model.id.clone())));
        match found {
            Some((brain, model)) => self.pick(brain, Some(model), window, cx),
            None => self.under_box = Some(format!("There's no model called {said} here. /model lists them.")),
        }
    }

    /// `/effort <said>`: one of the levels the model takes, or "default".
    fn effort_said(&mut self, said: &str, cx: &mut Context<Self>) {
        let wanted = said.trim().to_lowercase();
        let choices = self.effort_choices();
        match choices.iter().find(|choice| choice.id.as_deref().is_some_and(|id| id == wanted) || choice.name.to_lowercase() == wanted) {
            Some(choice) => self.pick_effort(choice.id.clone(), cx),
            None if choices.is_empty() => self.under_box = Some("This model doesn't take an effort.".into()),
            None => self.under_box = Some(format!("It doesn't take {said}. /effort lists what it takes.")),
        }
    }

    /// The list above the box: what a "/" or an "@" offers, the picked row marked.
    fn offer_list(&self, theme: &Theme, width: f32, offers: &Offers, cx: &mut Context<Self>) -> AnyElement {
        let rows = offers.rows.len();
        let picked = self.list_at.min(rows - 1);
        let first = picked.saturating_sub(OFFER_ROWS - 1);
        let (marked, hover) = (theme.element_hover, theme.ghost_hover);
        let mut list = card(theme).w(px(width)).py(px(4.)).overflow_hidden();
        for (n, offer) in offers.rows.iter().enumerate().skip(first).take(OFFER_ROWS) {
            let (glyph, title, id, about) = match offer {
                Offer::Command(command) => ("terminal", format!("/{}", command.name), command.takes.to_string(), command.about.to_string()),
                Offer::Thing(thing) => {
                    let glyph = match thing.kind {
                        mention::Kind::Machine => "server",
                        mention::Kind::Session => "bot",
                        mention::Kind::Box => "box",
                        mention::Kind::Runner => "play",
                        mention::Kind::Workspace => "folder",
                        mention::Kind::Repo => "git-branch",
                    };
                    // Its id, when that isn't what it goes by: it's what lands in the message.
                    let id = if thing.name.eq_ignore_ascii_case(&thing.id) { String::new() } else { format!("@{}", thing.id) };
                    let about = if thing.kind == mention::Kind::Machine { format!("Machine · {}", thing.about) } else { thing.about.clone() };
                    (glyph, thing.name.clone(), id, about)
                }
            };
            list = list.child(
                div()
                    .id(row_id("chat-offer", n))
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .h(px(30.))
                    .mx(px(4.))
                    .px(px(8.))
                    .rounded(kit::corner(6.))
                    .cursor_pointer()
                    .when(n == picked, |d| d.bg(marked))
                    .hover(move |d| d.bg(hover))
                    .child(icon(glyph, 14., theme.text_placeholder))
                    .child(div().flex_none().max_w(px(width * 0.45)).truncate().text_size(px(13.)).text_color(theme.text).child(title))
                    .when(!id.is_empty(), |d| d.child(div().flex_none().max_w(px(width * 0.3)).truncate().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_placeholder).child(id)))
                    .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(12.)).text_color(theme.text_muted).child(about))
                    .on_click(cx.listener(move |this, _, window, cx| this.take_offer(n, window, cx))),
            );
        }
        // More than fit: where in them the picked row is.
        if rows > OFFER_ROWS {
            list = list.child(div().px(px(12.)).pt(px(4.)).pb(px(2.)).text_size(px(11.)).text_color(theme.text_placeholder).child(format!("{} of {rows}", picked + 1)));
        }
        list.into_any_element()
    }

    /// Adds files to the next message. One that can't be attached is left
    /// out, and the page says why.
    pub(super) fn attach(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.under_box = None;
        for path in paths {
            if self.attached.contains(&path) {
                continue;
            }
            if self.attached.len() >= attach::MOST_FILES {
                self.under_box = Some(format!("A message takes {} files.", attach::MOST_FILES));
                break;
            }
            match attach::kind(&path) {
                Ok(_) => self.attached.push(path),
                Err(why) => self.under_box = Some(why),
            }
        }
        cx.notify();
    }

    /// The paperclip: asks which files, in the system's own picker.
    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let asked = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Attach".into()) });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = asked.await {
                let _ = this.update(cx, |this, cx| this.attach(paths, cx));
            }
        })
        .detach();
    }

    /// A picture on the clipboard, pasted into the field: kept as a file
    /// beside the chats, and attached.
    fn attach_pasted(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        let Some(image) = item.entries().iter().find_map(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => Some(image.clone()),
            _ => None,
        }) else {
            return;
        };
        let ext = match image.format {
            gpui::ImageFormat::Png => "png",
            gpui::ImageFormat::Jpeg => "jpg",
            gpui::ImageFormat::Gif => "gif",
            gpui::ImageFormat::Webp => "webp",
            _ => {
                self.under_box = Some("That kind of picture can't be attached. PNG, JPEG, GIF and WebP can.".into());
                cx.notify();
                return;
            }
        };
        let dir = self.engine.paths().dir.join("chats").join("pasted");
        let file = dir.join(format!("Pasted {}.{ext}", chrono::Local::now().format("%Y-%m-%d %H.%M.%S%.3f")));
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, &image.bytes)) {
            Ok(()) => self.attach(vec![file], cx),
            Err(e) => {
                self.under_box = Some(format!("Couldn't keep the pasted picture: {e}"));
                cx.notify();
            }
        }
    }

    /// Puts `text` in the field as if typed (a hidden `--view` hook uses it to check the page).
    pub(super) fn draft(&mut self, text: &str, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.set_text(text.to_string(), cx));
    }

    /// Where the pickers under the box are, about, in the window: for a
    /// hidden `--view` hook that opens one without a click.
    pub(super) fn picker_anchor(&self, effort: bool, window: &Window) -> Point<Pixels> {
        let size = window.viewport_size();
        let column = (self.width - PADDING * 2.).clamp(280., MAX_WIDTH);
        let left = f32::from(size.width) - self.width + (self.width - column) / 2.;
        // After the mode's picker; the effort picker after the model's, whose width goes by its words.
        let mode = if self.mode == Mode::Bypass { 178. } else { 158. };
        let label = match (self.brain(), self.model_label()) {
            (Some(kind), Some(model)) => format!("{} · {model}", kind.name),
            (Some(kind), None) => kind.name.to_string(),
            _ => String::new(),
        };
        let model = label.chars().count() as f32 * 6.9 + 36.;
        gpui::point(px(left + mode + if effort { model + 4. } else { 0. }), size.height - px(48.))
    }

    /// Nothing is being answered, and nothing waits for a yes or no.
    pub(super) fn is_idle(&self) -> bool {
        !self.busy && !self.rows.iter().any(|row| matches!(row, Row::Ask { answer: None, .. }))
    }

    /// Sends what's in the field, as Return does.
    pub(super) fn send_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    /// A chat is open, with something said in it.
    pub(super) fn has_chat(&self) -> bool {
        self.live.is_some() && !self.rows.is_empty()
    }

    /// What answers in the next new chat, by its id, as if picked under the
    /// box (a hidden `--view` hook: demo mode reads no settings).
    pub(super) fn choose_brain(&mut self, id: &str, cx: &mut Context<Self>) {
        if brain::available().iter().any(|kind| kind.id == id) {
            self.brain_choice = Some(id.to_string());
            cx.notify();
        }
    }

    /// Types `text` into the field a letter at a time, then sends it, as
    /// someone at the keyboard would, starting `after_ms` from now. Several
    /// messages, with " | " between them, are a conversation: each is typed
    /// once the answer to the one before is done. (A hidden `--view` hook,
    /// to check the page and to film it without touching the window.)
    pub(super) fn type_and_send(&mut self, text: String, after_ms: u64, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let executor = cx.background_executor().clone();
            let wait = |ms: u64| executor.timer(std::time::Duration::from_millis(ms));
            wait(after_ms).await;
            for (n, message) in text.split('|').map(str::trim).filter(|m| !m.is_empty()).enumerate() {
                if n > 0 {
                    // The turn before has to have begun, then ended, with nothing left waiting for an answer.
                    wait(1500).await;
                    loop {
                        match this.update(cx, |this, _| this.busy || this.rows.iter().any(|row| matches!(row, Row::Ask { answer: None, .. }))) {
                            Ok(true) => wait(150).await,
                            Ok(false) => break,
                            Err(_) => return,
                        }
                    }
                    wait(2200).await;
                }
                let letters: Vec<char> = message.chars().collect();
                for end in 1..=letters.len() {
                    let so_far: String = letters[..end].iter().collect();
                    if this.update(cx, |this, cx| this.draft(&so_far, cx)).is_err() {
                        return;
                    }
                    wait(if letters[end - 1] == ' ' { 70 } else { 38 }).await;
                }
                wait(450).await;
                if this.update_in(cx, |this, window, cx| this.submit(window, cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Answers each call that waits for the user, once it's been on the page
    /// for `shown_ms` (a hidden `--view` hook, as `type_and_send`).
    pub(super) fn answer_each(&mut self, answer: Answer, shown_ms: u64, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let executor = cx.background_executor().clone();
            let wait = |ms: u64| executor.timer(std::time::Duration::from_millis(ms));
            loop {
                wait(100).await;
                let waiting = this.update(cx, |this, _| {
                    this.rows.iter().find_map(|row| match row {
                        Row::Ask { request, answer: None } => Some(request.id),
                        _ => None,
                    })
                });
                match waiting {
                    Ok(Some(id)) => {
                        wait(shown_ms).await;
                        if this.update(cx, |this, cx| this.answer(id, answer.clone(), cx)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
        })
        .detach();
    }

    /// Says `text` to the assistant, in the open chat or a new one.
    pub(super) fn say(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.say_with(text, Vec::new(), window, cx);
    }

    /// Says `text`, with these files attached.
    fn say_with(&mut self, text: String, files: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.live.is_none() && !self.open(None, window, cx) {
            return;
        }
        if let Some(live) = &self.live {
            live.send_with(text, files);
            self.stick = true;
        }
    }

    fn stop(&mut self) {
        if let Some(live) = &self.live {
            live.stop();
        }
    }

    /// What answers: the open chat's brain, else the one a new chat would
    /// get (the one picked here, else the one set in Settings, else the first there is).
    fn brain(&self) -> Option<&'static brain::Kind> {
        match &self.meta {
            Some(meta) => brain::kind(&meta.brain),
            None => {
                let set = self.engine.settings().chat.brain;
                let usable = brain::available();
                let wanted = self.brain_choice.as_deref().or(Some(set.as_str()).filter(|b| !b.is_empty()));
                wanted.and_then(|id| usable.iter().find(|k| k.id == id).copied()).or_else(|| usable.into_iter().next())
            }
        }
    }

    /// Takes in what may have changed while the page wasn't shown: how new
    /// chats start (Settings), and the models each program lists.
    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load_models();
        if self.live.is_none() {
            self.mode = Mode::from_setting(&self.engine.settings().chat.mode);
            // A brain picked here that's gone (its program was removed) is no longer the pick.
            if let Some(pick) = &self.brain_choice
                && !brain::available().iter().any(|k| k.id == pick)
            {
                self.brain_choice = None;
            }
        }
        cx.notify();
    }

    /// Reads each program's list of models.
    fn load_models(&mut self) {
        self.catalog = brain::KINDS.iter().map(|k| (k.id, catalog::of(k.id))).collect();
    }

    fn models_of(&self, brain: &str) -> &[Model] {
        self.catalog.iter().find(|(id, _)| *id == brain).map(|(_, models)| models.as_slice()).unwrap_or(&[])
    }

    /// The model and effort picked: the open chat's own, else what a new
    /// chat with this brain would start with. None: the brain's own choice.
    fn picked(&self) -> (Option<String>, Option<String>) {
        match &self.meta {
            Some(meta) => (meta.model.clone(), meta.effort.clone()),
            None => self.engine.settings().chat.pick(self.brain().map(|k| k.id).unwrap_or_default()),
        }
    }

    /// The model that answers, as far as it's known: the one picked, else
    /// the one the brain said it's using, else the first it lists.
    fn current_model(&self) -> Option<&Model> {
        let models = self.models_of(self.brain()?.id);
        let id = self.picked().0.or_else(|| self.model.clone());
        id.and_then(|id| catalog::find(models, &id)).or(models.first())
    }

    /// What the model picker's button says: the model by its name.
    fn model_label(&self) -> Option<String> {
        let id = self.picked().0.or_else(|| self.model.clone())?;
        let models = self.brain().map(|k| self.models_of(k.id)).unwrap_or(&[]);
        Some(catalog::find(models, &id).map(|m| m.name.clone()).unwrap_or(id))
    }

    /// What the effort picker's button says; None when the model has no such choice.
    fn effort_label(&self) -> Option<String> {
        let model = self.current_model()?;
        let named = |id: &str| model.efforts.iter().find(|e| e.id == id).map(|e| e.name.clone());
        if model.efforts.is_empty() {
            return None;
        }
        Some(self.picked().1.as_deref().and_then(named).or_else(|| model.effort.as_deref().and_then(named)).unwrap_or_else(|| "Effort".into()))
    }

    /// The model picker's lines: each brain there is, with its models.
    pub(super) fn model_choices(&self) -> Vec<BrainChoice> {
        let now = self.brain().map(|k| k.id);
        let settings = self.engine.settings().chat;
        brain::available()
            .into_iter()
            .map(|kind| {
                let current = Some(kind.id) == now;
                let model = if current { self.picked().0 } else { settings.pick(kind.id).0 };
                let mut models = vec![Choice { id: None, name: "Default".into(), picked: model.is_none() }];
                models.extend(self.models_of(kind.id).iter().map(|m| Choice { id: Some(m.id.clone()), name: m.name.clone(), picked: model.as_deref() == Some(m.id.as_str()) }));
                BrainChoice { id: kind.id, name: kind.name, current, model: model.map(|id| catalog::find(self.models_of(kind.id), &id).map(|m| m.name.clone()).unwrap_or(id)), models }
            })
            .collect()
    }

    /// The effort picker's lines, for the model that answers.
    pub(super) fn effort_choices(&self) -> Vec<Choice> {
        let Some(model) = self.current_model() else { return Vec::new() };
        let picked = self.picked().1.filter(|e| model.efforts.iter().any(|x| x.id == *e));
        let own = match model.effort.as_deref().and_then(|id| model.efforts.iter().find(|e| e.id == id)) {
            Some(effort) => format!("Default ({})", effort.name),
            None => "Default".to_string(),
        };
        let mut choices = vec![Choice { id: None, name: own, picked: picked.is_none() }];
        choices.extend(model.efforts.iter().map(|e| Choice { id: Some(e.id.clone()), name: e.name.clone(), picked: picked.as_deref() == Some(e.id.as_str()) }));
        choices
    }

    /// Picks what answers: a brain, and one of its models (None: its own
    /// choice). In the open chat when it's the same brain, from its next
    /// message on; another brain answers in a new chat, since it knows
    /// nothing of this one. The pick is what that brain's next chats start with.
    pub(super) fn pick(&mut self, brain_id: &str, model: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if brain::kind(brain_id).is_none() {
            return;
        }
        // An effort the new model doesn't take isn't carried over to it.
        let efforts: Vec<String> = match &model {
            Some(id) => catalog::find(self.models_of(brain_id), id).map(|m| m.efforts.iter().map(|e| e.id.clone()).collect()).unwrap_or_default(),
            None => Vec::new(),
        };
        let mut settings = self.engine.settings();
        let kept = settings.chat.picked.entry(brain_id.to_string()).or_default();
        kept.model = model.clone().unwrap_or_default();
        if model.is_some() && !efforts.contains(&kept.effort) {
            kept.effort.clear();
        }
        let effort = Some(kept.effort.clone()).filter(|e| !e.is_empty());
        let _ = self.engine.update_settings(settings);

        let same = self.meta.as_ref().is_some_and(|meta| meta.brain == brain_id);
        match (&self.live, same) {
            (Some(live), true) => {
                live.choose(model.clone(), effort.clone());
                if let Some(meta) = &mut self.meta {
                    meta.model = model;
                    meta.effort = effort;
                }
                // The brain says which model answers once it does; until then, the pick is what's shown.
                self.model = None;
            }
            _ => {
                if self.live.is_some() {
                    self.close();
                    window.focus(&self.input.focus_handle(cx));
                }
                self.brain_choice = Some(brain_id.to_string());
            }
        }
        cx.notify();
    }

    /// Picks how hard the model that answers is asked to think (None: its own choice).
    pub(super) fn pick_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        let Some(brain_id) = self.brain().map(|k| k.id) else { return };
        let model = self.picked().0;
        let mut settings = self.engine.settings();
        let kept = settings.chat.picked.entry(brain_id.to_string()).or_default();
        kept.effort = effort.clone().unwrap_or_default();
        // Kept with the model it was picked for.
        if kept.model.is_empty() {
            kept.model = model.clone().unwrap_or_default();
        }
        let _ = self.engine.update_settings(settings);
        if let Some(live) = &self.live {
            live.choose(model.clone(), effort.clone());
            if let Some(meta) = &mut self.meta {
                meta.model = model;
                meta.effort = effort;
            }
        }
        cx.notify();
    }

    /// Tools run in this chat without asking first.
    pub(super) fn bypasses(&self) -> bool {
        self.mode == Mode::Bypass
    }

    /// Whether this chat asks before a tool changes something, from now on.
    pub(super) fn set_bypass(&mut self, bypass: bool, cx: &mut Context<Self>) {
        self.mode = if bypass { Mode::Bypass } else { Mode::Ask };
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

    fn heard(&mut self, event: Event) {
        match event {
            Event::Started(text, files) => {
                self.rows.push(Row::User(text));
                if !files.is_empty() {
                    self.rows.push(Row::Files(files));
                }
                self.busy = true;
                self.stick = true;
            }
            Event::Update(Update::Model(name)) => self.model = Some(name),
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
        Update::Text(whole) => {
            // Its pieces were shown as they came: the whole replaces them. A
            // question may have come in between (Grok says the whole only once
            // the call it asked about has begun): then it's the words above
            // the question that this is the whole of, not something new.
            let mut asked = false;
            let shown = rows.iter_mut().rev().find_map(|row| match row {
                Row::Ask { .. } => {
                    asked = true;
                    None
                }
                Row::Said { text, writing } => Some(if *writing || (asked && !text.trim().is_empty() && whole.trim_start().starts_with(text.trim())) { Ok((text, writing)) } else { Err(()) }),
                _ => Some(Err(())),
            });
            match shown {
                Some(Ok((text, writing))) => {
                    *text = whole;
                    *writing = false;
                }
                _ if whole.trim().is_empty() => {}
                _ => rows.push(Row::Said { text: whole, writing: false }),
            }
        }
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
            let cost = cost.map(term::priced).unwrap_or_default();
            rows.push(Row::Note(format!("{} tokens in, {} out{cost}", term::count(input), term::count(output))));
        }
        Update::Limit(text) => rows.push(Row::Note(text)),
        // Shown beside the brain's name, not in the conversation.
        Update::Model(_) => {}
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
            Item::User { text, files, .. } => {
                rows.push(Row::User(text));
                if !files.is_empty() {
                    rows.push(Row::Files(files));
                }
            }
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
        let chats: Vec<Meta> = crate::chat::store(&self.engine).list().into_iter().filter(|m| !m.title.is_empty()).collect();
        let more = chats.len().saturating_sub(RECENT);
        let recent: Vec<Meta> = chats.into_iter().take(if self.all_chats { MOST_LISTED } else { RECENT }).collect();
        if !recent.is_empty() {
            let now = crate::chat::store::now();
            let mut list = card(theme).w(px(width)).overflow_hidden();
            for (n, meta) in recent.into_iter().enumerate() {
                let id = meta.id.clone();
                let (by_button, by_right_click) = (meta.id.clone(), meta.id.clone());
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
                        // Its menu (rename, delete): by its button, or a right click on the row.
                        .child(Button::icon_only(row_id("chat-recent-more", n), "ellipsis").tooltip("Rename or delete").on_click(cx.listener(move |_, e: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            cx.emit(ChatEvent::ChatMenu { id: by_button.clone(), position: e.position(), above: false });
                        })))
                        .on_mouse_down(MouseButton::Right, cx.listener(move |_, e: &MouseDownEvent, _, cx| cx.emit(ChatEvent::ChatMenu { id: by_right_click.clone(), position: e.position, above: false })))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open(Some(id.clone()), window, cx);
                        })),
                );
            }
            // The latest few, until asked for the rest.
            if more > 0 {
                let all = self.all_chats;
                list = list.child(
                    div()
                        .id("chat-recent-all")
                        .flex()
                        .items_center()
                        .h(px(32.))
                        .px(px(12.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .border_t_1()
                        .border_color(theme.border_variant)
                        .hover(|st| st.bg(theme.ghost_hover))
                        .child(if all { "Show the latest only".to_string() } else { format!("Show all {}", more + RECENT) })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.all_chats = !this.all_chats;
                            cx.notify();
                        })),
                );
            }
            body = body.child(div().flex().flex_col().gap(px(8.)).child(kit::label(theme, "Go on with")).child(list));
        }
        body.into_any_element()
    }

    /// No brain on this computer: what's needed, and where to get it.
    fn needs(&self, theme: &Theme, why: &str, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let mut buttons = div().flex().flex_wrap().items_center().gap(px(8.));
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
        let kind = self.brain();
        let (accent, hover) = (theme.accent, theme.ghost_hover);
        let action = if self.busy && !has_text {
            Button::icon_only("chat-stop", "square").style(ButtonStyle::Outlined).tooltip("Stop").on_click(cx.listener(|this, _, _, _| this.stop()))
        } else {
            Button::icon_only("chat-send", "arrow-up").style(ButtonStyle::Primary).disabled(!has_text).tooltip("Send (Return). Shift+Return starts a new line.").on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };
        let offers = self.offers(cx);
        div()
            .w(px(width))
            .flex()
            .flex_col()
            .gap(px(6.))
            // What a "/" or an "@" being typed offers, right above where it's typed.
            .children(offers.map(|offers| self.offer_list(theme, width, &offers, cx)))
            .child(
                div()
                    .id("chat-box")
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .py(px(8.))
                    .pl(px(14.))
                    .pr(px(8.))
                    .rounded(kit::corner(10.))
                    .border_1()
                    .border_color(if bypass { theme.warning.opacity(0.7) } else if focused { theme.border_focused } else { theme.border })
                    .bg(theme.surface)
                    // Files dropped on it go with the next message.
                    .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| style.border_color(accent))
                    .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| this.attach(paths.paths().to_vec(), cx)))
                    .when(!self.attached.is_empty(), |d| {
                        d.child(div().flex().flex_wrap().gap(px(6.)).children(self.attached.iter().enumerate().map(|(n, path)| {
                            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                            let remove = div()
                                .id(("chat-unattach", n))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(18.))
                                .rounded(kit::corner(4.))
                                .cursor_pointer()
                                .hover(move |d| d.bg(hover))
                                .child(icon("x", 11., theme.text_placeholder))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if n < this.attached.len() {
                                        this.attached.remove(n);
                                    }
                                    this.under_box = None;
                                    cx.notify();
                                }))
                                .into_any_element();
                            chip(theme, &name, Some(remove))
                        })))
                    })
                    .child(
                        div()
                            .flex()
                            // The buttons stay at the bottom as the field grows.
                            .items_end()
                            .gap(px(6.))
                            .child(div().flex_1().min_w(px(0.)).child(self.input.clone()))
                            .child(Button::icon_only("chat-attach", "paperclip").style(ButtonStyle::Ghost).tooltip("Attach files: text, pictures or PDFs. Dropping them here, or pasting a picture, does the same.").on_click(cx.listener(|this, _, window, cx| this.pick_files(window, cx))))
                            .child(action),
                    ),
            )
            // Why a file couldn't be attached.
            .children(self.under_box.clone().map(|why| div().px(px(4.)).text_size(px(12.)).text_color(theme.warning).child(why)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        // The mode: whether it asks before a tool changes something.
                        div()
                            .id("chat-mode")
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(5.))
                            .h(px(26.))
                            .px(px(9.))
                            .rounded(kit::corner(6.))
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if bypass { theme.warning } else { theme.text_muted })
                            .cursor_pointer()
                            .hover(move |d| d.bg(hover))
                            .child(icon(if bypass { "triangle-alert" } else { "lock" }, 13., if bypass { theme.warning } else { theme.text_placeholder }))
                            .child(if bypass { "Bypass permissions" } else { "Ask permissions" })
                            .child(icon("chevron-down", 12., theme.text_placeholder))
                            .tooltip(move |window, cx| kit::Tooltip::view(if bypass { "Every tool runs without asking, the ones that delete things too." } else { "It asks before a tool changes anything." }.into(), window, cx))
                            .on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(ChatEvent::ModeMenu { position: e.position() }))),
                    )
                    // What answers, and how hard it thinks: two pickers.
                    .children(kind.map(|kind| {
                        let label = match self.model_label() {
                            Some(model) => format!("{} · {model}", kind.name),
                            None => kind.name.to_string(),
                        };
                        picker(theme, "chat-model", label, "What answers in this chat: which program, and which of its models.", cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(ChatEvent::Menu { effort: false, position: e.position() })))
                    }))
                    .children(self.effort_label().map(|effort| picker(theme, "chat-effort", effort, "How hard the model is asked to think. More takes longer.", cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(ChatEvent::Menu { effort: true, position: e.position() })))))
                    .child(div().flex_1())
                    .when(self.live.is_some(), |d| d.child(Button::new("chat-new", "New chat").icon("plus").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx)))))
                    // The open chat's own menu, once there's something in it to name or delete.
                    .when_some(self.meta.as_ref().filter(|_| self.has_chat()).map(|meta| meta.id.clone()), |d, id| {
                        d.child(Button::icon_only("chat-more", "ellipsis").tooltip("Rename or delete this chat").on_click(cx.listener(move |_, e: &ClickEvent, _, cx| cx.emit(ChatEvent::ChatMenu { id: id.clone(), position: e.position(), above: true }))))
                    }),
            )
            .into_any_element()
    }
}

/// An attached file: a paperclip and its name, and a way to take it off
/// again while it hasn't been sent.
fn chip(theme: &Theme, name: &str, remove: Option<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.))
        .h(px(24.))
        .pl(px(8.))
        .pr(px(if remove.is_some() { 3. } else { 9. }))
        .rounded(kit::corner(6.))
        .bg(theme.element)
        .border_1()
        .border_color(theme.border_variant)
        .text_size(px(12.))
        .text_color(theme.text_muted)
        .child(icon("paperclip", 12., theme.text_placeholder))
        .child(div().max_w(px(220.)).truncate().child(name.to_string()))
        .children(remove)
        .into_any_element()
}

/// A button that opens a list to pick from: what's picked now, and a chevron.
fn picker(theme: &Theme, id: &'static str, label: String, tip: &'static str, on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static) -> AnyElement {
    let hover = theme.ghost_hover;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.))
        .h(px(26.))
        .px(px(9.))
        .rounded(kit::corner(6.))
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(move |d| d.bg(hover))
        .child(label)
        .child(icon("chevron-down", 12., theme.text_placeholder))
        .tooltip(move |window, cx| kit::Tooltip::view(tip.into(), window, cx))
        .on_click(on_click)
        .into_any_element()
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
                Row::Files(names) => div().w(px(width)).flex().flex_wrap().gap(px(6.)).children(names.iter().map(|name| chip(&theme, name, None))).into_any_element(),
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
                    .child(div().flex_1().min_w(px(0.)).child(markdown(&theme, why, width - 22., format!("chat-failed-{n}"), "")))
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
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .px(px(PADDING))
                    .pb(px(18.))
                    .child(self.composer(&theme, width, window, cx)),
            )
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
    fn words_said_before_a_question_arent_said_twice() {
        let ask = Request { id: 1, tool: "session_start".into(), args: json!({"machine_id": "beacon", "agent": "codex"}), risk: Risk::Change, rule: Some("session_start".into()) };
        // As Grok does it: the words as they come, the question, and only then the words whole and the call.
        let mut rows = vec![Row::User("start codex on beacon".into())];
        apply(&mut rows, Update::Delta("Beacon is online. ".into()));
        apply(&mut rows, Update::Delta("Starting Codex there.".into()));
        apply(&mut rows, Update::Ask(ask.clone()));
        apply(&mut rows, Update::Text("Beacon is online. Starting Codex there.".into()));
        apply(&mut rows, Update::Tool { id: "t1".into(), name: "session_start".into(), args: ask.args.clone() });
        let said: Vec<&str> = rows.iter().filter_map(|r| if let Row::Said { text, .. } = r { Some(text.as_str()) } else { None }).collect();
        assert_eq!(said, ["Beacon is online. Starting Codex there."], "{rows:?}");
        // Something new said after a question is its own line, as are the same words said again with no question between.
        apply(&mut rows, Update::Text("Codex is running.".into()));
        apply(&mut rows, Update::Text("Codex is running.".into()));
        assert_eq!(rows.iter().filter(|r| matches!(r, Row::Said { .. })).count(), 3);
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
            Item::User { at: 1, text: "restart it".into(), files: vec![] },
            Item::Tool { at: 2, id: "t1".into(), name: "runner_action".into(), args: restart.clone() },
            Item::Ask { at: 2, id: 1, tool: "runner_action".into(), args: restart.clone(), risk: Risk::Change },
            Item::Answer { at: 3, id: 1, answer: "no".into(), note: "mid-job".into() },
            Item::Result { at: 3, id: "t1".into(), text: "declined".into(), is_error: true },
            Item::Text { at: 4, text: "Left it alone.".into() },
            Item::Usage { at: 4, input: 1, output: 1, cached: 0, cost: None },
            Item::User { at: 5, text: "ok, do it".into(), files: vec![] },
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
