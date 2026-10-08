//! Settings: which GitHub orgs/repos to watch, polling intervals, the inventory file.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px};

use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, card, icon, mono, section_title};
use super::theme::{self as theme_mod, ActiveTheme, Appearance, mono_font, Mode, Theme};
use super::themes::{PALETTES, Palette};
use crate::config::Settings;
use crate::fleet::Engine;
use crate::fleet::model::Snapshot;

pub struct SettingsPage {
    engine: Arc<Engine>,
    snapshot: Arc<Snapshot>,
    add: Entity<TextInput>,
    poll: Entity<TextInput>,
    scan: Entity<TextInput>,
    interval: Entity<TextInput>,
    testing: bool,
    add_result: Option<(bool, String, Option<String>)>,
    save_error: Option<String>,
    saved_at: Option<Instant>,
    pub scroll: gpui::ScrollHandle,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = engine.settings();
        let add = cx.new(|cx| TextInput::new(cx, "acme  or  acme/api").mono());
        let poll = cx.new(|cx| TextInput::new(cx, "20").mono().with_text(s.github_poll_seconds.to_string()));
        let scan = cx.new(|cx| TextInput::new(cx, "10").mono().with_text(s.github.scan_repos.to_string()));
        let interval = cx.new(|cx| TextInput::new(cx, "15").mono().with_text(s.check_interval_minutes.to_string()));

        let mut subs = vec![cx.subscribe_in(&add, window, |this, _, e: &InputEvent, _, cx| match e {
            InputEvent::Submit => this.add_scope(None, cx),
            InputEvent::Changed => {
                this.add_result = None;
                cx.notify()
            }
            InputEvent::Cancel => {}
        })];
        for input in [&poll, &scan, &interval] {
            subs.push(cx.subscribe_in(input, window, |this, _, e: &InputEvent, _, cx| {
                if *e == InputEvent::Submit {
                    this.save_intervals(cx)
                }
            }));
        }

        let mut rx = engine.subscribe();
        let snapshot = rx.borrow_and_update().clone();
        let updates = cx.spawn(async move |this, cx| {
            while rx.changed().await.is_ok() {
                let next = rx.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| {
                        this.snapshot = next;
                        cx.notify()
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        SettingsPage {
            engine,
            snapshot,
            add,
            poll,
            scan,
            interval,
            testing: false,
            add_result: None,
            save_error: None,
            saved_at: None,
            scroll: gpui::ScrollHandle::new(),
            _tasks: vec![updates],
            _subscriptions: subs,
        }
    }

    /// Checks access to an org or repo, then adds it. `preset` comes from a detected-scope chip.
    fn add_scope(&mut self, preset: Option<String>, cx: &mut Context<Self>) {
        let scope = preset.unwrap_or_else(|| self.add.read(cx).text().trim().to_string());
        if scope.is_empty() || self.testing {
            return;
        }
        self.testing = true;
        self.add_result = None;
        cx.notify();
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            let scope = scope.clone();
            async move { engine.test_scope(&scope).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.testing = false;
                match result {
                    Ok(t) if t.error.is_none() => {
                        let mut s = this.engine.settings();
                        let list = if t.name.contains('/') { &mut s.github.repos } else { &mut s.github.orgs };
                        if !list.iter().any(|x| x.eq_ignore_ascii_case(&t.name)) {
                            list.push(t.name.clone());
                        }
                        match this.engine.update_settings(s) {
                            Ok(_) => {
                                this.add_result = Some((
                                    true,
                                    format!("Watching {} · {} runner{} found", t.name, t.runners, if t.runners == 1 { "" } else { "s" }),
                                    None,
                                ));
                                this.add.update(cx, |a, cx| a.set_text("", cx));
                            }
                            Err(e) => this.add_result = Some((false, e, None)),
                        }
                    }
                    Ok(t) => this.add_result = Some((false, t.error.unwrap_or_default(), t.hint)),
                    Err(e) => this.add_result = Some((false, format!("The check failed: {e}"), None)),
                }
                cx.notify();
            });
        });
        self._tasks.push(task);
    }

