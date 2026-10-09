//! Machine and runner pages, where runners are installed and controlled.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

use super::format;
use super::kit::{self, Button, ButtonStyle, card, chip, icon, kv, meter, pill, section_title};
use super::overview::page;
use super::theme::{ActiveTheme, mono_font, Theme};
use super::root::{Route, Root};
use crate::fleet::model::{Diagnostic, Machine, Operation, Runner, Snapshot, Unreachable};
use crate::ops::RunnerAction;

impl Root {
    pub(super) fn render_machine(&mut self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let Some(m) = s.machines.iter().find(|m| m.id == id).cloned() else {
            return gone(&theme, "This machine is no longer in config.toml.").into_any_element();
        };
        let p = m.probe.as_ref().filter(|p| p.ok);
        let busy = s.operations.iter().any(|o| o.status == "running" && o.machine_id.as_deref() == Some(&m.id) && o.kind != "logs");

        let mut meta = vec![if m.host.is_empty() { "no host".to_string() } else { m.host.clone() }];
        if !m.platform.is_empty() {
            meta.push(m.platform.clone());
        }
        // The last check that reached it found this (an offline one keeps it).
        if let Some(v) = m.probe.as_ref().and_then(|p| p.os_version.clone()) {
            meta.push(v);
        }
        let offline = m.status == "offline";
        if offline {
            // Whatever we knew about its connection and git is stale now;
            // look again when it's back.
            self.machine_conn.remove(&m.id);
            self.machine_git.remove(&m.id);
        }
        let checked = match &m.probe {
            Some(p) => format!("Checked {} in {:.1}s", format::ago(Some(p.checked_at)), p.duration_ms as f64 / 1000.0),
            None => "Not checked yet".into(),
        };

        let header = div()
            .flex()
            .items_start()
            .justify_between()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .child({
                        let (from, to) = format::machine_tile(&m);
                        kit::tile(format::machine_icon(&m), from, to, 40.)
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(m.name.clone()))
                                    .child(pill(status_label(&m), theme.machine_status(&m)))
                                    .child({
                                        let (mid, favorite) = (m.id.clone(), Root::is_favorite(cx, &m.id));
                                        Button::icon_only("m-favorite", if favorite { "star-filled" } else { "star" })
                                            .tooltip(if favorite { "Remove from favorites" } else { "Add to favorites" })
                                            .on_click(cx.listener(move |this, _, _, cx| this.set_favorite_machine(&mid, !favorite, cx)))
                                    }),
                            )
                            .child(div().font_family(mono_font()).text_size(px(12.)).text_color(theme.text_muted).child(meta.join("  ·  ")))
                            .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(checked)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .child({
                        let mid = m.id.clone();
                        Button::new("m-install", "Install runner")
                            .icon("plus")
                            .style(ButtonStyle::Primary)
                            .disabled(m.host.is_empty() || busy || offline)
                            .on_click(cx.listener(move |this, _, window, cx| this.show_install_dialog(&mid, window, cx)))
                    })
                    .child({
                        let mid = m.id.clone();
                        Button::new("m-session", "New session")
                            .icon("square-terminal")
                            .disabled(m.host.is_empty() || offline)
                            .on_click(cx.listener(move |this, _, window, cx| this.show_new_session(Some(mid.clone()), window, cx)))
                    })
                    .when(!m.platform.starts_with("windows"), |d| {
                        let mid = m.id.clone();
                        d.child(
                            Button::new("m-copy", "Copy files")
                                .icon("folder")
                                .disabled(m.host.is_empty() || offline)
                                .on_click(cx.listener(move |this, _, window, cx| this.show_copy_files(Some(mid.clone()), None, window, cx))),
                        )
                    })
                    .child({
                        let mid = m.id.clone();
                        Button::new("m-check", "Check")
                            .icon("refresh-cw")
                            .loading(m.checking)
                            .disabled(m.host.is_empty())
                            .on_click(cx.listener(move |this, _, _, _| this.engine.trigger_check(Some(mid.clone()))))
                    })
                    .child({
                        let machine = m.clone();
                        Button::icon_only("m-remove", "trash-2")
                            .style(ButtonStyle::Danger)
                            .tooltip(if machine.cloud.is_some() { "Destroy" } else { "Remove from fleet" })
                            .on_click(cx.listener(move |this, _, window, cx| this.confirm_remove_machine(&machine, window, cx)))
                    }),
            );

