//! Modal dialogs: add a machine, install runners on one, confirm a removal.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription,
    Window, div, prelude::*, px,
};

use super::input::{InputEvent, TextInput};
use super::kit::{Button, ButtonStyle, checkbox, icon, label};
use super::theme::{ActiveTheme, mono_font, Theme};
use crate::fleet::Engine;
use crate::fleet::workspaces::{CreateWorkspace, DEFAULT_IMAGE, WorkspaceIsolation};
use crate::fleet::model::Snapshot;
use crate::ops::{InstallRequest, NewMachine, ssh_hosts};

pub enum DialogEvent {
    Dismissed,
    OperationStarted(u64),
    MachineAdded(String),
    OpenSettings,
    /// The root view runs it, so it can open the workspace when it's ready.
    CreateWorkspace(CreateWorkspace, WorkspaceIsolation),
}

fn frame(theme: &Theme, title: impl Into<SharedString>, intro: Option<SharedString>) -> gpui::Div {
    div()
        .w(px(500.))
        .flex()
        .flex_col()
        .gap(px(14.))
        .p(px(18.))
        .rounded(px(10.))
        .bg(theme.elevated)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .text_color(theme.text)
        .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(title.into()))
        .children(intro.map(|t| div().text_size(px(12.5)).text_color(theme.text_muted).child(t)))
}

fn field(theme: &Theme, name: &'static str, input: impl IntoElement, help: Option<&'static str>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(label(theme, name).text_size(px(12.)).text_color(theme.text_muted))
        .child(input)
        .children(help.map(|h| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(h)))
}

fn error_line(theme: &Theme, error: &Option<String>) -> Option<impl IntoElement> {
    error.as_ref().map(|e| {
        div()
            .flex()
            .items_start()
            .gap(px(8.))
            .p(px(10.))
            .rounded(px(6.))
            .bg(theme.error.opacity(0.1))
            .text_size(px(12.5))
            .text_color(theme.text)
            .child(icon("circle-alert", 14., theme.error))
            .child(div().flex_1().child(e.clone()))
    })
}

fn footer(children: Vec<AnyElement>) -> impl IntoElement {
    div().flex().justify_end().gap(px(8.)).pt(px(4.)).children(children)
}

// ---------------------------------------------------------------------------

pub struct AddMachineDialog {
    engine: Arc<Engine>,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    hosts: Vec<String>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for AddMachineDialog {}

impl AddMachineDialog {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "Mac Studio"));
        let host = cx.new(|cx| TextInput::new(cx, "ci@mac-studio.local, an ~/.ssh/config alias, or local").mono());
        let on_input = |this: &mut Self, _: &Entity<TextInput>, e: &InputEvent, _: &mut Window, cx: &mut Context<Self>| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => {
                this.error = None;
                cx.notify()
            }
        };
        let subs = vec![cx.subscribe_in(&name, window, on_input), cx.subscribe_in(&host, window, on_input)];
        AddMachineDialog { engine, name, host, hosts: ssh_hosts(), error: None, _subscriptions: subs }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let machine = NewMachine { name: self.name.read(cx).text().to_string(), host: self.host.read(cx).text().to_string() };
        match self.engine.add_machine(machine) {
            Ok(id) => cx.emit(DialogEvent::MachineAdded(id)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn pick(&mut self, host: String, name: String, cx: &mut Context<Self>) {
        self.host.update(cx, |h, cx| h.set_text(host, cx));
        if self.name.read(cx).text().trim().is_empty() {
            self.name.update(cx, |n, cx| n.set_text(name, cx));
        }
    }
}

impl Render for AddMachineDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut suggestions: Vec<AnyElement> = vec![
            suggestion(&theme, "sugg-local", "This computer", "laptop")
                .on_click(cx.listener(|this, _, _, cx| this.pick("local".into(), crate::config::local_hostname(), cx)))
                .into_any_element(),
        ];
        for (i, h) in self.hosts.iter().take(12).enumerate() {
            let host = h.clone();
            suggestions.push(
                suggestion(&theme, SharedString::from(format!("sugg-{i}")), h.clone(), "terminal")
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(host.clone(), host.clone(), cx)))
                    .into_any_element(),
            );
        }
        frame(
            &theme,
            "Add a machine",
            Some("OpenRunner reaches machines with your system ssh, keys and ~/.ssh/config. Checks are read-only; nothing is installed until you ask.".into()),
        )
        .child(field(&theme, "Name", self.name.clone(), None))
        .child(field(&theme, "SSH host", self.host.clone(), Some("Key-based login must work without a password prompt.")))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(label(&theme, "Suggestions"))
                .child(div().flex().flex_wrap().gap(px(6.)).children(suggestions)),
        )
        .children(error_line(&theme, &self.error))
        .child(footer(vec![
            Button::new("am-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
            Button::new("am-add", "Add machine").style(ButtonStyle::Primary).key_hint("↵").on_click(cx.listener(|this, _, _, cx| this.submit(cx))).into_any_element(),
        ]))
    }
}

