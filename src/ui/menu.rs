//! Right-click menus for machines, sessions and repositories.

use gpui::{AnyElement, ClipboardItem, Context, FontWeight, MouseButton, Pixels, Point, Window, anchored, deferred, div, prelude::*, px};

use super::kit::{self, icon};
use super::root::{Root, Route};
use super::theme::Theme;
use crate::fleet::sessions::{AGENTS, StartSession};

pub enum MenuTarget {
    Machine(String),
    Session(String),
    /// A GitHub repository, "owner/name".
    Repo(String),
    /// An issue or pull request.
    Item { repo: String, number: u64, pull: bool, url: String },
    /// The GitHub accounts to switch between.
    GithubAccounts,
    /// An agent workspace, by id.
    Workspace(String),
}

pub struct ContextMenu {
    pub target: MenuTarget,
    /// Where the click was, in window coordinates.
    pub position: Point<Pixels>,
}

#[derive(Clone)]
enum Action {
    NewTerminal(String),
    ShowDesktop(String),
    CopyFiles(String),
    NewAgent(String, &'static str),
    Check(String),
    InstallRunner(String),
    Remove(String),
    CopyAttach(String),
    Restart(String),
    End(String),
    Favorite(String, bool),
    OpenUrl(String),
    CopyUrl(String),
    ItemToAgent(String, u64, bool),
    UseAccount(String, Option<String>),
    WorkspaceSession(String),
    DeleteWorkspace(String),
    AddAccount,
}

enum Item {
    Action { label: String, glyph: Glyph, action: Action, danger: bool },
    /// Text that isn't clickable.
    Note(String),
    Separator,
}

enum Glyph {
    Icon(&'static str),
    Agent(&'static str),
    /// The one picked (a plain check).
    Selected,
    None,
}

impl Root {
    pub(super) fn open_menu(&mut self, target: MenuTarget, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.context_menu = Some(ContextMenu { target, position });
        cx.notify();
    }

    fn menu_items(&self) -> Vec<Item> {
        let Some(menu) = &self.context_menu else { return Vec::new() };
        let s = &self.snapshot;
        let act = |label: &str, icon_name: &'static str, action: Action| Item::Action { label: label.into(), glyph: Glyph::Icon(icon_name), action, danger: false };
        match &menu.target {
            MenuTarget::Machine(id) => {
                let Some(m) = s.machines.iter().find(|m| &m.id == id) else { return Vec::new() };
                let mut items = Vec::new();
                if !m.host.is_empty() {
                    items.push(act("New terminal", "square-terminal", Action::NewTerminal(id.clone())));
                    for a in AGENTS.iter().filter(|a| a.id != "shell" && m.agents.iter().any(|i| i == a.id)) {
                        items.push(Item::Action { label: format!("New {} session…", a.name), glyph: Glyph::Agent(a.id), action: Action::NewAgent(id.clone(), a.id), danger: false });
                    }
                    if super::format::has_desktop(m) && !m.local {
                        items.push(act("Show desktop", "monitor", Action::ShowDesktop(id.clone())));
                    }
                    if !m.platform.starts_with("windows") {
                        items.push(act("Copy files…", "folder", Action::CopyFiles(id.clone())));
                    }
                    items.push(Item::Separator);
                    items.push(act("Check now", "refresh-cw", Action::Check(id.clone())));
                    items.push(act("Install runner…", "plus", Action::InstallRunner(id.clone())));
                    items.push(Item::Separator);
                }
                items.push(Item::Action { label: "Remove from fleet…".into(), glyph: Glyph::Icon("trash-2"), action: Action::Remove(id.clone()), danger: true });
                items
            }
            MenuTarget::Session(id) => {
                let Some(x) = s.sessions.iter().find(|x| &x.id == id) else { return Vec::new() };
                let mut items = Vec::new();
                if x.persistent {
                    items.push(act("Copy attach command", "copy", Action::CopyAttach(id.clone())));
                }
                if x.status == "exited" {
                    items.push(act("Restart", "rotate-ccw", Action::Restart(id.clone())));
                }
                if !items.is_empty() {
                    items.push(Item::Separator);
                }
                items.push(Item::Action { label: "End session…".into(), glyph: Glyph::Icon("power"), action: Action::End(id.clone()), danger: true });
                items
            }
            MenuTarget::Repo(name) => {
                let favorite = self.gh_is_favorite(name);
                vec![
                    if favorite {
                        act("Remove from favorites", "star", Action::Favorite(name.clone(), false))
                    } else {
                        act("Add to favorites", "star-filled", Action::Favorite(name.clone(), true))
                    },
                    Item::Separator,
                    act("Open on GitHub", "external-link", Action::OpenUrl(format!("https://github.com/{name}"))),
                    act("Copy link", "copy", Action::CopyUrl(format!("https://github.com/{name}"))),
                ]
            }
            MenuTarget::Item { repo, number, pull, url } => {
                let verb = if *pull { "Review" } else { "Fix" };
                vec![
                    act(&format!("{verb} with agent…"), "bot", Action::ItemToAgent(repo.clone(), *number, false)),
                    act(&format!("{verb} in a new workspace…"), "layers", Action::ItemToAgent(repo.clone(), *number, true)),
                    Item::Separator,
                    act("Open on GitHub", "external-link", Action::OpenUrl(url.clone())),
                    act("Copy link", "copy", Action::CopyUrl(url.clone())),
                ]
            }
            MenuTarget::Workspace(id) => {
                let Some(w) = s.workspaces.iter().find(|w| &w.id == id) else { return Vec::new() };
                let mut items = Vec::new();
                if w.status == "ready" {
                    items.push(act("New session here", "square-terminal", Action::WorkspaceSession(id.clone())));
                }
                items.push(act("Copy shell command", "copy", Action::CopyUrl(w.shell.clone())));
                items.push(Item::Separator);
                items.push(Item::Action { label: "Delete workspace…".into(), glyph: Glyph::Icon("trash-2"), action: Action::DeleteWorkspace(id.clone()), danger: true });
                items
            }
            MenuTarget::GithubAccounts => {
                use super::github::Load;
                let mut items = Vec::new();
                match &self.gh.accounts {
                    Some(Load::Ready(a)) if !a.token_source.is_empty() && a.token_source != "gh" => {
                        items.push(Item::Note(format!("Using the token in ${}", a.token_source)));
                        items.push(Item::Note("Unset it to switch accounts.".into()));
                        return items;
                    }
                    Some(Load::Ready(a)) => {
                        let using = |login: &str, machine: Option<&str>| a.machine_id.as_deref() == machine && a.using.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(login));
                        if !a.machines.is_empty() && !a.accounts.is_empty() {
                            items.push(Item::Note("On this computer".into()));
                        }
                        for x in &a.accounts {
                            let label = if x.ok { format!("@{}", x.login) } else { format!("@{} (sign in again)", x.login) };
                            let glyph = if using(&x.login, None) { Glyph::Selected } else { Glyph::None };
                            items.push(Item::Action { label, glyph, action: Action::UseAccount(x.login.clone(), None), danger: false });
                        }
                        if !a.machines.is_empty() {
                            items.push(Item::Note("Signed in on your machines".into()));
                        }
                        for m in &a.machines {
                            let glyph = if using(&m.login, Some(&m.machine_id)) { Glyph::Selected } else { Glyph::None };
                            let label = format!("@{} · {}", m.login, m.machine);
                            items.push(Item::Action { label, glyph, action: Action::UseAccount(m.login.clone(), Some(m.machine_id.clone())), danger: false });
                        }
                    }
                    Some(Load::Failed(e)) => items.push(Item::Note(e.clone())),
                    _ => items.push(Item::Note("Loading accounts…".into())),
                }
                items.push(Item::Separator);
                items.push(act("Add another account…", "plus", Action::AddAccount));
                items
            }
        }
    }

    fn run_menu_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        match action {
            Action::NewTerminal(machine) => {
                // A shell in the home folder, right away.
                let direct = self.snapshot.machines.iter().find(|m| m.id == machine).is_some_and(super::sessions::runs_in_app);
                let req = StartSession { machine_id: machine, agent: "shell".into(), cwd: "~".into(), direct, ..Default::default() };
                self.start_session(req, window, cx);
            }
            Action::NewAgent(machine, agent) => self.show_new_session_with(Some(machine), Some(agent), window, cx),
            Action::ShowDesktop(machine) => self.navigate(Route::Desktop(machine), window, cx),
            Action::CopyFiles(machine) => self.show_copy_files(Some(machine), None, window, cx),
            Action::Check(machine) => self.engine.trigger_check(Some(machine)),
            Action::InstallRunner(machine) => self.show_install_dialog(&machine, window, cx),
            Action::Remove(machine) => {
                if let Some(m) = self.snapshot.machines.iter().find(|m| m.id == machine).cloned() {
                    self.confirm_remove_machine(&m, window, cx);
                }
            }
            Action::CopyAttach(id) => {
                if let Some(x) = self.snapshot.sessions.iter().find(|x| x.id == id) {
                    cx.write_to_clipboard(ClipboardItem::new_string(x.attach_command.clone()));
                    self.notify_user("Copied. Paste it into any terminal to attach.", false, cx);
                }
            }
            Action::Restart(id) => {
                self.navigate(Route::Session(id.clone()), window, cx);
                self.restart_session(&id, window, cx);
            }
            Action::End(id) => {
                if let Some(x) = self.snapshot.sessions.iter().find(|x| x.id == id).cloned() {
                    self.confirm_end_session(&x, window, cx);
                }
            }
            Action::Favorite(name, favorite) => self.gh_set_favorite(&name, favorite, cx),
            Action::UseAccount(login, machine) => {
                let using = matches!(&self.gh.accounts, Some(super::github::Load::Ready(a))
                    if a.machine_id == machine && a.using.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(&login)));
                if !using {
                    self.gh_use_account(login, machine, cx)
                }
            }
            Action::AddAccount => {
                // Signing in makes the new account the CLI's active one; stay on
                // this one until the user switches.
                if let Some(super::github::Load::Ready(a)) = &self.gh.accounts {
                    let mut settings = self.engine.settings();
                    if let (true, Some(using)) = (settings.github.account.is_empty(), a.using.clone()) {
                        settings.github.account = using;
                        let _ = self.engine.update_settings(settings);
                    }
                }
                window.dispatch_action(Box::new(super::root::SignInGitHub), cx)
            }
            Action::ItemToAgent(repo, number, workspace) => self.gh_item_to_agent(repo, number, workspace, window, cx),
            Action::OpenUrl(url) => cx.open_url(&url),
            Action::WorkspaceSession(id) => {
                if let Some(w) = self.snapshot.workspaces.iter().find(|w| w.id == id).cloned() {
                    self.new_workspace_session(&w, window, cx)
                }
            }
            Action::DeleteWorkspace(id) => {
                if let Some(w) = self.snapshot.workspaces.iter().find(|w| w.id == id).cloned() {
                    self.confirm_destroy_workspace(&w.id, &w.name, window, cx)
                }
            }
            Action::CopyUrl(url) => {
                cx.write_to_clipboard(ClipboardItem::new_string(url));
                self.notify_user("Copied the link.", false, cx);
            }
        }
        cx.notify();
    }