    fn remove_scope(&mut self, name: &str, cx: &mut Context<Self>) {
        let mut s = self.engine.settings();
        s.github.orgs.retain(|o| o != name);
        s.github.repos.retain(|r| r != name);
        if let Err(e) = self.engine.update_settings(s) {
            self.save_error = Some(e);
        }
        cx.notify();
    }

    fn save_intervals(&mut self, cx: &mut Context<Self>) {
        let num = |input: &Entity<TextInput>, what: &str| -> Result<u32, String> {
            input.read(cx).text().trim().parse::<u32>().map_err(|_| format!("{what} must be a whole number."))
        };
        let parsed = (|| -> Result<Settings, String> {
            let mut s = self.engine.settings();
            s.github_poll_seconds = num(&self.poll, "The GitHub poll interval")?;
            s.github.scan_repos = num(&self.scan, "Repos to scan")?;
            s.check_interval_minutes = num(&self.interval, "The check interval")?;
            Ok(s)
        })();
        match parsed.and_then(|s| self.engine.update_settings(s)) {
            Ok(saved) => {
                self.save_error = None;
                self.saved_at = Some(Instant::now());
                // Show the clamped values.
                self.poll.update(cx, |i, cx| i.set_text(saved.github_poll_seconds.to_string(), cx));
                self.scan.update(cx, |i, cx| i.set_text(saved.github.scan_repos.to_string(), cx));
                self.interval.update(cx, |i, cx| i.set_text(saved.check_interval_minutes.to_string(), cx));
                let clear = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(3)).await;
                    let _ = this.update(cx, |this, cx| {
                        this.saved_at = None;
                        cx.notify()
                    });
                });
                self._tasks.push(clear);
            }
            Err(e) => self.save_error = Some(e),
        }
        cx.notify();
    }

    fn github_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.snapshot;
        let settings = self.engine.settings();
        let account: AnyElement = if s.github.token_source.is_empty() && !s.demo {
            note(theme, "triangle-alert", theme.warning, "No GitHub token found. Run `gh auth login` in a terminal, or set GITHUB_TOKEN.").into_any_element()
        } else {
            let who = s.github.login.clone().map(|l| format!("Signed in as {l}")).unwrap_or_else(|| "Token found".into());
            let via = match s.github.token_source.as_str() {
                "gh" => "using your gh login".to_string(),
                "" => String::new(),
                var => format!("from ${var}"),
            };
            note(theme, "circle-check", theme.success, format!("{who} {via}")).into_any_element()
        };

        let watched: Vec<(String, &'static str)> = settings
            .github
            .orgs
            .iter()
            .map(|o| (o.clone(), "layers"))
            .chain(settings.github.repos.iter().map(|r| (r.clone(), "git-branch")))
            .collect();
        let rows: Vec<AnyElement> = watched
            .iter()
            .enumerate()
            .map(|(i, (name, icon_name))| {
                let status = s.github.scopes.iter().find(|sc| &sc.name == name);
                let (color, text) = match status {
                    Some(sc) if sc.error.is_some() => (theme.error, sc.error.clone().unwrap_or_default()),
                    Some(sc) if sc.synced_at.is_some() => {
                        (theme.text_muted, format!("{} runner{}", sc.runners, if sc.runners == 1 { "" } else { "s" }))
                    }
                    _ => (theme.text_placeholder, "syncing…".to_string()),
                };
                let hint = status.and_then(|sc| sc.hint.clone());
                let remove = name.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .px(px(14.))
                    .py(px(9.))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(icon(icon_name, 14., theme.text_muted))
                            .child(div().font_family(mono_font()).text_size(px(13.)).child(name.clone()))
                            .child(div().flex_1().truncate().text_size(px(12.)).text_color(color).child(text))
                            .child(
                                Button::icon_only(("scope-remove", i), "x")
                                    .tooltip("Stop watching")
                                    .on_click(cx.listener(move |this, _, _, cx| this.remove_scope(&remove, cx))),
                            ),
                    )
                    .children(hint.map(|h| hint_line(theme, i, h)))
                    .into_any_element()
            })
            .collect();

        let detected: Vec<AnyElement> = s
            .github
            .detected
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let scope = d.clone();
                div()
                    .id(("detected", i))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(24.))
                    .px(px(8.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(theme.border_variant)
                    .bg(theme.element)
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.element_hover))
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .child(icon("plus", 12., theme.text_muted))
                    .child(d.clone())
                    .on_click(cx.listener(move |this, _, _, cx| this.add_scope(Some(scope.clone()), cx)))
                    .into_any_element()
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "GitHub", None))
            .child(
                div()
                    .w_full()
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child("Live runner status and jobs come from the organizations and repositories you watch. Reading org runners needs an org admin token (admin:org)."),
            )
            .child(account)
            .child(
                card(theme)
                    .children(rows)
                    .when(watched.is_empty(), |d| d.child(kit::empty_state(theme, "layers", "Not watching anything yet.")))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .p(px(10.))
                            .border_t_1()
                            .border_color(theme.border_variant)
                            // Wrap inputs in a flex column (like dialog fields) so they stretch to the
                            // row instead of overflowing it.
                            .child(div().flex().flex_col().flex_1().min_w(px(0.)).child(self.add.clone()))
                            .child(
                                Button::new("scope-add", "Add")
                                    .style(ButtonStyle::Primary)
                                    .icon("plus")
                                    .loading(self.testing)
                                    .on_click(cx.listener(|this, _, _, cx| this.add_scope(None, cx))),
                            ),
                    ),
            )
            .children(self.add_result.as_ref().map(|(ok, msg, hint)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(note(theme, if *ok { "circle-check" } else { "circle-alert" }, if *ok { theme.success } else { theme.error }, msg.clone()))
                    .children(hint.clone().map(|h| hint_line(theme, 99, h)))
            }))
            .when(!detected.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child("Found in runner configs:"))
                        .children(detected),
                )
            })
    }

    fn checks_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let row = |label: &'static str, input: Entity<TextInput>, unit: &'static str, help: &'static str, first: bool| {
            div()
                .flex()
                .items_center()
                .when(!first, |d| d.border_t_1().border_color(theme.border_variant))
                .gap(px(12.))
                .px(px(14.))
                .py(px(10.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .child(div().text_size(px(13.)).child(label))
                        .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(help)),
                )
                .child(div().w(px(80.)).flex_none().flex().flex_col().child(input))
                .child(div().w(px(60.)).text_size(px(12.)).text_color(theme.text_muted).child(unit))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Polling", None))
            .child(
                card(theme)
                    .child(row("Sync GitHub every", self.poll.clone(), "seconds", "Unchanged responses don't count against the API rate limit.", true))
                    .child(row(
                        "Scan recent repos for jobs",
                        self.scan.clone(),
                        "per org",
                        "The most recently pushed repositories are checked for queued and running jobs.",
                        false,
                    ))
                    .child(row(
                        "Check machines every",
                        self.interval.clone(),
                        "minutes",
                        "Runs the read-only machine check over SSH. 0 means only when you ask.",
                        false,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(Button::new("save-intervals", "Save").style(ButtonStyle::Primary).on_click(cx.listener(|this, _, _, cx| this.save_intervals(cx))))
                    .when(self.saved_at.is_some(), |d| d.child(div().text_size(px(12.)).text_color(theme.success).child("Saved")))
                    .children(self.save_error.clone().map(|e| div().text_size(px(12.)).text_color(theme.error).child(e))),
            )
    }

    fn appearance_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = cx.global::<Appearance>().clone();
        let mode_button = |mode: Mode, label: &'static str, icon_name: &'static str| {
            let selected = appearance.mode == mode;
            div()
                .id(label)
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(28.))
                .px(px(12.))
                .rounded(px(7.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(if selected { theme.text } else { theme.text_muted })
                .when(selected, |d| d.bg(theme.surface).border_1().border_color(theme.border).shadow_sm())
                .when(!selected, |d| d.hover(|st| st.text_color(theme.text)))
                .child(icon(icon_name, 13., if selected { theme.text_accent } else { theme.text_placeholder }))
                .child(label)
                .on_click(cx.listener(move |_, _, window, cx| theme_mod::update_appearance(window, cx, |a, _| a.mode = mode)))
        };
        let grid = |dark: bool, cx: &mut Context<Self>| {
            let chosen = if dark { appearance.dark.clone() } else { appearance.light.clone() };
            let swatches: Vec<AnyElement> = PALETTES.iter().filter(|p| p.dark == dark).map(|p| {
                let id = p.id;
                swatch(theme, p, p.id == chosen, theme.id == p.id).on_click(cx.listener(move |_, _, window, cx| {
                    theme_mod::update_appearance(window, cx, |a, system| {
                        let system_dark = matches!(system, gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark);
                        if dark {
                            a.dark = id.into();
                        } else {
                            a.light = id.into();
                        }
                        // Picking a theme you can't currently see switches to it.
                        let showing_dark = match a.mode {
                            Mode::Dark => true,
                            Mode::Light => false,
                            Mode::System => system_dark,
                        };
                        if showing_dark != dark {
                            a.mode = if dark { Mode::Dark } else { Mode::Light };
                        }
                    })
                })).into_any_element()
            }).collect();
            swatch_rows(swatches)
        };
        let modes = [
            mode_button(Mode::System, "System", "monitor").into_any_element(),
            mode_button(Mode::Light, "Light", "circle").into_any_element(),
            mode_button(Mode::Dark, "Dark", "radio").into_any_element(),
        ];
        let dark_grid = grid(true, cx);
        let light_grid = grid(false, cx);
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(section_title(theme, "Appearance", None))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().flex_1().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text_muted).child("Follow the system, or pin light or dark. Each has its own theme."))
                    .child(
                        div()
                            .flex()
                            .gap(px(2.))
                            .p(px(3.))
                            .rounded(px(9.))
                            .bg(theme.element)
                            .border_1()
                            .border_color(theme.border_variant)
                            .children(modes),
                    ),
            )
            .child(kit::label(theme, "Dark themes").text_size(px(12.)).text_color(theme.text_muted))
            .child(dark_grid)
            .child(kit::label(theme, "Light themes").text_size(px(12.)).text_color(theme.text_muted))
            .child(light_grid)
    }

    fn inventory_section(&self, theme: &Theme) -> impl IntoElement {
        let path = self.engine.paths().inventory.clone();
        let reveal = path.clone();
        let shown = self.snapshot.inventory.path.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Inventory", None))
            .child(
                card(theme)
                    .p(px(14.))
                    .gap(px(10.))
                    .child(div().w_full().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text_muted).child(
                        "Machines and the runners they should host live in fleet.toml. OpenRunner edits it when you add machines or install runners, and keeps your comments.",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().flex_1().truncate().font_family(mono_font()).text_size(px(12.5)).child(shown))
                            .when(!self.snapshot.demo, |d| {
                                d.child(Button::new("inv-open", "Open").icon("external-link").on_click(move |_, _, cx| cx.open_with_system(&path)))
                                    .child(Button::new("inv-reveal", "Reveal").icon("folder-open").on_click(move |_, _, cx| cx.reveal_path(&reveal)))
                            }),
                    ),
            )
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let shortcuts = [
            ("⌘R", "Check now"),
            ("⇧⌘R", "Sync GitHub"),
            ("⌘N", "Add machine"),
            ("⌘1 / ⌘2", "Overview / Jobs"),
            ("⌘F or /", "Filter the fleet"),
            ("⌘J", "Toggle activity"),
            ("⌘[ / ⌘]", "Back / forward"),
            ("⌘,", "Settings"),
        ];
        div()
            .id("settings-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(28.))
            .pt(px(24.))
            .pb(px(40.))
            .child(
                div()
                    .w_full()
                    .max_w(px(760.))
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .child(self.github_section(&theme, cx))
                    .child(self.appearance_section(&theme, cx))
                    .child(self.checks_section(&theme, cx))
                    .child(self.inventory_section(&theme))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child(section_title(&theme, "Keyboard", None))
                            .child(card(&theme).px(px(14.)).py(px(6.)).children(shortcuts.iter().map(|(k, what)| {
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .py(px(5.))
                                    .text_size(px(12.5))
                                    .child(div().text_color(theme.text_muted).child(*what))
                                    .child(kit::kbd(&theme, *k))
                            }))),
                    ),
            )
    }
}