fn suggestion(theme: &Theme, id: impl Into<gpui::ElementId>, text: impl Into<SharedString>, icon_name: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(5.))
        .h(px(24.))
        .px(px(8.))
        .rounded(px(5.))
        .border_1()
        .border_color(theme.border_variant)
        .bg(theme.element)
        .cursor_pointer()
        .hover(|st| st.bg(theme.element_hover))
        .text_size(px(12.))
        .font_family(mono_font())
        .text_color(theme.text_muted)
        .child(icon(icon_name, 12., theme.text_placeholder))
        .child(text.into())
}

// ---------------------------------------------------------------------------

pub struct InstallDialog {
    engine: Arc<Engine>,
    machine_id: String,
    scopes: Vec<String>,
    scope: Option<String>,
    count: usize,
    names: Entity<TextInput>,
    labels: Entity<TextInput>,
    mode: ModeChoice,
    image: Entity<TextInput>,
    docker_socket: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for InstallDialog {}

/// How the runner process is kept alive on the machine.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModeChoice {
    Auto,
    Service,
    Docker,
    Background,
}

impl InstallDialog {
    pub fn new(engine: Arc<Engine>, snapshot: Arc<Snapshot>, machine_id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = engine.settings();
        let mut scopes: Vec<String> = settings.github.orgs.iter().chain(settings.github.repos.iter()).cloned().collect();
        for d in &snapshot.github.detected {
            if !scopes.iter().any(|s| s.eq_ignore_ascii_case(d)) {
                scopes.push(d.clone());
            }
        }
        let suggested = engine.suggest_runner_names(&machine_id, 1).join(", ");
        let names = cx.new(|cx| TextInput::new(cx, "runner-01, runner-02").mono().with_text(suggested));
        let labels = cx.new(|cx| TextInput::new(cx, "gpu, xcode-16 (optional)").mono());
        let image = cx.new(|cx| TextInput::new(cx, "ghcr.io/actions/actions-runner (latest)").mono());
        let on_input = |this: &mut Self, _: &Entity<TextInput>, e: &InputEvent, _: &mut Window, cx: &mut Context<Self>| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => {
                this.error = None;
                cx.notify()
            }
        };
        let subs = vec![cx.subscribe_in(&names, window, on_input), cx.subscribe_in(&labels, window, on_input)];
        InstallDialog {
            engine,
            machine_id,
            scope: scopes.first().cloned(),
            scopes,
            count: 1,
            names,
            labels,
            mode: ModeChoice::Auto,
            image,
            docker_socket: false,
            error: None,
            _subscriptions: subs,
        }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.names.focus_handle(cx)
    }

    fn set_count(&mut self, count: usize, cx: &mut Context<Self>) {
        self.count = count.clamp(1, 16);
        let names = self.engine.suggest_runner_names(&self.machine_id, self.count).join(", ");
        self.names.update(cx, |n, cx| n.set_text(names, cx));
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.scope.clone() else {
            self.error = Some("Choose where to register the runners.".into());
            cx.notify();
            return;
        };
        let split = |s: &str| -> Vec<String> {
            s.split([',', ' ', '\n']).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
        };
        let req = InstallRequest {
            machine_id: self.machine_id.clone(),
            scope,
            names: split(self.names.read(cx).text()),
            labels: split(self.labels.read(cx).text()),
            group: None,
            as_service: self.mode != ModeChoice::Background,
            mode: match self.mode {
                ModeChoice::Auto => crate::ops::InstallMode::Auto,
                ModeChoice::Service => crate::ops::InstallMode::Service,
                ModeChoice::Background => crate::ops::InstallMode::Background,
                ModeChoice::Docker => crate::ops::InstallMode::Docker {
                    image: Some(self.image.read(cx).text().trim().to_string()).filter(|i| !i.is_empty()),
                    docker_socket: self.docker_socket,
                },
            },
        };
        match self.engine.install_runners(req) {
            Ok(op) => cx.emit(DialogEvent::OperationStarted(op)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }
}

impl Render for InstallDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let snapshot = self.engine.current();
        let machine = snapshot.machines.iter().find(|m| m.id == self.machine_id);
        let machine_name = machine.map(|m| m.name.clone()).unwrap_or_else(|| self.machine_id.clone());
        let platform = machine.map(|m| m.platform.clone()).filter(|p| !p.is_empty());
        let linux = platform.as_deref().is_some_and(|p| p.starts_with("linux"));

        let scope_picker: AnyElement = if self.scopes.is_empty() {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .p(px(10.))
                .rounded(px(6.))
                .bg(theme.warning.opacity(0.1))
                .text_size(px(12.5))
                .child(icon("triangle-alert", 14., theme.warning))
                .child(div().flex_1().child("Runners register with a GitHub organization or repository. Add one in Settings first."))
                .child(Button::new("inst-settings", "Settings").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::OpenSettings))))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .children(self.scopes.iter().enumerate().map(|(i, sc)| {
                    let selected = self.scope.as_deref() == Some(sc.as_str());
                    let value = sc.clone();
                    div()
                        .id(("inst-scope", i))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .h(px(26.))
                        .px(px(10.))
                        .rounded(px(6.))
                        .border_1()
                        .cursor_pointer()
                        .border_color(if selected { theme.accent } else { theme.border_variant })
                        .bg(if selected { theme.accent.opacity(0.12) } else { theme.element })
                        .text_size(px(12.5))
                        .font_family(mono_font())
                        .child(icon(if sc.contains('/') { "git-branch" } else { "layers" }, 12., if selected { theme.accent } else { theme.text_placeholder }))
                        .child(sc.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.scope = Some(value.clone());
                            cx.notify()
                        }))
                }))
                .into_any_element()
        };

        let count = self.count;
        let stepper = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(Button::icon_only("inst-minus", "minus").disabled(count <= 1).on_click(cx.listener(move |this, _, _, cx| this.set_count(count - 1, cx))))
            .child(div().w(px(28.)).flex().justify_center().font_family(mono_font()).child(count.to_string()))
            .child(Button::icon_only("inst-plus", "plus").disabled(count >= 16).on_click(cx.listener(move |this, _, _, cx| this.set_count(count + 1, cx))));

        let mode_picker = self.mode_picker(&theme, machine, linux, cx);

        frame(
            &theme,
            format!("Install runners on {machine_name}"),
            Some(
                match &platform {
                    Some(p) => format!("Downloads the latest GitHub Actions runner for {p}, registers it with GitHub and starts it."),
                    None => "The machine is checked first to pick the right runner build, then it's downloaded, registered and started.".into(),
                }
                .into(),
            ),
        )
        .child(field(&theme, "Register with", scope_picker, None))
        .child(
            div()
                .flex()
                .gap(px(14.))
                .child(div().flex_none().child(field(&theme, "How many", stepper, None)))
                .child(div().flex_1().child(field(&theme, "Names", self.names.clone(), None))),
        )
        .child(field(&theme, "Extra labels", self.labels.clone(), Some("GitHub adds self-hosted, the OS and the architecture automatically.")))
        .child(field(&theme, "Run as", mode_picker, None))
        .children(error_line(&theme, &self.error))
        .child(footer(vec![
            Button::new("inst-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
            Button::new("inst-go", if count == 1 { "Install runner".to_string() } else { format!("Install {count} runners") })
                .style(ButtonStyle::Primary)
                .disabled(self.scope.is_none())
                .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                .into_any_element(),
        ]))
    }
}

