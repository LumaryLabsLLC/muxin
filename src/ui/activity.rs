//! The Activity page: one timeline of everything happening on the fleet —
//! operations started here (installs, restarts, logs, workspace setup) and the
//! commands agents ran in workspaces — with the selected item's output.

use chrono::{DateTime, Utc};
use gpui::{AnyElement, ClipboardItem, Context, FontWeight, SharedString, div, prelude::*, px};

use super::detail::op_status_icon;
use super::format;
use super::kit::{self, Button, card, icon};
use super::root::{Root, Route};
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::model::{Operation, Snapshot, WorkspaceCommand, WorkspaceInfo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityItem {
    Op(u64),
    /// A command in a workspace's history.
    Command { workspace: String, index: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ActivityFilter {
    #[default]
    All,
    Operations,
    Agents,
    Failed,
}

/// Feed entries, newest first.
enum Entry<'a> {
    Op(&'a Operation),
    Command(&'a WorkspaceInfo, usize, &'a WorkspaceCommand),
}

impl Entry<'_> {
    fn at(&self) -> DateTime<Utc> {
        match self {
            Entry::Op(o) => o.started_at,
            Entry::Command(_, _, c) => c.started_at,
        }
    }

    fn item(&self) -> ActivityItem {
        match self {
            Entry::Op(o) => ActivityItem::Op(o.id),
            Entry::Command(w, i, _) => ActivityItem::Command { workspace: w.id.clone(), index: *i },
        }
    }

    fn failed(&self) -> bool {
        match self {
            Entry::Op(o) => o.status == "failed",
            Entry::Command(_, _, c) => c.timed_out || c.exit_code.is_some_and(|e| e != 0),
        }
    }
}

const MAX_ENTRIES: usize = 300;

fn feed(s: &Snapshot, filter: ActivityFilter) -> Vec<Entry<'_>> {
    let mut entries: Vec<Entry> = Vec::new();
    if filter != ActivityFilter::Agents {
        entries.extend(s.operations.iter().map(Entry::Op));
    }
    if filter != ActivityFilter::Operations {
        for w in &s.workspaces {
            entries.extend(w.history.iter().enumerate().map(|(i, c)| Entry::Command(w, i, c)));
        }
    }
    if filter == ActivityFilter::Failed {
        entries.retain(|e| e.failed());
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.at()));
    entries.truncate(MAX_ENTRIES);
    entries
}

impl Root {
    /// The selected item, falling back to the newest one in the feed.
    pub(super) fn selected_activity(&self, s: &Snapshot) -> Option<ActivityItem> {
        let entries = feed(s, self.activity_filter);
        self.activity_selected
            .clone()
            .filter(|sel| entries.iter().any(|e| &e.item() == sel))
            .or_else(|| entries.first().map(|e| e.item()))
    }

