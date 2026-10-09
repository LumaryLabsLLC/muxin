//! New cloud machine: a DigitalOcean droplet, set up and added to the fleet.

use std::sync::Arc;

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use super::dialogs::{DialogEvent, MODAL_CHROME, error_line, field, footer, frame};
use super::github::Load;
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle};
use super::theme::{ActiveTheme, mono_font};
use crate::cloud::{CloudCreate, CloudOptions};
use crate::fleet::Engine;

pub struct CloudDialog {
    engine: Arc<Engine>,
    options: Load<CloudOptions>,
    name: Entity<TextInput>,
    region: String,
    size: String,
    /// What it comes with, by id (cloud::setup::EXTRAS).
    extras: Vec<String>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for CloudDialog {}

impl CloudDialog {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // A name not taken yet: dev-1, dev-2, ...
        let taken: Vec<String> = engine.current().machines.iter().map(|m| m.name.to_lowercase()).collect();
        let first = (1..).map(|n| format!("dev-{n}")).find(|n| !taken.contains(n)).unwrap_or_else(|| "dev".into());
        let name = cx.new(|cx| TextInput::new(cx, "dev-1").with_text(first));
        let subs = vec![cx.subscribe_in(&name, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => cx.notify(),
        })];
        let settings = engine.settings().cloud;
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.cloud_options().await }
        });
        cx.spawn(async move |this, cx| {
            if let Ok(options) = job.await {
                let _ = this.update(cx, |this, cx| {
                    if this.region.is_empty() {
                        this.region = options.default_region.clone();
                    }
                    this.options = Load::Ready(options);
                    cx.notify();
                });
            }
        })
        .detach();
        CloudDialog {
            engine,
            options: Load::Loading,
            name,
            region: settings.region,
            size: settings.size,
            extras: settings.install,
            error: None,
            _subscriptions: subs,
        }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if !matches!(&self.options, Load::Ready(o) if o.account.is_some()) {
            return;
        }
        let req = CloudCreate {
            name: self.name.read(cx).text().trim().to_string(),
            region: Some(self.region.clone()),
            size: Some(self.size.clone()),
            install: Some(self.extras.clone()),
        };
        match self.engine.cloud_create(req) {
            Ok(op) => cx.emit(DialogEvent::OperationStarted(op)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }
}

impl Focusable for CloudDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for CloudDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let cancel = Button::new("cloud-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element();
        let options = match &self.options {
            Load::Ready(o) => o.clone(),
            Load::Loading => {
                return frame(&theme, "New cloud machine", None)
                    .child(div().flex().items_center().gap(px(8.)).text_size(px(12.5)).text_color(theme.text_muted).child(kit::spinner("cloud-load", 13., theme.text_muted)).child("Asking DigitalOcean…"))
                    .child(footer(vec![cancel]));
            }
            Load::Failed(e) => {
                return frame(&theme, "New cloud machine", None).children(error_line(&theme, &Some(e.clone()))).child(footer(vec![cancel]));
            }
        };
        // Not connected yet: that comes first.
        if options.account.is_none() {
            let why = options.error.clone().unwrap_or_else(|| "Connect your DigitalOcean account first.".into());
            return frame(&theme, "New cloud machine", None)
                .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(why))
                .child(footer(vec![
                    cancel,
                    Button::new("cloud-settings", "Open Settings")
                        .style(ButtonStyle::Primary)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::OpenCloudSettings)))
                        .into_any_element(),
                ]));
        }

        let chip = |id: SharedString, text: String, selected: bool| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(9.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .border_1()
                .cursor_pointer()
                .text_size(px(12.5))
                .border_color(if selected { theme.accent } else { theme.border_variant })
                .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                .text_color(if selected { theme.text } else { theme.text_muted })
                .child(text)
        };
        let chosen_city = crate::cloud::city_of_slug(&options.regions, &self.region);
        let regions: Vec<AnyElement> = crate::cloud::cities(&options.regions)
            .into_iter()
            .map(|(city, slug)| {
                let selected = chosen_city.as_deref() == Some(city.as_str());
                chip(SharedString::from(format!("cr-{slug}")), city, selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.region = slug.clone();
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        // Basic sizes available in the region, cheapest first.
        let sizes: Vec<&crate::cloud::Size> = options.sizes.iter().filter(|s| super::settings::regular_size(&s.slug) && s.regions.contains(&self.region)).take(8).collect();
        let picked = options.sizes.iter().find(|s| s.slug == self.size && s.regions.contains(&self.region)).cloned();
        let size_rows: Vec<AnyElement> = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let selected = s.slug == self.size;
                let slug = s.slug.clone();
                div()
                    .id(("cs", i))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .h(px(30.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border_variant })
                    .bg(if selected { theme.accent.opacity(0.08) } else { theme.element })
                    .text_size(px(12.5))
                    .child(div().flex_1().child(format!("{} vCPU · {} GB · {} GB disk", s.vcpus, s.memory_mb / 1024, s.disk_gb)))
                    .child(div().font_family(mono_font()).text_size(px(12.)).text_color(theme.text_muted).child(format!("${:.0}/mo", s.price_monthly)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.size = slug.clone();
                        cx.notify()
                    }))
                    .into_any_element()
            })
            .collect();
        // The extras in their groups, one checkbox each, three to a row.
        let this = cx.entity().downgrade();
        let groups: Vec<AnyElement> = crate::cloud::setup::GROUPS
            .iter()
            .map(|(group, ids)| {
                let boxes: Vec<AnyElement> = ids
                    .iter()
                    .map(|id| {
                        let on = self.extras.iter().any(|x| x == id);
                        let (id, this) = (id.to_string(), this.clone());
                        div()
                            .w(px(145.))
                            .child(kit::checkbox(&theme, SharedString::from(format!("cd-x-{id}")), on, crate::cloud::setup::extra_name(&id), move |v, _, cx: &mut App| {
                                let (v, id) = (*v, id.clone());
                                let _ = this.update(cx, |d, cx| {
                                    d.extras.retain(|x| *x != id);
                                    if v {
                                        d.extras.push(id);
                                    }
                                    cx.notify()
                                });
                            }))
                            .into_any_element()
                    })
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(7.))
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(*group))
                    .child(kit::rows_of(boxes, 3, 9.))
                    .into_any_element()
            })
            .collect();
        let extras = div().flex().flex_col().gap(px(12.)).children(groups);
        let price = picked.as_ref().map(|s| {
            div()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(format!("${:.3} an hour (${:.0} a month), billed until you destroy it.", s.price_hourly, s.price_monthly))
        });
        let create_label = match &picked {
            Some(s) => format!("Create · ${:.0}/mo", s.price_monthly),
            None => "Create".into(),
        };
        let body_max = (f32::from(window.viewport_size().height) - MODAL_CHROME).max(220.);
        let fields = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(field(&theme, "Name", self.name.clone(), None))
            .child(field(&theme, "Region", kit::rows_of(regions, 4, 6.), None))
            .child(field(&theme, "Size", div().flex().flex_col().gap(px(4.)).children(size_rows), None))
            .child(field(&theme, "Comes with", extras, None))
            .children(price);
        frame(&theme, "New cloud machine", Some("A DigitalOcean droplet, set up for you and added to your fleet.".into()))
            .child(div().id("cloud-body").max_h(px(body_max)).overflow_y_scroll().child(fields))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                cancel,
                Button::new("cloud-create", create_label)
                    .style(ButtonStyle::Primary)
                    .disabled(picked.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                    .into_any_element(),
            ]))
            .font_weight(FontWeight::NORMAL)
    }
}