        let mut body = page("machine-page", &self.page_scroll).child(header);
        if let Some(c) = &m.cloud {
            body = body.child(self.cloud_card(&theme, &m, c, cx));
        }

        if let Some(u) = m.unreachable.as_ref().filter(|_| offline) {
            body = body.child(self.unreachable_card(&theme, &m, u, cx));
        }

        if let Some(p) = p {
            let mem = format::mem_fraction(&m);
            let disk = format::disk_used_fraction(&m);
            body = body.child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(metric_card(
                        &theme,
                        "CPU",
                        p.cpu_pct.map(|v| format!("{v:.0}%")),
                        p.cores.map(|c| format!("{c} cores")),
                        p.cpu_pct.map(|v| v as f32 / 100.0),
                        theme.accent,
                    ))
                    .child(metric_card(
                        &theme,
                        "Memory",
                        mem.map(|f| format!("{:.0}%", f * 100.0)),
                        p.mem_used.zip(p.mem_total).map(|(u, t)| format!("{} of {}", format::bytes(u), format::bytes(t))),
                        mem,
                        theme.accent,
                    ))
                    .child(metric_card(
                        &theme,
                        "Disk free",
                        p.disk_free.map(format::bytes),
                        p.disk_total.map(|t| format!("of {} · {}", format::bytes(t), p.disk_path.clone().unwrap_or_default())),
                        disk,
                        if format::disk_low(&m) { theme.error } else { theme.accent },
                    ))
                    .child(metric_card(
                        &theme,
                        "Uptime",
                        p.uptime_sec.map(format::uptime),
                        (!p.load.is_empty()).then(|| format!("load {}", p.load.iter().map(|l| format!("{l:.2}")).collect::<Vec<_>>().join(" "))),
                        None,
                        theme.accent,
                    )),
            );
        }

        if !m.host.is_empty() {
            body = body.child(self.machine_sessions(&theme, &s, &m, cx));
            // Connection and git are about a machine we can reach.
            if !m.platform.starts_with("windows") && !offline {
                if m.host != "local" {
                    body = body.child(self.machine_connection(&theme, &m, cx));
                }
                body = body.child(self.machine_git(&theme, &m, cx));
            }
        }

        let runners: Vec<&Runner> = m.runner_ids.iter().filter_map(|id| s.runners.iter().find(|r| &r.id == id)).collect();
        if m.kind == "runner" || !runners.is_empty() {
            let rows: Vec<AnyElement> = runners.iter().enumerate().map(|(i, r)| self.machine_runner_row(&theme, &s, r, i, busy, cx)).collect();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(section_title(&theme, "Runners", Some(runners.len())))
                    .child(card(&theme).children(rows)),
            );
        } else if !m.host.is_empty() && !offline {
            let mid = m.id.clone();
            body = body.child(
                card(&theme)
                    .p(px(16.))
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .child(kit::color_tile("box", 28.))
                    .child(div().flex_1().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text_muted).child(
                        "No runners on this machine yet. Install one and it will register with GitHub and start picking up jobs.",
                    ))
                    .child(
                        Button::new("m-install-empty", "Install runner")
                            .icon("plus")
                            .disabled(offline)
                            .on_click(cx.listener(move |this, _, window, cx| this.show_install_dialog(&mid, window, cx))),
                    ),
            );
        }

        // Being unreachable has its own card above.
        let diags: Vec<Diagnostic> = m.diagnostics.iter().filter(|d| d.code != "unreachable").cloned().collect();
        body = body.child(diagnostics_section(&theme, &diags, &s));
        if !m.host.is_empty() {
            body = body.child(self.machine_workspaces(&theme, &s, &m, cx));
        }
        body = body.child(self.operations_section(&theme, &s, |o| o.machine_id.as_deref() == Some(id), cx));
        if let Some(notes) = &m.notes {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(section_title(&theme, "Notes", None))
                    .child(card(&theme).p(px(14.)).child(div().w_full().text_size(px(12.5)).text_color(theme.text_muted).child(notes.clone()))),
            );
        }
        body.into_any_element()
    }

    /// Where a cloud machine runs, what it costs, and its power.
    fn cloud_card(&self, theme: &Theme, m: &Machine, c: &crate::fleet::model::CloudInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let off = m.status == "offline";
        let idle = c.idle_since.map(|t| (chrono::Utc::now() - t).num_minutes()).filter(|min| *min >= 30).map(|min| format!(" · idle {}", crate::util::short_duration(min as u64 * 60)));
        let detail = format!(
            "{} · {} · ${:.3}/h · ${:.2} so far{}",
            c.region.to_uppercase(),
            c.size,
            c.price_hourly,
            c.cost_so_far,
            idle.unwrap_or_default()
        );
        let (mid, machine) = (m.id.clone(), m.clone());
        card(theme)
            .flex_row()
            .items_center()
            .gap(px(12.))
            .px(px(14.))
            .py(px(12.))
            .child(kit::color_tile("cloud", 28.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .child(div().text_size(px(13.)).child(if off { "DigitalOcean · powered off (still billed)" } else { "DigitalOcean" }))
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(detail)),
            )
            .child(Button::new("m-cloud-power", if off { "Power on" } else { "Power off" }).icon("power").on_click(cx.listener(move |this, _, _, cx| this.cloud_power(&mid, off, cx))))
            .child(Button::new("m-cloud-destroy", "Destroy…").style(ButtonStyle::Danger).on_click(cx.listener(move |this, _, window, cx| this.confirm_remove_machine(&machine, window, cx))))
    }

    fn machine_runner_row(&self, theme: &Theme, s: &Snapshot, r: &Runner, i: usize, busy: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = r.id.clone();
        let job = r.job_id.and_then(|j| s.jobs.running.iter().find(|x| x.id == j));
        let local = r.local.as_ref();
        let service = local.and_then(|l| l.service_state.clone()).map(|st| format!("service {st}")).unwrap_or_else(|| {
            if local.is_some() { "no service".into() } else { "not installed".into() }
        });
        div()
            .id(SharedString::from(format!("mr-{}", r.id)))
            .group("runner-row")
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(14.))
            .py(px(9.))
            .cursor_pointer()
            .hover(|st| st.bg(theme.ghost_hover))
            .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
            .child(kit::runner_glyph(theme, &r.state, 10.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(190.))
                    .flex_none()
                    .child(div().truncate().font_family(mono_font()).text_size(px(12.5)).child(r.name.clone()))
                    .child(div().text_size(px(11.5)).text_color(theme.state(&r.state)).child(state_label(r))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(div().truncate().text_size(px(12.5)).text_color(if job.is_some() { theme.text } else { theme.text_placeholder }).child(
                        job.map(|j| j.name.clone()).unwrap_or_else(|| if r.state == "working" { "Running a job".into() } else { "—".into() }),
                    ))
                    .child(div().truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
                        "{}{}",
                        local.and_then(|l| l.version.clone()).map(|v| format!("v{v} · ")).unwrap_or_default(),
                        service
                    ))),
            )
            .child(self.runner_actions(theme, r, busy, "row", cx))
            .on_mouse_down(gpui::MouseButton::Right, {
                let rid = r.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Runner(rid.clone()), cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Runner(id.clone()), window, cx)))
            .into_any_element()
    }

    /// Start / stop / restart / logs / remove buttons for a runner.
    fn runner_actions(&self, theme: &Theme, r: &Runner, busy: bool, prefix: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let controllable = r.local.is_some() && r.machine_id.is_some();
        let running = r.local.as_ref().is_some_and(|l| l.listener);
        let mk = |action: RunnerAction, icon_name: &'static str, tip: &'static str, enabled: bool| {
            let id = r.id.clone();
            Button::icon_only(SharedString::from(format!("{prefix}-{}-{}", action.label(), r.id)), icon_name)
                .tooltip(tip)
                .disabled(!enabled)
                .on_click(cx.listener(move |this, _, _, cx| this.runner_action(&id, action, cx)))
        };
        let runner = r.clone();
        div()
            .flex()
            .flex_none()
            .gap(px(2.))
            .child(mk(RunnerAction::Start, "play", "Start", controllable && !running && !busy))
            .child(mk(RunnerAction::Stop, "square", "Stop", controllable && running && !busy))
            .child(mk(RunnerAction::Restart, "rotate-ccw", "Restart", controllable && !busy))
            .child(mk(RunnerAction::Logs, "scroll-text", "Show logs", controllable))
            .child(
                Button::icon_only(SharedString::from(format!("{prefix}-remove-{}", r.id)), "trash-2")
                    .style(ButtonStyle::Danger)
                    .tooltip("Remove runner")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm_remove_runner(&runner, window, cx))),
            )
            .text_color(theme.text_muted)
    }

    pub(super) fn render_runner(&mut self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let Some(r) = s.runners.iter().find(|r| r.id == id).cloned() else {
            return gone(&theme, "This runner is gone. It was removed, or renamed on its machine.").into_any_element();
        };
        let machine = r.machine_id.as_deref().and_then(|mid| s.machines.iter().find(|m| m.id == mid)).cloned();
        let busy = machine.as_ref().is_some_and(|m| {
            s.operations.iter().any(|o| o.status == "running" && o.machine_id.as_deref() == Some(&m.id) && o.kind != "logs")
        });
        let job = r.job_id.and_then(|j| s.jobs.running.iter().find(|x| x.id == j)).cloned();
        let source = match r.source.as_str() {
            "github" => "live from GitHub",
            "probe" => "from the last machine check",
            _ => "no data yet",
        };

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
                            .child(kit::runner_glyph(&theme, &r.state, 14.))
                            .child(div().font_family(mono_font()).text_size(px(20.)).font_weight(FontWeight::MEDIUM).child(r.name.clone()))
                            .child(pill(state_label(&r), theme.state(&r.state))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(theme.text_muted)
                            .children(machine.as_ref().map(|m| {
                                let mid = m.id.clone();
                                div()
                                    .id("runner-machine-link")
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .cursor_pointer()
                                    .text_color(theme.text_accent)
                                    .child(icon(format::machine_icon(m), 12., theme.text_accent))
                                    .child(m.name.clone())
                                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(mid.clone()), window, cx)))
                            }))
                            .when(machine.is_none(), |d| d.child("Not in config.toml"))
                            .child("·")
                            .child(format!("State {source}"))
                            .when(!r.declared && r.machine_id.is_some(), |d| d.child("·").child("discovered")),
                    )
                    .child(div().flex().flex_wrap().gap(px(4.)).children(r.labels.iter().map(|l| chip(&theme, l.clone())))),
            )
            .child(self.runner_actions(&theme, &r, busy, "page", cx));

        let mut body = page("runner-page", &self.page_scroll).child(header);

        if let Some(j) = &job {
            let url = j.url.clone();
            body = body.child(
                card(&theme)
                    .p(px(14.))
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .border_color(theme.accent.opacity(0.4))
                    .child(kit::spinner("runner-job-spin", 15., theme.accent))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().text_size(px(13.)).child(j.name.clone()))
                            .child(div().text_size(px(12.)).text_color(theme.text_muted).child(format!(
                                "{} · {}{}",
                                j.repo,
                                j.workflow,
                                j.actor.as_ref().map(|a| format!(" · by {a}")).unwrap_or_default()
                            ))),
                    )
                    .child(div().font_family(mono_font()).text_size(px(13.)).text_color(theme.text_muted).child(format::elapsed(j.started_at.unwrap_or(j.created_at))))
                    .child(Button::new("runner-job-open", "Open").icon("external-link").on_click(move |_, _, cx| cx.open_url(&url))),
            );
        }

        let gh = match &r.github {
            Some(g) => card(&theme)
                .px(px(14.))
                .py(px(8.))
                .child(kv(&theme, "Status", status_dot_text(&theme, &g.status, if g.status == "online" { theme.success } else { theme.text_placeholder })))
                .child(kv(&theme, "Busy", if g.busy { "yes" } else { "no" }))
                .child(kv(&theme, "Registered to", kit::mono(g.scope.clone())))
                .child(kv(&theme, "Runner id", kit::mono(g.id.to_string())))
                .child(kv(&theme, "OS", g.os.clone())),
            None => card(&theme).p(px(14.)).child(div().text_size(px(12.5)).text_color(theme.text_placeholder).child(
                if s.github.configured {
                    "GitHub doesn't list this runner in the orgs and repos you watch."
                } else {
                    "Add your GitHub organization in Settings to see this runner's live status."
                },
            )),
        };
        let local = match &r.local {
            Some(l) => card(&theme)
                .px(px(14.))
                .py(px(8.))
                .child(kv(&theme, "Folder", kit::mono(l.dir.clone())))
                .child(kv(&theme, "Version", kit::mono(l.version.clone().unwrap_or_else(|| "—".into()))))
                .child(kv(&theme, "Service", kit::mono(match (&l.service, &l.service_state) {
                    (Some(sv), Some(st)) => format!("{sv} ({st})"),
                    (Some(sv), None) => sv.clone(),
                    _ => "not installed as a service".into(),
                })))
                .child(kv(&theme, "Listener", kit::mono(l.listener_pid.map(|p| format!("running · pid {p}")).unwrap_or_else(|| "stopped".into()))))
                .child(kv(&theme, "Worker", kit::mono(l.worker_pid.map(|p| format!("running a job · pid {p}")).unwrap_or_else(|| "idle".into()))))
                .children(l.url.clone().map(|u| kv(&theme, "Config URL", kit::mono(u)))),
            None => card(&theme).p(px(14.)).child(div().text_size(px(12.5)).text_color(theme.text_placeholder).child(
                if machine.is_some() { "Not found on the machine at the last check." } else { "No machine in config.toml runs this runner." },
            )),
        };
        let wide = self.content_width >= 900.;
        body = body.child(
            div()
                .flex()
                .when(!wide, |d| d.flex_col())
                .gap(px(if wide { 16. } else { 26. }))
                .child(div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(10.)).child(section_title(&theme, "GitHub", None)).child(gh))
                .child(div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(10.)).child(section_title(&theme, "On the machine", None)).child(local)),
        );

        let diags: Vec<Diagnostic> =
            machine.iter().flat_map(|m| m.diagnostics.iter()).filter(|d| d.runner_id.as_deref() == Some(id)).cloned().collect();
        body = body.child(diagnostics_section(&theme, &diags, &s));
        let rid = r.id.clone();
        body = body.child(self.operations_section(&theme, &s, move |o| o.runner_id.as_deref() == Some(&rid), cx));
        body.into_any_element()
    }

    fn operations_section(&self, theme: &Theme, s: &Snapshot, filter: impl Fn(&Operation) -> bool, cx: &mut Context<Self>) -> AnyElement {
        let ops: Vec<&Operation> = s.operations.iter().filter(|o| filter(o)).take(6).collect();
        if ops.is_empty() {
            return div().into_any_element();
        }
        let rows: Vec<AnyElement> = ops
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let id = o.id;
                div()
                    .id(SharedString::from(format!("op-row-{}", o.id)))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(8.))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.ghost_hover))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .child(op_status_icon(theme, o))
                    .child(div().flex_1().truncate().text_size(px(12.5)).child(o.title.clone()))
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format::ago(Some(o.started_at))))
                    .on_click(cx.listener(move |this, _, _, cx| this.show_operation(id, cx)))
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Recent operations", None))
            .child(card(theme).children(rows))
            .into_any_element()
    }
}

