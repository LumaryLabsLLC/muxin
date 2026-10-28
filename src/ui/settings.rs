//! Settings: which GitHub orgs/repos to watch, polling intervals, the inventory file.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AnyElement, App, Context, Entity, Focusable, SharedString, Subscription, Task, Window, div, prelude::*, px};

mod cloud;
mod ipads;
mod prompts;
mod shell;


use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, card, icon, mono, section_title};
use super::theme::{self as theme_mod, ActiveTheme, Appearance, mono_font, Mode, Theme};
use super::themes::{PALETTES, Palette};
use crate::config::Settings;
use crate::fleet::Engine;
use crate::fleet::model::Snapshot;

/// The groups Settings is split into, one tab each.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SettingsTab {
    #[default]
    General,
    Appearance,
    Terminal,
    GitHub,
    Cloud,
    Ipads,
    Prompts,
    Keyboard,
}

impl SettingsTab {
    const ALL: [SettingsTab; 8] = [SettingsTab::General, SettingsTab::Appearance, SettingsTab::Terminal, SettingsTab::GitHub, SettingsTab::Cloud, SettingsTab::Ipads, SettingsTab::Prompts, SettingsTab::Keyboard];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Terminal => "Terminal",
            SettingsTab::GitHub => "GitHub",
            SettingsTab::Cloud => "Cloud",
            SettingsTab::Ipads => "iPads",
            SettingsTab::Prompts => "Prompts",
            SettingsTab::Keyboard => "Keyboard",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsTab::General => "sliders-horizontal",
            SettingsTab::Appearance => "palette",
            SettingsTab::Terminal => "terminal",
            SettingsTab::GitHub => "agent-github",
            SettingsTab::Cloud => "cloud",
            SettingsTab::Ipads => "tablet",
            SettingsTab::Prompts => "message-square",
            SettingsTab::Keyboard => "keyboard",
        }
    }
}

pub struct SettingsPage {
    engine: Arc<Engine>,
    snapshot: Arc<Snapshot>,
    add: Entity<TextInput>,
    /// A folder to look for checkouts in, and what adding the last one did.
    code_add: Entity<TextInput>,
    code_note: Option<(bool, String)>,
    poll: Entity<TextInput>,
    scan: Entity<TextInput>,
    interval: Entity<TextInput>,
    testing: bool,
    add_result: Option<(bool, String, Option<String>)>,
    save_error: Option<String>,
    saved_at: Option<Instant>,
    /// The login in use: its permissions and organizations (to offer them).
    access: Option<crate::api::AccountsOut>,
    pub scroll: gpui::ScrollHandle,
    tab: SettingsTab,
    cloud: cloud::CloudState,
    /// Pairing an iPad, and the ones paired.
    ipad: ipads::IpadState,
    /// The `muxin` command in the PATH.
    shell: shell::ShellState,
    /// A shortcut waiting for its new keys, and what catches them before
    /// they run anything.
    recording: Option<&'static str>,
    catch_keys: Option<Subscription>,
    /// What the last change did ("Removed from Jobs"), or why it didn't happen.
    shortcut_note: Option<String>,
    /// The session action prompt being changed (see prompts.rs).
    prompt_edit: Option<prompts::PromptEdit>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = engine.settings();
        let add = cx.new(|cx| TextInput::new(cx, "acme  or  acme/api").mono());
        let code_add = cx.new(|cx| TextInput::new(cx, "~/app").mono());
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
        subs.push(cx.subscribe_in(&code_add, window, |this, input, e: &InputEvent, _, cx| match e {
            InputEvent::Submit => this.add_code_folder(cx),
            // Typing clears what the last add said (emptying the field after one doesn't).
            InputEvent::Changed => {
                if !input.read(cx).text().is_empty() {
                    this.code_note = None;
                }
                cx.notify()
            }
            InputEvent::Cancel => {}
        }));
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

