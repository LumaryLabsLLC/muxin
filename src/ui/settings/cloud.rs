//! Settings › Cloud: how Muxen signs in to DigitalOcean, what new cloud
//! machines come with, and what happens to ones nobody is using.

use gpui::{AnyElement, App, Context, Entity, SharedString, Window, div, prelude::*, px};

use super::SettingsPage;
use crate::cloud::{CloudOptions, TAILSCALE_KEY, secrets};
use crate::config::CloudSettings;
use crate::ui::github::Load;
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::kit::{self, Button, ButtonStyle, card, section_title};
use crate::ui::theme::{Theme, mono_font};

pub(super) struct CloudState {
    options: Option<Load<CloudOptions>>,
    token: Entity<TextInput>,
    tailscale_key: Entity<TextInput>,
    busy: bool,
    /// What the last action did: (is an error, text).
    message: Option<(bool, String)>,
}

impl CloudState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SettingsPage>) -> (CloudState, Vec<gpui::Subscription>) {
        let token = cx.new(|cx| TextInput::new(cx, "dop_v1_…").mono().secret());
        let tailscale_key = cx.new(|cx| TextInput::new(cx, "tskey-auth-… (optional)").mono().secret());
        let subs = vec![
            cx.subscribe_in(&token, window, |this: &mut SettingsPage, _, e: &InputEvent, _, cx| {
                if *e == InputEvent::Submit {
                    this.cloud_connect("token", cx);
                }
            }),
            cx.subscribe_in(&tailscale_key, window, |this: &mut SettingsPage, _, e: &InputEvent, _, cx| {
                if *e == InputEvent::Submit {
                    this.save_tailscale_key(cx);
                }
            }),
        ];
        (CloudState { options: None, token, tailscale_key, busy: false, message: None }, subs)
    }
}