impl InstallDialog {
    /// Auto / Service / Docker / Background, with what the last check says about the machine.
    fn mode_picker(&self, theme: &Theme, machine: Option<&crate::fleet::model::Machine>, linux: bool, cx: &mut Context<Self>) -> AnyElement {
        let probe = machine.and_then(|m| m.probe.as_ref()).filter(|p| p.ok);
        let docker = probe.and_then(|p| p.docker.as_deref()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let sudo_ok = probe.and_then(|p| p.sudo.as_deref()).is_some_and(|s| matches!(s, "root" | "passwordless"));
        let service_works = !linux || sudo_ok;
        let auto_pick = if service_works {
            "a service"
        } else if docker.is_some() {
            "Docker"
        } else {
            "a background process"
        };
        let options: [(ModeChoice, &'static str, &'static str, String, bool); 4] = [
            (ModeChoice::Auto, "Auto", "zap", format!("Best fit for this machine: {auto_pick}."), true),
            (
                ModeChoice::Service,
                "Service",
                "server",
                if !linux {
                    "Starts at boot (launchd on macOS, a service on Windows).".into()
                } else if sudo_ok {
                    "systemd service; starts at boot.".into()
                } else {
                    "systemd needs passwordless sudo, which this machine doesn't allow.".into()
                },
                service_works,
            ),
            (
                ModeChoice::Docker,
                "Docker",
                "box",
                match &docker {
                    Some(v) => format!("Docker {v}: a container per runner that restarts with Docker. No sudo."),
                    None => "Docker wasn't found at the last check.".into(),
                },
                docker.is_some(),
            ),
            (ModeChoice::Background, "Background", "play", "No root needed, but stops when the machine restarts.".into(), true),
        ];
        let card = |(choice, title, icon_name, detail, available): (ModeChoice, &'static str, &'static str, String, bool), cx: &mut Context<Self>| {
            let selected = self.mode == choice;
            div()
                .id(title)
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_start()
                .gap(px(8.))
                .p(px(9.))
                .rounded(px(8.))
                .border_1()
                .cursor_pointer()
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .when(!available, |d| d.opacity(0.6))
                .child(div().pt(px(1.)).child(icon(icon_name, 13., if selected { theme.accent } else { theme.text_placeholder })))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_size(px(11.)).text_color(theme.text_placeholder).child(detail)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mode = choice;
                    cx.notify()
                }))
                .into_any_element()
        };
        let [a, b, c, d] = options;
        let row1 = div().flex().gap(px(8.)).child(card(a, cx)).child(card(b, cx));
        let row2 = div().flex().gap(px(8.)).child(card(c, cx)).child(card(d, cx));
        let socket = self.docker_socket;
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(row1)
            .child(row2)
            .when(self.mode == ModeChoice::Docker, |el| {
                el.child(self.image.clone()).child(checkbox(
                    theme,
                    "inst-docker-socket",
                    socket,
                    "Let jobs use the machine's Docker (mounts the socket; root-equivalent)",
                    cx.listener(|this, value: &bool, _, cx| {
                        this.docker_socket = *value;
                        cx.notify()
                    }),
                ))
            })
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------