    /// The open menu, drawn above everything, with a backdrop that closes it.
    pub(super) fn render_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let position = self.context_menu.as_ref()?.position;
        let items = self.menu_items();
        if items.is_empty() {
            self.context_menu = None;
            return None;
        }
        let close = |this: &mut Root, _: &gpui::MouseDownEvent, _: &mut Window, cx: &mut Context<Root>| {
            this.context_menu = None;
            cx.notify();
        };
        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(i, item)| match item {
                Item::Separator => div().h(px(1.)).my(px(4.)).mx(px(8.)).bg(theme.border_variant).into_any_element(),
                Item::Note(text) => div().px(px(12.)).py(px(5.)).text_size(px(12.)).text_color(theme.text_placeholder).child(text).into_any_element(),
                Item::Action { label, glyph, action, danger } => {
                    let color = if danger { theme.error } else { theme.text };
                    let glyph = match glyph {
                        Glyph::Icon(name) => kit::color_tile(name, 18.).into_any_element(),
                        Glyph::Agent(agent) => {
                            let (name, (from, to)) = kit::agent_look(agent);
                            kit::tile(name, from, to, 18.).into_any_element()
                        }
                        Glyph::Selected => icon("check", 14., theme.accent).into_any_element(),
                        Glyph::None => div().into_any_element(),
                    };
                    div()
                        .id(("menu-item", i))
                        .flex()
                        .items_center()
                        .gap(px(9.))
                        .h(px(28.))
                        .mx(px(4.))
                        .px(px(8.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|st| st.bg(theme.element_hover))
                        .text_size(px(13.))
                        .text_color(color)
                        .child(div().w(px(18.)).flex().justify_center().child(glyph))
                        .child(label)
                        .on_click(cx.listener(move |this, _, window, cx| this.run_menu_action(action.clone(), window, cx)))
                        .into_any_element()
                }
            })
            .collect();
        let menu = div()
            .id("context-menu")
            .occlude()
            .w(px(240.))
            .py(px(4.))
            .rounded(px(9.))
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .font_weight(FontWeight::NORMAL)
            .children(rows);
        Some(
            div()
                .absolute()
                .inset_0()
                .child(div().id("menu-backdrop").absolute().inset_0().occlude().on_mouse_down(MouseButton::Left, cx.listener(close)).on_mouse_down(MouseButton::Right, cx.listener(close)))
                .child(deferred(anchored().position(position).snap_to_window_with_margin(px(8.)).child(menu)).with_priority(1))
                .into_any_element(),
        )
    }
}

/// Makes a sidebar row open its menu on right-click.
pub(super) fn on_right_click(target: impl Fn() -> MenuTarget + 'static, cx: &mut Context<Root>) -> impl Fn(&gpui::MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| this.open_menu(target(), e.position, cx))
}