impl SettingsPage {
    /// Reads the account, regions and sizes (when the Cloud tab opens).
    pub(super) fn load_cloud(&mut self, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        if self.cloud.options.is_none() {
            self.cloud.options = Some(Load::Loading);
        }
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.cloud_options().await }
        });
        let task = cx.spawn(async move |this, cx| {
            if let Ok(options) = job.await {
                let _ = this.update(cx, |this, cx| {
                    this.cloud.options = Some(Load::Ready(options));
                    cx.notify()
                });
            }
        });
        self._tasks.push(task);
    }

    fn cloud_connect(&mut self, method: &'static str, cx: &mut Context<Self>) {
        if self.cloud.busy {
            return;
        }
        let token = (method == "token").then(|| self.cloud.token.read(cx).text().trim().to_string());
        if token.as_deref() == Some("") {
            return;
        }
        self.cloud.busy = true;
        self.cloud.message = None;
        cx.notify();
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.cloud_sign_in(method, token, |url| crate::util::open_url(url)).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.cloud.busy = false;
                // Connected shows itself; only a failure needs saying.
                this.cloud.message = result.err().map(|e| (true, e));
                this.cloud.token.update(cx, |i, cx| i.set_text("", cx));
                this.load_cloud(cx);
                cx.notify();
            });
        });
        self._tasks.push(task);
    }

    fn cloud_sign_out(&mut self, cx: &mut Context<Self>) {
        self.engine.cloud_sign_out();
        self.cloud.message = None;
        self.load_cloud(cx);
        cx.notify();
    }

    fn save_cloud(&mut self, f: impl FnOnce(&mut CloudSettings), cx: &mut Context<Self>) {
        let mut s = self.engine.settings();
        f(&mut s.cloud);
        if let Err(e) = self.engine.update_settings(s) {
            self.cloud.message = Some((true, e));
        }
        cx.notify();
    }

    fn save_tailscale_key(&mut self, cx: &mut Context<Self>) {
        let key = self.cloud.tailscale_key.read(cx).text().trim().to_string();
        let dir = self.engine.paths().dir.clone();
        self.cloud.message = Some(match key.is_empty() {
            true => {
                secrets::delete(&dir, TAILSCALE_KEY);
                (false, "Removed the Tailscale key.".into())
            }
            false => match secrets::set(&dir, TAILSCALE_KEY, &key) {
                Ok(()) => (false, "Saved the Tailscale key.".into()),
                Err(e) => (true, e),
            },
        });
        self.cloud.tailscale_key.update(cx, |i, cx| i.set_text("", cx));
        cx.notify();
    }

    pub(super) fn cloud_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.engine.settings().cloud;
        let method = settings.digitalocean_auth.clone();
        let options = match &self.cloud.options {
            Some(Load::Ready(o)) => Some(o.clone()),
            _ => None,
        };
        let loading = matches!(self.cloud.options, Some(Load::Loading) | None);

        // How to sign in.
        let methods = segmented(
            theme,
            "do-auth",
            &[("token", "API token"), ("oauth", "Browser"), ("doctl", "doctl")],
            &method,
            cx,
            |this, value, cx| {
                this.save_cloud(|c| c.digitalocean_auth = value.into(), cx);
                this.cloud.message = None;
                this.load_cloud(cx);
            },
        );
        let status: AnyElement = match (&options, loading) {
            (_, true) => div().flex().items_center().gap(px(6.)).text_size(px(12.5)).text_color(theme.text_muted).child(kit::spinner("cloud-check", 12., theme.text_muted)).child("Checking…").into_any_element(),
            (Some(o), _) if o.account.is_some() => {
                let a = o.account.clone().unwrap_or_default();
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.5))
                    .child(kit::icon("circle-check", 14., theme.success))
                    .child(div().flex_1().child(match &a.team {
                        Some(t) => format!("Connected as {} · {t}", a.email),
                        None => format!("Connected as {}", a.email),
                    }))
                    .when(method != "doctl", |d| d.child(Button::new("do-signout", "Sign out").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, _, cx| this.cloud_sign_out(cx)))))
                    .into_any_element()
            }
            (o, _) => {
                let oauth_ok = o.as_ref().is_some_and(|o| o.oauth_available);
                match method.as_str() {
                    "oauth" if oauth_ok => Button::new("do-oauth", "Sign in with DigitalOcean")
                        .style(ButtonStyle::Primary)
                        .loading(self.cloud.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.cloud_connect("oauth", cx)))
                        .into_any_element(),
                    "oauth" => note(theme, "This copy of Muxen can't sign in in the browser yet. Use an API token or doctl.").into_any_element(),
                    "doctl" => div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(note(theme, "Run `doctl auth init` in a terminal, then check again."))
                        .child(Button::new("do-doctl", "Check again").icon("refresh-cw").on_click(cx.listener(|this, _, _, cx| this.cloud_connect("doctl", cx))))
                        .into_any_element(),
                    _ => div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(div().flex_1().child(self.cloud.token.clone()))
                                .child(Button::new("do-connect", "Connect").style(ButtonStyle::Primary).loading(self.cloud.busy).on_click(cx.listener(|this, _, _, cx| this.cloud_connect("token", cx)))),
                        )
                        .child(
                            div()
                                .id("do-token-link")
                                .text_size(px(12.))
                                .text_color(theme.text_accent)
                                .cursor_pointer()
                                .child("Create a token (read and write) on DigitalOcean")
                                .on_click(|_, _, _| crate::util::open_url("https://cloud.digitalocean.com/account/api/tokens")),
                        )
                        .into_any_element(),
                }
            }
        };
        let error = options.as_ref().and_then(|o| o.error.clone()).filter(|_| options.as_ref().is_some_and(|o| o.signed_in));
        let account = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "DigitalOcean", None))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(12.))
                    .gap(px(12.))
                    .child(row(theme, "Sign in with", methods.into_any_element()))
                    .child(status)
                    .children(error.map(|e| div().text_size(px(12.)).text_color(theme.error).child(e)))
                    .children(self.cloud.message.clone().map(|(bad, m)| div().text_size(px(12.)).text_color(if bad { theme.error } else { theme.success }).child(m))),
            );

        // New machines: only with a working sign-in.
        let defaults = options.as_ref().filter(|o| o.account.is_some()).map(|o| {
            let region = if settings.region.is_empty() { o.default_region.clone() } else { settings.region.clone() };
            let chosen_city = crate::cloud::city_of_slug(&o.regions, &region);
            let regions: Vec<AnyElement> = crate::cloud::cities(&o.regions)
                .into_iter()
                .map(|(city, slug)| {
                    let selected = chosen_city.as_deref() == Some(city.as_str());
                    chip(theme, SharedString::from(format!("do-region-{slug}")), city, selected)
                        .on_click(cx.listener(move |this, _, _, cx| this.save_cloud(|c| c.region = slug.clone(), cx)))
                        .into_any_element()
                })
                .collect();
            let sizes: Vec<AnyElement> = o
                .sizes
                .iter()
                .filter(|s| regular_size(&s.slug))
                .take(8)
                .enumerate()
                .map(|(i, s)| {
                    let selected = s.slug == settings.size;
                    let slug = s.slug.clone();
                    div()
                        .id(("do-size", i))
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
                        .on_click(cx.listener(move |this, _, _, cx| this.save_cloud(|c| c.size = slug.clone(), cx)))
                        .into_any_element()
                })
                .collect();
            // The extras in their groups, one checkbox each, three to a row.
            let page = cx.entity().downgrade();
            let groups: Vec<AnyElement> = crate::cloud::setup::GROUPS
                .iter()
                .map(|(group, ids)| {
                    let boxes: Vec<AnyElement> = ids
                        .iter()
                        .map(|id| {
                            let on = settings.install.iter().any(|p| p == id);
                            let (id, page) = (id.to_string(), page.clone());
                            div()
                                .w(px(160.))
                                .child(kit::checkbox(theme, SharedString::from(format!("do-x-{id}")), on, crate::cloud::setup::extra_name(&id), move |v, _, cx: &mut App| {
                                    let (v, id) = (*v, id.clone());
                                    let _ = page.update(cx, |this, cx| {
                                        this.save_cloud(
                                            |c| {
                                                c.install.retain(|x| *x != id);
                                                if v {
                                                    c.install.push(id);
                                                }
                                            },
                                            cx,
                                        )
                                    });
                                }))
                                .into_any_element()
                        })
                        .collect();
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(*group))
                        .child(kit::rows_of(boxes, 3, 10.))
                        .into_any_element()
                })
                .collect();
            let extras = div().flex().flex_col().gap(px(14.)).children(groups);
            let has_key = secrets::get(&self.engine.paths().dir, TAILSCALE_KEY).is_some();
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(section_title(theme, "New machines", None))
                .child(
                    card(theme)
                        .px(px(14.))
                        .py(px(12.))
                        .gap(px(12.))
                        .child(row(theme, "Region", kit::rows_of(regions, 5, 6.).into_any_element()))
                        .child(row(theme, "Size", div().flex().flex_col().gap(px(4.)).children(sizes).into_any_element()))
                        .child(row(theme, "Comes with", extras.into_any_element()))
                        .when(settings.install.iter().any(|x| x == "tailscale"), |d| {
                            d.child(row(
                                theme,
                                "Tailscale key",
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.))
                                            .child(div().flex_1().child(self.cloud.tailscale_key.clone()))
                                            .child(Button::new("ts-key-save", "Save").on_click(cx.listener(|this, _, _, cx| this.save_tailscale_key(cx)))),
                                    )
                                    .child(note(theme, if has_key { "A key is saved. New machines join your tailnet with it." } else { "Without one, Muxen opens Tailscale's approval page for each new machine." }))
                                    .into_any_element(),
                            ))
                        }),
                )
        });

        // When nobody uses a cloud machine.
        let idle = segmented(
            theme,
            "do-idle",
            &[("off", "Nothing"), ("warn", "Tell me"), ("poweroff", "Power off"), ("destroy", "Destroy")],
            &settings.idle_action,
            cx,
            |this, value, cx| this.save_cloud(|c| c.idle_action = value.into(), cx),
        );
        let hours = settings.idle_hours;
        let stepper = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(Button::icon_only("idle-less", "minus").on_click(cx.listener(move |this, _, _, cx| this.save_cloud(|c| c.idle_hours = hours.saturating_sub(1).max(1), cx))))
            .child(div().w(px(70.)).flex().justify_center().font_family(mono_font()).text_size(px(12.5)).child(format!("{hours} h")))
            .child(Button::icon_only("idle-more", "plus").on_click(cx.listener(move |this, _, _, cx| this.save_cloud(|c| c.idle_hours = (hours + 1).min(72), cx))));
        let warning = match settings.idle_action.as_str() {
            "poweroff" => Some("DigitalOcean still bills a droplet that's off."),
            "destroy" => Some("Everything on it is lost. Push your work."),
            _ => None,
        };
        let when_idle = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "When one sits idle", None))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(12.))
                    .gap(px(12.))
                    .child(row(theme, "Then", idle.into_any_element()))
                    .when(settings.idle_action != "off", |d| d.child(row(theme, "After", stepper.into_any_element())))
                    .children(warning.map(|w| note(theme, w)))
                    .child(note(theme, "Idle: no busy session, running job or CPU use. Muxen has to be open to notice.")),
            );

        div().flex().flex_col().gap(px(28.)).child(account).children(defaults).child(when_idle)
    }
}

