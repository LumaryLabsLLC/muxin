//! Schedules: commands and agent sessions that run on the user's machines at
//! set times (kept and run there; see fleet/schedules.rs), with each one's
//! runs and their output.

use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*, px};

use super::github::Load;
use super::kit::{self, Button, ButtonStyle, card, icon, section_title, spinner};
use super::overview::page;
use super::root::Root;
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::schedules::{MachineSchedules, Schedule, ScheduleLog};

/// The page's tile colors.
pub const TILE: (u32, u32) = (0x7cc7ff, 0x3a7bea);

impl Root {
    /// Asks every machine for its schedules (again).
    pub(super) fn load_schedules(&mut self, cx: &mut Context<Self>) {
        if self.schedules_loading {
            return;
        }
        self.schedules_loading = true;
        let job = self.engine.runtime().spawn({
            let engine = self.engine.clone();
            async move { engine.schedules(None).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.map_err(|e| e.to_string());
            let _ = this.update(cx, |this, cx| {
                this.schedules_loading = false;
                this.schedules = match result {
                    Ok(list) => Load::Ready(list),
                    Err(e) => Load::Failed(e),
                };
                cx.notify();
            });
        });
        self.track(task);
        cx.notify();
    }

    /// A schedule's runs and one's output.
    fn load_schedule_log(&mut self, machine: &str, id: &str, started: Option<i64>, cx: &mut Context<Self>) {
        let key = (machine.to_string(), id.to_string());
        self.schedule_logs.insert(key.clone(), Load::Loading);
        let job = self.engine.runtime().spawn({
            let (engine, machine, id) = (self.engine.clone(), machine.to_string(), id.to_string());
            async move { engine.schedule_log(&machine, &id, started).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.schedule_logs.insert(key, match result {
                    Ok(l) => Load::Ready(l),
                    Err(e) => Load::Failed(e),
                });
                cx.notify();
            });
        });
        self.track(task);
        cx.notify();
    }