pub(super) fn op_status_icon(theme: &Theme, o: &Operation) -> AnyElement {
    match o.status.as_str() {
        "running" => kit::spinner(SharedString::from(format!("op-spin-{}", o.id)), 13., theme.accent).into_any_element(),
        "succeeded" => icon("circle-check", 13., theme.success).into_any_element(),
        _ => icon("circle-x", 13., theme.error).into_any_element(),
    }
}

fn diagnostics_section(theme: &Theme, diags: &[Diagnostic], _s: &Snapshot) -> AnyElement {
    let rows: Vec<AnyElement> = diags
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let color = theme.level(&d.level);
            let icon_name = match d.level.as_str() {
                "error" => "circle-x",
                "warning" => "triangle-alert",
                "unknown" => "circle",
                _ => "info",
            };
            div()
                .flex()
                .items_start()
                .gap(px(10.))
                .px(px(14.))
                .py(px(9.))
                .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                .child(div().pt(px(1.)).child(icon(icon_name, 14., color)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(4.))
                        .child(div().text_size(px(12.5)).child(d.message.clone()))
                        .children(d.hint.clone().map(|h| copyable(theme, i, h))),
                )
                .into_any_element()
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(section_title(theme, "Diagnostics", Some(diags.len())))
        .child(card(theme).when(rows.is_empty(), |d| d.child(kit::empty_state(theme, "circle-check", "No warnings."))).children(rows))
        .into_any_element()
}

