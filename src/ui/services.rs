//! A machine's services and Docker (see fleet/services.rs and ops/docker.rs):
//! services with what they use, containers, Compose projects and images;
//! starting, stopping, restarting, running, pulling, removing. Stopping or
//! removing asks first; the system's services take the user's password,
//! asked here when sudo wants it and kept in memory only. Slow Docker work
//! (pulls, runs, Compose up) runs as an operation, its output in Activity.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription, Task, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list};

use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, icon, spinner};
use super::theme::{ActiveTheme, Theme};
use crate::fleet::Engine;
use crate::fleet::desktop::AddDisplayError;
use crate::fleet::logs::LogSource;
use super::compose::{self as picture, NodeAction};
use crate::fleet::services::{ComposeGraph, ComposeProject, DockerRun, Image, Service, ServiceList};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Services,
    Containers,
    Compose,
    Images,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    /// Failed first, then running, then the rest (as the machine lists them).
    State,
    Cpu,
    Memory,
}

/// What the page asks the rest of the app to do.
pub enum ServicesEvent {
    /// Show this log in the machine's log viewer.
    OpenLogs(LogSource),
    /// A terminal on the machine running `command` in `cwd` (a container's
    /// shell, an editor on a compose file).
    OpenTerminal { cwd: String, command: String, title: String },
}

impl EventEmitter<ServicesEvent> for ServicesView {}

/// The fields of "Run a container".
struct RunForm {
    image: Entity<TextInput>,
    name: Entity<TextInput>,
    ports: Entity<TextInput>,
    env: Entity<TextInput>,
    volumes: Entity<TextInput>,
    command: Entity<TextInput>,
    restart: &'static str,
}

