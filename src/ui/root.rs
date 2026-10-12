//! The root view: a full-height sidebar of machines, a page area with its own
//! toolbar and history, and an activity drawer that slides over from the right.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, ClickEvent, Context, DragMoveEvent, Entity, FocusHandle, Focusable, FontWeight, KeyBinding, Menu,
    MenuItem, MouseButton, ScrollHandle, SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};

use super::dialogs::{AddDisplayDialog, MonitorPasswordDialog, AddMachineDialog, ConfirmDialog, DialogEvent, InstallDialog, NewSessionDialog, NewWorkspaceDialog, RenameDialog, RenameTarget};
use super::terminal::TerminalView;
use super::format;
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, icon, spinner};
use super::settings::SettingsPage;
use super::theme::{ActiveTheme, mono_font, Theme, ui_font};
use super::workspaces::WorkspaceUi;
use super::activity::{ActivityFilter, ActivityItem};
use crate::fleet::workspaces::{CreateWorkspace, WorkspaceIsolation};
use crate::fleet::Engine;
use crate::fleet::sessions::StartSession;
use crate::fleet::model::{Action, Machine, Runner, Snapshot};
use crate::ops::RunnerAction;

actions!(
    muxen,
    [
        CheckNow,
        SyncGitHub,
        ShowOverview,
        ShowJobs,
        ShowWorkspaces,
        ShowGithub,
        GoToMachine1,
        GoToMachine2,
        GoToMachine3,
        GoToMachine4,
        GoToMachine5,
        GoToMachine6,
        GoToMachine7,
        GoToMachine8,
        GoToMachine9,
        OpenSettings,
        FocusFilter,
        GoBack,
        GoForward,
        ToggleActivity,
        ToggleSidebar,
        AddMachine,
        NewSession,
        SplitRight,
        SplitDown,
        NextPane,
        PreviousPane,
        QuickOpen,
        CommandPalette,
        ShowAbout,
        SignInGitHub,
        GrantGitHubAccess,
        Dismiss,
        Quit
    ]
);

const WORKSPACE: &str = "Root";
pub(super) const SIDEBAR_DEFAULT: f32 = 256.;
const SIDEBAR_MIN: f32 = 220.;
const SIDEBAR_MAX: f32 = 420.;
/// Utilization samples kept for the overview's pulse chart.
const PULSE_SAMPLES: usize = 64;
const PULSE_EVERY: Duration = Duration::from_secs(5);

/// The app's shortcuts, as the user has them (see shortcuts.rs), and the
/// few that don't change.
pub fn bind_keys(cx: &mut App) {
    let custom = super::shortcuts::bindings(cx);
    cx.bind_keys(custom);
    super::palette::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("escape", Dismiss, Some(WORKSPACE)),
        KeyBinding::new("/", FocusFilter, Some(WORKSPACE)),
    ]);
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Muxen".into(),
            items: vec![
                MenuItem::action("About Muxen", ShowAbout),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Quit Muxen", Quit),
            ],
        },
        Menu {
            name: "Machines".into(),
            items: vec![
                MenuItem::action("Check Machines", CheckNow),
                MenuItem::action("Sync GitHub", SyncGitHub),
                MenuItem::separator(),
                MenuItem::action("Add Machine…", AddMachine),
                MenuItem::action("New Session…", NewSession),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Go to…", QuickOpen),
                MenuItem::action("Command Palette…", CommandPalette),
                MenuItem::separator(),
                MenuItem::action("Overview", ShowOverview),
                MenuItem::action("Jobs", ShowJobs),
                MenuItem::action("Workspaces", ShowWorkspaces),
                MenuItem::action("GitHub", ShowGithub),
                MenuItem::action("Activity", ToggleActivity),
                MenuItem::separator(),
                MenuItem::action("Back", GoBack),
                MenuItem::action("Forward", GoForward),
            ],
        },
    ]
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Route {
    Overview,
    Jobs,
    Workspaces,
    Workspace(String),
    /// What needs you on GitHub.
    Github,
    GithubRepo(String),
    GithubItem(String, u64),
    Activity,
    Machine(String),
    Runner(String),
    /// An agent session: "<machine-id>/<name>".
    Session(String),
    /// A machine's desktop, live.
    Desktop(String),
    /// A machine's logs, live.
    Logs(String),
    /// A machine's services and containers.
    Services(String),
    Settings,
    About,
}

/// Drag payload for the sidebar's resize handle; renders nothing.
struct DraggedSidebar;

/// Drag payload for the divider between a terminal and a desktop.
pub(super) struct DraggedSplit;

