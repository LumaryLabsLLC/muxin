//! ⌘P: go to a machine, a session or a desktop. ⇧⌘P (or ">" first): run a
//! command. One search box for both, like an editor's; both shortcuts can be
//! changed in Settings › Keyboard.

use gpui::{AnyElement, App, Context, Entity, Focusable, FontWeight, KeyBinding, ScrollHandle, SharedString, Subscription, Window, actions, div, prelude::*, px};

use super::format;
use super::input::{InputEvent, TextInput};
use super::kit::{self, icon};
use super::root::{Root, Route};
use super::settings::SettingsTab;
use super::theme::{self, Mode, Theme, mono_font};

actions!(palette, [PaletteUp, PaletteDown]);

const CONTEXT: &str = "Palette";
const WIDTH: f32 = 640.;
/// A row's inset from the panel, and its own padding: the search icon and the
/// hints line up with the rows' icons.
const ROW_INSET: f32 = 10.;
const ROW_PAD: f32 = 14.;
const EDGE: f32 = ROW_INSET + ROW_PAD;
/// How far a machine's sessions and desktop sit in under it.
const INDENT: f32 = 28.;
/// What's left for an item's words: the width less insets, glyph, gaps and the right-hand label.
const TEXT_WIDTH: f32 = WIDTH - 2. * EDGE - 22. - 2. * 12. - 120.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", PaletteUp, Some(CONTEXT)),
        KeyBinding::new("down", PaletteDown, Some(CONTEXT)),
        KeyBinding::new("ctrl-p", PaletteUp, Some(CONTEXT)),
        KeyBinding::new("ctrl-n", PaletteDown, Some(CONTEXT)),
    ]);
}

pub struct Palette {
    input: Entity<TextInput>,
    /// A row picked with the arrows, and the text it was picked for. Until
    /// then, or once the text changes, the selection is the best match.
    picked: Option<(String, usize)>,
    scroll: ScrollHandle,
    /// Picking a theme: the preferences to go back to, and the theme that was showing.
    themes: Option<(theme::Appearance, String)>,
    /// The text the list was last made for (changes that leave it alone don't reset the selection).
    query: String,
    _changes: Subscription,
}

#[derive(Clone, Copy, PartialEq)]
enum Command {
    AddMachine,
    NewCloudMachine,
    NewVm,
    NewSchedule,
    NewSession,
    NewWorkspace,
    CopyFiles,
    CheckMachines,
    SyncGitHub,
    ToggleSidebar,
    Back,
    Forward,
    Settings(SettingsTab),
    Theme(Mode),
    ChooseTheme,
    /// The machine or session on screen.
    Rename,
    JoinShare,
    /// The session on screen.
    Share,
}