pub struct ServicesView {
    engine: Arc<Engine>,
    machine: String,
    list: Option<Result<ServiceList, String>>,
    tab: Tab,
    sort: Sort,
    find: Entity<TextInput>,
    /// Something waiting for a yes (stop, remove, down): its key and the action.
    confirming: Option<(String, &'static str)>,
    /// Actions under way, by key.
    busy: HashSet<String>,
    /// An action waiting for the user's password, and the field for it.
    asking: Option<(Service, &'static str)>,
    password: Entity<TextInput>,
    password_error: Option<String>,
    /// The last thing that didn't work.
    error: Option<String>,
    /// Docker operations under way: (operation id, what it is).
    ops: Vec<(u64, String)>,
    run: Option<RunForm>,
    pull: Entity<TextInput>,
    new_compose: Option<Entity<TextInput>>,
    /// The Compose project shown as a picture (its name), and its parts.
    project: Option<String>,
    graph: Option<Result<ComposeGraph, String>>,
    scroll: UniformListScrollHandle,
    /// The list's width, measured as it's drawn: GPUI only cuts off text
    /// (with …) when told its width, so the rows work it out from this.
    width: Rc<Cell<f32>>,
    focus: FocusHandle,
    refreshing: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for ServicesView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// A service's key in the page: kind, user or system, and target.
fn key(s: &Service) -> String {
    format!("{}:{}:{}", s.kind, s.user, s.target)
}

pub(super) fn memory(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        Some(b) if b >= 1 << 20 => format!("{} MB", b >> 20),
        Some(b) if b > 0 => format!("{} KB", (b >> 10).max(1)),
        _ => String::new(),
    }
}

pub(super) fn cpu(pct: Option<f64>) -> String {
    match pct {
        Some(p) if p >= 10. => format!("{p:.0}%"),
        Some(p) if p >= 0.05 => format!("{p:.1}%"),
        Some(_) => "0%".into(),
        None => String::new(),
    }
}

/// "a, b, c" as its parts.
fn parts(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

impl ServicesView {
    pub fn new(engine: Arc<Engine>, machine: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let find = cx.new(|cx| TextInput::new(cx, "Find"));
        let password = cx.new(|cx| TextInput::new(cx, "Password").secret());
        let pull = cx.new(|cx| TextInput::new(cx, "Image to pull, e.g. postgres:17"));
        let notify = |_: &mut Self, _: &Entity<TextInput>, e: &InputEvent, _: &mut Window, cx: &mut Context<Self>| {
            if *e == InputEvent::Changed {
                cx.notify();
            }
        };
        let subs = vec![
            cx.subscribe_in(&find, window, notify),
            cx.subscribe_in(&password, window, |this, _, e: &InputEvent, window, cx| match e {
                InputEvent::Submit => this.give_password(window, cx),
                InputEvent::Cancel => {
                    this.asking = None;
                    cx.notify();
                }
                InputEvent::Changed => {}
            }),
            cx.subscribe_in(&pull, window, |this, _, e: &InputEvent, window, cx| {
                if *e == InputEvent::Submit {
                    this.pull_image(window, cx);
                }
            }),
        ];
        let mut view = ServicesView {
            engine,
            machine,
            list: None,
            tab: Tab::Services,
            sort: Sort::State,
            find,
            confirming: None,
            busy: HashSet::new(),
            asking: None,
            password,
            password_error: None,
            error: None,
            ops: Vec::new(),
            run: None,
            pull,
            new_compose: None,
            project: None,
            graph: None,
            scroll: UniformListScrollHandle::new(),
            width: Rc::new(Cell::new(0.)),
            focus: cx.focus_handle(),
            refreshing: None,
            _tasks: Vec::new(),
            _subscriptions: subs,
        };
        view.refresh(window, cx);
        // Their state and use change on their own: look again now and then.
        let tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(15)).await;
                if this.update_in(cx, |this, window, cx| this.refresh(window, cx)).is_err() {
                    return;
                }
            }
        });
        // Operations finishing (a pull, a Compose up): show it, look again.
        let mut rx = view.engine.subscribe();
        let follow = cx.spawn_in(window, async move |this, cx| {
            while rx.changed().await.is_ok() {
                let done = this.update_in(cx, |this, window, cx| this.check_ops(window, cx));
                if done.is_err() {
                    return;
                }
            }
        });
        view._tasks.extend([tick, follow]);
        view
    }

    /// Shows a tab by name (services, containers, compose, images).
    pub fn show_tab(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(project) = name.strip_prefix("compose/") {
            self.tab = Tab::Compose;
            self.open_project(project.to_string(), cx);
            return;
        }
        self.tab = match name {
            "containers" => Tab::Containers,
            "compose" => Tab::Compose,
            "images" => Tab::Images,
            _ => Tab::Services,
        };
        cx.notify();
    }

    /// Looks again: the list at once, then with what each uses (measured).
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.project.is_some() {
            self.fetch_graph(cx);
        }
        let (engine, machine) = (self.engine.clone(), self.machine.clone());
        let fast = engine.runtime().spawn({
            let (engine, machine) = (engine.clone(), machine.clone());
            async move { engine.services(&machine, false).await }
        });
        let full = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.services(&machine, true).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            // The quick look first, keeping the numbers from before until the new ones come.
            if let Ok(Ok(mut quick)) = fast.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(Ok(old)) = &this.list {
                        let usage = |list: &[Service]| list.iter().map(|s| (key(s), (s.cpu, s.memory))).collect::<std::collections::HashMap<_, _>>();
                        let (svc, ctr) = (usage(&old.services), usage(&old.containers));
                        for s in quick.services.iter_mut() {
                            (s.cpu, s.memory) = svc.get(&key(s)).copied().unwrap_or((None, None));
                        }
                        for s in quick.containers.iter_mut() {
                            (s.cpu, s.memory) = ctr.get(&key(s)).copied().unwrap_or((None, None));
                        }
                    } else if quick.services.is_empty() && !quick.containers.is_empty() {
                        this.tab = Tab::Containers;
                    }
                    this.list = Some(Ok(quick));
                    cx.notify();
                });
            }
            let result = full.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                // On the first look, the tab that has something.
                if this.list.is_none()
                    && let Ok(l) = &result
                    && l.services.is_empty()
                    && !l.containers.is_empty()
                {
                    this.tab = Tab::Containers;
                }
                // A failed refresh keeps what was shown.
                if result.is_ok() || this.list.is_none() {
                    this.list = Some(result);
                }
                cx.notify();
            });
        });
        // A newer look replaces one still waiting.
        self.refreshing = Some(task);
    }

    /// Operations that ended: why, when one failed; then the list again.
    fn check_ops(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ops.is_empty() {
            return;
        }
        let snap = self.engine.current();
        let mut ended = false;
        let mut failed = None;
        self.ops.retain(|(id, _)| match snap.operations.iter().find(|o| o.id == *id) {
            Some(o) if o.status == "running" => true,
            Some(o) => {
                if o.error.is_some() {
                    failed = Some(o.error.clone().unwrap_or_default());
                }
                ended = true;
                false
            }
            None => false,
        });
        if let Some(e) = failed {
            self.error = Some(e);
        }
        if ended {
            self.refresh(window, cx);
        }
        cx.notify();
    }

    fn watch(&mut self, result: Result<u64, String>, what: String, cx: &mut Context<Self>) {
        match result {
            Ok(op) => self.ops.push((op, what)),
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    /// Does an action on a service or container: stop, restart, remove ask
    /// first (`confirmed` once they have).
    fn act(&mut self, service: Service, action: &'static str, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(action, "stop" | "restart" | "remove" | "prune") && !confirmed {
            self.confirming = Some((key(&service), action));
            cx.notify();
            return;
        }
        self.confirming = None;
        self.error = None;
        let k = key(&service);
        self.busy.insert(k.clone());
        cx.notify();
        let (engine, machine) = (self.engine.clone(), self.machine.clone());
        let job = engine.runtime().spawn({
            let (engine, service) = (engine.clone(), service.clone());
            async move { engine.service_action(&machine, &service, action).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(AddDisplayError::Failed(e.to_string())));
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy.remove(&k);
                match result {
                    Ok(()) => {
                        this.asking = None;
                        this.password_error = None;
                    }
                    Err(AddDisplayError::NeedsPassword(why)) => {
                        // Asked once already: it didn't work.
                        if this.asking.is_some() {
                            this.password_error = Some(why);
                        }
                        this.asking = Some((service, action));
                        window.focus(&this.password.focus_handle(cx));
                    }
                    Err(AddDisplayError::Failed(why)) => this.error = Some(why),
                }
                this.refresh(window, cx);
                cx.notify();
            });
        });
        task.detach();
    }

    /// The password typed: kept in memory for this machine, then the action again.
    fn give_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.password.read(cx).text().to_string();
        if typed.is_empty() {
            return;
        }
        self.password.update(cx, |i, cx| i.set_text("", cx));
        self.engine.remember_password(&self.machine, typed);
        if let Some((service, action)) = self.asking.clone() {
            self.act(service, action, true, window, cx);
        }
    }

    fn pull_image(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let image = self.pull.read(cx).text().trim().to_string();
        if image.is_empty() {
            return;
        }
        self.pull.update(cx, |i, cx| i.set_text("", cx));
        let result = self.engine.docker_pull(&self.machine, &image);
        self.watch(result, format!("Pulling {image}"), cx);
    }

    /// Opens "Run a container", with an image already in it.
    fn open_run(&mut self, image: &str, window: &mut Window, cx: &mut Context<Self>) {
        let input = |cx: &mut Context<Self>, placeholder: &str, text: &str| {
            let (placeholder, text) = (placeholder.to_string(), text.to_string());
            cx.new(|cx| {
                let mut i = TextInput::new(cx, placeholder);
                i.set_text(text, cx);
                i
            })
        };
        let form = RunForm {
            image: input(cx, "Image, e.g. nginx:alpine", image),
            name: input(cx, "Name (optional)", ""),
            ports: input(cx, "Ports: 8080:80, 5432:5432", ""),
            env: input(cx, "Env: KEY=value, OTHER=x", ""),
            volumes: input(cx, "Volumes: /srv/data:/data", ""),
            command: input(cx, "Command (optional)", ""),
            restart: "unless-stopped",
        };
        window.focus(&form.image.focus_handle(cx));
        self.run = Some(form);
        self.tab = Tab::Containers;
        cx.notify();
    }

    fn start_run(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.run else { return };
        let text = |i: &Entity<TextInput>| i.read(cx).text().trim().to_string();
        let run = DockerRun {
            image: text(&form.image),
            name: text(&form.name),
            ports: parts(&text(&form.ports)),
            env: parts(&text(&form.env)),
            volumes: parts(&text(&form.volumes)),
            restart: form.restart.to_string(),
            command: text(&form.command),
        };
        let what = format!("Running {}", run.image);
        let result = self.engine.docker_run(&self.machine, run);
        if result.is_ok() {
            self.run = None;
        }
        self.watch(result, what, cx);
    }

    /// Shows a Compose project as a picture of its parts.
    fn open_project(&mut self, name: String, cx: &mut Context<Self>) {
        self.project = Some(name);
        self.graph = None;
        self.confirming = None;
        self.fetch_graph(cx);
        cx.notify();
    }

    fn known_project(&self, name: &str) -> ComposeProject {
        let known = self.list.as_ref().and_then(|l| l.as_ref().ok()).and_then(|l| l.compose.iter().find(|p| p.name == name).cloned());
        known.unwrap_or_else(|| ComposeProject { name: name.to_string(), ..Default::default() })
    }

    /// Asks the machine for the open project's parts (and its containers' state).
    fn fetch_graph(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.project.clone() else { return };
        let project = self.known_project(&name);
        let (engine, machine) = (self.engine.clone(), self.machine.clone());
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.compose_describe(&machine, &project).await }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                if this.project.as_deref() != Some(name.as_str()) {
                    return;
                }
                // A failed look keeps the picture there was.
                if result.is_ok() || !matches!(this.graph, Some(Ok(_))) {
                    this.graph = Some(result);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn compose(&mut self, project: ComposeProject, action: &'static str, confirmed: bool, cx: &mut Context<Self>) {
        if action == "down" && !confirmed {
            self.confirming = Some((format!("compose:{}", project.name), action));
            cx.notify();
            return;
        }
        self.confirming = None;
        let what = format!("{} {}", action, project.name);
        let result = self.engine.compose_action(&self.machine, &project, action);
        self.watch(result, what, cx);
    }

    /// A new Compose project: its folder and a starting compose.yaml, opened to edit.
    fn create_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.new_compose else { return };
        let name = input.read(cx).text().trim().to_string();
        if name.is_empty() {
            return;
        }
        let (engine, machine) = (self.engine.clone(), self.machine.clone());
        let job = engine.runtime().spawn({
            let (engine, name) = (engine.clone(), name.clone());
            async move { engine.compose_new(&machine, &name, None).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(file) => {
                        this.new_compose = None;
                        let folder = file.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
                        cx.emit(ServicesEvent::OpenTerminal { cwd: folder, command: "${EDITOR:-nano} compose.yaml; exec \"$SHELL\" -l".into(), title: format!("{name} · compose") });
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            });
        });
        task.detach();
    }
}