    pub(super) fn render_activity(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        // Nothing has ever happened: show how to make something happen instead of empty panes.
        if s.operations.is_empty() && s.workspaces.iter().all(|w| w.history.is_empty()) {
            return self.activity_welcome(&theme, &s, cx);
        }
        let entries = feed(&s, self.activity_filter);
        let selected = self.selected_activity(&s);
        let running = s.operations.iter().filter(|o| o.status == "running").count();

        let filters = [
            (ActivityFilter::All, "All"),
            (ActivityFilter::Operations, "Operations"),
            (ActivityFilter::Agents, "Agent commands"),
            (ActivityFilter::Failed, "Failed"),
        ];
        let filter_bar = div()
            .flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(9.))
            .bg(theme.element)
            .border_1()
            .border_color(theme.border_variant)
            .children(filters.into_iter().map(|(f, label)| {
                let active = self.activity_filter == f;
                div()
                    .id(label)
                    .h(px(26.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .text_color(if active { theme.text } else { theme.text_muted })
                    .when(active, |d| d.bg(theme.surface).border_1().border_color(theme.border).shadow_sm())
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.activity_filter = f;
                        cx.notify()
                    }))
            }));

        let list: Vec<AnyElement> = entries.iter().map(|e| self.feed_row(&theme, e, selected.as_ref() == Some(&e.item()), cx)).collect();
        let empty = entries.is_empty();
        let detail = match &selected {
            Some(ActivityItem::Op(id)) => s.operations.iter().find(|o| o.id == *id).map(|o| self.op_detail(&theme, o, cx)),
            Some(ActivityItem::Command { workspace, index }) => s
                .workspaces
                .iter()
                .find(|w| &w.id == workspace)
                .and_then(|w| w.history.get(*index).map(|c| command_detail(&theme, w, c, cx))),
            None => None,
        };
        let has_detail = detail.is_some();

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(12.))
                    .px(px(28.))
                    .pt(px(18.))
                    .pb(px(14.))
                    .child(filter_bar)
                    .child(div().flex_1())
                    .when(running > 0, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(12.))
                                .text_color(theme.text_muted)
                                .child(kit::spinner("activity-running", 12., theme.accent))
                                .child(format!("{running} running")),
                        )
                    })
                    .child(
                        Button::new("activity-clear", "Clear finished")
                            .icon("trash-2")
                            .disabled(s.operations.iter().all(|o| o.status == "running"))
                            .on_click(cx.listener(|this, _, _, _| this.engine.clear_operations())),
                    ),
            )
            .child(if empty {
                filtered_empty(&theme, self.activity_filter)
            } else {
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .gap(px(16.))
                    .px(px(28.))
                    .pb(px(24.))
                    .child(
                        card(&theme).w(px((self.content_width * 0.36).clamp(260., 380.))).flex_none().overflow_hidden().child(
                            div().id("activity-feed").size_full().overflow_y_scroll().p(px(6.)).children(list),
                        ),
                    )
                    .child(div().flex().flex_col().flex_1().min_w(px(0.)).children(detail).when(!has_detail, |d| {
                        d.child(
                            card(&theme)
                                .flex_1()
                                .items_center()
                                .justify_center()
                                .text_size(px(12.5))
                                .text_color(theme.text_placeholder)
                                .child("Select an entry to see its output."),
                        )
                    }))
                    .into_any_element()
            })
            .into_any_element()
    }

    /// First run: what shows up here, and three ways to make something happen.
    fn activity_welcome(&self, theme: &Theme, s: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let width = self.content_width.min(480.);
        let target = s.machines.iter().find(|m| m.status == "online").or(s.machines.first()).map(|m| (m.id.clone(), m.name.clone()));
        let install_detail = match &target {
            Some((_, name)) => format!("Register a GitHub Actions runner on {name}."),
            None => "Add a machine first, then put a runner on it.".into(),
        };
        let mcp = mcp_add_command();

        div()
            .id("activity-welcome")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .px(px(28.))
            .pt(px(72.))
            .pb(px(40.))
            .child(kit::tile("activity", kit::tiles::ACTIVITY.0, kit::tiles::ACTIVITY.1, 56.))
            .child(div().mt(px(18.)).text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("No activity yet"))
            .child(
                div().mt(px(8.)).w(px(width)).text_center().text_size(px(13.)).text_color(theme.text_muted).child(
                    "Everything Muxen does on your machines lands here with its full output: runner installs and restarts, log requests, workspace setup, and the commands agents run.",
                ),
            )
            .child(
                card(theme)
                    .mt(px(28.))
                    .w(px(width))
                    .overflow_hidden()
                    .child(start_row(theme, "start-install", "box", kit::tiles::OVERVIEW, "Install a runner", install_detail, "chevron-right", width).on_click(
                        cx.listener(move |this, _, window, cx| match &target {
                            Some((id, _)) => this.show_install_dialog(id, window, cx),
                            None => this.show_add_machine(window, cx),
                        }),
                    ))
                    .child(
                        start_row(
                            theme,
                            "start-workspace",
                            "terminal",
                            kit::tiles::WORKSPACES,
                            "Start an agent workspace",
                            "A persistent work environment for agents, on the host or in Docker.".into(),
                            "chevron-right",
                            width,
                        )
                        .border_t_1()
                        .border_color(theme.border_variant)
                        .on_click(cx.listener(|this, _, window, cx| this.show_new_workspace(None, window, cx))),
                    )
                    .child(
                        start_row(
                            theme,
                            "start-agent",
                            "zap",
                            kit::tiles::JOBS,
                            "Connect an agent",
                            "Copy the command that adds Muxen to Claude Code over MCP.".into(),
                            "copy",
                            width,
                        )
                        .border_t_1()
                        .border_color(theme.border_variant)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(mcp.clone()));
                            this.notify_user("Copied the Claude Code command", false, cx);
                        })),
                    ),
            )
            .child(
                div()
                    .mt(px(18.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .text_color(theme.text_placeholder)
                    .child("Open Activity from anywhere with")
                    .child(kit::kbd(theme, "⌘J")),
            )
            .into_any_element()
    }

    fn feed_row(&self, theme: &Theme, e: &Entry, selected: bool, cx: &mut Context<Self>) -> AnyElement {
        let item = e.item();
        let (status, title, subtitle) = match e {
            Entry::Op(o) => {
                let when = match (&o.progress, o.status.as_str()) {
                    (Some(p), "running") => progress_text(p),
                    _ => format::ago(Some(o.started_at)),
                };
                (op_status_icon(theme, o), o.title.clone(), format!("{} · {when}", kind_label(&o.kind)))
            }
            Entry::Command(w, _, c) => (
                command_icon(theme, c),
                c.command.clone(),
                format!(
                    "{} in {} · {}",
                    c.by.clone().or_else(|| w.owner.clone()).unwrap_or_else(|| "someone".into()),
                    w.name,
                    format::ago(Some(c.started_at))
                ),
            ),
        };
        let is_command = matches!(e, Entry::Command(..));
        div()
            .id(SharedString::from(format!("feed-{item:?}")))
            .flex()
            .items_start()
            .gap(px(10.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(div().pt(px(2.)).child(status))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .child(div().truncate().text_size(px(12.5)).when(is_command, |d| d.font_family(mono_font())).child(title))
                    .child(div().truncate().text_size(px(11.)).text_color(theme.text_placeholder).child(subtitle))
                    .children(match e {
                        Entry::Op(o) if o.status == "running" => o.progress.as_ref().map(|p| div().pt(px(3.)).child(progress_bar(theme, p))),
                        _ => None,
                    }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activity_selected = Some(item.clone());
                this.activity_seen_len = usize::MAX;
                cx.notify()
            }))
            .into_any_element()
    }

    fn op_detail(&self, theme: &Theme, op: &Operation, cx: &mut Context<Self>) -> AnyElement {
        let copy_text = op.log.join("\n");
        let machine = op
            .machine_id
            .clone()
            .and_then(|id| self.snapshot.machines.iter().find(|m| m.id == id).map(|m| (id, m.name.clone())));
        let runner = op.runner_id.clone();
        let when = match op.finished_at {
            Some(t) => format!(
                "{} {} · took {}",
                if op.status == "succeeded" { "Finished" } else { "Failed" },
                format::ago(Some(t)),
                crate::util::short_duration((t - op.started_at).num_seconds().max(0) as u64)
            ),
            None => format!("Running for {}", format::elapsed(op.started_at)),
        };
        // A copy under way: how far, and a way to stop it.
        let progress = op.progress.clone().filter(|_| op.status == "running").map(|p| {
            let id = op.id;
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .pt(px(10.))
                .child(div().flex_1().child(progress_bar(theme, &p)))
                .child(div().flex_none().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_muted).child(progress_text(&p)))
                .when(op.cancellable, |d| {
                    d.child(Button::new("op-stop", "Stop").icon("x").on_click(cx.listener(move |this, _, _, cx| {
                        this.engine.cancel_op(id);
                        cx.notify()
                    })))
                })
        });
        let lines: Vec<AnyElement> = op.log.iter().map(|l| log_line(theme, l)).collect();
        let meta = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(theme.text_placeholder)
            .child(when)
            .children(machine.map(|(id, name)| {
                div()
                    .id("op-machine")
                    .cursor_pointer()
                    .text_color(theme.text_accent)
                    .child(name)
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(id.clone()), window, cx)))
            }))
            .children(runner.map(|id| {
                div()
                    .id("op-runner")
                    .cursor_pointer()
                    .text_color(theme.text_accent)
                    .child(id.rsplit('/').next().unwrap_or_default().to_string())
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Runner(id.clone()), window, cx)))
            }))
            .into_any_element();
        let log = div()
            .id(SharedString::from(format!("op-log-{}", op.id)))
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.activity_scroll)
            .p(px(14.))
            .font_family(mono_font())
            .text_size(px(12.))
            .line_height(px(19.))
            .children(lines)
            .when_some(op.error.clone().filter(|e| !op.log.iter().any(|l| l.contains(e.as_str()))), |d, e| {
                d.child(div().mt(px(6.)).flex().gap(px(6.)).text_color(theme.error).child(icon("circle-x", 13., theme.error)).child(e))
            })
            .when(op.log.is_empty() && op.status == "running", |d| d.child(div().text_color(theme.text_placeholder).child("Waiting for output…")));
        let body = div().flex_1().min_h(px(0.)).flex().flex_col().children(progress).child(log).into_any_element();
        detail_frame(theme, op_status_icon(theme, op), op.title.clone(), meta, copy_text, body)
    }
}