/// A hint line; commands get a copy button.
fn copyable(theme: &Theme, i: usize, text: String) -> impl IntoElement {
    let looks_like_command = ["gh ", "ssh ", "ssh-", "sudo ", "cd ", "Start-Service", "systemctl", "launchctl"].iter().any(|p| text.starts_with(p));
    let copy = text.clone();
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .when(looks_like_command, |d| d.font_family(mono_font()).px(px(6.)).py(px(2.)).rounded(px(4.)).bg(theme.element))
                .child(text),
        )
        .when(looks_like_command, |d| {
            d.child(Button::icon_only(("copy-hint", i), "copy").tooltip("Copy").on_click(move |_, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))
            }))
        })
}

fn metric_card(theme: &Theme, title: &'static str, value: Option<String>, sub: Option<String>, fraction: Option<f32>, color: gpui::Hsla) -> impl IntoElement {
    card(theme)
        .flex_1()
        .min_w(px(0.))
        .px(px(14.))
        .py(px(12.))
        .gap(px(6.))
        .child(div().text_size(px(12.)).text_color(theme.text_muted).child(title))
        .child(div().font_family(mono_font()).text_size(px(20.)).child(value.unwrap_or_else(|| "—".into())))
        .children(fraction.map(|f| meter(theme, f, color)))
        .children(sub.map(|s| div().truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(s)))
}

