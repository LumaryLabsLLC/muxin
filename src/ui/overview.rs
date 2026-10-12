//! The overview (your machines first: what's running on each, then the agent
//! sessions across them) and the jobs page (with how busy the runners are).

use gpui::{AnyElement, Context, Div, FontWeight, Hsla, ScrollHandle, SharedString, Stateful, Window, div, prelude::*, px, relative};

use super::format;
use super::kit::{self, Button, ButtonStyle, card, chip, icon, meter, section_title};
use super::theme::{ActiveTheme, mono_font, Theme};
use super::root::{Route, Root};
use crate::fleet::model::{Job, Machine, Snapshot};
use crate::fleet::sessions::SessionInfo;

const CARD_MIN_WIDTH: f32 = 300.;
const GAP: f32 = 14.;
const PAGE_PADDING: f32 = 28.;
/// Lines in a machine card's "what's running" list.
const NOW_ROWS: usize = 3;
const NOW_ROW_H: f32 = 26.;

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

        let mut body = page("overview-page", &self.page_scroll).child(self.overview_header(&theme, &s, cx));
        if let Some(err) = &s.inventory.error {
            body = body.child(callout(&theme, "error", "config.toml has an error", err.clone(), None));
        }
        if !s.attention.is_empty() {
            body = body.child(self.heads_up(&theme, &s, content_width, cx));
        }
        body.child(self.render_machine_grid(&theme, &s, content_width, cx)).child(self.render_agents(&theme, &s, cx))
    }

    /// A greeting, then the day in one line: machines, agents, runners.
    fn overview_header(&self, theme: &Theme, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        use chrono::Timelike;
        let greeting = match chrono::Local::now().hour() {
            5..=11 => "Good morning",
            12..=17 => "Good afternoon",
            _ => "Good evening",
        };
        let plural = |n: usize, one: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") };
        let online = s.machines.iter().filter(|m| m.status == "online").count();
        let working = s.sessions.iter().filter(|x| x.busy && x.status == "running").count();
        let waiting = s.sessions.iter().filter(|x| x.attention.is_some()).count();
        let mut parts = vec![plural(s.machines.len(), "machine"), format!("{online} online")];
        if working > 0 {
            parts.push(format!("{} working", plural(working, "agent")));
        }
        if waiting > 0 {
            parts.push(format!("{waiting} waiting for you"));
        }
        if s.counts.runners > 0 {
            parts.push(format!("{} of {} busy", s.counts.working, plural(s.counts.runners, "runner")));
        }
        let checked = match (s.check.running, s.check.finished_at) {
            (true, _) => "Checking your machines now…".to_string(),
            (false, Some(_)) => format!("Updated {}", format::ago(s.check.finished_at)),
            (false, None) => "Not updated yet".into(),
        };
        div()
            .flex()
            .items_end()
            .justify_between()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .min_w(px(0.))
                    .child(div().text_size(px(26.)).line_height(px(32.)).font_weight(FontWeight::SEMIBOLD).child(greeting))
                    .child(div().text_size(px(13.)).text_color(theme.text_muted).child(parts.join(" · ")))
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(checked)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(8.))
                    .child(
                        Button::new("ov-new-session", "New session")
                            .style(ButtonStyle::Primary)
                            .icon("square-terminal")
                            .on_click(cx.listener(|this, _, window, cx| this.show_new_session(None, window, cx))),
                    ),
            )
    }

    /// What needs a look, one compact line each; the first few unless opened.
    fn heads_up(&self, theme: &Theme, s: &Snapshot, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        const SHOWN: usize = 3;
        let hidden = if self.heads_up_open { 0 } else { s.attention.len().saturating_sub(SHOWN) };
        let mut rows: Vec<AnyElement> = s
            .attention
            .iter()
            .take(s.attention.len() - hidden)
            .enumerate()
            .map(|(i, a)| {
                let color = theme.level(&a.level);
                let icon_name = match a.level.as_str() {
                    "error" => "circle-x",
                    "warning" => "triangle-alert",
                    _ => "info",
                };
                let button = a.action.clone().map(|action| {
                    Button::new(SharedString::from(format!("att-{}", a.id)), action.label.clone())
                        .style(ButtonStyle::Ghost)
                        .on_click(cx.listener(move |this, _, window, cx| this.run_attention_action(&action, window, cx)))
                        .into_any_element()
                });
                // Explicit widths: GPUI 0.2 only ellipsizes text whose width it knows.
                let text_w = (width - 2. - 28. - 14. - 12. - if button.is_some() { 140. } else { 0. }).max(120.);
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(8.))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .child(icon(icon_name, 14., color))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().w(px(text_w)).truncate().text_size(px(13.)).text_color(theme.text).child(a.title.clone()))
                            .when(!a.detail.is_empty(), |d| d.child(div().w(px(text_w)).truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(a.detail.clone()))),
                    )
                    .children(button)
                    .into_any_element()
            })
            .collect();
        if s.attention.len() > SHOWN {
            let label = if hidden > 0 { format!("Show {hidden} more") } else { "Show fewer".to_string() };
            rows.push(
                div()
                    .id("heads-up-more")
                    .px(px(14.))
                    .py(px(7.))
                    .border_t_1()
                    .border_color(theme.border_variant)
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(theme.text_accent)
                    .hover(|st| st.bg(theme.ghost_hover))
                    .child(label)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.heads_up_open = !this.heads_up_open;
                        cx.notify()
                    }))
                    .into_any_element(),
            );
        }
        card(theme).children(rows)
    }

    fn render_machine_grid(&self, theme: &Theme, s: &Snapshot, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let favorites = cx.global::<super::theme::Appearance>().favorite_machines.clone();
        let machines: Vec<&Machine> = Self::machine_order(s, &favorites);
        let cols = (((width + GAP) / (CARD_MIN_WIDTH + GAP)).floor() as usize).clamp(1, 3);
        let card_width = ((width - GAP * (cols as f32 - 1.)) / cols as f32).floor();
        // Explicit rows rather than flex-wrap, so the section's height is always right.
        let cards: Vec<AnyElement> = machines.iter().map(|m| self.machine_card(theme, s, m, card_width, cx)).collect();
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut iter = cards.into_iter().peekable();
        while iter.peek().is_some() {
            let row: Vec<AnyElement> = iter.by_ref().take(cols).collect();
            rows.push(div().flex().gap(px(GAP)).children(row).into_any_element());
        }
        div()
            .flex()
            .flex_col()
            .gap(px(GAP))
            .children(rows)
            .when(machines.is_empty(), |d| {
                d.child(card(theme).w_full().child(kit::empty_state(theme, "server", "No machines yet. Add one to run agents, see its desktop or host runners.")))
            })
    }

    /// A machine and what's on it right now: its agents, desktop and runners.
    fn machine_card(&self, theme: &Theme, s: &Snapshot, m: &Machine, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let id = m.id.clone();
        let inner = width - 2. - 32.;
        let (from, to) = format::machine_tile(m);
        let (subtitle, tone) = format::machine_subtitle(m);
        let offline = m.status == "offline";
        let desktop = cx.global::<super::theme::Appearance>().desktops.contains(&m.id);

        // What's running: its agents (working ones first), its desktop, its runners.
        let mut now: Vec<AnyElement> = Vec::new();
        if !offline {
            let mut sessions: Vec<&SessionInfo> = s.sessions.iter().filter(|x| x.machine_id == m.id).collect();
            sessions.sort_by_key(|x| (!x.busy, x.attention.is_none(), std::cmp::Reverse(x.created_at)));
            let others = usize::from(desktop) + usize::from(!m.runner_ids.is_empty());
            let room = NOW_ROWS.saturating_sub(others).max(1);
            let extra = sessions.len().saturating_sub(room);
            for x in sessions.iter().take(if extra > 0 { room - 1 } else { room }) {
                let (glyph, (gf, gt)) = kit::agent_look(&x.agent);
                let (state, color) = if x.status == "exited" {
                    ("Exited", theme.text_placeholder)
                } else if x.busy {
                    ("Working…", theme.text_accent)
                } else if x.attention.is_some() {
                    ("Waiting for you", theme.warning)
                } else {
                    ("Idle", theme.text_placeholder)
                };
                now.push(now_row(theme, kit::tile(glyph, gf, gt, 18.).into_any_element(), x.title.clone(), state, color, inner));
            }
            if extra > 0 {
                now.push(now_row(theme, kit::color_tile("square-terminal", 18.).into_any_element(), format!("{} more sessions", extra + 1), "", theme.text_placeholder, inner));
            }
            if desktop {
                now.push(now_row(theme, kit::tile("monitor", from, to, 18.).into_any_element(), "Desktop".into(), "Open", theme.text_placeholder, inner));
            }
            if !m.runner_ids.is_empty() {
                let sum = &m.summary;
                let label = if sum.total == 1 { "1 runner".to_string() } else { format!("{} runners", sum.total) };
                let (state, color) = if sum.working > 0 {
                    (format!("{} busy", sum.working), theme.text_accent)
                } else if sum.offline > 0 {
                    (format!("{} stopped", sum.offline), theme.warning)
                } else {
                    ("Idle".to_string(), theme.text_placeholder)
                };
                let (rf, rt) = kit::tiles::JOBS;
                now.push(now_row_owned(theme, kit::tile("workflow", rf, rt, 18.).into_any_element(), label, state, color, inner));
            }
        }
        let empty_line = if offline {
            Some(match &m.unreachable {
                Some(u) if u.kind == "offline" => match u.last_seen_at {
                    Some(t) => format!("Asleep or off the network. Last seen {}.", format::ago(Some(t))),
                    None => "Asleep or off the network.".into(),
                },
                Some(u) => u.title.clone(),
                None => "Can't connect.".into(),
            })
        } else if now.is_empty() {
            Some(if m.agents.is_empty() { "Nothing running.".to_string() } else { "Nothing running. Start an agent here.".to_string() })
        } else {
            None
        };

        // Its resources, in one line.
        let p = m.probe.as_ref().filter(|p| p.ok && !offline);
        let footer: AnyElement = match p {
            Some(p) => {
                let cpu = p.cpu_pct.map(|v| v as f32 / 100.);
                let mem = format::mem_fraction(m);
                let disk_free = p.disk_free.map(|f| format!("{} free", format::bytes(f)));
                div()
                    .flex()
                    .gap(px(14.))
                    .child(mini_stat(theme, "CPU", cpu.map(|f| format!("{:.0}%", f * 100.)), cpu, theme.accent))
                    .child(mini_stat(theme, "Memory", mem.map(|f| format!("{:.0}%", f * 100.)), mem, theme.accent))
                    .child(mini_stat(theme, "Disk", disk_free, format::disk_used_fraction(m), if format::disk_low(m) { theme.error } else { theme.accent }))
                    .into_any_element()
            }
            None => div()
                .text_size(px(11.5))
                .text_color(theme.text_placeholder)
                .child(m.probe.as_ref().and_then(|p| p.uptime_sec).filter(|_| !offline).map(|u| format!("Up {}", format::uptime(u))).unwrap_or_default())
                .into_any_element(),
        };

        let status = match (m.checking, m.cloud.as_ref()) {
            (true, _) => kit::spinner(SharedString::from(format!("mcard-spin-{}", m.id)), 12., theme.text_muted).into_any_element(),
            (false, Some(c)) if m.status == "online" => div().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_muted).child(format!("${:.3}/h", c.price_hourly)).into_any_element(),
            _ => div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .child(kit::dot(theme.machine_status(m), 7.))
                .child(status_word(m))
                .into_any_element(),
        };

        div()
            .id(SharedString::from(format!("mcard-{}", m.id)))
            .w(px(width))
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .rounded(px(14.))
            .border_1()
            .border_color(theme.border_variant)
            .bg(theme.surface)
            .cursor_pointer()
            .hover(|st| st.border_color(theme.accent.opacity(0.55)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(div().when(offline, |d| d.opacity(0.55)).child(kit::tile(format::machine_icon(m), from, to, 36.)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().w(px(inner - 48. - 90.)).truncate().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(m.name.clone()))
                            .child(
                                div()
                                    .w(px(inner - 48. - 90.))
                                    .truncate()
                                    .text_size(px(11.5))
                                    .text_color(if tone == "error" { theme.error } else { theme.text_placeholder })
                                    .child(subtitle),
                            ),
                    )
                    .child(status),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_h(px(NOW_ROW_H * NOW_ROWS as f32))
                    .children(now)
                    .children(empty_line.map(|t| div().pt(px(4.)).text_size(px(12.)).text_color(if offline && !format::is_just_offline(m) { theme.error } else { theme.text_placeholder }).child(t))),
            )
            .child(div().pt(px(10.)).border_t_1().border_color(theme.border_variant).child(footer))
            .on_mouse_down(gpui::MouseButton::Right, {
                let mid = m.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Machine(mid.clone()), cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(id.clone()), window, cx)))
            .into_any_element()
    }

    /// Every agent session, across the machines: working first, then waiting.
    fn render_agents(&self, theme: &Theme, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let mut sessions: Vec<&SessionInfo> = s.sessions.iter().collect();
        sessions.sort_by_key(|x| (!x.busy, x.attention.is_none(), x.status == "exited", std::cmp::Reverse(x.created_at)));
        let content = if sessions.is_empty() {
            card(theme).child(kit::empty_state(theme, "square-terminal", "No agents running. Start Claude Code, Codex or a shell on any machine.")).into_any_element()
        } else {
            card(theme).children(sessions.iter().enumerate().map(|(i, x)| self.machine_session_row(theme, x, i, true, cx))).into_any_element()
        };
        div().flex().flex_col().gap(px(10.)).child(section_title(theme, "Agents", Some(sessions.len()))).child(content)
    }

    /// How busy the runners have been over the last few minutes.
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
        let busy = if c.runners == 0 { "No runners yet".to_string() } else { format!("{} of {} online runners busy", c.working, online) };
        let stuck = s.jobs.queued.iter().filter(|j| j.matching_online == 0).count();
        let queue = match (c.queued, stuck) {
            (0, _) => "Nothing queued".to_string(),
            (q, 0) => format!("{q} queued"),
            (q, n) => format!("{q} queued, {n} with no runner that fits"),
        };
        card(theme)
            .flex_row()
            .gap(px(24.))
            .p(px(18.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(230.))
                    .flex_none()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(theme.text_muted)
                            .child(kit::color_icon(theme, "gauge", 13., theme.text_accent))
                            .child("Runners busy"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap(px(4.))
                            .child(div().font_family(mono_font()).text_size(px(34.)).line_height(px(38.)).font_weight(FontWeight::MEDIUM).child(format!("{:.0}", pct * 100.)))
                            .child(div().pb(px(5.)).font_family(mono_font()).text_size(px(16.)).text_color(theme.text_muted).child("%")),
                    )
                    .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(busy))
                    .child(div().text_size(px(11.5)).text_color(if stuck > 0 { theme.warning } else { theme.text_placeholder }).child(queue)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(8.))
                    .child(div().flex().items_end().gap(px(3.)).h(px(76.)).children(bars))
                    .child(div().flex().justify_between().text_size(px(11.)).text_color(theme.text_placeholder).child("the last few minutes").child("now")),
            )
    }

    /// The queue, `width` wide (its rows' text needs to know).
    pub(super) fn render_queued(&self, theme: &Theme, s: &Snapshot, prefix: &str, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = s.jobs.queued.iter().enumerate().map(|(i, j)| queued_row(theme, j, i, prefix, width, j.waiting_reason(&s.runners), cx)).collect();
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
            .child(self.pulse_card(&theme, &s))
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
                            .on_click(cx.listener(|this, _, window, cx| this.open_settings_tab(super::settings::SettingsTab::GitHub, window, cx)))
                            .into_any_element(),
                    ),
                ))
            })
            .child(self.render_queued(&theme, &s, "jobs-q", self.content_width, cx))
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
        let job_id = j.id;
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
            .child(job_title(theme, j, None))
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
            .child(Button::icon_only(("running-job-more", i), "ellipsis").tooltip("More").on_click(job_menu(job_id, cx)))
            .on_mouse_down(gpui::MouseButton::Right, super::menu::on_right_click(move || super::menu::MenuTarget::Job(job_id), cx))
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