    /// Runs an engine call on a schedule, then shows what changed.
    fn schedule_do(&mut self, machine: &str, id: &str, what: &'static str, cx: &mut Context<Self>) {
        let (engine, m, s) = (self.engine.clone(), machine.to_string(), id.to_string());
        let job = self.engine.runtime().spawn(async move {
            match what {
                "run" => engine.schedule_run(&m, &s).await,
                "delete" => engine.schedule_delete(&m, &s).await,
                _ => Err("unknown".into()),
            }
        });
        let (machine, id) = (machine.to_string(), id.to_string());
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) if what == "run" => {
                        this.notify_user("Started. Its output shows here when it's done.".to_string(), false, cx);
                        this.schedule_open = Some((machine.clone(), id.clone()));
                    }
                    Ok(()) => this.notify_user("Deleted".to_string(), false, cx),
                    Err(e) => this.notify_user(e, true, cx),
                }
                this.load_schedules(cx);
                if what == "run" {
                    // Its output, once it's had a moment.
                    let later = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                        let _ = this.update(cx, |this, cx| {
                            this.load_schedule_log(&machine, &id, None, cx);
                            this.load_schedules(cx);
                        });
                    });
                    this.track(later);
                }
            });
        });
        self.track(task);
    }

    /// Turns a schedule off (kept, not run) or on again.
    fn schedule_toggle(&mut self, machine: &str, s: &Schedule, cx: &mut Context<Self>) {
        let input = crate::fleet::schedules::ScheduleInput {
            id: Some(s.id.clone()),
            name: s.name.clone(),
            when: s.when.clone(),
            command: s.what.command.clone().filter(|_| s.what.kind == "command"),
            agent: s.what.agent.clone().filter(|_| s.what.kind == "agent"),
            prompt: s.what.prompt.clone(),
            folder: s.what.cwd.clone(),
            enabled: !s.enabled,
        };
        let (engine, m) = (self.engine.clone(), machine.to_string());
        let job = self.engine.runtime().spawn(async move { engine.schedule_save(&m, input).await });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.notify_user(e, true, cx);
                }
                this.load_schedules(cx);
            });
        });
        self.track(task);
    }

    pub(super) fn render_schedules(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if matches!(self.schedules, Load::Loading) && !self.schedules_loading {
            self.load_schedules(cx);
        }
        let header = div()
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
                    .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child("Schedules"))
                    .child(div().text_size(px(13.)).text_color(theme.text_muted).child("Commands and agent sessions that run on your machines at set times, in each machine’s own time. They run there, so they don’t need Muxen open.")),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(8.))
                    .child(Button::icon_only("sch-refresh", "refresh-cw").style(ButtonStyle::Ghost).tooltip("Ask the machines again").loading(self.schedules_loading).on_click(cx.listener(|this, _, _, cx| this.load_schedules(cx))))
                    .child(Button::new("sch-new", "New schedule").style(ButtonStyle::Primary).icon("plus").on_click(cx.listener(|this, _, window, cx| this.show_schedule_dialog(None, window, cx)))),
            );
        let mut body = page("schedules-page", &self.page_scroll).child(header);
        let list = match &self.schedules {
            Load::Ready(l) => l.clone(),
            Load::Loading => {
                return body
                    .child(div().flex().items_center().gap(px(8.)).text_size(px(12.5)).text_color(theme.text_muted).child(spinner("sch-load", 13., theme.text_muted)).child("Asking your machines…"))
                    .into_any_element();
            }
            Load::Failed(e) => return body.child(div().text_size(px(12.5)).text_color(theme.error).child(e.clone())).into_any_element(),
        };
        let any = list.iter().any(|m| !m.schedules.is_empty());
        if !any {
            body = body.child(empty(&theme, cx));
        }
        for m in list.iter().filter(|m| !m.schedules.is_empty()) {
            body = body.child(self.machine_schedules(&theme, m, cx));
        }
        // Where they can't run, said once, below.
        let problems: Vec<&MachineSchedules> = list.iter().filter(|m| m.problem.is_some() && !m.schedules.is_empty()).collect();
        for m in problems {
            body = body.child(div().text_size(px(12.)).text_color(theme.warning).child(format!("{}: {}", m.machine, m.problem.clone().unwrap_or_default())));
        }
        body.into_any_element()
    }

    fn machine_schedules(&mut self, theme: &Theme, m: &MachineSchedules, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = m.schedules.iter().enumerate().map(|(i, s)| self.schedule_row(theme, &m.machine_id, s, i, cx)).collect();
        let machine = self.snapshot.machines.iter().find(|x| x.id == m.machine_id).cloned();
        let title = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .children(machine.as_ref().map(|x| {
                let (from, to) = super::format::machine_tile(x);
                kit::tile(super::format::machine_icon(x), from, to, 18.)
            }))
            .child(section_title(theme, m.machine.clone(), Some(m.schedules.len())))
            .child(div().flex_1())
            .children(m.scheduler.clone().map(|s| {
                let by = match s.as_str() {
                    "cron" => "Run by cron".to_string(),
                    "launchd" => "Run by launchd".to_string(),
                    "task-scheduler" => "Run by Task Scheduler".to_string(),
                    _ => s,
                };
                div().text_size(px(11.5)).text_color(theme.text_placeholder).child(by)
            }));
        div().flex().flex_col().gap(px(10.)).child(title).child(card(theme).children(rows)).into_any_element()
    }

    fn schedule_row(&mut self, theme: &Theme, machine: &str, s: &Schedule, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let key = (machine.to_string(), s.id.clone());
        let open = self.schedule_open.as_ref() == Some(&key);
        let (glyph, (from, to)) = match s.what.kind.as_str() {
            "agent" => kit::agent_look(s.what.agent.as_deref().unwrap_or("shell")),
            _ => ("terminal", kit::tiles::RUN),
        };
        let what = match s.what.kind.as_str() {
            "agent" => {
                let name = crate::fleet::sessions::agent(s.what.agent.as_deref().unwrap_or("")).map(|a| a.name).unwrap_or("Agent");
                match &s.what.prompt {
                    Some(p) => format!("{name}: {p}"),
                    None => name.to_string(),
                }
            }
            _ => s.what.command.clone().unwrap_or_default(),
        };
        let folder = s.what.cwd.clone().filter(|c| c != "~").map(|c| format!(" · in {c}")).unwrap_or_default();
        let (last, last_color) = match &s.last_run {
            _ if s.running => ("Running…".to_string(), theme.text_accent),
            Some(r) if r.session.is_some() => (format!("Started a session {}", ago(r.started)), theme.text_placeholder),
            Some(r) if r.ok => (format!("Ran {}", ago(r.started)), theme.success),
            Some(r) => (format!("Failed {}{}", ago(r.started), r.exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default()), theme.error),
            None => ("Not run yet".to_string(), theme.text_placeholder),
        };
        let (m1, m2, m3) = (machine.to_string(), machine.to_string(), machine.to_string());
        let (id1, id2, s_toggle) = (s.id.clone(), s.id.clone(), s.clone());
        let key_click = key.clone();
        let row = div()
            .id(SharedString::from(format!("sch-{}-{}", machine, s.id)))
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(14.))
            .py(px(10.))
            .cursor_pointer()
            .hover(|st| st.bg(theme.ghost_hover))
            .when(!s.enabled, |d| d.opacity(0.6))
            .child(kit::tile(glyph, from, to, 28.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child(s.name.clone()))
                            .child(div().text_size(px(12.)).text_color(theme.text_muted).child(s.when_text.clone()))
                            .when(!s.enabled, |d| d.child(kit::pill("off", theme.text_placeholder))),
                    )
                    .child(div().truncate().when(s.what.kind == "command", |d| d.font_family(mono_font())).text_size(px(12.)).text_color(theme.text_placeholder).child(format!("{what}{folder}"))),
            )
            .child(div().flex_none().text_size(px(12.)).text_color(last_color).child(last))
            .child(
                Button::new(SharedString::from(format!("sch-run-{}", s.id)), "Run now").style(ButtonStyle::Ghost).icon("play").disabled(s.running).on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.schedule_do(&m1, &id1, "run", cx)
                })),
            )
            .child(
                Button::new(SharedString::from(format!("sch-toggle-{}", s.id)), if s.enabled { "Pause" } else { "Resume" }).style(ButtonStyle::Ghost).on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.schedule_toggle(&m2, &s_toggle, cx)
                })),
            )
            .child(
                Button::icon_only(SharedString::from(format!("sch-del-{}", s.id)), "trash-2").style(ButtonStyle::Ghost).tooltip("Delete it, with its runs").on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.schedule_do(&m3, &id2, "delete", cx)
                })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.schedule_open.as_ref() == Some(&key_click) {
                    this.schedule_open = None;
                } else {
                    this.schedule_open = Some(key_click.clone());
                    this.load_schedule_log(&key_click.0, &key_click.1, None, cx);
                }
                cx.notify()
            }));
        let detail = open.then(|| self.schedule_detail(theme, &key, cx));
        div().when(i > 0, |d| d.border_t_1().border_color(theme.border_variant)).child(row).children(detail).into_any_element()
    }

    /// Its runs, and the chosen one's output.
    fn schedule_detail(&mut self, theme: &Theme, key: &(String, String), cx: &mut Context<Self>) -> AnyElement {
        let state = match self.schedule_logs.get(key) {
            Some(Load::Ready(l)) => Some(Ok(l.clone())),
            Some(Load::Failed(e)) => Some(Err(e.clone())),
            _ => None,
        };
        let content: AnyElement = match state {
            None => div().flex().items_center().gap(px(8.)).text_size(px(12.)).text_color(theme.text_muted).child(spinner("sch-log", 12., theme.text_muted)).child("Reading its runs…").into_any_element(),
            Some(Err(e)) => div().text_size(px(12.)).text_color(theme.error).child(e).into_any_element(),
            Some(Ok(l)) => self.runs_view(theme, key, &l, cx),
        };
        div().px(px(14.)).pb(px(12.)).pl(px(54.)).child(content).into_any_element()
    }

    fn runs_view(&mut self, theme: &Theme, key: &(String, String), l: &ScheduleLog, cx: &mut Context<Self>) -> AnyElement {
        if l.runs.is_empty() {
            return div().text_size(px(12.)).text_color(theme.text_placeholder).child("It hasn't run yet. Run now runs it once.").into_any_element();
        }
        let shown = l.run.as_ref().map(|r| r.started);
        let chips: Vec<AnyElement> = l
            .runs
            .iter()
            .take(12)
            .map(|r| {
                let (mark, color) = if r.session.is_some() || r.ok { ("check", theme.success) } else { ("x", theme.error) };
                let selected = shown == Some(r.started);
                let (m, id, t) = (key.0.clone(), key.1.clone(), r.started);
                div()
                    .id(SharedString::from(format!("sch-r-{}", r.started)))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(24.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border_variant })
                    .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                    .text_size(px(11.5))
                    .cursor_pointer()
                    .child(icon(mark, 11., color))
                    .child(ago(r.started))
                    .on_click(cx.listener(move |this, _, _, cx| this.load_schedule_log(&m, &id, Some(t), cx)))
                    .into_any_element()
            })
            .collect();
        let output: AnyElement = match l.run.as_ref() {
            Some(r) if r.session.is_some() => {
                let session = format!("{}/{}", key.0, r.session.clone().unwrap_or_default());
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(format!("It started the session {}.", r.session.clone().unwrap_or_default()))
                    .child(Button::new("sch-open-session", "Open it").style(ButtonStyle::Ghost).on_click(cx.listener(move |this, _, window, cx| this.navigate(super::root::Route::Session(session.clone()), window, cx))))
                    .into_any_element()
            }
            _ if l.output.trim().is_empty() => div().text_size(px(12.)).text_color(theme.text_placeholder).child("It printed nothing.").into_any_element(),
            _ => div()
                .id("sch-output")
                .max_h(px(280.))
                .overflow_y_scroll()
                .p(px(10.))
                .rounded(px(8.))
                .bg(theme.canvas)
                .border_1()
                .border_color(theme.border_variant)
                .font_family(mono_font())
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .children(l.output.lines().map(|line| div().child(line.to_string())))
                .into_any_element(),
        };
        let status = l.run.as_ref().map(|r| {
            let what = match (r.ok, r.exit_code, r.seconds) {
                _ if r.session.is_some() => String::new(),
                (true, _, Some(s)) => format!("Finished in {}", crate::util::short_duration(s)),
                (false, Some(c), _) => format!("Exited with {c}"),
                (false, None, _) => r.error.clone().unwrap_or_else(|| "Ended without an exit status (stopped after 6 hours?)".into()),
                _ => String::new(),
            };
            div().text_size(px(11.5)).text_color(theme.text_placeholder).child(what)
        });
        div().flex().flex_col().gap(px(8.)).child(div().flex().flex_wrap().gap(px(6.)).children(chips)).children(status).child(output).into_any_element()
    }
}

fn empty(theme: &Theme, cx: &mut Context<Root>) -> AnyElement {
    let example = |text: &'static str| div().flex().items_start().gap(px(8.)).text_size(px(12.5)).text_color(theme.text_muted).child(icon("clock", 13., theme.text_placeholder)).child(text);
    card(theme)
        .p(px(18.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(div().text_size(px(13.5)).font_weight(FontWeight::MEDIUM).child("No schedules yet"))
        .child(example("Every morning at 9, have Claude Code go through new issues in a repo."))
        .child(example("Weekdays at 18:00, run the test suite and keep its output."))
        .child(example("Every 15 minutes, check a site or a disk and print what it found."))
        .child(div().pt(px(4.)).flex().child(Button::new("sch-empty-new", "New schedule").style(ButtonStyle::Primary).icon("plus").on_click(cx.listener(|this, _, window, cx| this.show_schedule_dialog(None, window, cx)))))
        .into_any_element()
}

/// "5m ago" from Unix seconds.
fn ago(t: i64) -> String {
    let secs = (chrono::Utc::now().timestamp() - t).max(0) as u64;
    if secs < 60 { "just now".into() } else { format!("{} ago", crate::util::short_duration(secs)) }
}
