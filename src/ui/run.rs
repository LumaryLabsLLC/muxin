//! Run: one command on several machines at once, each one's output side by
//! side as it comes (see fleet/command.rs). Each machine's run is an
//! operation, so the Activity page has the whole output too.

use std::collections::HashSet;

use gpui::{AnyElement, Context, Focusable, FontWeight, SharedString, Window, div, prelude::*, px};

use super::format;
use super::kit::{self, Button, ButtonStyle, card, section_title, spinner};
use super::overview::page;
use super::root::Root;
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::model::{Machine, Operation};

/// Output lines each machine's card shows, and the one made bigger.
const LINES: usize = 14;
const MORE_LINES: usize = 2000;
/// Commands remembered for the Recent row.
const HISTORY: usize = 8;

/// The last command run: on which machines, and each one's operation (or why it didn't start).
pub struct RunBatch {
    pub command: String,
    pub runs: Vec<(String, Result<u64, String>)>,
}

impl Root {
    /// The machines a command can run on now.
    fn runnable(&self) -> Vec<Machine> {
        self.snapshot.machines.iter().filter(|m| !m.is_down() && m.probe.is_some()).cloned().collect()
    }

    /// The machines picked (all that can run, until the user picks).
    fn run_targets(&self) -> Vec<String> {
        let ok = self.runnable();
        match &self.run_picked {
            Some(p) => ok.iter().filter(|m| p.contains(&m.id)).map(|m| m.id.clone()).collect(),
            None => ok.iter().map(|m| m.id.clone()).collect(),
        }
    }

    /// Runs what's typed on the machines picked.
    pub(super) fn start_run(&mut self, cx: &mut Context<Self>) {
        let command = self.run_input.read(cx).text().trim().to_string();
        if command.is_empty() {
            return;
        }
        let targets = self.run_targets();
        if targets.is_empty() {
            self.notify_user("Pick a machine to run it on.".to_string(), true, cx);
            return;
        }
        let runs = self.engine.run_on_machines(&targets, &command, crate::fleet::command::DEFAULT_TIMEOUT);
        self.run_history.retain(|c| c != &command);
        self.run_history.insert(0, command.clone());
        self.run_history.truncate(HISTORY);
        // The one shown bigger stays so when it runs there again.
        if self.run_expanded.as_ref().is_some_and(|id| !targets.contains(id)) {
            self.run_expanded = None;
        }
        self.run_last = Some(RunBatch { command, runs });
        cx.notify();
    }

    fn toggle_run_machine(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut picked: HashSet<String> = self.run_picked.clone().unwrap_or_else(|| self.runnable().into_iter().map(|m| m.id).collect());
        if !picked.remove(id) {
            picked.insert(id.to_string());
        }
        self.run_picked = Some(picked);
        cx.notify();
    }

    pub(super) fn render_run(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let header = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child("Run"))
            .child(div().text_size(px(13.)).text_color(theme.text_muted).child("One command on several machines at once, in each one’s login shell from the home folder."));

