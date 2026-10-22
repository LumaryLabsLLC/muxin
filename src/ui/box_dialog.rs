//! New box: a task for an agent that works on it by itself, in a container
//! of its own on one of your machines, from one of your GitHub repositories
//! or a folder on that machine (a local project).

use std::sync::Arc;

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, SharedString, Subscription, Window, actions, div, prelude::*, px};

use super::dialogs::{DialogEvent, MODAL_CHROME, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle};
use super::theme::{ActiveTheme, Theme};
use crate::fleet::Engine;
use crate::fleet::boxes::{BOX_AGENTS, CreateBox};
use crate::fleet::model::{Checkout, Machine};
use crate::github::hub;

actions!(box_dialog, [PickUp, PickDown]);

const CONTEXT: &str = "BoxDialog";
/// Results shown under the Repository field at most.
const SHOWN: usize = 6;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("up", PickUp, Some(CONTEXT)), KeyBinding::new("down", PickDown, Some(CONTEXT))]);
}

/// Where the box's code comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// One of your GitHub repositories (or any git URL), cloned fresh.
    GitHub,
    /// A folder on one of your machines (a checkout there): the box goes on that machine.
    Folder,
}

/// A result under the Repository field.
#[derive(Clone)]
enum Found {
    Repo(hub::Repo),
    Checkout(Checkout),
}

