//! Modal dialogs: add a machine, install runners on one, confirm a removal.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription,
    Window, div, prelude::*, px,
};

use super::github::Load;
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, checkbox, icon, label};
use super::theme::{ActiveTheme, mono_font, Theme};
use crate::fleet::Engine;
use crate::fleet::files::Listing;
use crate::fleet::sessions::{AGENTS, StartSession};
use crate::fleet::workspaces::{CreateWorkspace, DEFAULT_IMAGE, WorkspaceIsolation};
use crate::fleet::model::Snapshot;
use crate::ops::{InstallRequest, NewMachine, ssh_hosts};

pub enum DialogEvent {
    Dismissed,
    OperationStarted(u64),
    MachineAdded(String),
    OpenSettings,
    /// Open the New cloud machine dialog instead.
    NewCloudMachine,
    /// Open the New VM dialog instead.
    NewVm,
    /// A schedule was saved: show the schedules again.
    ScheduleSaved,
    OpenCloudSettings,
    /// The root view runs it, so it can open the workspace when it's ready.
    CreateWorkspace(CreateWorkspace, WorkspaceIsolation),
    /// The root view starts it, sized to the page, and opens its terminal.
    StartSession(StartSession),
    /// Set up the workspace first, then start the session in its folder.
    StartInWorkspace(CreateWorkspace, StartSession),
    /// Make a VM first, then (a workspace in it and) the session there.
    StartInNewVm(crate::ops::vm::VmCreate, Option<CreateWorkspace>, StartSession),
    /// A virtual monitor was added: (machine, display) to show.
    DisplayAdded(String, String),
    /// A share joined and let in: open its session.
    JoinedShare(super::share::JoinedHandle),
    /// Open the Join dialog instead (with a link pasted here).
    JoinShare(Option<String>),
}

pub(super) fn frame(theme: &Theme, title: impl Into<SharedString>, intro: Option<SharedString>) -> gpui::Div {
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

pub(super) fn field(theme: &Theme, name: &'static str, input: impl IntoElement, help: Option<&'static str>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(label(theme, name).text_size(px(12.)).text_color(theme.text_muted))
        .child(input)
        .children(help.map(|h| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(h)))
}

pub(super) fn error_line(theme: &Theme, error: &Option<String>) -> Option<impl IntoElement> {
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
            .child(div().flex_1().min_w(px(0.)).child(e.clone()))
    })
}

pub(super) fn footer(children: Vec<AnyElement>) -> impl IntoElement {
    div().flex().justify_end().gap(px(8.)).pt(px(4.)).children(children)
}

// ---------------------------------------------------------------------------

pub struct AddMachineDialog {
    engine: Arc<Engine>,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    hosts: Vec<String>,
    /// Computers on the user's tailnet that aren't in Muxin yet.
    tailnet: Vec<crate::api::TailnetHost>,
    /// Computers on this network that aren't in Muxin yet; still looking while None.
    network: Option<Vec<crate::fleet::discover::NetworkHost>>,
    /// What to do before a picked machine can be added (a Mac with Remote Login off).
    note: Option<String>,
    /// A share link pasted where a machine goes: joined instead.
    share_link: Option<String>,
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
                this.share_link = [&this.name, &this.host].into_iter().map(|i| i.read(cx).text().trim().to_string()).find(|t| crate::share::ticket::Ticket::parse(t).is_ok());
                cx.notify()
            }
        };
        let subs = vec![cx.subscribe_in(&name, window, on_input), cx.subscribe_in(&host, window, on_input)];
        // The tailnet takes a moment (Tailscale, and where each alias goes).
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { crate::api::ssh_hosts(&engine).await }
        });
        cx.spawn(async move |this, cx| {
            if let Ok(found) = job.await {
                let _ = this.update(cx, |this, cx| {
                    // The demo's own aliases, not this computer's.
                    if this.engine.demo_world().is_some() {
                        this.hosts = found.hosts.clone();
                    }
                    this.tailnet = found.tailnet.into_iter().filter(|t| t.machine_id.is_none()).collect();
                    this.network = Some(found.network);
                    cx.notify();
                });
            }
        })
        .detach();
        let hosts = if engine.demo_world().is_some() { Vec::new() } else { ssh_hosts() };
        AddMachineDialog { engine, name, host, hosts, tailnet: Vec::new(), network: None, note: None, share_link: None, error: None, _subscriptions: subs }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if let Some(link) = self.share_link.clone() {
            cx.emit(DialogEvent::JoinShare(Some(link)));
            return;
        }
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
        self.note = None;
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
        // A tailnet computer shows once, below, under the user's alias for it.
        let on_tailnet: Vec<&str> = self.tailnet.iter().map(|t| t.host.as_str()).collect();
        for (i, h) in self.hosts.iter().filter(|h| !on_tailnet.contains(&h.as_str())).take(12).enumerate() {
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
            Some("Muxin reaches machines with your system ssh, keys and ~/.ssh/config. Checks are read-only; nothing is installed until you ask.".into()),
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
        .when(!self.tailnet.is_empty(), |d| {
            let chips: Vec<AnyElement> = self
                .tailnet
                .iter()
                .take(16)
                .enumerate()
                .map(|(i, t)| {
                    let glyph = match t.os.to_ascii_lowercase().as_str() {
                        "macos" => "laptop",
                        "windows" => "monitor",
                        _ => "server",
                    };
                    let (host, name) = (t.host.clone(), t.name.clone());
                    suggestion(&theme, SharedString::from(format!("sugg-ts-{i}")), t.host.clone(), glyph)
                        .child(kit::dot(if t.online { theme.success } else { theme.text_placeholder }, 6.))
                        .on_click(cx.listener(move |this, _, _, cx| this.pick(host.clone(), name.clone(), cx)))
                        .into_any_element()
                })
                .collect();
            d.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(label(&theme, "On your tailnet"))
                    .child(div().flex().flex_wrap().gap(px(6.)).children(chips)),
            )
        })
        .child({
            // What's on this network (Bonjour): ready to add, or a Mac that needs Remote Login.
            let chips: Vec<AnyElement> = self
                .network
                .iter()
                .flatten()
                .take(16)
                .enumerate()
                .map(|(i, n)| {
                    let (host, name, ssh) = (n.host.clone(), n.name.clone(), n.ssh);
                    let label = if ssh { n.host.clone() } else { format!("{} · Remote Login off", n.name) };
                    suggestion(&theme, SharedString::from(format!("sugg-net-{i}")), label, if n.mac { "laptop" } else { "server" })
                        .when(!ssh, |d| d.text_color(theme.text_placeholder))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick(host.clone(), name.clone(), cx);
                            if !ssh {
                                this.note = Some(format!("Turn on Remote Login on {name}: Settings › General › Sharing."));
                            }
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect();
            let body = match &self.network {
                None => div().flex().items_center().gap(px(6.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(kit::spinner("net-spin", 11., theme.text_placeholder)).child("Looking on this network…").into_any_element(),
                Some(n) if n.is_empty() => div().text_size(px(11.5)).text_color(theme.text_placeholder).child("Nothing else found. Macs show up once Remote Login is on.").into_any_element(),
                Some(_) => div().flex().flex_wrap().gap(px(6.)).children(chips).into_any_element(),
            };
            div().flex().flex_col().gap(px(6.)).child(label(&theme, "On this network")).child(body)
        })
        .children(self.note.clone().map(|n| {
            div().flex().items_start().gap(px(8.)).text_size(px(12.)).text_color(theme.text_muted).child(icon("info", 13., theme.info)).child(div().flex_1().min_w(px(0.)).child(n))
        }))
        // Someone else's machine comes by link: a way there, or the link pasted here.
        .child({
            let (text, button) = match &self.share_link {
                Some(_) => ("That's a share link: join it instead.", Button::new("am-join", "Join").style(ButtonStyle::Primary)),
                None => ("Someone sent you a link to their machine or session?", Button::new("am-join", "Join a share…").style(ButtonStyle::Outlined)),
            };
            let link = self.share_link.clone();
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(9.))
                .rounded(px(9.))
                .border_1()
                .border_color(if link.is_some() { theme.accent } else { theme.border_variant })
                .bg(theme.surface)
                .child(icon("link", 14., theme.text_muted))
                .child(div().flex_1().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text_muted).child(text))
                .child(button.icon("link").on_click(cx.listener(move |_, _, _, cx| cx.emit(DialogEvent::JoinShare(link.clone())))))
        })
        .children(error_line(&theme, &self.error))
        .child(footer(vec![
            Button::new("am-cloud", "New cloud machine…").style(ButtonStyle::Ghost).icon("cloud").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::NewCloudMachine))).into_any_element(),
            Button::new("am-vm", "New VM…").style(ButtonStyle::Ghost).icon("box").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::NewVm))).into_any_element(),
            div().flex_1().into_any_element(),
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
        .child(kit::color_icon(theme, icon_name, 12., theme.text_placeholder))
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
    /// Fills in the extra labels ("gpu, cuda").
    pub fn set_labels(&self, labels: String, cx: &mut Context<Self>) {
        self.labels.update(cx, |i, cx| i.set_text(labels, cx));
    }

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
                .child(div().flex_1().min_w(px(0.)).child("Runners register with a GitHub organization or repository. Add one in Settings first."))
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
                        .child(kit::color_icon(&theme, if sc.contains('/') { "git-branch" } else { "layers" }, 12., theme.text_placeholder))
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
                .child(kit::color_tile(icon_name, 20.))
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