/// A small chip button (tabs, sort, restart policy).
fn chip(theme: &Theme, id: String, label: impl Into<SharedString>, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .px(px(9.))
        .h(px(24.))
        .flex()
        .items_center()
        .gap(px(5.))
        .rounded(px(5.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(if on { theme.text } else { theme.text_placeholder })
        .when(on, |d| d.bg(theme.element_selected))
        .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
        .child(label.into())
}

/// "Are you sure?" with a yes and a cancel, for the right side of a row.
#[allow(clippy::too_many_arguments)]
fn confirm_row(theme: &Theme, n: usize, text: String, risky: bool, yes: &'static str, yes_icon: &'static str, on_yes: impl Fn(&mut Window, &mut gpui::App) + 'static, on_no: impl Fn(&mut gpui::App) + 'static) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(div().text_size(px(12.)).text_color(if risky { theme.error } else { theme.text_muted }).child(text))
        .child(Button::new(SharedString::from(format!("svc-yes-{n}")), yes).icon(yes_icon).style(ButtonStyle::Danger).on_click(move |_, window, cx| on_yes(window, cx)))
        .child(Button::new(SharedString::from(format!("svc-no-{n}")), "Cancel").style(ButtonStyle::Ghost).on_click(move |_, _, cx| on_no(cx)))
        .into_any_element()
}

fn empty(theme: &Theme, text: String) -> AnyElement {
    div().flex_1().flex().items_center().justify_center().text_size(px(13.)).text_color(theme.text_muted).child(text).into_any_element()
}

/// One row's frame: its columns, then the actions at the right.
fn row(theme: &Theme) -> gpui::Div {
    div().w_full().flex().items_center().gap(px(10.)).h(px(40.)).px(px(14.)).border_b_1().border_color(theme.border_variant)
}

impl Render for ServicesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let machine_name = self.engine.current().machines.iter().find(|m| m.id == self.machine).map(|m| m.name.clone()).unwrap_or_else(|| self.machine.clone());
        let data = self.list.as_ref().and_then(|l| l.as_ref().ok()).cloned().unwrap_or_default();
        let mut tab_list = vec![(Tab::Services, "Services", data.services.len()), (Tab::Containers, "Containers", data.containers.len())];
        if data.docker {
            tab_list.extend([(Tab::Compose, "Compose", data.compose.len()), (Tab::Images, "Images", data.images.len())]);
        }
        let tabs = div().flex_none().flex().gap(px(2.)).p(px(2.)).rounded(px(7.)).bg(theme.element).children(tab_list.into_iter().map(|(tab, label, n)| {
            chip(&theme, format!("svc-tab-{label}"), label, self.tab == tab)
                .when(n > 0, |d| d.child(div().text_size(px(10.5)).text_color(theme.text_placeholder).child(n.to_string())))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = tab;
                    this.confirming = None;
                    this.project = None;
                    cx.notify();
                }))
        }));
        // What each tab adds to the toolbar.
        let extra: Vec<AnyElement> = match self.tab {
            Tab::Containers if data.docker => vec![
                Button::new("svc-run", "Run a container")
                    .icon("plus")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.run.is_some() {
                            this.run = None;
                            cx.notify();
                        } else {
                            this.open_run("", window, cx);
                        }
                    }))
                    .into_any_element(),
            ],
            Tab::Images => vec![
                div().w(px(240.)).child(self.pull.clone()).into_any_element(),
                Button::new("svc-pull", "Pull").icon("arrow-down").on_click(cx.listener(|this, _, window, cx| this.pull_image(window, cx))).into_any_element(),
                Button::new("svc-prune", "Remove unused")
                    .icon("trash-2")
                    .style(ButtonStyle::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirming = Some(("image:prune".into(), "prune"));
                        cx.notify();
                    }))
                    .into_any_element(),
            ],
            Tab::Compose if self.project.is_none() => vec![
                Button::new("svc-compose-new", "New project")
                    .icon("plus")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.new_compose.is_some() {
                            this.new_compose = None;
                        } else {
                            let input = cx.new(|cx| TextInput::new(cx, "Project name, e.g. blog"));
                            let sub = cx.subscribe_in(&input, window, |this, _, e: &InputEvent, window, cx| {
                                if *e == InputEvent::Submit {
                                    this.create_compose(window, cx);
                                }
                            });
                            this._subscriptions.push(sub);
                            window.focus(&input.focus_handle(cx));
                            this.new_compose = Some(input);
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            ],
            _ => Vec::new(),
        };
        let toolbar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(44.))
            .px(px(14.))
            .border_b_1()
            .border_color(theme.border_variant)
            .child(tabs)
            .child(div().flex_1())
            .children(extra)
            .child(div().w(px(200.)).child(self.find.clone()))
            .child(Button::icon_only("svc-refresh", "refresh-cw").tooltip("Refresh").on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))));

        // Bars under the toolbar: the password, operations under way, the last error.
        let asking = self.asking.as_ref().map(|(s, action)| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(8.))
                .border_b_1()
                .border_color(theme.border_variant)
                .bg(theme.surface)
                .child(icon("lock", 14., theme.text_muted))
                .child(div().text_size(px(12.5)).text_color(theme.text).child(format!("To {action} {}, Muxen needs your password on {machine_name}.", s.name)))
                .child(div().w(px(200.)).child(self.password.clone()))
                .child(Button::new("svc-password-go", "Continue").style(ButtonStyle::Primary).on_click(cx.listener(|this, _, window, cx| this.give_password(window, cx))))
                .child(
                    Button::new("svc-password-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, _, cx| {
                        this.asking = None;
                        this.password_error = None;
                        cx.notify();
                    })),
                )
                .children(self.password_error.clone().map(|e| div().text_size(px(12.)).text_color(theme.error).child(e)))
                .child(div().flex_1())
                .child(div().text_size(px(11.)).text_color(theme.text_placeholder).child("Kept in memory until you quit."))
        });
        let ops = (!self.ops.is_empty()).then(|| {
            let snap = self.engine.current();
            div().flex_none().flex().flex_col().children(self.ops.iter().map(|(id, what)| {
                let last = snap.operations.iter().find(|o| o.id == *id).and_then(|o| o.log.last().cloned()).unwrap_or_default();
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(theme.border_variant)
                    .text_size(px(12.))
                    .child(spinner(SharedString::from(format!("svc-op-{id}")), 12., theme.accent))
                    .child(div().flex_none().text_color(theme.text).child(format!("{what}…")))
                    .child(div().flex_1().min_w(px(0.)).overflow_hidden().whitespace_nowrap().text_color(theme.text_placeholder).child(last))
            }))
        });
        let error = self.error.clone().map(|e| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(14.))
                .py(px(7.))
                .border_b_1()
                .border_color(theme.border_variant)
                .text_size(px(12.5))
                .text_color(theme.error)
                .child(icon("circle-alert", 14., theme.error))
                .child(div().flex_1().child(e))
                .child(Button::icon_only("svc-error-x", "x").on_click(cx.listener(|this, _, _, cx| {
                    this.error = None;
                    cx.notify();
                })))
        });
        // Removing every unused image: are you sure?
        let prune = self.confirming.as_ref().is_some_and(|(k, _)| k == "image:prune").then(|| {
            let (yes, no) = (cx.entity().downgrade(), cx.entity().downgrade());
            div().flex_none().flex().justify_end().px(px(14.)).py(px(6.)).border_b_1().border_color(theme.border_variant).child(confirm_row(
                &theme,
                9999,
                "Remove every image no container uses?".into(),
                false,
                "Remove",
                "trash-2",
                move |window, cx| {
                    let _ = yes.update(cx, |v, cx| {
                        let s = Service { kind: "image".into(), target: "unused".into(), name: "unused images".into(), ..Default::default() };
                        v.act(s, "prune", true, window, cx);
                    });
                },
                move |cx| {
                    let _ = no.update(cx, |v, cx| {
                        v.confirming = None;
                        cx.notify();
                    });
                },
            ))
        });
        // Run a container.
        let run_form = self.run.as_ref().filter(|_| self.tab == Tab::Containers).map(|f| {
            let field = |label: &'static str, input: &Entity<TextInput>, w: f32| div().flex().flex_col().gap(px(4.)).child(div().text_size(px(11.)).text_color(theme.text_placeholder).child(label)).child(div().w(px(w)).child(input.clone()));
            let restarts = div().flex().flex_col().gap(px(4.)).child(div().text_size(px(11.)).text_color(theme.text_placeholder).child("Restart")).child(
                div().flex().gap(px(2.)).p(px(2.)).rounded(px(7.)).bg(theme.element).children([("no", "Never"), ("always", "Always"), ("unless-stopped", "Unless stopped"), ("on-failure", "On failure")].into_iter().map(|(value, label)| {
                    chip(&theme, format!("svc-restart-{value}"), label, f.restart == value).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = &mut this.run {
                            f.restart = value;
                        }
                        cx.notify();
                    }))
                })),
            );
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(10.))
                .p(px(14.))
                .border_b_1()
                .border_color(theme.border_variant)
                .bg(theme.surface)
                .child(div().text_size(px(13.)).text_color(theme.text).child("Run a container"))
                .child(div().flex().flex_wrap().gap(px(12.)).child(field("Image", &f.image, 260.)).child(field("Name", &f.name, 180.)).child(restarts))
                .child(div().flex().flex_wrap().gap(px(12.)).child(field("Ports", &f.ports, 220.)).child(field("Environment", &f.env, 260.)).child(field("Volumes", &f.volumes, 240.)))
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(12.))
                        .child(field("Command", &f.command, 360.))
                        .child(Button::new("svc-run-go", "Run").icon("play").style(ButtonStyle::Primary).on_click(cx.listener(|this, _, window, cx| this.start_run(window, cx))))
                        .child(Button::new("svc-run-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, _, cx| {
                            this.run = None;
                            cx.notify();
                        })))
                        .child(div().text_size(px(11.)).text_color(theme.text_placeholder).child("Pulls the image if it's not here. Commas separate several ports, variables or volumes.")),
                )
        });
        let new_compose = self.new_compose.as_ref().filter(|_| self.tab == Tab::Compose && self.project.is_none()).map(|input| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(10.))
                .border_b_1()
                .border_color(theme.border_variant)
                .bg(theme.surface)
                .child(div().w(px(240.)).child(input.clone()))
                .child(Button::new("svc-compose-create", "Create").style(ButtonStyle::Primary).on_click(cx.listener(|this, _, window, cx| this.create_compose(window, cx))))
                .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child("Makes ~/compose/<name>/compose.yaml on the machine and opens it to edit; then Up."))
        });

        let find = self.find.read(cx).text().trim().to_lowercase();
        let matches = |name: &str, detail: &str| find.is_empty() || name.to_lowercase().contains(&find) || detail.to_lowercase().contains(&find);
        let view = cx.entity().downgrade();
        // The column names go over rows, not over "Nothing here yet".
        let mut rows = false;
        let body: AnyElement = match &self.list {
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_size(px(13.))
                .text_color(theme.text_muted)
                .child(spinner("svc-loading", 14., theme.text_muted))
                .child(format!("Asking {machine_name}…"))
                .into_any_element(),
            Some(Err(e)) => div().flex_1().flex().items_center().justify_center().text_size(px(13.)).text_color(theme.error).child(e.clone()).into_any_element(),
            Some(Ok(_)) => match self.tab {
                Tab::Services | Tab::Containers => {
                    let (items, note) = if self.tab == Tab::Services {
                        (data.services.clone(), data.services_note.clone())
                    } else {
                        (data.containers.clone(), data.containers_note.clone().or_else(|| (!data.docker).then(|| "Docker isn't installed on this machine.".into())))
                    };
                    let mut items: Vec<Service> = items.into_iter().filter(|s| matches(&s.name, &s.detail)).collect();
                    match self.sort {
                        Sort::State => {}
                        Sort::Cpu => items.sort_by(|a, b| b.cpu.unwrap_or(-1.).total_cmp(&a.cpu.unwrap_or(-1.))),
                        Sort::Memory => items.sort_by_key(|s| std::cmp::Reverse(s.memory.unwrap_or(0))),
                    }
                    if items.is_empty() {
                        empty(&theme, if find.is_empty() { note.unwrap_or_else(|| "Nothing here yet.".into()) } else { "Nothing matches.".into() })
                    } else {
                        rows = true;
                        self.service_rows(items, view, machine_name.clone(), &theme)
                    }
                }
                Tab::Compose if self.project.is_some() => self.project_view(&data, cx),
                Tab::Compose => {
                    let items: Vec<ComposeProject> = data.compose.iter().filter(|p| matches(&p.name, &p.files)).cloned().collect();
                    if items.is_empty() {
                        empty(&theme, if find.is_empty() { "No Compose projects yet: New project makes one.".into() } else { "Nothing matches.".into() })
                    } else {
                        rows = true;
                        self.compose_rows(items, view, &theme)
                    }
                }
                Tab::Images => {
                    let items: Vec<Image> = data.images.iter().filter(|i| matches(&i.name, &i.id)).cloned().collect();
                    if items.is_empty() {
                        empty(&theme, if find.is_empty() { "No images yet: pull one.".into() } else { "Nothing matches.".into() })
                    } else {
                        rows = true;
                        self.image_rows(items, view, &theme)
                    }
                }
            },
        };
        let header = rows.then(|| self.header(cx));
        div()
            .id("services-view")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.canvas)
            .child(toolbar)
            .children(asking)
            .children(ops)
            .children(error)
            .children(prune)
            .children(run_form)
            .children(new_compose)
            .children(header)
            .child(body)
    }
}