pub struct BoxDialog {
    engine: Arc<Engine>,
    source: Source,
    repo: Entity<TextInput>,
    task: Entity<TextInput>,
    agent: String,
    /// Where a GitHub box runs; None: the machine with Docker and the most free memory.
    machine: Option<String>,
    /// The machine whose folder a box starts from.
    folder_machine: Option<String>,
    /// Your GitHub repositories (favorites first), once they're read.
    repos: Option<Vec<hub::Repo>>,
    /// The result the arrows are on.
    highlight: usize,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for BoxDialog {}

/// A machine a box can run on: online, not Windows, with Docker.
fn can_run(m: &Machine) -> bool {
    m.status == "online" && !m.platform.starts_with("windows") && m.probe.as_ref().is_some_and(|p| p.docker.is_some())
}

/// "~/Developer/app" for "/Users/me/Developer/app" (or /home/me/...).
fn tilde(path: &str) -> String {
    for home in ["/Users/", "/home/"] {
        if let Some(rest) = path.strip_prefix(home) {
            return match rest.split_once('/') {
                Some((_, under)) => format!("~/{under}"),
                None => "~".into(),
            };
        }
    }
    path.to_string()
}

/// A folder's own name: "app" for "~/Developer/app".
fn folder_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

const GITHUB_HINT: &str = "Search your repositories, or owner/name or a git URL";
const FOLDER_HINT: &str = "Search its checkouts, or a folder like ~/code/app";

impl BoxDialog {
    /// `repo` and `task`, when something opened it with them (an issue to fix).
    pub fn new(engine: Arc<Engine>, repo: Option<String>, task: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let repo = cx.new(|cx| TextInput::new(cx, GITHUB_HINT).mono().with_text(repo.unwrap_or_default()));
        let task = cx.new(|cx| TextInput::new(cx, "Fix the login redirect loop, add a test, and open a pull request").with_text(task.unwrap_or_default()));
        let subs = vec![
            cx.subscribe_in(&repo, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
                // Enter on a result picks it; on what's picked, starts the box.
                InputEvent::Submit => match this.found(cx).get(this.highlight).cloned() {
                    Some(f) => this.pick(f, cx),
                    None => this.submit(cx),
                },
                InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
                InputEvent::Changed => {
                    this.error = None;
                    this.highlight = 0;
                    cx.notify()
                }
            }),
            cx.subscribe_in(&task, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
                InputEvent::Submit => this.submit(cx),
                InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
                InputEvent::Changed => {
                    this.error = None;
                    cx.notify()
                }
            }),
        ];
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
        BoxDialog {
            engine,
            source: Source::GitHub,
            repo,
            task,
            agent: "claude".into(),
            machine: None,
            folder_machine: None,
            repos: None,
            highlight: 0,
            error: None,
            _subscriptions: subs,
        }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        if self.repo.read(cx).text().trim().is_empty() { self.repo.focus_handle(cx) } else { self.task.focus_handle(cx) }
    }

    fn set_source(&mut self, source: Source, window: &mut Window, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        self.source = source;
        self.highlight = 0;
        // A folder's machine: one with checkouts, else any that can run a box.
        if source == Source::Folder && self.folder_machine.is_none() {
            let s = self.engine.current();
            let runs: Vec<&Machine> = s.machines.iter().filter(|m| can_run(m)).collect();
            self.folder_machine = runs.iter().find(|m| s.checkouts.iter().any(|c| c.machine_id == m.id)).or(runs.first()).map(|m| m.id.clone());
        }
        let hint = if source == Source::GitHub { GITHUB_HINT } else { FOLDER_HINT };
        self.repo.update(cx, |i, cx| {
            i.set_text("", cx);
            i.set_placeholder(hint, cx)
        });
        window.focus(&self.repo.focus_handle(cx));
        cx.notify();
    }

    /// The results for what's typed (none once one is picked).
    fn found(&self, cx: &App) -> Vec<Found> {
        let typed = self.repo.read(cx).text().trim().to_string();
        let q = typed.to_lowercase();
        match self.source {
            Source::GitHub => {
                // Nothing typed: the favorites show instead.
                let repos = self.repos.iter().flatten();
                if q.is_empty() || repos.clone().any(|r| r.full_name.eq_ignore_ascii_case(&typed)) {
                    return Vec::new();
                }
                let mut hits: Vec<&hub::Repo> = repos.filter(|r| !r.archived && r.full_name.to_lowercase().contains(&q)).collect();
                // Names that start with it first ("open" finds acme/openrunner before acme/reopen).
                hits.sort_by_key(|r| !r.full_name.to_lowercase().split('/').any(|part| part.starts_with(&q)));
                hits.into_iter().take(SHOWN).cloned().map(Found::Repo).collect()
            }
            Source::Folder => {
                let s = self.engine.current();
                let Some(machine) = &self.folder_machine else { return Vec::new() };
                let here: Vec<&Checkout> = s.checkouts.iter().filter(|c| &c.machine_id == machine).collect();
                if here.iter().any(|c| c.path == typed) {
                    return Vec::new();
                }
                let mut hits: Vec<&Checkout> =
                    here.into_iter().filter(|c| q.is_empty() || c.path.to_lowercase().contains(&q) || c.repo.as_deref().is_some_and(|r| r.to_lowercase().contains(&q))).collect();
                hits.sort_by_key(|c| (!folder_name(&c.path).to_lowercase().starts_with(&q), std::cmp::Reverse(c.last_commit_at)));
                hits.into_iter().take(SHOWN).cloned().map(Found::Checkout).collect()
            }
        }
    }

    fn pick(&mut self, found: Found, cx: &mut Context<Self>) {
        let text = match found {
            Found::Repo(r) => r.full_name,
            Found::Checkout(c) => c.path,
        };
        self.repo.update(cx, |i, cx| i.set_text(text, cx));
        self.highlight = 0;
        cx.notify();
    }

    fn move_highlight(&mut self, by: isize, cx: &mut Context<Self>) {
        let n = self.found(cx).len();
        if n > 0 {
            self.highlight = (self.highlight as isize + by).rem_euclid(n as isize) as usize;
            cx.notify();
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.read(cx).text().trim().to_string();
        let task = self.task.read(cx).text().trim().to_string();
        if repo.is_empty() {
            self.error = Some(match self.source {
                Source::GitHub => "Which repository? Search for one, or type owner/name or a git URL.".into(),
                Source::Folder => "Which folder? Pick a checkout, or type its folder.".into(),
            });
            cx.notify();
            return;
        }
        if task.is_empty() {
            self.error = Some("What should the agent do?".into());
            cx.notify();
            return;
        }
        let machine = match self.source {
            Source::GitHub => self.machine.clone(),
            Source::Folder => self.folder_machine.clone(),
        };
        if self.source == Source::Folder && machine.is_none() {
            self.error = Some("Which machine is the folder on?".into());
            cx.notify();
            return;
        }
        let req = CreateBox { repo, prompt: task, agent: self.agent.clone(), machine, ..Default::default() };
        match self.engine.box_create(req) {
            Ok((op, _)) => cx.emit(DialogEvent::BoxStarted(op)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    /// One result: its name first, then where it is.
    fn result_row(&self, theme: &Theme, i: usize, found: &Found, cx: &mut Context<Self>) -> AnyElement {
        let (icon, name, detail) = match found {
            Found::Repo(r) => {
                let mut detail = Vec::new();
                if r.private {
                    detail.push("private".to_string());
                }
                if let Some(d) = r.description.clone().filter(|d| !d.is_empty()) {
                    detail.push(d);
                }
                ("agent-github", r.full_name.clone(), detail.join(" · "))
            }
            Found::Checkout(c) => {
                let mut detail = vec![tilde(&c.path)];
                if !c.branch.is_empty() {
                    detail.push(c.branch.clone());
                }
                if c.dirty > 0 {
                    detail.push(format!("{} uncommitted", c.dirty));
                }
                ("folder", folder_name(&c.path).to_string(), detail.join(" · "))
            }
        };
        let on = i == self.highlight;
        let pick = found.clone();
        div()
            .id(("bx-found", i))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(8.))
            .rounded(px(6.))
            .cursor_pointer()
            .when(on, |d| d.bg(theme.element_selected))
            .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::icon(icon, 14., theme.text_muted))
            .child(div().flex_none().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.text).child(name))
            .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(detail))
            .on_click(cx.listener(move |this, _, _, cx| this.pick(pick.clone(), cx)))
            .into_any_element()
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
        let chip = |id: SharedString, selected: bool, enabled: bool| {
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
        let machine_name = |id: &str| s.machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.to_string());
        let typed = self.repo.read(cx).text().trim().to_string();
        let quiet = |text: String| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(text);

        // Where the code comes from.
        let sources = div()
            .flex()
            .gap(px(6.))
            .child(chip("bx-src-gh".into(), self.source == Source::GitHub, true).pl(px(6.)).child(kit::icon("agent-github", 13., theme.text_muted)).child("GitHub").on_click(cx.listener(
                |this, _, window, cx| this.set_source(Source::GitHub, window, cx),
            )))
            .child(chip("bx-src-folder".into(), self.source == Source::Folder, true).pl(px(6.)).child(kit::icon("folder", 13., theme.text_muted)).child("A folder on a machine").on_click(
                cx.listener(|this, _, window, cx| this.set_source(Source::Folder, window, cx)),
            ));

        let found = self.found(cx);
        let rows: Vec<AnyElement> = found.iter().enumerate().map(|(i, f)| self.result_row(&theme, i, f, cx)).collect();
        let results = (!rows.is_empty()).then(|| div().flex().flex_col().p(px(4.)).rounded(px(8.)).border_1().border_color(theme.border_variant).bg(theme.surface).children(rows));

        let code: AnyElement = match self.source {
            Source::GitHub => {
                // Nothing typed yet: your favorites, one click away.
                let favorites: Vec<AnyElement> = if typed.is_empty() {
                    self.repos
                        .iter()
                        .flatten()
                        .filter(|r| r.favorite)
                        .take(6)
                        .map(|r| {
                            let name = r.full_name.clone();
                            chip(SharedString::from(format!("bx-fav-{name}")), false, true)
                                .child(r.full_name.clone())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.repo.update(cx, |i, cx| i.set_text(name.clone(), cx));
                                    cx.notify()
                                }))
                                .into_any_element()
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let status = match &self.repos {
                    None if !typed.is_empty() => Some(quiet("Reading your repositories…".into())),
                    Some(_) if !typed.is_empty() && found.is_empty() && !self.repos.iter().flatten().any(|r| r.full_name.eq_ignore_ascii_case(&typed)) => {
                        Some(quiet("None of your repositories match; it's used as typed (owner/name or a git URL).".into()))
                    }
                    _ => None,
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(self.repo.clone())
                    .children(results)
                    .when(!favorites.is_empty(), |d| d.child(div().flex().flex_wrap().items_center().gap(px(6.)).child(quiet("Favorites".into())).children(favorites)))
                    .children(status)
                    .into_any_element()
            }
            Source::Folder => {
                let machines: Vec<AnyElement> = s
                    .machines
                    .iter()
                    .filter(|m| m.status == "online" && !m.platform.starts_with("windows"))
                    .map(|m| {
                        let id = m.id.clone();
                        let ok = can_run(m);
                        let (from, to) = super::format::machine_tile(m);
                        chip(SharedString::from(format!("bx-fm-{id}")), self.folder_machine.as_deref() == Some(id.as_str()), ok)
                            .pl(px(4.))
                            .child(kit::tile(super::format::machine_icon(m), from, to, 18.))
                            .child(m.name.clone())
                            .when(!ok, |d| d.child(div().text_size(px(11.)).child("no Docker")))
                            .when(ok, |d| {
                                d.on_click(cx.listener(move |this, _, _, cx| {
                                    this.folder_machine = Some(id.clone());
                                    this.highlight = 0;
                                    cx.notify()
                                }))
                            })
                            .into_any_element()
                    })
                    .collect();
                let checkouts = self.folder_machine.as_ref().map(|m| s.checkouts.iter().filter(|c| &c.machine_id == m).count()).unwrap_or(0);
                let picked = self.folder_machine.as_ref().and_then(|m| s.checkouts.iter().find(|c| &c.machine_id == m && c.path == typed));
                let note = match (picked, &self.folder_machine) {
                    (Some(c), _) => Some(format!(
                        "The box starts from this checkout as it is now{}, on a branch of its own. Your checkout isn't touched.",
                        if c.dirty > 0 { format!(", with its {} uncommitted files", c.dirty) } else { String::new() }
                    )),
                    (None, Some(m)) if !typed.is_empty() && found.is_empty() => Some(format!("A git checkout in this folder on {}: the box starts from it, with its uncommitted changes.", machine_name(m))),
                    (None, Some(m)) if checkouts == 0 => Some(format!("Muxin hasn't found checkouts on {} (Settings › General › Code folders): type the folder.", machine_name(m))),
                    _ => None,
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().flex().flex_wrap().gap(px(6.)).children(machines))
                    .child(self.repo.clone())
                    .children(results)
                    .children(note.map(quiet))
                    .into_any_element()
            }
        };

        let agents: Vec<AnyElement> = BOX_AGENTS
            .iter()
            .filter_map(|a| crate::fleet::sessions::agent(a))
            .map(|a| {
                let id = a.id.to_string();
                let (glyph, (from, to)) = kit::agent_look(a.id);
                chip(SharedString::from(format!("bx-a-{id}")), self.agent == id, true)
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
            .child(quiet(format!("{name} is signed in as it is on that machine. If it isn't signed in there, sign it in once from the box's terminal.")));

        // Where a GitHub box runs (a folder's box runs on the folder's machine).
        let any = s.machines.iter().any(can_run);
        let place = (self.source == Source::GitHub).then(|| {
            let mut machines: Vec<AnyElement> = vec![chip("bx-m-auto".into(), self.machine.is_none(), true)
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
                chip(SharedString::from(format!("bx-m-{id}")), self.machine.as_deref() == Some(id.as_str()), ok)
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
            div().flex().flex_col().gap(px(8.)).child(div().flex().flex_wrap().gap(px(6.)).children(machines)).child(
                div().text_size(px(11.5)).text_color(if any { theme.text_placeholder } else { theme.warning }).child(if any {
                    "In a Docker container there, on a new branch of a fresh clone. The first box on a machine builds Muxin's image (a few minutes)."
                } else {
                    "Boxes run in Docker: install it on a Linux machine or a Mac (Docker Desktop, OrbStack or Colima)."
                }),
            )
        });

        let body_max = (f32::from(window.viewport_size().height) - MODAL_CHROME).max(220.);
        let fields = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(field(&theme, "Code", div().flex().flex_col().gap(px(10.)).child(sources).child(code), None))
            .child(field(&theme, "Task", self.task.clone(), Some("What the agent works on. It can push its branch and open a pull request.")))
            .child(field(&theme, "Agent", agent, None))
            .children(place.map(|p| field(&theme, "On", p, None)));
        frame(&theme, "New box", Some("An agent works on the task by itself, in a container of its own with its permission prompts off. The box pauses when it's been quiet a while and picks up again when you ask it more.".into()))
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &PickUp, _, cx| this.move_highlight(-1, cx)))
            .on_action(cx.listener(|this, _: &PickDown, _, cx| this.move_highlight(1, cx)))
            .child(div().id("bx-body").max_h(px(body_max)).overflow_y_scroll().child(fields))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                Button::new("bx-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                Button::new("bx-start", "Start").style(ButtonStyle::Primary).icon("box").disabled(!any).on_click(cx.listener(|this, _, _, cx| this.submit(cx))).into_any_element(),
            ]))
            .font_weight(FontWeight::NORMAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_show_by_name_with_short_paths() {
        assert_eq!(tilde("/Users/adib/Developer/opensource/openrunner"), "~/Developer/opensource/openrunner");
        assert_eq!(tilde("/home/adib/code/api"), "~/code/api");
        assert_eq!(tilde("/srv/app"), "/srv/app");
        assert_eq!(folder_name("/Users/adib/Developer/opensource/openrunner"), "openrunner");
        assert_eq!(folder_name("~/code/api/"), "api");
    }
}