type ConfirmAction = Box<dyn Fn(bool, &Arc<Engine>) -> Result<Option<u64>, String>>;

pub struct ConfirmDialog {
    engine: Arc<Engine>,
    title: String,
    detail: String,
    confirm_label: &'static str,
    option_label: Option<&'static str>,
    option: bool,
    action: ConfirmAction,
    error: Option<String>,
    focus: FocusHandle,
}

impl EventEmitter<DialogEvent> for ConfirmDialog {}

impl ConfirmDialog {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        title: String,
        detail: String,
        confirm_label: &'static str,
        option_label: Option<&'static str>,
        action: impl Fn(bool, &Arc<Engine>) -> Result<Option<u64>, String> + 'static,
        engine: Arc<Engine>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        ConfirmDialog {
            engine,
            title,
            detail,
            confirm_label,
            option_label,
            option: false,
            action: Box::new(action),
            error: None,
            focus: cx.focus_handle(),
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        match (self.action)(self.option, &self.engine) {
            Ok(Some(op)) => cx.emit(DialogEvent::OperationStarted(op)),
            Ok(None) => cx.emit(DialogEvent::Dismissed),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }
}

impl Focusable for ConfirmDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ConfirmDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let option = self.option;
        frame(&theme, self.title.clone(), Some(self.detail.clone().into()))
            .track_focus(&self.focus)
            .children(self.option_label.map(|l| {
                checkbox(&theme, "confirm-option", option, l, cx.listener(|this, value: &bool, _, cx| {
                    this.option = *value;
                    cx.notify()
                }))
            }))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                Button::new("cf-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                Button::new("cf-go", self.confirm_label)
                    .style(ButtonStyle::Outlined)
                    .icon("trash-2")
                    .on_click(cx.listener(|this, _, _, cx| this.confirm(cx)))
                    .into_any_element(),
            ]))
    }
}

