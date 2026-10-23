//! Boxes: tasks handed to agents that work on them by themselves, each in a
//! container of its own on one of your machines. Lists them and opens their
//! agent's terminal (starting a paused one again).

use std::collections::HashSet;

use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*, px};

use super::dialogs::ConfirmDialog;
use super::format;
use super::kit::{self, Button, ButtonStyle, card, section_title};
use super::overview::page;
use super::root::{Modal, Root, Route};
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::boxes::BoxInfo;

pub const TILE: (u32, u32) = (0xffb36b, 0xe0702c);

/// Per-view state for the Boxes page, kept on the root view.
#[derive(Default)]
pub struct BoxUi {
    /// Boxes being started again to open them.
    pub opening: HashSet<String>,
}

impl Root {
    pub(super) fn render_boxes(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let cards: Vec<AnyElement> = s.boxes.iter().map(|b| self.box_card(&theme, b, cx)).collect();
        let docker = s.machines.iter().any(|m| m.status == "online" && m.probe.as_ref().is_some_and(|p| p.docker.is_some()));

        // Wrapped text needs its width known before layout (GPUI 0.2): what the button leaves.
        let intro_width = (self.content_width - 150.).clamp(240., 760.);
        page("boxes-page", &self.page_scroll)
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(20.))
                    .child(
                        div().flex().flex_col().flex_none().w(px(intro_width)).gap(px(4.)).child(
                            div().text_size(px(12.5)).text_color(theme.text_muted).child(
                                "Hand a task to an agent that works on it by itself: the agent lives in the box, a container of its own on one of your machines, on a new branch of a fresh checkout, with its permission prompts off. A box pauses when its agent has been quiet for a while, and picks up where it left off when you ask it more.",
                            ),
                        ).child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(if docker {
                            "Agents make them with the box_create MCP tool or `muxin box new`."
                        } else {
                            "Boxes run in Docker: install it on a Linux machine or a Mac (Docker Desktop, OrbStack or Colima)."
                        })),
                    )
                    .child(
                        Button::new("box-new", "New box")
                            .icon("plus")
                            .style(ButtonStyle::Primary)
                            .on_click(cx.listener(|this, _, window, cx| this.show_box_dialog(Default::default(), window, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(section_title(&theme, "Boxes", Some(s.boxes.len())))
                    .children(cards)
                    .when(s.boxes.is_empty(), |d| d.child(card(&theme).child(kit::empty_state(&theme, "box", "No boxes yet. Start one, or ask an agent to.")))),
            )
            .child(self.boxes_page_workspaces(&theme, &s, cx))
    }

    fn box_card(&self, theme: &Theme, b: &BoxInfo, cx: &mut Context<Self>) -> AnyElement {
        let (glyph, (from, to)) = kit::agent_look(&b.agent);
        let opening = self.box_ui.opening.contains(&b.id);
        let running = b.state == "running";
        let machine = self.snapshot.machines.iter().find(|m| m.id == b.machine_id).map(|m| m.name.clone()).unwrap_or_else(|| b.machine_id.clone());
        let when = match b.state.as_str() {
            "paused" => format!("paused {}", format::ago(b.state_at)),
            _ => format!("started {}", format::ago(b.state_at)),
        };
        let idle = match b.idle_minutes {
            0 => "never pauses by itself".to_string(),
            m => format!("pauses after {m} quiet minutes"),
        };
        div()
            .id(SharedString::from(format!("boxcard-{}", b.id)))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .rounded(crate::ui::kit::corner(12.))
            .border_1()
            .border_color(if b.attention.is_some() { theme.warning.opacity(0.6) } else { theme.border_variant })
            .bg(theme.surface)
            .cursor_pointer()
            .hover(|st| st.border_color(theme.accent.opacity(0.55)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(kit::tile(glyph, from, to, 32.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().truncate().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child(b.name.clone()))
                                    .child(div().font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(b.id.clone())),
                            )
                            .child(div().truncate().text_size(px(12.)).text_color(theme.text_muted).child(format!("{machine}  ·  {}  ·  {}", b.repo, b.agent_name))),
                    )
                    .child(state_pill(theme, b))
                    .child({
                        let id = b.id.clone();
                        Button::new(SharedString::from(format!("box-open-{}", b.id)), if running { "Open" } else { "Resume" })
                            .icon(if running { "square-terminal" } else { "play" })
                            .loading(opening)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_box(&id, window, cx)
                            }))
                    })
                    .when(running, |d| {
                        let id = b.id.clone();
                        d.child(
                            Button::icon_only(SharedString::from(format!("box-pause-{}", b.id)), "pause")
                                .tooltip("Pause: its agent ends and its container stops; its files stay")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.pause_box(&id, cx)
                                })),
                        )
                    })
                    .child({
                        let (id, name) = (b.id.clone(), b.name.clone());
                        Button::icon_only(SharedString::from(format!("box-delete-{}", b.id)), "trash-2")
                            .style(ButtonStyle::Danger)
                            .tooltip("Delete box")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.confirm_destroy_box(&id, &name, window, cx)
                            }))
                    }),
            )
            .when(!b.task.is_empty(), |d| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(crate::ui::kit::corner(8.))
                        .bg(theme.sidebar)
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .line_clamp(2)
                        .child(b.task.clone()),
                )
            })
            // What its work changed: branch, lines, pull request.
            .child(match &b.git {
                Some(g) => super::sessions::git_line(theme, &format!("box-{}", b.id), g, true).into_any_element(),
                None => div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(kit::icon("git-branch", 12., theme.text_placeholder))
                    .child(div().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_muted).child(b.branch.clone()))
                    .into_any_element(),
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .text_size(px(11.5))
                    .text_color(theme.text_placeholder)
                    .child(format!("made {}", format::ago(b.created_at)))
                    .child(when)
                    .child(idle)
                    .child(div().flex_1())
                    .child(div().truncate().font_family(mono_font()).child(b.path.clone())),
            )
            .on_click({
                let id = b.id.clone();
                cx.listener(move |this, _, window, cx| this.open_box(&id, window, cx))
            })
            .into_any_element()
    }

    /// A paused box's agent page: what it was doing, and Resume.
    pub(super) fn paused_box(&self, theme: &Theme, b: &BoxInfo, cx: &mut Context<Self>) -> AnyElement {
        let (glyph, (from, to)) = kit::agent_look(&b.agent);
        let id = b.id.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .pb(px(80.))
            .child(kit::tile(glyph, from, to, 44.))
            .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(format!("{} is paused", b.name)))
            .child(div().max_w(px(520.)).text_size(px(12.5)).text_color(theme.text_muted).text_center().child(format!(
                "Its agent stopped {} and its container with it; its files and conversation are kept. Resume it to pick up where it left off.",
                format::ago(b.state_at)
            )))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        Button::new("paused-box-resume", "Resume")
                            .icon("play")
                            .style(ButtonStyle::Primary)
                            .loading(self.box_ui.opening.contains(&b.id))
                            .on_click(cx.listener(move |this, _, window, cx| this.open_box(&id, window, cx))),
                    )
                    .child(Button::new("paused-box-back", "All boxes").icon("arrow-left").on_click(cx.listener(|this, _, window, cx| this.navigate(Route::Boxes, window, cx)))),
            )
            .into_any_element()
    }

    /// New box, with a repository and task when something opened it with them.
    pub(super) fn show_box_dialog(&mut self, prefill: super::box_dialog::BoxPrefill, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| super::box_dialog::BoxDialog::new(engine, prefill, window, cx));
        self.show_modal(Modal::NewBox(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Opens a box's agent: its terminal while it runs, else after starting it again.
    pub(super) fn open_box(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.snapshot.boxes.iter().find(|b| b.id == id).and_then(|b| b.session_id.clone()) {
            self.navigate(Route::Session(session), window, cx);
            return;
        }
        if !self.box_ui.opening.insert(id.to_string()) {
            return;
        }
        cx.notify();
        let job = self.engine.runtime().spawn({
            let (engine, id) = (self.engine.clone(), id.to_string());
            async move { engine.box_send(&id, None).await }
        });
        let id = id.to_string();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.box_ui.opening.remove(&id);
                match result {
                    Ok(b) => match b.session_id {
                        Some(session) => this.navigate(Route::Session(session), window, cx),
                        None => this.notify_user(format!("{} is starting.", b.name), false, cx),
                    },
                    Err(e) => this.notify_user(e, true, cx),
                }
                cx.notify()
            });
        });
        self.track(task);
    }

    pub(super) fn pause_box(&mut self, id: &str, cx: &mut Context<Self>) {
        let job = self.engine.runtime().spawn({
            let (engine, id) = (self.engine.clone(), id.to_string());
            async move { engine.box_pause(&id).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| match result {
                Ok(b) => this.notify_user(format!("Paused {}", b.name), false, cx),
                Err(e) => this.notify_user(e, true, cx),
            });
        });
        self.track(task);
    }

    pub(super) fn confirm_destroy_box(&mut self, id: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let bid = id.to_string();
        // Work that isn't on GitHub goes with it.
        let unpushed = self.snapshot.boxes.iter().find(|b| b.id == id).and_then(|b| b.git.as_ref()).is_some_and(|g| g.uncommitted > 0 || g.ahead > 0 || (g.commits > 0 && g.pull.is_none()));
        let mut text = "Its agent ends, and its container and checkout are deleted from the machine. Its branch on GitHub stays.".to_string();
        if unpushed {
            text.push_str(" It has work that may not be pushed yet: it's lost.");
        }
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Delete {name}?"),
                text,
                "Delete box",
                None,
                move |_, engine| {
                    let (engine, bid) = (engine.clone(), bid.clone());
                    engine.runtime().clone().spawn(async move {
                        if let Err(e) = engine.box_destroy(&bid).await {
                            tracing::warn!("deleting {bid}: {e}");
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
}

fn state_pill(theme: &Theme, b: &BoxInfo) -> AnyElement {
    match (b.state.as_str(), b.busy, b.attention.as_deref()) {
        ("running", _, Some("waiting" | "bell")) => kit::pill("waiting for you", theme.warning).into_any_element(),
        ("running", true, _) => div()
            .flex()
            .items_center()
            .gap(px(5.))
            .child(kit::spinner(SharedString::from(format!("box-spin-{}", b.id)), 11., theme.accent))
            .child(kit::pill("working", theme.accent))
            .into_any_element(),
        ("running", false, _) => kit::pill("idle", theme.success).into_any_element(),
        ("starting", _, _) => div()
            .flex()
            .items_center()
            .gap(px(5.))
            .child(kit::spinner(SharedString::from(format!("box-spin-{}", b.id)), 11., theme.accent))
            .child(kit::pill("starting", theme.accent))
            .into_any_element(),
        _ => kit::pill("paused", theme.text_placeholder).into_any_element(),
    }
}
