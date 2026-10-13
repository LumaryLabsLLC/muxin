//! New schedule: a command or an agent session that runs on one of your
//! machines at set times.

use std::sync::Arc;

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use super::dialogs::{DialogEvent, MODAL_CHROME, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle};
use super::theme::ActiveTheme;
use crate::fleet::Engine;
use crate::fleet::schedules::{ScheduleInput, When};
use crate::fleet::sessions::AGENTS;

const DAYS: [(&str, &str); 7] = [("mon", "Mon"), ("tue", "Tue"), ("wed", "Wed"), ("thu", "Thu"), ("fri", "Fri"), ("sat", "Sat"), ("sun", "Sun")];

pub struct ScheduleDialog {
    engine: Arc<Engine>,
    machine: Option<String>,
    name: Entity<TextInput>,
    command: Entity<TextInput>,
    prompt: Entity<TextInput>,
    folder: Entity<TextInput>,
    at: Entity<TextInput>,
    n: Entity<TextInput>,
    /// "command" | "agent"
    kind: &'static str,
    agent: String,
    /// "minutes" | "hours" | "day" | "weekdays" | "week"
    every: &'static str,
    days: Vec<&'static str>,
    saving: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for ScheduleDialog {}

impl ScheduleDialog {
    pub fn new(engine: Arc<Engine>, machine: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, text: &str, cx: &mut Context<Self>| cx.new(|cx| TextInput::new(cx, placeholder.to_string()).with_text(text.to_string()));
        let name = input("Nightly tests", "", cx);
        let command = input("cd ~/code/api && make test", "", cx);
        let prompt = input("Go through new issues in acme/api and label them", "", cx);
        let folder = input("~", "", cx);
        let at = input("09:00", "09:00", cx);
        let n = input("15", "15", cx);
        let mut subs = Vec::new();
        for i in [&name, &command, &prompt, &folder, &at, &n] {
            subs.push(cx.subscribe_in(i, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
                InputEvent::Submit => this.submit(cx),
                InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
                InputEvent::Changed => cx.notify(),
            }));
        }
        // The machine asked for, else the first one online.
        let online: Vec<String> = engine.current().machines.iter().filter(|m| m.status == "online").map(|m| m.id.clone()).collect();
        let machine = machine.filter(|m| online.contains(m)).or_else(|| online.first().cloned());
        ScheduleDialog {
            engine,
            machine,
            name,
            command,
            prompt,
            folder,
            at,
            n,
            kind: "command",
            agent: "claude".into(),
            every: "day",
            days: vec!["mon"],
            saving: false,
            error: None,
            _subscriptions: subs,
        }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    fn when(&self, cx: &App) -> Result<When, String> {
        let at = self.at.read(cx).text().trim().to_string();
        let valid_at = at.split_once(':').is_some_and(|(h, m)| h.parse::<u32>().is_ok_and(|h| h < 24) && m.len() == 2 && m.parse::<u32>().is_ok_and(|m| m < 60));
        let n = self.n.read(cx).text().trim().parse::<u32>().ok();
        Ok(match self.every {
            "minutes" => When { every: "minutes".into(), n: Some(n.filter(|n| (1..60).contains(n)).ok_or("Every 1 to 59 minutes.")?), ..Default::default() },
            "hours" => When { every: "hours".into(), n: Some(1), at: Some("00:00".into()), ..Default::default() },
            every => {
                if !valid_at {
                    return Err("A time like 09:00.".into());
                }
                let days = if every == "week" { self.days.iter().map(|d| d.to_string()).collect() } else { Vec::new() };
                if every == "week" && days.is_empty() {
                    return Err("Pick a day.".into());
                }
                When { every: every.into(), at: Some(format!("{:0>5}", at)), days, ..Default::default() }
            }
        })
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(machine) = self.machine.clone() else {
            self.error = Some("Pick a machine.".into());
            cx.notify();
            return;
        };
        let when = match self.when(cx) {
            Ok(w) => w,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        let text = |e: &Entity<TextInput>, cx: &App| Some(e.read(cx).text().trim().to_string()).filter(|t| !t.is_empty());
        let input = ScheduleInput {
            id: None,
            name: text(&self.name, cx).unwrap_or_default(),
            when,
            command: (self.kind == "command").then(|| text(&self.command, cx)).flatten(),
            agent: (self.kind == "agent").then(|| self.agent.clone()),
            prompt: (self.kind == "agent").then(|| text(&self.prompt, cx)).flatten(),
            folder: text(&self.folder, cx),
            enabled: true,
        };
        self.saving = true;
        self.error = None;
        let job = self.engine.runtime().spawn({
            let engine = self.engine.clone();
            async move { engine.schedule_save(&machine, input).await }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(_) => cx.emit(DialogEvent::ScheduleSaved),
                    Err(e) => {
                        this.error = Some(e);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }
}

impl Focusable for ScheduleDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for ScheduleDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let base = |id: SharedString, selected: bool| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(9.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(px(6.))
                .border_1()
                .cursor_pointer()
                .text_size(px(12.5))
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .text_color(if selected { theme.text } else { theme.text_muted })
        };
        let chip = |id: SharedString, text: String, selected: bool| base(id, selected).child(text);
        let s = self.engine.current();
        let machines: Vec<AnyElement> = s
            .machines
            .iter()
            .filter(|m| m.status == "online")
            .map(|m| {
                let id = m.id.clone();
                let (from, to) = super::format::machine_tile(m);
                base(SharedString::from(format!("sd-m-{id}")), self.machine.as_deref() == Some(id.as_str()))
                    .pl(px(4.))
                    .child(kit::tile(super::format::machine_icon(m), from, to, 18.))
                    .child(m.name.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.machine = Some(id.clone());
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        // The agents installed there (all of them until it's been looked at).
        let installed: Vec<String> = s.machines.iter().find(|m| Some(&m.id) == self.machine.as_ref()).map(|m| m.agents.clone()).unwrap_or_default();
        let agents: Vec<AnyElement> = AGENTS
            .iter()
            .filter(|a| a.id != "shell" && (installed.is_empty() || installed.iter().any(|i| i == a.id)))
            .map(|a| {
                let id = a.id.to_string();
                let (glyph, (from, to)) = kit::agent_look(a.id);
                base(SharedString::from(format!("sd-a-{id}")), self.agent == id)
                    .pl(px(4.))
                    .child(kit::tile(glyph, from, to, 18.))
                    .child(a.name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agent = id.clone();
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        let kinds = div()
            .flex()
            .gap(px(6.))
            .child(chip("sd-k-command".into(), "A command".into(), self.kind == "command").on_click(cx.listener(|this, _, _, cx| {
                this.kind = "command";
                cx.notify()
            })))
            .child(chip("sd-k-agent".into(), "An agent session".into(), self.kind == "agent").on_click(cx.listener(|this, _, _, cx| {
                this.kind = "agent";
                cx.notify()
            })));
        let what: AnyElement = if self.kind == "command" {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(self.command.clone())
                .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child("Runs in your login shell there. For an agent that works on its own and finishes, try claude -p \"…\"."))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(div().flex().flex_wrap().gap(px(6.)).children(agents))
                .child(self.prompt.clone())
                .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child("Starts a session with this prompt, like New session does. It shows up under the machine, waiting for you when it's done."))
                .into_any_element()
        };
        let everies: [(&'static str, &str); 5] = [("minutes", "Every few minutes"), ("hours", "Every hour"), ("day", "Every day"), ("weekdays", "Weekdays"), ("week", "Some days")];
        let every_chips: Vec<AnyElement> = everies
            .iter()
            .map(|(id, label)| {
                let id: &'static str = id;
                chip(SharedString::from(format!("sd-e-{id}")), label.to_string(), self.every == id)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.every = id;
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        let detail: AnyElement = match self.every {
            "minutes" => div().flex().items_center().gap(px(8.)).text_size(px(12.5)).text_color(theme.text_muted).child("Every").child(div().w(px(56.)).child(self.n.clone())).child("minutes").into_any_element(),
            "hours" => div().text_size(px(12.)).text_color(theme.text_placeholder).child("On the hour.").into_any_element(),
            every => {
                let days: Vec<AnyElement> = DAYS
                    .iter()
                    .map(|(id, label)| {
                        let id: &'static str = id;
                        chip(SharedString::from(format!("sd-d-{id}")), label.to_string(), self.days.contains(&id))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.days.contains(&id) {
                                    this.days.push(id);
                                } else {
                                    this.days.retain(|d| *d != id);
                                }
                                cx.notify()
                            }))
                            .into_any_element()
                    })
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .when(every == "week", |d| d.child(div().flex().flex_wrap().gap(px(6.)).children(days)))
                    .child(div().flex().items_center().gap(px(8.)).text_size(px(12.5)).text_color(theme.text_muted).child("At").child(div().w(px(72.)).child(self.at.clone())).child("in the machine's own time"))
                    .into_any_element()
            }
        };
        let when = div().flex().flex_col().gap(px(8.)).child(div().flex().flex_wrap().gap(px(6.)).children(every_chips)).child(detail);
        let body_max = (f32::from(window.viewport_size().height) - MODAL_CHROME).max(220.);
        let fields = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(field(&theme, "Name", self.name.clone(), None))
            .child(field(&theme, "On", div().flex().flex_wrap().gap(px(6.)).children(machines), None))
            .child(field(&theme, "Runs", div().flex().flex_col().gap(px(8.)).child(kinds).child(what), None))
            .child(field(&theme, "Folder", self.folder.clone(), None))
            .child(field(&theme, "When", when, None));
        frame(&theme, "New schedule", Some("It's kept and run on the machine itself, so it runs even when Muxen isn't open.".into()))
            .child(div().id("sd-body").max_h(px(body_max)).overflow_y_scroll().child(fields))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                Button::new("sd-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                Button::new("sd-save", "Create").style(ButtonStyle::Primary).icon("clock").loading(self.saving).disabled(self.machine.is_none()).on_click(cx.listener(|this, _, _, cx| this.submit(cx))).into_any_element(),
            ]))
            .font_weight(FontWeight::NORMAL)
    }
}