/// One line of a machine card's "what's running" list: a glyph, what it is, its state.
fn now_row(theme: &Theme, glyph: AnyElement, text: String, state: &'static str, color: Hsla, width: f32) -> AnyElement {
    now_row_owned(theme, glyph, text, state.to_string(), color, width)
}

fn now_row_owned(theme: &Theme, glyph: AnyElement, text: String, state: String, color: Hsla, width: f32) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(9.))
        .h(px(NOW_ROW_H))
        .child(glyph)
        .child(div().w(px((width - 18. - 9. * 2. - 104.).max(40.))).truncate().text_size(px(12.5)).text_color(theme.text).child(text))
        .child(div().flex_1().flex().justify_end().whitespace_nowrap().text_size(px(11.5)).text_color(color).child(state))
        .into_any_element()
}

/// A resource in a machine card's footer: its name and value over a thin bar.
fn mini_stat(theme: &Theme, label: &'static str, value: Option<String>, fraction: Option<f32>, color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.))
        .gap(px(5.))
        .child(
            div()
                .flex()
                .justify_between()
                .gap(px(4.))
                .text_size(px(11.))
                .child(div().text_color(theme.text_placeholder).child(label))
                .child(div().font_family(mono_font()).whitespace_nowrap().text_color(theme.text_muted).child(value.unwrap_or_else(|| "—".into()))),
        )
        .child(meter(theme, fraction.unwrap_or(0.), color))
}

