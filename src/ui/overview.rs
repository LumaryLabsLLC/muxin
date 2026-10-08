//! The overview (fleet pulse, stats, attention, machine cards) and the jobs page.

use gpui::{AnyElement, Context, Div, FontWeight, Hsla, ScrollHandle, SharedString, Stateful, Window, div, prelude::*, px, relative};

use super::format;
use super::kit::{self, Button, ButtonStyle, card, chip, icon, meter, section_title};
use super::theme::{ActiveTheme, mono_font, Theme};
use super::root::{Route, Root};
use crate::fleet::model::{Job, Machine, Snapshot};

const CARD_MIN_WIDTH: f32 = 290.;
const GAP: f32 = 14.;
const PAGE_PADDING: f32 = 28.;

/// A scrollable page body with consistent padding.
pub(super) fn page(id: &'static str, scroll: &ScrollHandle) -> Stateful<Div> {
    div()
        .id(id)
        .size_full()
        .overflow_y_scroll()
        .track_scroll(scroll)
        .px(px(PAGE_PADDING))
        .pt(px(22.))
        .pb(px(40.))
        .flex()
        .flex_col()
        .gap(px(26.))
}

impl Root {
    pub(super) fn render_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let content_width = f32::from(window.viewport_size().width) - self.sidebar_width - PAGE_PADDING * 2.;

        let mut body = page("overview-page", &self.page_scroll).child(self.pulse_card(&theme, &s)).child(stat_tiles(&theme, &s));

