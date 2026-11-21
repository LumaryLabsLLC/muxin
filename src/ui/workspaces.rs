//! Agent workspaces: folders on the user's machines that agents check out as their
//! work environment. Lists them, shows what agents ran, and lets you step in.

use std::collections::HashSet;
use crate::ui::i18n::{tr, trf, tr_msg};

use gpui::{AnyElement, ClipboardItem, Context, FontWeight, SharedString, div, prelude::*, px};

use super::format;
use super::kit::{self, Button, ButtonStyle, card, icon, kv, section_title};
use super::overview::{callout, page};
use super::root::{Root, Route};
use super::theme::{ActiveTheme, mono_font, Theme};
use crate::fleet::model::{WorkspaceCommand, WorkspaceInfo};

/// How many workspaces a machine's page and the Boxes page list before Show more.
const WORKSPACES_SHOWN: usize = 5;

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

        // Wrapped text needs its width known before layout (GPUI 0.2): what the button leaves.
        let intro_width = (self.content_width - 150.).clamp(240., 760.);
        page("workspaces-page", &self.page_scroll)
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(20.))
                    .child(
                        div().flex().flex_col().flex_none().w(px(intro_width)).gap(px(4.)).child(
                            div().text_size(px(12.5)).text_color(theme.text_muted).child(
                                tr("A place on your machines for an agent you're using elsewhere (Claude Code on your laptop, say) to work in: it runs commands, edits files and syncs code there through Muxin. Nothing works there on its own; to hand a task to an agent that does, start a box. Workspaces stay until deleted."),
                            ),
                        ).child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(trf("{allowed} of {total} machines accept workspaces. Agents create them with the workspace_create MCP tool or `muxin workspace create`.",
                            &[("allowed", &allowed), ("total", &s.machines.len())],
                        ))),
                    )
                    .child(
                        Button::new("ws-new", tr("New workspace"))
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
                    .child(section_title(&theme, tr("Workspaces"), Some(s.workspaces.len())))
                    .children(rows)
                    .when(s.workspaces.is_empty(), |d| {
                        d.child(card(&theme).child(kit::empty_state(&theme, "terminal", tr("No workspaces yet. Ask an agent to create one, or make one here."))))
                    }),
            )
    }

    fn workspace_card(&self, theme: &Theme, w: &WorkspaceInfo, cx: &mut Context<Self>) -> AnyElement {
        let id = w.id.clone();
        let shell = w.shell.clone();
        let last = w.history.last();
        let stats = [
            if w.commands == 1 { tr("1 command").to_string() } else { trf("{n} commands", &[("n", &w.commands)]) },
            trf("used {ago}", &[("ago", &format::ago(Some(w.last_used_at)))]),
            format!("created {}", format::ago(Some(w.created_at))),
        ];
        let room = self.content_width - 34. - stats.iter().map(|t| about(t, 11.5) + 14.).sum::<f32>();
        div()
            .id(SharedString::from(format!("wscard-{}", w.id)))
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .rounded(crate::ui::kit::corner(12.))
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
                            .child(div().text_size(px(12.)).text_color(theme.text_muted).child(workspace_subtitle(w, false))),
                    )
                    .when(w.isolation == "docker", |d| d.child(docker_pill(theme, w)))
                    .children(w.owner.clone().map(|o| kit::pill(o, theme.info)))
                    .child(status_pill(theme, w))
                    .child({
                        let ws = w.clone();
                        Button::icon_only(SharedString::from(format!("ws-session-{}", w.id)), "square-terminal")
                            .tooltip(tr("New session here"))
                            .disabled(w.status != "ready")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.new_workspace_session(&ws, window, cx)
                            }))
                    })
                    .child(
                        Button::icon_only(SharedString::from(format!("ws-copy-{}", w.id)), "copy")
                            .tooltip(tr("Copy shell command"))
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(shell.clone()))),
                    )
                    .child({
                        let (wid, name) = (w.id.clone(), w.name.clone());
                        Button::icon_only(SharedString::from(format!("ws-delete-{}", w.id)), "trash-2")
                            .style(ButtonStyle::Danger)
                            .tooltip(tr("Delete workspace"))
                            .on_click(cx.listener(move |this, _, window, cx| this.confirm_destroy_workspace(&wid, &name, window, cx)))
                    }),
            )
            // What its work changed: branch, lines, pull request.
            .children(w.git.as_ref().map(|g| super::sessions::git_line(theme, &format!("ws-{}", w.id), g, true)))
            .children(merged_pull(w).map(|n| {
                let (wid, name) = (w.id.clone(), w.name.clone());
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(crate::ui::kit::corner(8.))
                    .bg(theme.accent2.opacity(0.10))
                    .text_size(px(12.5))
                    .child(icon("git-merge", 14., theme.accent2))
                    .child(div().flex_1().min_w(px(0.)).child(trf("Pull request #{n} is merged, so this worktree's work is done. Its branch stays in the checkout.", &[("n", &n)])))
                    .child(Button::new(SharedString::from(format!("ws-clean-{}", w.id)), tr("Clean up")).on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.confirm_destroy_workspace(&wid, &name, window, cx)
                    })))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(crate::ui::kit::corner(8.))
                    .bg(theme.sidebar)
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .child(div().text_color(theme.text_placeholder).child("$"))
                    .child(div().flex_1().min_w(px(0.)).truncate().text_color(if last.is_some() { theme.text } else { theme.text_placeholder }).child(
                        last.map(|c| c.command.clone()).unwrap_or_else(|| tr("no commands yet").into()),
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
                    .children(stats.map(|t| div().flex_none().child(t)))
                    .child(folder_at_end(&w.path, room)),
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
                .child(tr("This workspace was destroyed."))
                .into_any_element();
        };
        let shell = w.shell.clone();
        let path = w.path.clone();
        let machine_id = w.machine_id.clone();

        // The buttons go beside the name when there's room, else under it:
        // they take ~530, and the page is ~650 wide at the smallest window.
        let wide = self.content_width >= 900.;
        let buttons = div()
            .flex()
            .gap(px(6.))
            .when(!wide, |d| d.flex_wrap().pt(px(4.)))
            .child({
                let ws = w.clone();
                Button::new("ws-new-session", tr("New session"))
                    .icon("square-terminal")
                    .style(ButtonStyle::Primary)
                    .disabled(w.status != "ready")
                    .on_click(cx.listener(move |this, _, window, cx| this.new_workspace_session(&ws, window, cx)))
            })
            .child({
                let (mid, path) = (w.machine_id.clone(), w.path.clone());
                Button::new("ws-editor", tr("Open in editor"))
                    .icon("code")
                    .disabled(w.status != "ready")
                    .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| this.pick_editor(&mid, &path, e.position(), cx)))
            })
            .child(
                Button::new("ws-copy-shell", tr("Copy shell command"))
                    .icon("terminal")
                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(shell.clone()))),
            )
            .child({
                let wid = w.id.clone();
                Button::new("ws-changes", tr("Changes"))
                    .icon("git-branch")
                    .loading(self.ws_ui.loading_changes.contains(&w.id))
                    .on_click(cx.listener(move |this, _, _, cx| this.load_workspace_changes(&wid, cx)))
            })
            .child({
                let wid = w.id.clone();
                let name = w.name.clone();
                Button::icon_only("ws-destroy", "trash-2")
                    .style(ButtonStyle::Danger)
                    .tooltip(tr("Destroy workspace"))
                    .disabled(w.status == "destroying")
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm_destroy_workspace(&wid, &name, window, cx)))
            });
        let title = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().min_w(px(0.)).text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(w.name.clone()))
                    .child(div().flex_none().font_family(mono_font()).text_size(px(12.)).text_color(theme.text_placeholder).child(w.id.clone()))
                    .when(w.isolation == "docker", |d| d.child(docker_pill(&theme, &w)))
                    .child(status_pill(&theme, &w)),
            )
            // "machine · owner/repo @ branch · worktree of /path" wraps in what's left.
            .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(workspace_subtitle(&w, true)));
        let header = div().flex().items_start().justify_between().gap(px(16.));
        let header = if wide { header.child(title).child(buttons) } else { header.child(title.child(buttons)) };

        let details = card(&theme)
            .px(px(14.))
            .py(px(8.))
            .child(kv(
                &theme,
                tr("Machine"),
                div()
                    .id("ws-machine-link")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_color(theme.text_accent)
                    .child(div().flex_none().child(w.machine_name.clone()))
                    .child(div().min_w(px(0.)).font_family(mono_font()).text_color(theme.text_placeholder).child(format!("{} · {}", w.host, w.platform)))
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(machine_id.clone()), window, cx))),
            ))
            .child(kv(
                &theme,
                tr("Folder"),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    // A long folder wraps; the copy button stays beside it.
                    .child(kit::mono(w.path.clone()).min_w(px(0.)))
                    .child(Button::icon_only("ws-copy-path", "copy").tooltip(tr("Copy path")).on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))
                    })),
            ))
            .child(kv(
                &theme,
                tr("Environment"),
                if w.isolation == "docker" {
                    kit::mono(format!(
                        "Docker · {}{}",
                        w.image.clone().unwrap_or_default(),
                        w.container.as_ref().map(|c| format!(" · {c}")).unwrap_or_default()
                    ))
                    .into_any_element()
                } else {
                    div().child(tr("Directly on the machine")).into_any_element()
                },
            ))
            .children(w.repo.clone().map(|r| kv(&theme, tr("Repository"), kit::mono(r))))
            .children(w.git_ref.clone().map(|r| kv(&theme, tr("Checked out"), kit::mono(r))))
            .children(w.branch.clone().map(|b| kv(&theme, tr("Branch"), kit::mono(b))))
            .children(w.owner.clone().map(|o| kv(&theme, tr("Created by"), o)))
            .child(kv(&theme, tr("Created"), trf("{created} · last used {used}", &[("created", &format::ago(Some(w.created_at))), ("used", &format::ago(Some(w.last_used_at)))])));

        let mut body = page("workspace-page", &self.page_scroll).child(header);
        if let Some(err) = &w.error {
            body = body.child(callout(&theme, "error", tr("Workspace error"), tr_msg(err), None));
        }
        body = body.child(details);
        // Sessions working in this folder.
        let here: Vec<_> = self.snapshot.sessions.iter().filter(|x| x.machine_id == w.machine_id && x.cwd.starts_with(&w.path)).cloned().collect();
        if !here.is_empty() {
            let rows: Vec<AnyElement> = here.iter().enumerate().map(|(i, x)| self.machine_session_row(&theme, x, i, false, cx)).collect();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(kit::section_title(&theme, tr("Sessions"), Some(here.len())))
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
                    .child(section_title(&theme, tr("Changes"), None))
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
                .child(section_title(&theme, tr("Commands"), Some(w.commands)))
                .child(
                    card(&theme)
                        .children(history)
                        .when(w.history.is_empty(), |d| d.child(kit::empty_state(&theme, "terminal", tr("Nothing has run here yet.")))),
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
                        .rounded(crate::ui::kit::corner(8.))
                        .bg(theme.sidebar)
                        .font_family(mono_font())
                        .text_size(px(11.5))
                        .line_height(px(17.))
                        .text_color(theme.text_muted)
                        .children(c.output_tail.iter().map(|l| div().child(l.clone())))
                        .when(c.output_tail.is_empty(), |d| d.child(div().text_color(theme.text_placeholder).child(tr("(no output)")))),
                )
            })
            .into_any_element()
    }

    /// The machine page's list of workspaces, plus the switch that allows them.
    /// Agents' workspaces on all machines, small, below the boxes: the other
    /// way agents work on your machines (an agent you use elsewhere, there).
    pub(super) fn boxes_page_workspaces(&self, theme: &Theme, s: &crate::fleet::model::Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let all: Vec<&WorkspaceInfo> = s.workspaces.iter().collect();
        let rows = self.workspace_rows(theme, "", &all, true, cx);
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(section_title(theme, tr("Agents' workspaces"), Some(s.workspaces.len())))
                            .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(tr("Folders on your machines that AI tools you use elsewhere work in through Muxin."))),
                    )
                    .child(
                        Button::new("ws-all", tr("All workspaces"))
                            .icon("arrow-right")
                            .style(ButtonStyle::Ghost)
                            .on_click(cx.listener(|this, _, window, cx| this.navigate(Route::Workspaces, window, cx))),
                    ),
            )
            .child(if rows.is_empty() {
                card(theme)
                    .child(div().px(px(14.)).py(px(10.)).text_size(px(12.5)).text_color(theme.text_placeholder).child(tr("None yet. An agent makes one with the workspace_create MCP tool.")))
                    .into_any_element()
            } else {
                card(theme).children(rows).into_any_element()
            })
            .into_any_element()
    }

    /// A list of workspaces: the first few, the rest behind Show more. `key`
    /// says which list is open in full (`Root::workspaces_more`).
    fn workspace_rows(&self, theme: &Theme, key: &str, list: &[&WorkspaceInfo], machine: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let more = self.workspaces_more.contains(key);
        let folded = list.len().saturating_sub(WORKSPACES_SHOWN);
        let mut rows: Vec<AnyElement> =
            list.iter().take(if more { list.len() } else { WORKSPACES_SHOWN }).enumerate().map(|(i, w)| self.workspace_row(theme, i, w, machine, cx)).collect();
        if folded > 0 {
            let key = key.to_string();
            rows.push(
                div()
                    .id(SharedString::from(format!("ws-more-{key}")))
                    .px(px(14.))
                    .py(px(9.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .border_t_1()
                    .border_color(theme.border_variant)
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(theme.text_placeholder)
                    .hover(|st| st.text_color(theme.text_muted))
                    .child(icon(if more { "chevron-down" } else { "chevron-right" }, 11., theme.text_placeholder))
                    .child(if more { tr("Show fewer").to_string() } else { trf("Show {folded} more", &[("folded", &folded)]) })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.workspaces_more.remove(&key) {
                            this.workspaces_more.insert(key.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        rows
    }

    /// One workspace in a list: on a machine's page, or with its machine on the Boxes page.
    fn workspace_row(&self, theme: &Theme, i: usize, w: &WorkspaceInfo, machine: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = w.id.clone();
        let place = machine.then(|| workspace_subtitle(w, false));
        let name = super::root::ellipsize(&w.name, 32);
        let used = trf("used {ago}", &[("ago", &format::ago(Some(w.last_used_at)))]);
        // About what the rest of the row takes: GPUI 0.2 only puts "…" on
        // text whose width it knows.
        let taken = 30. + 18. + about(&name, 13.).min(240.) + about_mono(&w.id, 11.) + w.owner.as_deref().map_or(0., |o| about(o, 11.5) + 10.) + about(&used, 11.5) + about(&w.status, 11.) + 12. + 10. * 5.;
        let place_width = (self.content_width - taken).max(40.);
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
            // A long name is cut, so what's after it still shows.
            .child(div().min_w(px(0.)).max_w(px(240.)).truncate().text_size(px(13.)).child(name))
            .child(div().flex_none().font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(w.id.clone()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .children(place.map(|p| div().w(px(place_width)).truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(p))),
            )
            .children(w.owner.clone().map(|o| div().flex_none().text_size(px(11.5)).text_color(theme.text_placeholder).child(o)))
            .child(div().flex_none().text_size(px(11.5)).text_color(theme.text_placeholder).child(used))
            .child(status_pill(theme, w))
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Workspace(id.clone()), window, cx)))
            .on_mouse_down(gpui::MouseButton::Right, {
                let wid = w.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Workspace(wid.clone()), cx)
            })
            .into_any_element()
    }

    pub(super) fn machine_workspaces(&self, theme: &Theme, s: &crate::fleet::model::Snapshot, m: &crate::fleet::model::Machine, cx: &mut Context<Self>) -> AnyElement {
        let mine: Vec<&WorkspaceInfo> = s.workspaces.iter().filter(|w| w.machine_id == m.id).collect();
        let rows = self.workspace_rows(theme, &m.id, &mine, false, cx);
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
                    .child(section_title(theme, tr("Agent workspaces"), Some(mine.len())))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(kit::checkbox(theme, "ws-allow", allowed, tr("Allow agents to create workspaces here"), cx.listener(move |this, value: &bool, _, cx| {
                                if let Err(e) = this.engine.set_workspaces_allowed(&machine_id, *value) {
                                    this.notify_user(e, true, cx);
                                }
                            })))
                            .child(
                                Button::new("ws-new-here", tr("New workspace"))
                                    .icon("plus")
                                    .disabled(!allowed || m.is_down())
                                    .on_click(cx.listener(move |this, _, window, cx| this.show_new_workspace(Some(new_on.clone()), window, cx))),
                            ),
                    ),
            )
            .child(if rows.is_empty() {
                card(theme)
                    .child(kit::empty_state(
                        theme,
                        "terminal",
                        if allowed { tr("No workspaces here. Agents create them with workspace_create.") } else { tr("Agents can't create workspaces on this machine.") },
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

/// "machine · owner/repo @ branch · worktree of ~/code/app". `full`: the
/// worktree's checkout as it is (the workspace's own page), else cut short.
fn workspace_subtitle(w: &WorkspaceInfo, full: bool) -> String {
    let mut parts = vec![w.machine_name.clone()];
    if let Some(repo) = &w.repo {
        let at = w.branch.as_ref().or(w.git_ref.as_ref()).map(|r| format!(" @ {r}")).unwrap_or_default();
        parts.push(format!("{repo}{at}"));
    }
    if let Some(src) = &w.worktree_of {
        parts.push(trf("worktree of {folder}", &[("folder", &if full { src.clone() } else { format::short_folder(src) })]));
    }
    parts.join("  ·  ")
}

/// About how wide `text` is at `size`, a little over (for a width to give
/// text that has to end in "…").
pub(super) fn about(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.52 + 4.
}

/// `about` for monospace text (folders, ids), whose characters are wider.
pub(super) fn about_mono(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.62 + 4.
}

/// A card's folder at the end of its last line: cut in the middle, then
/// with "…" when that still doesn't fit `room`. It sits at the line's end.
/// The width is explicit and in a column: GPUI 0.2 only ellipsizes text
/// whose width it knows, and not straight in a row.
pub(super) fn folder_at_end(path: &str, room: f32) -> gpui::Div {
    let shown = format::short_folder(path);
    let width = about_mono(&shown, 11.5).min(room).max(40.);
    div().flex().flex_col().items_end().flex_1().min_w(px(0.)).overflow_hidden().child(div().w(px(width)).truncate().text_right().font_family(mono_font()).child(shown))
}

/// A worktree whose pull request is merged: its work is done, so it can go.
fn merged_pull(w: &WorkspaceInfo) -> Option<u64> {
    w.worktree_of.as_ref()?;
    w.git.as_ref()?.pull.as_ref().filter(|p| p.state == "merged").map(|p| p.number)
}

fn status_pill(theme: &Theme, w: &WorkspaceInfo) -> AnyElement {
    match w.status.as_str() {
        "ready" => kit::pill(tr("ready"), theme.success).into_any_element(),
        "creating" => div()
            .flex()
            .items_center()
            .gap(px(5.))
            .child(kit::spinner(SharedString::from(format!("ws-spin-{}", w.id)), 11., theme.accent))
            .child(kit::pill(tr("creating"), theme.accent))
            .into_any_element(),
        "destroying" => kit::pill(tr("destroying"), theme.warning).into_any_element(),
        _ => kit::pill(tr("failed"), theme.error).into_any_element(),
    }
}

fn exit_badge(theme: &Theme, c: &WorkspaceCommand) -> AnyElement {
    let (text, color) = match (c.exit_code, c.timed_out, c.background) {
        (_, true, _) => (tr("timed out").to_string(), theme.warning),
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
        .rounded(crate::ui::kit::corner(4.))
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

/// "docker · universal:2": the image's own name, not where it's from
/// (mcr.microsoft.com/devcontainers/…), which the Environment row has.
fn docker_pill(theme: &Theme, w: &WorkspaceInfo) -> AnyElement {
    let image = w.image.as_deref().and_then(|i| i.rsplit('/').next()).filter(|i| !i.is_empty()).unwrap_or("container");
    kit::pill(format!("docker · {image}"), theme.info).into_any_element()
}
