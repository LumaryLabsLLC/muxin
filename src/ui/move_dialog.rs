//! Move a session to another machine: its work goes along (commits that aren't
//! on origin, uncommitted changes, new files) and an agent there picks it up,
//! with a note of the conversation so far (see fleet::sessions::handoff).

use std::sync::Arc;

use gpui::{AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, SharedString, Window, div, prelude::*, px};

use super::dialogs::{DialogEvent, field, footer, frame};
use super::kit::{self, Button, ButtonStyle};
use super::theme::ActiveTheme;
use crate::fleet::Engine;
use crate::fleet::sessions::{AGENTS, SessionInfo};

pub struct MoveDialog {
    engine: Arc<Engine>,
    session: SessionInfo,
    /// The machine it moves to.
    machine: String,
    agent: String,
    keep: bool,
    focus: FocusHandle,
}

impl EventEmitter<DialogEvent> for MoveDialog {}

impl MoveDialog {
    pub fn new(engine: Arc<Engine>, session: SessionInfo, machine: String, cx: &mut Context<Self>) -> Self {
        let there = agents_on(&engine, &machine);
        // The same agent when it's there too.
        let agent = if there.contains(&session.agent.as_str()) { session.agent.clone() } else { there.first().map(|a| a.to_string()).unwrap_or_else(|| session.agent.clone()) };
        MoveDialog { engine, session, machine, agent, keep: false, focus: cx.focus_handle() }
    }

    pub fn first_focus(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Focusable for MoveDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The agent CLIs Muxin found on a machine.
fn agents_on(engine: &Engine, machine: &str) -> Vec<&'static str> {
    let s = engine.current();
    let found: Vec<String> = s.machines.iter().find(|m| m.id == machine).map(|m| m.agents.clone()).unwrap_or_default();
    AGENTS.iter().filter(|a| !a.command.is_empty() && found.iter().any(|f| f == a.id)).map(|a| a.id).collect()
}

impl Render for MoveDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = self.engine.current();
        let there = s.machines.iter().find(|m| m.id == self.machine).map(|m| m.name.clone()).unwrap_or_else(|| self.machine.clone());
        let here = s.machines.iter().find(|m| m.id == self.session.machine_id).map(|m| m.name.clone()).unwrap_or_default();
        let quiet = |text: String| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(text);
        let chip = |id: SharedString, selected: bool| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(9.))
                .pl(px(4.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(crate::ui::kit::corner(6.))
                .border_1()
                .text_size(px(12.5))
                .cursor_pointer()
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .text_color(if selected { theme.text } else { theme.text_muted })
        };

        let agents: Vec<AnyElement> = agents_on(&self.engine, &self.machine)
            .into_iter()
            .filter_map(crate::fleet::sessions::agent)
            .map(|a| {
                let (glyph, (from, to)) = kit::agent_look(a.id);
                let id = a.id.to_string();
                chip(SharedString::from(format!("mv-a-{id}")), self.agent == id)
                    .child(kit::tile(glyph, from, to, 18.))
                    .child(a.name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agent = id.clone();
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        let none = agents.is_empty();
        let agent = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().flex_wrap().gap(px(6.)).children(agents))
            .when(none, |d| d.child(quiet(format!("Muxin hasn't found an agent CLI on {there} yet: install one there (Claude Code, Codex, …).")))) ;

        // What goes along, and where it lands: the session as it is now (its git may come in after the dialog opens).
        let current = s.sessions.iter().find(|x| x.id == self.session.id);
        let git = current.and_then(|x| x.git.as_ref()).or(self.session.git.as_ref());
        let what = match git {
            Some(g) => {
                let mut parts = vec![if g.branch.is_empty() { "its commits".to_string() } else { format!("branch {}", g.branch) }];
                if g.uncommitted > 0 {
                    parts.push(format!("{} uncommitted file{}", g.uncommitted, if g.uncommitted == 1 { "" } else { "s" }));
                }
                format!("{}, with its commits that aren't on GitHub yet and its new files.", parts.join(", "))
            }
            None => "Its commits that aren't on its remote yet, uncommitted changes and new files (a git checkout's).".to_string(),
        };
        let checkout = git.and_then(|g| g.repo.as_ref()).and_then(|r| s.checkouts.iter().find(|c| c.machine_id == self.machine && c.repo.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(r))));
        let lands = match (checkout, git.and_then(|g| g.repo.as_ref())) {
            (Some(c), _) => format!("A new worktree of {} on {there}, a folder of its own on the same branch.", c.path),
            (None, Some(r)) => format!("A fresh clone of {r} on {there} (with your GitHub login), on the same branch."),
            (None, None) => format!("A copy of this repository on {there}: from its remote, or its whole history when it has none."),
        };
        let name = crate::fleet::sessions::agent(&self.agent).map(|k| k.name).unwrap_or("The agent");
        let keep = self.keep;

        frame(
            &theme,
            format!("Move to {there}"),
            Some(format!("{name} takes over on {there}, with a note of the conversation so far. The folder on {here} stays as it is.").into()),
        )
        .child(field(&theme, "Agent", agent, None))
        .child(field(&theme, "What goes along", quiet(what), None))
        .child(field(&theme, "Where it lands", quiet(lands), Some("It shows under Boxes › Agents' workspaces, to clean up when the work's done.")))
        .child(kit::checkbox(&theme, "mv-keep", keep, format!("Keep this session running on {here} too"), cx.listener(|this, value: &bool, _, cx| {
            this.keep = *value;
            cx.notify()
        })))
        .child(footer(vec![
            Button::new("mv-cancel", "Cancel").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
            Button::new("mv-go", "Move")
                .icon("arrow-right-left")
                .style(ButtonStyle::Primary)
                .disabled(none)
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.emit(DialogEvent::Handoff { session: this.session.id.clone(), agent: this.agent.clone(), machine: Some(this.machine.clone()), keep: this.keep })
                }))
                .into_any_element(),
        ]))
    }
}