/// Basic droplets on regular CPUs: the premium AMD and Intel twins of each size are left out.
pub(crate) fn regular_size(slug: &str) -> bool {
    slug.starts_with("s-") && !slug.ends_with("-amd") && !slug.ends_with("-intel")
}

fn row(theme: &Theme, label: &'static str, control: AnyElement) -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap(px(12.))
        .child(div().w(px(110.)).flex_none().pt(px(5.)).text_size(px(12.5)).text_color(theme.text_muted).child(label))
        .child(div().flex_1().min_w(px(0.)).child(control))
}

fn note(theme: &Theme, text: &'static str) -> gpui::Div {
    div().text_size(px(12.)).text_color(theme.text_placeholder).child(text)
}

fn chip(theme: &Theme, id: SharedString, text: String, selected: bool) -> gpui::Stateful<gpui::Div> {
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
}

/// A row of choices, one picked.
fn segmented(
    theme: &Theme,
    id: &'static str,
    choices: &[(&'static str, &'static str)],
    selected: &str,
    cx: &mut Context<SettingsPage>,
    pick: fn(&mut SettingsPage, &'static str, &mut Context<SettingsPage>),
) -> AnyElement {
    let items: Vec<AnyElement> = choices
        .iter()
        .map(|(value, label)| {
            let on = *value == selected;
            let value = *value;
            div()
                .id(SharedString::from(format!("{id}-{value}")))
                .h(px(26.))
                .px(px(11.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(if on { theme.text } else { theme.text_muted })
                .when(on, |d| d.bg(theme.surface).border_1().border_color(theme.border).shadow_sm())
                .when(!on, |d| d.hover(|st| st.text_color(theme.text)))
                .child(*label)
                .on_click(cx.listener(move |this, _, _, cx| pick(this, value, cx)))
                .into_any_element()
        })
        .collect();
    div().flex().gap(px(2.)).p(px(3.)).rounded(px(8.)).bg(theme.element).border_1().border_color(theme.border_variant).children(items).into_any_element()
}