#[derive(Clone)]
enum Run {
    Go(Route),
    /// A machine's desktop on one of its monitors (None: the main one).
    Screen(String, Option<String>),
    Do(Command),
    /// Connect to a machine that's disconnected.
    Connect(String),
    /// A theme, by id.
    Theme(&'static str),
}

enum Glyph {
    Icon(&'static str),
    /// An agent CLI's id ("claude", "shell", ...).
    Agent(String),
    Machine(&'static str, (u32, u32)),
    /// A theme's background and accent.
    Swatch(u32, u32),
}

struct Item {
    title: String,
    detail: String,
    /// The right-hand side: "Machine", "Session", "Desktop", or a command's keys.
    side: String,
    side_is_keys: bool,
    glyph: Glyph,
    run: Run,
    /// Under its machine (a session or desktop), like the sidebar's tree.
    indent: bool,
    /// The machine it's under, or is (places only).
    group: Option<String>,
    /// More words a search can match that the row doesn't show (its machine's name).
    also: String,
    /// How well it matched the search; None when it's only there as its group's heading.
    score: Option<i32>,
}

impl Item {
    fn new(title: impl Into<String>, detail: impl Into<String>, side: impl Into<String>, glyph: Glyph, run: Run) -> Item {
        Item { title: title.into(), detail: detail.into(), side: side.into(), side_is_keys: false, glyph, run, indent: false, group: None, also: String::new(), score: None }
    }
}

/// How well `query` matches `text`: every word of it, in order within the
/// word, case-insensitive; None when it doesn't. Higher is better: whole
/// substrings beat scattered letters, and starts of words count extra.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let text = text.to_lowercase();
    let mut total = 0;
    for word in query.to_lowercase().split_whitespace() {
        total += word_score(word, &text)?;
    }
    Some(total)
}

fn word_score(word: &str, text: &str) -> Option<i32> {
    if let Some(pos) = text.find(word) {
        let at_word = pos == 0 || !text[..pos].chars().next_back().is_some_and(char::is_alphanumeric);
        return Some(1000 - pos.min(200) as i32 + if pos == 0 { 300 } else if at_word { 150 } else { 0 });
    }
    // Letters in order, anywhere.
    let chars: Vec<char> = text.chars().collect();
    let (mut i, mut s, mut last) = (0, 0, None::<usize>);
    for c in word.chars() {
        let found = (i..chars.len()).find(|&j| chars[j] == c)?;
        s += 10;
        if last == Some(found.wrapping_sub(1)) {
            s += 15;
        }
        if found == 0 || !chars[found - 1].is_alphanumeric() {
            s += 20;
        }
        last = Some(found);
        i = found + 1;
    }
    Some(s)
}

impl Root {
    /// Opens the palette: commands (`commands`) or places. The same shortcut
    /// again closes it.
    pub(super) fn open_palette(&mut self, commands: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        if let Some(p) = &self.palette {
            let showing_commands = p.input.read(cx).text().starts_with('>');
            if showing_commands == commands {
                self.close_palette(window, cx);
                return;
            }
        }
        let input = cx.new(|cx| TextInput::new(cx, "Go to a machine, session or desktop   (> for commands)").bare().with_text(if commands { ">" } else { "" }));
        let changes = cx.subscribe_in(&input, window, |this, _, e: &InputEvent, window, cx| match e {
            InputEvent::Changed => {
                let Some(p) = &this.palette else { return };
                let text = p.input.read(cx).text().to_string();
                if text == p.query {
                    return;
                }
                let best = this.best_palette_item(cx);
                let Some(p) = &mut this.palette else { return };
                p.query = text;
                p.scroll.scroll_to_item(best);
                this.preview_theme(window, cx);
                cx.notify()
            }
            InputEvent::Submit => this.run_palette_item(None, window, cx),
            InputEvent::Cancel => this.close_palette(window, cx),
        });
        window.focus(&input.focus_handle(cx));
        self.context_menu = None;
        let query = input.read(cx).text().to_string();
        self.palette = Some(Palette { input, picked: None, scroll: ScrollHandle::new(), themes: None, query, _changes: changes });
        cx.notify();
    }

    /// The palette with `text` typed (test views).
    pub(super) fn open_palette_with(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette(text.starts_with('>'), window, cx);
        if let Some(p) = &self.palette {
            p.input.update(cx, |i, cx| i.set_text(text.to_string(), cx));
        }
    }

    /// Closes it; a theme being previewed goes back to the one before.
    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.palette.take() {
            if let Some((before, _)) = p.themes {
                theme::preview_appearance(window, cx, before);
            }
            window.focus(&self.focus);
            cx.notify();
        }
    }

    /// The selected row: the one picked for this text, else the best match.
    fn palette_selection(&self, cx: &App) -> usize {
        let Some(p) = &self.palette else { return 0 };
        match &p.picked {
            Some((text, ix)) if text.as_str() == p.input.read(cx).text() => *ix,
            _ => self.best_palette_item(cx),
        }
    }

    fn move_palette(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.palette_items(cx).len();
        let from = self.palette_selection(cx);
        if let Some(p) = &mut self.palette {
            if count > 0 {
                let to = (from as isize + by).rem_euclid(count as isize) as usize;
                p.picked = Some((p.input.read(cx).text().to_string(), to));
                p.scroll.scroll_to_item(to);
            }
        }
        self.preview_theme(window, cx);
        cx.notify();
    }