fn command_detail(theme: &Theme, w: &WorkspaceInfo, c: &WorkspaceCommand, cx: &mut Context<Root>) -> AnyElement {
    let ws = w.id.clone();
    let outcome = match (c.background, c.timed_out, c.exit_code) {
        (true, _, _) => "started in the background".to_string(),
        (_, true, _) => "timed out".to_string(),
        (_, _, Some(code)) => format!("exit {code} in {}", crate::util::short_duration(c.duration_ms / 1000)),
        _ => "running".to_string(),
    };
    let meta = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .text_color(theme.text_placeholder)
        .child(format!(
            "{} · {} · {outcome}",
            c.by.clone().or_else(|| w.owner.clone()).unwrap_or_else(|| "someone".into()),
            format::ago(Some(c.started_at)),
        ))
        .child(
            div()
                .id("cmd-workspace")
                .cursor_pointer()
                .text_color(theme.text_accent)
                .child(format!("{} on {}", w.name, w.machine_name))
                .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Workspace(ws.clone()), window, cx))),
        )
        .into_any_element();
    let mut lines: Vec<AnyElement> = vec![div().text_color(theme.text_accent).child(format!("$ {}", c.command)).into_any_element()];
    lines.extend(c.output_tail.iter().map(|l| div().text_color(theme.text_muted).child(l.clone()).into_any_element()));
    if c.output_tail.is_empty() {
        lines.push(div().text_color(theme.text_placeholder).child("(no output)").into_any_element());
    }
    let body = div()
        .id("cmd-output")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .p(px(14.))
        .font_family(mono_font())
        .text_size(px(12.))
        .line_height(px(19.))
        .children(lines)
        .into_any_element();
    detail_frame(theme, command_icon(theme, c), c.command.clone(), meta, c.output_tail.join("\n"), body)
}