/// What a rename dialog renames, by id.
#[derive(Clone)]
pub enum RenameTarget {
    Machine(String),
    Session(String),
    /// A machine's display: (machine, display id).
    Display(String, String),
}

pub struct RenameDialog {
    engine: Arc<Engine>,
    target: RenameTarget,
    /// Where it lives, for the intro ("Beelink").
    machine: String,
    name: Entity<TextInput>,
    saving: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for RenameDialog {}

impl RenameDialog {
    pub fn new(engine: Arc<Engine>, target: RenameTarget, current: String, machine: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, current.clone()).with_text(current).selected());
        let sub = cx.subscribe_in(&name, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => {
                this.error = None;
                cx.notify()
            }
        });
        RenameDialog { engine, target, machine, name, saving: false, error: None, _subscriptions: vec![sub] }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let text = self.name.read(cx).text().trim().to_string();
        if text.is_empty() {
            self.error = Some("Give it a name.".into());
            cx.notify();
            return;
        }
        match self.target.clone() {
            RenameTarget::Machine(id) => match self.engine.rename_machine(&id, &text) {
                Ok(()) => cx.emit(DialogEvent::Dismissed),
                Err(e) => {
                    self.error = Some(e);
                    cx.notify();
                }
            },
            // The machine keeps it: one round trip.
            RenameTarget::Display(machine, display) => {
                self.saving = true;
                cx.notify();
                let engine = self.engine.clone();
                let job = engine.runtime().spawn({
                    let engine = engine.clone();
                    async move { engine.rename_display(&machine, &display, &text).await }
                });
                cx.spawn(async move |this, cx| {
                    let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
                    let _ = this.update(cx, |this, cx| match result {
                        Ok(()) => cx.emit(DialogEvent::Dismissed),
                        Err(e) => {
                            this.saving = false;
                            this.error = Some(e);
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            // The agent on the machine keeps it: one round trip.
            RenameTarget::Session(id) => {
                self.saving = true;
                cx.notify();
                let engine = self.engine.clone();
                let job = engine.runtime().spawn({
                    let engine = engine.clone();
                    async move { engine.rename_session(&id, &text).await }
                });
                cx.spawn(async move |this, cx| {
                    let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
                    let _ = this.update(cx, |this, cx| match result {
                        Ok(_) => cx.emit(DialogEvent::Dismissed),
                        Err(e) => {
                            this.saving = false;
                            this.error = Some(e);
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
    }
}

impl Render for RenameDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (title, intro, field_name) = match &self.target {
            RenameTarget::Machine(id) => (
                "Rename machine",
                format!("Only the name Muxin shows changes. Its id ({id}), runners and sessions stay the same, and nothing on the machine changes."),
                "Name",
            ),
            RenameTarget::Display(..) => (
                "Rename display",
                format!("What Muxin calls this monitor on {}. Its windows and settings stay as they are.", self.machine),
                "Name",
            ),
            RenameTarget::Session(_) => (
                "Rename session",
                format!("Muxin's agent on {} keeps the title, so it shows wherever you use Muxin. What runs in the session isn't touched.", self.machine),
                "Title",
            ),
        };
        frame(&theme, title, Some(intro.into()))
            .child(field(&theme, field_name, self.name.clone(), None))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                Button::new("rn-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                Button::new("rn-save", if self.saving { "Saving…" } else { "Rename" })
                    .style(ButtonStyle::Primary)
                    .loading(self.saving)
                    .key_hint("↵")
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                    .into_any_element(),
            ]))
    }
}

// ---------------------------------------------------------------------------

/// Sizes to pick from for a virtual monitor.
const MONITOR_SIZES: &[(u32, u32)] = &[(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];

pub struct AddDisplayDialog {
    engine: Arc<Engine>,
    machine: String,
    machine_name: String,
    name: Entity<TextInput>,
    /// The machine's password, when it asks for one (cleared once read).
    password: Entity<TextInput>,
    size: (u32, u32),
    /// The size of the machine's main monitor, which a new one starts at.
    main: Option<(u32, u32)>,
    adding: bool,
    /// The machine wants the password (sudo, on Linux): ask for it.
    asking: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for AddDisplayDialog {}

impl AddDisplayDialog {
    pub fn new(engine: Arc<Engine>, machine: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snap = engine.current();
        let m = snap.machines.iter().find(|m| m.id == machine);
        let machine_name = m.map(|m| m.name.clone()).unwrap_or_else(|| machine.clone());
        // Named like plugging in another one: "Display 3" when there are two.
        let n = m.map(|m| m.displays.iter().filter(|d| d.id != "all").count()).unwrap_or(1) + 1;
        let screens: Vec<_> = m.map(|m| m.displays.iter().filter(|d| d.id != "all").collect()).unwrap_or_default();
        let main = screens.iter().find(|d| d.main).or(screens.first()).map(|d| (d.width.round() as u32, d.height.round() as u32)).filter(|(w, h)| *w >= 640 && *h >= 480);
        let name = cx.new(|cx| TextInput::new(cx, format!("Display {n}")));
        let password = cx.new(|cx| TextInput::new(cx, "Password").secret());
        let on_input = |this: &mut Self, _: &Entity<TextInput>, e: &InputEvent, window: &mut Window, cx: &mut Context<Self>| match e {
            InputEvent::Submit => this.submit(window, cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => {
                this.error = None;
                cx.notify()
            }
        };
        let subs = vec![cx.subscribe_in(&name, window, on_input), cx.subscribe_in(&password, window, on_input)];
        AddDisplayDialog { engine, machine, machine_name, name, password, size: main.unwrap_or((1920, 1080)), main, adding: false, asking: false, error: None, _subscriptions: subs }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.adding {
            return;
        }
        if self.asking {
            // Read once, cleared from the field, kept only in memory while Muxin runs.
            let p = self.password.read(cx).text().to_string();
            if p.is_empty() {
                self.error = Some(format!("Type your password on {}.", self.machine_name));
                cx.notify();
                return;
            }
            self.password.update(cx, |i, cx| i.set_text("", cx));
            self.engine.remember_password(&self.machine, p);
        }
        self.adding = true;
        self.error = None;
        cx.notify();
        let typed = self.name.read(cx).text().trim().to_string();
        let (engine, machine, (w, h)) = (self.engine.clone(), self.machine.clone(), self.size);
        let job = engine.runtime().spawn({
            let (engine, machine) = (engine.clone(), machine.clone());
            async move { engine.add_display(&machine, w, h, Some(&typed).filter(|t| !t.is_empty()).map(|t| t.as_str())).await }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(crate::fleet::desktop::AddDisplayError::Failed(e.to_string())));
            let _ = this.update_in(cx, |this, window, cx| {
                this.adding = false;
                match result {
                    Ok(d) => cx.emit(DialogEvent::DisplayAdded(machine, d.id)),
                    Err(crate::fleet::desktop::AddDisplayError::NeedsPassword(why)) => {
                        // Asked for the first time, or the one given didn't work.
                        if this.asking {
                            this.error = Some(why);
                        }
                        this.asking = true;
                        window.focus(&this.password.focus_handle(cx));
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for AddDisplayDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // The usual sizes, and the main monitor's when it's another.
        let mut choices = MONITOR_SIZES.to_vec();
        if let Some(main) = self.main.filter(|m| !choices.contains(m)) {
            choices.push(main);
            choices.sort_by_key(|(w, h)| w * h);
        }
        let main = self.main;
        let sizes: Vec<AnyElement> = choices
            .into_iter()
            .map(|(w, h)| {
                let on = self.size == (w, h);
                div()
                    .id(SharedString::from(format!("mon-{w}x{h}")))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(2.))
                    .py(px(8.))
                    .rounded(px(7.))
                    .border_1()
                    .border_color(if on { theme.accent } else { theme.border_variant })
                    .bg(if on { theme.accent.opacity(0.1) } else { theme.element })
                    .cursor_pointer()
                    .hover(|st| st.border_color(theme.accent.opacity(0.6)))
                    .child(div().text_size(px(12.5)).text_color(theme.text).child(format!("{w}×{h}")))
                    .child(div().text_size(px(11.)).text_color(theme.text_placeholder).child(match (w, h) {
                        _ if main == Some((w, h)) => "Same as main",
                        (1280, 720) => "720p",
                        (1920, 1080) => "1080p",
                        (2560, 1440) => "1440p",
                        _ => "4K",
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.size = (w, h);
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        if self.asking {
            return password_frame(&theme, &self.machine_name, "plug in the new monitor")
                .child(field(&theme, "Your password", self.password.clone(), None))
                .children(error_line(&theme, &self.error))
                .child(footer(vec![
                    Button::new("ad-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                    Button::new("ad-pass", if self.adding { "Adding…" } else { "Add monitor" })
                        .style(ButtonStyle::Primary)
                        .loading(self.adding)
                        .key_hint("↵")
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                        .into_any_element(),
                ]))
                .into_any_element();
        }
        frame(
            &theme,
            "Add a virtual monitor",
            Some(format!("Like plugging another screen into {}: windows can move there, and Muxin shows it on its own. It stays until you remove it or the machine restarts.", self.machine_name).into()),
        )
        .child(field(&theme, "Name", self.name.clone(), None))
        .child(div().flex().flex_col().gap(px(6.)).child(label(&theme, "Size").text_size(px(12.)).text_color(theme.text_muted)).child(div().flex().gap(px(8.)).children(sizes)))
        .children(error_line(&theme, &self.error))
        .child(footer(vec![
            Button::new("ad-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
            Button::new("ad-add", if self.adding { "Adding…" } else { "Add monitor" })
                .style(ButtonStyle::Primary)
                .loading(self.adding)
                .key_hint("↵")
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                .into_any_element(),
        ]))
        .into_any_element()
    }
}

/// Asking for the machine's password: what it's for, and where it goes.
fn password_frame(theme: &Theme, machine: &str, what: &str) -> gpui::Div {
    frame(
        theme,
        format!("Your password on {machine}"),
        Some(format!("{machine} asks for it to {what}. Muxin sends it to sudo there over its encrypted connection, and keeps it only in memory until you quit.").into()),
    )
}

/// The machine's password, to remove a virtual monitor (Linux asks sudo).
pub struct MonitorPasswordDialog {
    engine: Arc<Engine>,
    machine: String,
    machine_name: String,
    display: String,
    password: Entity<TextInput>,
    busy: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for MonitorPasswordDialog {}

impl MonitorPasswordDialog {
    pub fn new(engine: Arc<Engine>, machine: String, display: String, error: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let machine_name = engine.current().machines.iter().find(|m| m.id == machine).map(|m| m.name.clone()).unwrap_or_else(|| machine.clone());
        let password = cx.new(|cx| TextInput::new(cx, "Password").secret());
        let sub = cx.subscribe_in(&password, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => {
                this.error = None;
                cx.notify()
            }
        });
        // A password that didn't work says so; a first ask doesn't need to.
        let error = error.filter(|e| e.contains("didn't work"));
        MonitorPasswordDialog { engine, machine, machine_name, display, password, busy: false, error, _subscriptions: vec![sub] }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.password.focus_handle(cx)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let p = self.password.read(cx).text().to_string();
        if p.is_empty() {
            self.error = Some(format!("Type your password on {}.", self.machine_name));
            cx.notify();
            return;
        }
        self.password.update(cx, |i, cx| i.set_text("", cx));
        self.engine.remember_password(&self.machine, p);
        self.busy = true;
        self.error = None;
        cx.notify();
        let (engine, machine, display) = (self.engine.clone(), self.machine.clone(), self.display.clone());
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.remove_display(&machine, &display).await }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(crate::fleet::desktop::AddDisplayError::Failed(e.to_string())));
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => cx.emit(DialogEvent::Dismissed),
                Err(e) => {
                    this.busy = false;
                    this.error = Some(e.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

impl Render for MonitorPasswordDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        password_frame(&theme, &self.machine_name, "unplug the virtual monitor")
            .child(field(&theme, "Your password", self.password.clone(), None))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                Button::new("mp-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                Button::new("mp-go", if self.busy { "Removing…" } else { "Remove monitor" })
                    .style(ButtonStyle::Primary)
                    .loading(self.busy)
                    .key_hint("↵")
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                    .into_any_element(),
            ]))
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
    /// None = let Muxin pick the least busy machine.
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
                .child(kit::color_tile(icon_name, 22.))
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
                worktree_of: None,
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
                    kit::color_icon(&theme, "zap", 12., theme.text_placeholder).into_any_element()
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

// ---------------------------------------------------------------------------

/// Start an agent CLI (or a shell) on a machine.
pub struct NewSessionDialog {
    engine: Arc<Engine>,
    machine: Option<String>,
    agent: &'static str,
    /// Chosen by hand; until then the default follows what's installed.
    agent_picked: bool,
    cwd: Entity<TextInput>,
    prompt: Entity<TextInput>,
    /// Set by whatever opened the dialog (an issue to fix, a workspace).
    prefill: Prefill,
    /// Where it works: a folder, a new worktree of a checkout there, or a
    /// fresh clone (`prefill.workspace`). See `place`.
    place: Place,
    branch: Entity<TextInput>,
    /// The folders where the Folder field points: (machine, folder) asked
    /// for, and the last listing that came back (for which machine and folder).
    folders_asked: Option<(String, String)>,
    folders: Option<((String, String), Load<Listing>)>,
    /// Start in a new Linux VM instead of a machine: (system, the machine it runs on).
    new_vm: Option<(String, Option<String>)>,
    _subscriptions: Vec<Subscription>,
}

/// Where a new session works.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// A folder that's there (the Folder field).
    Folder,
    /// A new git worktree of a checkout on the machine: its own folder and branch.
    Worktree,
    /// A new workspace with a fresh clone of the repository (`prefill.workspace`).
    Clone,
}

/// What a caller already knows about the session to start.
#[derive(Clone, Default)]
pub struct Prefill {
    pub title: Option<String>,
    /// A first message prepared for the agent (the user can add to it).
    pub message: Option<String>,
    /// Where it came from, shown with the message ("ZenNotes/zennotes#632").
    pub source: Option<String>,
    /// The GitHub repository it's about: its checkouts are suggested as folders.
    pub repo: Option<String>,
    /// A command to run instead of an agent (a Docker workspace's shell).
    pub command: Option<(String, String)>,
    /// Offer a fresh workspace (a clone of the repository) as the place to work.
    pub workspace: Option<WorkspacePlan>,
}

/// A fresh workspace to start the session in: the repository cloned on the
/// chosen machine, on its own branch.
#[derive(Clone)]
pub struct WorkspacePlan {
    pub repo: String,
    /// What to check out ("pull/123/head" for a pull request from a fork).
    pub git_ref: Option<String>,
    /// A new branch to make; None works on `git_ref` itself (a pull request's branch).
    pub branch: Option<String>,
    pub name: String,
    /// Start with the workspace chosen, rather than a folder.
    pub preferred: bool,
}

impl EventEmitter<DialogEvent> for NewSessionDialog {}

impl NewSessionDialog {
    pub fn new(engine: Arc<Engine>, machine: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd = cx.new(|cx| TextInput::new(cx, "~ (home folder)").mono());
        let prompt = cx.new(|cx| TextInput::new(cx, "What should it work on? (optional)"));
        let branch = cx.new(|cx| TextInput::new(cx, "new branch (optional)").mono());
        let on_input = |this: &mut Self, _: &Entity<TextInput>, e: &InputEvent, _: &mut Window, cx: &mut Context<Self>| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => cx.notify(),
        };
        // Typing a folder lists what's in it.
        let on_folder = move |this: &mut Self, input: &Entity<TextInput>, e: &InputEvent, window: &mut Window, cx: &mut Context<Self>| {
            if matches!(e, InputEvent::Changed) {
                this.list_folders(cx);
            }
            on_input(this, input, e, window, cx)
        };
        let subs = vec![cx.subscribe_in(&cwd, window, on_folder), cx.subscribe_in(&prompt, window, on_input), cx.subscribe_in(&branch, window, on_input)];
        let snapshot = engine.current();
        let machine = machine.or_else(|| session_machines(&snapshot).into_iter().find(|m| m.status == "online").map(|m| m.id.clone()));
        let mut dialog = NewSessionDialog {
            engine,
            machine,
            agent: "shell",
            agent_picked: false,
            cwd,
            prompt,
            prefill: Prefill::default(),
            place: Place::Folder,
            branch,
            folders_asked: None,
            folders: None,
            new_vm: None,
            _subscriptions: subs,
        };
        dialog.agent = dialog.default_agent(&snapshot);
        dialog.list_folders(cx);
        dialog.load_checkouts(&snapshot);
        dialog
    }

    /// The chosen machine's git checkouts, when they haven't been looked at:
    /// a worktree of one is offered once they're known.
    fn load_checkouts(&self, s: &Snapshot) {
        let Some(machine) = self.machine.clone() else { return };
        if s.checkouts.iter().any(|c| c.machine_id == machine) || s.machines.iter().any(|m| m.id == machine && m.is_down()) {
            return;
        }
        let engine = self.engine.clone();
        engine.runtime().clone().spawn(async move {
            let _ = engine.refresh_checkouts(&machine).await;
        });
    }

    /// Lists the folder the Folder field points at, on the chosen machine,
    /// unless that's what's listed already.
    fn list_folders(&mut self, cx: &mut Context<Self>) {
        let Some(machine) = self.machine.clone() else { return };
        if self.without_agent(&self.engine.current()) {
            self.folders_asked = None;
            return;
        }
        let (dir, _) = folder_query(self.cwd.read(cx).text());
        let key = (machine.clone(), dir.clone());
        if self.folders_asked.as_ref() == Some(&key) {
            return;
        }
        self.folders_asked = Some(key.clone());
        if self.folders.is_none() {
            self.folders = Some((key.clone(), Load::Loading));
        }
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.list_files(&machine, &dir).await }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                if this.folders_asked.as_ref() == Some(&key) {
                    this.folders = Some((key, result.map_or_else(Load::Failed, Load::Ready)));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Puts `path` in the Folder field (which lists it).
    fn go_to_folder(&mut self, path: String, cx: &mut Context<Self>) {
        self.cwd.update(cx, |i, cx| i.set_text(path, cx));
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.cwd.focus_handle(cx)
    }

    pub fn with_agent(mut self, agent: &'static str) -> Self {
        self.agent = agent;
        self.agent_picked = true;
        self
    }

    /// Starts from a folder and what the caller prepared (title, first message, command).
    pub fn prefill(&mut self, cwd: Option<String>, prefill: Prefill, cx: &mut Context<Self>) {
        if let Some(cwd) = cwd {
            self.cwd.update(cx, |i, cx| i.set_text(cwd, cx));
        }
        if prefill.message.is_some() {
            self.prompt.update(cx, |i, cx| i.set_placeholder("Anything to add? (optional)", cx));
        }
        if prefill.command.is_some() {
            self.agent = "shell";
            self.agent_picked = true;
        }
        if let Some(plan) = &prefill.workspace {
            // A checkout of it there: a worktree of that (see `place`); else a clone, when that's preferred.
            self.place = if plan.preferred { Place::Clone } else { Place::Worktree };
            let (text, hint) = match &plan.branch {
                Some(b) => (b.clone(), "new branch (optional)".to_string()),
                None => (String::new(), format!("stay on {}", plan.git_ref.clone().unwrap_or_else(|| "the default branch".into()))),
            };
            self.branch.update(cx, |i, cx| {
                i.set_text(text, cx);
                i.set_placeholder(hint, cx);
            });
        }
        self.prefill = prefill;
        cx.notify();
    }

    /// The first agent CLI the machine has, else a shell.
    fn default_agent(&self, s: &Snapshot) -> &'static str {
        // A new VM comes with whichever is picked.
        if self.new_vm.is_some() {
            return "claude";
        }
        let installed = self.machine.as_ref().and_then(|id| s.machines.iter().find(|m| &m.id == id)).map(|m| m.agents.clone()).unwrap_or_default();
        AGENTS.iter().find(|a| installed.iter().any(|i| i == a.id)).map(|a| a.id).unwrap_or("shell")
    }

    /// The chosen machine can't keep sessions in Muxin's agent (Windows, for now).
    fn without_agent(&self, s: &Snapshot) -> bool {
        self.machine.as_ref().and_then(|id| s.machines.iter().find(|m| &m.id == id)).is_some_and(super::sessions::runs_in_app)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        // A new VM: its id is its name's, and it's made first.
        let vm = self.new_vm.clone().and_then(|(system, host)| {
            let host = host?;
            let name = vm_name(&self.engine.current(), &system);
            Some(crate::ops::vm::VmCreate {
                name: name.clone(),
                system,
                host: Some(host),
                cpus: Some(4),
                memory_gb: Some(8),
                disk_gb: None,
                install: if self.agent == "shell" { Vec::new() } else { vec![self.agent.to_string()] },
            })
        });
        if self.new_vm.is_some() && vm.is_none() {
            return;
        }
        let Some(machine_id) = vm.as_ref().map(|v| crate::util::slug(&v.name)).or_else(|| self.machine.clone()) else { return };
        let direct = vm.is_none() && self.without_agent(&self.engine.current());
        let text = |e: &Entity<TextInput>, cx: &Context<Self>| Some(e.read(cx).text().trim().to_string()).filter(|t| !t.is_empty());
        // The prepared message, then whatever the user added.
        let message = match (self.prefill.message.clone(), text(&self.prompt, cx)) {
            (Some(m), Some(extra)) => Some(format!("{m}\n\n{extra}")),
            (m, extra) => m.or(extra),
        };
        let title = self.prefill.title.clone().map(|t| match (&self.prefill.command, crate::fleet::sessions::agent(self.agent)) {
            (None, Some(kind)) if kind.id != "shell" => format!("{} · {t}", kind.name),
            _ => t,
        });
        let session = StartSession {
            machine_id: machine_id.clone(),
            agent: self.agent.to_string(),
            cwd: text(&self.cwd, cx).map(|t| if t.len() > 1 { t.trim_end_matches('/').to_string() } else { t }).unwrap_or_else(|| "~".into()),
            title,
            prompt: if self.agent == "shell" { None } else { message },
            cols: 0,
            rows: 0,
            direct,
            command: self.prefill.command.as_ref().map(|(c, _)| c.clone()),
            typed: None,
        };
        let snapshot = self.engine.current();
        let plan = self.prefill.workspace.clone();
        match (self.place(&snapshot, cx), plan) {
            (Place::Clone, Some(plan)) => {
                if vm.is_none() && !self.workspaces_allowed(&snapshot) {
                    return;
                }
                let workspace = CreateWorkspace {
                    machine: Some(machine_id),
                    platform: None,
                    name: Some(plan.name),
                    repo: Some(plan.repo),
                    git_ref: plan.git_ref,
                    branch: text(&self.branch, cx),
                    owner: Some("app".into()),
                    worktree_of: None,
                };
                match vm {
                    Some(vm) => cx.emit(DialogEvent::StartInNewVm(vm, Some(workspace), session)),
                    None => cx.emit(DialogEvent::StartInWorkspace(workspace, session)),
                }
            }
            (Place::Worktree, plan) => {
                let Some(source) = self.worktree_source(&snapshot, cx) else { return };
                if !self.workspaces_allowed(&snapshot) {
                    return;
                }
                let branch = text(&self.branch, cx);
                let folder = source.path.trim_end_matches('/').rsplit('/').next().unwrap_or("worktree").to_string();
                let name = plan.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| match &branch {
                    Some(b) => format!("{folder} · {b}"),
                    None => folder,
                });
                let workspace = CreateWorkspace {
                    machine: Some(machine_id),
                    platform: None,
                    name: Some(name),
                    repo: source.repo.clone(),
                    git_ref: plan.and_then(|p| p.git_ref),
                    branch,
                    owner: Some("app".into()),
                    worktree_of: Some(source.path),
                };
                cx.emit(DialogEvent::StartInWorkspace(workspace, session));
            }
            _ => match vm {
                Some(vm) => cx.emit(DialogEvent::StartInNewVm(vm, None, session)),
                None => cx.emit(DialogEvent::StartSession(session)),
            },
        }
    }

    /// The checkout a worktree would come from, on the chosen machine: one of
    /// the repository it's about (a clean one first, not a workspace itself),
    /// else the one the Folder field points at.
    fn worktree_source(&self, s: &Snapshot, cx: &App) -> Option<crate::fleet::model::Checkout> {
        let machine = self.machine.as_ref().filter(|_| self.new_vm.is_none())?;
        let here = s.checkouts.iter().filter(|c| &c.machine_id == machine);
        match &self.prefill.workspace {
            Some(plan) => here
                .filter(|c| c.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&plan.repo)) && !c.path.contains("/muxen/workspaces/"))
                .min_by_key(|c| c.dirty)
                .cloned(),
            None => {
                let typed = self.cwd.read(cx).text().trim().to_string();
                here.into_iter().find(|c| !typed.is_empty() && crate::fleet::workspaces::same_folder(&c.path, &typed)).cloned()
            }
        }
    }

    /// Where it works, as chosen: a worktree needs a checkout there and a
    /// clone needs a repository, else it's the folder.
    fn place(&self, s: &Snapshot, cx: &App) -> Place {
        match self.place {
            Place::Worktree if self.worktree_source(s, cx).is_some() => Place::Worktree,
            Place::Worktree | Place::Clone if self.prefill.workspace.as_ref().is_some_and(|p| p.preferred || self.place == Place::Clone) => Place::Clone,
            _ => Place::Folder,
        }
    }

    fn workspaces_allowed(&self, s: &Snapshot) -> bool {
        self.machine.as_ref().and_then(|id| s.machines.iter().find(|m| &m.id == id)).is_some_and(|m| m.workspaces_allowed)
    }
}

/// What the Folder field points at: the folder to list ("~/code") and the
/// start of a name in it ("ap" in "~/code/ap").
fn folder_query(text: &str) -> (String, String) {
    let t = text.trim();
    if t.is_empty() || t == "~" {
        return ("~".into(), String::new());
    }
    let (dir, name) = match t.rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((dir, name)) => (dir, name),
        // A bare name is in the home folder, where sessions start.
        None => ("~", t),
    };
    (dir.to_string(), name.to_string())
}

/// `name` inside `dir`, ending in a slash so its folders list next.
fn folder_path(dir: &str, name: &str) -> String {
    if dir == "/" { format!("/{name}/") } else { format!("{}/{name}/", dir.trim_end_matches('/')) }
}

impl NewSessionDialog {
    /// The folders where the Folder field points: click one to go into it.
    fn folder_browser(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let asked = self.folders_asked.clone()?;
        let (shown, load) = self.folders.as_ref()?;
        // Still showing the last folder while the next one loads: not clickable.
        let stale = *shown != asked;
        let (dir, typed) = folder_query(self.cwd.read(cx).text());
        let quiet = |text: String| div().px(px(2.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(text).into_any_element();
        let listing = match load {
            Load::Ready(l) => l,
            Load::Loading => {
                return Some(div().flex().items_center().gap(px(6.)).child(kit::spinner("ns-folders", 11., theme.text_placeholder)).child(quiet("Looking…".into())).into_any_element());
            }
            Load::Failed(e) => return Some(quiet(e.clone())),
        };
        let needle = typed.to_lowercase();
        let mut found: Vec<&str> = listing
            .entries
            .iter()
            .filter(|e| e.folder && (needle.starts_with('.') || !e.name.starts_with('.')))
            .map(|e| e.name.as_str())
            .filter(|n| n.to_lowercase().contains(&needle))
            .collect();
        // Names that start with what's typed first.
        found.sort_by_key(|n| !n.to_lowercase().starts_with(&needle));
        const SHOWN: usize = 24;
        let more = found.len().saturating_sub(SHOWN);
        let chips: Vec<AnyElement> = found
            .iter()
            .take(SHOWN)
            .enumerate()
            .map(|(i, name)| {
                let next = folder_path(&dir, name);
                div()
                    .id(("ns-dir", i))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(24.))
                    .px(px(8.))
                    .rounded(px(5.))
                    .bg(theme.element)
                    .border_1()
                    .border_color(theme.border_variant)
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .when(stale, |d| d.opacity(0.5))
                    .when(!stale, |d| d.cursor_pointer().hover(|st| st.bg(theme.element_hover)))
                    .child(kit::color_icon(theme, "folder", 12., theme.text_placeholder))
                    .child(name.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !stale {
                            this.go_to_folder(next.clone(), cx)
                        }
                    }))
                    .into_any_element()
            })
            .collect();
        // Where it is, and the way up.
        let here = super::files::display(listing);
        let parent = match listing.path.trim_end_matches('/').rsplit_once('/') {
            _ if listing.path == "/" => None,
            Some(("", _)) => Some("/".to_string()),
            Some((up, _)) => Some(super::files::tilde(up, &listing.home) + "/"),
            None => None,
        };
        let header = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(22.))
            .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!("Folders in {here}")))
            .when(stale, |d| d.child(kit::spinner("ns-folders", 11., theme.text_placeholder)))
            .children(parent.map(|up| {
                Button::icon_only("ns-up", "arrow-up").tooltip("Up a folder").disabled(stale).on_click(cx.listener(move |this, _, _, cx| this.go_to_folder(up.clone(), cx)))
            }));
        let body = if chips.is_empty() {
            quiet(if needle.is_empty() { "No folders here.".into() } else { format!("No folder matches \"{typed}\".") })
        } else {
            div().flex().flex_wrap().gap(px(6.)).children(chips).into_any_element()
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .p(px(8.))
                .rounded(px(7.))
                .border_1()
                .border_color(theme.border_variant)
                .child(header)
                .child(body)
                .when(more > 0, |d| d.child(quiet(format!("{more} more: type to narrow them down."))))
                .into_any_element(),
        )
    }

    /// Where it works: a new worktree of a checkout there, a new workspace
    /// with a fresh clone, or a folder that's already there.
    fn where_choice(&self, theme: &Theme, place: Place, source: Option<&crate::fleet::model::Checkout>, cx: &mut Context<Self>) -> AnyElement {
        let plan = self.prefill.workspace.as_ref();
        let option = |id: &'static str, glyph: &'static str, title: &'static str, note: SharedString, value: Place, cx: &mut Context<Self>| {
            let selected = place == value;
            div()
                .id(id)
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap(px(10.))
                .p(px(10.))
                .rounded(px(8.))
                .border_1()
                .cursor_pointer()
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .child(kit::color_tile(glyph, 26.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w(px(0.))
                        .child(div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_size(px(11.)).text_color(theme.text_placeholder).truncate().child(note)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.place = value;
                    cx.notify()
                }))
                .into_any_element()
        };
        let mut options = Vec::new();
        if source.is_some() {
            options.push(option("ns-where-wt", "git-branch", "New worktree", "Its own branch".into(), Place::Worktree, cx));
        }
        if plan.is_some() {
            options.push(option("ns-where-ws", "layers", "Fresh clone", "A new copy".into(), Place::Clone, cx));
        }
        let folder_note = if plan.is_some() { "One that's there" } else { "Right here" };
        options.push(option("ns-where-folder", "folder-open", "A folder", folder_note.into(), Place::Folder, cx));
        div().flex().gap(px(8.)).children(options).into_any_element()
    }

    /// The branch to work on, and whether the machine takes workspaces.
    fn workspace_fields(&self, theme: &Theme, place: Place, source: Option<&crate::fleet::model::Checkout>, machine: Option<&crate::fleet::model::Machine>, cx: &mut Context<Self>) -> AnyElement {
        let name = machine.map(|m| m.name.clone()).unwrap_or_else(|| "the machine".into());
        let plan = self.prefill.workspace.as_ref();
        let what = match (place, source, plan) {
            (Place::Worktree, Some(c), plan) => {
                let from = match plan.and_then(|p| p.git_ref.as_deref()) {
                    Some(r) if r.starts_with("pull/") => "the pull request".to_string(),
                    Some(r) => r.to_string(),
                    None => "the default branch".to_string(),
                };
                format!("Makes a git worktree of {} on {name}: a folder of its own, on its own branch from {from}. The checkout stays as it is, so other agents can work there too.", c.path)
            }
            (_, _, Some(plan)) => match &plan.git_ref {
                Some(r) if r.starts_with("pull/") => format!("Clones {} on {name} and checks out the pull request.", plan.repo),
                Some(r) => format!("Clones {} on {name} at {r}.", plan.repo),
                None => format!("Clones {} on {name}.", plan.repo),
            },
            _ => String::new(),
        };
        let off = machine.filter(|m| !m.workspaces_allowed).map(|m| {
            let id = m.id.clone();
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .p(px(10.))
                .rounded(px(6.))
                .bg(theme.warning.opacity(0.10))
                .text_size(px(12.5))
                .child(icon("info", 14., theme.warning))
                .child(div().flex_1().min_w(px(0.)).child(format!("Workspaces are off on {}.", m.name)))
                .child(Button::new("ns-allow-ws", "Turn them on").on_click(cx.listener(move |this, _, _, cx| {
                    if let Err(e) = this.engine.set_workspaces_allowed(&id, true) {
                        tracing::warn!("turning on workspaces: {e}");
                    }
                    cx.notify()
                })))
        });
        // No checkout of it found there: say how to point Muxin at one.
        let missing = plan.filter(|_| source.is_none()).map(|p| {
            div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
                "No checkout of {} found on {name}. Have one? Add its folder in Settings › General › Code folders and start this again: the agent then works in a worktree of it, on its own branch, instead of a fresh clone.",
                p.repo
            ))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(field(theme, "Branch", self.branch.clone(), None))
            .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(what))
            .children(missing)
            .children(off)
            .into_any_element()
    }
}

/// The window height a dialog's title, intro, buttons and margins take.
pub(super) const MODAL_CHROME: f32 = 300.;

/// "ubuntu-1", "debian-2": the first name of that kind not taken yet.
fn vm_name(s: &Snapshot, system: &str) -> String {
    let base = system.split('-').next().unwrap_or("vm");
    let taken: Vec<String> = s.machines.iter().map(|m| m.name.to_lowercase()).collect();
    (1..).map(|n| format!("{base}-{n}")).find(|n| !taken.contains(n)).unwrap_or_else(|| base.to_string())
}

/// Machines that can run sessions: any with a host (or this computer).
fn session_machines(s: &Snapshot) -> Vec<&crate::fleet::model::Machine> {
    s.machines.iter().filter(|m| !m.host.is_empty()).collect()
}

impl Render for NewSessionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let snapshot = self.engine.current();
        if !self.agent_picked {
            // What's installed often arrives after the dialog opens.
            self.agent = self.default_agent(&snapshot);
        }
        let machine = self.machine.as_ref().and_then(|id| snapshot.machines.iter().find(|m| &m.id == id)).cloned();
        let checked = machine.as_ref().is_some_and(|m| m.agent_version.is_some() || m.sessions_error.is_some());
        let installed: Vec<String> = machine.as_ref().map(|m| m.agents.clone()).unwrap_or_default();

        // Machines of their own first; the VMs and containers inside one
        // under it ("In Beelink"); then a new VM, made for this.
        let all = session_machines(&snapshot);
        let host_of = |m: &crate::fleet::model::Machine| m.parent.as_ref().filter(|p| p.inside() && all.iter().any(|h| h.id == p.machine_id)).map(|p| p.machine_id.clone());
        let machine_chip = |i: usize, m: &crate::fleet::model::Machine, cx: &mut Context<Self>| {
            let selected = self.new_vm.is_none() && self.machine.as_deref() == Some(m.id.as_str());
            let id = m.id.clone();
            let kind = m.parent.as_ref().filter(|p| p.inside()).map(|p| if p.relation == "vm" { "VM" } else { "Container" });
            div()
                .id(("ns-machine", i))
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
                .child(div().size(px(7.)).rounded_full().bg(theme.machine_status(m)))
                .child(m.name.clone())
                .children(kind.map(|k| div().text_size(px(11.)).text_color(theme.text_placeholder).child(k)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.machine = Some(id.clone());
                    this.new_vm = None;
                    this.agent_picked = false;
                    this.list_folders(cx);
                    this.load_checkouts(&this.engine.current());
                    // Find out what's installed there if we don't know yet.
                    let engine = this.engine.clone();
                    let id = id.clone();
                    engine.runtime().clone().spawn(async move {
                        let _ = engine.refresh_sessions(&id).await;
                    });
                    cx.notify()
                }))
                .into_any_element()
        };
        let mut i = 0;
        let mut own: Vec<AnyElement> = Vec::new();
        for m in all.iter().filter(|m| host_of(m).is_none()) {
            own.push(machine_chip(i, m, cx));
            i += 1;
        }
        // Linux VMs need a machine with Docker.
        let vm_hosts: Vec<crate::ops::vm::VmHost> = self.engine.vm_options().hosts.into_iter().filter(|h| h.systems.iter().any(|s| s.starts_with("ubuntu"))).collect();
        if !vm_hosts.is_empty() {
            let on = self.new_vm.is_some();
            let best = vm_hosts.iter().max_by_key(|h| h.memory_free.unwrap_or(0)).map(|h| h.machine_id.clone());
            own.push(
                div()
                    .id("ns-new-vm")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(26.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .border_1()
                    .border_dashed()
                    .cursor_pointer()
                    .border_color(if on { theme.accent } else { theme.border })
                    .bg(if on { theme.accent.opacity(0.12) } else { theme.element })
                    .text_size(px(12.5))
                    .text_color(if on { theme.text } else { theme.text_muted })
                    .child(icon("box", 12., if on { theme.text } else { theme.text_muted }))
                    .child("New VM")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.new_vm = Some(("ubuntu-24.04".into(), best.clone()));
                        this.agent_picked = false;
                        cx.notify()
                    }))
                    .into_any_element(),
            );
        }
        let mut groups: Vec<AnyElement> = Vec::new();
        for h in all.iter().filter(|m| host_of(m).is_none()) {
            let guests: Vec<&&crate::fleet::model::Machine> = all.iter().filter(|g| host_of(g).as_deref() == Some(h.id.as_str())).collect();
            if guests.is_empty() {
                continue;
            }
            let mut chips: Vec<AnyElement> = Vec::new();
            for g in guests {
                chips.push(machine_chip(i, g, cx));
                i += 1;
            }
            groups.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_none().flex().items_center().gap(px(4.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(icon("corner-down-right", 11., theme.text_placeholder)).child(format!("In {}", h.name)))
                    .child(div().flex().flex_wrap().gap(px(6.)).children(chips))
                    .into_any_element(),
            );
        }
        let vm_form = self.new_vm.clone().map(|(system, host)| {
            let chip = |id: SharedString, text: String, selected: bool| {
                div()
                    .id(id)
                    .h(px(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .border_1()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .border_color(if selected { theme.accent } else { theme.border_variant })
                    .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                    .text_color(if selected { theme.text } else { theme.text_muted })
                    .child(text)
            };
            let systems: Vec<AnyElement> = crate::ops::vm::SYSTEMS
                .iter()
                .filter(|s| s.os == "linux" && vm_hosts.iter().any(|h| h.systems.iter().any(|x| x == s.id)))
                .map(|s| {
                    let id = s.id.to_string();
                    chip(SharedString::from(format!("ns-vs-{id}")), s.name.to_string(), system == id)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(v) = this.new_vm.as_mut() {
                                v.0 = id.clone();
                            }
                            cx.notify()
                        }))
                        .into_any_element()
                })
                .collect();
            let hosts: Vec<AnyElement> = vm_hosts
                .iter()
                .filter(|h| h.systems.iter().any(|x| *x == system))
                .map(|h| {
                    let id = h.machine_id.clone();
                    chip(SharedString::from(format!("ns-vh-{id}")), format!("on {}", h.name), host.as_deref() == Some(id.as_str()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(v) = this.new_vm.as_mut() {
                                v.1 = Some(id.clone());
                            }
                            cx.notify()
                        }))
                        .into_any_element()
                })
                .collect();
            let name = vm_name(&snapshot, &system);
            let with = match crate::fleet::sessions::agent(self.agent) {
                Some(k) if k.id != "shell" => format!(", with {}", k.name),
                _ => String::new(),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(10.))
                .rounded(px(8.))
                .border_1()
                .border_color(theme.border_variant)
                .bg(theme.element.opacity(0.5))
                .child(div().flex().flex_wrap().gap(px(6.)).children(systems))
                .child(div().flex().flex_wrap().gap(px(6.)).children(hosts))
                .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
                    "A Linux container named {name}{with}, ready in a few minutes. It joins your machines; destroy it from its page when you're done."
                )))
        });
        let picker = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().flex_wrap().gap(px(6.)).children(own))
            .children(groups)
            .children(vm_form);

        // Agents, three to a row.
        let card = |i: usize, kind: &'static crate::fleet::sessions::AgentKind, cx: &mut Context<Self>| {
            let selected = self.agent == kind.id;
            let found = kind.id == "shell" || installed.iter().any(|a| a == kind.id);
            let (icon_name, (from, to)) = super::kit::agent_look(kind.id);
            let new_vm = self.new_vm.is_some();
            let found = found || new_vm;
            let note = match (kind.id, found, checked) {
                ("shell", _, _) => "Your login shell",
                (_, _, _) if new_vm => "Installed with the VM",
                (_, true, _) => "Installed",
                (_, false, true) => "Not installed",
                (_, false, false) => "Checking…",
            };
            div()
                .id(("ns-agent", i))
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap(px(10.))
                .p(px(10.))
                .rounded(px(8.))
                .border_1()
                .cursor_pointer()
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .when(!found && checked, |d| d.opacity(0.55))
                .child(super::kit::tile(icon_name, from, to, 28.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w(px(0.))
                        .child(div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).child(kind.name))
                        .child(div().text_size(px(11.)).text_color(theme.text_placeholder).child(note)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.agent = kind.id;
                    this.agent_picked = true;
                    cx.notify()
                }))
        };
        let mut rows = Vec::new();
        for (r, chunk) in AGENTS.chunks(3).enumerate() {
            let cards: Vec<AnyElement> = chunk.iter().enumerate().map(|(c, k)| card(r * 3 + c, k, cx).into_any_element()).collect();
            rows.push(div().flex().gap(px(8.)).children(cards).into_any_element());
        }

        // Checkouts of the repository at hand, then where sessions run on this
        // machine, then workspace folders there.
        let on_machine = |id: &str| self.machine.as_deref() == Some(id);
        let mut folders: Vec<String> = Vec::new();
        for path in snapshot
            .checkouts
            .iter()
            .filter(|c| on_machine(&c.machine_id) && self.prefill.repo.is_some() && c.repo.as_deref().map(str::to_lowercase) == self.prefill.repo.as_deref().map(str::to_lowercase))
            .map(|c| c.path.clone())
            .chain(snapshot.sessions.iter().filter(|x| on_machine(&x.machine_id) && !matches!(x.cwd.as_str(), "" | "~")).map(|x| x.cwd.clone()))
            .chain(snapshot.workspaces.iter().filter(|w| on_machine(&w.machine_id)).map(|w| w.path.clone()))
        {
            if folders.len() < 5 && !folders.contains(&path) {
                folders.push(path);
            }
        }
        // Shown as ~/... once the machine's home folder is known.
        let home = match &self.folders {
            Some(((m, _), Load::Ready(l))) if self.machine.as_ref() == Some(m) => l.home.clone(),
            _ => String::new(),
        };
        let folder_chips = div().flex().flex_wrap().gap(px(6.)).children(folders.into_iter().enumerate().map(|(i, path)| {
            let path = super::files::tilde(&path, &home);
            let value = path.clone();
            div()
                .id(("ns-folder", i))
                .h(px(22.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(5.))
                .child(kit::color_icon(&theme, "clock", 11., theme.text_placeholder))
                .rounded(px(5.))
                .border_1()
                .border_color(theme.border_variant)
                .bg(theme.element)
                .cursor_pointer()
                .hover(|st| st.bg(theme.element_hover))
                .font_family(mono_font())
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .child(path)
                .on_click(cx.listener(move |this, _, _, cx| this.cwd.update(cx, |i, cx| i.set_text(value.clone(), cx))))
        }));

        // Without the agent the session lives in this window only; say so.
        let in_app = self.without_agent(&snapshot);
        let machine_name = machine.as_ref().map(|m| m.name.clone()).unwrap_or_default();
        let in_app_note = in_app.then(|| {
            div()
                .flex()
                .items_start()
                .gap(px(10.))
                .p(px(10.))
                .rounded(px(6.))
                .bg(theme.warning.opacity(0.10))
                .text_size(px(12.5))
                .text_color(theme.text)
                .child(div().pt(px(1.)).child(icon("info", 14., theme.warning)))
                .child(div().flex_1().min_w(px(0.)).child(format!(
                    "Sessions on {machine_name} run while Muxin is open; they can't keep running in the background there yet."
                )))
        });
        let place = self.place(&snapshot, cx);
        let source = self.worktree_source(&snapshot, cx);
        let into_workspace = place != Place::Folder;
        let can_start = match &self.new_vm {
            Some((_, host)) => host.is_some(),
            None => self.machine.is_some() && (!into_workspace || self.workspaces_allowed(&snapshot)),
        };
        let start_label = match (self.new_vm.is_some(), into_workspace) {
            (true, true) => "Create VM, workspace and start",
            (true, false) => "Create VM and start",
            (false, true) if place == Place::Worktree => "Create worktree and start",
            (false, true) => "Create workspace and start",
            (false, false) => "Start session",
        };

        // The buttons stay in view; what's between them and the title scrolls.
        let body_max = (f32::from(window.viewport_size().height) - MODAL_CHROME).max(220.);
        let fields = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(field(&theme, "Machine", picker, None))
        .map(|d| match &self.prefill.command {
            // A prepared command (a Docker workspace's shell) instead of an agent.
            Some((_, what)) => d.child(field(&theme, "Runs", div().text_size(px(12.5)).text_color(theme.text_muted).child(what.clone()), None)),
            None => d.child(field(&theme, "Agent", div().flex().flex_col().gap(px(8.)).children(rows), None)),
        })
        .when(self.prefill.workspace.is_some() || source.is_some(), |d| d.child(field(&theme, "Where", self.where_choice(&theme, place, source.as_ref(), cx), None)))
        .map(|d| match place {
            Place::Worktree | Place::Clone => d.child(self.workspace_fields(&theme, place, source.as_ref(), machine.as_ref(), cx)),
            Place::Folder if self.new_vm.is_some() => d,
            Place::Folder => d.child(field(&theme, "Folder", div().flex().flex_col().gap(px(8.)).child(self.cwd.clone()).child(folder_chips).children(self.folder_browser(&theme, cx)), None)),
        })
        .when(self.agent != "shell", |d| {
            let prepared = self.prefill.message.clone().map(|m| {
                // An excerpt (the agent gets all of it). No scroll box of its own:
                // one inside the dialog's makes the wheel stick.
                let lines: Vec<&str> = m.lines().filter(|l| !l.trim().is_empty()).collect();
                let mut shown: String = lines.iter().take(5).copied().collect::<Vec<_>>().join("\n").chars().take(480).collect();
                if lines.len() > 5 || shown.chars().count() >= 480 {
                    shown.push_str(" …");
                }
                div()
                    .p(px(10.))
                    .rounded(px(6.))
                    .bg(theme.element)
                    .border_1()
                    .border_color(theme.border_variant)
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .children(self.prefill.source.clone().map(|s| div().text_size(px(11.)).text_color(theme.text_placeholder).child(format!("From {s}"))))
                    .child(div().w_full().text_size(px(12.)).text_color(theme.text_muted).child(shown))
            });
            d.child(field(&theme, "First message", div().flex().flex_col().gap(px(8.)).children(prepared).child(self.prompt.clone()), None))
        })
        .children(in_app_note);
        // A scroll area around one column (as a column itself it squeezes its children).
        let body = div().id("ns-body").max_h(px(body_max)).overflow_y_scroll().child(fields);
        frame(
            &theme,
            "New session",
            Some("Runs an agent (or a shell) in a terminal on one of your machines. It keeps running when you close Muxin; come back to it from the sidebar.".into()),
        )
        .child(body)
        .child(footer(vec![
            Button::new("ns-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
            Button::new("ns-start", start_label)
                .style(ButtonStyle::Primary)
                .icon(if into_workspace { "layers" } else { "square-terminal" })
                .disabled(!can_start)
                .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                .into_any_element(),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_field_points_at_a_folder_and_a_name() {
        let q = |t: &str| folder_query(t);
        assert_eq!(q(""), ("~".into(), "".into()));
        assert_eq!(q("~"), ("~".into(), "".into()));
        assert_eq!(q("~/"), ("~".into(), "".into()));
        assert_eq!(q("~/code/"), ("~/code".into(), "".into()));
        assert_eq!(q("~/code/ap"), ("~/code".into(), "ap".into()));
        assert_eq!(q("co"), ("~".into(), "co".into()));
        assert_eq!(q("/"), ("/".into(), "".into()));
        assert_eq!(q("/us"), ("/".into(), "us".into()));
        assert_eq!(folder_path("~", "code"), "~/code/");
        assert_eq!(folder_path("/", "usr"), "/usr/");
        assert_eq!(folder_path("~/code/", "api"), "~/code/api/");
    }
}
