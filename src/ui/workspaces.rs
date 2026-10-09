//! Agent workspaces: folders on fleet machines that agents check out as their
//! work environment. Lists them, shows what agents ran, and lets you step in.

use std::collections::HashSet;

use gpui::{AnyElement, ClipboardItem, Context, FontWeight, SharedString, div, prelude::*, px};

use super::format;
use super::kit::{self, Button, ButtonStyle, card, icon, kv, section_title};
use super::overview::{callout, page};
use super::root::{Root, Route};
use super::theme::{ActiveTheme, mono_font, Theme};
use crate::fleet::model::{WorkspaceCommand, WorkspaceInfo};

/// Per-view state for the workspace pages, kept on the root view.
#[derive(Default)]
pub struct WorkspaceUi {
    /// Loaded `git status` + diff per workspace id.
    pub changes: std::collections::HashMap<String, Result<String, String>>,
    pub loading_changes: HashSet<String>,
    /// History entries whose output is expanded: (workspace id, command index).
    pub expanded: HashSet<(String, usize)>,
}

impl Root {
    pub(super) fn render_workspaces(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let rows: Vec<AnyElement> = s.workspaces.iter().map(|w| self.workspace_card(&theme, w, cx)).collect();
        let allowed = s.machines.iter().filter(|m| m.workspaces_allowed).count();

        page("workspaces-page", &self.page_scroll)
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(20.))
                    .child(
                        div().flex().flex_col().flex_1().min_w(px(0.)).max_w(px(760.)).gap(px(4.)).child(
                            div().text_size(px(12.5)).text_color(theme.text_muted).child(
                                "A workspace is a folder on one of your machines, optionally a git checkout, that an agent uses as its work environment: it runs commands, edits files and syncs code there. Workspaces persist until destroyed.",
                            ),
                        ).child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(format!(
                            "{allowed} of {} machines accept workspaces. Agents create them with the workspace_create MCP tool or `muxen workspace create`.",
                            s.machines.len()
                        ))),
                    )
                    .child(
                        Button::new("ws-new", "New workspace")
                            .icon("plus")
                            .style(ButtonStyle::Primary)
                            .on_click(cx.listener(|this, _, window, cx| this.show_new_workspace(None, window, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(section_title(&theme, "Workspaces", Some(s.workspaces.len())))
                    .children(rows)
                    .when(s.workspaces.is_empty(), |d| {
                        d.child(card(&theme).child(kit::empty_state(&theme, "terminal", "No workspaces yet. Ask an agent to create one, or make one here.")))
                    }),
            )
    }

    fn workspace_card(&self, theme: &Theme, w: &WorkspaceInfo, cx: &mut Context<Self>) -> AnyElement {
        let id = w.id.clone();
        let shell = w.shell.clone();
        let last = w.history.last();
        div()
            .id(SharedString::from(format!("wscard-{}", w.id)))
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .rounded(px(12.))
            .border_1()
            .border_color(theme.border_variant)
            .bg(theme.surface)
            .cursor_pointer()
            .hover(|st| st.border_color(theme.accent.opacity(0.55)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(kit::tile("terminal", kit::tiles::WORKSPACES.0, kit::tiles::WORKSPACES.1, 32.))
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
                                    .child(div().truncate().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child(w.name.clone()))
                                    .child(div().font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(w.id.clone())),
                            )
                            .child(div().truncate().text_size(px(12.)).text_color(theme.text_muted).child(workspace_subtitle(w))),
                    )
                    .when(w.isolation == "docker", |d| d.child(docker_pill(theme, w)))
                    .children(w.owner.clone().map(|o| kit::pill(o, theme.info)))
                    .child(status_pill(theme, w))
                    .child({
                        let ws = w.clone();
                        Button::icon_only(SharedString::from(format!("ws-session-{}", w.id)), "square-terminal")
                            .tooltip("New session here")
                            .disabled(w.status != "ready")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.new_workspace_session(&ws, window, cx)
                            }))
                    })
                    .child(
                        Button::icon_only(SharedString::from(format!("ws-copy-{}", w.id)), "copy")
                            .tooltip("Copy shell command")
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(shell.clone()))),
                    )
                    .child({
                        let (wid, name) = (w.id.clone(), w.name.clone());
                        Button::icon_only(SharedString::from(format!("ws-delete-{}", w.id)), "trash-2")
                            .style(ButtonStyle::Danger)
                            .tooltip("Delete workspace")
                            .on_click(cx.listener(move |this, _, window, cx| this.confirm_destroy_workspace(&wid, &name, window, cx)))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .bg(theme.sidebar)
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .child(div().text_color(theme.text_placeholder).child("$"))
                    .child(div().flex_1().min_w(px(0.)).truncate().text_color(if last.is_some() { theme.text } else { theme.text_placeholder }).child(
                        last.map(|c| c.command.clone()).unwrap_or_else(|| "no commands yet".into()),
                    ))
                    .children(last.map(|c| exit_badge(theme, c))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .text_size(px(11.5))
                    .text_color(theme.text_placeholder)
                    .child(format!("{} command{}", w.commands, if w.commands == 1 { "" } else { "s" }))
                    .child(format!("used {}", format::ago(Some(w.last_used_at))))
                    .child(format!("created {}", format::ago(Some(w.created_at))))
                    .child(div().flex_1())
                    .child(div().truncate().font_family(mono_font()).child(w.path.clone())),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Workspace(id.clone()), window, cx)))
            .on_mouse_down(gpui::MouseButton::Right, {
                let wid = w.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Workspace(wid.clone()), cx)
            })
            .into_any_element()
    }

    pub(super) fn render_workspace(&mut self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let Some(w) = s.workspaces.iter().find(|w| w.id == id).cloned() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_placeholder)
                .child("This workspace was destroyed.")
                .into_any_element();
        };
        let shell = w.shell.clone();
        let path = w.path.clone();
        let machine_id = w.machine_id.clone();

        let header = div()
            .flex()
            .items_start()
            .justify_between()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(w.name.clone()))
                            .child(div().font_family(mono_font()).text_size(px(12.)).text_color(theme.text_placeholder).child(w.id.clone()))
                            .when(w.isolation == "docker", |d| d.child(docker_pill(&theme, &w)))
                            .child(status_pill(&theme, &w)),
                    )
                    .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(workspace_subtitle(&w))),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .child({
                        let ws = w.clone();
                        Button::new("ws-new-session", "New session")
                            .icon("square-terminal")
                            .style(ButtonStyle::Primary)
                            .disabled(w.status != "ready")
                            .on_click(cx.listener(move |this, _, window, cx| this.new_workspace_session(&ws, window, cx)))
                    })
                    .child(
                        Button::new("ws-copy-shell", "Copy shell command")
                            .icon("terminal")
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(shell.clone()))),
                    )
                    .child({
                        let wid = w.id.clone();
                        Button::new("ws-changes", "Changes")
                            .icon("git-branch")
                            .loading(self.ws_ui.loading_changes.contains(&w.id))
                            .on_click(cx.listener(move |this, _, _, cx| this.load_workspace_changes(&wid, cx)))
                    })
                    .child({
                        let wid = w.id.clone();
                        let name = w.name.clone();
                        Button::icon_only("ws-destroy", "trash-2")
                            .style(ButtonStyle::Danger)
                            .tooltip("Destroy workspace")
                            .disabled(w.status == "destroying")
                            .on_click(cx.listener(move |this, _, window, cx| this.confirm_destroy_workspace(&wid, &name, window, cx)))
                    }),
            );

        let details = card(&theme)
            .px(px(14.))
            .py(px(8.))
            .child(kv(
                &theme,
                "Machine",
                div()
                    .id("ws-machine-link")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_color(theme.text_accent)
                    .child(w.machine_name.clone())
                    .child(div().font_family(mono_font()).text_color(theme.text_placeholder).child(format!("{} · {}", w.host, w.platform)))
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(machine_id.clone()), window, cx))),
            ))
            .child(kv(
                &theme,
                "Folder",
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(kit::mono(w.path.clone()))
                    .child(Button::icon_only("ws-copy-path", "copy").tooltip("Copy path").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))
                    })),
            ))
            .child(kv(
                &theme,
                "Environment",
                if w.isolation == "docker" {
                    kit::mono(format!(
                        "Docker · {}{}",
                        w.image.clone().unwrap_or_default(),
                        w.container.as_ref().map(|c| format!(" · {c}")).unwrap_or_default()
                    ))
                    .into_any_element()
                } else {
                    div().child("Directly on the machine").into_any_element()
                },
            ))
            .children(w.repo.clone().map(|r| kv(&theme, "Repository", kit::mono(r))))
            .children(w.git_ref.clone().map(|r| kv(&theme, "Checked out", kit::mono(r))))
            .children(w.branch.clone().map(|b| kv(&theme, "Branch", kit::mono(b))))
            .children(w.owner.clone().map(|o| kv(&theme, "Created by", o)))
            .child(kv(&theme, "Created", format!("{} · last used {}", format::ago(Some(w.created_at)), format::ago(Some(w.last_used_at)))));

        let mut body = page("workspace-page", &self.page_scroll).child(header);
        if let Some(err) = &w.error {
            body = body.child(callout(&theme, "error", "Workspace error", err.clone(), None));
        }
        body = body.child(details);
        // Sessions working in this folder.
        let here: Vec<_> = self.snapshot.sessions.iter().filter(|x| x.machine_id == w.machine_id && x.cwd.starts_with(&w.path)).cloned().collect();
        if !here.is_empty() {
            let rows: Vec<AnyElement> = here.iter().enumerate().map(|(i, x)| self.machine_session_row(&theme, x, i, cx)).collect();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(kit::section_title(&theme, "Sessions", Some(here.len())))
                    .child(card(&theme).children(rows)),
            );
        }

        if let Some(result) = self.ws_ui.changes.get(&w.id) {
            let (text, failed) = match result {
                Ok(t) => (t.clone(), false),
                Err(e) => (e.clone(), true),
            };
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(section_title(&theme, "Changes", None))
                    .child(
                        card(&theme)
                            .p(px(12.))
                            .font_family(mono_font())
                            .text_size(px(11.5))
                            .line_height(px(18.))
                            .children(text.lines().take(400).map(|l| diff_line(&theme, l, failed))),
                    ),
            );
        }

        let history: Vec<AnyElement> =
            w.history.iter().enumerate().rev().map(|(i, c)| self.history_row(&theme, &w.id, i, c, cx)).collect();
        body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(section_title(&theme, "Commands", Some(w.commands)))
                .child(
                    card(&theme)
                        .children(history)
                        .when(w.history.is_empty(), |d| d.child(kit::empty_state(&theme, "terminal", "Nothing has run here yet."))),
                ),
        )
        .into_any_element()
    }

    fn history_row(&self, theme: &Theme, ws: &str, i: usize, c: &WorkspaceCommand, cx: &mut Context<Self>) -> AnyElement {
        let key = (ws.to_string(), i);
        let open = self.ws_ui.expanded.contains(&key);
        let toggle = key.clone();
        div()
            .id(SharedString::from(format!("wsh-{ws}-{i}")))
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(theme.border_variant)
            .child(
                div()
                    .id(SharedString::from(format!("wsh-head-{ws}-{i}")))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(8.))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.ghost_hover))
                    .child(icon(if open { "chevron-down" } else { "chevron-right" }, 12., theme.text_placeholder))
                    .child(div().flex_1().min_w(px(0.)).truncate().font_family(mono_font()).text_size(px(12.5)).child(c.command.clone()))
                    .children(c.by.clone().map(|b| div().text_size(px(11.)).text_color(theme.text_placeholder).child(b)))
                    .child(div().w(px(64.)).flex_none().text_size(px(11.5)).text_color(theme.text_placeholder).child(if c.background {
                        "background".to_string()
                    } else {
                        crate::util::short_duration(c.duration_ms / 1000)
                    }))
                    .child(div().w(px(70.)).flex_none().text_size(px(11.5)).text_color(theme.text_placeholder).child(format::ago(Some(c.started_at))))
                    .child(exit_badge(theme, c))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.ws_ui.expanded.remove(&toggle) {
                            this.ws_ui.expanded.insert(toggle.clone());
                        }
                        cx.notify()
                    })),
            )
            .when(open, |d| {
                d.child(
                    div()
                        .mx(px(14.))
                        .mb(px(10.))
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(theme.sidebar)
                        .font_family(mono_font())
                        .text_size(px(11.5))
                        .line_height(px(17.))
                        .text_color(theme.text_muted)
                        .children(c.output_tail.iter().map(|l| div().child(l.clone())))
                        .when(c.output_tail.is_empty(), |d| d.child(div().text_color(theme.text_placeholder).child("(no output)"))),
                )
            })
            .into_any_element()
    }

    /// The machine page's list of workspaces, plus the switch that allows them.
    pub(super) fn machine_workspaces(&self, theme: &Theme, s: &crate::fleet::model::Snapshot, m: &crate::fleet::model::Machine, cx: &mut Context<Self>) -> AnyElement {
        let mine: Vec<&WorkspaceInfo> = s.workspaces.iter().filter(|w| w.machine_id == m.id).collect();
        let rows: Vec<AnyElement> = mine
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let id = w.id.clone();
                div()
                    .id(SharedString::from(format!("mws-{}", w.id)))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(9.))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.ghost_hover))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .child(kit::tile("terminal", kit::tiles::WORKSPACES.0, kit::tiles::WORKSPACES.1, 18.))
                    .child(div().text_size(px(13.)).child(w.name.clone()))
                    .child(div().font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(w.id.clone()))
                    .child(div().flex_1())
                    .children(w.owner.clone().map(|o| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(o)))
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!("used {}", format::ago(Some(w.last_used_at)))))
                    .child(status_pill(theme, w))
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Workspace(id.clone()), window, cx)))
                    .on_mouse_down(gpui::MouseButton::Right, {
                        let wid = w.id.clone();
                        super::menu::on_right_click(move || super::menu::MenuTarget::Workspace(wid.clone()), cx)
                    })
                    .into_any_element()
            })
            .collect();
        let machine_id = m.id.clone();
        let new_on = m.id.clone();
        let allowed = m.workspaces_allowed;
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section_title(theme, "Agent workspaces", Some(mine.len())))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(kit::checkbox(theme, "ws-allow", allowed, "Allow agents to create workspaces here", cx.listener(move |this, value: &bool, _, cx| {
                                if let Err(e) = this.engine.set_workspaces_allowed(&machine_id, *value) {
                                    this.notify_user(e, true, cx);
                                }
                            })))
                            .child(
                                Button::new("ws-new-here", "New workspace")
                                    .icon("plus")
                                    .disabled(!allowed || m.status == "offline")
                                    .on_click(cx.listener(move |this, _, window, cx| this.show_new_workspace(Some(new_on.clone()), window, cx))),
                            ),
                    ),
            )
            .child(if rows.is_empty() {
                card(theme)
                    .child(kit::empty_state(
                        theme,
                        "terminal",
                        if allowed { "No workspaces here. Agents create them with workspace_create." } else { "Agents can't create workspaces on this machine." },
                    ))
                    .into_any_element()
            } else {
                card(theme).children(rows).into_any_element()
            })
            .into_any_element()
    }

    fn load_workspace_changes(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.ws_ui.loading_changes.insert(id.to_string()) {
            return;
        }
        cx.notify();
        let engine = self.engine.clone();
        let wid = id.to_string();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            let wid = wid.clone();
            async move { engine.workspace_changes(&wid).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.ws_ui.loading_changes.remove(&wid);
                this.ws_ui.changes.insert(wid, result);
                cx.notify()
            });
        });
        self.track(task);
    }
}