/// Column widths, shared by the header and the rows.
const W_CPU: f32 = 60.;
const W_MEM: f32 = 76.;
const W_STATE: f32 = 190.;
const W_BOOT: f32 = 92.;
const W_ACTIONS: f32 = 300.;
const ROW_H: f32 = 50.;

/// About how wide `text` is at `size` (for a name with a pill after it).
fn about(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.62 + 4.
}

/// Text that may not fit: cut off, the whole of it on hover.
fn clipped(id: String, text: String, size: f32, color: gpui::Hsla, w: f32) -> gpui::Stateful<gpui::Div> {
    let tip = text.clone();
    div()
        .id(SharedString::from(id))
        .flex_none()
        .w(px(w.max(40.)))
        .truncate()
        .text_size(px(size))
        .text_color(color)
        .when(!tip.is_empty(), |d| d.tooltip(move |window, cx| kit::Tooltip::view(tip.clone().into(), window, cx)))
        .child(text)
}

impl ServicesView {
    /// The column names above the list (CPU, Memory and State sort it).
    fn header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let label = |text: &'static str| div().text_size(px(11.)).text_color(theme.text_placeholder).child(text);
        let sortable = |id: &'static str, text: &'static str, sort: Sort, w: f32, right: bool, cx: &mut Context<Self>| {
            let on = self.sort == sort;
            div()
                .id(id)
                .flex_none()
                .w(px(w))
                .flex()
                .when(right, |d| d.justify_end())
                .when(!right, |d| d.pl(px(12.)))
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(if on { theme.text } else { theme.text_placeholder })
                .when(!on, |d| d.hover(|st| st.text_color(theme.text_muted)))
                .tooltip(move |window, cx| {
                    let tip = match (sort, on) {
                        (Sort::State, _) => "Failed first, then running, then stopped",
                        (Sort::Cpu, false) => "Sort by CPU, highest first",
                        (Sort::Memory, false) => "Sort by memory, highest first",
                        _ => "Highest first: click for the usual order",
                    };
                    kit::Tooltip::view(tip.into(), window, cx)
                })
                .child(if on { format!("{text} ↓") } else { text.to_string() })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sort = if this.sort == sort { Sort::State } else { sort };
                    cx.notify();
                }))
        };
        let width = self.width.clone();
        let measure = gpui::canvas(
            move |b, window, _| {
                let w = f32::from(b.size.width);
                if (width.get() - w).abs() > 0.5 {
                    width.set(w);
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let frame = div().flex_none().w_full().relative().flex().items_center().gap(px(10.)).h(px(28.)).px(px(14.)).border_b_1().border_color(theme.border_variant).bg(theme.surface).child(measure);
        match self.tab {
            Tab::Services | Tab::Containers => frame
                .child(div().flex_none().w(px(7.)))
                .child(div().flex_1().min_w(px(0.)).child(label(if self.tab == Tab::Services { "Name" } else { "Name · image" })))
                .child(sortable("svc-h-cpu", "CPU", Sort::Cpu, W_CPU, true, cx))
                .child(sortable("svc-h-mem", "Memory", Sort::Memory, W_MEM, true, cx))
                .child(sortable("svc-h-state", if self.tab == Tab::Services { "State" } else { "Status" }, Sort::State, W_STATE, false, cx))
                .when(self.tab == Tab::Services, |d| d.child(div().flex_none().w(px(W_BOOT)).child(label("At boot"))))
                .child(div().flex_none().w(px(W_ACTIONS)))
                .into_any_element(),
            Tab::Compose => frame
                .child(div().flex_none().w(px(7.)))
                .child(div().flex_1().min_w(px(0.)).child(label("Project · files")))
                .child(div().flex_none().w(px(W_STATE)).pl(px(12.)).child(label("Status")))
                .child(div().flex_none().w(px(360.)))
                .into_any_element(),
            Tab::Images => frame
                .child(div().flex_none().w(px(14.)))
                .child(div().flex_1().min_w(px(0.)).child(label("Image · id")))
                .child(div().flex_none().w(px(80.)).flex().justify_end().child(label("Size")))
                .child(div().flex_none().w(px(W_STATE)).pl(px(12.)).child(label("Created")))
                .child(div().flex_none().w(px(240.)))
                .into_any_element(),
        }
    }

    fn service_rows(&self, items: Vec<Service>, view: gpui::WeakEntity<Self>, machine_name: String, theme: &Theme) -> AnyElement {
        let (confirming, busy) = (self.confirming.clone(), self.busy.clone());
        let theme = *theme;
        let services_tab = self.tab == Tab::Services;
        // What's left for the name and detail: the width less the fixed columns and gaps.
        let text_w = self.width.get() - 28. - 7. - W_CPU - W_MEM - W_STATE - W_ACTIONS - 50. - if services_tab { W_BOOT + 10. } else { 0. };
        uniform_list("svc-rows", items.len(), move |range, _, _| {
            range
                .map(|n| {
                    let s = items[n].clone();
                    let k = key(&s);
                    let running = s.running();
                    let failed = matches!(s.state.as_str(), "failed" | "error");
                    let dot = if failed {
                        theme.error
                    } else if running {
                        theme.success
                    } else {
                        theme.text_placeholder
                    };
                    let container = s.kind == "docker";
                    let state = if container { s.status.clone() } else { s.state.clone() };
                    let act = |id: &str, label: &'static str, icon_name: &'static str, action: &'static str| {
                        let (view, s) = (view.clone(), s.clone());
                        Button::new(SharedString::from(format!("svc-{id}-{n}")), label).icon(icon_name).style(ButtonStyle::Ghost).on_click(move |_, window, cx| {
                            let _ = view.update(cx, |v, cx| v.act(s.clone(), action, false, window, cx));
                        })
                    };
                    let actions = match &confirming {
                        Some((ck, action)) if *ck == k => {
                            let risky = s.keeps_muxen_connected() && *action != "restart";
                            let (verb, glyph) = match *action {
                                "stop" => ("Stop", "circle-stop"),
                                "remove" => ("Remove", "trash-2"),
                                _ => ("Restart", "rotate-ccw"),
                            };
                            let text = if risky {
                                format!("{verb} {}? It can cut Muxen off from {machine_name}.", s.name)
                            } else if *action == "remove" {
                                format!("Remove {} for good?", s.name)
                            } else {
                                format!("{verb} {}?", s.name)
                            };
                            let (yes, no, s2, action) = (view.clone(), view.clone(), s.clone(), *action);
                            confirm_row(
                                &theme,
                                n,
                                text,
                                risky,
                                verb,
                                glyph,
                                move |window, cx| {
                                    let _ = yes.update(cx, |v, cx| v.act(s2.clone(), action, true, window, cx));
                                },
                                move |cx| {
                                    let _ = no.update(cx, |v, cx| {
                                        v.confirming = None;
                                        cx.notify();
                                    });
                                },
                            )
                        }
                        _ if busy.contains(&k) => div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(theme.text_muted).child(spinner(SharedString::from(format!("svc-busy-{n}")), 12., theme.text_muted)).child("Working…").into_any_element(),
                        _ => {
                            let logs = {
                                let (view, source) = (view.clone(), s.log_source());
                                Button::icon_only(SharedString::from(format!("svc-logs-{n}")), "scroll-text").tooltip("Its logs").on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |_, cx| cx.emit(ServicesEvent::OpenLogs(source.clone())));
                                })
                            };
                            let mut row = div().flex().items_center().gap(px(4.)).child(logs);
                            if container && running {
                                let (view, name) = (view.clone(), s.name.clone());
                                row = row.child(Button::icon_only(SharedString::from(format!("svc-shell-{n}")), "square-terminal").tooltip("A shell inside it").on_click(move |_, _, cx| {
                                    let command = format!("docker exec -it {name} sh -c 'command -v bash >/dev/null && exec bash || exec sh'");
                                    let _ = view.update(cx, |_, cx| cx.emit(ServicesEvent::OpenTerminal { cwd: "~".into(), command, title: format!("{name} · shell") }));
                                }));
                            }
                            if running {
                                row = row.child(act("restart", "Restart", "rotate-ccw", "restart")).child(act("stop", "Stop", "circle-stop", "stop"));
                            } else {
                                row = row.child(act("start", "Start", "play", "start"));
                            }
                            if container {
                                let (view, s2) = (view.clone(), s.clone());
                                row = row.child(Button::icon_only(SharedString::from(format!("svc-rm-{n}")), "trash-2").tooltip("Remove").on_click(move |_, window, cx| {
                                    let _ = view.update(cx, |v, cx| v.act(s2.clone(), "remove", false, window, cx));
                                }));
                            }
                            row.into_any_element()
                        }
                    };
                    // Whether it starts at boot (systemd): click to change.
                    let boot = (s.kind == "service" && matches!(s.at_boot.as_str(), "enabled" | "disabled")).then(|| {
                        let on = s.at_boot == "enabled";
                        let (view, s2) = (view.clone(), s.clone());
                        div()
                            .id(SharedString::from(format!("svc-boot-{n}")))
                            .flex_none()
                            .px(px(7.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .border_1()
                            .border_color(theme.border_variant)
                            .text_color(if on { theme.text_muted } else { theme.text_placeholder })
                            .hover(|st| st.bg(theme.ghost_hover))
                            .tooltip(move |window, cx| kit::Tooltip::view(if on { "Starts at boot: click to stop that" } else { "Doesn't start at boot: click to make it" }.into(), window, cx))
                            .child(if on { "At boot" } else { "Not at boot" })
                            .on_click(move |_, window, cx| {
                                let _ = view.update(cx, |v, cx| v.act(s2.clone(), if on { "disable" } else { "enable" }, true, window, cx));
                            })
                    });
                    let yours = s.user && s.kind == "service";
                    let detail = if s.ports.is_empty() { s.detail.clone() } else { format!("{} · {}", s.detail, s.ports) };
                    // Name over what it is; then CPU, memory, state, at boot, actions.
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .h(px(ROW_H))
                        .px(px(14.))
                        .border_b_1()
                        .border_color(theme.border_variant)
                        .child(kit::dot(dot, 7.))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .min_w(px(0.))
                                        .when(!yours, |d| d.child(clipped(format!("svc-name-{n}"), s.name.clone(), 13., theme.text, text_w)))
                                        .when(yours, |d| d.child(clipped(format!("svc-name-{n}"), s.name.clone(), 13., theme.text, about(&s.name, 13.).min(text_w - 60.))).child(kit::pill("yours", theme.text_placeholder))),
                                )
                                .child(clipped(format!("svc-detail-{n}"), detail, 11.5, theme.text_placeholder, text_w)),
                        )
                        .child(div().flex_none().w(px(W_CPU)).flex().justify_end().text_size(px(12.)).text_color(theme.text_muted).child(cpu(s.cpu)))
                        .child(div().flex_none().w(px(W_MEM)).flex().justify_end().text_size(px(12.)).text_color(theme.text_muted).child(memory(s.memory)))
                        .child(div().flex_none().w(px(W_STATE)).pl(px(12.)).child(clipped(format!("svc-state-{n}"), state, 12., if failed { theme.error } else { theme.text_muted }, W_STATE - 12.)))
                        .when(services_tab, |d| d.child(div().flex_none().w(px(W_BOOT)).flex().children(boot)))
                        .child(div().flex_none().w(px(W_ACTIONS)).flex().justify_end().child(actions))
                })
                .collect()
        })
        .track_scroll(self.scroll.clone())
        .flex_1()
        .into_any_element()
    }

    /// A Compose project as a picture: a bar with what can be done to it, the
    /// key, then its parts.
    fn project_view(&self, data: &ServiceList, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let name = self.project.clone().unwrap_or_default();
        let graph = self.graph.as_ref().and_then(|g| g.as_ref().ok());
        let mut project = self.known_project(&name);
        if project.files.is_empty()
            && let Some(g) = graph
        {
            project.files = g.files.join(",");
        }
        let running = project.running() || graph.is_some_and(|g| g.services.iter().any(|s| s.running()));
        let back = Button::new("cmp-back", "Compose").icon("arrow-left").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, _, cx| {
            this.project = None;
            this.graph = None;
            this.confirming = None;
            cx.notify();
        }));
        let confirming_down = matches!(&self.confirming, Some((k, _)) if *k == format!("compose:{name}"));
        let actions: AnyElement = if confirming_down {
            let (yes, no, p2) = (cx.entity().downgrade(), cx.entity().downgrade(), project.clone());
            confirm_row(
                &theme,
                9999,
                format!("Take {name} down? Its containers stop and go."),
                false,
                "Down",
                "circle-stop",
                move |_, cx| {
                    let _ = yes.update(cx, |v, cx| v.compose(p2.clone(), "down", true, cx));
                },
                move |cx| {
                    let _ = no.update(cx, |v, cx| {
                        v.confirming = None;
                        cx.notify();
                    });
                },
            )
        } else {
            let act = |id: &'static str, label: &'static str, glyph: &'static str, action: &'static str, cx: &mut Context<Self>| {
                let p = project.clone();
                Button::new(id, label).icon(glyph).style(ButtonStyle::Ghost).on_click(cx.listener(move |this, _, _, cx| this.compose(p.clone(), action, false, cx)))
            };
            let logs = {
                let source = project.log_source();
                Button::icon_only("cmp-p-logs", "scroll-text").tooltip("Its logs").on_click(cx.listener(move |_, _, _, cx| cx.emit(ServicesEvent::OpenLogs(source.clone()))))
            };
            let edit = {
                let (folder, file, name) = (project.folder().to_string(), project.first_file().to_string(), name.clone());
                Button::icon_only("cmp-p-edit", "pencil").tooltip("Edit its compose file").on_click(cx.listener(move |_, _, _, cx| {
                    let command = format!("${{EDITOR:-nano}} '{}'; exec \"$SHELL\" -l", file.replace('\'', "'\\''"));
                    cx.emit(ServicesEvent::OpenTerminal { cwd: folder.clone(), command, title: format!("{name} · compose") });
                }))
            };
            let mut row = div().flex().items_center().gap(px(4.)).child(logs).child(edit).child(act("cmp-p-pull", "Pull", "arrow-down", "pull", cx));
            if running {
                row = row.child(act("cmp-p-restart", "Restart", "rotate-ccw", "restart", cx)).child(act("cmp-p-down", "Down", "circle-stop", "down", cx));
            } else {
                row = row.child(act("cmp-p-up", "Up", "play", "up", cx));
            }
            row.into_any_element()
        };
        let bar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(48.))
            .pl(px(6.))
            .pr(px(14.))
            .border_b_1()
            .border_color(theme.border_variant)
            .child(back)
            .child(kit::dot(if running { theme.success } else { theme.text_placeholder }, 8.))
            .child(div().flex_none().text_size(px(14.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme.text).child(name.clone()))
            .child(div().flex_none().text_size(px(12.)).text_color(theme.text_muted).child(project.status.clone()))
            .child(div().flex_1().min_w(px(0.)).overflow_hidden().whitespace_nowrap().text_size(px(12.)).text_color(theme.text_placeholder).child(project.first_file().to_string()))
            .child(actions);
        let body = match &self.graph {
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_size(px(13.))
                .text_color(theme.text_muted)
                .child(spinner("cmp-loading", 14., theme.text_muted))
                .child(format!("Reading {name}'s compose file…"))
                .into_any_element(),
            Some(Err(e)) => div().flex_1().flex().items_center().justify_center().px(px(40.)).text_size(px(13.)).text_color(theme.error).child(e.clone()).into_any_element(),
            Some(Ok(g)) if g.services.is_empty() => empty(&theme, "It has no services yet: Edit adds them.".into()),
            Some(Ok(g)) => {
                let usage: picture::Usage = data.containers.iter().map(|c| (c.name.clone(), (c.cpu, c.memory))).collect();
                let images: std::collections::HashMap<String, String> = g.services.iter().flat_map(|s| s.containers.iter().map(move |c| (c.name.clone(), s.image.clone()))).collect();
                let view = cx.entity().downgrade();
                let on: picture::OnNode = Rc::new(move |action, _, cx| {
                    let _ = view.update(cx, |_, cx| match action {
                        NodeAction::Logs(container) => {
                            let detail = images.get(&container).cloned().unwrap_or_default();
                            cx.emit(ServicesEvent::OpenLogs(LogSource { kind: "docker".into(), target: container.clone(), name: container, detail, ..Default::default() }));
                        }
                        NodeAction::Shell(container) => {
                            let command = format!("docker exec -it {container} sh -c 'command -v bash >/dev/null && exec bash || exec sh'");
                            cx.emit(ServicesEvent::OpenTerminal { cwd: "~".into(), command, title: format!("{container} · shell") });
                        }
                    });
                });
                div().flex_1().min_h(px(0.)).flex().flex_col().child(picture::legend(g, &theme)).child(picture::diagram(g, &usage, &theme, self.width.clone(), on)).into_any_element()
            }
        };
        div().flex_1().min_h(px(0.)).flex().flex_col().child(bar).child(body).into_any_element()
    }

    fn compose_rows(&self, items: Vec<ComposeProject>, view: gpui::WeakEntity<Self>, theme: &Theme) -> AnyElement {
        let confirming = self.confirming.clone();
        let theme = *theme;
        let text_w = self.width.get() - 28. - 7. - W_STATE - 360. - 30.;
        uniform_list("compose-rows", items.len(), move |range, _, _| {
            range
                .map(|n| {
                    let p = items[n].clone();
                    let running = p.running();
                    let do_it = |id: &str, label: &'static str, icon_name: &'static str, action: &'static str| {
                        let (view, p) = (view.clone(), p.clone());
                        Button::new(SharedString::from(format!("cmp-{id}-{n}")), label).icon(icon_name).style(ButtonStyle::Ghost).on_click(move |_, _, cx| {
                            let _ = view.update(cx, |v, cx| v.compose(p.clone(), action, false, cx));
                        })
                    };
                    let actions = match &confirming {
                        Some((k, _)) if *k == format!("compose:{}", p.name) => {
                            let (yes, no, p2) = (view.clone(), view.clone(), p.clone());
                            confirm_row(
                                &theme,
                                n,
                                format!("Take {} down? Its containers stop and go.", p.name),
                                false,
                                "Down",
                                "circle-stop",
                                move |_, cx| {
                                    let _ = yes.update(cx, |v, cx| v.compose(p2.clone(), "down", true, cx));
                                },
                                move |cx| {
                                    let _ = no.update(cx, |v, cx| {
                                        v.confirming = None;
                                        cx.notify();
                                    });
                                },
                            )
                        }
                        _ => {
                            let logs = {
                                let (view, source) = (view.clone(), p.log_source());
                                Button::icon_only(SharedString::from(format!("cmp-logs-{n}")), "scroll-text").tooltip("Its logs").on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |_, cx| cx.emit(ServicesEvent::OpenLogs(source.clone())));
                                })
                            };
                            let edit = {
                                let (view, folder, file, name) = (view.clone(), p.folder().to_string(), p.first_file().to_string(), p.name.clone());
                                Button::icon_only(SharedString::from(format!("cmp-edit-{n}")), "pencil").tooltip("Edit its compose file").on_click(move |_, _, cx| {
                                    let command = format!("${{EDITOR:-nano}} '{}'; exec \"$SHELL\" -l", file.replace('\'', "'\\''"));
                                    let _ = view.update(cx, |_, cx| cx.emit(ServicesEvent::OpenTerminal { cwd: folder.clone(), command, title: format!("{name} · compose") }));
                                })
                            };
                            let parts = {
                                let (view, name) = (view.clone(), p.name.clone());
                                Button::icon_only(SharedString::from(format!("cmp-parts-{n}")), "workflow").tooltip("Its parts, as a picture").on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |v, cx| v.open_project(name.clone(), cx));
                                })
                            };
                            let mut row = div().flex().items_center().gap(px(4.)).child(parts).child(logs).child(edit).child(do_it("pull", "Pull", "arrow-down", "pull"));
                            if running {
                                row = row.child(do_it("restart", "Restart", "rotate-ccw", "restart")).child(do_it("down", "Down", "circle-stop", "down"));
                            } else {
                                row = row.child(do_it("up", "Up", "play", "up"));
                            }
                            row.into_any_element()
                        }
                    };
                    row(&theme)
                        .h(px(ROW_H))
                        .child(kit::dot(if running { theme.success } else { theme.text_placeholder }, 7.))
                        .child(
                            div()
                                .id(SharedString::from(format!("cmp-open-{n}")))
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .cursor_pointer()
                                .on_click({
                                    let (view, name) = (view.clone(), p.name.clone());
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |v, cx| v.open_project(name.clone(), cx));
                                    }
                                })
                                .child(clipped(format!("cmp-name-{n}"), p.name.clone(), 13., theme.text, text_w))
                                .child(clipped(format!("cmp-files-{n}"), p.files.clone(), 11.5, theme.text_placeholder, text_w)),
                        )
                        .child(div().flex_none().w(px(W_STATE)).pl(px(12.)).child(clipped(format!("cmp-status-{n}"), p.status.clone(), 12., theme.text_muted, W_STATE - 12.)))
                        .child(div().flex_none().w(px(360.)).flex().justify_end().child(actions))
                })
                .collect()
        })
        .track_scroll(self.scroll.clone())
        .flex_1()
        .into_any_element()
    }

    fn image_rows(&self, items: Vec<Image>, view: gpui::WeakEntity<Self>, theme: &Theme) -> AnyElement {
        let (confirming, busy) = (self.confirming.clone(), self.busy.clone());
        let theme = *theme;
        let text_w = self.width.get() - 28. - 14. - 80. - W_STATE - 240. - 40.;
        uniform_list("image-rows", items.len(), move |range, _, _| {
            range
                .map(|n| {
                    let i = items[n].clone();
                    let as_service = Service { kind: "image".into(), target: i.id.clone(), name: i.name.clone(), ..Default::default() };
                    let k = key(&as_service);
                    let actions = match &confirming {
                        Some((ck, _)) if *ck == k => {
                            let (yes, no, s2) = (view.clone(), view.clone(), as_service.clone());
                            confirm_row(
                                &theme,
                                n,
                                format!("Remove {}?", i.name),
                                false,
                                "Remove",
                                "trash-2",
                                move |window, cx| {
                                    let _ = yes.update(cx, |v, cx| v.act(s2.clone(), "remove", true, window, cx));
                                },
                                move |cx| {
                                    let _ = no.update(cx, |v, cx| {
                                        v.confirming = None;
                                        cx.notify();
                                    });
                                },
                            )
                        }
                        _ if busy.contains(&k) => div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(theme.text_muted).child(spinner(SharedString::from(format!("img-busy-{n}")), 12., theme.text_muted)).child("Working…").into_any_element(),
                        _ => {
                            let (run_view, image) = (view.clone(), i.name.clone());
                            let (rm_view, s2) = (view.clone(), as_service.clone());
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .child(Button::new(SharedString::from(format!("img-run-{n}")), "Run").icon("play").style(ButtonStyle::Ghost).on_click(move |_, window, cx| {
                                    let _ = run_view.update(cx, |v, cx| v.open_run(&image, window, cx));
                                }))
                                .child(Button::icon_only(SharedString::from(format!("img-rm-{n}")), "trash-2").tooltip("Remove").on_click(move |_, window, cx| {
                                    let _ = rm_view.update(cx, |v, cx| v.act(s2.clone(), "remove", false, window, cx));
                                }))
                                .into_any_element()
                        }
                    };
                    row(&theme)
                        .h(px(ROW_H))
                        .child(icon("box", 14., theme.text_placeholder))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(clipped(format!("img-name-{n}"), i.name.clone(), 13., theme.text, text_w))
                                .child(clipped(format!("img-id-{n}"), i.id.clone(), 11.5, theme.text_placeholder, text_w)),
                        )
                        .child(div().flex_none().w(px(80.)).flex().justify_end().text_size(px(12.)).text_color(theme.text_muted).child(i.size.clone()))
                        .child(div().flex_none().w(px(W_STATE)).pl(px(12.)).text_size(px(12.)).text_color(theme.text_placeholder).child(i.created.clone()))
                        .child(div().flex_none().w(px(240.)).flex().justify_end().child(actions))
                })
                .collect()
        })
        .track_scroll(self.scroll.clone())
        .flex_1()
        .into_any_element()
    }
}