        let (cloud_state, cloud_subs) = cloud::CloudState::new(window, cx);
        subs.extend(cloud_subs);
        let mut page = SettingsPage {
            engine,
            snapshot,
            add,
            code_add,
            code_note: None,
            poll,
            scan,
            interval,
            testing: false,
            add_result: None,
            save_error: None,
            saved_at: None,
            access: None,
            scroll: gpui::ScrollHandle::new(),
            tab: SettingsTab::default(),
            cloud: cloud_state,
            ipad: ipads::IpadState::default(),
            shell: shell::ShellState::default(),
            recording: None,
            catch_keys: None,
            shortcut_note: None,
            prompt_edit: None,
            _tasks: vec![updates],
            _subscriptions: subs,
        };
        if !page.snapshot.demo {
            page.load_access(cx);
        }
        page.load_shell(cx);
        page
    }

    /// Puts the cursor in the field for watching an organization.
    pub fn focus_add(&self, window: &mut Window, cx: &App) {
        window.focus(&self.add.focus_handle(cx));
    }

    /// Shows a tab, from its top.
    pub fn show_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.scroll.set_offset(gpui::point(px(0.), px(0.)));
            if tab != SettingsTab::Keyboard {
                self.stop_recording(cx);
            }
            if tab == SettingsTab::Cloud {
                self.load_cloud(cx);
            }
            if tab == SettingsTab::Ipads {
                self.load_ipads(cx);
            }
            cx.notify();
        }
    }

    /// Reads the login's permissions and organizations (fresh from the GitHub
    /// CLI, so a permission just granted shows).
    fn load_access(&mut self, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { crate::api::github_accounts(&engine).await }
        });
        let task = cx.spawn(async move |this, cx| {
            if let Ok(Ok(access)) = job.await {
                let _ = this.update(cx, |this, cx| {
                    this.access = Some(access);
                    cx.notify()
                });
            }
        });
        self._tasks.push(task);
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
                    Ok(t) => {
                        this.add_result = Some((false, t.error.unwrap_or_default(), t.hint));
                        // A missing permission may have just been granted elsewhere: look again.
                        this.load_access(cx);
                    }
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

    /// Adds a folder to look for checkouts in (as deep as the usual ones,
    /// unless it says how deep: "~/app:2").
    fn add_code_folder(&mut self, cx: &mut Context<Self>) {
        let typed = self.code_add.read(cx).text().trim().trim_end_matches(['/', '\\']).to_string();
        if typed.is_empty() {
            return;
        }
        let entry = if code_folder_depth(&typed).is_some() { typed } else { format!("{typed}:4") };
        let folder = code_folder_path(&entry).to_string();
        let mut s = self.engine.settings();
        if s.code_folders.iter().any(|f| code_folder_path(f) == folder) {
            self.code_note = Some((false, format!("{folder} is on the list already.")));
        } else {
            s.code_folders.push(entry);
            match self.engine.update_settings(s) {
                Ok(_) => {
                    self.code_note = Some((true, format!("Looking for checkouts in {folder} on your machines.")));
                    self.code_add.update(cx, |i, cx| i.set_text("", cx));
                    self.rescan_checkouts();
                }
                Err(e) => self.code_note = Some((false, e)),
            }
        }
        cx.notify();
    }

    fn remove_code_folder(&mut self, entry: &str, cx: &mut Context<Self>) {
        let mut s = self.engine.settings();
        s.code_folders.retain(|f| f != entry);
        self.save_code_folders(s, cx);
    }

    fn reset_code_folders(&mut self, cx: &mut Context<Self>) {
        let mut s = self.engine.settings();
        s.code_folders = crate::config::default_code_folders();
        self.save_code_folders(s, cx);
    }

    fn save_code_folders(&mut self, s: Settings, cx: &mut Context<Self>) {
        match self.engine.update_settings(s) {
            Ok(_) => {
                self.code_note = None;
                self.rescan_checkouts();
            }
            Err(e) => self.code_note = Some((false, e)),
        }
        cx.notify();
    }

    /// Looks for checkouts again on the machines that are on.
    fn rescan_checkouts(&self) {
        for m in self.snapshot.machines.iter().filter(|m| m.status == "online") {
            let engine = self.engine.clone();
            let id = m.id.clone();
            engine.runtime().clone().spawn(async move {
                let _ = engine.refresh_checkouts(&id).await;
            });
        }
    }

    /// Where the agents look for git checkouts, for worktrees of them.
    fn code_folders_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let folders = self.engine.settings().code_folders;
        let usual = folders == crate::config::default_code_folders();
        let chips: Vec<AnyElement> = folders
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let remove = entry.clone();
                // "~:1": checkouts right in the home folder.
                let label = match code_folder_path(entry) {
                    "~" => "~ (its own folders)".to_string(),
                    folder => folder.to_string(),
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(26.))
                    .pl(px(8.))
                    .pr(px(2.))
                    .rounded(crate::ui::kit::corner(5.))
                    .border_1()
                    .border_color(theme.border_variant)
                    .bg(theme.element)
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .child(icon("folder", 12., theme.text_muted))
                    .child(label)
                    .child(
                        Button::icon_only(("code-folder-remove", i), "x")
                            .tooltip("Stop looking here")
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_code_folder(&remove, cx))),
                    )
                    .into_any_element()
            })
            .collect();
        // As wide as the column (760, see render): a row of chips that wraps
        // would otherwise make it as wide as all of them in one line.
        div()
            .w_full()
            .min_w(px(0.))
            .max_w(px(760.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Code folders", None))
            .child(
                card(theme)
                    .w_full()
                    .min_w(px(0.))
                    .px(px(14.))
                    .py(px(12.))
                    .gap(px(10.))
                    .child(div().w_full().min_w(px(0.)).flex().flex_wrap().gap(px(6.)).children(chips))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().flex().flex_col().flex_1().min_w(px(0.)).child(self.code_add.clone()))
                            .child(Button::new("code-folder-add", "Add").icon("plus").on_click(cx.listener(|this, _, _, cx| this.add_code_folder(cx))))
                            .when(!usual, |d| {
                                d.child(
                                    Button::new("code-folder-reset", "Usual folders")
                                        .style(ButtonStyle::Ghost)
                                        .tooltip("Back to Muxin's own list: ~/Developer, ~/code, ~/src and the like")
                                        .on_click(cx.listener(|this, _, _, cx| this.reset_code_folders(cx))),
                                )
                            }),
                    )
                    .children(self.code_note.as_ref().map(|(ok, text)| note(theme, if *ok { "circle-check" } else { "circle-alert" }, if *ok { theme.success } else { theme.error }, text.clone())))
                    .child(div().w_full().min_w(px(0.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(
                        "Muxin's agent on each machine looks for git checkouts in these folders, a few levels deep. Work on it with an agent and Review with agent then work in a new worktree of a checkout found there, on its own branch, instead of a fresh clone.",
                    )),
            )
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
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(note(theme, "triangle-alert", theme.warning, "Not signed in to GitHub."))
                .child(
                    Button::new("gh-sign-in", "Sign in with GitHub")
                        .style(ButtonStyle::Primary)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(super::root::SignInGitHub), cx)),
                )
                .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child("Uses the GitHub CLI, or set GITHUB_TOKEN."))
                .into_any_element()
        } else {
            let who = s
                .github
                .login
                .clone()
                .or_else(|| self.access.as_ref().and_then(|a| a.using.clone()))
                .map(|l| format!("Signed in as {l}"))
                .unwrap_or_else(|| "Token found".into());
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
                            .child(kit::color_tile(icon_name, 20.))
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

        // Organizations to watch with one click: yours, and ones found in runner configs.
        let watching = |name: &str| settings.github.orgs.iter().chain(settings.github.repos.iter()).any(|w| w.eq_ignore_ascii_case(name));
        let mine: Vec<String> = self.access.as_ref().map(|a| a.orgs.clone()).unwrap_or_default().into_iter().filter(|o| !watching(o)).collect();
        let found: Vec<String> = s.github.detected.iter().filter(|d| !watching(d) && !mine.iter().any(|m| m.eq_ignore_ascii_case(d))).cloned().collect();
        let grant = self.access.as_ref().and_then(|a| a.grant_command.clone()).filter(|_| !s.demo).map(|cmd| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .p(px(12.))
                .rounded(crate::ui::kit::corner(8.))
                .bg(theme.warning.opacity(0.10))
                .child(icon("triangle-alert", 15., theme.warning))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(div().text_size(px(13.)).text_color(theme.text).child("Watching an organization's runners needs one more permission"))
                        .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
                            "Your GitHub login doesn't have admin:org yet. Granting it runs `{cmd}` in a terminal and finishes in your browser."
                        ))),
                )
                .child(
                    Button::new("gh-grant", "Grant access")
                        .style(ButtonStyle::Primary)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(super::root::GrantGitHubAccess), cx)),
                )
                .child(Button::icon_only("gh-recheck", "refresh-cw").tooltip("Refresh").on_click(cx.listener(|this, _, _, cx| this.load_access(cx))))
        });
        let chip = |id: (&'static str, usize), name: &str, cx: &mut Context<Self>| {
            let scope = name.to_string();
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(5.))
                .h(px(24.))
                .px(px(8.))
                .rounded(crate::ui::kit::corner(5.))
                .border_1()
                .border_color(theme.border_variant)
                .bg(theme.element)
                .cursor_pointer()
                .hover(|st| st.bg(theme.element_hover))
                .font_family(mono_font())
                .text_size(px(12.))
                .child(kit::color_icon(theme, "plus", 12., theme.text_muted))
                .child(name.to_string())
                .on_click(cx.listener(move |this, _, _, cx| this.add_scope(Some(scope.clone()), cx)))
                .into_any_element()
        };
        let my_orgs: Vec<AnyElement> = mine.iter().enumerate().map(|(i, o)| chip(("my-org", i), o, cx)).collect();
        let detected: Vec<AnyElement> = found
            .iter()
            .enumerate()
            .map(|(i, d)| chip(("detected", i), d, cx))
            .collect();

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Watching", None))
            .child(
                div()
                    .w_full()
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child("Live runner status and jobs come from the organizations and repositories you watch."),
            )
            .child(account)
            .children(grant)
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
            .when(!my_orgs.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child("Your organizations:"))
                        .children(my_orgs),
                )
            })
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

    /// How often to sync GitHub (`github`) or check the machines (otherwise).
    fn checks_section(&self, theme: &Theme, github: bool, cx: &mut Context<Self>) -> impl IntoElement {
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
        let rows = if github {
            card(theme)
                .child(row("Sync GitHub every", self.poll.clone(), "seconds", "Asks GitHub for runner and job status. Unchanged answers are free.", true))
                .child(row("Scan recent repos for jobs", self.scan.clone(), "per org", "The most recently pushed repositories are checked for jobs.", false))
        } else {
            card(theme).child(row("Check machines every", self.interval.clone(), "minutes", "A read-only check over ssh. 0: only when you ask.", true))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, if github { "Syncing" } else { "Machine checks" }, None))
            .child(rows)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        Button::new(if github { "save-sync" } else { "save-checks" }, "Save")
                            .style(ButtonStyle::Primary)
                            .on_click(cx.listener(|this, _, _, cx| this.save_intervals(cx))),
                    )
                    .when(self.saved_at.is_some(), |d| d.child(div().text_size(px(12.)).text_color(theme.success).child("Saved")))
                    .children(self.save_error.clone().map(|e| div().text_size(px(12.)).text_color(theme.error).child(e))),
            )
    }

    /// Font size and line height for agent session terminals, with a preview.
    fn terminal_section(&self, theme: &Theme, family: SharedString, fonts: Vec<SharedString>, cx: &mut Context<Self>) -> impl IntoElement {
        let a = cx.global::<Appearance>().clone();
        // Each installed monospace font, in itself.
        let font_chips: Vec<AnyElement> = fonts
            .into_iter()
            .enumerate()
            .map(|(i, f)| {
                let selected = f == family;
                let pick = f.to_string();
                div()
                    .id(("term-font", i))
                    .h(px(28.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .rounded(crate::ui::kit::corner(6.))
                    .border_1()
                    .cursor_pointer()
                    .border_color(if selected { theme.accent } else { theme.border_variant })
                    .bg(if selected { theme.accent.opacity(0.10) } else { theme.element })
                    .when(!selected, |d| d.hover(|st| st.bg(theme.element_hover)))
                    .font_family(f.clone())
                    .text_size(px(12.5))
                    .text_color(if selected { theme.text } else { theme.text_muted })
                    .child(f)
                    .on_click(cx.listener(move |_, _, _, cx| theme_mod::update_terminal(cx, |a| a.terminal_font = pick.clone())))
                    .into_any_element()
            })
            .collect();
        let (size, line_height) = (a.terminal_font_size, a.terminal_line_height);
        let stepper = |id: &'static str, value: String, step: fn(&mut Appearance, f32), cx: &mut Context<Self>| {
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(Button::icon_only((id, 0usize), "minus").on_click(cx.listener(move |_, _, _, cx| theme_mod::update_terminal(cx, |a| step(a, -1.)))))
                .child(div().w(px(56.)).flex().justify_center().font_family(mono_font()).text_size(px(12.5)).child(value))
                .child(Button::icon_only((id, 1usize), "plus").on_click(cx.listener(move |_, _, _, cx| theme_mod::update_terminal(cx, |a| step(a, 1.)))))
        };
        let preset = |label: &'static str, value: f32, cx: &mut Context<Self>| {
            let selected = (line_height - value).abs() < 0.01;
            div()
                .id(label)
                .h(px(24.))
                .px(px(9.))
                .flex()
                .items_center()
                .rounded(crate::ui::kit::corner(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if selected { theme.text } else { theme.text_muted })
                .when(selected, |d| d.bg(theme.surface).border_1().border_color(theme.border))
                .when(!selected, |d| d.hover(|st| st.text_color(theme.text)))
                .child(label)
                .on_click(cx.listener(move |_, _, _, cx| theme_mod::update_terminal(cx, |a| a.terminal_line_height = value)))
        };
        let row = |label: &'static str, control: AnyElement, hint: Option<&'static str>| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .py(px(8.))
                .child(div().w(px(110.)).flex_none().text_size(px(12.5)).text_color(theme.text_muted).child(label))
                .child(control)
                .child(div().flex_1())
                .children(hint.map(|h| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(h)))
        };
        let size_control = stepper("term-size", format!("{size:.0} pt"), |a, d| a.terminal_font_size += d, cx).into_any_element();
        let presets = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(preset("Compact", 1.15, cx))
            .child(preset("Normal", 1.3, cx))
            .child(preset("Relaxed", 1.5, cx))
            .child(div().w(px(8.)))
            .child(stepper("term-line", format!("{line_height:.2}"), |a, d| a.terminal_line_height += d * 0.05, cx))
            .into_any_element();
        let changed = size != theme_mod::TERMINAL_FONT_SIZE
            || (line_height - theme_mod::TERMINAL_LINE_HEIGHT).abs() > 0.01
            || a.terminal_font != super::terminal::DEFAULT_FONT;
        let reset = changed.then(|| {
            Button::new("term-reset", "Reset").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| {
                theme_mod::update_terminal(cx, |a| {
                    a.terminal_font = super::terminal::DEFAULT_FONT.into();
                    a.terminal_font_size = theme_mod::TERMINAL_FONT_SIZE;
                    a.terminal_line_height = theme_mod::TERMINAL_LINE_HEIGHT;
                })
            }))
        });
        // A few lines in the terminal's font, size and spacing.
        let line_px = (size * line_height).round();
        let preview = div()
            .mt(px(4.))
            .p(px(12.))
            .rounded(crate::ui::kit::corner(8.))
            .bg(theme.canvas)
            .border_1()
            .border_color(theme.border_variant)
            .font_family(family.clone())
            .text_size(px(size))
            .line_height(px(line_px))
            .text_color(theme.text)
            .child(div().child("~/code/api $ claude \"fix the flaky test\""))
            .child(div().text_color(theme.success).child("⏺ Read(e2e/checkout.spec.ts)"))
            .child(div().text_color(theme.text_muted).child("  ⎿  12 passed (4.1s)"));
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(div().flex().items_center().child(section_title(theme, "Text and cursor", None)).child(div().flex_1()).children(reset))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(12.))
                            .py(px(8.))
                            .child(div().w(px(110.)).flex_none().pt(px(6.)).text_size(px(12.5)).text_color(theme.text_muted).child("Font"))
                            .child(div().flex_1().min_w(px(0.)).flex().flex_wrap().gap(px(6.)).children(font_chips)),
                    )
                    .child(row("Text size", size_control, Some("⌘+ / ⌘− in a terminal")))
                    .child(row("Line height", presets, None))
                    .child(row(
                        "Cursor",
                        kit::checkbox(theme, "term-blink", a.terminal_cursor_blink, "Blink", |on, _, cx| {
                            let on = *on;
                            theme_mod::update_terminal(cx, |a| a.terminal_cursor_blink = on)
                        })
                        .into_any_element(),
                        Some("While the terminal has focus"),
                    )),
            )
            .child(preview)
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
                .rounded(crate::ui::kit::corner(7.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(if selected { theme.text } else { theme.text_muted })
                .when(selected, |d| d.bg(theme.surface).border_1().border_color(theme.border).when(!crate::ui::kit::square(), |d| d.shadow_sm()))
                .when(!selected, |d| d.hover(|st| st.text_color(theme.text)))
                .child(kit::color_icon(theme, icon_name, 13., if selected { theme.text_accent } else { theme.text_placeholder }))
                .child(label)
                .on_click(cx.listener(move |_, _, window, cx| theme_mod::update_appearance(window, cx, |a, _| a.mode = mode)))
        };
        let grid = |dark: bool, cx: &mut Context<Self>| {
            let chosen = if dark { appearance.dark.clone() } else { appearance.light.clone() };
            let swatches: Vec<AnyElement> = PALETTES.iter().filter(|p| p.dark == dark).map(|p| {
                let id = p.id;
                swatch(theme, p, p.id == chosen, theme.id == p.id).on_click(cx.listener(move |_, _, window, cx| {
                    // Picking a theme you can't currently see switches to it.
                    theme_mod::update_appearance(window, cx, |a, system| theme_mod::choose_theme(a, id, system))
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
            .child(section_title(theme, "Theme", None))
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
                            .rounded(crate::ui::kit::corner(9.))
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
            .child(kit::label(theme, "Icons").text_size(px(12.)).text_color(theme.text_muted))
            .child(self.icon_styles(theme, &appearance, cx))
    }

    /// One card per icon style, each drawn in its own style.
    fn icon_styles(&self, theme: &Theme, appearance: &Appearance, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = kit::IconStyle::from_id(&appearance.icon_style);
        let sample = [("gauge", kit::tiles::OVERVIEW), ("workflow", kit::tiles::JOBS), ("terminal", kit::tiles::WORKSPACES), ("laptop", kit::tiles::MACOS)];
        let cards: Vec<AnyElement> = kit::IconStyle::ALL
            .into_iter()
            .map(|style| {
                let look = kit::Look::new(style, theme);
                let selected = style == chosen;
                div()
                    .id(SharedString::from(format!("icon-style-{}", style.id())))
                    .w(px(118.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(10.))
                    .rounded(crate::ui::kit::corner(9.))
                    .border_1()
                    .cursor_pointer()
                    .border_color(if selected { theme.accent } else { theme.border_variant })
                    .bg(if selected { theme.accent.opacity(0.08) } else { theme.surface })
                    .when(!selected, |d| d.hover(|st| st.border_color(theme.border)))
                    .child(div().flex().gap(px(6.)).children(sample.iter().map(|(name, (from, to))| kit::tile_in(look, name, *from, *to, 20.))))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .text_size(px(12.))
                            .text_color(if selected { theme.text } else { theme.text_muted })
                            .child(style.label())
                            .when(selected, |d| d.child(icon("check", 12., theme.text_accent))),
                    )
                    .on_click(cx.listener(move |_, _, window, cx| theme_mod::update_appearance(window, cx, |a, _| a.icon_style = style.id().into())))
                    .into_any_element()
            })
            .collect();
        div().flex().flex_wrap().gap(px(10.)).children(cards)
    }

    /// Tailscale on this computer: whether it's on, and what Muxin uses it for.
    fn tailscale_section(&self, theme: &Theme) -> Option<impl IntoElement> {
        let t = self.snapshot.tailscale.clone()?;
        let here = if cfg!(target_os = "macos") { "this Mac" } else { "this computer" };
        let on_it = self.snapshot.machines.iter().filter(|m| m.tailnet.is_some()).count();
        let (glyph, color, title, detail) = if t.running {
            (
                "circle-check",
                theme.success,
                match &t.tailnet {
                    Some(n) => format!("Connected to {n}'s tailnet"),
                    None => "Connected to your tailnet".into(),
                },
                match on_it {
                    0 => "None of your machines are on it yet.".to_string(),
                    1 => "1 machine on it, reachable from anywhere.".to_string(),
                    n => format!("{n} machines on it, reachable from anywhere."),
                },
            )
        } else if t.state == "NeedsLogin" {
            ("circle-alert", theme.warning, format!("Tailscale is signed out on {here}"), "Sign in to reach machines away from home.".to_string())
        } else {
            ("circle-alert", theme.warning, format!("Tailscale is off on {here}"), "Turn it on to reach machines away from home.".to_string())
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(section_title(theme, "Tailscale", None))
                .child(
                    card(theme)
                        .flex_row()
                        .items_center()
                        .gap(px(12.))
                        .px(px(14.))
                        .py(px(12.))
                        .child(icon(glyph, 15., color))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.))
                                .gap(px(2.))
                                .child(div().text_size(px(13.)).child(title))
                                .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(detail)),
                        )
                        .when(!t.running && super::can_open_tailscale(), |d| {
                            d.child(Button::new("ts-open", "Open Tailscale").on_click(|_, _, _| super::open_tailscale()))
                        }),
                ),
        )
    }

    /// Sharing the clipboard with the machines.
    fn clipboard_section(&self, theme: &Theme) -> impl IntoElement {
        let on = self.engine.settings().share_clipboard;
        let sharing: Vec<String> = self.snapshot.machines.iter().filter(|m| crate::fleet::clipboard::sharing(&m.id)).map(|m| m.name.clone()).collect();
        let here = if cfg!(target_os = "macos") { "this Mac" } else { "this computer" };
        // Linux doesn't say which copies are a password manager's (see pasteboard_state).
        let passwords = if cfg!(target_os = "linux") { "Passwords copied here are shared too." } else { "Passwords from a password manager aren't shared." };
        let status = match (on, sharing.as_slice()) {
            (false, _) => "Off.".to_string(),
            (true, []) => "No machine with a desktop is connected right now.".to_string(),
            (true, names) => format!("Shared with {} now.", names.join(", ")),
        };
        let engine = self.engine.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Clipboard", None))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(12.))
                    .gap(px(6.))
                    .child(kit::checkbox(theme, "share-clipboard", on, "Share the clipboard with your machines", move |on, _, cx| {
                        let mut s = engine.settings();
                        s.share_clipboard = *on;
                        let _ = engine.update_settings(s);
                        cx.refresh_windows();
                    }))
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!("Copy on {here}, paste on your machines, and back. {passwords}")))
                    .child(div().text_size(px(11.5)).text_color(theme.text_muted).child(status)),
            )
    }

    /// Whether Muxin asks GitHub for its latest release (crate::update).
    fn updates_section(&self, theme: &Theme) -> impl IntoElement {
        let on = self.engine.settings().check_updates;
        let engine = self.engine.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Updates", None))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(12.))
                    .gap(px(6.))
                    .child(kit::checkbox(theme, "check-updates", on, "Tell me when a new version is out", move |on, _, cx| {
                        let mut s = engine.settings();
                        s.check_updates = *on;
                        let _ = engine.update_settings(s);
                        cx.refresh_windows();
                    }))
                    .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child("Muxin asks GitHub for its latest release when it opens and twice a day. Nothing about you is sent.")),
            )
    }

    /// The editor machines' folders open in.
    fn editor_section(&self, theme: &Theme) -> impl IntoElement {
        let chosen = self.engine.settings().editor;
        let installed = crate::fleet::editor::installed();
        let mut options: Vec<(String, String)> = vec![(String::new(), "First installed".into())];
        options.extend(installed.iter().map(|e| (e.id.to_string(), e.name.to_string())));
        let detail = if installed.is_empty() {
            "No editor found: Muxin opens folders in Zed, Cursor or VS Code (with Remote - SSH).".to_string()
        } else {
            "Open in editor opens a machine’s folder over ssh in the editor you pick from its list. The one chosen here comes first, and `muxin edit` uses it.".to_string()
        };
        // Full width, so its wrapped text is measured at that width (else the
        // section below starts a line too early).
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Editor", None))
            .child(
                card(theme)
                    .w_full()
                    .px(px(14.))
                    .py(px(12.))
                    .gap(px(8.))
                    // As wide as its choices (a row keeps it from stretching).
                    .child(div().flex().child(div().flex().gap(px(2.)).p(px(2.)).rounded(crate::ui::kit::corner(7.)).bg(theme.element).children(options.into_iter().map(|(id, name)| {
                        let on = chosen == id;
                        let engine = self.engine.clone();
                        div()
                            .id(SharedString::from(format!("editor-{}", if id.is_empty() { "auto" } else { id.as_str() })))
                            .px(px(10.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .rounded(crate::ui::kit::corner(5.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(if on { theme.text } else { theme.text_placeholder })
                            .when(on, |d| d.bg(theme.element_selected))
                            .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                            .gap(px(6.))
                            .children(crate::fleet::editor::icon(&id).map(|png| gpui::img(png).size(px(14.)).flex_none()))
                            .child(name)
                            .on_click(move |_, _, cx| {
                                let mut s = engine.settings();
                                s.editor = id.clone();
                                let _ = engine.update_settings(s);
                                cx.refresh_windows();
                            })
                    }))))
                    .child(div().w_full().min_w(px(0.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(detail)),
            )
    }

    fn inventory_section(&self, theme: &Theme) -> impl IntoElement {
        let path = self.engine.paths().inventory.clone();
        let reveal = path.clone();
        let shown = self.snapshot.inventory.path.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Machines file", None))
            .child(
                card(theme)
                    .p(px(14.))
                    .gap(px(10.))
                    .child(div().w_full().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text_muted).child(
                        "Machines and the runners they should host live in config.toml. Muxin edits it when you add machines or install runners, and keeps your comments.",
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tabs: Vec<AnyElement> = SettingsTab::ALL
            .into_iter()
            .map(|tab| {
                let selected = self.tab == tab;
                div()
                    .id(SharedString::from(format!("settings-tab-{}", tab.label())))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .h(px(30.))
                    .px(px(12.))
                    .rounded(crate::ui::kit::corner(7.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(if selected { theme.text } else { theme.text_muted })
                    .when(selected, |d| d.bg(theme.surface).border_1().border_color(theme.border).when(!crate::ui::kit::square(), |d| d.shadow_sm()))
                    .when(!selected, |d| d.hover(|st| st.text_color(theme.text)))
                    .child(icon(tab.icon(), 14., if selected { theme.text } else { theme.text_placeholder }))
                    .child(tab.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.show_tab(tab, cx)))
                    .into_any_element()
            })
            .collect();
        let bar = div().flex_none().px(px(28.)).pt(px(18.)).pb(px(14.)).border_b_1().border_color(theme.border_variant).child(
            div().flex().w_full().max_w(px(760.)).gap(px(2.)).p(px(3.)).rounded(crate::ui::kit::corner(9.)).bg(theme.element).border_1().border_color(theme.border_variant).children(tabs),
        );
        let body = div().w_full().max_w(px(760.)).flex().flex_col().gap(px(28.));
        let body = match self.tab {
            SettingsTab::General => body
                .child(self.shell_section(&theme, cx))
                .child(self.clipboard_section(&theme))
                .child(self.editor_section(&theme))
                .child(self.code_folders_section(&theme, cx))
                .child(self.updates_section(&theme))
                .children(self.tailscale_section(&theme))
                .child(self.checks_section(&theme, false, cx))
                .child(self.inventory_section(&theme)),
            SettingsTab::Appearance => body.child(self.appearance_section(&theme, cx)),
            SettingsTab::Terminal => body.child(self.terminal_section(&theme, super::terminal::font_family(window), super::terminal::monospace_families(window), cx)),
            SettingsTab::GitHub => body.child(self.github_section(&theme, cx)).child(self.checks_section(&theme, true, cx)),
            SettingsTab::Cloud => body.child(self.cloud_usage_section(&theme, cx)).child(self.cloud_section(&theme, cx)),
            SettingsTab::Ipads => body.child(self.ipads_section(&theme, cx)),
            SettingsTab::Prompts => body.child(self.prompts_section(&theme, cx)),
            SettingsTab::Keyboard => body.child(self.keyboard_section(&theme, cx)),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(div().id("settings-page").flex_1().min_h(px(0.)).overflow_y_scroll().track_scroll(&self.scroll).px(px(28.)).pt(px(22.)).pb(px(40.)).child(body))
    }
}

impl SettingsPage {
    /// The shortcuts, each one changeable: click its keys, then press new ones.
    fn keyboard_section(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        use super::shortcuts::{self, ALL};
        let any_changed = !cx.global::<theme_mod::Appearance>().shortcuts.is_empty();
        let row = |label: SharedString| div().flex().items_center().gap(px(8.)).min_h(px(34.)).text_size(px(12.5)).child(div().flex_1().text_color(theme.text_muted).child(label));

        // ⌘1…⌘9: one modifier for all nine.
        let chosen = shortcuts::machine_modifier(cx);
        let options: Vec<AnyElement> = shortcuts::machine_modifiers()
            .iter()
            .map(|(value, shown)| {
                let on = chosen == *value;
                div()
                    .id(SharedString::from(format!("sc-m-{shown}")))
                    .h(px(24.))
                    .min_w(px(36.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(crate::ui::kit::corner(5.))
                    .border_1()
                    .border_color(if on { theme.accent } else { theme.border_variant })
                    .bg(if on { theme.accent.opacity(0.12) } else { theme.element })
                    .cursor_pointer()
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .text_color(if on { theme.text } else { theme.text_muted })
                    .child(*shown)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_machine_keys(value, cx)))
                    .into_any_element()
            })
            .collect();
        let machines_label = match shortcuts::machine_keys(cx, 1) {
            Some(k) => format!("Go to machine 1–9 ({} …)", shortcuts::label(&k)),
            None => "Go to machine 1–9".to_string(),
        };
        let machines = row(machines_label.into()).child(div().flex().gap(px(4.)).children(options));

        let rows: Vec<AnyElement> = ALL
            .iter()
            .enumerate()
            .map(|(i, sc)| {
                let recording = self.recording == Some(sc.id);
                let keys = shortcuts::label_for(cx, sc.id);
                let id = sc.id;
                let chip = div()
                    .id(("sc-keys", i))
                    .h(px(24.))
                    .min_w(px(72.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(crate::ui::kit::corner(5.))
                    .border_1()
                    .cursor_pointer()
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .map(|d| match (recording, keys.is_empty()) {
                        (true, _) => d.border_color(theme.accent).bg(theme.accent.opacity(0.12)).text_color(theme.text_accent).child("Press keys…"),
                        (false, true) => d.border_color(theme.border_variant).bg(theme.element).text_color(theme.text_placeholder).child("None"),
                        (false, false) => d.border_color(theme.border_variant).bg(theme.element).text_color(theme.text).child(keys.clone()),
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.record(id, cx)));
                let reset = if shortcuts::changed(cx, sc.id) {
                    Button::icon_only(("sc-reset", i), "rotate-ccw").tooltip("Back to the default").on_click(cx.listener(move |this, _, _, cx| this.reset_shortcut(id, cx))).into_any_element()
                } else {
                    div().w(px(26.)).into_any_element()
                };
                row(sc.label.into()).child(chip).child(reset).into_any_element()
            })
            .collect();
        let fixed = row("Terminal text size".into()).child(kit::kbd(theme, "⌘+  ⌘−  ⌘0")).child(div().w(px(26.)));

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(section_title(theme, "Shortcuts", None))
                    .child(div().flex_1())
                    .when(any_changed, |d| {
                        d.child(Button::new("sc-reset-all", "Reset all").style(ButtonStyle::Ghost).on_click(cx.listener(|this, _, _, cx| this.reset_all_shortcuts(cx))))
                    }),
            )
            .child(div().text_size(px(12.5)).text_color(theme.text_placeholder).child(if cfg!(target_os = "macos") { "Hold ⌘ or ⌃ to see them anywhere." } else { "Hold Ctrl to see them anywhere." }))
            .children(self.shortcut_note.clone().map(|n| note(theme, "info", theme.info, n)))
            .child(
                card(theme)
                    .px(px(14.))
                    .py(px(4.))
                    .child(machines)
                    .child(div().h(px(1.)).my(px(4.)).bg(theme.border_variant))
                    .children(rows)
                    .child(fixed),
            )
    }

    /// Waits for the keys of shortcut `id` (or stops waiting, on a second click).
    fn record(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.recording == Some(id) {
            self.stop_recording(cx);
            return;
        }
        self.recording = Some(id);
        self.shortcut_note = None;
        if self.catch_keys.is_none() {
            // Before shortcuts run: a key pressed here is recorded, not done.
            let page = cx.entity().downgrade();
            self.catch_keys = Some(cx.intercept_keystrokes(move |e, _, cx| {
                let Some(page) = page.upgrade() else { return };
                if page.read(cx).recording.is_none() {
                    return;
                }
                cx.stop_propagation();
                let k = e.keystroke.clone();
                page.update(cx, |p, cx| p.pressed(&k, cx));
            }));
        }
        cx.notify();
    }

    pub(super) fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.take().is_some() {
            cx.notify();
        }
    }

    /// A key pressed while recording: Esc cancels, ⌫ clears, anything with
    /// ⌘, ⌃ or ⌥ becomes the shortcut (taken from whatever had it).
    fn pressed(&mut self, k: &gpui::Keystroke, cx: &mut Context<Self>) {
        use super::shortcuts::{self, MACHINES};
        let Some(id) = self.recording else { return };
        let m = &k.modifiers;
        let plain = !(m.platform || m.control || m.alt || m.shift);
        match k.key.as_str() {
            "escape" if plain => self.recording = None,
            "backspace" | "delete" if plain => {
                shortcuts::set(cx, id, "");
                shortcuts::apply(cx);
                self.recording = None;
            }
            _ => match shortcuts::from_press(k) {
                None => self.shortcut_note = Some(if cfg!(target_os = "macos") { "Add ⌘, ⌃ or ⌥." } else { "Add Ctrl or Alt." }.into()),
                Some(keys) => match shortcuts::used_by(cx, &keys, id) {
                    Some((MACHINES, what)) => self.shortcut_note = Some(format!("{} goes to {what}.", shortcuts::label(&keys))),
                    taken => {
                        if let Some((other, what)) = taken {
                            shortcuts::set(cx, other, "");
                            self.shortcut_note = Some(format!("Taken from {what}."));
                        }
                        shortcuts::set(cx, id, &keys);
                        shortcuts::apply(cx);
                        self.recording = None;
                    }
                },
            },
        }
        cx.notify();
    }

    fn reset_shortcut(&mut self, id: &'static str, cx: &mut Context<Self>) {
        use super::shortcuts::{self, ALL, MACHINES};
        let default = ALL.iter().find(|s| s.id == id).map(|s| s.default).unwrap_or_default();
        self.shortcut_note = None;
        match shortcuts::used_by(cx, default, id) {
            Some((MACHINES, what)) => {
                self.shortcut_note = Some(format!("{} goes to {what}.", shortcuts::label(default)));
                cx.notify();
                return;
            }
            Some((other, what)) => {
                shortcuts::set(cx, other, "");
                self.shortcut_note = Some(format!("Taken from {what}."));
            }
            None => {}
        }
        shortcuts::set(cx, id, default);
        shortcuts::apply(cx);
        cx.notify();
    }

    fn reset_all_shortcuts(&mut self, cx: &mut Context<Self>) {
        theme_mod::remember(cx, |a| a.shortcuts.clear());
        super::shortcuts::apply(cx);
        self.recording = None;
        self.shortcut_note = None;
        cx.notify();
    }

    /// The modifier for ⌘1…⌘9; shortcuts on those keys give way.
    fn set_machine_keys(&mut self, modifier: &'static str, cx: &mut Context<Self>) {
        use super::shortcuts::{self, MACHINES};
        shortcuts::set(cx, MACHINES, modifier);
        let mut freed = Vec::new();
        for n in 1..=9 {
            if let Some((other, what)) = shortcuts::machine_keys(cx, n).and_then(|k| shortcuts::used_by(cx, &k, MACHINES)) {
                shortcuts::set(cx, other, "");
                freed.push(what);
            }
        }
        shortcuts::apply(cx);
        self.shortcut_note = (!freed.is_empty()).then(|| format!("Removed from {}.", freed.join(", ")));
        cx.notify();
    }
}

/// A code folder's folder: "~/code" in "~/code:4" (how deep to look).
fn code_folder_path(entry: &str) -> &str {
    match entry.rsplit_once(':') {
        Some((folder, depth)) if !depth.is_empty() && depth.chars().all(|c| c.is_ascii_digit()) => folder,
        _ => entry,
    }
}

/// How deep to look in a code folder, when the entry says.
fn code_folder_depth(entry: &str) -> Option<usize> {
    let (_, depth) = entry.rsplit_once(':')?;
    if depth.is_empty() || !depth.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    depth.parse().ok()
}

fn note(_theme: &Theme, icon_name: &'static str, color: gpui::Hsla, text: impl Into<SharedString>) -> impl IntoElement {
    div().flex().items_center().gap(px(8.)).text_size(px(12.5)).child(icon(icon_name, 14., color)).child(text.into())
}

/// A fix to run: copyable, and a button when it's a GitHub permission Muxin can ask for.
fn hint_line(theme: &Theme, i: usize, hint: String) -> impl IntoElement {
    let copy = hint.clone();
    let grant = hint.contains("gh auth refresh") && hint.contains("admin:org");
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .pl(px(24.))
        .child(mono(hint).text_size(px(12.)).px(px(6.)).py(px(2.)).rounded(crate::ui::kit::corner(4.)).bg(theme.element).text_color(theme.text_muted))
        .child(Button::icon_only(("hint-copy", i), "copy").tooltip("Copy").on_click(move |_, _, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))
        }))
        .when(grant, |d| {
            d.child(
                Button::new(("hint-grant", i), "Grant access")
                    .style(ButtonStyle::Primary)
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(super::root::GrantGitHubAccess), cx)),
            )
        })
}

/// A miniature of the theme: sidebar, a card, the accent and the status colors.
fn swatch(theme: &Theme, p: &Palette, chosen: bool, showing: bool) -> gpui::Stateful<gpui::Div> {
    let t = p.theme();
    let bar = |w: f32, color: gpui::Hsla| div().w(px(w)).h(px(4.)).rounded_full().bg(color);
    div()
        .id(SharedString::from(format!("theme-{}", p.id)))
        // A quarter of the row: the grid fills the page's width.
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(7.))
        .cursor_pointer()
        .child(
            div()
                .h(px(78.))
                .flex()
                .rounded(crate::ui::kit::corner(10.))
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
                        .child(div().size(px(10.)).rounded(crate::ui::kit::corner(3.)).bg(t.accent))
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
                                .rounded(crate::ui::kit::corner(6.))
                                .bg(t.surface)
                                .border_1()
                                .border_color(t.border_variant)
                                .flex()
                                .flex_col()
                                .justify_center()
                                .gap(px(4.))
                                .px(px(7.))
                                .child(bar(46., t.text.opacity(0.85)))
                                .child(div().flex().gap(px(3.)).child(div().size(px(6.)).rounded(crate::ui::kit::corner(1.5)).bg(t.accent)).child(div().size(px(6.)).rounded(crate::ui::kit::corner(1.5)).bg(t.success)).child(div().size(px(6.)).rounded(crate::ui::kit::corner(1.5)).bg(t.success)).child(div().size(px(6.)).rounded(crate::ui::kit::corner(1.5)).border_1().border_color(t.text_placeholder))),
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
                                .child(div().w(px(24.)).h(px(9.)).rounded(crate::ui::kit::corner(3.)).bg(t.accent)),
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
        let mut row: Vec<AnyElement> = iter.by_ref().take(4).collect();
        // A short last row keeps the same column widths.
        while row.len() < 4 {
            row.push(div().flex_1().into_any_element());
        }
        rows.push(div().flex().gap(px(12.)).children(row));
    }
    div().flex().flex_col().gap(px(14.)).children(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_code_folder_entries() {
        assert_eq!((code_folder_path("~/code:4"), code_folder_depth("~/code:4")), ("~/code", Some(4)));
        assert_eq!((code_folder_path("~/app"), code_folder_depth("~/app")), ("~/app", None));
        assert_eq!((code_folder_path("~:1"), code_folder_depth("~:1")), ("~", Some(1)));
        // A Windows folder's drive letter isn't a depth.
        assert_eq!((code_folder_path("C:\\code"), code_folder_depth("C:\\code")), ("C:\\code", None));
        assert_eq!((code_folder_path("C:\\code:2"), code_folder_depth("C:\\code:2")), ("C:\\code", Some(2)));
        assert_eq!(code_folder_depth("~/odd:"), None);
    }
}