        let targets: HashSet<String> = self.run_targets().into_iter().collect();
        let running = self.run_last.as_ref().is_some_and(|b| b.runs.iter().any(|(_, r)| r.as_ref().is_ok_and(|op| self.op(*op).is_some_and(|o| o.status == "running"))));
        let chips = self.snapshot.machines.iter().map(|m| {
            let can = !m.is_down() && m.probe.is_some();
            let on = can && targets.contains(&m.id);
            let (from, to) = format::machine_tile(m);
            let id = m.id.clone();
            div()
                .id(SharedString::from(format!("run-on-{}", m.id)))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(28.))
                .pl(px(4.))
                .pr(px(10.))
                .rounded(px(7.))
                .border_1()
                .border_color(if on { theme.accent } else { theme.border_variant })
                .bg(if on { theme.accent.opacity(0.10) } else { theme.element })
                .text_size(px(12.5))
                .when(!can, |d| d.opacity(0.45))
                .when(can, |d| d.cursor_pointer().hover(|st| st.bg(theme.element_hover)).on_click(cx.listener(move |this, _, _, cx| this.toggle_run_machine(&id, cx))))
                .when(!can, |d| {
                    let why: SharedString = if m.is_disconnected() { "Disconnected".into() } else { "Offline".into() };
                    d.tooltip(move |window, cx| kit::Tooltip::view(why.clone(), window, cx))
                })
                .child(kit::tile(format::machine_icon(m), from, to, 20.))
                .child(m.name.clone())
                .when(on, |d| d.child(kit::icon("check", 12., theme.accent)))
        });
        let all_on = targets.len() == self.runnable().len();
        let picker = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .child(div().text_size(px(12.)).text_color(theme.text_muted).mr(px(4.)).child("On"))
            .children(chips)
            .child(
                div()
                    .id("run-all")
                    .ml(px(4.))
                    .text_size(px(12.))
                    .text_color(theme.text_accent)
                    .cursor_pointer()
                    .child(if all_on { "None" } else { "All" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.run_picked = if all_on { Some(HashSet::new()) } else { None };
                        cx.notify();
                    })),
            );
        let enabled = !targets.is_empty();
        let form = card(&theme)
            .p(px(14.))
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().min_w(px(0.)).flex().child(self.run_input.clone()))
                    .child(
                        Button::new("run-go", if targets.len() > 1 { format!("Run on {}", targets.len()) } else { "Run".to_string() })
                            .icon("play")
                            .style(ButtonStyle::Primary)
                            .disabled(!enabled)
                            .loading(running)
                            .on_click(cx.listener(|this, _, _, cx| this.start_run(cx))),
                    ),
            )
            .child(picker);

        let mut body = page("run-page", &self.page_scroll).child(header).child(form);
        if !self.run_history.is_empty() {
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(div().text_size(px(12.)).text_color(theme.text_muted).mr(px(4.)).child("Recent"))
                    .children(self.run_history.iter().enumerate().map(|(i, c)| {
                        let c2 = c.clone();
                        div()
                            .id(("run-recent", i))
                            .max_w(px(320.))
                            .px(px(8.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .bg(theme.element)
                            .cursor_pointer()
                            .hover(|st| st.bg(theme.element_hover))
                            .font_family(mono_font())
                            .text_size(px(11.5))
                            .child(div().w_full().truncate().child(c.clone()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let text = c2.clone();
                                this.run_input.update(cx, |i, cx| i.set_text(text, cx));
                                window.focus(&this.run_input.read(cx).focus_handle(cx));
                            }))
                    }))
                    .child(Button::icon_only("run-forget", "x").tooltip("Forget these").on_click(cx.listener(|this, _, _, cx| {
                        this.run_history.clear();
                        cx.notify();
                    }))),
            );
        }
        if let Some(batch) = &self.run_last {
            let done = batch.runs.iter().filter(|(_, r)| r.as_ref().is_ok_and(|op| self.op(*op).is_some_and(|o| o.status != "running"))).count();
            let failed = batch.runs.iter().filter(|(_, r)| r.is_err() || r.as_ref().is_ok_and(|op| self.op(*op).is_some_and(|o| o.status == "failed"))).count();
            let summary = match (done + batch.runs.iter().filter(|(_, r)| r.is_err()).count(), failed) {
                (d, 0) if d == batch.runs.len() => "All done".to_string(),
                (d, f) if d == batch.runs.len() => format!("Done · {f} failed"),
                (d, _) => format!("{d} of {} done", batch.runs.len()),
            };
            // The one made bigger first, across the page.
            let mut runs: Vec<&(String, Result<u64, String>)> = batch.runs.iter().collect();
            runs.sort_by_key(|(id, _)| self.run_expanded.as_ref() != Some(id));
            let cards: Vec<AnyElement> = runs.into_iter().map(|(machine_id, r)| self.run_card(&theme, machine_id, r, cx)).collect();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().flex_1().min_w(px(0.)).truncate().font_family(mono_font()).text_size(px(13.)).child(format!("$ {}", batch.command)))
                            .child(div().flex_none().text_size(px(12.)).text_color(theme.text_muted).child(summary))
                            .child(Button::new("run-clear", "Clear").icon("x").style(ButtonStyle::Ghost).tooltip("Clear the results").on_click(cx.listener(|this, _, _, cx| {
                                this.run_last = None;
                                this.run_expanded = None;
                                cx.notify();
                            }))),
                    )
                    .child(div().flex().flex_wrap().gap(px(10.)).children(cards)),
            );
        } else if self.runnable().is_empty() {
            body = body.child(section_title(&theme, "No machines online", None)).child(card(&theme).child(kit::empty_state(&theme, "zap", "Commands run on machines that are online.")));
        }
        body.into_any_element()
    }

    fn op(&self, id: u64) -> Option<&Operation> {
        self.snapshot.operations.iter().find(|o| o.id == id)
    }

    fn run_card(&self, theme: &Theme, machine_id: &str, r: &Result<u64, String>, cx: &mut Context<Self>) -> AnyElement {
        let m = self.snapshot.machines.iter().find(|m| m.id == machine_id);
        let name = m.map(|m| m.name.clone()).unwrap_or_else(|| machine_id.to_string());
        let (from, to) = m.map(format::machine_tile).unwrap_or(kit::tiles::OTHER);
        let icon = m.map(format::machine_icon).unwrap_or("server");
        let op = r.as_ref().ok().and_then(|id| self.op(*id));
        let (state, color): (AnyElement, _) = match (r, op) {
            (Err(e), _) => (div().child(e.clone()).into_any_element(), theme.error),
            (Ok(_), None) => (div().child("Gone").into_any_element(), theme.text_muted),
            (Ok(id), Some(o)) if o.status == "running" => (
                div().flex().items_center().gap(px(5.)).child(spinner(("run-spin", *id as usize), 11., theme.text_muted)).child(format::elapsed(o.started_at)).into_any_element(),
                theme.text_muted,
            ),
            (Ok(_), Some(o)) => {
                let took = o.finished_at.map(|f| (f - o.started_at).num_milliseconds() as f64 / 1000.).unwrap_or(0.);
                let took = if took < 10. { format!("{took:.1}s") } else { format!("{took:.0}s") };
                if o.status == "succeeded" {
                    (div().flex().items_center().gap(px(4.)).child(kit::icon("check", 12., theme.success)).child(took).into_any_element(), theme.success)
                } else {
                    let why = o.error.clone().unwrap_or_else(|| "Failed".into());
                    let why = why.strip_prefix("Exited with status ").map(|c| format!("Exit {}", c.trim_end_matches('.'))).unwrap_or(why);
                    (div().flex().items_center().gap(px(4.)).child(kit::icon("x", 12., theme.error)).child(why).into_any_element(), theme.error)
                }
            }
        };
        let lines: Vec<String> = op.map(|o| o.log.iter().filter(|l| !l.starts_with("error: ")).cloned().collect()).unwrap_or_default();
        let big = self.run_expanded.as_deref() == Some(machine_id);
        let hidden = lines.len().saturating_sub(if big { MORE_LINES } else { LINES });
        let shown = &lines[hidden..];
        let op_id = r.as_ref().ok().copied();
        let toggle = {
            let id = machine_id.to_string();
            cx.listener(move |this: &mut Root, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Root>| {
                this.run_expanded = if this.run_expanded.as_ref() == Some(&id) { None } else { Some(id.clone()) };
                cx.notify();
            })
        };
        let more = {
            let id = machine_id.to_string();
            cx.listener(move |this: &mut Root, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Root>| {
                this.run_expanded = Some(id.clone());
                cx.notify();
            })
        };
        card(theme)
            .when(big, |d| d.flex_none().w_full())
            .when(!big, |d| d.flex_1().min_w(px(300.)))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(9.))
                    .border_b_1()
                    .border_color(theme.border_variant)
                    .child(kit::tile(icon, from, to, 20.))
                    .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child(name))
                    .child(div().flex_none().text_size(px(12.)).text_color(color).child(state))
                    .child(
                        Button::icon_only(SharedString::from(format!("run-big-{machine_id}")), if big { "minimize" } else { "maximize" })
                            .tooltip(if big { "Smaller" } else { "Bigger: all of its output" })
                            .on_click(toggle),
                    )
                    .when_some(op_id, |d, id| {
                        d.child(Button::icon_only(SharedString::from(format!("run-full-{id}")), "scroll-text").tooltip("All of its output").on_click(cx.listener(move |this, _, _, cx| this.show_operation(id, cx))))
                    }),
            )
            .child(
                div()
                    .id(SharedString::from(format!("run-out-{machine_id}")))
                    .when(big, |d| d.max_h(px(560.)).overflow_scroll())
                    .when(!big, |d| d.overflow_x_scroll())
                    .px(px(12.))
                    .py(px(9.))
                    .min_h(px(60.))
                    .flex()
                    .flex_col()
                    .font_family(mono_font())
                    .text_size(px(11.5))
                    .when(hidden > 0 && !big, |d| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("run-more-{machine_id}")))
                                .text_color(theme.text_accent)
                                .cursor_pointer()
                                .child(format!("… {hidden} more lines"))
                                .on_click(more),
                        )
                    })
                    .when(hidden > 0 && big, |d| d.child(div().text_color(theme.text_placeholder).child(format!("… {hidden} earlier lines: all of it is in Activity"))))
                    .when(shown.is_empty() && op.is_some_and(|o| o.status != "running"), |d| d.child(div().text_color(theme.text_placeholder).child("No output")))
                    .children(shown.iter().map(|l| div().whitespace_nowrap().text_color(theme.text).child(l.clone()))),
            )
            .into_any_element()
    }
}