impl Root {
    /// Why the machine can't be reached, calmly: what probably happened, when
    /// it was last reached, a way to check again, and what ssh said (folded).
    fn unreachable_card(&self, theme: &Theme, m: &Machine, u: &Unreachable, cx: &mut Context<Self>) -> AnyElement {
        let just_offline = u.kind == "offline";
        let (glyph, color) = match u.kind.as_str() {
            "offline" => ("power", theme.text_muted),
            "refused" => ("unplug", theme.warning),
            "tailscale-off" => ("network", theme.warning),
            "auth" | "host-key" => ("lock", theme.error),
            _ => ("circle-alert", theme.error),
        };
        let open = self.offline_details.contains(&m.id);
        // "Last reached 12m ago · checked just now"
        let when = [
            u.last_seen_at.map(|t| format!("Last reached {}", format::ago(Some(t)))),
            m.probe.as_ref().map(|p| format!("checked {}", format::ago(Some(p.checked_at)))),
        ];
        let mut when: Vec<String> = when.into_iter().flatten().collect();
        if let Some(first) = when.first_mut() {
            first[..1].make_ascii_uppercase();
        }
        let when = when.join(" · ");
        let mid = m.id.clone();
        card(theme)
            .p(px(16.))
            .flex_row()
            .items_start()
            .gap(px(14.))
            .when(!just_offline, |d| d.border_color(color.opacity(0.45)))
            .child(
                div()
                    .size(px(34.))
                    .flex_none()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(color.opacity(0.12))
                    .child(icon(glyph, 16., color)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(6.))
                    .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(u.title.clone()))
                    .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(u.detail.clone()))
                    .children(u.hint.clone().map(|h| copyable(theme, 1000, h)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .pt(px(2.))
                            .text_size(px(11.5))
                            .text_color(theme.text_placeholder)
                            .child(when)
                            .child("·")
                            .child(
                                div()
                                    .id("m-offline-details")
                                    .flex()
                                    .items_center()
                                    .gap(px(3.))
                                    .cursor_pointer()
                                    .hover(|st| st.text_color(theme.text_muted))
                                    .child(icon(if open { "chevron-down" } else { "chevron-right" }, 11., theme.text_placeholder))
                                    .child("What ssh said")
                                    .on_click(cx.listener({
                                        let mid = mid.clone();
                                        move |this, _, _, cx| {
                                            if !this.offline_details.remove(&mid) {
                                                this.offline_details.insert(mid.clone());
                                            }
                                            cx.notify();
                                        }
                                    })),
                            ),
                    )
                    .when(open, |d| {
                        d.child(
                            div()
                                .p(px(8.))
                                .rounded(px(6.))
                                .bg(theme.element)
                                .font_family(mono_font())
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .child(u.error.clone()),
                        )
                    }),
            )
            .when(u.kind == "auth", |d| {
                let mid = m.id.clone();
                d.child(
                    Button::new("m-offline-access", "Set up access")
                        .style(ButtonStyle::Primary)
                        .on_click(cx.listener(move |this, _, window, cx| this.setup_ssh_access(&mid, window, cx))),
                )
            })
            .when(u.kind == "tailscale-off" && super::can_open_tailscale(), |d| {
                d.child(Button::new("m-offline-tailscale", "Open Tailscale").style(ButtonStyle::Primary).on_click(|_, _, _| super::open_tailscale()))
            })
            .child(
                Button::new("m-offline-check", "Check again")
                    .icon("refresh-cw")
                    .loading(m.checking)
                    .on_click(cx.listener(move |this, _, _, _| this.engine.trigger_check(Some(mid.clone())))),
            )
            .into_any_element()
    }
}

fn gone(theme: &Theme, text: &'static str) -> impl IntoElement {
    div().size_full().flex().items_center().justify_center().text_color(theme.text_placeholder).child(text)
}

fn status_dot_text(_theme: &Theme, text: &str, color: gpui::Hsla) -> impl IntoElement {
    div().flex().items_center().gap(px(6.)).child(kit::dot(color, 7.)).child(text.to_string())
}

fn status_label(m: &Machine) -> &'static str {
    match m.status.as_str() {
        "online" => "Online",
        "offline" => format::unreachable_word(m),
        "manual" => "No host",
        _ => "Not checked",
    }
}

fn state_label(r: &Runner) -> &'static str {
    match r.state.as_str() {
        "working" => "Working",
        "idle" => "Idle",
        "offline" => "Offline",
        _ => "Unknown",
    }
}
