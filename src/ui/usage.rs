//! Usage: what your agents used and what it'd cost at the API's prices,
//! over a day, a week or a month: by day, model, machine and folder. Read
//! from each machine's Claude Code and Codex logs (see fleet/usage.rs).

use std::collections::HashMap;
use std::time::Instant;

use chrono::{Duration as Ago, Local, NaiveDate};
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px, relative};

use super::github::Load;
use super::kit::{self, Button, card, section_title, spinner};
use super::overview::page;
use super::root::Root;
use super::theme::{ActiveTheme, mono_font};
use crate::fleet::usage::{self, MachineUsage, Tokens};

/// Each machine's usage, or why it couldn't say.
pub type UsageData = Vec<(String, Result<MachineUsage, String>)>;

/// The periods offered: label and days.
const PERIODS: &[(&str, u32)] = &[("Today", 1), ("7 days", 7), ("30 days", 30), ("90 days", 90)];

/// "1.2M", "34K".
fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.0}K", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// "$12.34", "$0.42", "$1,234".
fn dollars(x: f64) -> String {
    if x >= 1000. {
        let whole = x.round() as u64;
        let s = whole.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        format!("${out}")
    } else {
        format!("${x:.2}")
    }
}

impl Root {
    /// Reads every machine's usage for the period chosen.
    pub(super) fn load_usage(&mut self, cx: &mut Context<Self>) {
        self.usage_data = Some(Load::Loading);
        self.usage_at = Some(Instant::now());
        let (engine, days) = (self.engine.clone(), self.usage_days);
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.usage_all(days).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.map_err(|e| e.to_string());
            let _ = this.update(cx, |this, cx| {
                this.usage_data = Some(match result {
                    Ok(v) => Load::Ready(v),
                    Err(e) => Load::Failed(e),
                });
                cx.notify();
            });
        });
        self.track(task);
    }

    pub(super) fn render_usage(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        // First visit, or stale after five minutes: look again.
        if self.usage_data.is_none() || self.usage_at.is_some_and(|t| t.elapsed().as_secs() > 300) {
            self.load_usage(cx);
        }
        let header = div()
            .flex()
            .flex_wrap()
            .items_end()
            .justify_between()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child("Usage"))
                    .child(div().text_size(px(13.)).text_color(theme.text_muted).child("What your agents used, from Claude Code’s and Codex’s own logs on each machine.")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex().gap(px(2.)).p(px(2.)).rounded(px(7.)).bg(theme.element).children(PERIODS.iter().map(|&(label, days)| {
                        let on = self.usage_days == days;
                        div()
                            .id(SharedString::from(format!("usage-{days}")))
                            .px(px(10.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(if on { theme.text } else { theme.text_placeholder })
                            .when(on, |d| d.bg(theme.element_selected))
                            .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.usage_days != days {
                                    this.usage_days = days;
                                    this.load_usage(cx);
                                }
                            }))
                    })))
                    .child(Button::icon_only("usage-refresh", "refresh-cw").tooltip("Read the logs again").on_click(cx.listener(|this, _, _, cx| this.load_usage(cx)))),
            );
        let mut body = page("usage-page", &self.page_scroll).child(header);
        let data = match &self.usage_data {
            Some(Load::Ready(d)) => d.clone(),
            Some(Load::Failed(e)) => return body.child(card(&theme).p(px(16.)).text_size(px(13.)).text_color(theme.error).child(e.clone())).into_any_element(),
            _ => {
                return body
                    .child(card(&theme).p(px(18.)).flex_row().items_center().gap(px(10.)).text_size(px(13.)).text_color(theme.text_muted).child(spinner("usage-loading", 14., theme.text_muted)).child("Reading each machine’s agent logs…"))
                    .into_any_element();
            }
        };
        // "Today" counts from midnight here.
        let today = Local::now().date_naive();
        let first_day = if self.usage_days == 1 { today } else { today - Ago::days(self.usage_days as i64 - 1) };
        let in_period = |hour: &str| usage::local_day(hour).is_some_and(|d| d >= first_day);
        let mut machines: Vec<MachineUsage> = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        for (id, r) in data {
            match r {
                Ok(mut u) => {
                    u.hours.retain(|h| in_period(&h.hour));
                    machines.push(u);
                }
                Err(e) => failed.push((id, e)),
            }
        }
        let name = |id: &str| self.snapshot.machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.to_string());

        // Totals.
        let models = usage::by_model(&machines);
        let mut all = Tokens::default();
        let mut cost = 0.;
        let mut unpriced = Vec::new();
        for (_, model, t, c) in &models {
            all.add(t);
            match c {
                Some(c) => cost += c,
                None if t.total() > 0 => unpriced.push(model.clone()),
                None => {}
            }
        }
        if all.requests == 0 {
            let none = if machines.iter().all(|m| m.tools.is_empty()) {
                "No Claude Code or Codex logs on your machines yet."
            } else {
                "No agent use in this period."
            };
            return body.child(card(&theme).child(kit::empty_state(&theme, "gauge", none))).into_any_element();
        }
        let top = models
            .first()
            .map(|(tool, _, _, c)| match c {
                Some(c) if cost > 0. => format!("{} · {:.0}% of the cost", usage::tool_name(tool), c / cost * 100.),
                _ => usage::tool_name(tool).to_string(),
            })
            .unwrap_or_default();
        let stat = |label: &str, value: String, sub: String| {
            card(&theme)
                .flex_1()
                .min_w(px(180.))
                .p(px(16.))
                .gap(px(6.))
                .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(label.to_string()))
                // A long value (a model's name) smaller, on one line.
                .child(div().text_size(px(if value.chars().count() > 10 { 17. } else { 24. })).font_family(mono_font()).text_color(theme.text).overflow_hidden().whitespace_nowrap().child(value))
                .child(div().text_size(px(12.)).text_color(theme.text_placeholder).overflow_hidden().whitespace_nowrap().child(sub))
        };
        body = body.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(12.))
                .child(stat("At API prices", dollars(cost), if unpriced.is_empty() { "what the same work costs on the API".into() } else { format!("{} model(s) without a price", unpriced.len()) }))
                .child(stat("Tokens", tokens(all.total()), format!("{} out · {} read from cache", tokens(all.output), tokens(all.cache_read))))
                .child(stat("Requests", all.requests.to_string(), format!("across {} machine{}", machines.iter().filter(|m| m.hours.iter().any(|h| h.tokens.requests > 0)).count(), if machines.len() == 1 { "" } else { "s" })))
                .child(stat("Most used", models.first().map(|m| m.1.clone()).unwrap_or_default(), top)),
        );

        // By day: cost (or tokens when nothing's priced), Claude Code and Codex stacked.
        let mut days: HashMap<NaiveDate, (f64, f64)> = HashMap::new();
        for m in &machines {
            for h in &m.hours {
                let Some(day) = usage::local_day(&h.hour) else { continue };
                let value = usage::cost(&h.model, &h.tokens).unwrap_or(0.);
                let e = days.entry(day).or_default();
                if h.tool == "codex" {
                    e.1 += value;
                } else {
                    e.0 += value;
                }
            }
        }
        let span: Vec<NaiveDate> = (0..=(today - first_day).num_days()).map(|i| first_day + Ago::days(i)).collect();
        let peak = span.iter().map(|d| days.get(d).map(|v| v.0 + v.1).unwrap_or(0.)).fold(0., f64::max).max(0.01);
        let (claude_color, codex_color) = (gpui::rgb(0xe8845b), gpui::rgb(0x5b8def));
        let bars = div().h(px(140.)).flex().items_end().gap(px(if span.len() > 40 { 2. } else { 4. })).children(span.iter().map(|d| {
            let (c, x) = days.get(d).copied().unwrap_or_default();
            let tip = format!("{} · {}{}{}", d.format("%a %b %-d"), dollars(c + x), if c > 0. { format!(" · Claude Code {}", dollars(c)) } else { String::new() }, if x > 0. { format!(" · Codex {}", dollars(x)) } else { String::new() });
            div()
                .id(SharedString::from(format!("usage-day-{d}")))
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .justify_end()
                .tooltip(move |window, cx| kit::Tooltip::view(tip.clone().into(), window, cx))
                .child(div().w_full().h(relative((x / peak) as f32)).bg(codex_color).rounded_t(px(2.)))
                .child(div().w_full().h(relative((c / peak) as f32)).bg(claude_color).when(x == 0., |d| d.rounded_t(px(2.))))
                .when(c + x == 0., |d| d.child(div().w_full().h(px(2.)).rounded(px(1.)).bg(theme.track)))
        }));
        let legend = |color: gpui::Rgba, text: &'static str| div().flex().items_center().gap(px(6.)).child(div().size(px(8.)).rounded(px(2.)).bg(color)).child(text);
        body = body.child(
            card(&theme)
                .p(px(16.))
                .gap(px(10.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(16.))
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .child(div().flex_1().text_color(theme.text).font_weight(FontWeight::MEDIUM).child("Each day, at API prices"))
                        .child(legend(claude_color, "Claude Code"))
                        .child(legend(codex_color, "Codex")),
                )
                .child(bars)
                .child(div().flex().justify_between().text_size(px(11.)).text_color(theme.text_placeholder).child(first_day.format("%b %-d").to_string()).child(today.format("%b %-d").to_string())),
        );

        // By model.
        let row = |cells: Vec<AnyElement>, head: bool| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(16.))
                .h(px(if head { 30. } else { 38. }))
                .border_b_1()
                .border_color(theme.border_variant)
                .when(head, |d| d.bg(theme.surface).text_size(px(11.)).text_color(theme.text_placeholder))
                .when(!head, |d| d.text_size(px(12.5)).text_color(theme.text_muted))
                .children(cells)
        };
        let cell = |text: String, w: Option<f32>, right: bool, color: gpui::Hsla| {
            div()
                .when_some(w, |d, w| d.flex_none().w(px(w)))
                .when(w.is_none(), |d| d.flex_1().min_w(px(0.)))
                .when(right, |d| d.flex().justify_end())
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let head = |cols: &[(&str, Option<f32>, bool)]| row(cols.iter().map(|(t, w, r)| cell(t.to_string(), *w, *r, theme.text_placeholder)).collect(), true);
        let model_rows = models.iter().map(|(tool, model, t, c)| {
            row(
                vec![
                    cell(model.clone(), None, false, theme.text),
                    cell(usage::tool_name(tool).to_string(), Some(110.), false, theme.text_muted),
                    cell(t.requests.to_string(), Some(80.), true, theme.text_muted),
                    cell(tokens(t.output), Some(80.), true, theme.text_muted),
                    cell(tokens(t.cache_read), Some(90.), true, theme.text_muted),
                    cell(tokens(t.total()), Some(80.), true, theme.text_muted),
                    cell(c.map(dollars).unwrap_or_else(|| "no price".into()), Some(90.), true, if c.is_some() { theme.text } else { theme.text_placeholder }),
                ],
                false,
            )
        });
        body = body.child(
            div().flex().flex_col().gap(px(10.)).child(section_title(&theme, "By model", Some(models.len()))).child(
                card(&theme)
                    .child(head(&[("Model", None, false), ("Agent", Some(110.), false), ("Requests", Some(80.), true), ("Output", Some(80.), true), ("Cache reads", Some(90.), true), ("All tokens", Some(80.), true), ("At API prices", Some(90.), true)]))
                    .children(model_rows),
            ),
        );

        // By machine.
        let mut per_machine: Vec<(String, Tokens, f64)> = machines
            .iter()
            .map(|m| {
                let mut t = Tokens::default();
                let mut c = 0.;
                for h in &m.hours {
                    t.add(&h.tokens);
                    c += usage::cost(&h.model, &h.tokens).unwrap_or(0.);
                }
                (m.machine_id.clone(), t, c)
            })
            .filter(|(_, t, _)| t.requests > 0)
            .collect();
        per_machine.sort_by(|a, b| b.2.total_cmp(&a.2));
        let machine_rows = per_machine.iter().map(|(id, t, c)| row(vec![cell(name(id), None, false, theme.text), cell(t.requests.to_string(), Some(80.), true, theme.text_muted), cell(tokens(t.total()), Some(90.), true, theme.text_muted), cell(dollars(*c), Some(90.), true, theme.text)], false));
        // By folder: the most recent first (folders don't split by period: their whole window).
        let mut folders: Vec<(String, usage::UsageFolder, f64)> = machines
            .iter()
            .flat_map(|m| m.folders.iter().map(move |f| (m.machine_id.clone(), f.clone())))
            .filter(|(_, f)| usage::local_day(f.last.get(..13).unwrap_or("")).is_some_and(|d| d >= first_day))
            .map(|(id, f)| {
                let c = f.models.first().and_then(|m| usage::cost(m, &f.tokens)).unwrap_or(0.);
                (id, f, c)
            })
            .collect();
        folders.sort_by(|a, b| b.2.total_cmp(&a.2));
        folders.truncate(12);
        let folder_rows = folders.iter().map(|(id, f, c)| {
            // Its last two folders: "opensource/zennotes".
            let parts: Vec<&str> = f.cwd.trim_end_matches('/').rsplit('/').take(2).collect();
            let short = parts.into_iter().rev().collect::<Vec<_>>().join("/");
            row(
                vec![
                    cell(short, None, false, theme.text),
                    cell(name(id), Some(140.), false, theme.text_muted),
                    cell(usage::tool_name(&f.tool).to_string(), Some(100.), false, theme.text_muted),
                    cell(tokens(f.tokens.total()), Some(80.), true, theme.text_muted),
                    cell(dollars(*c), Some(90.), true, theme.text),
                ],
                false,
            )
        });
        body = body.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(16.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(320.))
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .child(section_title(&theme, "By machine", Some(per_machine.len())))
                        .child(card(&theme).child(head(&[("Machine", None, false), ("Requests", Some(80.), true), ("Tokens", Some(90.), true), ("At API prices", Some(90.), true)])).children(machine_rows)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(420.))
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .child(section_title(&theme, "Busiest folders", Some(folders.len())))
                        .child(card(&theme).child(head(&[("Folder", None, false), ("Machine", Some(140.), false), ("Agent", Some(100.), false), ("Tokens", Some(80.), true), ("At API prices", Some(90.), true)])).children(folder_rows)),
                ),
        );
        let mut notes = vec![format!(
            "At Anthropic’s and OpenAI’s list prices ({}). With a Claude or ChatGPT plan you don’t pay per token: this is what the same work would cost on the API.",
            usage::PRICES_AS_OF
        )];
        if !unpriced.is_empty() {
            notes.push(format!("No price known for {}: counted in tokens only.", unpriced.join(", ")));
        }
        for (id, e) in &failed {
            notes.push(format!("{}: {e}", name(id)));
        }
        body.child(div().flex().flex_col().gap(px(4.)).text_size(px(12.)).text_color(theme.text_placeholder).children(notes)).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{dollars, tokens};

    #[test]
    fn says_amounts_plainly() {
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(34_600), "35K");
        assert_eq!(tokens(1_250_000), "1.2M");
        assert_eq!(tokens(11_800_000_000), "11.8B");
        assert_eq!(dollars(0.4234), "$0.42");
        assert_eq!(dollars(1234.6), "$1,235");
        assert_eq!(dollars(12_345_678.), "$12,345,678");
    }
}
