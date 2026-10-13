//! New VM: Windows or Linux on one of your machines, set up and added to
//! your machines (under the one it runs on).

use std::sync::Arc;

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use super::dialogs::{DialogEvent, MODAL_CHROME, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{Button, ButtonStyle, icon};
use super::theme::ActiveTheme;
use crate::fleet::Engine;
use crate::ops::vm::{VmCreate, VmOptions};

pub struct VmDialog {
    engine: Arc<Engine>,
    options: VmOptions,
    name: Entity<TextInput>,
    system: String,
    host: Option<String>,
    cpus: u32,
    memory_gb: u32,
    disk_gb: u32,
    /// Agent CLIs a Linux VM comes with.
    install: Vec<String>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for VmDialog {}

impl VmDialog {
    /// `host`: the machine to put it on (opened from its page), else the best one.
    pub fn new(engine: Arc<Engine>, host: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // "beacon@windows-11": that machine, with that system chosen.
        let (host, system) = match host.as_deref().and_then(|h| h.split_once('@')) {
            Some((h, s)) => (Some(h.to_string()), Some(s.to_string())),
            None => (host, None),
        };
        let options = engine.vm_options();
        // Windows where some machine can run it, else Linux.
        let system = system.filter(|s| crate::ops::vm::system(s).is_some()).unwrap_or_else(|| if options.hosts.iter().any(|h| h.systems.iter().any(|s| s == "windows-11")) { "windows-11" } else { "ubuntu-24.04" }.to_string());
        let name = cx.new(|cx| TextInput::new(cx, "win-dev").with_text(free_name(&engine, &system)));
        let subs = vec![cx.subscribe_in(&name, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => cx.notify(),
        })];
        let mut d = VmDialog {
            engine,
            cpus: options.default_cpus,
            memory_gb: options.default_memory_gb,
            disk_gb: options.default_disk_gb,
            install: vec!["claude".to_string()],
            options,
            name,
            system,
            host,
            error: None,
            _subscriptions: subs,
        };
        d.pick_host();
        d
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    /// Keeps the chosen machine if it can run the system, else the one with the most free memory that can.
    fn pick_host(&mut self) {
        let can = |id: &str| self.options.hosts.iter().any(|h| h.machine_id == id && h.systems.contains(&self.system));
        if self.host.as_deref().is_some_and(can) {
            return;
        }
        self.host = self.options.hosts.iter().filter(|h| h.systems.contains(&self.system)).max_by_key(|h| h.memory_free.unwrap_or(0)).map(|h| h.machine_id.clone());
    }

    fn set_system(&mut self, id: &str, cx: &mut Context<Self>) {
        let was = self.system.clone();
        self.system = id.to_string();
        self.pick_host();
        // A name made for the old kind (win-1) follows the new one (ubuntu-1).
        let current = self.name.read(cx).text().trim().to_string();
        if current.is_empty() || current == free_name(&self.engine, &was) {
            let next = free_name(&self.engine, id);
            self.name.update(cx, |n, cx| n.set_text(next, cx));
        }
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.host.clone() else { return };
        let windows = self.system.starts_with("windows");
        let req = VmCreate {
            name: self.name.read(cx).text().trim().to_string(),
            system: self.system.clone(),
            host: Some(host),
            cpus: Some(self.cpus),
            memory_gb: Some(self.memory_gb),
            disk_gb: windows.then_some(self.disk_gb),
            install: if windows { Vec::new() } else { self.install.clone() },
        };
        match self.engine.vm_create(req) {
            Ok(op) => cx.emit(DialogEvent::OperationStarted(op)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }
}

/// "win-1", "ubuntu-2": the first such name not taken yet.
fn free_name(engine: &Engine, system: &str) -> String {
    let base = match system.split('-').next().unwrap_or("vm") {
        "windows" => "win",
        other => other,
    };
    let taken: Vec<String> = engine.current().machines.iter().map(|m| m.name.to_lowercase()).collect();
    (1..).map(|n| format!("{base}-{n}")).find(|n| !taken.contains(n)).unwrap_or_else(|| base.to_string())
}

impl Focusable for VmDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for VmDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // What the machines can run arrives with their checks (just after start, say).
        self.options = self.engine.vm_options();
        self.pick_host();
        let cancel = Button::new("vm-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element();
        let chip = |id: SharedString, text: String, selected: bool, enabled: bool| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(9.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .border_1()
                .text_size(px(12.5))
                .when(enabled, |d| d.cursor_pointer())
                .when(!enabled, |d| d.opacity(0.45))
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .text_color(if selected { theme.text } else { theme.text_muted })
                .child(text)
        };

        // Systems in two groups; one no machine here can run is greyed out.
        let runnable = |id: &str| self.options.hosts.iter().any(|h| h.systems.iter().any(|s| s == id));
        let group = |os: &str| -> Vec<AnyElement> {
            self.options
                .systems
                .iter()
                .filter(|s| s.os == os)
                .map(|s| {
                    let (id, enabled) = (s.id.to_string(), runnable(s.id));
                    let short = s.name.replace("Windows Server", "Server").replace(" Pro", "");
                    chip(SharedString::from(format!("vm-s-{id}")), short, self.system == id, enabled)
                        .when(enabled, |d| d.on_click(cx.listener(move |this, _, _, cx| this.set_system(&id, cx))))
                        .into_any_element()
                })
                .collect()
        };
        let systems = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().items_center().gap(px(8.)).child(div().w(px(56.)).text_size(px(11.5)).text_color(theme.text_placeholder).child("Windows")).child(div().flex().flex_wrap().gap(px(6.)).children(group("windows"))))
            .child(div().flex().items_center().gap(px(8.)).child(div().w(px(56.)).text_size(px(11.5)).text_color(theme.text_placeholder).child("Linux")).child(div().flex().flex_wrap().gap(px(6.)).children(group("linux"))));

        // The machines it can run on, and why the others can't.
        let system = crate::ops::vm::system(&self.system);
        let windows = system.is_some_and(|s| s.os == "windows");
        let hosts: Vec<AnyElement> = self
            .options
            .hosts
            .iter()
            .filter(|h| h.systems.contains(&self.system))
            .map(|h| {
                let id = h.machine_id.clone();
                let free = h.memory_free.map(|b| format!(" · {} free", super::format::bytes(b))).unwrap_or_default();
                chip(SharedString::from(format!("vm-h-{id}")), format!("{}{free}", h.name), self.host.as_deref() == Some(id.as_str()), true)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.host = Some(id.clone());
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        let hosts = if hosts.is_empty() {
            div()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(if windows {
                    "None of your machines can run Windows: it needs a Linux machine (x86-64) with Docker and KVM, like a mini PC or a server."
                } else {
                    "None of your machines can run it: it needs one with Docker."
                })
                .into_any_element()
        } else {
            div().flex().flex_wrap().gap(px(6.)).children(hosts).into_any_element()
        };

        let sizes = |label: &'static str, values: &[u32], unit: &'static str, current: u32, set: fn(&mut VmDialog, u32)| {
            let chips: Vec<AnyElement> = values
                .iter()
                .map(|v| {
                    let v = *v;
                    chip(SharedString::from(format!("vm-{label}-{v}")), format!("{v}{unit}"), current == v, true)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            set(this, v);
                            cx.notify()
                        }))
                        .into_any_element()
                })
                .collect();
            div().flex().items_center().gap(px(8.)).child(div().w(px(56.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(label)).child(div().flex().gap(px(6.)).children(chips))
        };
        let size = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(sizes("CPUs", &[2, 4, 8], "", self.cpus, |d, v| d.cpus = v))
            .child(sizes("Memory", &[4, 8, 16], " GB", self.memory_gb, |d, v| d.memory_gb = v))
            .when(windows, |d| d.child(sizes("Disk", &[64, 128, 256], " GB", self.disk_gb, |d, v| d.disk_gb = v)));

        // What a Linux VM comes with: agent CLIs (with Node.js).
        let comes_with = (!windows).then(|| {
            let chips: Vec<AnyElement> = ["claude", "codex", "gemini", "opencode"]
                .iter()
                .map(|id| {
                    let on = self.install.iter().any(|x| x == id);
                    let name = crate::fleet::sessions::agent(id).map(|a| a.name).unwrap_or(id);
                    let id = id.to_string();
                    chip(SharedString::from(format!("vm-i-{id}")), name.to_string(), on, true)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.install.contains(&id) {
                                this.install.retain(|x| x != &id);
                            } else {
                                this.install.push(id.clone());
                            }
                            cx.notify()
                        }))
                        .into_any_element()
                })
                .collect();
            div().flex().flex_col().gap(px(6.)).child(div().flex().flex_wrap().gap(px(6.)).children(chips)).child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child("Installed with Node.js as it starts (a few minutes more). They sign in the first time you use them."))
        });
        let note = system.map(|s| {
            div().flex().items_start().gap(px(8.)).text_size(px(12.)).text_color(theme.text_muted).child(icon("info", 13., theme.info)).child(div().flex_1().min_w(px(0.)).child(s.note))
        });
        let body_max = (f32::from(window.viewport_size().height) - MODAL_CHROME).max(220.);
        let fields = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(field(&theme, "Name", self.name.clone(), None))
            .child(field(&theme, "System", systems, None))
            .child(field(&theme, "Runs on", hosts, None))
            .child(field(&theme, "Size", size, None))
            .children(comes_with.map(|c| field(&theme, "Comes with", c, None)))
            .children(note);
        frame(&theme, "New VM", Some("Windows or Linux on one of your machines, set up with your ssh key and added to your machines.".into()))
            .child(div().id("vm-body").max_h(px(body_max)).overflow_y_scroll().child(fields))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                cancel,
                Button::new("vm-create", "Create")
                    .style(ButtonStyle::Primary)
                    .icon("box")
                    .disabled(self.host.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                    .into_any_element(),
            ]))
            .font_weight(FontWeight::NORMAL)
    }
}