/// Header (status, title, meta, copy) above a scrolling output pane.
fn detail_frame(theme: &Theme, status: AnyElement, title: String, meta: AnyElement, copy_text: String, body: AnyElement) -> AnyElement {
    card(theme)
        .flex_1()
        .min_h(px(0.))
        .overflow_hidden()
        .child(
            div()
                .flex()
                .flex_none()
                .items_start()
                .gap(px(10.))
                .px(px(16.))
                .py(px(12.))
                .border_b_1()
                .border_color(theme.border_variant)
                .child(div().pt(px(2.)).child(status))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(3.))
                        .child(div().truncate().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child(title))
                        .child(meta),
                )
                .child(Button::icon_only("detail-copy", "copy").tooltip("Copy output").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()))
                })),
        )
        .child(div().flex().flex_col().flex_1().min_h(px(0.)).bg(theme.sidebar).child(body))
        .into_any_element()
}

fn command_icon(theme: &Theme, c: &WorkspaceCommand) -> AnyElement {
    let color = match (c.background, c.timed_out, c.exit_code) {
        (true, _, _) => theme.info,
        (_, true, _) => theme.warning,
        (_, _, Some(0)) => theme.success,
        (_, _, Some(_)) => theme.error,
        _ => theme.text_placeholder,
    };
    icon("terminal", 13., color).into_any_element()
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "install" => "install",
        "start" => "start",
        "stop" => "stop",
        "restart" => "restart",
        "logs" => "logs",
        "remove" => "remove",
        "workspace" => "workspace",
        "machine" => "machine",
        "copy" => "copy",
        _ => "operation",
    }
}