        if let Some(err) = &s.inventory.error {
            body = body.child(callout(&theme, "error", "fleet.toml has an error", err.clone(), None));
        }
        if !s.attention.is_empty() {
            body = body.child(self.render_attention(&theme, &s, cx));
        }
        // Side by side when there's room, stacked otherwise.
        let wide = content_width >= 1000.;
        body.child(self.render_machine_grid(&theme, &s, content_width, cx)).child(
            div()
                .flex()
                .when(!wide, |d| d.flex_col())
                .gap(px(if wide { GAP } else { 26. }))
                .child(div().flex_1().min_w(px(0.)).child(self.render_running(&theme, &s, cx)))
                .child(div().flex_1().min_w(px(0.)).child(self.render_queued(&theme, &s, "ov-q"))),
        )
    }

    /// Fleet utilization (busy / online runners) over the last few minutes.
    fn pulse_card(&self, theme: &Theme, s: &Snapshot) -> impl IntoElement {
        let c = &s.counts;
        let online = c.working + c.idle;
        let pct = if online == 0 { 0.0 } else { c.working as f32 / online as f32 };
        let samples: Vec<_> = self.pulse.iter().copied().collect();
        let slots = 64usize;
        let pad = slots.saturating_sub(samples.len());
        let bars: Vec<AnyElement> = (0..slots)
            .map(|i| {
                let sample = (i >= pad).then(|| samples[i - pad]);
                let frac = sample.map(|p| if p.online == 0 { 0.0 } else { p.working as f32 / p.online as f32 }).unwrap_or(0.0);
                let last = i + 1 == slots;
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .justify_end()
                    .when_some(sample, |d, _| {
                        d.child(
                            div()
                                .w_full()
                                .rounded(px(2.))
                                .h(relative(frac.max(0.04)))
                                .bg(if last { theme.accent } else { theme.accent.opacity(0.45) }),
                        )
                    })
                    .when(sample.is_none(), |d| d.child(div().w_full().h(px(2.)).rounded(px(1.)).bg(theme.track)))
                    .into_any_element()
            })
            .collect();
        // When Muxen last looked at the machines, in plain words.
        let checked = match (s.check.running, s.check.finished_at, s.check.next_at) {
            (true, _, _) => "Checking your machines now…".to_string(),
            (false, Some(_), Some(next)) => format!("Machines checked {} · next {}", format::ago(s.check.finished_at), format::until(next)),
            (false, Some(_), None) => format!("Machines checked {}", format::ago(s.check.finished_at)),
            (false, None, _) => "Machines not checked yet".into(),
        };
        let busy = if c.runners == 0 { "No runners yet".to_string() } else { format!("{} of {} online runners busy", c.working, online) };
        card(theme)
            .flex_row()
            .gap(px(24.))
            .p(px(20.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(250.))
                    .flex_none()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(theme.text_muted)
                            .child(icon("gauge", 13., theme.text_accent))
                            .child("Fleet pulse"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap(px(4.))
                            .child(
                                div()
                                    .font_family(mono_font())
                                    .text_size(px(40.))
                                    .line_height(px(44.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("{:.0}", pct * 100.)),
                            )
                            .child(div().pb(px(6.)).font_family(mono_font()).text_size(px(18.)).text_color(theme.text_muted).child("%")),
                    )
                    .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(busy))
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(checked)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(8.))
                    .child(div().flex().items_end().gap(px(3.)).h(px(84.)).children(bars))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(theme.text_placeholder)
                            .child("utilization, last few minutes")
                            .child("now"),
                    ),
            )
    }

    fn render_attention(&self, theme: &Theme, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<AnyElement> = s
            .attention
            .iter()
            .map(|a| {
                let button = a.action.clone().map(|action| {
                    Button::new(SharedString::from(format!("att-{}", a.id)), action.label.clone())
                        .on_click(cx.listener(move |this, _, window, cx| this.run_attention_action(&action, window, cx)))
                        .into_any_element()
                });
                callout(theme, &a.level, a.title.clone(), a.detail.clone(), button).into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Needs attention", Some(s.attention.len())))
            .child(div().flex().flex_col().gap(px(8.)).children(items))
    }

    fn render_machine_grid(&self, theme: &Theme, s: &Snapshot, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let mut machines: Vec<&Machine> = s.machines.iter().filter(|m| m.kind == "runner").collect();
        machines.extend(s.machines.iter().filter(|m| m.kind == "other"));
        let cols = (((width + GAP) / (CARD_MIN_WIDTH + GAP)).floor() as usize).clamp(1, 4);
        let card_width = ((width - GAP * (cols as f32 - 1.)) / cols as f32).floor();
        // Explicit rows rather than flex-wrap, so the section's height is always right.
        let cards: Vec<AnyElement> = machines.iter().map(|m| self.machine_card(theme, s, m, card_width, cx)).collect();
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut iter = cards.into_iter().peekable();
        while iter.peek().is_some() {
            let row: Vec<AnyElement> = iter.by_ref().take(cols).collect();
            rows.push(div().flex().gap(px(GAP)).children(row).into_any_element());
        }

        let legend = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .text_size(px(11.5))
            .text_color(theme.text_muted)
            .child(div().flex().items_center().gap(px(5.)).child(kit::runner_glyph(theme, "working", 8.)).child("Working"))
            .child(div().flex().items_center().gap(px(5.)).child(kit::runner_glyph(theme, "idle", 8.)).child("Idle"))
            .child(div().flex().items_center().gap(px(5.)).child(kit::runner_glyph(theme, "offline", 8.)).child("Offline"));

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section_title(theme, "Machines", Some(s.machines.len())))
                    .child(
                        div().flex().items_center().gap(px(14.)).child(legend).child(
                            Button::new("ov-add-machine", "Add machine")
                                .icon("plus")
                                .on_click(cx.listener(|this, _, window, cx| this.show_add_machine(window, cx))),
                        ),
                    ),
            )
            .child(div().flex().flex_col().gap(px(GAP)).children(rows))
            .when(machines.is_empty(), |d| {
                d.child(card(theme).w_full().child(kit::empty_state(theme, "server", "No machines yet. Add the first one to start watching its runners.")))
            })
    }

    fn machine_card(&self, theme: &Theme, s: &Snapshot, m: &Machine, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let id = m.id.clone();
        let p = m.probe.as_ref().filter(|p| p.ok);
        let (diag, tone) = format::diagnostics_summary(m);
        let diag_color = match tone {
            "error" => theme.error,
            "warning" => theme.warning,
            _ => theme.text_placeholder,
        };

        let body: AnyElement = match p {
            Some(p) => {
                let cpu = p.cpu_pct.map(|v| v as f32 / 100.);
                let mem = format::mem_fraction(m);
                let disk = format::disk_used_fraction(m);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(meter_row(theme, "CPU", cpu.map(|f| format!("{:.0}%", f * 100.)), cpu, theme.accent))
                    .child(meter_row(theme, "Memory", mem.map(|f| format!("{:.0}%", f * 100.)), mem, theme.accent))
                    .child(meter_row(
                        theme,
                        "Disk",
                        p.disk_free.map(|f| format!("{} free", format::bytes(f))),
                        disk,
                        if format::disk_low(m) { theme.error } else { theme.accent },
                    ))
                    .into_any_element()
            }
            None => {
                let text = match m.status.as_str() {
                    "offline" => m.probe.as_ref().and_then(|p| p.error.clone()).unwrap_or_else(|| "Unreachable".into()),
                    "manual" => "No host configured, so it isn't checked.".into(),
                    _ if m.checking => "Checking…".into(),
                    _ => "Not checked yet.".into(),
                };
                div()
                    .h(px(70.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(if m.status == "offline" { theme.error } else { theme.text_placeholder })
                    .child(div().line_clamp(3).child(text))
                    .into_any_element()
            }
        };

        div()
            .id(SharedString::from(format!("mcard-{}", m.id)))
            .w(px(width))
            .flex()
            .flex_col()
            .gap(px(14.))
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
                    .child({
                        let (from, to) = format::machine_tile(m);
                        kit::tile(format::machine_icon(m), from, to, 32.)
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().truncate().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child(m.name.clone()))
                            .child(div().truncate().font_family(mono_font()).text_size(px(11.)).text_color(theme.text_placeholder).child(
                                if m.platform.is_empty() { m.host.clone() } else { m.platform.clone() },
                            )),
                    )
                    .child(if m.checking {
                        kit::spinner(SharedString::from(format!("mcard-spin-{}", m.id)), 12., theme.text_muted).into_any_element()
                    } else {
                        kit::pill(status_word(m), theme.machine_status(&m.status)).into_any_element()
                    }),
            )
            .when(m.kind == "runner", |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(div().flex().flex_wrap().gap(px(3.)).children(
                            m.runner_ids.iter().filter_map(|rid| s.runners.iter().find(|r| &r.id == rid)).map(|r| kit::runner_glyph(theme, &r.state, 11.)),
                        ))
                        .child(div().flex_1())
                        .child(div().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_muted).child(format::runner_summary(m))),
                )
            })
            .child(body)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pt(px(10.))
                    .border_t_1()
                    .border_color(theme.border_variant)
                    .text_size(px(11.5))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .text_color(theme.text_placeholder)
                            .child(icon("clock", 12., theme.text_placeholder))
                            .child(p.and_then(|p| p.uptime_sec).map(|u| format!("up {}", format::uptime(u))).unwrap_or_else(|| "—".into())),
                    )
                    .child(div().text_color(diag_color).child(diag)),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(id.clone()), window, cx)))
            .into_any_element()
    }

    fn render_running(&self, theme: &Theme, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let working: Vec<_> = s.runners.iter().filter(|r| r.state == "working").collect();
        let rows: Vec<AnyElement> = working
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let job = r.job_id.and_then(|id| s.jobs.running.iter().find(|j| j.id == id));
                let id = r.id.clone();
                div()
                    .id(SharedString::from(format!("run-{}", r.id)))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(9.))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.ghost_hover))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .child(div().flex_none().w(px(3.)).h(px(26.)).rounded_full().bg(theme.accent))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().truncate().text_size(px(13.)).child(job.map(|j| j.name.clone()).unwrap_or_else(|| "Running a job".into())))
                            .child(div().truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
                                "{} · {}",
                                r.name,
                                job.map(|j| j.repo.clone()).unwrap_or_else(|| "job details need GitHub".into())
                            ))),
                    )
                    .children(
                        job.and_then(|j| j.started_at)
                            .map(|t| div().flex_none().font_family(mono_font()).text_size(px(12.)).text_color(theme.text_muted).child(format::elapsed(t))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Runner(id.clone()), window, cx)))
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Working now", Some(working.len())))
            .child(card(theme).when(rows.is_empty(), |d| d.child(kit::empty_state(theme, "circle", "No runner is working right now."))).children(rows))
    }

    pub(super) fn render_queued(&self, theme: &Theme, s: &Snapshot, prefix: &str) -> impl IntoElement {
        let rows: Vec<AnyElement> = s.jobs.queued.iter().enumerate().map(|(i, j)| queued_row(theme, j, i, prefix)).collect();
        let empty = if !s.github.configured && !s.demo {
            kit::empty_state(theme, "inbox", "Waiting for job data from GitHub.")
        } else {
            kit::empty_state(theme, "inbox", "No queued self-hosted jobs.")
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Queued", Some(s.jobs.queued.len())))
            .child(card(theme).when(rows.is_empty(), |d| d.child(empty)).children(rows))
    }

    pub(super) fn render_jobs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let running: Vec<AnyElement> = s.jobs.running.iter().enumerate().map(|(i, j)| self.running_job_row(&theme, j, i, cx)).collect();
        let scanned = if s.github.scanned_repos.is_empty() {
            "No repositories scanned yet.".to_string()
        } else {
            format!("Scanning {} repos: {}", s.github.scanned_repos.len(), s.github.scanned_repos.join(", "))
        };
        page("jobs-page", &self.page_scroll)
            .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(scanned))
            .when(!s.github.configured && !s.demo, |d| {
                d.child(callout(
                    &theme,
                    "warning",
                    "Jobs come from GitHub",
                    "Add your organization or repositories in Settings to see what's queued and running.".into(),
                    Some(
                        Button::new("jobs-settings", "Open settings")
                            .style(ButtonStyle::Primary)
                            .on_click(cx.listener(|this, _, window, cx| this.navigate(Route::Settings, window, cx)))
                            .into_any_element(),
                    ),
                ))
            })
            .child(self.render_queued(&theme, &s, "jobs-q"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(section_title(&theme, "Running", Some(s.jobs.running.len())))
                    .child(card(&theme).when(running.is_empty(), |d| d.child(kit::empty_state(&theme, "circle", "Nothing is running."))).children(running)),
            )
    }

    fn running_job_row(&self, theme: &Theme, j: &Job, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let url = j.url.clone();
        let runner = j.runner_id.clone();
        div()
            .id(("running-job", i))
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(14.))
            .py(px(10.))
            .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
            .child(div().flex_none().w(px(3.)).h(px(26.)).rounded_full().bg(theme.accent))
            .child(job_title(theme, j))
            .child(
                div()
                    .id(("running-job-runner", i))
                    .w(px(150.))
                    .flex_none()
                    .truncate()
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .text_color(theme.text_accent)
                    .when(runner.is_some(), |d| d.cursor_pointer())
                    .child(j.runner_name.clone().unwrap_or_else(|| "—".into()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(r) = &runner {
                            this.navigate(Route::Runner(r.clone()), window, cx)
                        }
                    })),
            )
            .child(
                div()
                    .w(px(80.))
                    .flex_none()
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(format::elapsed(j.started_at.unwrap_or(j.created_at))),
            )
            .child(Button::icon_only(("running-job-open", i), "external-link").tooltip("Open on GitHub").on_click(move |_, _, cx| cx.open_url(&url)))
            .into_any_element()
    }
}