// ---------------------------------------------------------------------------

pub struct NewWorkspaceDialog {
    engine: Arc<Engine>,
    /// None = let OpenRunner pick the least busy machine.
    machine: Option<String>,
    name: Entity<TextInput>,
    repo: Entity<TextInput>,
    git_ref: Entity<TextInput>,
    branch: Entity<TextInput>,
    /// Run inside a fresh Docker container instead of directly on the machine.
    docker: bool,
    image: Entity<TextInput>,
    docker_socket: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for NewWorkspaceDialog {}

/// Common images agents ask for; any image name works.
const IMAGE_PRESETS: &[&str] = &["node:22", "python:3.13", "rust:1", "golang:1.25", DEFAULT_IMAGE];

impl NewWorkspaceDialog {
    pub fn new(engine: Arc<Engine>, machine: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "api-tests (optional)"));
        let repo = cx.new(|cx| TextInput::new(cx, "acme/api or a git URL (optional)").mono());
        let git_ref = cx.new(|cx| TextInput::new(cx, "main").mono());
        let branch = cx.new(|cx| TextInput::new(cx, "new branch (optional)").mono());
        let image = cx.new(|cx| TextInput::new(cx, DEFAULT_IMAGE).mono());
        let on_input = |_: &mut Self, _: &Entity<TextInput>, e: &InputEvent, _: &mut Window, cx: &mut Context<Self>| match e {
            InputEvent::Submit => cx.emit(DialogEvent::Dismissed),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => cx.notify(),
        };
        let subs = [&name, &repo, &git_ref, &branch, &image].into_iter().map(|i| cx.subscribe_in(i, window, on_input)).collect();
        NewWorkspaceDialog { engine, machine, name, repo, git_ref, branch, docker: false, image, docker_socket: false, _subscriptions: subs }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.repo.focus_handle(cx)
    }

    pub fn set_docker(&mut self, docker: bool, cx: &mut Context<Self>) {
        self.docker = docker;
        cx.notify();
    }

    /// "On the machine" vs "In a Docker container", with the image picker for the latter.
    fn environment_picker(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let option = |id: &'static str, docker: bool, icon_name: &'static str, title: &'static str, detail: &'static str, cx: &mut Context<Self>| {
            let selected = self.docker == docker;
            div()
                .id(id)
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_start()
                .gap(px(10.))
                .p(px(10.))
                .rounded(px(8.))
                .border_1()
                .cursor_pointer()
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .child(div().pt(px(1.)).child(icon(icon_name, 14., if selected { theme.accent } else { theme.text_placeholder })))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(detail)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.docker = docker;
                    cx.notify()
                }))
        };
        let host = option("env-host", false, "server", "On the machine", "Runs as your ssh user with its tools.", cx);
        let docker = option("env-docker", true, "box", "In a Docker container", "A fresh, isolated container per workspace.", cx);
        let socket = self.docker_socket;
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(div().flex().gap(px(8.)).child(host).child(docker))
            .when(self.docker, |d| {
                d.child(self.image.clone())
                    .child(div().flex().flex_wrap().gap(px(6.)).children(IMAGE_PRESETS.iter().enumerate().map(|(i, img)| {
                        let value = img.to_string();
                        div()
                            .id(("img-preset", i))
                            .h(px(22.))
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.border_variant)
                            .bg(theme.element)
                            .cursor_pointer()
                            .hover(|st| st.bg(theme.element_hover))
                            .font_family(mono_font())
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .child(*img)
                            .on_click(cx.listener(move |this, _, _, cx| this.image.update(cx, |i, cx| i.set_text(value.clone(), cx))))
                    })))
                    .child(checkbox(theme, "ws-docker-socket", socket, "Share the machine's Docker socket (docker-in-docker)", cx.listener(|this, value: &bool, _, cx| {
                        this.docker_socket = *value;
                        cx.notify()
                    })))
            })
            .into_any_element()
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = |e: &Entity<TextInput>, cx: &Context<Self>| Some(e.read(cx).text().trim().to_string()).filter(|t| !t.is_empty());
        let iso = if self.docker {
            WorkspaceIsolation { isolation: Some("docker".into()), image: text(&self.image, cx), docker_socket: self.docker_socket }
        } else {
            WorkspaceIsolation::default()
        };
        cx.emit(DialogEvent::CreateWorkspace(
            CreateWorkspace {
                machine: self.machine.clone(),
                platform: None,
                name: text(&self.name, cx),
                repo: text(&self.repo, cx),
                git_ref: text(&self.git_ref, cx),
                branch: text(&self.branch, cx),
                owner: Some("app".into()),
            },
            iso,
        ));
    }
}

