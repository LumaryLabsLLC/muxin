//! The root view: a full-height fleet sidebar, a page area with its own
//! toolbar and history, and an activity drawer that slides over from the right.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight, KeyBinding, Menu, MenuItem,
    ScrollHandle, SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};

use super::dialogs::{AddMachineDialog, ConfirmDialog, DialogEvent, InstallDialog, NewWorkspaceDialog};
use super::format;
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, icon, runner_glyph, spinner};
use super::settings::SettingsPage;
use super::theme::{ActiveTheme, mono_font, Theme, ui_font};
use super::workspaces::WorkspaceUi;
use super::activity::{ActivityFilter, ActivityItem};
use crate::fleet::workspaces::{CreateWorkspace, WorkspaceIsolation};
use crate::fleet::Engine;
use crate::fleet::model::{Action, Machine, Runner, Snapshot};
use crate::ops::RunnerAction;

actions!(
    openrunner,
    [
        CheckNow,
        SyncGitHub,
        ShowOverview,
        ShowJobs,
        OpenSettings,
        FocusFilter,
        GoBack,
        GoForward,
        ToggleActivity,
        AddMachine,
        Dismiss,
        Quit
    ]
);

const WORKSPACE: &str = "Root";
pub(super) const SIDEBAR_WIDTH: f32 = 256.;
/// Utilization samples kept for the overview's pulse chart.
const PULSE_SAMPLES: usize = 64;
const PULSE_EVERY: Duration = Duration::from_secs(5);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-r", CheckNow, None),
        KeyBinding::new("secondary-shift-r", SyncGitHub, None),
        KeyBinding::new("secondary-1", ShowOverview, None),
        KeyBinding::new("secondary-2", ShowJobs, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-f", FocusFilter, None),
        KeyBinding::new("secondary-[", GoBack, None),
        KeyBinding::new("secondary-]", GoForward, None),
        KeyBinding::new("secondary-j", ToggleActivity, None),
        KeyBinding::new("secondary-n", AddMachine, None),
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("escape", Dismiss, Some(WORKSPACE)),
        KeyBinding::new("/", FocusFilter, Some(WORKSPACE)),
    ]);
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "OpenRunner".into(),
            items: vec![MenuItem::action("Settings…", OpenSettings), MenuItem::separator(), MenuItem::action("Quit OpenRunner", Quit)],
        },
        Menu {
            name: "Fleet".into(),
            items: vec![
                MenuItem::action("Check Now", CheckNow),
                MenuItem::action("Sync GitHub", SyncGitHub),
                MenuItem::separator(),
                MenuItem::action("Add Machine…", AddMachine),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Overview", ShowOverview),
                MenuItem::action("Jobs", ShowJobs),
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
    Activity,
    Machine(String),
    Runner(String),
    Settings,
}

pub enum Modal {
    AddMachine(Entity<AddMachineDialog>),
    Install(Entity<InstallDialog>),
    Confirm(Entity<ConfirmDialog>),
    NewWorkspace(Entity<NewWorkspaceDialog>),
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
    pub(super) other_open: bool,
    pub(super) activity_filter: ActivityFilter,
    pub(super) activity_selected: Option<ActivityItem>,
    pub(super) activity_scroll: ScrollHandle,
    pub(super) activity_seen_len: usize,
    pub(super) modal: Option<Modal>,
    pub(super) settings: Option<Entity<SettingsPage>>,
    pub(super) notice: Option<Notice>,
    pub(super) page_scroll: ScrollHandle,
    pub(super) pulse: VecDeque<PulseSample>,
    last_pulse: Option<Instant>,
    /// Width of the page area, for layouts that change with the window size.
    pub(super) content_width: f32,
    pub(super) ws_ui: WorkspaceUi,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Root {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx, "Search fleet"));
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

        let mut ws = Root {
            engine,
            snapshot,
            focus: cx.focus_handle(),
            route: Route::Overview,
            back: Vec::new(),
            forward: Vec::new(),
            filter,
            collapsed: HashSet::new(),
            other_open: false,
            activity_filter: ActivityFilter::default(),
            activity_selected: None,
            activity_scroll: ScrollHandle::new(),
            activity_seen_len: 0,
            modal: None,
            settings: None,
            notice: None,
            page_scroll: ScrollHandle::new(),
            pulse: VecDeque::with_capacity(PULSE_SAMPLES),
            last_pulse: None,
            content_width: 1000.,
            ws_ui: WorkspaceUi::default(),
            _tasks: vec![updates, ticker],
            _subscriptions: subscriptions,
        };
        ws.record_pulse();
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
        self.snapshot = snapshot;
        cx.notify();
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

    fn on_route_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.route == Route::Settings && self.settings.is_none() {
            let engine = self.engine.clone();
            self.settings = Some(cx.new(|cx| SettingsPage::new(engine, window, cx)));
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
            "size" => {}
            "settings" => self.navigate(Route::Settings, window, cx),
            "machine" => self.navigate(Route::Machine(arg.into()), window, cx),
            "runner" => self.navigate(Route::Runner(arg.into()), window, cx),
            "install" => {
                self.navigate(Route::Machine(arg.into()), window, cx);
                self.show_install_dialog(arg, window, cx);
            }
            "add-machine" => self.show_add_machine(window, cx),
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
            "bottom" => {
                self.page_scroll.scroll_to_bottom();
                if let (Route::Settings, Some(page)) = (&self.route, &self.settings) {
                    page.read(cx).scroll.scroll_to_bottom();
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

    pub(super) fn run_attention_action(&mut self, action: &Action, window: &mut Window, cx: &mut Context<Self>) {
        match action.kind.as_str() {
            "settings" => self.navigate(Route::Settings, window, cx),
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
        let engine = self.engine.clone();
        let snapshot = self.snapshot.clone();
        let machine_id = machine_id.to_string();
        let dialog = cx.new(|cx| InstallDialog::new(engine, snapshot, machine_id, window, cx));
        self.show_modal(Modal::Install(dialog.clone()), window, cx);
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
                "{} will be stopped, its service uninstalled, and it will be deregistered from GitHub and removed from fleet.toml.",
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
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Remove {} from the fleet?", machine.name),
                "It's taken out of fleet.toml. Nothing on the machine changes, and its runners stay registered.".into(),
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

    pub(super) fn confirm_destroy_workspace(&mut self, id: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let wid = id.to_string();
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Destroy {name}?"),
                "Background commands it started are stopped and its folder is deleted from the machine.".into(),
                "Destroy workspace",
                Some("Keep the files on the machine"),
                move |keep_files, engine| {
                    let engine = engine.clone();
                    let wid = wid.clone();
                    // Runs as an operation; progress and errors show in Activity.
                    engine.runtime().clone().spawn(async move {
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

    pub(super) fn runner_action(&mut self, runner_id: &str, action: RunnerAction, cx: &mut Context<Self>) {
        match self.engine.runner_action(runner_id, action) {
            Ok(op) => self.show_operation(op, cx),
            Err(e) => self.notify_user(e, true, cx),
        }
    }

    fn show_modal(&mut self, modal: Modal, window: &mut Window, cx: &mut Context<Self>) {
        let focus = match &modal {
            Modal::AddMachine(d) => d.read(cx).first_focus(cx),
            Modal::Install(d) => d.read(cx).first_focus(cx),
            Modal::Confirm(d) => d.focus_handle(cx),
            Modal::NewWorkspace(d) => d.read(cx).first_focus(cx),
        };
        window.focus(&focus);
        self.modal = Some(modal);
        cx.notify();
    }

    fn on_dialog_event<D>(&mut self, _: &Entity<D>, event: &DialogEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        match event {
            DialogEvent::Dismissed => {}
            DialogEvent::OperationStarted(op) => self.show_operation(*op, cx),
            DialogEvent::MachineAdded(id) => self.navigate(Route::Machine(id.clone()), window, cx),
            DialogEvent::OpenSettings => self.navigate(Route::Settings, window, cx),
            DialogEvent::CreateWorkspace(req, iso) => self.create_workspace(req.clone(), iso.clone(), window, cx),
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

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Route::Settings, window, cx);
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.filter.focus_handle(cx));
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

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
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
        let running_ops = s.operations.iter().filter(|o| o.status == "running").count();
        let mac_titlebar = cfg!(target_os = "macos") && !window.is_fullscreen();

        let mut rows: Vec<AnyElement> = Vec::new();
        let runner_machines: Vec<&Machine> = s.machines.iter().filter(|m| m.kind == "runner").collect();
        let others: Vec<&Machine> = s.machines.iter().filter(|m| m.kind == "other").collect();
        let unassigned: Vec<&Runner> = s.runners.iter().filter(|r| r.machine_id.is_none() && matches_runner(r)).collect();

        rows.push(self.section_header("Machines", runner_machines.len(), true, cx).into_any_element());
        for m in &runner_machines {
            let runners: Vec<&Runner> = m.runner_ids.iter().filter_map(|id| s.runners.iter().find(|r| &r.id == id)).collect();
            let machine_match = q.is_empty() || m.name.to_lowercase().contains(&q);
            let visible: Vec<&Runner> = runners.iter().copied().filter(|r| machine_match || matches_runner(r)).collect();
            if !machine_match && visible.is_empty() {
                continue;
            }
            let open = !self.collapsed.contains(&m.id) || !q.is_empty();
            rows.push(self.machine_row(&theme, &s, m, open, selected_machine.as_deref() == Some(&m.id), cx).into_any_element());
            if open {
                for r in visible {
                    rows.push(self.runner_row(&theme, &s, r, selected_runner.as_deref() == Some(&r.id), cx).into_any_element());
                }
            }
        }
        if !unassigned.is_empty() {
            rows.push(self.section_header("On GitHub only", unassigned.len(), false, cx).into_any_element());
            for r in unassigned {
                rows.push(self.runner_row(&theme, &s, r, selected_runner.as_deref() == Some(&r.id), cx).into_any_element());
            }
        }
        let others_visible: Vec<&&Machine> = others.iter().filter(|m| q.is_empty() || m.name.to_lowercase().contains(&q)).collect();
        if !others_visible.is_empty() {
            let open = self.other_open || !q.is_empty();
            rows.push(
                div()
                    .id("others-toggle")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .mt(px(14.))
                    .mx(px(10.))
                    .px(px(8.))
                    .h(px(24.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.ghost_hover))
                    .text_size(px(11.5))
                    .text_color(theme.text_placeholder)
                    .child(icon(if open { "chevron-down" } else { "chevron-right" }, 12., theme.text_placeholder))
                    .child(format!("Other machines · {}", others_visible.len()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.other_open = !this.other_open;
                        cx.notify()
                    }))
                    .into_any_element(),
            );
            if open {
                for m in others_visible {
                    rows.push(self.machine_row(&theme, &s, m, false, selected_machine.as_deref() == Some(&m.id), cx).into_any_element());
                }
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
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border_variant)
            // Room for the macOS traffic lights; the window drags from here.
            .when(mac_titlebar, |d| {
                d.child(div().id("sidebar-drag").h(px(40.)).flex_none().on_click(|e: &ClickEvent, window, _| {
                    if e.click_count() == 2 {
                        window.titlebar_double_click();
                    }
                }))
            })
            .child(self.brand(&theme, &s))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(10.))
                    .pt(px(6.))
                    .child(self.nav_item("nav-overview", "gauge", "Overview", None, Route::Overview, cx))
                    .child(self.nav_item("nav-jobs", "workflow", "Jobs", Some(s.counts.queued + s.counts.running), Route::Jobs, cx))
                    .child(self.nav_item("nav-workspaces", "terminal", "Workspaces", Some(s.workspaces.len()), Route::Workspaces, cx))
                    .child(self.activity_nav(&theme, running_ops, cx))
                    .child(self.nav_item("nav-settings", "settings", "Settings", None, Route::Settings, cx)),
            )
            .child(
                div().px(px(12.)).pt(px(12.)).pb(px(4.)).child(
                    div()
                        .relative()
                        .child(self.filter.clone())
                        .child(div().absolute().right(px(7.)).top(px(6.)).child(kit::kbd(&theme, "/"))),
                ),
            )
            .child(div().id("fleet-list").flex_1().min_h(px(0.)).overflow_y_scroll().pb(px(12.)).children(rows))
            .child(self.connection_card(&theme, &s, cx))
    }

    fn brand(&self, theme: &Theme, s: &Snapshot) -> impl IntoElement {
        let scope = format::scope_label(s);
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(16.))
            .pt(px(if cfg!(target_os = "macos") { 2. } else { 14. }))
            .pb(px(8.))
            .child(gpui::img("brand/icon.png").size(px(32.)).flex_none())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.))
                    .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("OpenRunner"))
                    .child(div().truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
                        "{} · {} runners",
                        scope.unwrap_or_else(|| "self-hosted".into()),
                        s.counts.runners
                    ))),
            )
            .when(s.demo, |d| d.child(div().flex_1()).child(kit::pill("demo", theme.warning)))
    }

    fn nav_item(&self, id: &'static str, icon_name: &'static str, text: &'static str, count: Option<usize>, route: Route, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // A workspace page keeps "Workspaces" highlighted.
        let active = self.route == route || (route == Route::Workspaces && matches!(self.route, Route::Workspace(_)));
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
            .child(icon(icon_name, 15., if active { theme.text_accent } else { theme.text_placeholder }))
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
            .child(icon("activity", 15., if open { theme.text_accent } else { theme.text_placeholder }))
            .child("Activity")
            .child(div().flex_1())
            .when(running > 0, |d| d.child(spinner("nav-activity-spin", 13., theme.accent)))
            .when(running == 0, |d| d.child(kit::kbd(theme, "⌘J")))
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
                        .tooltip("Add machine  ⌘N")
                        .on_click(cx.listener(|this, _, window, cx| this.show_add_machine(window, cx))),
                )
            })
    }

    fn machine_row(&self, theme: &Theme, s: &Snapshot, m: &Machine, open: bool, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = m.id.clone();
        let toggle_id = m.id.clone();
        let has_runners = !m.runner_ids.is_empty();
        let glyphs: Vec<AnyElement> = m
            .runner_ids
            .iter()
            .filter_map(|rid| s.runners.iter().find(|r| &r.id == rid))
            .take(8)
            .map(|r| runner_glyph(theme, &r.state, 7.).into_any_element())
            .collect();
        let extra = m.runner_ids.len().saturating_sub(8);
        div()
            .id(SharedString::from(format!("side-m-{}", m.id)))
            .group("machine-row")
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .mx(px(10.))
            .pl(px(6.))
            .pr(px(8.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(
                div()
                    .id(SharedString::from(format!("side-t-{}", m.id)))
                    .size(px(14.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(has_runners, |d| {
                        d.invisible()
                            .group_hover("machine-row", |st| st.visible())
                            .child(icon(if open { "chevron-down" } else { "chevron-right" }, 11., theme.text_placeholder))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if !this.collapsed.remove(&toggle_id) {
                                    this.collapsed.insert(toggle_id.clone());
                                }
                                cx.notify()
                            }))
                    }),
            )
            .child(if m.checking {
                spinner(SharedString::from(format!("side-spin-{}", m.id)), 10., theme.text_muted).into_any_element()
            } else {
                kit::dot(theme.machine_status(&m.status), 7.).into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if selected { theme.text } else { theme.text })
                    .child(m.name.clone()),
            )
            .child(div().flex().flex_none().gap(px(2.)).children(glyphs))
            .when(extra > 0, |d| d.child(div().font_family(mono_font()).text_size(px(10.5)).text_color(theme.text_placeholder).child(format!("+{extra}"))))
            .when(m.kind == "other", |d| {
                d.child(div().font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(m.platform.clone()))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(id.clone()), window, cx)))
    }

    fn runner_row(&self, theme: &Theme, s: &Snapshot, r: &Runner, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = r.id.clone();
        let job = r.job_id.and_then(|j| s.jobs.running.iter().find(|x| x.id == j));
        let right = match (r.state.as_str(), job) {
            ("working", Some(j)) => ellipsize(&j.name, 14),
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
            .pl(px(28.))
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
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Runner(id.clone()), window, cx)))
    }

    fn connection_card(&self, theme: &Theme, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let (color, title) = github_status(theme, s);
        let detail = match (s.check.running, s.check.next_at) {
            (true, _) => "Checking machines…".to_string(),
            (false, Some(next)) => format!("Checked {} · next {}", format::ago(s.check.finished_at), format::until(next)),
            (false, None) if s.check.finished_at.is_some() => format!("Checked {}", format::ago(s.check.finished_at)),
            _ => "Machines not checked yet".into(),
        };
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
                        .child(div().truncate().text_size(px(12.5)).text_color(theme.text).child(title))
                        .child(div().truncate().text_size(px(11.)).text_color(theme.text_placeholder).child(detail)),
                )
                .on_click(cx.listener(|this, _, window, cx| this.navigate(Route::Settings, window, cx))),
        )
    }

    // -----------------------------------------------------------------------
    // Toolbar and overlays

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let (section, title) = route_title(&s, &self.route);
        div()
            .id("toolbar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .h(px(52.))
            .px(px(16.))
            .border_b_1()
            .border_color(theme.border_variant)
            .on_click(|e: &ClickEvent, window, _| {
                if e.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .child(
                        Button::icon_only("back", "arrow-left")
                            .tooltip("Back  ⌘[")
                            .disabled(self.back.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| this.go_back(&GoBack, window, cx))),
                    )
                    .child(
                        Button::icon_only("forward", "arrow-right")
                            .tooltip("Forward  ⌘]")
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
            .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(format!("Updated {}", format::ago(s.updated_at))))
            .child(
                Button::icon_only("sync", "refresh-cw")
                    .loading(s.github.syncing && s.github.configured)
                    .tooltip("Sync GitHub  ⇧⌘R")
                    .on_click(cx.listener(|this, _, _, _| this.engine.trigger_sync())),
            )
            .child(
                Button::new("check-now", if s.check.running { "Checking" } else { "Check now" })
                    .style(ButtonStyle::Primary)
                    .icon("refresh-cw")
                    .loading(s.check.running)
                    .key_hint("⌘R")
                    .on_click(cx.listener(|this, _, _, _| this.engine.trigger_check(None))),
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

    fn render_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.route.clone() {
            Route::Overview => self.render_overview(window, cx).into_any_element(),
            Route::Jobs => self.render_jobs(cx).into_any_element(),
            Route::Workspaces => self.render_workspaces(cx).into_any_element(),
            Route::Workspace(id) => self.render_workspace(&id, cx),
            Route::Activity => self.render_activity(cx).into_any_element(),
            Route::Machine(id) => self.render_machine(&id, cx),
            Route::Runner(id) => self.render_runner(&id, cx),
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
        self.content_width = f32::from(window.viewport_size().width) - SIDEBAR_WIDTH - 56.;
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
        });
        let notice = self.render_notice(&theme);

        div()
            .id("workspace")
            .key_context(WORKSPACE)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::check_now))
            .on_action(cx.listener(Self::sync_github))
            .on_action(cx.listener(Self::show_overview))
            .on_action(cx.listener(Self::show_jobs))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::toggle_activity))
            .on_action(cx.listener(Self::add_machine))
            .on_action(cx.listener(Self::dismiss))
            .relative()
            .flex()
            .size_full()
            .bg(theme.canvas)
            .text_color(theme.text)
            .font_family(ui_font())
            .text_size(px(13.))
            .child(self.render_sidebar(window, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(self.render_toolbar(cx))
                    .child(div().flex_1().min_h(px(0.)).child(page)),
            )
            .children(notice)
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
        Route::Machine(id) => (Some("Machines"), s.machines.iter().find(|m| &m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.clone())),
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