fn status_word(m: &Machine) -> &'static str {
    match m.status.as_str() {
        "online" => "online",
        "offline" if format::is_powered_off(m) => "powered off",
        "offline" if format::is_just_offline(m) => "offline",
        "offline" => "can't connect",
        "manual" => "no host",
        _ => "unchecked",
    }
}

fn queued_row(theme: &Theme, j: &Job, i: usize, prefix: &str, width: f32, reason: String, cx: &mut Context<Root>) -> AnyElement {
    let url = j.url.clone();
    let id = j.id;
    let stuck = j.matching_online == 0;
    // The row's chrome: border, padding, the bar, gaps, the wait column and the buttons.
    let title_w = (width - 2. - 28. - 3. - 4. * 12. - 190. - 26. - 26. - if stuck { 112. } else { 0. }).max(80.);
    div()
        .id(SharedString::from(format!("{prefix}-{i}")))
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(14.))
        .py(px(10.))
        .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
        .on_mouse_down(gpui::MouseButton::Right, super::menu::on_right_click(move || super::menu::MenuTarget::Job(id), cx))
        .child(div().flex_none().w(px(3.)).h(px(26.)).rounded_full().bg(if stuck { theme.warning } else { theme.track }))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap(px(6.))
                .child(job_title(theme, j, Some(title_w)))
                .child(div().flex().flex_wrap().gap(px(4.)).children(j.labels.iter().map(|l| chip(theme, l.clone())))),
        )
        .child(
            div()
                .w(px(190.))
                .flex_none()
                .flex()
                .flex_col()
                .items_end()
                .child(div().font_family(mono_font()).text_size(px(12.)).text_color(theme.text_muted).child(format!("waiting {}", format::elapsed(j.created_at))))
                .child(div().text_size(px(11.)).text_color(if stuck { theme.warning } else { theme.text_placeholder }).child(reason)),
        )
        // Nothing can take it: the fix, one click away (pick the machine in the menu).
        .when(stuck, |d| {
            d.child(Button::new(SharedString::from(format!("{prefix}-add-{i}")), "Add runner").icon("plus").on_click(job_menu(id, cx)))
        })
        .child(
            Button::icon_only(SharedString::from(format!("{prefix}-open-{i}")), "external-link")
                .tooltip("Open on GitHub")
                .on_click(move |_, _, cx| cx.open_url(&url)),
        )
        .child(Button::icon_only(SharedString::from(format!("{prefix}-more-{i}")), "ellipsis").tooltip("More").on_click(job_menu(id, cx)))
        .into_any_element()
}

/// Opens a job's menu where the pointer is (for its buttons).
fn job_menu(id: u64, cx: &mut Context<Root>) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _, window, cx| this.open_menu(super::menu::MenuTarget::Job(id), window.mouse_position(), cx))
}

/// The job's name, then where it's from. `width`: GPUI 0.2 only ellipsizes
/// text whose width it knows before layout, which a row inside a column doesn't.
fn job_title(theme: &Theme, j: &Job, width: Option<f32>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.))
        .child(div().when_some(width, |d, w| d.w(px(w))).truncate().text_size(px(13.)).child(j.name.clone()))
        .child(div().when_some(width, |d, w| d.w(px(w))).truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
            "{} · {}{}",
            j.repo,
            j.workflow,
            j.branch.as_ref().map(|b| format!(" · {b}")).unwrap_or_default()
        )))
}