/// A message with a tinted background: attention items, warnings, errors.
pub(super) fn callout(theme: &Theme, level: &str, title: impl Into<SharedString>, detail: String, action: Option<AnyElement>) -> impl IntoElement {
    let color = theme.level(level);
    let icon_name = match level {
        "error" => "circle-x",
        "warning" => "triangle-alert",
        _ => "info",
    };
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .pl(px(12.))
        .pr(px(12.))
        .py(px(10.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme.border_variant)
        .bg(theme.surface)
        .child(
            div()
                .flex_none()
                .size(px(28.))
                .rounded(px(7.))
                .bg(color.opacity(0.16))
                .flex()
                .items_center()
                .justify_center()
                .child(icon(icon_name, 14., color)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap(px(2.))
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.text).child(title.into()))
                .when(!detail.is_empty(), |d| d.child(div().text_size(px(12.)).text_color(theme.text_muted).child(detail))),
        )
        .children(action)
}

fn stat_tiles(theme: &Theme, s: &Snapshot) -> impl IntoElement {
    let c = &s.counts;
    let stuck = s.jobs.queued.iter().filter(|j| j.matching_online == 0).count();
    let tile = |icon_name: &'static str, color: Hsla, title: &'static str, value: usize, sub: String| {
        card(theme)
            .flex_1()
            .min_w(px(0.))
            .p(px(16.))
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().size(px(24.)).rounded(px(7.)).bg(color.opacity(0.16)).flex().items_center().justify_center().child(icon(icon_name, 13., color)))
                    .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(title)),
            )
            .child(div().font_family(mono_font()).text_size(px(28.)).line_height(px(30.)).font_weight(FontWeight::MEDIUM).child(value.to_string()))
            .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(sub))
    };
    div()
        .flex()
        .gap(px(GAP))
        .child(tile("zap", theme.accent, "Working", c.working, format!("of {} runners", c.runners)))
        .child(tile("circle-check", theme.success, "Idle", c.idle, "online, ready for jobs".into()))
        .child(tile(
            "circle-x",
            theme.text_placeholder,
            "Offline",
            c.offline,
            if c.unknown > 0 { format!("{} more unknown", c.unknown) } else { "not connected".into() },
        ))
        .child(tile(
            "clock",
            if stuck > 0 { theme.warning } else { theme.info },
            "Queued",
            c.queued,
            if stuck > 0 { format!("{stuck} can't find a runner") } else { "self-hosted, scanned repos".into() },
        ))
}