/// Colors Muxen's own lines (see the conventions in crate::ops).
fn log_line(theme: &Theme, line: &str) -> AnyElement {
    let color = if line.starts_with("▸ ") {
        theme.text_accent
    } else if line.starts_with("✓ ") {
        theme.success
    } else if line.starts_with("error: ") || line.starts_with("Error") {
        theme.error
    } else if line.starts_with("warning: ") {
        theme.warning
    } else {
        theme.text_muted
    };
    div().text_color(color).child(line.to_string()).into_any_element()
}

/// A row on the first-run card: tile, title, one line of detail, trailing icon.
#[allow(clippy::too_many_arguments)]
fn start_row(
    theme: &Theme,
    id: &'static str,
    icon_name: &'static str,
    colors: (u32, u32),
    title: &'static str,
    detail: String,
    trailing: &'static str,
    width: f32,
) -> gpui::Stateful<gpui::Div> {
    // Card width minus padding, tile, gaps and the trailing icon; the detail wraps within it.
    let text_w = (width - 28. - 32. - 24. - 16.).max(80.);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(14.))
        .py(px(12.))
        .cursor_pointer()
        .hover(|st| st.bg(theme.ghost_hover))
        .child(kit::tile(icon_name, colors.0, colors.1, 32.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .w(px(text_w))
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.text).child(title))
                .child(div().w(px(text_w)).text_size(px(12.)).text_color(theme.text_muted).child(detail)),
        )
        .child(div().flex_1())
        .child(icon(trailing, 14., theme.text_placeholder))
}

/// The filter matched nothing (but there is other activity).
fn filtered_empty(theme: &Theme, filter: ActivityFilter) -> AnyElement {
    let (icon_name, color, title, body) = match filter {
        ActivityFilter::Failed => ("circle-check", theme.success, "Nothing failed", "Failed operations and agent commands that exit non-zero show up here."),
        ActivityFilter::Operations => {
            ("activity", theme.text_muted, "No operations yet", "Runner installs, restarts, log requests and workspace setup show up here.")
        }
        ActivityFilter::Agents => ("terminal", theme.text_muted, "No agent commands yet", "Commands agents run in workspaces show up here with their output."),
        ActivityFilter::All => ("activity", theme.text_muted, "Nothing here", "Activity shows up here as it happens."),
    };
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .px(px(28.))
        .pb(px(80.))
        .child(div().size(px(44.)).mb(px(4.)).rounded_full().bg(theme.element).flex().items_center().justify_center().child(icon(icon_name, 20., color)))
        .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title))
        .child(div().w(px(340.)).text_center().text_size(px(12.5)).text_color(theme.text_muted).child(body))
        .into_any_element()
}

/// `claude mcp add` for this binary, quoted when its path has spaces.
fn mcp_add_command() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "muxen".into());
    let exe = if exe.contains(' ') { format!("'{exe}'") } else { exe };
    format!("claude mcp add muxen -- {exe} mcp")
}

/// "45 MB of 120 MB · 6.1 MB/s"
fn progress_text(p: &crate::fleet::model::OpProgress) -> String {
    let done = format::bytes(p.done);
    let amount = match p.total {
        Some(t) if t > 0 => format!("{done} of {}", format::bytes(t)),
        _ => done,
    };
    match p.rate {
        Some(r) if r > 0. => format!("{amount} · {}/s", format::bytes(r as u64)),
        _ => amount,
    }
}

fn progress_bar(theme: &Theme, p: &crate::fleet::model::OpProgress) -> gpui::Div {
    let fraction = match p.total {
        Some(t) if t > 0 => p.done as f32 / t as f32,
        _ => 0.,
    };
    kit::meter(theme, fraction, theme.accent)
}