fn note(_theme: &Theme, icon_name: &'static str, color: gpui::Hsla, text: impl Into<SharedString>) -> impl IntoElement {
    div().flex().items_center().gap(px(8.)).text_size(px(12.5)).child(icon(icon_name, 14., color)).child(text.into())
}

fn hint_line(theme: &Theme, i: usize, hint: String) -> impl IntoElement {
    let copy = hint.clone();
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .pl(px(24.))
        .child(mono(hint).text_size(px(12.)).px(px(6.)).py(px(2.)).rounded(px(4.)).bg(theme.element).text_color(theme.text_muted))
        .child(Button::icon_only(("hint-copy", i), "copy").tooltip("Copy").on_click(move |_, _, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))
        }))
}

/// A miniature of the theme: sidebar, a card, the accent and the status colors.
fn swatch(theme: &Theme, p: &Palette, chosen: bool, showing: bool) -> gpui::Stateful<gpui::Div> {
    let t = p.theme();
    let bar = |w: f32, color: gpui::Hsla| div().w(px(w)).h(px(4.)).rounded_full().bg(color);
    div()
        .id(SharedString::from(format!("theme-{}", p.id)))
        .w(px(164.))
        .flex()
        .flex_col()
        .gap(px(7.))
        .cursor_pointer()
        .child(
            div()
                .h(px(78.))
                .flex()
                .rounded(px(10.))
                .overflow_hidden()
                .border_2()
                .border_color(if chosen { theme.accent } else { theme.border_variant })
                .hover(|st| st.border_color(if chosen { theme.accent } else { theme.border }))
                .child(
                    div()
                        .w(px(36.))
                        .h_full()
                        .bg(t.sidebar)
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .px(px(6.))
                        .pt(px(10.))
                        .child(div().size(px(10.)).rounded(px(3.)).bg(t.accent))
                        .child(bar(20., t.text_placeholder.opacity(0.7)))
                        .child(bar(16., t.text_placeholder.opacity(0.5)))
                        .child(bar(18., t.text_placeholder.opacity(0.5))),
                )
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .bg(t.canvas)
                        .p(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(7.))
                        .child(
                            div()
                                .h(px(30.))
                                .rounded(px(6.))
                                .bg(t.surface)
                                .border_1()
                                .border_color(t.border_variant)
                                .flex()
                                .flex_col()
                                .justify_center()
                                .gap(px(4.))
                                .px(px(7.))
                                .child(bar(46., t.text.opacity(0.85)))
                                .child(div().flex().gap(px(3.)).child(div().size(px(6.)).rounded(px(1.5)).bg(t.accent)).child(div().size(px(6.)).rounded(px(1.5)).bg(t.success)).child(div().size(px(6.)).rounded(px(1.5)).bg(t.success)).child(div().size(px(6.)).rounded(px(1.5)).border_1().border_color(t.text_placeholder))),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .child(div().size(px(6.)).rounded_full().bg(t.warning))
                                .child(div().size(px(6.)).rounded_full().bg(t.error))
                                .child(div().size(px(6.)).rounded_full().bg(t.info))
                                .child(div().flex_1())
                                .child(div().w(px(24.)).h(px(9.)).rounded(px(3.)).bg(t.accent)),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(if chosen { theme.text } else { theme.text_muted })
                .child(p.name)
                .when(showing, |d| d.child(icon("check", 12., theme.text_accent))),
        )
}

/// Swatches in explicit rows of four. A wrapping flex container inside this
/// auto-width column gets measured as a single row and the sections below
/// overlap it.
fn swatch_rows(swatches: Vec<AnyElement>) -> gpui::Div {
    let mut rows = Vec::new();
    let mut iter = swatches.into_iter().peekable();
    while iter.peek().is_some() {
        let row: Vec<AnyElement> = iter.by_ref().take(4).collect();
        rows.push(div().flex().gap(px(12.)).children(row));
    }
    div().flex().flex_col().gap(px(14.)).children(rows)
}