impl Render for DraggedSplit {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Render for DraggedSidebar {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

pub enum Modal {
    AddMachine(Entity<AddMachineDialog>),
    Install(Entity<InstallDialog>),
    Confirm(Entity<ConfirmDialog>),
    NewWorkspace(Entity<NewWorkspaceDialog>),
    NewSession(Entity<NewSessionDialog>),
    Licenses(Entity<super::about::LicensesDialog>),
    Copy(Entity<super::files::CopyDialog>),
    Cloud(Entity<super::cloud_dialog::CloudDialog>),
    Rename(Entity<RenameDialog>),
    AddDisplay(Entity<AddDisplayDialog>),
    MonitorPassword(Entity<MonitorPasswordDialog>),
}

pub struct Notice {
    pub text: SharedString,
    pub error: bool,
    pub at: Instant,
}

/// One point on the pulse chart.
#[derive(Clone, Copy)]
pub struct PulseSample {
    pub working: usize,
    pub online: usize,
}

pub struct Root {
    pub(super) engine: Arc<Engine>,
    pub(super) snapshot: Arc<Snapshot>,
    pub(super) focus: FocusHandle,
    pub(super) route: Route,
    back: Vec<Route>,
    forward: Vec<Route>,
    pub(super) filter: Entity<TextInput>,
    pub(super) collapsed: HashSet<String>,
    pub(super) activity_filter: ActivityFilter,
    pub(super) activity_selected: Option<ActivityItem>,
    pub(super) activity_scroll: ScrollHandle,
    pub(super) activity_seen_len: usize,
    pub(super) modal: Option<Modal>,
    pub(super) settings: Option<Entity<SettingsPage>>,
    pub(super) notice: Option<Notice>,
    pub(super) page_scroll: ScrollHandle,
    pub(super) pulse: VecDeque<PulseSample>,
    /// The overview lists every notice, not just the first few.
    pub(super) heads_up_open: bool,
    last_pulse: Option<Instant>,
    /// Width of the page area, for layouts that change with the window size.
    pub(super) content_width: f32,
    pub(super) sidebar_width: f32,
    sidebar_resizing: bool,
    /// The sidebar is tucked away (the toolbar's button, ⌘B).
    pub(super) sidebar_hidden: bool,
    /// ⌘ is held down: shortcuts show next to what they do. `keys_soon` shows
    /// them after a moment, so a quick ⌘C doesn't flash them.
    pub(super) show_keys: bool,
    keys_soon: Option<Task<()>>,
    /// Terminals attached to agent sessions, kept while the app runs so
    /// switching pages doesn't drop them.
    pub(super) terminals: HashMap<String, Entity<TerminalView>>,
    /// Terminals whose connection dropped, being re-attached: tries so far,
    /// and whether one is on its way.
    pub(super) resuming: HashMap<String, (u32, bool)>,
    /// Machines whose sessions a page asked for before discovery finished.
    pub(super) sessions_requested: HashSet<String>,
    /// The GitHub page's data and selection.
    pub(super) gh: super::github::GithubUi,
    /// Open desktop views by machine id: the Desktop page's, or the one next to
    /// a session's terminal. Only what's on screen streams.
    desktops: std::collections::HashMap<String, Entity<super::desktop::DesktopView>>,
    /// Sessions showing their machine's desktop beside the terminal.
    pub(super) split: HashSet<String>,
    /// Log viewers open, by machine (each an item in the sidebar while open).
    pub(super) logs: HashMap<String, Entity<super::logs::LogsView>>,
    /// Services pages open, by machine (each an item in the sidebar while open).
    pub(super) service_pages: HashMap<String, Entity<super::services::ServicesView>>,
    /// Terminals split into panes (see panes.rs); a session page shows the layout it's in.
    pub(super) panes: Vec<super::panes::Panes>,
    /// The terminal's share of a split page, and whether its divider is being dragged.
    pub(super) split_ratio: f32,
    pub(super) split_resizing: bool,
    /// How git is set up on each machine (the machine page's Git card), and fixes in flight.
    pub(super) machine_git: std::collections::HashMap<String, super::github::Load<crate::api::GitSetupOut>>,
    pub(super) git_fixing: std::collections::HashSet<String>,
    /// How each machine is reached (the machine page's Connection card), and changes in flight.
    pub(super) machine_conn: std::collections::HashMap<String, super::github::Load<crate::fleet::connection::Connection>>,
    pub(super) conn_changing: std::collections::HashSet<String>,
    /// What's listening on each machine (the machine page's Servers), the
    /// machines whose other ports are shown, and the field to open one by number.
    pub(super) machine_ports: std::collections::HashMap<String, super::github::Load<Vec<crate::fleet::ports::ListeningPort>>>,
    pub(super) ports_more: std::collections::HashSet<String>,
    /// Forwarded ports seen listening there (so one that stops can be closed here).
    pub(super) forward_seen: std::collections::HashSet<(String, u16)>,
    pub(super) port_input: Entity<TextInput>,
    /// Machines whose "can't reach it" card shows what ssh said.
    pub(super) offline_details: std::collections::HashSet<String>,
    /// The open right-click menu.
    pub(super) context_menu: Option<super::menu::ContextMenu>,
    /// ⌘P / ⇧⌘P, while it's open.
    pub(super) palette: Option<super::palette::Palette>,
    /// Focus the root on the next render (the focused terminal went away).
    refocus: bool,
    /// The newest session notice already announced (None until the first snapshot).
    last_notice: Option<u64>,
    window_active: bool,
    pub(super) ws_ui: WorkspaceUi,
    /// This computer's clipboard as last seen (see watch_clipboard).
    clipboard_seen: (i64, String),
    _tasks: Vec<Task<()>>,
    pub(super) _subscriptions: Vec<Subscription>,
}

impl Root {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx, "Search"));
        let mut subscriptions = vec![cx.subscribe_in(&filter, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::Changed => cx.notify(),
            InputEvent::Cancel => {
                this.filter.update(cx, |f, cx| f.set_text("", cx));
                window.focus(&this.focus);
            }
            InputEvent::Submit => {
                // Enter opens the first match.
                if let Some(target) = this.first_filter_match(cx) {
                    this.navigate(target, window, cx);
                }
            }
        })];
        subscriptions.push(cx.observe_window_appearance(window, |_, window, cx| {
            super::theme::apply(window, cx);
            cx.notify();
        }));

        // Snapshots arrive from the engine's tokio runtime over a watch channel.
        let mut rx = engine.subscribe();
        let snapshot = rx.borrow_and_update().clone();
        let updates = cx.spawn(async move |this, cx| {
            while rx.changed().await.is_ok() {
                let next = rx.borrow_and_update().clone();
                if this.update(cx, |this, cx| this.apply_snapshot(next, cx)).is_err() {
                    break;
                }
            }
        });
        // Relative times ("2m ago", running job timers) tick once a second.
        // The shared clipboard: what's copied here goes to the machines, what's
        // copied there lands here (see fleet::clipboard).
        let clipboard_out = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(400)).await;
                if this.update(cx, |this, cx| this.watch_clipboard(cx)).is_err() {
                    break;
                }
            }
        });
        let mut copied_there = crate::fleet::clipboard::subscribe();
        let clipboard_in = cx.spawn(async move |this, cx| {
            loop {
                match copied_there.recv().await {
                    Ok(text) => {
                        let alive = this.update(cx, |this, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                            this.clipboard_seen = (super::pasteboard_state().map(|s| s.0).unwrap_or(0), text);
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    if this.notice.as_ref().is_some_and(|n| n.at.elapsed() > Duration::from_secs(6)) {
                        this.notice = None;
                    }
                    this.record_pulse();
                    cx.notify()
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        // The machine page's "Open a port": a number, then Return.
        let port_input = cx.new(|cx| TextInput::new(cx, "Port, e.g. 3000"));
        subscriptions.push(cx.subscribe_in(&port_input, window, |this, input, e: &InputEvent, _, cx| {
            if *e != InputEvent::Submit {
                return;
            }
            let text = input.read(cx).text().trim().trim_start_matches(':').to_string();
            let Route::Machine(machine) = this.route.clone() else { return };
            match text.parse::<u16>() {
                Ok(port) if port > 0 => {
                    input.update(cx, |i, cx| i.set_text("", cx));
                    this.open_port(&machine, port, None, true, cx);
                }
                _ => this.notify_user(format!("\"{text}\" isn't a port number."), true, cx),
            }
        }));
        let mut ws = Root {
            engine,
            snapshot,
            focus: cx.focus_handle(),
            route: Route::Overview,
            back: Vec::new(),
            forward: Vec::new(),
            filter,
            collapsed: HashSet::new(),
            activity_filter: ActivityFilter::default(),
            activity_selected: None,
            activity_scroll: ScrollHandle::new(),
            activity_seen_len: 0,
            modal: None,
            settings: None,
            notice: None,
            page_scroll: ScrollHandle::new(),
            pulse: VecDeque::with_capacity(PULSE_SAMPLES),
            heads_up_open: false,
            last_pulse: None,
            content_width: 1000.,
            sidebar_width: cx.global::<super::theme::Appearance>().sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX),
            sidebar_resizing: false,
            sidebar_hidden: cx.global::<super::theme::Appearance>().sidebar_hidden,
            show_keys: false,
            keys_soon: None,
            terminals: HashMap::new(),
            resuming: HashMap::new(),
            sessions_requested: HashSet::new(),
            gh: Default::default(),
            desktops: Default::default(),
            split: Default::default(),
            logs: HashMap::new(),
            service_pages: HashMap::new(),
            panes: cx.global::<super::theme::Appearance>().panes.iter().filter_map(|t| super::panes::Panes::decode(t)).filter(|p| p.ids().len() > 1).collect(),
            split_ratio: cx.global::<super::theme::Appearance>().split_ratio.clamp(0.2, 0.8),
            split_resizing: false,
            machine_git: Default::default(),
            git_fixing: Default::default(),
            machine_conn: Default::default(),
            conn_changing: Default::default(),
            machine_ports: Default::default(),
            ports_more: Default::default(),
            forward_seen: Default::default(),
            port_input,
            offline_details: Default::default(),
            last_notice: None,
            refocus: false,
            context_menu: None,
            palette: None,
            window_active: false,
            ws_ui: WorkspaceUi::default(),
            clipboard_seen: (super::pasteboard_state().map(|s| s.0).unwrap_or(0), String::new()),
            _tasks: vec![updates, ticker, clipboard_out, clipboard_in],
            _subscriptions: subscriptions,
        };
        ws.record_pulse();
        ws.restore_forwards(cx);
        // Servers starting and stopping: forwards whose server stopped close,
        // and the machine page shown keeps its Servers current.
        let watch = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(8)).await;
                if this.update(cx, |this, cx| this.watch_servers(cx)).is_err() {
                    break;
                }
            }
        });
        ws._tasks.push(watch);
        ws
    }

    fn apply_snapshot(&mut self, snapshot: Arc<Snapshot>, cx: &mut Context<Self>) {
        // Keep the activity log pinned to the bottom while output streams in.
        let selected = self.selected_operation_id_in(&snapshot);
        let len = selected.and_then(|id| snapshot.operations.iter().find(|o| o.id == id)).map(|o| o.log.len()).unwrap_or(0);
        if len != self.activity_seen_len {
            self.activity_seen_len = len;
            self.activity_scroll.scroll_to_bottom();
        }
        self.announce_notices(&snapshot);
        // Terminals of sessions that ended go away (one without the agent stops with its terminal).
        self.terminals.retain(|id, _| snapshot.sessions.iter().any(|s| &s.id == id));
        // Looking at a session that just closed: back to its machine.
        if let Route::Session(id) = self.route.clone() {
            let gone = !snapshot.sessions.iter().any(|s| s.id == id) && self.snapshot.sessions.iter().any(|s| s.id == id);
            if gone {
                // In a split: the pane next to it.
                let next = self.panes.iter().find(|p| p.contains(&id)).and_then(|p| p.ids().into_iter().find(|x| *x != id && snapshot.sessions.iter().any(|s| s.id == *x)).map(str::to_string));
                match next {
                    Some(next) => self.route = Route::Session(next),
                    None => self.show(Route::Machine(id.split('/').next().unwrap_or_default().to_string()), cx),
                }
                self.refocus = true;
            }
        }
        self.prune_panes(&snapshot, cx);
        // Favorites of machines that are gone (removed, destroyed) go too. Not
        // while the list is empty: that's before config.toml has been read.
        let favorites = &cx.global::<super::theme::Appearance>().favorite_machines;
        if !snapshot.machines.is_empty() && favorites.iter().any(|f| !snapshot.machines.iter().any(|m| &m.id == f)) {
            let ids: Vec<String> = snapshot.machines.iter().map(|m| m.id.clone()).collect();
            super::theme::remember(cx, |a| a.favorite_machines.retain(|f| ids.contains(f)));
        }
        self.snapshot = snapshot;
        cx.notify();
    }

    /// Session notices show as colored dots in the sidebar until the session is
    /// opened; one for the session you're looking at counts as read right away.
    fn announce_notices(&mut self, snapshot: &Snapshot) {
        let newest = snapshot.notifications.first().map(|n| n.id).unwrap_or(0);
        let seen = self.last_notice.unwrap_or(newest);
        for n in snapshot.notifications.iter().filter(|n| n.id > seen) {
            if self.window_active && self.route == Route::Session(n.session_id.clone()) {
                self.engine.ack_session(&n.session_id);
            }
        }
        self.last_notice = Some(newest.max(seen));
    }

    fn record_pulse(&mut self) {
        if self.last_pulse.is_some_and(|t| t.elapsed() < PULSE_EVERY) {
            return;
        }
        self.last_pulse = Some(Instant::now());
        let c = &self.snapshot.counts;
        if self.pulse.len() == PULSE_SAMPLES {
            self.pulse.pop_front();
        }
        self.pulse.push_back(PulseSample { working: c.working, online: c.working + c.idle });
    }

    // -----------------------------------------------------------------------
    // Navigation

    pub(super) fn navigate(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        self.show(route, cx);
        self.on_route_changed(window, cx);
    }

    /// Navigation that doesn't need the window (the settings page is created on first render).
    pub(super) fn show(&mut self, route: Route, cx: &mut Context<Self>) {
        if route == self.route {
            return;
        }
        let previous = std::mem::replace(&mut self.route, route);
        self.back.push(previous);
        self.forward.clear();
        self.page_scroll = ScrollHandle::new();
        cx.notify();
    }

    /// Shows an operation's live output on the Activity page.
    pub(super) fn show_operation(&mut self, op: u64, cx: &mut Context<Self>) {
        self.activity_filter = ActivityFilter::All;
        self.activity_selected = Some(ActivityItem::Op(op));
        self.activity_seen_len = usize::MAX;
        self.show(Route::Activity, cx);
    }

    pub(super) fn on_route_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.route == Route::Settings && self.settings.is_none() {
            let engine = self.engine.clone();
            self.settings = Some(cx.new(|cx| SettingsPage::new(engine, window, cx)));
        }
        // Leaving Settings while it waits for a shortcut's keys: stop waiting.
        if self.route != Route::Settings {
            if let Some(page) = &self.settings {
                page.update(cx, |p, cx| p.stop_recording(cx));
            }
        }
        // A machine's page: what's listening there, fresh (keeping what was shown meanwhile).
        if let Route::Machine(id) = self.route.clone()
            && self.machine_ports.contains_key(&id)
        {
            self.load_machine_ports(&id, cx);
        }
        // A desktop streams only while it's shown: its page, or beside a session.
        let showing = match &self.route {
            Route::Desktop(id) => Some(id.clone()),
            Route::Session(sid) if self.page_split(sid).is_some() => sid.split('/').next().map(str::to_string),
            _ => None,
        };
        // Desktops in the sidebar keep streaming, so going back is instant.
        if let Route::Desktop(id) = self.route.clone() {
            self.pin_desktop(&id, cx);
        }
        let pinned = cx.global::<super::theme::Appearance>().desktops.clone();
        self.desktops.retain(|id, _| Some(id) == showing.as_ref() || pinned.contains(id));
        if let (Some(id), Route::Session(_)) = (&showing, &self.route) {
            if !self.desktops.contains_key(id) {
                let (engine, mid) = (self.engine.clone(), id.clone());
                let view = cx.new(|cx| super::desktop::DesktopView::new(engine, mid, window, cx));
                self.desktops.insert(id.clone(), view);
            }
        }
        // A session page types into its terminal, a desktop into the machine;
        // other pages take keys at the root.
        match self.route.clone() {
            Route::Session(id) => self.open_terminal(&id, window, cx),
            Route::Desktop(id) => {
                let engine = self.engine.clone();
                let view = self.desktops.entry(id.clone()).or_insert_with(|| cx.new(|cx| super::desktop::DesktopView::new(engine, id, window, cx))).clone();
                window.focus(&view.focus_handle(cx));
            }
            Route::Logs(id) => {
                let view = self.logs_view(&id, window, cx);
                window.focus(&view.focus_handle(cx));
            }
            Route::Services(id) => {
                if !self.service_pages.contains_key(&id) {
                    let engine = self.engine.clone();
                    let view = cx.new(|cx| super::services::ServicesView::new(engine, id.clone(), window, cx));
                    // Its Logs buttons: that log in the machine's log viewer.
                    let machine = id.clone();
                    let sub = cx.subscribe_in(&view, window, move |this, _, e: &super::services::ServicesEvent, window, cx| match e {
                        // A Compose service's port: open here, in the browser.
                        super::services::ServicesEvent::Port(port) => this.open_port(&machine, *port, None, true, cx),
                        super::services::ServicesEvent::Logs(source) => {
                            let logs = this.logs_view(&machine, window, cx);
                            logs.update(cx, |v, cx| v.show_source(source.clone(), window, cx));
                            this.navigate(Route::Logs(machine.clone()), window, cx);
                        }
                        // A container's shell, an editor on a compose file: a terminal session.
                        super::services::ServicesEvent::Terminal { cwd, command, title } => {
                            let req = crate::fleet::sessions::StartSession {
                                machine_id: machine.clone(),
                                agent: "shell".into(),
                                cwd: cwd.clone(),
                                title: Some(title.clone()),
                                command: Some(command.clone()),
                                ..Default::default()
                            };
                            this.start_session(req, window, cx);
                        }
                    });
                    self._subscriptions.push(sub);
                    self.service_pages.insert(id.clone(), view);
                }
                window.focus(&self.service_pages[&id].focus_handle(cx));
            }
            _ => window.focus(&self.focus),
        }
        self.page_scroll = ScrollHandle::new();
        cx.notify();
    }

    /// Opens a view by name; see `--view` in main.rs.
    pub fn open_view(&mut self, view: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (kind, arg) = view.split_once(':').unwrap_or((view, ""));
        match kind {
            "jobs" => self.navigate(Route::Jobs, window, cx),
            "workspaces" => self.navigate(Route::Workspaces, window, cx),
            "workspace" => self.navigate(Route::Workspace(arg.into()), window, cx),
            "new-workspace" => {
                self.show_new_workspace(None, window, cx);
                if arg == "docker" {
                    if let Some(Modal::NewWorkspace(d)) = &self.modal {
                        d.update(cx, |d, cx| d.set_docker(true, cx));
                    }
                }
            }
            "confirm-remove" => {
                if let Some(r) = self.snapshot.runners.iter().find(|r| r.id == arg).cloned() {
                    self.confirm_remove_runner(&r, window, cx);
                }
            }
            "size" | "display" => {}
            "session" => self.navigate(Route::Session(arg.into()), window, cx),
            // "new-session:<machine>", or "new-session:<machine>@<folder>".
            "new-session" => match arg.split_once('@') {
                Some((m, folder)) => self.show_new_session_prefilled(Some(m.to_string()), Some(folder.to_string()), Default::default(), window, cx),
                None => self.show_new_session(Some(arg.to_string()).filter(|a| !a.is_empty()), window, cx),
            },
            // An issue or pull request handed to an agent: "fix:owner/name#12", or in a new workspace "fix-ws:...".
            "fix" | "fix-ws" => {
                if let Some((repo, n)) = arg.split_once('#') {
                    self.gh_item_to_agent(repo.to_string(), n.parse().unwrap_or(0), kind == "fix-ws", window, cx);
                }
            }
            "sidebar" if arg == "hidden" => {
                self.sidebar_hidden = true;
                cx.notify();
            }
            "sidebar" => {
                if let Ok(w) = arg.parse::<f32>() {
                    self.sidebar_width = w.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                    cx.notify();
                }
            }
            "activity-filter" => {
                self.activity_filter = match arg {
                    "failed" => ActivityFilter::Failed,
                    "operations" => ActivityFilter::Operations,
                    "agents" => ActivityFilter::Agents,
                    _ => ActivityFilter::All,
                };
                self.show(Route::Activity, cx);
            }
            // "settings:<tab>", and "settings:icons" (Appearance, scrolled to the icon styles).
            "settings" => {
                use super::settings::SettingsTab;
                let tab = match arg {
                    "appearance" | "icons" => SettingsTab::Appearance,
                    "terminal" => SettingsTab::Terminal,
                    "github" => SettingsTab::GitHub,
                    "cloud" => SettingsTab::Cloud,
                    "keyboard" => SettingsTab::Keyboard,
                    _ => SettingsTab::General,
                };
                self.open_settings_tab(tab, window, cx);
                if arg == "icons" {
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(Duration::from_millis(400)).await;
                        let _ = this.update(cx, |this, cx| {
                            if let Some(p) = &this.settings {
                                p.read(cx).scroll.scroll_to_bottom();
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
            }
            // Shortcuts shown, as while ⌘ is held (for screenshots).
            "keys" => {
                self.show_keys = true;
                cx.notify();
            }
            "machine" => self.navigate(Route::Machine(arg.into()), window, cx),
            "runner" => self.navigate(Route::Runner(arg.into()), window, cx),
            "install" => {
                self.navigate(Route::Machine(arg.into()), window, cx);
                self.show_install_dialog(arg, window, cx);
            }
            "add-machine" => self.show_add_machine(window, cx),
            "add-display" => self.show_add_display(arg.into(), window, cx),
            // "rename:<machine-id>", and "rename-session:<session-id>" (the first session without one).
            "rename" => self.show_rename(RenameTarget::Machine(arg.into()), window, cx),
            "rename-session" => {
                let id = if arg.is_empty() { self.snapshot.sessions.first().map(|x| x.id.clone()) } else { Some(arg.to_string()) };
                if let Some(id) = id {
                    self.show_rename(RenameTarget::Session(id), window, cx)
                }
            }
            "menu" => self.open_menu(super::menu::MenuTarget::Machine(arg.into()), gpui::point(px(260.), px(160.)), cx),
            "runner-menu" => self.open_menu(super::menu::MenuTarget::Runner(arg.into()), gpui::point(px(200.), px(560.)), cx),
            "favorite" => self.set_favorite_machine(arg, true, cx),
            // The palette with this text typed: "palette:" (places), "palette:>set" (commands).
            "palette" => self.open_palette_with(arg, window, cx),
            "themes" => {
                self.open_palette(true, window, cx);
                self.choose_theme(cx);
            }
            "cloud" => self.show_cloud_dialog(window, cx),
            // An icon style ("icons:tiles"), for screenshots.
            "icons" => super::theme::update_appearance(window, cx, |a, _| a.icon_style = arg.into()),
            // The first queued job's menu.
            "job-menu" => {
                if let Some(id) = self.snapshot.jobs.queued.first().map(|j| j.id) {
                    self.open_menu(super::menu::MenuTarget::Job(id), gpui::point(px(900.), px(150.)), cx)
                }
            }
            // "copy:<from>", or "copy:<from>[@<folder>]><to>:<file>+<file>" with files picked (for screenshots).
            "copy" => match arg.split_once('>') {
                Some((from, rest)) => {
                    let (from, from_dir) = from.split_once('@').unwrap_or((from, "~"));
                    let (to, files) = rest.split_once(':').unwrap_or((rest, ""));
                    let selected = files.split('+').filter(|f| !f.is_empty()).map(String::from).collect();
                    let start = super::files::CopyStart { from: from.into(), from_dir: from_dir.into(), selected, to: to.into(), to_dir: "~".into() };
                    self.show_copy_files(None, Some(start), window, cx)
                }
                None => self.show_copy_files(Some(arg.to_string()).filter(|a| !a.is_empty()), None, window, cx),
            },
            "logs" => {
                self.navigate(Route::Runner(arg.into()), window, cx);
                // Give the first machine check a moment, so the runner's folder is known.
                let runner = arg.to_string();
                let task = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(3)).await;
                    let _ = this.update(cx, |this, cx| this.runner_action(&runner, RunnerAction::Logs, cx));
                });
                self._tasks.push(task);
            }
            "activity" => self.show(Route::Activity, cx),
            "about" => self.navigate(Route::About, window, cx),
            // "desktop:<machine>", or "desktop:<machine>@<display>" for one of its displays.
            "desktop" => match arg.split_once('@') {
                Some((machine, display)) => self.show_display(machine.into(), Some(display.into()), window, cx),
                None => self.navigate(Route::Desktop(arg.into()), window, cx),
            },
            "desktop-window" => {
                let name = self.snapshot.machines.iter().find(|m| m.id == arg).map(|m| m.name.clone()).unwrap_or_else(|| arg.to_string());
                super::desktop::open_window(self.engine.clone(), arg.to_string(), None, name, cx);
            }
            // Sessions split into panes: "panes:r(<id>+d(<id>+<id>))" (`+` for
            // the layout's commas, which separate views).
            "panes" => {
                if let Some(layout) = super::panes::Panes::decode(&arg.replace('+', ",")).filter(|p| p.ids().len() > 1) {
                    let first = layout.ids()[0].to_string();
                    self.panes.retain(|p| !layout.ids().iter().any(|id| p.contains(id)));
                    self.panes.push(layout);
                    let saved: Vec<String> = self.panes.iter().map(super::panes::Panes::encode).collect();
                    super::theme::remember(cx, |a| a.panes = saved);
                    self.navigate(Route::Session(first), window, cx);
                }
            }
            // "machine-logs:<machine>", or "machine-logs:<machine>@compose/<project>" for that log.
            "machine-logs" => {
                let (machine, what) = arg.split_once('@').unwrap_or((arg, ""));
                self.navigate(Route::Logs(machine.into()), window, cx);
                if let Some(project) = what.strip_prefix("compose/") {
                    let source = crate::fleet::logs::LogSource { kind: "compose".into(), target: project.into(), name: project.into(), project: Some(project.into()), ..Default::default() };
                    let view = self.logs_view(machine, window, cx);
                    view.update(cx, |v, cx| v.show_source(source, window, cx));
                }
            }
            // "machine-services:<machine>" or "machine-services:<machine>@<tab>".
            "machine-services" => {
                let (machine, tab) = arg.split_once('@').unwrap_or((arg, ""));
                self.navigate(Route::Services(machine.into()), window, cx);
                if let Some(view) = self.service_pages.get(machine).cloned() {
                    view.update(cx, |v, cx| v.show_tab(tab, window, cx));
                }
            }
            // The session shown, split: a new shell beside it or below it.
            // (once the session is known: sessions are found over the network).
            "split-right" | "split-down" => {
                let down = kind == "split-down";
                let task = cx.spawn_in(window, async move |this, cx| {
                    for _ in 0..100 {
                        let known = this
                            .update(cx, |this, _| matches!(&this.route, Route::Session(id) if this.snapshot.sessions.iter().any(|s| &s.id == id)))
                            .unwrap_or(false);
                        if known {
                            let _ = this.update_in(cx, |this, window, cx| this.split_session(down, window, cx));
                            return;
                        }
                        cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
                    }
                });
                self.track(task);
            }
            // A session with its machine's desktop beside the terminal.
            "split" => {
                self.split.insert(arg.to_string());
                self.navigate(Route::Session(arg.into()), window, cx);
            }
            "github" => match arg.split_once('#') {
                Some((repo, n)) => self.navigate(Route::GithubItem(repo.into(), n.parse().unwrap_or(0)), window, cx),
                None if !arg.is_empty() => self.navigate(Route::GithubRepo(arg.into()), window, cx),
                None => self.navigate(Route::Github, window, cx),
            },
            "bottom" => {
                self.page_scroll.scroll_to_bottom();
                if let (Route::Settings, Some(page)) = (&self.route, &self.settings) {
                    page.read(cx).scroll.scroll_to_bottom();
                }
            }
            // "forward:<machine>:<port>": that port of the machine open here (no browser).
            "forward" => {
                if let Some((machine, port)) = arg.rsplit_once(':')
                    && let Ok(port) = port.parse::<u16>()
                {
                    self.open_port(machine, port, None, false, cx);
                }
            }
            "theme" => {
                let id = arg.to_string();
                if let Some(p) = super::themes::find(&id) {
                    let dark = p.dark;
                    super::theme::update_appearance(window, cx, |a, _| {
                        if dark { a.dark = id.clone() } else { a.light = id.clone() }
                        a.mode = if dark { super::theme::Mode::Dark } else { super::theme::Mode::Light };
                    });
                }
            }
            _ => tracing::warn!("unknown view {view:?}"),
        }
    }

    /// Lets a background UI task finish on its own. They only hold a weak handle
    /// to this view, so a closed window just makes them no-ops.
    pub(super) fn track(&mut self, task: Task<()>) {
        task.detach();
    }

    pub(super) fn notify_user(&mut self, text: impl Into<SharedString>, error: bool, cx: &mut Context<Self>) {
        self.notice = Some(Notice { text: text.into(), error, at: Instant::now() });
        cx.notify();
    }

    /// Runs a `gh auth ...` fix (from a hint) where the login in use lives.
    fn run_gh_fix(&mut self, args: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !args.starts_with("auth ") || args.chars().any(|c| ";&|`$<>\n'\"\\".contains(c)) {
            return;
        }
        match self.gh_login_machine() {
            Some(id) => self.gh_cli_run(&id, args, "GitHub access", window, cx),
            None => self.notify_user(format!("Run `gh {args}` in a terminal."), true, cx),
        }
    }

    /// Settings, with the field to watch an organization ready to type in.
    fn open_github_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_tab(super::settings::SettingsTab::GitHub, window, cx);
        if let Some(s) = &self.settings {
            s.read(cx).focus_add(window, cx);
        }
    }

    /// Settings, on one of its tabs.
    pub(super) fn open_settings_tab(&mut self, tab: super::settings::SettingsTab, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Settings, window, cx);
        if let Some(s) = &self.settings {
            s.update(cx, |s, cx| s.show_tab(tab, cx));
        }
    }

    pub(super) fn run_attention_action(&mut self, action: &Action, window: &mut Window, cx: &mut Context<Self>) {
        match action.kind.as_str() {
            "settings" if action.target.as_deref() == Some("github") => self.open_github_settings(window, cx),
            "settings" => self.navigate(Route::Settings, window, cx),
            "gh-sign-in" => window.dispatch_action(Box::new(SignInGitHub), cx),
            "gh-command" => {
                if let Some(args) = action.target.clone() {
                    self.run_gh_fix(&args, window, cx)
                }
            }
            "check" => self.engine.trigger_check(action.target.clone()),
            "machine" => {
                if let Some(t) = &action.target {
                    self.navigate(Route::Machine(t.clone()), window, cx)
                }
            }
            "runner" => {
                if let Some(t) = &action.target {
                    self.navigate(Route::Runner(t.clone()), window, cx)
                }
            }
            "queue" => self.navigate(Route::Jobs, window, cx),
            "link" => {
                if let Some(t) = &action.target {
                    cx.open_url(t)
                }
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Commands shared by pages

    pub(super) fn show_install_dialog(&mut self, machine_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.show_install_dialog_with(machine_id, &[], window, cx);
    }

    /// The Install dialog with extra labels filled in (for a job that needs them).
    pub(super) fn show_install_dialog_with(&mut self, machine_id: &str, labels: &[String], window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let snapshot = self.snapshot.clone();
        let machine_id = machine_id.to_string();
        let labels = labels.join(", ");
        let dialog = cx.new(|cx| {
            let d = InstallDialog::new(engine, snapshot, machine_id, window, cx);
            if !labels.is_empty() {
                d.set_labels(labels, cx);
            }
            d
        });
        self.show_modal(Modal::Install(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// The Copy files dialog. Without a start: from this computer to `machine`
    /// (or the other way round when `machine` is this computer).
    pub(super) fn show_copy_files(&mut self, machine: Option<String>, start: Option<super::files::CopyStart>, window: &mut Window, cx: &mut Context<Self>) {
        let s = &self.snapshot;
        let local = s.machines.iter().find(|m| m.local).map(|m| m.id.clone());
        let start = start.or_else(|| {
            let target = machine.clone().or_else(|| s.machines.iter().find(|m| !m.local && !m.host.is_empty()).map(|m| m.id.clone()))?;
            let (from, to) = match &local {
                Some(l) if *l == target => (l.clone(), s.machines.iter().find(|m| !m.local && !m.host.is_empty()).map(|m| m.id.clone()).unwrap_or_else(|| l.clone())),
                Some(l) => (l.clone(), target),
                None => (target.clone(), target),
            };
            Some(super::files::CopyStart { from, from_dir: "~".into(), selected: Vec::new(), to, to_dir: "~".into() })
        });
        let Some(start) = start else {
            self.notify_user("Add a machine first.", true, cx);
            return;
        };
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| super::files::CopyDialog::new(engine, start, window, cx));
        self.show_modal(Modal::Copy(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Files dropped from the Finder on a machine: copy them there.
    pub(super) fn copy_dropped(&mut self, machine_id: &str, paths: &[std::path::PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let Some(local) = self.snapshot.machines.iter().find(|m| m.local).map(|m| m.id.clone()) else {
            self.notify_user("Add this computer to Muxen (host \"local\") to copy files from it.", true, cx);
            return;
        };
        let Some(dir) = paths.first().and_then(|p| p.parent()).map(|p| p.display().to_string()) else { return };
        // Things from one folder at a time (the Finder drops them that way).
        let selected: Vec<String> = paths
            .iter()
            .filter(|p| p.parent().map(|d| d.display().to_string()) == Some(dir.clone()))
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();
        let start = super::files::CopyStart { from: local, from_dir: dir, selected, to: machine_id.to_string(), to_dir: "~".into() };
        self.show_copy_files(None, Some(start), window, cx);
    }

    /// Offers what was just copied on this computer to the machines sharing
    /// the clipboard (not a password manager's copies).
    fn watch_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.engine.is_demo() || !self.engine.settings().share_clipboard {
            return;
        }
        match super::pasteboard_state() {
            Some((count, private)) => {
                if count == self.clipboard_seen.0 {
                    return;
                }
                self.clipboard_seen.0 = count;
                if private {
                    return;
                }
            }
            // No change counter here: compare the text.
            None => {}
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        if text != self.clipboard_seen.1 {
            crate::fleet::clipboard::local_changed(&text);
            self.clipboard_seen.1 = text;
        }
    }

    /// Powers a cloud machine on or off.
    /// Connects to a machine again, or disconnects from it (its sessions go on there).
    pub(super) fn set_machine_connected(&mut self, machine_id: &str, connected: bool, cx: &mut Context<Self>) {
        if let Err(e) = self.engine.set_connected(machine_id, connected) {
            self.notify_user(e, true, cx);
        }
    }

    pub(super) fn cloud_power(&mut self, machine_id: &str, on: bool, cx: &mut Context<Self>) {
        match self.engine.cloud_power(machine_id, on) {
            Ok(op) => self.show_operation(op, cx),
            Err(e) => self.notify_user(e, true, cx),
        }
    }

    /// New cloud machine (a DigitalOcean droplet).
    pub(super) fn show_cloud_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| super::cloud_dialog::CloudDialog::new(engine, window, cx));
        self.show_modal(Modal::Cloud(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Asks for a new virtual monitor's name and size.
    pub(super) fn show_add_display(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| AddDisplayDialog::new(engine, machine, window, cx));
        self.show_modal(Modal::AddDisplay(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Shows one of a machine's displays (None: its default one) on the desktop page.
    pub(super) fn show_display(&mut self, machine: String, display: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.pin_desktop(&machine, cx);
        self.navigate(Route::Desktop(machine.clone()), window, cx);
        if let Some(view) = self.desktops.get(&machine).cloned() {
            view.update(cx, |v, cx| v.set_display(display, window, cx));
        }
        cx.notify();
    }

    /// Sets a monitor's size or scale, through its desktop view when one is
    /// open (it follows the new size).
    pub(super) fn set_display(&mut self, machine: String, display: String, size: Option<(u32, u32)>, scale: Option<f64>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.desktops.get(&machine).cloned() {
            view.update(cx, |v, cx| v.change_display(display, size, scale, window, cx));
            return;
        }
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.set_display(&machine, &display, size, scale).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            if let Ok(Err(e)) = job.await {
                let _ = this.update(cx, |this, cx| this.notify_user(e, true, cx));
            }
        });
        self.track(task);
    }

    /// Removes a virtual monitor; the desktop goes back to the default display.
    pub(super) fn remove_display(&mut self, machine: String, display: String, window: &mut Window, cx: &mut Context<Self>) {
        let (machine_for_dialog, display_for_dialog) = (machine.clone(), display.clone());
        if let Some(view) = self.desktops.get(&machine).cloned() {
            view.update(cx, |v, cx| v.set_display(None, window, cx));
        }
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.remove_display(&machine, &display).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| match job.await {
            // The machine asks for the password (sudo, on Linux).
            Ok(Err(crate::fleet::desktop::AddDisplayError::NeedsPassword(why))) => {
                let _ = this.update_in(cx, |this, window, cx| {
                    let engine = this.engine.clone();
                    let dialog = cx.new(|cx| MonitorPasswordDialog::new(engine, machine_for_dialog, display_for_dialog, Some(why), window, cx));
                    this.show_modal(Modal::MonitorPassword(dialog.clone()), window, cx);
                    this._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
                });
            }
            Ok(Err(e)) => {
                let _ = this.update(cx, |this, cx| this.notify_user(e.to_string(), true, cx));
            }
            _ => {}
        });
        self.track(task);
    }

    /// Asks for a machine's or a session's new name.
    pub(super) fn show_rename(&mut self, target: RenameTarget, window: &mut Window, cx: &mut Context<Self>) {
        let s = &self.snapshot;
        let machine_name = |id: &str| s.machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.to_string());
        let (current, machine) = match &target {
            RenameTarget::Machine(id) => (machine_name(id), machine_name(id)),
            RenameTarget::Session(id) => match s.sessions.iter().find(|x| &x.id == id) {
                Some(x) => (x.title.clone(), machine_name(&x.machine_id)),
                None => return,
            },
            RenameTarget::Display(machine, display) => {
                let name = s.machines.iter().find(|m| &m.id == machine).and_then(|m| m.displays.iter().find(|d| &d.id == display)).map(|d| d.name.clone());
                match name {
                    Some(n) => (n, machine_name(machine)),
                    None => return,
                }
            }
        };
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| RenameDialog::new(engine, target, current, machine, window, cx));
        self.show_modal(Modal::Rename(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    pub(super) fn show_add_machine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| AddMachineDialog::new(engine, window, cx));
        self.show_modal(Modal::AddMachine(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    pub(super) fn confirm_remove_runner(&mut self, runner: &Runner, window: &mut Window, cx: &mut Context<Self>) {
        let on_machine = runner.local.is_some();
        let detail = match (&runner.machine_id, on_machine) {
            (Some(_), true) => format!(
                "{} will be stopped, its service uninstalled, and it will be deregistered from GitHub and removed from config.toml.",
                runner.name
            ),
            _ => format!("{} will be deregistered from GitHub.", runner.name),
        };
        let runner_id = runner.id.clone();
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Remove {}?", runner.name),
                detail,
                "Remove runner",
                on_machine.then_some("Also delete the runner folder"),
                move |delete_files, engine| engine.runner_action(&runner_id, RunnerAction::Remove { delete_files }).map(Some),
                self.engine.clone(),
                window,
                cx,
            )
        });
        self.show_modal(Modal::Confirm(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    pub(super) fn confirm_remove_machine(&mut self, machine: &Machine, window: &mut Window, cx: &mut Context<Self>) {
        let machine_id = machine.id.clone();
        // A cloud machine goes with its droplet.
        if let Some(c) = &machine.cloud {
            let detail = format!(
                "Its droplet and everything on it are deleted, and it leaves Muxen. Push your work first. It has cost about ${:.2} so far.",
                c.cost_so_far
            );
            let dialog = cx.new(|cx| {
                ConfirmDialog::new(
                    format!("Destroy {}?", machine.name),
                    detail,
                    "Destroy",
                    None,
                    move |_, engine| engine.cloud_destroy(&machine_id).map(Some),
                    self.engine.clone(),
                    window,
                    cx,
                )
            });
            self.show_modal(Modal::Confirm(dialog.clone()), window, cx);
            self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
            return;
        }
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Remove {} from Muxen?", machine.name),
                "It's taken out of config.toml. Nothing on the machine changes, and its runners stay registered.".into(),
                "Remove machine",
                None,
                move |_, engine| engine.remove_machine(&machine_id).map(|_| None),
                self.engine.clone(),
                window,
                cx,
            )
        });
        self.show_modal(Modal::Confirm(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    pub(super) fn show_new_workspace(&mut self, machine: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| NewWorkspaceDialog::new(engine, machine, window, cx));
        self.show_modal(Modal::NewWorkspace(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Creates a workspace in the background; opens it when ready.
    fn create_workspace(&mut self, req: CreateWorkspace, iso: WorkspaceIsolation, window: &mut Window, cx: &mut Context<Self>) {
        // Its setup shows up on the Activity page as the newest operation.
        self.activity_filter = ActivityFilter::All;
        self.activity_selected = None;
        self.show(Route::Activity, cx);
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.create_workspace_with(req, iso).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(ws) => this.navigate(Route::Workspace(ws.id), window, cx),
                Err(e) => this.notify_user(e, true, cx),
            });
        });
        self.track(task);
    }

    /// Sets up a workspace (its clone shows on the Activity page), then starts
    /// the session in its folder and opens it.
    fn start_in_workspace(&mut self, req: CreateWorkspace, session: StartSession, window: &mut Window, cx: &mut Context<Self>) {
        self.activity_filter = ActivityFilter::All;
        self.activity_selected = None;
        self.show(Route::Activity, cx);
        let machine = self.snapshot.machines.iter().find(|m| m.id == session.machine_id).map(|m| m.name.clone()).unwrap_or_default();
        self.notify_user(format!("Setting up a workspace on {machine}; the session starts when it's ready."), false, cx);
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.create_workspace_with(req, WorkspaceIsolation::default()).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(ws) => this.start_session(StartSession { cwd: ws.path, ..session }, window, cx),
                Err(e) => this.notify_user(format!("Couldn't set up the workspace: {e}"), true, cx),
            });
        });
        self.track(task);
    }

    pub(super) fn confirm_destroy_workspace(&mut self, id: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let wid = id.to_string();
        // Sessions working in its folder end with it.
        let sessions: Vec<String> = match self.snapshot.workspaces.iter().find(|w| w.id == id) {
            Some(w) => self
                .snapshot
                .sessions
                .iter()
                .filter(|s| s.machine_id == w.machine_id && (s.cwd == w.path || s.cwd.starts_with(&format!("{}/", w.path))))
                .map(|s| s.id.clone())
                .collect(),
            None => Vec::new(),
        };
        let text = match sessions.len() {
            0 => "Background commands it started are stopped and its folder is deleted from the machine.".to_string(),
            1 => "The session working in it ends, background commands it started are stopped, and its folder is deleted from the machine.".to_string(),
            n => format!("The {n} sessions working in it end, background commands it started are stopped, and its folder is deleted from the machine."),
        };
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Delete {name}?"),
                text,
                "Delete workspace",
                Some("Keep the files on the machine"),
                move |keep_files, engine| {
                    let engine = engine.clone();
                    let (wid, sessions) = (wid.clone(), sessions.clone());
                    // Runs as an operation; progress and errors show in Activity.
                    engine.runtime().clone().spawn(async move {
                        for s in &sessions {
                            let _ = engine.control_session(s, crate::fleet::sessions::Control::Stop).await;
                        }
                        if let Err(e) = engine.destroy_workspace(&wid, keep_files).await {
                            tracing::warn!("destroying {wid}: {e}");
                        }
                    });
                    Ok(None)
                },
                self.engine.clone(),
                window,
                cx,
            )
        });
        self.show_modal(Modal::Confirm(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Asks before cancelling a job's workflow run (GitHub cancels whole runs).
    pub(super) fn confirm_cancel_run(&mut self, job_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let s = &self.snapshot;
        let Some(job) = s.jobs.queued.iter().chain(s.jobs.running.iter()).find(|j| j.id == job_id).cloned() else { return };
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Cancel this {} run?", job.workflow),
                format!("Every unfinished job in it stops, not just “{}”. You can re-run it on GitHub.", job.name),
                "Cancel run",
                None,
                move |_, engine| engine.cancel_run(job_id).map(Some),
                self.engine.clone(),
                window,
                cx,
            )
        });
        self.show_modal(Modal::Confirm(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    pub(super) fn runner_action(&mut self, runner_id: &str, action: RunnerAction, cx: &mut Context<Self>) {
        match self.engine.runner_action(runner_id, action) {
            Ok(op) => self.show_operation(op, cx),
            Err(e) => self.notify_user(e, true, cx),
        }
    }

    pub(super) fn show_modal(&mut self, modal: Modal, window: &mut Window, cx: &mut Context<Self>) {
        let focus = match &modal {
            Modal::AddMachine(d) => d.read(cx).first_focus(cx),
            Modal::Install(d) => d.read(cx).first_focus(cx),
            Modal::Confirm(d) => d.focus_handle(cx),
            Modal::NewWorkspace(d) => d.read(cx).first_focus(cx),
            Modal::NewSession(d) => d.read(cx).first_focus(cx),
            Modal::Copy(d) => d.read(cx).first_focus(cx),
            Modal::Cloud(d) => d.read(cx).first_focus(cx),
            Modal::Rename(d) => d.read(cx).first_focus(cx),
            Modal::AddDisplay(d) => d.read(cx).first_focus(cx),
            Modal::MonitorPassword(d) => d.read(cx).first_focus(cx),
            Modal::Licenses(d) => d.focus_handle(cx),
        };
        window.focus(&focus);
        self.modal = Some(modal);
        cx.notify();
    }

    pub(super) fn on_dialog_event<D>(&mut self, _: &Entity<D>, event: &DialogEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        match event {
            DialogEvent::Dismissed => {}
            DialogEvent::OperationStarted(op) => self.show_operation(*op, cx),
            DialogEvent::MachineAdded(id) => self.navigate(Route::Machine(id.clone()), window, cx),
            DialogEvent::OpenSettings => self.open_settings_tab(super::settings::SettingsTab::GitHub, window, cx),
            DialogEvent::OpenCloudSettings => self.open_settings_tab(super::settings::SettingsTab::Cloud, window, cx),
            DialogEvent::NewCloudMachine => self.show_cloud_dialog(window, cx),
            DialogEvent::CreateWorkspace(req, iso) => self.create_workspace(req.clone(), iso.clone(), window, cx),
            DialogEvent::StartInWorkspace(req, session) => self.start_in_workspace(req.clone(), session.clone(), window, cx),
            DialogEvent::DisplayAdded(machine, display) => self.show_display(machine.clone(), Some(display.clone()), window, cx),
            DialogEvent::StartSession(req) => {
                self.start_session(req.clone(), window, cx);
                cx.notify();
                return;
            }
        }
        window.focus(&self.focus);
        cx.notify();
    }

    pub(super) fn filter_query(&self, cx: &App) -> String {
        self.filter.read(cx).text().trim().to_lowercase()
    }

    fn first_filter_match(&self, cx: &App) -> Option<Route> {
        let q = self.filter_query(cx);
        if q.is_empty() {
            return None;
        }
        let s = &self.snapshot;
        s.machines
            .iter()
            .find(|m| m.name.to_lowercase().contains(&q))
            .map(|m| Route::Machine(m.id.clone()))
            .or_else(|| s.runners.iter().find(|r| r.name.to_lowercase().contains(&q)).map(|r| Route::Runner(r.id.clone())))
    }

    pub(super) fn selected_operation_id_in(&self, s: &Snapshot) -> Option<u64> {
        match self.selected_activity(s) {
            Some(ActivityItem::Op(id)) => Some(id),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Action handlers

    fn check_now(&mut self, _: &CheckNow, _: &mut Window, _: &mut Context<Self>) {
        self.engine.trigger_check(None);
    }

    fn sync_github(&mut self, _: &SyncGitHub, _: &mut Window, _: &mut Context<Self>) {
        self.engine.trigger_sync();
    }

    fn show_overview(&mut self, _: &ShowOverview, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Overview, window, cx);
    }

    fn show_jobs(&mut self, _: &ShowJobs, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Jobs, window, cx);
    }

    /// The machines in the sidebar's order (⌘1 is the first).
    /// Favorites first, then machines with runners, then the rest.
    pub(super) fn machine_order<'a>(s: &'a Snapshot, favorites: &[String]) -> Vec<&'a Machine> {
        let mut out: Vec<&Machine> = s.machines.iter().collect();
        out.sort_by_key(|m| (!favorites.contains(&m.id), m.kind != "runner"));
        out
    }

    pub(super) fn is_favorite(cx: &App, machine_id: &str) -> bool {
        cx.global::<super::theme::Appearance>().favorite_machines.iter().any(|f| f == machine_id)
    }

    /// Stars or unstars a machine.
    pub(super) fn set_favorite_machine(&mut self, machine_id: &str, favorite: bool, cx: &mut Context<Self>) {
        super::theme::remember(cx, |a| {
            a.favorite_machines.retain(|f| f != machine_id);
            if favorite {
                a.favorite_machines.push(machine_id.to_string());
            }
        });
        cx.notify();
    }

    /// ⌘1…⌘9: the n-th machine in the sidebar (1-based).
    fn go_to_machine(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        let favorites = cx.global::<super::theme::Appearance>().favorite_machines.clone();
        if let Some(id) = Self::machine_order(&self.snapshot, &favorites).get(n - 1).map(|m| m.id.clone()) {
            self.navigate(Route::Machine(id), window, cx);
        }
    }

    /// ⌘ or ⌃ went down or up: shortcuts show while one is held.
    fn modifiers_changed(&mut self, e: &gpui::ModifiersChangedEvent, _: &mut Window, cx: &mut Context<Self>) {
        // ⌘ (the machines and most shortcuts) or ⌃ (the pages).
        let held = e.modifiers.platform || e.modifiers.control;
        if !held {
            self.keys_soon = None;
            if self.show_keys {
                self.show_keys = false;
                cx.notify();
            }
        } else if !self.show_keys && self.keys_soon.is_none() {
            self.keys_soon = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(350)).await;
                let _ = this.update(cx, |this, cx| {
                    this.keys_soon = None;
                    this.show_keys = true;
                    cx.notify();
                });
            }));
        }
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Settings, window, cx);
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        // Not from under an open dialog.
        if self.modal.is_none() {
            window.focus(&self.filter.focus_handle(cx));
        }
    }

    fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prev) = self.back.pop() {
            let current = std::mem::replace(&mut self.route, prev);
            self.forward.push(current);
            self.on_route_changed(window, cx);
        }
    }

    fn go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.forward.pop() {
            let current = std::mem::replace(&mut self.route, next);
            self.back.push(current);
            self.on_route_changed(window, cx);
        }
    }

    fn toggle_activity(&mut self, _: &ToggleActivity, window: &mut Window, cx: &mut Context<Self>) {
        if self.route == Route::Activity {
            self.go_back(&GoBack, window, cx);
        } else {
            self.show(Route::Activity, cx);
        }
    }

    fn add_machine(&mut self, _: &AddMachine, window: &mut Window, cx: &mut Context<Self>) {
        self.show_add_machine(window, cx);
    }

    /// ⌘T: a new session on the machine being looked at, if any.
    fn new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        let machine = match &self.route {
            Route::Machine(id) => Some(id.clone()),
            Route::Session(id) => id.split('/').next().map(str::to_string),
            _ => None,
        };
        self.show_new_session(machine, window, cx);
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        if self.context_menu.take().is_some() {
            cx.notify();
            return;
        }
        if self.modal.take().is_some() {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Sidebar

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let q = self.filter_query(cx);
        let matches_runner = |r: &Runner| q.is_empty() || r.name.to_lowercase().contains(&q);
        let selected_machine = match &self.route {
            Route::Machine(id) => Some(id.clone()),
            _ => None,
        };
        let selected_runner = match &self.route {
            Route::Runner(id) => Some(id.clone()),
            _ => None,
        };
        let selected_session = match &self.route {
            Route::Session(id) => Some(id.clone()),
            _ => None,
        };
        let running_ops = s.operations.iter().filter(|o| o.status == "running").count();
        let mac_titlebar = cfg!(target_os = "macos") && !window.is_fullscreen();

        let mut rows: Vec<AnyElement> = Vec::new();
        // Machines with runners first, then the rest; all under one heading.
        let favorites = cx.global::<super::theme::Appearance>().favorite_machines.clone();
        let runner_machines: Vec<&Machine> = Self::machine_order(&s, &favorites);
        let unassigned: Vec<&Runner> = s.runners.iter().filter(|r| r.machine_id.is_none() && matches_runner(r)).collect();

        rows.push(self.section_header("Machines", runner_machines.len(), true, cx).into_any_element());
        let pinned_desktops = cx.global::<super::theme::Appearance>().desktops.clone();
        let forwards = self.engine.forwards();
        for (n, m) in runner_machines.iter().enumerate() {
            // While ⌘ is held: the shortcut that goes to it.
            let hint = if self.show_keys { super::shortcuts::machine_keys(cx, n + 1).map(|k| super::shortcuts::label(&k)) } else { None };
            let runners: Vec<&Runner> = m.runner_ids.iter().filter_map(|id| s.runners.iter().find(|r| &r.id == id)).collect();
            let machine_match = q.is_empty() || m.name.to_lowercase().contains(&q);
            let visible: Vec<&Runner> = runners.iter().copied().filter(|r| machine_match || matches_runner(r)).collect();
            let sessions: Vec<&crate::fleet::sessions::SessionInfo> = s
                .sessions
                .iter()
                .filter(|x| x.machine_id == m.id && (machine_match || x.title.to_lowercase().contains(&q) || x.agent_name.to_lowercase().contains(&q)))
                .collect();
            if !machine_match && visible.is_empty() && sessions.is_empty() {
                continue;
            }
            let open = !self.collapsed.contains(&m.id) || !q.is_empty();
            // Collapsed: the machine carries its sessions' unread dot.
            let alert = (!open).then(|| sessions.iter().find_map(|x| x.attention.clone().map(|k| (k, x.exit_code)))).flatten();
            let has_desktop = pinned_desktops.contains(&m.id);
            let has_forwards = forwards.iter().any(|f| f.machine_id == m.id);
            rows.push(self.machine_row(&theme, m, open, !sessions.is_empty() || has_desktop || has_forwards || self.logs.contains_key(&m.id) || self.service_pages.contains_key(&m.id), alert, selected_machine.as_deref() == Some(&m.id), hint, cx).into_any_element());
            if open {
                // Agent sessions first: they're what you come back to.
                for x in sessions {
                    rows.push(self.session_row(&theme, x, selected_session.as_deref() == Some(&x.id), cx).into_any_element());
                }
                // Which of its displays the desktop page shows, when it's on screen.
                let showing = (self.route == Route::Desktop(m.id.clone())).then(|| self.desktops.get(&m.id).map(|v| v.read(cx).display().map(str::to_string))).flatten();
                let on_virtual = showing.as_ref().is_some_and(|d| d.as_ref().is_some_and(|d| m.displays.iter().any(|x| &x.id == d && x.is_virtual)));
                // The open desktop: its main screen, then the virtual monitors
                // (closed together).
                if has_desktop {
                    rows.push(self.desktop_row(&theme, m, showing.is_some() && !on_virtual, cx).into_any_element());
                    for d in m.displays.iter().filter(|d| d.is_virtual) {
                        let selected = showing.as_ref().is_some_and(|s| s.as_deref() == Some(d.id.as_str()));
                        rows.push(self.monitor_row(&theme, m, d, selected, cx).into_any_element());
                    }
                }
                if self.logs.contains_key(&m.id) {
                    let shown = self.logs.get(&m.id).and_then(|v| v.read(cx).source_name().map(str::to_string));
                    rows.push(self.tool_row(&theme, m, Route::Logs(m.id.clone()), "scroll-text", "Logs", shown, cx).into_any_element());
                }
                if self.service_pages.contains_key(&m.id) {
                    rows.push(self.tool_row(&theme, m, Route::Services(m.id.clone()), "layers", "Services", None, cx).into_any_element());
                }
                // Its ports open on this computer.
                for f in forwards.iter().filter(|f| f.machine_id == m.id) {
                    rows.push(self.forward_row(&theme, m, f, cx));
                }
                // Runners as one line; a search shows the ones it found.
                if !machine_match {
                    for r in visible {
                        rows.push(self.runner_row(&theme, &s, r, selected_runner.as_deref() == Some(&r.id), cx).into_any_element());
                    }
                } else if !runners.is_empty() {
                    let here = selected_runner.as_deref().is_some_and(|r| r.starts_with(&format!("{}/", m.id)));
                    rows.push(self.runners_row(&theme, m, here, cx).into_any_element());
                }
            }
        }
        if !unassigned.is_empty() {
            rows.push(self.section_header("On GitHub only", unassigned.len(), false, cx).into_any_element());
            for r in unassigned {
                rows.push(self.runner_row(&theme, &s, r, selected_runner.as_deref() == Some(&r.id), cx).into_any_element());
            }
        }
        if s.machines.is_empty() {
            rows.push(
                div()
                    .mx(px(18.))
                    .my(px(8.))
                    .text_size(px(12.5))
                    .text_color(theme.text_placeholder)
                    .child("No machines yet. Add one to start watching its runners.")
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(self.sidebar_width))
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border_variant)
            // Room for the macOS traffic lights; the window drags from here.
            .when(mac_titlebar, |d| {
                d.child(
                    div()
                        .id("sidebar-drag")
                        .h(px(40.))
                        .flex_none()
                        .on_mouse_down(gpui::MouseButton::Left, |e: &gpui::MouseDownEvent, window, _| super::title_bar_press(e, window)),
                )
            })
            .child(self.brand(&theme, &s, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(10.))
                    .pt(px(6.))
                    .child(self.nav_item("nav-overview", "gauge", kit::tiles::OVERVIEW, "Overview", None, Route::Overview, "overview", cx))
                    .child(self.nav_item("nav-jobs", "workflow", kit::tiles::JOBS, "Jobs", Some(s.counts.queued + s.counts.running), Route::Jobs, "jobs", cx))
                    .child(self.nav_item("nav-workspaces", "terminal", kit::tiles::WORKSPACES, "Workspaces", Some(s.workspaces.len()), Route::Workspaces, "workspaces", cx))
                    .child(self.nav_item("nav-github", "agent-github", super::github::TILE, "GitHub", self.gh.waiting(), Route::Github, "github", cx))
                    .child(self.activity_nav(&theme, running_ops, cx)),
            )
            .child(
                div().px(px(12.)).pt(px(12.)).pb(px(4.)).child(
                    div()
                        .relative()
                        .child(self.filter.clone())
                        .child(div().absolute().right(px(7.)).top(px(6.)).child(kit::kbd(&theme, "/"))),
                ),
            )
            .child(div().id("machine-list").flex_1().min_h(px(0.)).overflow_y_scroll().pb(px(12.)).children(rows))
            // The "what's happening now" card is hidden for now (connection_card).
            .child(self.sidebar_footer(&theme, cx))
    }

    fn brand(&self, theme: &Theme, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        // GPUI 0.2 only ellipsizes text whose width is known before layout.
        let text_w = self.sidebar_width - 74. - if s.demo { 48. } else { 0. };
        div()
            .id("brand")
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(16.))
            .pt(px(if cfg!(target_os = "macos") { 2. } else { 14. }))
            .pb(px(8.))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| this.navigate(Route::About, window, cx)))
            .child(gpui::img("brand/icon.png").size(px(32.)).flex_none())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.))
                    .child(div().w(px(text_w)).truncate().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Muxen")),
            )
            .when(s.demo, |d| d.child(div().flex_1()).child(kit::pill("demo", theme.warning)))
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn nav_item(
        &self,
        id: &'static str,
        icon_name: &'static str,
        colors: (u32, u32),
        text: &'static str,
        count: Option<usize>,
        route: Route,
        shortcut: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        // While ⌘ is held the count gives way to the shortcut.
        let keys = self.show_keys.then(|| super::shortcuts::label_for(cx, shortcut)).filter(|k| !k.is_empty());
        let count = count.filter(|_| keys.is_none());
        // A workspace page keeps "Workspaces" highlighted.
        let active = self.route == route
            || (route == Route::Workspaces && matches!(self.route, Route::Workspace(_)))
            || (route == Route::Github && matches!(self.route, Route::GithubRepo(_) | Route::GithubItem(..)));
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(30.))
            .px(px(10.))
            .rounded(px(8.))
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(if active { theme.text } else { theme.text_muted })
            .when(active, |d| d.bg(theme.element_selected))
            .when(!active, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::tile(icon_name, colors.0, colors.1, 22.))
            .child(text)
            .child(div().flex_1())
            .children(count.filter(|n| *n > 0).map(|n| {
                div()
                    .px(px(6.))
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .bg(theme.element_hover)
                    .font_family(mono_font())
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(n.to_string())
            }))
            .children(keys.map(|k| kit::kbd(&theme, k)))
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(route.clone(), window, cx)))
    }

    fn activity_nav(&self, theme: &Theme, running: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.route == Route::Activity;
        div()
            .id("nav-activity")
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(30.))
            .px(px(10.))
            .rounded(px(8.))
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(if open { theme.text } else { theme.text_muted })
            .when(open, |d| d.bg(theme.element_selected))
            .when(!open, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::tile("activity", kit::tiles::ACTIVITY.0, kit::tiles::ACTIVITY.1, 22.))
            .child("Activity")
            .child(div().flex_1())
            .when(running > 0, |d| d.child(spinner("nav-activity-spin", 13., theme.accent)))
            .when(running == 0 && self.show_keys, |d| {
                let keys = super::shortcuts::label_for(cx, "activity");
                d.when(!keys.is_empty(), |d| d.child(kit::kbd(theme, keys)))
            })
            .on_click(cx.listener(|this, _, _, cx| this.show(Route::Activity, cx)))
    }

    fn section_header(&self, title: &'static str, count: usize, with_add: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .mt(px(10.))
            .pl(px(20.))
            .pr(px(12.))
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_placeholder)
            .child(title)
            .child(div().font_family(mono_font()).child(count.to_string()))
            .child(div().flex_1())
            .when(with_add, |d| {
                d.child(
                    Button::icon_only("sidebar-add-machine", "plus")
                        .tooltip(tip("Add machine", &super::shortcuts::label_for(cx, "add-machine")))
                        .on_click(cx.listener(|this, _, window, cx| this.show_add_machine(window, cx))),
                )
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn machine_row(
        &self,
        theme: &Theme,
        m: &Machine,
        open: bool,
        has_sessions: bool,
        alert: Option<(String, Option<i32>)>,
        selected: bool,
        hint: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = m.id.clone();
        let toggle_id = m.id.clone();
        // The shortcut takes the place of the toggle.
        let has_children = hint.is_none() && (has_sessions || !m.runner_ids.is_empty());
        let (tile_from, tile_to) = format::machine_tile(m);
        let (subtitle, tone) = format::machine_subtitle(m);
        // The text column gets an explicit width: GPUI 0.2 only ellipsizes text
        // whose width is known before layout. Row chrome: margins, padding, tile, gaps.
        let favorite = Self::is_favorite(cx, &m.id);
        let trailing = if hint.is_some() { 40. } else if has_children { 32. } else { 0. }
            + if alert.is_some() { 18. } else { 0. }
            + if favorite { 22. } else { 0. };
        let text_w = (self.sidebar_width - 20. - 16. - 30. - 10. - trailing).max(40.);
        div()
            .id(SharedString::from(format!("side-m-{}", m.id)))
            .group("machine-row")
            // Files from the Finder dropped here are copied to the machine.
            .when(!m.host.is_empty() && !m.platform.starts_with("windows"), |d| {
                let (mid, accent) = (m.id.clone(), theme.accent);
                d.drag_over::<gpui::ExternalPaths>(move |st, _, _, _| st.bg(accent.opacity(0.18)))
                    .on_drop(cx.listener(move |this, paths: &gpui::ExternalPaths, window, cx| this.copy_dropped(&mid, paths.paths(), window, cx)))
            })
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(44.))
            .mx(px(10.))
            .px(px(8.))
            .rounded(px(9.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            // OS tile with a status badge in the corner.
            .child(
                div().relative().flex_none().child(kit::tile(format::machine_icon(m), tile_from, tile_to, 30.)).child(
                    div()
                        .absolute()
                        .right(px(-4.))
                        .bottom(px(-4.))
                        .size(px(13.))
                        .rounded_full()
                        .bg(theme.sidebar)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(if m.checking {
                            spinner(SharedString::from(format!("side-spin-{}", m.id)), 10., theme.text_muted).into_any_element()
                        } else {
                            kit::dot(theme.machine_status(m), 8.).into_any_element()
                        }),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .gap(px(1.))
                    .child(
                        div()
                            .w(px(text_w))
                            .truncate()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(m.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(text_w))
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(if tone == "error" { theme.error } else { theme.text_placeholder })
                            .child(subtitle),
                    ),
            )
            .children(alert.map(|(kind, code)| super::sessions::attention_dot(theme, &kind, code)))
            .when(favorite, |d| d.child(icon("star-filled", 12., theme.warning)))
            .children(hint.map(|k| kit::kbd(theme, k)))
            .when(has_children, |d| {
                d.child(
                    div()
                        .relative()
                        .flex_none()
                        .min_w(px(22.))
                        .h(px(18.))
                        .flex()
                        .justify_end()
                        // The expand/collapse toggle, on hover.
                        .child(
                            div()
                                .id(SharedString::from(format!("side-t-{}", m.id)))
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_end()
                                .pr(px(4.))
                                .invisible()
                                .group_hover("machine-row", |st| st.visible())
                                .child(icon(if open { "chevron-down" } else { "chevron-right" }, 12., theme.text_muted))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if !this.collapsed.remove(&toggle_id) {
                                        this.collapsed.insert(toggle_id.clone());
                                    }
                                    cx.notify()
                                })),
                        ),
                )
            })
            .on_mouse_down(MouseButton::Right, {
                let mid = m.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Machine(mid.clone()), cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(id.clone()), window, cx)))
    }

    /// A machine's runners as one line ("3 runners · 2 busy"); opens the machine.
    fn runners_row(&self, theme: &Theme, m: &Machine, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = m.id.clone();
        let sum = &m.summary;
        let label = if sum.total == 1 { "1 runner".to_string() } else { format!("{} runners", sum.total) };
        let (state, color) = if sum.working > 0 {
            (format!("{} busy", sum.working), theme.text_accent)
        } else if sum.offline > 0 {
            (format!("{} stopped", sum.offline), theme.text_placeholder)
        } else {
            ("idle".to_string(), theme.text_placeholder)
        };
        let (from, to) = kit::tiles::JOBS;
        div()
            .id(SharedString::from(format!("side-rs-{}", m.id)))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .mx(px(10.))
            .pl(px(22.))
            .pr(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::tile("workflow", from, to, 16.))
            .child(div().flex_1().text_size(px(12.5)).text_color(if selected { theme.text } else { theme.text_muted }).child(label))
            .child(div().flex_none().text_size(px(11.)).text_color(color).child(state))
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(id.clone()), window, cx)))
    }

    fn runner_row(&self, theme: &Theme, s: &Snapshot, r: &Runner, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = r.id.clone();
        let job = r.job_id.and_then(|j| s.jobs.running.iter().find(|x| x.id == j));
        let right = match (r.state.as_str(), job) {
            ("working", Some(j)) => ellipsize(&j.name, (14. + (self.sidebar_width - SIDEBAR_DEFAULT) / 7.).max(6.) as usize),
            ("working", None) => "working".into(),
            ("offline", _) => "offline".into(),
            ("unknown", _) => "unknown".into(),
            _ => String::new(),
        };
        let dim = matches!(r.state.as_str(), "offline" | "unknown");
        div()
            .id(SharedString::from(format!("side-r-{}", r.id)))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .mx(px(10.))
            .pl(px(22.))
            .pr(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(div().flex_none().w(px(3.)).h(px(12.)).rounded_full().bg(if dim { theme.track } else { theme.state(&r.state) }))
            // The runner name wins; the job name takes whatever room is left.
            .child(
                div()
                    .flex_none()
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .text_color(if dim { theme.text_placeholder } else { theme.text_muted })
                    .child(r.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .justify_end()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(if r.state == "working" { theme.text_accent } else { theme.text_placeholder })
                    .child(right),
            )
            .on_mouse_down(MouseButton::Right, {
                let rid = r.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Runner(rid.clone()), cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Runner(id.clone()), window, cx)))
    }

    /// A thin grip on the sidebar's edge: drag to resize, double-click to reset.
    fn sidebar_handle(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let resizing = self.sidebar_resizing;
        let hover = theme.accent.opacity(0.5);
        div()
            .id("sidebar-resize")
            .group("sidebar-resize")
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(self.sidebar_width - 4.))
            .w(px(6.))
            .flex()
            .justify_center()
            .cursor_col_resize()
            .child(
                div()
                    .w(px(2.))
                    .h_full()
                    .when(resizing, |d| d.bg(theme.accent))
                    .when(!resizing, |d| d.group_hover("sidebar-resize", |st| st.bg(hover))),
            )
            .on_drag(DraggedSidebar, |_, _, _, cx| cx.new(|_| DraggedSidebar))
            .on_click(cx.listener(|this, e: &ClickEvent, _, cx| {
                if e.click_count() == 2 {
                    this.sidebar_width = SIDEBAR_DEFAULT;
                    this.finish_sidebar_resize(cx);
                    cx.notify();
                }
            }))
    }

    fn resize_sidebar(&mut self, e: &DragMoveEvent<DraggedSidebar>, window: &mut Window, cx: &mut Context<Self>) {
        // A release GPUI never saw (e.g. outside the window) must not leave the
        // sidebar following a bare mouse.
        if e.event.pressed_button != Some(gpui::MouseButton::Left) {
            cx.stop_active_drag(window);
            self.finish_sidebar_resize(cx);
            cx.notify();
            return;
        }
        let width = f32::from(e.event.position.x).round().clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.sidebar_resizing = true;
        if width != self.sidebar_width {
            self.sidebar_width = width;
            cx.notify();
        }
    }

    /// Follows the divider between a terminal and a desktop.
    pub(super) fn resize_split(&mut self, e: &DragMoveEvent<DraggedSplit>, window: &mut Window, cx: &mut Context<Self>) {
        if e.event.pressed_button != Some(gpui::MouseButton::Left) {
            cx.stop_active_drag(window);
            self.finish_split_resize(cx);
            return;
        }
        let (x, left, width) = (f32::from(e.event.position.x), f32::from(e.bounds.origin.x), f32::from(e.bounds.size.width));
        if width > 0. {
            self.split_ratio = ((x - left) / width).clamp(0.2, 0.8);
            self.split_resizing = true;
            cx.notify();
        }
    }

    /// Ends a divider drag and remembers where it was left.
    pub(super) fn finish_split_resize(&mut self, cx: &mut Context<Self>) {
        self.split_resizing = false;
        let ratio = self.split_ratio;
        super::theme::remember(cx, |a| a.split_ratio = ratio);
        cx.notify();
    }

    /// Ends a resize and remembers the width for next launch.
    fn finish_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        self.sidebar_resizing = false;
        super::theme::save_sidebar_width(cx, self.sidebar_width);
    }

    /// What's happening now, in a line: a copy or install under way, else an
    /// agent that needs you or is working, else a GitHub problem, else the
    /// machines. Clicking opens it (or checks the machines). Hidden for now.
    #[allow(dead_code)]
    fn connection_card(&self, theme: &Theme, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        enum Click {
            Op(u64),
            Session(String),
            Settings,
            Check,
        }
        let machine_name = |id: &str| s.machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.to_string());
        let who = |x: &crate::fleet::sessions::SessionInfo| if x.agent_name.is_empty() { "A session".to_string() } else { x.agent_name.clone() };
        let running: Vec<&crate::fleet::model::Operation> = s.operations.iter().filter(|o| o.status == "running" && o.kind != "logs").collect();
        let needs_you = s.sessions.iter().find(|x| matches!(x.attention.as_deref(), Some("bell" | "waiting")));
        let working: Vec<&crate::fleet::sessions::SessionInfo> = s.sessions.iter().filter(|x| x.busy && x.status == "running").collect();
        let github_trouble = s.github.configured && (!s.github.live || s.github.scopes.iter().any(|sc| sc.error.is_some()));
        let mut progress = None;
        let (color, title, detail, click) = if let Some(op) = running.first() {
            progress = op.progress.clone();
            let detail = match (&op.progress, running.len()) {
                (Some(p), 1) => super::activity::progress_text(p),
                (Some(p), n) => format!("{} · {} more", super::activity::progress_text(p), n - 1),
                (None, 1) => "Running…".to_string(),
                (None, n) => format!("{} more running", n - 1),
            };
            (theme.accent, op.title.clone(), detail, Click::Op(op.id))
        } else if let Some(x) = needs_you {
            (theme.warning, format!("{} on {} needs you", who(x), machine_name(&x.machine_id)), x.title.clone(), Click::Session(x.id.clone()))
        } else if let Some(x) = working.first() {
            let title = match working.len() {
                1 => format!("{} is working on {}", who(x), machine_name(&x.machine_id)),
                n => format!("{n} agents are working"),
            };
            (theme.accent, title, x.title.clone(), Click::Session(x.id.clone()))
        } else if github_trouble {
            let (color, title) = github_status(theme, s);
            (color, title, "Runner status from GitHub".to_string(), Click::Settings)
        } else {
            let total = s.machines.iter().filter(|m| !m.host.is_empty()).count();
            let offline = s.machines.iter().filter(|m| m.status == "offline").count();
            // Off or asleep, versus on but needing something (a key, Remote Login, ...).
            let off = s.machines.iter().filter(|m| format::is_just_offline(m)).count();
            let stuck = offline - off;
            let title = match (total, offline) {
                (0, _) => "No machines yet".to_string(),
                (1, 0) => "Your machine is online".to_string(),
                (n, 0) => format!("All {n} machines online"),
                (n, _) => {
                    let mut parts = vec![format!("{} online", n - offline)];
                    if off > 0 {
                        parts.push(format!("{off} offline"));
                    }
                    if stuck > 0 {
                        parts.push(format!("{stuck} can't connect"));
                    }
                    parts.join(" · ")
                }
            };
            let detail = if s.check.running {
                "Refreshing…".to_string()
            } else {
                match s.check.finished_at {
                    Some(t) => format!("Updated {}", format::ago(Some(t))),
                    None => "Not updated yet".to_string(),
                }
            };
            let color = if offline > 0 { theme.warning } else if total > 0 { theme.success } else { theme.text_placeholder };
            (color, title, detail, Click::Check)
        };
        // Outer and inner padding, border, dot and gap.
        let text_w = self.sidebar_width - 60.;
        div().flex_none().p(px(10.)).border_t_1().border_color(theme.border_variant).child(
            div()
                .id("connection")
                .flex()
                .items_start()
                .gap(px(10.))
                .px(px(10.))
                .py(px(9.))
                .rounded(px(10.))
                .border_1()
                .border_color(theme.border_variant)
                .bg(theme.surface)
                .cursor_pointer()
                .hover(|st| st.border_color(theme.border))
                .child(div().pt(px(4.)).child(kit::dot(color, 8.)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(div().w(px(text_w)).truncate().text_size(px(12.5)).text_color(theme.text).child(title))
                        .child(div().w(px(text_w)).truncate().text_size(px(11.)).text_color(theme.text_placeholder).child(detail))
                        .children(progress.map(|p| div().w(px(text_w)).pt(px(3.)).child(super::activity::progress_bar(theme, &p)))),
                )
                .on_click(cx.listener(move |this, _, window, cx| match &click {
                    Click::Op(id) => this.show_operation(*id, cx),
                    Click::Session(id) => this.navigate(Route::Session(id.clone()), window, cx),
                    Click::Settings => this.navigate(Route::Settings, window, cx),
                    Click::Check => this.engine.trigger_check(None),
                })),
        )
    }

    /// Settings and About, with the version, at the bottom of the sidebar.
    fn sidebar_footer(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let footer_button = |id: &'static str, icon_name: &'static str, label: &'static str, route: Route, cx: &mut Context<Self>| {
            let active = self.route == route;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .px(px(8.))
                .rounded(px(7.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(if active { theme.text } else { theme.text_muted })
                .when(active, |d| d.bg(theme.element_selected))
                .when(!active, |d| d.hover(|st| st.bg(theme.ghost_hover).text_color(theme.text)))
                .child(kit::color_icon(theme, icon_name, 13., theme.text_placeholder))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| this.navigate(route.clone(), window, cx)))
        };
        let footer = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .pt(px(8.))
            .child(footer_button("footer-settings", "settings", "Settings", Route::Settings, cx))
            .child(footer_button("footer-about", "info", "About", Route::About, cx))
            .child(div().flex_1())
            .child(div().pr(px(6.)).font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(format!("v{}", env!("CARGO_PKG_VERSION"))));
        div().flex_none().px(px(10.)).pb(px(10.)).border_t_1().border_color(theme.border_variant).child(footer)
    }

    // -----------------------------------------------------------------------
    // Toolbar and overlays

    /// Hides or shows the sidebar, and remembers it.
    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_hidden = !self.sidebar_hidden;
        let hidden = self.sidebar_hidden;
        super::theme::remember(cx, |a| a.sidebar_hidden = hidden);
        cx.notify();
    }

    fn render_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let (section, title) = route_title(&s, &self.route);
        // Without the sidebar, the macOS window buttons sit on this bar.
        let traffic_lights = self.sidebar_hidden && cfg!(target_os = "macos") && !window.is_fullscreen();
        div()
            .id("toolbar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .h(px(52.))
            .pl(px(if traffic_lights { 86. } else { 16. }))
            .pr(px(16.))
            .border_b_1()
            .border_color(theme.border_variant)
            // The whole bar moves the window (except its buttons), like a title bar.
            .on_mouse_down(gpui::MouseButton::Left, |e: &gpui::MouseDownEvent, window, _| super::title_bar_press(e, window))
            .child(
                Button::icon_only("toggle-sidebar", "panel-left")
                    .tooltip(tip(if self.sidebar_hidden { "Show sidebar" } else { "Hide sidebar" }, &super::shortcuts::label_for(cx, "sidebar")))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .child(
                        Button::icon_only("back", "arrow-left")
                            .tooltip(tip("Back", &super::shortcuts::label_for(cx, "back")))
                            .disabled(self.back.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| this.go_back(&GoBack, window, cx))),
                    )
                    .child(
                        Button::icon_only("forward", "arrow-right")
                            .tooltip(tip("Forward", &super::shortcuts::label_for(cx, "forward")))
                            .disabled(self.forward.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| this.go_forward(&GoForward, window, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .min_w(px(0.))
                    .text_size(px(13.5))
                    .when_some(section, |d, sec| {
                        d.child(div().text_color(theme.text_placeholder).child(sec)).child(div().text_color(theme.text_placeholder).child("›"))
                    })
                    .child(div().truncate().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title)),
            )
            .child(div().flex_1())
            // GitHub and the machine checks run by themselves; ⇧⌘R, ⌘R and the menu still force them.
            .child(
                Button::new("toolbar-add-machine", "Add machine")
                    .style(ButtonStyle::Primary)
                    .icon("plus")
                    .map(|b| match super::shortcuts::label_for(cx, "add-machine") {
                        k if self.show_keys && !k.is_empty() => b.key_hint(k),
                        _ => b,
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.show_add_machine(window, cx))),
            )

    }

    fn render_notice(&self, theme: &Theme) -> Option<AnyElement> {
        let n = self.notice.as_ref()?;
        let color = if n.error { theme.error } else { theme.success };
        Some(
            div()
                .absolute()
                .bottom(px(18.))
                .right(px(18.))
                .child(
                    div()
                        .w(px(400.))
                        .flex()
                        .items_start()
                        .gap(px(10.))
                        .px(px(14.))
                        .py(px(11.))
                        .rounded(px(10.))
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .text_size(px(12.5))
                        .text_color(theme.text)
                        .child(div().flex_none().w(px(3.)).h(px(16.)).rounded_full().bg(color))
                        .child(div().flex_1().min_w(px(0.)).whitespace_normal().child(n.text.clone())),
                )
                .into_any_element(),
        )
    }

    /// A machine's desktop: its toolbar (status, network, resolution, open in
    /// a window, close), then the screen.
    fn render_desktop(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.desktops.contains_key(id) {
            self.on_route_changed(window, cx);
        }
        let Some(view) = self.desktops.get(id).cloned() else { return div().into_any_element() };
        let (pop, close) = (id.to_string(), id.to_string());
        let machine = self.snapshot.machines.iter().find(|m| m.id == id);
        let shown = view.read(cx).display().map(str::to_string);
        let virtual_shown = shown.clone().filter(|d| machine.is_some_and(|m| m.displays.iter().any(|x| &x.id == d && x.is_virtual)));
        let can_add = machine.is_some() && !self.snapshot.demo;
        let mut extra = Vec::new();
        if let Some(display) = virtual_shown {
            let mid = id.to_string();
            extra.push(
                Button::new("desktop-remove-monitor", "Remove monitor")
                    .style(ButtonStyle::Ghost)
                    .icon("trash-2")
                    .on_click(cx.listener(move |this, _, window, cx| this.remove_display(mid.clone(), display.clone(), window, cx)))
                    .into_any_element(),
            );
        } else if can_add {
            let mid = id.to_string();
            extra.push(
                Button::new("desktop-add-monitor", "Add monitor")
                    .style(ButtonStyle::Ghost)
                    .icon("plus")
                    .on_click(cx.listener(move |this, _, window, cx| this.show_add_display(mid.clone(), window, cx)))
                    .into_any_element(),
            );
        }
        extra.extend(vec![
            Button::new("desktop-window", "Open in window")
                .style(ButtonStyle::Ghost)
                .icon("external-link")
                .on_click(cx.listener(move |this, _, window, cx| this.pop_out_desktop(&pop, window, cx)))
                .into_any_element(),
            Button::new("desktop-close", "Close")
                .style(ButtonStyle::Ghost)
                .icon("x")
                .on_click(cx.listener(move |this, _, window, cx| this.close_desktop(&close, window, cx)))
                .into_any_element(),
        ]);
        let root = cx.entity().downgrade();
        let menu: super::desktop::DisplayMenu = std::rc::Rc::new(move |machine: &str, display: &str, at, _window, cx| {
            let target = super::menu::MenuTarget::Display { machine: machine.to_string(), display: display.to_string() };
            let _ = root.update(cx, |this, cx| this.open_menu(target, at, cx));
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(super::desktop::toolbar_with_menu(&view, extra, Some(menu), cx))
            .child(div().flex_1().min_h(px(0.)).child(view))
            .into_any_element()
    }

    /// Lists a machine's desktop in the sidebar, under the machine.
    fn pin_desktop(&mut self, machine_id: &str, cx: &mut Context<Self>) {
        if !cx.global::<super::theme::Appearance>().desktops.iter().any(|d| d == machine_id) {
            let id = machine_id.to_string();
            super::theme::remember(cx, |a| a.desktops.push(id));
        }
    }

    /// Closes a desktop: out of the sidebar, its stream stopped.
    pub(super) fn close_desktop(&mut self, machine_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let id = machine_id.to_string();
        super::theme::remember(cx, |a| a.desktops.retain(|d| *d != id));
        self.desktops.remove(machine_id);
        if self.route == Route::Desktop(machine_id.to_string()) {
            self.navigate(Route::Machine(machine_id.to_string()), window, cx);
        } else {
            cx.notify();
        }
    }

    /// The desktop beside a session's terminal becomes its own item in the
    /// sidebar (the same stream: nothing reconnects).
    pub(super) fn desktop_to_sidebar(&mut self, session_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let machine = session_id.split('/').next().unwrap_or_default().to_string();
        self.pin_desktop(&machine, cx);
        if let Some(key) = self.page_split(session_id) {
            self.split.remove(&key);
        }
        self.navigate(Route::Desktop(machine), window, cx);
    }

    /// A desktop's item in the sidebar, under its machine.
    fn desktop_row(&self, theme: &Theme, m: &Machine, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let (open, close) = (m.id.clone(), m.id.clone());
        let (from, to) = format::machine_tile(m);
        // Its main monitor: the size, and its menu on right-click (no Remove: it's real).
        let main = m.displays.iter().find(|d| d.main && d.id != "all").cloned();
        let size = main.as_ref().map(|d| format!("{}×{}", d.width as u32, d.height as u32));
        let right_click = main.map(|d| {
            let (machine, display) = (m.id.clone(), d.id);
            super::menu::on_right_click(move || super::menu::MenuTarget::Display { machine: machine.clone(), display: display.clone() }, cx)
        });
        div()
            .id(SharedString::from(format!("side-d-{}", m.id)))
            .group("desktop-row")
            .relative()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .mx(px(10.))
            .pl(px(22.))
            .pr(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::tile("monitor", from, to, 16.))
            .child(div().flex_1().text_size(px(12.5)).text_color(if selected { theme.text } else { theme.text_muted }).child("Desktop"))
            // The size lines up with the monitors' below; the close button
            // takes its place on hover.
            .children(size.map(|s| div().flex_none().font_family(mono_font()).text_size(px(10.5)).text_color(theme.text_placeholder).group_hover("desktop-row", |st| st.invisible()).child(s)))
            .child(
                div()
                    .id(SharedString::from(format!("side-d-x-{}", m.id)))
                    .absolute()
                    .right(px(8.))
                    .invisible()
                    .group_hover("desktop-row", |st| st.visible())
                    .tooltip(|window, cx| kit::Tooltip::view("Close the desktop".into(), window, cx))
                    .child(icon("x", 12., theme.text_placeholder))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_desktop(&close, window, cx)
                    })),
            )
            .when_some(right_click, |el, f| el.on_mouse_down(MouseButton::Right, f))
            // Its main monitor (the virtual ones are listed below it).
            .on_click(cx.listener(move |this, _, window, cx| this.show_display(open.clone(), None, window, cx)))
    }

    /// A machine's open Logs or Services page in the sidebar (with what it
    /// shows, when there's something to say); × closes it.
    #[allow(clippy::too_many_arguments)]
    fn tool_row(&self, theme: &Theme, m: &Machine, route: Route, icon_name: &'static str, label: &'static str, shown: Option<String>, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.route == route;
        let (open, close) = (route.clone(), route);
        let (from, to) = format::machine_tile(m);
        let group = SharedString::from(format!("tool-row-{label}"));
        let hover = group.clone();
        div()
            .id(SharedString::from(format!("side-{label}-{}", m.id)))
            .group(group.clone())
            .relative()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .mx(px(10.))
            .pl(px(22.))
            .pr(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::tile(icon_name, from, to, 16.))
            .child(div().flex_none().text_size(px(12.5)).text_color(if selected { theme.text } else { theme.text_muted }).child(label))
            .child(div().flex_1())
            .children(shown.map(|s| {
                let w = (self.sidebar_width - 20. - 30. - 16. - 16. - 60. - 16.).max(30.);
                let hover = hover.clone();
                div().w(px(w)).flex().justify_end().group_hover(hover, |st| st.invisible()).child(div().max_w(px(w)).truncate().text_size(px(11.)).text_color(theme.text_placeholder).child(s))
            }))
            .child(
                div()
                    .id(SharedString::from(format!("side-{label}-x-{}", m.id)))
                    .absolute()
                    .right(px(8.))
                    .invisible()
                    .group_hover(group, |st| st.visible())
                    .tooltip(move |window, cx| kit::Tooltip::view(format!("Close {}", label.to_lowercase()).into(), window, cx))
                    .child(icon("x", 12., theme.text_placeholder))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_tool(&close, window, cx)
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(open.clone(), window, cx)))
    }

    /// Closes a machine's Logs or Services page (logs stop following).
    pub(super) fn close_tool(&mut self, route: &Route, window: &mut Window, cx: &mut Context<Self>) {
        let machine = match route {
            Route::Logs(id) => {
                self.logs.remove(id);
                id.clone()
            }
            Route::Services(id) => {
                self.service_pages.remove(id);
                id.clone()
            }
            _ => return,
        };
        if &self.route == route {
            self.navigate(Route::Machine(machine), window, cx);
        }
        cx.notify();
    }

    /// The machine's log viewer, opened if it isn't.
    pub(super) fn logs_view(&mut self, machine: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<super::logs::LogsView> {
        let engine = self.engine.clone();
        self.logs.entry(machine.to_string()).or_insert_with(|| cx.new(|cx| super::logs::LogsView::new(engine, machine.to_string(), window, cx))).clone()
    }

    /// A virtual monitor's item in the sidebar, under its machine: opens the
    /// desktop page on it.
    fn monitor_row(&self, theme: &Theme, m: &Machine, d: &crate::fleet::model::DisplayInfo, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let (machine, display) = (m.id.clone(), d.id.clone());
        let remove = (m.id.clone(), d.id.clone());
        let (from, to) = format::machine_tile(m);
        div()
            .id(SharedString::from(format!("side-v-{}-{}", m.id, d.id)))
            .group("monitor-row")
            .relative()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .mx(px(10.))
            .pl(px(22.))
            .pr(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.element_selected))
            .when(!selected, |el| el.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::tile("monitor", from, to, 16.))
            // An explicit width: GPUI 0.2 only ellipsizes text whose width it knows.
            .child(div().w(px((self.sidebar_width - 20. - 30. - 16. - 16. - 74. - 52.).max(40.))).truncate().text_size(px(12.5)).text_color(if selected { theme.text } else { theme.text_muted }).child(d.name.clone()))
            .child(div().flex_1())
            // Which ones Muxen added (and the × takes away).
            .child(kit::pill("virtual", theme.accent))
            .child(div().flex_none().font_family(mono_font()).text_size(px(10.5)).text_color(theme.text_placeholder).group_hover("monitor-row", |st| st.invisible()).child(format!("{}×{}", d.width as u32, d.height as u32)))
            // On hover, in place of the size: unplug it.
            .child(
                div()
                    .id(SharedString::from(format!("side-v-x-{}-{}", m.id, d.id)))
                    .absolute()
                    .right(px(8.))
                    .invisible()
                    .group_hover("monitor-row", |st| st.visible())
                    .tooltip(|window, cx| kit::Tooltip::view("Remove this virtual monitor".into(), window, cx))
                    .child(icon("x", 12., theme.text_placeholder))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.remove_display(remove.0.clone(), remove.1.clone(), window, cx)
                    })),
            )
            .on_mouse_down(MouseButton::Right, {
                let (machine, display) = (m.id.clone(), d.id.clone());
                super::menu::on_right_click(move || super::menu::MenuTarget::Display { machine: machine.clone(), display: display.clone() }, cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.show_display(machine.clone(), Some(display.clone()), window, cx)))
    }

    /// Moves a machine's desktop into a window of its own (and out of this one).
    pub(super) fn pop_out_desktop(&mut self, machine_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let display = self.desktops.get(machine_id).and_then(|v| v.read(cx).display().map(str::to_string));
        let id = machine_id.to_string();
        super::theme::remember(cx, |a| a.desktops.retain(|d| *d != id));
        self.desktops.remove(machine_id);
        let name = self.snapshot.machines.iter().find(|m| m.id == machine_id).map(|m| m.name.clone()).unwrap_or_else(|| machine_id.to_string());
        super::desktop::open_window(self.engine.clone(), machine_id.to_string(), display, name, cx);
        match self.route.clone() {
            Route::Desktop(_) => self.navigate(Route::Machine(machine_id.to_string()), window, cx),
            Route::Session(sid) => {
                if let Some(key) = self.page_split(&sid) {
                    self.split.remove(&key);
                }
                self.on_route_changed(window, cx);
            }
            _ => {}
        }
    }

    /// Shows or hides a machine's desktop next to a session's terminal.
    pub(super) fn toggle_split(&mut self, session_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.page_split(session_id) {
            Some(key) => {
                self.split.remove(&key);
            }
            None => {
                self.split.insert(session_id.to_string());
            }
        }
        self.on_route_changed(window, cx);
    }

    /// The desktop shown next to a session's terminal, if it's split.
    pub(super) fn split_desktop(&self, session_id: &str) -> Option<Entity<super::desktop::DesktopView>> {
        self.page_split(session_id)?;
        self.desktops.get(session_id.split('/').next().unwrap_or_default()).cloned()
    }

    fn render_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.route.clone() {
            Route::Overview => self.render_overview(window, cx).into_any_element(),
            Route::Jobs => self.render_jobs(cx).into_any_element(),
            Route::Workspaces => self.render_workspaces(cx).into_any_element(),
            Route::Workspace(id) => self.render_workspace(&id, cx),
            Route::Activity => self.render_activity(cx).into_any_element(),
            Route::Machine(id) => self.render_machine(&id, cx),
            Route::Runner(id) => self.render_runner(&id, cx),
            Route::Session(id) => self.render_session(&id, window, cx),
            Route::Desktop(id) => self.render_desktop(&id, window, cx),
            Route::Logs(id) => match self.logs.get(&id).cloned() {
                Some(view) => view.into_any_element(),
                None => {
                    self.on_route_changed(window, cx);
                    div().into_any_element()
                }
            },
            Route::Services(id) => match self.service_pages.get(&id).cloned() {
                Some(view) => view.into_any_element(),
                None => {
                    self.on_route_changed(window, cx);
                    div().into_any_element()
                }
            },
            Route::About => self.render_about(cx),
            Route::Github | Route::GithubRepo(_) | Route::GithubItem(..) => self.render_github(window, cx),
            Route::Settings => match &self.settings {
                Some(page) => page.clone().into_any_element(),
                None => {
                    self.on_route_changed(window, cx);
                    div().into_any_element()
                }
            },
        }
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // A drag released outside the window never reaches `on_drop`.
        if self.sidebar_resizing && !cx.has_active_drag() {
            self.finish_sidebar_resize(cx);
        }
        if self.split_resizing && !cx.has_active_drag() {
            self.finish_split_resize(cx);
        }
        self.content_width = f32::from(window.viewport_size().width) - if self.sidebar_hidden { 0. } else { self.sidebar_width } - 56.;
        // Switching away with ⌘ held (⌘Tab): its release never arrives here.
        let was_active = std::mem::replace(&mut self.window_active, window.is_window_active());
        if was_active && !self.window_active {
            self.show_keys = false;
            self.keys_soon = None;
        }
        if std::mem::take(&mut self.refocus) {
            // Moved to another pane of a split: its terminal; else the page.
            match &self.route {
                Route::Session(id) if self.terminals.contains_key(id) => window.focus(&self.terminals[id].focus_handle(cx)),
                _ => window.focus(&self.focus),
            }
        }
        let window_height = f32::from(window.viewport_size().height);
        // Dialogs sit lower on tall windows and scroll when they don't fit.
        let modal_top = if window_height > 800. { 96. } else { 40. };
        let modal_max_height = (window_height - modal_top - 24.).max(200.);
        let page = self.render_page(window, cx);
        let modal = self.modal.as_ref().map(|m| match m {
            Modal::AddMachine(d) => d.clone().into_any_element(),
            Modal::Install(d) => d.clone().into_any_element(),
            Modal::Confirm(d) => d.clone().into_any_element(),
            Modal::NewWorkspace(d) => d.clone().into_any_element(),
            Modal::NewSession(d) => d.clone().into_any_element(),
            Modal::Copy(d) => d.clone().into_any_element(),
            Modal::Cloud(d) => d.clone().into_any_element(),
            Modal::Rename(d) => d.clone().into_any_element(),
            Modal::AddDisplay(d) => d.clone().into_any_element(),
            Modal::MonitorPassword(d) => d.clone().into_any_element(),
            Modal::Licenses(d) => d.clone().into_any_element(),
        });
        let notice = self.render_notice(&theme);
        let menu = self.render_menu(&theme, cx);
        let palette = self.render_palette(&theme, cx);

        div()
            .id("workspace")
            .key_context(WORKSPACE)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::check_now))
            .on_action(cx.listener(Self::sync_github))
            .on_action(cx.listener(Self::show_overview))
            .on_action(cx.listener(Self::show_jobs))
            .on_action(cx.listener(|this, _: &ShowWorkspaces, window, cx| this.navigate(Route::Workspaces, window, cx)))
            .on_action(cx.listener(|this, _: &ShowGithub, window, cx| this.navigate(Route::Github, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine1, window, cx| this.go_to_machine(1, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine2, window, cx| this.go_to_machine(2, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine3, window, cx| this.go_to_machine(3, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine4, window, cx| this.go_to_machine(4, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine5, window, cx| this.go_to_machine(5, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine6, window, cx| this.go_to_machine(6, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine7, window, cx| this.go_to_machine(7, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine8, window, cx| this.go_to_machine(8, window, cx)))
            .on_action(cx.listener(|this, _: &GoToMachine9, window, cx| this.go_to_machine(9, window, cx)))
            .on_modifiers_changed(cx.listener(Self::modifiers_changed))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::toggle_activity))
            .on_action(cx.listener(Self::add_machine))
            .on_action(cx.listener(Self::new_session))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| this.split_session(false, window, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| this.split_session(true, window, cx)))
            .on_action(cx.listener(|this, _: &NextPane, window, cx| this.step_pane(1, window, cx)))
            .on_action(cx.listener(|this, _: &PreviousPane, window, cx| this.step_pane(-1, window, cx)))
            .on_action(cx.listener(|this, _: &QuickOpen, window, cx| this.open_palette(false, window, cx)))
            .on_action(cx.listener(|this, _: &CommandPalette, window, cx| this.open_palette(true, window, cx)))
            .on_action(cx.listener(|this, _: &ShowAbout, window, cx| this.navigate(Route::About, window, cx)))
            .on_action(cx.listener(|this, _: &SignInGitHub, window, cx| {
                // GitHub CLI's browser sign-in, in a terminal on this computer.
                match this.snapshot.machines.iter().find(|m| m.host == "local").map(|m| m.id.clone()) {
                    Some(id) => this.gh_cli_sign_in(&id, window, cx),
                    None => this.notify_user("Add this computer as a machine first (host \"local\"), or run `gh auth login` in a terminal.", true, cx),
                }
            }))
            .on_action(cx.listener(|this, _: &GrantGitHubAccess, window, cx| this.run_gh_fix("auth refresh -h github.com -s admin:org", window, cx)))
            .on_action(cx.listener(Self::dismiss))
            .on_drag_move(cx.listener(Self::resize_sidebar))
            .on_drop(cx.listener(|this, _: &DraggedSidebar, _, cx| {
                this.finish_sidebar_resize(cx);
                cx.notify();
            }))
            .relative()
            .flex()
            .size_full()
            .bg(theme.canvas)
            .text_color(theme.text)
            .font_family(ui_font())
            .text_size(px(13.))
            .when(!self.sidebar_hidden, |d| d.child(self.render_sidebar(window, cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(self.render_toolbar(window, cx))
                    .child(div().flex_1().min_h(px(0.)).child(page)),
            )
            .when(!self.sidebar_hidden, |d| d.child(self.sidebar_handle(&theme, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .children(notice)
            .children(menu)
            .children(palette)
            .when_some(modal, |d, modal| {
                d.child(
                    div()
                        .id("modal-layer")
                        .absolute()
                        .inset_0()
                        .bg(theme.scrim)
                        .flex()
                        .justify_center()
                        .pt(px(modal_top))
                        .occlude()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.modal = None;
                            window.focus(&this.focus);
                            cx.notify()
                        }))
                        .child(
                            div()
                                .id("modal")
                                .max_h(px(modal_max_height))
                                .overflow_y_scroll()
                                .rounded(px(10.))
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .child(modal),
                        ),
                )
            })
    }
}

impl Focusable for Root {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

pub(super) fn github_status(theme: &Theme, s: &Snapshot) -> (gpui::Hsla, String) {
    let g = &s.github;
    if g.token_source.is_empty() && !s.demo {
        (theme.warning, "GitHub not connected".into())
    } else if !g.configured {
        (theme.warning, "No GitHub org · using last check".into())
    } else if g.scopes.iter().all(|sc| sc.error.is_some()) && !g.scopes.is_empty() {
        (theme.error, g.error.clone().unwrap_or_else(|| "GitHub error".into()))
    } else if g.live {
        (theme.success, format!("Live · synced {}", format::ago(g.last_sync_at)))
    } else {
        (theme.text_placeholder, "Connecting to GitHub…".into())
    }
}

/// (section, title) for the toolbar.
fn route_title(s: &Snapshot, route: &Route) -> (Option<&'static str>, String) {
    match route {
        Route::Overview => (None, "Overview".into()),
        Route::Jobs => (None, "Jobs".into()),
        Route::Workspaces => (None, "Workspaces".into()),
        Route::Activity => (None, "Activity".into()),
        Route::Workspace(id) => (Some("Workspaces"), s.workspaces.iter().find(|w| &w.id == id).map(|w| w.name.clone()).unwrap_or_else(|| id.clone())),
        Route::Settings => (None, "Settings".into()),
        Route::About => (None, "About".into()),
        Route::Github => (None, "GitHub".into()),
        Route::GithubRepo(name) => (Some("GitHub"), name.clone()),
        Route::GithubItem(repo, n) => (Some("GitHub"), format!("{repo} #{n}")),
        Route::Machine(id) => (Some("Machines"), s.machines.iter().find(|m| &m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.clone())),
        Route::Session(id) => (Some("Sessions"), s.sessions.iter().find(|x| &x.id == id).map(|x| x.title.clone()).unwrap_or_else(|| id.clone())),
        Route::Desktop(id) => (Some("Desktop"), s.machines.iter().find(|m| &m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.clone())),
        Route::Logs(id) => (Some("Logs"), s.machines.iter().find(|m| &m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.clone())),
        Route::Services(id) => (Some("Services"), s.machines.iter().find(|m| &m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.clone())),
        Route::Runner(id) => {
            let r = s.runners.iter().find(|r| &r.id == id);
            let machine = r.and_then(|r| r.machine_id.as_deref()).and_then(|mid| s.machines.iter().find(|m| m.id == mid));
            (Some(if machine.is_some() { "Runners" } else { "GitHub" }), r.map(|r| r.name.clone()).unwrap_or_else(|| id.clone()))
        }
    }
}

/// "build (macos-arm64)" -> "build (macos-arm6…" when longer than `max` characters.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// A tooltip with its shortcut, when it has one: "Back  ⌘[".
fn tip(text: &str, keys: &str) -> String {
    if keys.is_empty() { text.to_string() } else { format!("{text}  {keys}") }
}
