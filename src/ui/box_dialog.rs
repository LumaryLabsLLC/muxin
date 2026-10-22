//! New box: a task for an agent that works on it by itself, in a container
//! of its own on one of your machines.

use std::sync::Arc;

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use super::dialogs::{DialogEvent, MODAL_CHROME, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle};
use super::theme::ActiveTheme;
use crate::fleet::Engine;
use crate::fleet::boxes::{BOX_AGENTS, CreateBox};
use crate::fleet::model::Machine;

pub struct BoxDialog {
    engine: Arc<Engine>,
    repo: Entity<TextInput>,
    task: Entity<TextInput>,
    agent: String,
    /// None: the machine with Docker and the most free memory.
    machine: Option<String>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for BoxDialog {}

/// A machine a box can run on: online, not Windows, with Docker.
fn can_run(m: &Machine) -> bool {
    m.status == "online" && !m.platform.starts_with("windows") && m.probe.as_ref().is_some_and(|p| p.docker.is_some())
}

impl BoxDialog {
    /// `repo` and `task`, when something opened it with them (an issue to fix).
    pub fn new(engine: Arc<Engine>, repo: Option<String>, task: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let repo = cx.new(|cx| TextInput::new(cx, "owner/name, or a git URL").mono().with_text(repo.unwrap_or_default()));
        let task = cx.new(|cx| TextInput::new(cx, "Fix the login redirect loop, add a test, and open a pull request").with_text(task.unwrap_or_default()));
        let mut subs = Vec::new();
        for i in [&repo, &task] {
            subs.push(cx.subscribe_in(i, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
                InputEvent::Submit => this.submit(cx),
                InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
                InputEvent::Changed => {
                    this.error = None;
                    cx.notify()
                }
            }));
        }
        BoxDialog { engine, repo, task, agent: "claude".into(), machine: None, error: None, _subscriptions: subs }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        if self.repo.read(cx).text().trim().is_empty() { self.repo.focus_handle(cx) } else { self.task.focus_handle(cx) }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.read(cx).text().trim().to_string();
        let task = self.task.read(cx).text().trim().to_string();
        if repo.is_empty() {
            self.error = Some("Which repository? owner/name for GitHub, or a git URL.".into());
            cx.notify();
            return;
        }
        if task.is_empty() {
            self.error = Some("What should the agent do?".into());
            cx.notify();
            return;
        }
        let req = CreateBox { repo, prompt: task, agent: self.agent.clone(), machine: self.machine.clone(), ..Default::default() };
        match self.engine.box_create(req) {
            Ok((op, _)) => cx.emit(DialogEvent::BoxStarted(op)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }
}

impl Focusable for BoxDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.first_focus(cx)
    }
}

impl Render for BoxDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let base = |id: SharedString, selected: bool, enabled: bool| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(9.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(px(6.))
                .border_1()
                .text_size(px(12.5))
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .text_color(if selected { theme.text } else if enabled { theme.text_muted } else { theme.text_placeholder })
                .when(enabled, |d| d.cursor_pointer())
        };
        let s = self.engine.current();

        // Your favorite repositories, one click away.
        let typed = self.repo.read(cx).text().trim().to_string();
        let favorites: Vec<AnyElement> = self
            .engine
            .settings()
            .favorite_repos
            .into_iter()
            .take(8)
            .map(|name| {
                let pick = name.clone();
                base(SharedString::from(format!("bx-r-{name}")), typed.eq_ignore_ascii_case(&name), true)
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.repo.update(cx, |i, cx| i.set_text(pick.clone(), cx));
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        let repo = div().flex().flex_col().gap(px(8.)).child(self.repo.clone()).when(!favorites.is_empty(), |d| d.child(div().flex().flex_wrap().gap(px(6.)).children(favorites)));

        let agents: Vec<AnyElement> = BOX_AGENTS
            .iter()
            .filter_map(|a| crate::fleet::sessions::agent(a))
            .map(|a| {
                let id = a.id.to_string();
                let (glyph, (from, to)) = kit::agent_look(a.id);
                base(SharedString::from(format!("bx-a-{id}")), self.agent == id, true)
                    .pl(px(4.))
                    .child(kit::tile(glyph, from, to, 18.))
                    .child(a.name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agent = id.clone();
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        let name = crate::fleet::sessions::agent(&self.agent).map(|k| k.name).unwrap_or("The agent");
        let agent = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().flex_wrap().gap(px(6.)).children(agents))
            .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
                "{name} is signed in as it is on that machine. If it isn't signed in there, sign it in once from the box's terminal."
            )));

        // Where: the best machine, or one picked.
        let mut machines: Vec<AnyElement> = vec![base("bx-m-auto".into(), self.machine.is_none(), true)
            .child("Best one")
            .on_click(cx.listener(|this, _, _, cx| {
                this.machine = None;
                cx.notify()
            }))
            .into_any_element()];
        machines.extend(s.machines.iter().filter(|m| m.status == "online" && !m.platform.starts_with("windows")).map(|m| {
            let id = m.id.clone();
            let ok = can_run(m);
            let (from, to) = super::format::machine_tile(m);
            base(SharedString::from(format!("bx-m-{id}")), self.machine.as_deref() == Some(id.as_str()), ok)
                .pl(px(4.))
                .child(kit::tile(super::format::machine_icon(m), from, to, 18.))
                .child(m.name.clone())
                .when(!ok, |d| d.child(div().text_size(px(11.)).child("no Docker")))
                .when(ok, |d| {
                    d.on_click(cx.listener(move |this, _, _, cx| {
                        this.machine = Some(id.clone());
                        cx.notify()
                    }))
                })
                .into_any_element()
        }));
        let any = s.machines.iter().any(can_run);
        let place = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().flex_wrap().gap(px(6.)).children(machines))
            .child(div().text_size(px(11.5)).text_color(if any { theme.text_placeholder } else { theme.warning }).child(if any {
                "In a Docker container there, on a new branch of a fresh checkout. The first box on a machine builds Muxin's image (a few minutes)."
            } else {
                "Boxes run in Docker: install it on a Linux machine or a Mac (Docker Desktop, OrbStack or Colima)."
            }));

        let body_max = (f32::from(window.viewport_size().height) - MODAL_CHROME).max(220.);
        let fields = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(field(&theme, "Repository", repo, None))
            .child(field(&theme, "Task", self.task.clone(), Some("What the agent works on. It can push its branch and open a pull request.")))
            .child(field(&theme, "Agent", agent, None))
            .child(field(&theme, "On", place, None));
        frame(&theme, "New box", Some("An agent works on the task by itself, in a container of its own with its permission prompts off. The box pauses when it's been quiet a while and picks up again when you ask it more.".into()))
            .child(div().id("bx-body").max_h(px(body_max)).overflow_y_scroll().child(fields))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                Button::new("bx-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                Button::new("bx-start", "Start").style(ButtonStyle::Primary).icon("box").disabled(!any).on_click(cx.listener(|this, _, _, cx| this.submit(cx))).into_any_element(),
            ]))
            .font_weight(FontWeight::NORMAL)
    }
}