fn workspace_subtitle(w: &WorkspaceInfo) -> String {
    let mut parts = vec![w.machine_name.clone()];
    if let Some(repo) = &w.repo {
        let at = w.branch.as_ref().or(w.git_ref.as_ref()).map(|r| format!(" @ {r}")).unwrap_or_default();
        parts.push(format!("{repo}{at}"));
    }
    parts.join("  ·  ")
}

fn status_pill(theme: &Theme, w: &WorkspaceInfo) -> AnyElement {
    match w.status.as_str() {
        "ready" => kit::pill("ready", theme.success).into_any_element(),
        "creating" => div()
            .flex()
            .items_center()
            .gap(px(5.))
            .child(kit::spinner(SharedString::from(format!("ws-spin-{}", w.id)), 11., theme.accent))
            .child(kit::pill("creating", theme.accent))
            .into_any_element(),
        "destroying" => kit::pill("destroying", theme.warning).into_any_element(),
        _ => kit::pill("failed", theme.error).into_any_element(),
    }
}

fn exit_badge(theme: &Theme, c: &WorkspaceCommand) -> AnyElement {
    let (text, color) = match (c.exit_code, c.timed_out, c.background) {
        (_, true, _) => ("timed out".to_string(), theme.warning),
        (_, _, true) => ("bg".to_string(), theme.info),
        (Some(0), _, _) => ("0".to_string(), theme.success),
        (Some(code), _, _) => (code.to_string(), theme.error),
        (None, _, _) => ("…".to_string(), theme.text_placeholder),
    };
    div()
        .flex_none()
        .min_w(px(28.))
        .px(px(6.))
        .h(px(18.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .bg(color.opacity(0.14))
        .text_color(color)
        .font_family(mono_font())
        .text_size(px(11.))
        .child(text)
        .into_any_element()
}

fn diff_line(theme: &Theme, line: &str, failed: bool) -> AnyElement {
    let color = if failed {
        theme.error
    } else if line.starts_with('+') && !line.starts_with("+++") {
        theme.success
    } else if line.starts_with('-') && !line.starts_with("---") {
        theme.error
    } else if line.starts_with("@@") || line.starts_with("##") {
        theme.text_accent
    } else {
        theme.text_muted
    };
    div().text_color(color).child(line.to_string()).into_any_element()
}

fn docker_pill(theme: &Theme, w: &WorkspaceInfo) -> AnyElement {
    kit::pill(format!("docker · {}", w.image.clone().unwrap_or_else(|| "container".into())), theme.info).into_any_element()
}