impl Render for NewWorkspaceDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let snapshot = self.engine.current();
        let choices: Vec<(Option<String>, String, bool)> = std::iter::once((None, "Least busy".to_string(), true))
            .chain(snapshot.machines.iter().filter(|m| m.workspaces_allowed).map(|m| (Some(m.id.clone()), m.name.clone(), m.status == "online")))
            .collect();
        let picker = div().flex().flex_wrap().gap(px(6.)).children(choices.into_iter().enumerate().map(|(i, (id, label, online))| {
            let selected = self.machine == id;
            div()
                .id(("ws-machine", i))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .px(px(10.))
                .rounded(px(6.))
                .border_1()
                .cursor_pointer()
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.12) } else { theme.element })
                .text_size(px(12.5))
                .child(if id.is_none() {
                    icon("zap", 12., if selected { theme.accent } else { theme.text_placeholder }).into_any_element()
                } else {
                    div().size(px(7.)).rounded_full().bg(if online { theme.success } else { theme.text_placeholder }).into_any_element()
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.machine = id.clone();
                    cx.notify()
                }))
        }));
        frame(
            &theme,
            "New workspace",
            Some("A folder on one of your machines, optionally a git checkout, for an agent (or you) to work in. It stays until you destroy it.".into()),
        )
        .child(field(&theme, "Machine", picker, None))
        .child(field(&theme, "Environment", self.environment_picker(&theme, cx), None))
        .child(field(&theme, "Repository", self.repo.clone(), Some("Private GitHub repos clone with your gh login if the machine can't.")))
        .child(
            div()
                .flex()
                .gap(px(12.))
                .child(div().flex_1().child(field(&theme, "Check out", self.git_ref.clone(), None)))
                .child(div().flex_1().child(field(&theme, "Branch", self.branch.clone(), None))),
        )
        .child(field(&theme, "Name", self.name.clone(), None))
        .child(footer(vec![
            Button::new("nw-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
            Button::new("nw-create", "Create workspace")
                .style(ButtonStyle::Primary)
                .icon("terminal")
                .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                .into_any_element(),
        ]))
    }
}