    /// Lists the themes, starting at the one showing; moving through them shows each.
    pub(super) fn choose_theme(&mut self, cx: &mut Context<Self>) {
        let showing = theme::ActiveTheme::theme(&**cx).id.to_string();
        let before = cx.global::<theme::Appearance>().clone();
        let Some(p) = &mut self.palette else { return };
        p.themes = Some((before, showing.clone()));
        p.query = String::new();
        p.input.update(cx, |i, cx| {
            i.set_placeholder("Pick a theme   (↑↓ to try one)", cx);
            i.set_text("", cx);
        });
        let at = super::themes::PALETTES.iter().position(|t| t.id == showing).unwrap_or(0);
        if let Some(p) = &mut self.palette {
            p.picked = Some((String::new(), at));
            p.scroll.scroll_to_item(at);
        }
        cx.notify();
    }

    /// Shows the selected theme, without keeping it (Enter keeps it).
    fn preview_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = &self.palette else { return };
        let Some((before, _)) = p.themes.clone() else { return };
        if let Some(Run::Theme(id)) = self.palette_items(cx).into_iter().nth(self.palette_selection(cx)).map(|i| i.run) {
            let mut a = before;
            theme::choose_theme(&mut a, id, window.appearance());
            theme::preview_appearance(window, cx, a);
        }
    }

    /// Runs the item at `index`, or the selected one.
    fn run_palette_item(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_none() {
            return;
        }
        let ix = index.unwrap_or_else(|| self.palette_selection(cx));
        let Some(run) = self.palette_items(cx).into_iter().nth(ix).map(|i| i.run) else { return };
        match run {
            Run::Do(Command::ChooseTheme) => return self.choose_theme(cx),
            Run::Theme(id) => {
                // Keep it: nothing to go back to.
                if let Some(p) = &mut self.palette {
                    p.themes = None;
                }
                self.close_palette(window, cx);
                theme::update_appearance(window, cx, |a, system| theme::choose_theme(a, id, system));
                return;
            }
            _ => {}
        }
        self.close_palette(window, cx);
        match run {
            Run::Go(route) => self.navigate(route, window, cx),
            Run::Screen(machine, display) => self.show_display(machine, display, window, cx),
            Run::Connect(machine) => self.set_machine_connected(&machine, true, cx),
            Run::Do(c) => match c {
                Command::AddMachine => self.show_add_machine(window, cx),
                Command::NewCloudMachine => self.show_cloud_dialog(window, cx),
                Command::NewVm => self.show_vm_dialog(None, window, cx),
                Command::NewSchedule => self.show_schedule_dialog(None, window, cx),
                Command::NewSession => self.show_new_session(None, window, cx),
                Command::JoinShare => {
                    self.show_join(None, window, cx);
                }
                Command::Share => match self.route.clone() {
                    Route::Session(id) => self.show_share(&id, window, cx),
                    Route::Machine(id) => self.show_share(&format!("machine:{id}"), window, cx),
                    _ => {}
                },
                Command::NewWorkspace => self.show_new_workspace(None, window, cx),
                Command::CopyFiles => self.show_copy_files(None, None, window, cx),
                Command::CheckMachines => self.engine.trigger_check(None),
                Command::SyncGitHub => self.engine.trigger_sync(),
                Command::ToggleSidebar => self.toggle_sidebar(cx),
                Command::Back => window.dispatch_action(Box::new(super::root::GoBack), cx),
                Command::Forward => window.dispatch_action(Box::new(super::root::GoForward), cx),
                Command::Settings(tab) => self.open_settings_tab(tab, window, cx),
                Command::Theme(mode) => theme::update_appearance(window, cx, |a, _| a.mode = mode),
                Command::ChooseTheme => {}
                Command::Rename => {
                    if let Some((target, _)) = self.rename_target() {
                        self.show_rename(target, window, cx)
                    }
                }
            },
            Run::Theme(_) => {}
        }
    }

