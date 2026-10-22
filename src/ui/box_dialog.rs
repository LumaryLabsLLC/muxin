//! New box: a task for an agent that works on it by itself, in a container
//! of its own on one of your machines, from one of your repositories or a
//! checkout on that machine (a local project).

use std::sync::Arc;

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use super::dialogs::{DialogEvent, MODAL_CHROME, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle};
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::Engine;
use crate::fleet::boxes::{BOX_AGENTS, CreateBox};
use crate::fleet::model::{Checkout, Machine};
use crate::github::hub;

/// How many of each the Repository field suggests.
const SHOW_CHECKOUTS: usize = 4;
const SHOW_REPOS: usize = 6;

pub struct BoxDialog {
    engine: Arc<Engine>,
    repo: Entity<TextInput>,
    task: Entity<TextInput>,
    agent: String,
    /// None: the machine with Docker and the most free memory.
    machine: Option<String>,
    /// Your GitHub repositories (favorites first), once they're read.
    repos: Option<Vec<hub::Repo>>,
    /// A checkout picked from the list: the box goes on its machine, made from it.
    checkout: Option<Checkout>,
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
        let repo = cx.new(|cx| TextInput::new(cx, "Search your repositories and checkouts, or owner/name, a git URL, a folder").mono().with_text(repo.unwrap_or_default()));
        let task = cx.new(|cx| TextInput::new(cx, "Fix the login redirect loop, add a test, and open a pull request").with_text(task.unwrap_or_default()));
        let mut subs = Vec::new();
        for i in [&repo, &task] {
            subs.push(cx.subscribe_in(i, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
                InputEvent::Submit => this.submit(cx),
                InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
                InputEvent::Changed => {
                    this.error = None;
                    // Typed over the checkout picked: it's what's typed now.
                    let typed = this.repo.read(cx).text().trim().to_string();
                    if this.checkout.as_ref().is_some_and(|c| c.path != typed) {
                        this.checkout = None;
                    }
                    cx.notify()
                }
            }));
        }
        // Your repositories, and the checkouts on the machines boxes can run on.
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move {
                let s = engine.current();
                let unread: Vec<String> = s.machines.iter().filter(|m| can_run(m) && engine.checkouts_of(&m.id).is_empty()).map(|m| m.id.clone()).collect();
                futures::future::join_all(unread.iter().map(|id| engine.refresh_checkouts(id))).await;
                crate::api::github_repos(&engine).await.ok()
            }
        });
        cx.spawn(async move |this, cx| {
            let repos = job.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                this.repos = Some(repos.unwrap_or_default());
                cx.notify()
            });
        })
        .detach();
        BoxDialog { engine, repo, task, agent: "claude".into(), machine: None, repos: None, checkout: None, error: None, _subscriptions: subs }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        if self.repo.read(cx).text().trim().is_empty() { self.repo.focus_handle(cx) } else { self.task.focus_handle(cx) }
    }

    fn pick_repo(&mut self, name: String, cx: &mut Context<Self>) {
        self.checkout = None;
        self.repo.update(cx, |i, cx| i.set_text(name, cx));
        cx.notify();
    }

    fn pick_checkout(&mut self, c: Checkout, cx: &mut Context<Self>) {
        self.machine = Some(c.machine_id.clone());
        let path = c.path.clone();
        self.checkout = Some(c);
        self.repo.update(cx, |i, cx| i.set_text(path, cx));
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.read(cx).text().trim().to_string();
        let task = self.task.read(cx).text().trim().to_string();
        if repo.is_empty() {
            self.error = Some("Which repository? Pick one, or type owner/name, a git URL or a folder.".into());
            cx.notify();
            return;
        }
        if task.is_empty() {
            self.error = Some("What should the agent do?".into());
            cx.notify();
            return;
        }
        let machine = self.checkout.as_ref().map(|c| c.machine_id.clone()).or_else(|| self.machine.clone());
        let req = CreateBox { repo, prompt: task, agent: self.agent.clone(), machine, ..Default::default() };
        match self.engine.box_create(req) {
            Ok((op, _)) => cx.emit(DialogEvent::BoxStarted(op)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    /// The Repository field's suggestions: checkouts on the machines boxes
    /// can run on, then your GitHub repositories, matching what's typed.
    fn suggestions(&self, theme: &Theme, typed: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let q = typed.to_lowercase();
        // Picked (or typed exactly): nothing more to suggest.
        if self.checkout.as_ref().is_some_and(|c| c.path == typed) || self.repos.iter().flatten().any(|r| r.full_name.eq_ignore_ascii_case(typed)) {
            return None;
        }
        let s = self.engine.current();
        let machine_name = |id: &str| s.machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.to_string());
        let runs = |id: &str| s.machines.iter().any(|m| m.id == id && can_run(m));
        let checkouts: Vec<Checkout> = s
            .checkouts
            .iter()
            .filter(|c| runs(&c.machine_id))
            .filter(|c| q.is_empty() || c.path.to_lowercase().contains(&q) || c.repo.as_deref().is_some_and(|r| r.to_lowercase().contains(&q)) || machine_name(&c.machine_id).to_lowercase().contains(&q))
            .take(SHOW_CHECKOUTS)
            .cloned()
            .collect();
        let repos: Vec<hub::Repo> = self.repos.iter().flatten().filter(|r| !r.archived && (q.is_empty() || r.full_name.to_lowercase().contains(&q))).take(SHOW_REPOS).cloned().collect();
        let row = |id: SharedString, icon: &'static str, title: String, detail: String| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .py(px(5.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|st| st.bg(theme.ghost_hover))
                .child(kit::icon(icon, 13., theme.text_placeholder))
                .child(div().flex_none().max_w(px(260.)).truncate().font_family(mono_font()).text_size(px(12.)).text_color(theme.text).child(title))
                .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(detail))
        };
        let heading = |text: &'static str| div().px(px(8.)).pt(px(4.)).text_size(px(11.)).text_color(theme.text_placeholder).child(text);
        let mut list: Vec<AnyElement> = Vec::new();
        if !checkouts.is_empty() {
            list.push(heading("On your machines").into_any_element());
            for (i, c) in checkouts.into_iter().enumerate() {
                let mut detail = vec![machine_name(&c.machine_id)];
                if !c.branch.is_empty() {
                    detail.push(c.branch.clone());
                }
                if c.dirty > 0 {
                    detail.push(format!("{} uncommitted", c.dirty));
                }
                if let Some(r) = &c.repo {
                    detail.push(r.clone());
                }
                let pick = c.clone();
                list.push(
                    row(SharedString::from(format!("bx-co-{i}")), "folder", c.path.clone(), detail.join(" · "))
                        .on_click(cx.listener(move |this, _, _, cx| this.pick_checkout(pick.clone(), cx)))
                        .into_any_element(),
                );
            }
        }
        match &self.repos {
            None => list.push(div().px(px(8.)).py(px(4.)).text_size(px(11.5)).text_color(theme.text_placeholder).child("Reading your repositories…").into_any_element()),
            Some(_) if repos.is_empty() => {}
            Some(_) => {
                list.push(heading("On GitHub").into_any_element());
                for r in repos {
                    let mut detail = Vec::new();
                    if r.favorite {
                        detail.push("favorite".to_string());
                    }
                    if r.private {
                        detail.push("private".into());
                    }
                    if let Some(d) = r.description.clone().filter(|d| !d.is_empty()) {
                        detail.push(d);
                    }
                    let name = r.full_name.clone();
                    list.push(
                        row(SharedString::from(format!("bx-gh-{}", r.full_name)), "agent-github", r.full_name.clone(), detail.join(" · "))
                            .on_click(cx.listener(move |this, _, _, cx| this.pick_repo(name.clone(), cx)))
                            .into_any_element(),
                    );
                }
            }
        }
        (!list.is_empty()).then(|| div().flex().flex_col().gap(px(1.)).p(px(4.)).rounded(px(8.)).border_1().border_color(theme.border_variant).bg(theme.surface).children(list).into_any_element())
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

        let typed = self.repo.read(cx).text().trim().to_string();
        let picked = self.checkout.clone().filter(|c| c.path == typed);
        let note = match &picked {
            Some(c) => Some(format!(
                "A checkout on {}: the box starts from it{}, on a branch of its own. Your checkout isn't touched.",
                s.machines.iter().find(|m| m.id == c.machine_id).map(|m| m.name.clone()).unwrap_or_else(|| c.machine_id.clone()),
                if c.dirty > 0 { format!(", with its {} uncommitted files", c.dirty) } else { String::new() }
            )),
            None if typed.starts_with('/') || typed.starts_with("~/") => Some("A folder: a git checkout on the machine picked below, which the box starts from (with its uncommitted changes).".to_string()),
            None => None,
        };
        let repo = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(self.repo.clone())
            .children(self.suggestions(&theme, &typed, cx))
            .children(note.map(|n| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(n)));

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

        // Where: the best machine, or one picked (a checkout's own, when one is).
        let fixed = picked.as_ref().map(|c| c.machine_id.clone());
        let mut machines: Vec<AnyElement> = Vec::new();
        if fixed.is_none() {
            machines.push(
                base("bx-m-auto".into(), self.machine.is_none(), true)
                    .child("Best one")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.machine = None;
                        cx.notify()
                    }))
                    .into_any_element(),
            );
        }
        machines.extend(s.machines.iter().filter(|m| m.status == "online" && !m.platform.starts_with("windows")).filter(|m| fixed.as_ref().is_none_or(|f| *f == m.id)).map(|m| {
            let id = m.id.clone();
            let ok = can_run(m);
            let (from, to) = super::format::machine_tile(m);
            base(SharedString::from(format!("bx-m-{id}")), self.machine.as_deref() == Some(id.as_str()) || fixed.as_deref() == Some(id.as_str()), ok)
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
            .child(div().text_size(px(11.5)).text_color(if any { theme.text_placeholder } else { theme.warning }).child(match (any, fixed.is_some()) {
                (false, _) => "Boxes run in Docker: install it on a Linux machine or a Mac (Docker Desktop, OrbStack or Colima).",
                (true, true) => "On the checkout's machine, in a Docker container there. The first box on a machine builds Muxin's image (a few minutes).",
                (true, false) => "In a Docker container there, on a new branch of a fresh checkout. The first box on a machine builds Muxin's image (a few minutes).",
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