fn meter_row(theme: &Theme, label: &'static str, value: Option<String>, fraction: Option<f32>, color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .text_size(px(11.5))
        .child(div().w(px(50.)).flex_none().text_color(theme.text_placeholder).child(label))
        .child(div().flex_1().child(meter(theme, fraction.unwrap_or(0.), color)))
        .child(
            div()
                .w(px(76.))
                .flex_none()
                .flex()
                .justify_end()
                .font_family(mono_font())
                .text_color(theme.text_muted)
                .child(value.unwrap_or_else(|| "—".into())),
        )
}

fn status_word(m: &Machine) -> &'static str {
    match m.status.as_str() {
        "online" => "online",
        "offline" => "unreachable",
        "manual" => "no host",
        _ => "unchecked",
    }
}

fn queued_row(theme: &Theme, j: &Job, i: usize, prefix: &str) -> AnyElement {
    let url = j.url.clone();
    let stuck = j.matching_online == 0;
    div()
        .id(SharedString::from(format!("{prefix}-{i}")))
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(14.))
        .py(px(10.))
        .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
        .child(div().flex_none().w(px(3.)).h(px(26.)).rounded_full().bg(if stuck { theme.warning } else { theme.track }))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap(px(6.))
                .child(job_title(theme, j))
                .child(div().flex().flex_wrap().gap(px(4.)).children(j.labels.iter().map(|l| chip(theme, l.clone())))),
        )
        .child(
            div()
                .w(px(150.))
                .flex_none()
                .flex()
                .flex_col()
                .items_end()
                .child(div().font_family(mono_font()).text_size(px(12.)).text_color(theme.text_muted).child(format!("waiting {}", format::elapsed(j.created_at))))
                .child(div().text_size(px(11.)).text_color(if stuck { theme.warning } else { theme.text_placeholder }).child(if stuck {
                    "no online runner matches".to_string()
                } else {
                    format!("{} runner{} can take it", j.matching_online, if j.matching_online == 1 { "" } else { "s" })
                })),
        )
        .child(
            Button::icon_only(SharedString::from(format!("{prefix}-open-{i}")), "external-link")
                .tooltip("Open on GitHub")
                .on_click(move |_, _, cx| cx.open_url(&url)),
        )
        .into_any_element()
}

fn job_title(theme: &Theme, j: &Job) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.))
        .child(div().truncate().text_size(px(13.)).child(j.name.clone()))
        .child(div().truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
            "{} · {}{}",
            j.repo,
            j.workflow,
            j.branch.as_ref().map(|b| format!(" · {b}")).unwrap_or_default()
        )))
}