    /// What the palette lists for its query, best first.
    fn palette_items(&self, cx: &App) -> Vec<Item> {
        let Some(p) = &self.palette else { return Vec::new() };
        let text = p.input.read(cx).text().to_string();
        let (items, query) = match text.strip_prefix('>') {
            _ if p.themes.is_some() => (self.theme_items(), text.trim().to_string()),
            Some(q) => (self.command_items(cx), q.trim().to_string()),
            None => (self.place_items(cx), text.trim().to_string()),
        };
        if query.is_empty() {
            return items;
        }
        // The title counts most; the rest (machine, folder, kind) can still match.
        let matches = |i: &Item| {
            score(&query, &i.title)
                .map(|s| s + 500)
                .or_else(|| score(&query, &format!("{} {} {} {}", i.title, i.detail, i.also, if i.side_is_keys { "" } else { &i.side })))
        };
        let scores: Vec<Option<i32>> = items.iter().map(matches).collect();
        if items.iter().any(|i| i.group.is_some()) {
            // Places keep their tree: a machine stays, as the heading, when
            // something under it matches.
            let groups: std::collections::HashSet<String> = items.iter().zip(&scores).filter(|(_, s)| s.is_some()).filter_map(|(i, _)| i.group.clone()).collect();
            return items
                .into_iter()
                .zip(scores)
                .filter_map(|(mut i, s)| {
                    let heading = !i.indent && i.group.as_ref().is_some_and(|g| groups.contains(g));
                    (s.is_some() || heading).then(|| {
                        i.score = s;
                        i
                    })
                })
                .collect();
        }
        let mut scored: Vec<Item> = items
            .into_iter()
            .zip(scores)
            .filter_map(|(mut i, s)| {
                i.score = Some(s?);
                Some(i)
            })
            .collect();
        scored.sort_by(|a, b| b.score.cmp(&a.score));
        scored
    }

    /// The row to start on: the best match (the first, when nothing's typed).
    fn best_palette_item(&self, cx: &App) -> usize {
        let items = self.palette_items(cx);
        items.iter().enumerate().filter_map(|(ix, i)| Some((i.score?, std::cmp::Reverse(ix)))).max().map(|(_, std::cmp::Reverse(ix))| ix).unwrap_or(0)
    }

    /// Each machine (in the sidebar's order), with its sessions and its
    /// desktop under it.
    fn place_items(&self, cx: &App) -> Vec<Item> {
        let s = &self.snapshot;
        let favorites = cx.global::<theme::Appearance>().favorite_machines.clone();
        let machines = Root::machine_order(s, &favorites);
        let session = |x: &crate::fleet::sessions::SessionInfo, machine: &str| {
            let folder = x.cwd.rsplit('/').find(|p| !p.is_empty()).unwrap_or(&x.cwd).to_string();
            let who = if x.agent_name.is_empty() { "Session".to_string() } else { x.agent_name.clone() };
            let mut item = Item::new(
                x.title.clone(),
                [who, folder].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · "),
                if x.status == "exited" { "Session · ended" } else { "Session" },
                Glyph::Agent(x.agent.clone()),
                Run::Go(Route::Session(x.id.clone())),
            );
            item.also = machine.to_string();
            item
        };
        let mut out = Vec::new();
        for m in &machines {
            let mut head = Item::new(m.name.clone(), format::machine_subtitle(m).0, "Machine", Glyph::Machine(format::machine_icon(m), format::machine_tile(m)), Run::Go(Route::Machine(m.id.clone())));
            head.group = Some(m.id.clone());
            out.push(head);
            if m.is_disconnected() {
                let mut item = Item::new("Connect", "Muxin connects to it again", "Machine", Glyph::Icon("plug"), Run::Connect(m.id.clone()));
                item.indent = true;
                item.group = Some(m.id.clone());
                item.also = m.name.clone();
                out.push(item);
            }
            for x in s.sessions.iter().filter(|x| x.machine_id == m.id) {
                let mut item = session(x, &m.name);
                item.indent = true;
                item.group = Some(m.id.clone());
                out.push(item);
            }
            if format::has_desktop(m) && !m.local && m.status == "online" {
                let mut item = Item::new("Desktop", "See and use its screen", "Desktop", Glyph::Icon("monitor"), Run::Screen(m.id.clone(), None));
                item.indent = true;
                item.group = Some(m.id.clone());
                item.also = m.name.clone();
                out.push(item);
                let mut logs = Item::new("Logs", "Its system log, files, containers, services", "Logs", Glyph::Icon("scroll-text"), Run::Go(Route::Logs(m.id.clone())));
                logs.indent = true;
                logs.group = Some(m.id.clone());
                logs.also = m.name.clone();
                out.push(logs);
                let mut services = Item::new("Services", "Its services and containers: start, stop, restart", "Services", Glyph::Icon("layers"), Run::Go(Route::Services(m.id.clone())));
                services.indent = true;
                services.group = Some(m.id.clone());
                services.also = m.name.clone();
                out.push(services);
                // Its other monitors, virtual ones included.
                for d in m.displays.iter().filter(|d| d.id != "all" && !d.main) {
                    let kind = if d.is_virtual { "Virtual monitor" } else { "Monitor" };
                    let detail = format!("{kind} · {}×{}", d.width as u32, d.height as u32);
                    let mut item = Item::new(d.name.clone(), detail, "Desktop", Glyph::Icon("monitor"), Run::Screen(m.id.clone(), Some(d.id.clone())));
                    item.indent = true;
                    item.group = Some(m.id.clone());
                    item.also = m.name.clone();
                    out.push(item);
                }
            }
        }
        // Sessions on machines that aren't in the fleet anymore.
        for x in s.sessions.iter().filter(|x| !s.machines.iter().any(|m| m.id == x.machine_id)) {
            out.push(session(x, &x.machine_id));
        }
        out
    }

    fn theme_items(&self) -> Vec<Item> {
        let showing = self.palette.as_ref().and_then(|p| p.themes.as_ref()).map(|(_, id)| id.as_str()).unwrap_or_default();
        super::themes::PALETTES
            .iter()
            .map(|t| {
                let side = format!("{}{}", if t.dark { "Dark" } else { "Light" }, if t.id == showing { " · current" } else { "" });
                Item::new(t.name, "", side, Glyph::Swatch(t.canvas, t.accent), Run::Theme(t.id))
            })
            .collect()
    }

    /// The machine or session on screen, and its name.
    fn rename_target(&self) -> Option<(super::dialogs::RenameTarget, String)> {
        use super::dialogs::RenameTarget;
        let s = &self.snapshot;
        match &self.route {
            Route::Machine(id) | Route::Desktop(id) | Route::Logs(id) | Route::Services(id) => s.machines.iter().find(|m| &m.id == id).map(|m| (RenameTarget::Machine(m.id.clone()), m.name.clone())),
            Route::Session(id) => s.sessions.iter().find(|x| &x.id == id).map(|x| (RenameTarget::Session(x.id.clone()), x.title.clone())),
            _ => None,
        }
    }

    fn command_items(&self, cx: &App) -> Vec<Item> {
        use super::shortcuts::label_for;
        let keys = |id: &str| label_for(cx, id);
        let item = |title: &str, glyph: &'static str, shortcut: Option<&str>, run: Run| {
            let mut i = Item::new(title, "", shortcut.map(keys).unwrap_or_default(), Glyph::Icon(glyph), run);
            i.side_is_keys = true;
            i
        };
        // The machine or session on screen comes first.
        let rename = self.rename_target().map(|(_, name)| item(&format!("Rename {name}…"), "pencil", None, Run::Do(Command::Rename)));
        let share = match &self.route {
            Route::Session(id) if !self.engine.is_demo() => self.snapshot.sessions.iter().find(|x| &x.id == id && x.persistent).map(|x| item(&format!("Share {}…", x.title), "link", None, Run::Do(Command::Share))),
            Route::Machine(id) if !self.engine.is_demo() => self.snapshot.machines.iter().find(|m| &m.id == id && !m.is_down()).map(|m| item(&format!("Share {}…", m.name), "link", None, Run::Do(Command::Share))),
            _ => None,
        };
        let all = [
            item("Add machine…", "plus", Some("add-machine"), Run::Do(Command::AddMachine)),
            item("New cloud machine…", "cloud", None, Run::Do(Command::NewCloudMachine)),
            item("New VM…", "box", None, Run::Do(Command::NewVm)),
            item("New schedule…", "clock", None, Run::Do(Command::NewSchedule)),
            item("Go to Schedules", "clock", Some("schedules"), Run::Go(Route::Schedules)),
            item("New session…", "square-terminal", Some("new-session"), Run::Do(Command::NewSession)),
            item("New workspace…", "layers", None, Run::Do(Command::NewWorkspace)),
            item("Join a share…", "link", None, Run::Do(Command::JoinShare)),
            item("Copy files between machines…", "folder", None, Run::Do(Command::CopyFiles)),
            item("Go to Overview", "gauge", Some("overview"), Run::Go(Route::Overview)),
            item("Go to Jobs", "workflow", Some("jobs"), Run::Go(Route::Jobs)),
            item("Go to Workspaces", "layers", Some("workspaces"), Run::Go(Route::Workspaces)),
            item("Go to GitHub", "agent-github", Some("github"), Run::Go(Route::Github)),
            item("Go to Usage", "gauge", Some("usage"), Run::Go(Route::Usage)),
            item("Run a command on machines…", "zap", Some("run"), Run::Go(Route::Run)),
            item("Go to Activity", "activity", Some("activity"), Run::Go(Route::Activity)),
            item("Refresh all machines", "refresh-cw", Some("check"), Run::Do(Command::CheckMachines)),
            item("Sync GitHub", "refresh-cw", Some("sync"), Run::Do(Command::SyncGitHub)),
            item("Hide or show the sidebar", "panel-left", Some("sidebar"), Run::Do(Command::ToggleSidebar)),
            item("Back", "arrow-left", Some("back"), Run::Do(Command::Back)),
            item("Forward", "arrow-right", Some("forward"), Run::Do(Command::Forward)),
            item("Settings", "settings", Some("settings"), Run::Do(Command::Settings(SettingsTab::General))),
            item("Settings: Appearance", "palette", None, Run::Do(Command::Settings(SettingsTab::Appearance))),
            item("Settings: Terminal", "terminal", None, Run::Do(Command::Settings(SettingsTab::Terminal))),
            item("Settings: GitHub", "agent-github", None, Run::Do(Command::Settings(SettingsTab::GitHub))),
            item("Settings: Cloud", "cloud", None, Run::Do(Command::Settings(SettingsTab::Cloud))),
            item("Settings: Keyboard shortcuts", "keyboard", None, Run::Do(Command::Settings(SettingsTab::Keyboard))),
            item("Cloud usage and cost", "cloud", None, Run::Do(Command::Settings(SettingsTab::Cloud))),
            item("Install the muxin command", "terminal", None, Run::Do(Command::Settings(SettingsTab::General))),
            item("Theme: Choose…", "palette", None, Run::Do(Command::ChooseTheme)),
            item("Theme: Light mode", "palette", None, Run::Do(Command::Theme(Mode::Light))),
            item("Theme: Dark mode", "palette", None, Run::Do(Command::Theme(Mode::Dark))),
            item("Theme: Match the system", "palette", None, Run::Do(Command::Theme(Mode::System))),
            item("About Muxin", "info", None, Run::Go(Route::About)),
        ];
        share.into_iter().chain(rename).chain(all).collect()
    }

    pub(super) fn render_palette(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.palette.as_ref()?;
        let items = self.palette_items(cx);
        let picking = p.themes.is_some();
        let commands = !picking && p.input.read(cx).text().starts_with('>');
        let selected = self.palette_selection(cx).min(items.len().saturating_sub(1));
        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(i, item)| {
                let glyph: AnyElement = match item.glyph {
                    Glyph::Icon(name) => kit::color_tile(name, 22.).into_any_element(),
                    Glyph::Agent(agent) => {
                        let (name, (from, to)) = kit::agent_look(&agent);
                        kit::tile(name, from, to, 22.).into_any_element()
                    }
                    Glyph::Machine(name, (from, to)) => kit::tile(name, from, to, 22.).into_any_element(),
                    Glyph::Swatch(bg, accent) => div()
                        .size(px(22.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(gpui::rgb(bg))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(div().size(px(8.)).rounded_full().bg(gpui::rgb(accent)))
                        .into_any_element(),
                };
                let text_w = TEXT_WIDTH - if item.indent { INDENT } else { 0. };
                let side: AnyElement = if item.side_is_keys {
                    if item.side.is_empty() { div().into_any_element() } else { kit::kbd(theme, item.side).into_any_element() }
                } else {
                    div().text_size(px(11.5)).text_color(theme.text_placeholder).child(item.side).into_any_element()
                };
                div()
                    .id(("palette-item", i))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .h(px(if item.detail.is_empty() { 36. } else { 48. }))
                    .mx(px(ROW_INSET))
                    .px(px(ROW_PAD))
                    .when(item.indent, |d| d.pl(px(ROW_PAD + INDENT)))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .when(i == selected, |d| d.bg(theme.element_selected))
                    .when(i != selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                    .child(div().w(px(22.)).flex_none().flex().justify_center().child(glyph))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().w(px(text_w)).truncate().text_size(px(13.)).text_color(theme.text).child(item.title))
                            .when(!item.detail.is_empty(), |d| d.child(div().w(px(text_w)).truncate().text_size(px(11.5)).text_color(theme.text_muted).child(item.detail))),
                    )
                    .child(div().w(px(120.)).flex_none().flex().justify_end().child(side))
                    .on_click(cx.listener(move |this, _, window, cx| this.run_palette_item(Some(i), window, cx)))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        let hint = if picking {
            "↑↓ try   ↵ keep   esc put back"
        } else if commands {
            "↑↓ move   ↵ run   esc close"
        } else {
            "↑↓ move   ↵ open   >  commands   esc close"
        };
        let panel = div()
            .id("palette")
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &PaletteUp, window, cx| this.move_palette(-1, window, cx)))
            .on_action(cx.listener(|this, _: &PaletteDown, window, cx| this.move_palette(1, window, cx)))
            .occlude()
            .on_click(|_, _, cx| cx.stop_propagation())
            .w(px(WIDTH))
            .flex()
            .flex_col()
            .rounded(px(12.))
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .font_weight(FontWeight::NORMAL)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(EDGE + 4.))
                    .h(px(50.))
                    .border_b_1()
                    .border_color(theme.border_variant)
                    .child(icon(if picking { "palette" } else if commands { "zap" } else { "search" }, 15., theme.text_muted))
                    .child(div().flex_1().min_w(px(0.)).text_size(px(14.)).child(p.input.clone())),
            )
            .child(
                div()
                    .id("palette-items")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .track_scroll(&p.scroll)
                    .py(px(8.))
                    .when(empty, |d| d.child(div().px(px(EDGE)).py(px(10.)).text_size(px(12.5)).text_color(theme.text_muted).child(if picking { "No theme like that." } else if commands { "No command like that." } else { "Nothing like that. Type > for commands." })))
                    .children(rows),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(EDGE))
                    .h(px(32.))
                    .border_t_1()
                    .border_color(theme.border_variant)
                    .font_family(mono_font())
                    .text_size(px(11.))
                    .text_color(theme.text_placeholder)
                    .child(SharedString::from(hint)),
            );
        Some(
            div()
                .id("palette-layer")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                // At the top, as tall as what it lists.
                .items_start()
                .pt(px(72.))
                .occlude()
                .on_click(cx.listener(|this, _, window, cx| this.close_palette(window, cx)))
                .child(panel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn finds_words_and_letters_in_order() {
        assert!(score("bee", "Beelink").is_some());
        assert!(score("blk", "Beelink").is_some());
        assert!(score("xyz", "Beelink").is_none());
        // Every word must match.
        assert!(score("claude bee", "Claude Code · Beelink · api").is_some());
        assert!(score("claude mac", "Claude Code · Beelink · api").is_none());
        // A whole word at the start beats one inside; both beat scattered letters.
        assert!(score("set", "Settings") > score("set", "Reset machines"));
        assert!(score("dark", "Theme: Dark") > score("drk", "Theme: Dark"));
    }
}
