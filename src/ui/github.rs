//! The GitHub page: what needs you, your repositories, each repository's
//! issues, pull requests, Actions runs and checkouts on your machines, and an
//! issue or pull request in full, with one click to hand it to an agent.

use std::collections::{HashMap, HashSet};

use gpui::{AnyElement, ClipboardItem, Context, Entity, FontWeight, Hsla, MouseButton, SharedString, Window, div, prelude::*, px};

use super::dialogs::{Prefill, WorkspacePlan};
use super::format;
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle, card, icon, section_title, spinner};
use super::menu::{self, MenuTarget};
use super::root::{Root, Route};
use super::theme::{ActiveTheme, Appearance, Theme, mono_font, remember};
use crate::api;
use crate::fleet::model::Checkout;
use crate::github::hub::{Detail, Inbox, Item, Repo, Run};

/// GitHub's mark on a dark tile.
pub const TILE: (u32, u32) = (0x3a3f47, 0x16191d);
const LIST_WIDTH: f32 = 290.;
/// The grouped list's bucket for other people's repositories shared with you.
const SHARED: &str = "@shared";

/// Owner groups start open and are folded by listing them; the shared group
/// starts folded and is opened by listing it.
fn group_open(key: &str, listed: &HashSet<String>) -> bool {
    listed.contains(key) == (key == SHARED)
}

pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum RepoTab {
    #[default]
    Issues,
    Pulls,
    Actions,
    Checkouts,
}

#[derive(Default)]
pub struct GithubUi {
    inbox: Option<Load<Inbox>>,
    repos: Option<Load<Vec<Repo>>>,
    repo: HashMap<String, Load<api::RepoOut>>,
    items: HashMap<(String, u64), Load<Detail>>,
    tab: RepoTab,
    filter: Option<Entity<TextInput>>,
    scanned: bool,
    git_busy: HashSet<String>,
    /// The GitHub CLI's accounts, for the switcher.
    pub(super) accounts: Option<Load<api::AccountsOut>>,
}

impl GithubUi {
    /// Items waiting on you, for the sidebar badge.
    pub fn waiting(&self) -> Option<usize> {
        match &self.inbox {
            Some(Load::Ready(i)) => Some(i.review_requests.len() + i.assigned.len()),
            _ => None,
        }
    }
}

fn label_color(hex: &str) -> Hsla {
    u32::from_str_radix(hex.trim_start_matches('#'), 16).map(|v| gpui::rgb(v).into()).unwrap_or(gpui::rgb(0x8b949e).into())
}

/// A label as GitHub colors it, readable on any theme: its color as a dot.
fn label(theme: &Theme, l: &crate::github::hub::Label) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.))
        .h(px(18.))
        .px(px(7.))
        .rounded_full()
        .border_1()
        .border_color(theme.border_variant)
        .text_size(px(11.))
        .text_color(theme.text_muted)
        .child(kit::dot(label_color(&l.color), 7.))
        .child(l.name.clone())
        .into_any_element()
}

impl Root {
    // -----------------------------------------------------------------------
    // Loading

    /// Runs `fetch` on the engine's runtime and stores what it returns.
    fn gh_load<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        fetch: impl std::future::Future<Output = api::ApiResult<T>> + Send + 'static,
        store: impl FnOnce(&mut GithubUi, Load<T>) + 'static,
    ) {
        let job = self.engine.runtime().spawn(fetch);
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(api::ApiError::invalid(e.to_string())));
            let _ = this.update(cx, |this, cx| {
                store(&mut this.gh, match result {
                    Ok(v) => Load::Ready(v),
                    Err(e) => Load::Failed(match e.hint {
                        Some(h) => format!("{} {h}", e.message),
                        None => e.message,
                    }),
                });
                cx.notify();
            });
        });
        self.track(task);
    }

    fn gh_ensure(&mut self, cx: &mut Context<Self>) {
        if self.gh.inbox.is_none() {
            self.gh.inbox = Some(Load::Loading);
            let e = self.engine.clone();
            self.gh_load(cx, async move { api::github_inbox(&e).await }, |g, v| g.inbox = Some(v));
        }
        if self.gh.repos.is_none() {
            self.gh.repos = Some(Load::Loading);
            let e = self.engine.clone();
            self.gh_load(cx, async move { api::github_repos(&e).await }, |g, v| g.repos = Some(v));
        }
        if self.gh.accounts.is_none() {
            self.gh_reload_accounts(cx);
        }
        // Look for checkouts on the machines once per visit.
        if !self.gh.scanned {
            self.gh.scanned = true;
            for m in self.snapshot.machines.iter().filter(|m| m.status == "online" && !m.platform.starts_with("windows")) {
                let engine = self.engine.clone();
                let id = m.id.clone();
                engine.runtime().clone().spawn(async move {
                    let _ = engine.refresh_checkouts(&id).await;
                });
            }
        }
    }

    /// Re-reads the accounts, keeping the old list on screen meanwhile.
    pub(super) fn gh_reload_accounts(&mut self, cx: &mut Context<Self>) {
        if self.gh.accounts.is_none() {
            self.gh.accounts = Some(Load::Loading);
        }
        let e = self.engine.clone();
        self.gh_load(cx, async move { api::github_accounts(&e).await }, |g, v| {
            if !matches!((&g.accounts, &v), (Some(Load::Ready(_)), Load::Failed(_))) {
                g.accounts = Some(v);
            }
        });
    }

    /// Switches Muxen to another GitHub account (one signed in here, or a
    /// machine's) and reloads everything GitHub.
    pub(super) fn gh_use_account(&mut self, login: String, machine_id: Option<String>, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            let login = login.clone();
            async move { api::github_use_account(&engine, api::UseAccountRequest { login, machine_id }).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(api::ApiError::invalid(e.to_string())));
            let _ = this.update(cx, |this, cx| match result {
                Ok(out) => {
                    this.gh.accounts = Some(Load::Ready(out));
                    this.gh.inbox = None;
                    this.gh.repos = None;
                    this.gh.repo.clear();
                    this.gh.items.clear();
                    this.notify_user(format!("Now using @{login} on GitHub."), false, cx);
                }
                Err(e) => this.notify_user(e.message, true, cx),
            });
        });
        self.track(task);
    }

    fn gh_ensure_repo(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.gh.repo.contains_key(name) {
            self.gh.repo.insert(name.to_string(), Load::Loading);
            let (key, req, e) = (name.to_string(), name.to_string(), self.engine.clone());
            self.gh_load(cx, async move { api::github_repo(&e, api::RepoRequest { repo: req }).await }, move |g, v| {
                g.repo.insert(key, v);
            });
        }
    }

    fn gh_ensure_item(&mut self, repo: &str, number: u64, cx: &mut Context<Self>) {
        let key = (repo.to_string(), number);
        if !self.gh.items.contains_key(&key) {
            self.gh.items.insert(key.clone(), Load::Loading);
            let (reference, e) = (format!("{repo}#{number}"), self.engine.clone());
            self.gh_load(cx, async move { api::github_item(&e, api::ItemRequest { reference }).await }, move |g, v| {
                g.items.insert(key, v);
            });
        }
    }

    /// Reloads what's on screen.
    fn gh_refresh(&mut self, cx: &mut Context<Self>) {
        match self.route.clone() {
            Route::Github => {
                self.gh.inbox = None;
                self.gh.repos = None;
                self.gh.scanned = false;
            }
            Route::GithubRepo(name) => {
                self.gh.repo.remove(&name);
            }
            Route::GithubItem(repo, n) => {
                self.gh.items.remove(&(repo, n));
            }
            _ => {}
        }
        cx.notify();
    }

    fn gh_git(&mut self, c: &Checkout, action: &'static str, cx: &mut Context<Self>) {
        let key = format!("{}:{}", c.machine_id, c.path);
        if !self.gh.git_busy.insert(key.clone()) {
            return;
        }
        let engine = self.engine.clone();
        let (machine, path) = (c.machine_id.clone(), c.path.clone());
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.checkout_git(&machine, &path, action).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.gh.git_busy.remove(&key);
                match result {
                    Ok(out) => {
                        let line = out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("Done").trim().to_string();
                        this.notify_user(format!("git {action}: {line}"), false, cx)
                    }
                    Err(e) => this.notify_user(format!("git {action} failed: {e}"), true, cx),
                }
            });
        });
        self.track(task);
    }

    /// Hands an issue (fix it) or a pull request (review it) to an agent: in a
    /// checkout of its repository, or in a new workspace with a fresh clone
    /// (chosen up front with `workspace`, or when there's no checkout).
    fn gh_to_agent(&mut self, d: &Detail, workspace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let it = &d.item;
        let local: Vec<&Checkout> = self.snapshot.checkouts.iter().filter(|c| c.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&it.repo))).collect();
        // The machine whose GitHub login is in use (else this computer) first,
        // then a clean checkout, then any.
        let account_machine = self.engine.settings().github.account_machine;
        let home = match account_machine.is_empty() {
            false => Some(account_machine),
            true => self.snapshot.machines.iter().find(|m| m.host == "local").map(|m| m.id.clone()),
        };
        let pick = local
            .iter()
            .find(|c| Some(&c.machine_id) == home.as_ref() && c.dirty == 0)
            .or_else(|| local.iter().find(|c| Some(&c.machine_id) == home.as_ref()))
            .or_else(|| local.iter().find(|c| c.dirty == 0))
            .or(local.first())
            .copied()
            .filter(|_| !workspace);
        let verb = if it.pull_request { "Review this pull request" } else { "Fix this GitHub issue" };
        let body: String = d.body.trim().chars().take(3000).collect();
        let mut message = format!("{verb}: {}\n\n#{} {}", it.html_url, it.number, it.title);
        if !body.is_empty() {
            message.push_str(&format!("\n\n{body}"));
        }
        // A pull request's own branch when it's in this repository (so fixes can
        // be pushed to it); GitHub's ref for one from a fork.
        let git_ref = it.pull_request.then(|| match &d.pull {
            Some(p) if p.head_repo.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(&it.repo)) => p.head.clone(),
            _ => format!("pull/{}/head", it.number),
        });
        let short_repo = it.repo.rsplit('/').next().unwrap_or(&it.repo);
        let plan = WorkspacePlan {
            repo: it.repo.clone(),
            git_ref,
            branch: (!it.pull_request).then(|| issue_branch(it.number, &it.title)),
            name: format!("{short_repo} #{}", it.number),
            preferred: pick.is_none(),
        };
        let short = short_title(&it.title, 48);
        let prefill = Prefill {
            title: Some(format!("#{} {short}", it.number)),
            message: Some(message),
            source: Some(format!("{}#{}", it.repo, it.number)),
            repo: Some(it.repo.clone()),
            command: None,
            workspace: Some(plan),
        };
        let (machine, cwd) = match pick {
            Some(c) => (Some(c.machine_id.clone()), Some(c.path.clone())),
            None => (home, None),
        };
        self.show_new_session_prefilled(machine, cwd, prefill, window, cx);
    }

    /// `gh_to_agent` for an issue or pull request in a list: loads it first if needed.
    pub(super) fn gh_item_to_agent(&mut self, repo: String, number: u64, workspace: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Load::Ready(d)) = self.gh.items.get(&(repo.clone(), number)) {
            let d = d.clone();
            return self.gh_to_agent(&d, workspace, window, cx);
        }
        let (engine, reference) = (self.engine.clone(), format!("{repo}#{number}"));
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { api::github_item(&engine, api::ItemRequest { reference }).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(api::ApiError::invalid(e.to_string())));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(d) => {
                    this.gh.items.insert((repo, number), Load::Ready(d.clone()));
                    this.gh_to_agent(&d, workspace, window, cx)
                }
                Err(e) => this.notify_user(e.message, true, cx),
            });
        });
        self.track(task);
    }

    // -----------------------------------------------------------------------
    // The page

    pub(super) fn render_github(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        self.gh_ensure(cx);
        if self.gh.filter.is_none() {
            let input = cx.new(|cx| TextInput::new(cx, "Filter repositories"));
            self._subscriptions.push(cx.subscribe_in(&input, window, |_, _, e: &InputEvent, _, cx| {
                if let InputEvent::Changed = e {
                    cx.notify()
                }
            }));
            self.gh.filter = Some(input);
        }
        let main = match self.route.clone() {
            Route::GithubRepo(name) => {
                self.gh_ensure_repo(&name, cx);
                self.gh_repo(&theme, &name, cx)
            }
            Route::GithubItem(repo, n) => {
                self.gh_ensure_item(&repo, n, cx);
                self.gh_item(&theme, &repo, n, cx)
            }
            _ => self.gh_inbox(&theme, cx),
        };
        div()
            .size_full()
            .flex()
            .child(self.gh_list(&theme, cx))
            .child(div().id("gh-main").flex_1().min_w(px(0.)).h_full().overflow_y_scroll().track_scroll(&self.page_scroll).px(px(28.)).pt(px(22.)).pb(px(40.)).child(main))
            .into_any_element()
    }

    /// Stars or unstars a repository (kept in settings, so agents see it too).
    pub(super) fn gh_set_favorite(&mut self, repo: &str, favorite: bool, cx: &mut Context<Self>) {
        if let Err(e) = api::github_favorite(&self.engine, api::FavoriteRequest { repo: repo.to_string(), favorite }) {
            self.notify_user(e.message, true, cx);
        }
        cx.notify();
    }

    pub(super) fn gh_is_favorite(&self, repo: &str) -> bool {
        self.engine.settings().favorite_repos.iter().any(|f| f.eq_ignore_ascii_case(repo))
    }

    /// The left column: you, the inbox, and your repositories (favorites
    /// first, then one list or grouped by owner).
    fn gh_list(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let login = match &self.gh.inbox {
            Some(Load::Ready(i)) => Some(i.viewer.login.clone()),
            _ => None,
        };
        let q = self.gh.filter.as_ref().map(|f| f.read(cx).text().trim().to_lowercase()).unwrap_or_default();
        let inbox_active = self.route == Route::Github;
        let waiting = self.gh.waiting();
        let (grouped, folded) = {
            let a = cx.global::<Appearance>();
            (a.github_group_by_owner, a.github_folded_owners.iter().map(|o| o.to_lowercase()).collect::<HashSet<_>>())
        };
        let favorites = self.engine.settings().favorite_repos;
        let is_favorite = |name: &str| favorites.iter().any(|f| f.eq_ignore_ascii_case(name));
        let matches = |name: &str| q.is_empty() || name.to_lowercase().contains(&q);

        let mut body: Vec<AnyElement> = Vec::new();
        match &self.gh.repos {
            Some(Load::Ready(repos)) => {
                let repos: Vec<&Repo> = repos.iter().filter(|r| !r.archived).collect();
                // Favorites, in the order they were added; one that isn't in the
                // list (someone else's repository) still shows.
                let favs: Vec<AnyElement> = favorites
                    .iter()
                    .filter(|f| matches(f))
                    .map(|f| {
                        let private = repos.iter().find(|r| r.full_name.eq_ignore_ascii_case(f)).is_some_and(|r| r.private);
                        self.gh_repo_row(theme, f, private, true, true, cx)
                    })
                    .collect();
                if !favs.is_empty() {
                    body.push(list_heading(theme, "Favorites", None));
                    body.extend(favs);
                }
                let rest: Vec<&Repo> = repos.iter().copied().filter(|r| !is_favorite(&r.full_name) && matches(&r.full_name)).take(300).collect();
                body.push(list_heading(theme, "Repositories", Some(self.gh_view_toggle(theme, grouped, cx))));
                body.push(div().px(px(12.)).pb(px(8.)).children(self.gh.filter.clone()).into_any_element());
                if rest.is_empty() {
                    let text = if q.is_empty() { "No other repositories." } else { "No repositories match." };
                    body.push(div().px(px(18.)).py(px(6.)).text_size(px(12.)).text_color(theme.text_placeholder).child(text).into_any_element());
                } else if grouped {
                    // You first, then your organizations A to Z, then what other
                    // people shared with you, together and folded until opened.
                    let you = login.clone().unwrap_or_default();
                    let owner_of = |r: &Repo| r.full_name.split_once('/').map_or("", |(o, _)| o).to_string();
                    let is_shared = |r: &Repo| !r.owner_is_org && !owner_of(r).eq_ignore_ascii_case(&you);
                    let mut owners: Vec<String> = Vec::new();
                    for r in rest.iter().filter(|r| !is_shared(r)) {
                        let owner = owner_of(r);
                        if !owners.iter().any(|o| o.eq_ignore_ascii_case(&owner)) {
                            owners.push(owner);
                        }
                    }
                    owners.sort_by_key(|o| (!o.eq_ignore_ascii_case(&you), o.to_lowercase()));
                    let mut groups: Vec<(String, String, Vec<&Repo>)> = owners
                        .into_iter()
                        .map(|o| {
                            let mine = rest.iter().copied().filter(|r| !is_shared(r) && owner_of(r).eq_ignore_ascii_case(&o)).collect();
                            (o.to_lowercase(), o, mine)
                        })
                        .collect();
                    let shared: Vec<&Repo> = rest.iter().copied().filter(|r| is_shared(r)).collect();
                    if !shared.is_empty() {
                        groups.push((SHARED.to_string(), "Shared with you".to_string(), shared));
                    }
                    for (key, title, repos) in groups {
                        // A search shows every match, folded or not.
                        let open = !q.is_empty() || group_open(&key, &folded);
                        let you = key == you.to_lowercase();
                        body.push(self.gh_owner_row(theme, &key, &title, repos.len(), open, you, cx));
                        if open {
                            // Shared repositories belong to different people: show whose.
                            let owner_line = key == SHARED;
                            body.extend(repos.iter().map(|r| self.gh_repo_row(theme, &r.full_name, r.private, owner_line, false, cx)));
                        }
                    }
                } else {
                    body.extend(rest.iter().map(|r| self.gh_repo_row(theme, &r.full_name, r.private, true, false, cx)));
                }
            }
            Some(Load::Failed(e)) => {
                body.push(list_heading(theme, "Repositories", None));
                body.push(div().px(px(18.)).text_size(px(12.)).text_color(theme.error).child(e.clone()).into_any_element());
            }
            _ => {
                body.push(list_heading(theme, "Repositories", None));
                body.push(loading(theme, "Loading repositories…"));
            }
        }

        div()
            .id("gh-list")
            .w(px(LIST_WIDTH))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme.border_variant)
            .pt(px(16.))
            .pb(px(20.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(10.))
                    .pr(px(18.))
                    .pb(px(12.))
                    .child(
                        // You, and a menu to switch accounts.
                        div()
                            .id("gh-account")
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .pl(px(8.))
                            .pr(px(10.))
                            .py(px(4.))
                            .rounded(px(8.))
                            .cursor_pointer()
                            .hover(|st| st.bg(theme.ghost_hover))
                            .child(kit::tile("agent-github", TILE.0, TILE.1, 28.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("GitHub"))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(4.))
                                            .text_size(px(11.5))
                                            .text_color(theme.text_placeholder)
                                            .child(login.map(|l| format!("@{l}")).unwrap_or_else(|| "Connecting…".into()))
                                            .child(icon("chevron-down", 11., theme.text_placeholder)),
                                    ),
                            )
                            .tooltip(|window, cx| kit::Tooltip::view("Switch account".into(), window, cx))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                                    this.gh_reload_accounts(cx);
                                    this.open_menu(MenuTarget::GithubAccounts, e.position, cx)
                                }),
                            ),
                    )
                    .child(div().flex_1())
                    .child(Button::icon_only("gh-refresh", "refresh-cw").tooltip("Refresh").on_click(cx.listener(|this, _, _, cx| this.gh_refresh(cx)))),
            )
            .child(
                div()
                    .id("gh-inbox-nav")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(30.))
                    .mx(px(10.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(if inbox_active { theme.text } else { theme.text_muted })
                    .when(inbox_active, |d| d.bg(theme.element_selected))
                    .when(!inbox_active, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                    .child(kit::color_tile("inbox", 18.))
                    .child("For you")
                    .child(div().flex_1())
                    .children(waiting.filter(|n| *n > 0).map(|n| kit::pill(n.to_string(), theme.accent)))
                    .on_click(cx.listener(|this, _, window, cx| this.navigate(Route::Github, window, cx))),
            )
            .child(div().flex().flex_col().gap(px(1.)).children(body))
            .into_any_element()
    }

    /// "One list | By owner".
    fn gh_view_toggle(&self, theme: &Theme, grouped: bool, cx: &mut Context<Self>) -> AnyElement {
        let option = |id: &'static str, glyph: &'static str, tip: &'static str, on: bool, value: bool| {
            div()
                .id(id)
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .when(on, |d| d.bg(theme.element_selected))
                .when(!on, |d| d.hover(|st| st.bg(theme.ghost_hover)))
                .child(icon(glyph, 12., if on { theme.text } else { theme.text_placeholder }))
                .tooltip(move |window, cx| kit::Tooltip::view(tip.into(), window, cx))
                .on_click(cx.listener(move |_, _, _, cx| {
                    remember(cx, |a| a.github_group_by_owner = value);
                    cx.notify()
                }))
        };
        div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(7.))
            .bg(theme.element)
            .child(option("gh-view-list", "list", "One list", !grouped, false))
            .child(option("gh-view-owner", "layers", "Group by owner", grouped, true))
            .into_any_element()
    }

    /// An owner's heading in the grouped list; click to fold it.
    fn gh_owner_row(&self, theme: &Theme, key: &str, title: &str, count: usize, open: bool, you: bool, cx: &mut Context<Self>) -> AnyElement {
        let key = key.to_string();
        div()
            .id(SharedString::from(format!("gh-owner-{key}")))
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .mt(px(4.))
            .mx(px(10.))
            .px(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .overflow_hidden()
            .hover(|st| st.bg(theme.ghost_hover))
            .child(icon(if open { "chevron-down" } else { "chevron-right" }, 12., theme.text_placeholder))
            .child(div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).text_color(theme.text_muted).child(title.to_string()))
            .when(you, |d| d.child(div().text_size(px(11.)).text_color(theme.text_placeholder).child("you")))
            .child(div().flex_1())
            .child(div().text_size(px(11.)).font_family(mono_font()).text_color(theme.text_placeholder).child(count.to_string()))
            .on_click(cx.listener(move |_, _, _, cx| {
                remember(cx, |a| {
                    let before = a.github_folded_owners.len();
                    a.github_folded_owners.retain(|o| o.to_lowercase() != key);
                    if a.github_folded_owners.len() == before {
                        a.github_folded_owners.push(key.clone());
                    }
                });
                cx.notify()
            }))
            .into_any_element()
    }

    /// A repository in the list: name (and owner), on-this-machine mark, and a
    /// star that shows on hover (always, once starred).
    fn gh_repo_row(&self, theme: &Theme, full: &str, private: bool, owner_line: bool, favorite: bool, cx: &mut Context<Self>) -> AnyElement {
        let (owner, name) = full.split_once('/').unwrap_or(("", full));
        let selected = match &self.route {
            Route::GithubRepo(r) | Route::GithubItem(r, _) => r.eq_ignore_ascii_case(full),
            _ => false,
        };
        let local = self.snapshot.checkouts.iter().any(|c| c.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(full)));
        // Margins, padding, the lock/book icon, gaps, the laptop and the star.
        let text_w = LIST_WIDTH - 20. - 16. - 13. - 8. - 20. - 28.;
        let (nav, star, menu) = (full.to_string(), full.to_string(), full.to_string());
        div()
            .id(SharedString::from(format!("gh-r-{full}")))
            .group("gh-repo-row")
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(if owner_line { 40. } else { 30. }))
            .mx(px(10.))
            .px(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::color_icon(theme, if private { "lock" } else { "book-open" }, 13., theme.text_placeholder))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().w(px(text_w)).truncate().text_size(px(13.)).text_color(theme.text).child(name.to_string()))
                    .when(owner_line, |d| d.child(div().w(px(text_w)).truncate().text_size(px(11.)).text_color(theme.text_placeholder).child(owner.to_string()))),
            )
            .child(div().flex_1())
            .when(local, |d| d.child(kit::color_icon(theme, "laptop", 12., theme.text_accent)))
            .child(
                div()
                    .id(SharedString::from(format!("gh-star-{full}")))
                    .flex_none()
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .hover(|st| st.bg(theme.ghost_hover))
                    .when(!favorite, |d| d.invisible().group_hover("gh-repo-row", |st| st.visible()))
                    .child(icon(if favorite { "star-filled" } else { "star" }, 12., if favorite { theme.warning } else { theme.text_placeholder }))
                    .tooltip(move |window, cx| kit::Tooltip::view(if favorite { "Remove from favorites" } else { "Add to favorites" }.into(), window, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.gh_set_favorite(&star, !favorite, cx)
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::GithubRepo(nav.clone()), window, cx)))
            .on_mouse_down(MouseButton::Right, menu::on_right_click(move || MenuTarget::Repo(menu.clone()), cx))
            .into_any_element()
    }

    fn gh_inbox(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let inbox = match &self.gh.inbox {
            Some(Load::Ready(i)) => i.clone(),
            Some(Load::Failed(e)) => return failed(theme, e),
            _ => return loading(theme, "Loading what needs you…"),
        };
        let mut body = div().flex().flex_col().gap(px(26.)).child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("For you"))
                .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(format!(
                    "Signed in as @{}. Pick an issue and hand it to an agent, or review what's waiting.",
                    inbox.viewer.login
                ))),
        );
        for (title, items, empty) in [
            ("Waiting for your review", &inbox.review_requests, "No pull requests are waiting for your review."),
            ("Assigned to you", &inbox.assigned, "Nothing is assigned to you."),
            ("Your pull requests", &inbox.authored, "You have no open pull requests."),
        ] {
            let rows: Vec<AnyElement> = items.iter().enumerate().map(|(i, it)| self.gh_item_row(theme, it, true, i, cx)).collect();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(section_title(theme, title, Some(items.len())))
                    .child(if rows.is_empty() {
                        card(theme).child(kit::empty_state(theme, "circle-check", empty)).into_any_element()
                    } else {
                        card(theme).children(rows).into_any_element()
                    }),
            );
        }
        body.into_any_element()
    }

    fn gh_item_row(&self, theme: &Theme, it: &Item, show_repo: bool, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let (glyph, color) = match (it.pull_request, it.draft) {
            (true, true) => ("git-pull-request-draft", theme.text_placeholder),
            (true, false) => ("git-pull-request", theme.success),
            (false, _) => ("circle-dot", theme.success),
        };
        let reference = if show_repo { format!("{}#{}", it.repo, it.number) } else { format!("#{}", it.number) };
        let (repo, number) = (it.repo.clone(), it.number);
        let target = (it.repo.clone(), it.number, it.pull_request, it.html_url.clone());
        div()
            .id(SharedString::from(format!("gh-i-{}-{}-{}", it.repo, it.number, i)))
            .flex()
            .items_start()
            .gap(px(12.))
            .px(px(14.))
            .py(px(10.))
            .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
            .cursor_pointer()
            .hover(|st| st.bg(theme.ghost_hover))
            .child(div().pt(px(2.)).child(icon(glyph, 15., color)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(4.))
                    .child(div().text_size(px(13.5)).font_weight(FontWeight::MEDIUM).text_color(theme.text).child(it.title.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .text_color(theme.text_placeholder)
                            .child(div().font_family(mono_font()).child(reference))
                            .child(format!("· {} · updated {}", it.author, format::ago(Some(it.updated_at))))
                            .children(it.labels.iter().take(4).map(|l| label(theme, l))),
                    ),
            )
            .when(it.comments > 0, |d| {
                d.child(div().flex().items_center().gap(px(4.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(kit::color_icon(theme, "message-square", 12., theme.text_placeholder)).child(it.comments.to_string()))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::GithubItem(repo.clone(), number), window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                menu::on_right_click(move || MenuTarget::Item { repo: target.0.clone(), number: target.1, pull: target.2, url: target.3.clone() }, cx),
            )
            .into_any_element()
    }

    fn gh_repo(&mut self, theme: &Theme, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let data = match self.gh.repo.get(name) {
            Some(Load::Ready(d)) => d,
            Some(Load::Failed(e)) => return failed(theme, e),
            _ => return loading(theme, "Loading the repository…"),
        };
        let r = data.repo.clone();
        let issues = data.issues.clone();
        let pulls = data.pulls.clone();
        let runs = data.runs.clone();
        // Fresh from the machines, not the moment the repository loaded.
        let checkouts: Vec<Checkout> = self.snapshot.checkouts.iter().filter(|c| c.repo.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(name))).cloned().collect();

        let meta: Vec<String> = [
            Some(if r.private { "Private".to_string() } else { "Public".into() }),
            Some(r.default_branch.clone()),
            r.language.clone(),
            (r.stars > 0).then(|| format!("★ {}", r.stars)),
            r.pushed_at.map(|t| format!("pushed {}", format::ago(Some(t)))),
        ]
        .into_iter()
        .flatten()
        .collect();
        let url = r.html_url.clone();
        let first = checkouts.first().cloned();
        let favorite = self.gh_is_favorite(&r.full_name);
        let fav_name = r.full_name.clone();
        let header = div()
            .flex()
            .items_start()
            .gap(px(14.))
            .child(kit::tile("agent-github", TILE.0, TILE.1, 40.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(4.))
                    .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(r.full_name.clone()))
                    .children(r.description.clone().map(|d| div().text_size(px(13.)).text_color(theme.text_muted).child(d)))
                    .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(meta.join("  ·  "))),
            )
            .child(
                Button::new("gh-repo-fav", if favorite { "Favorite" } else { "Add to favorites" })
                    .icon(if favorite { "star-filled" } else { "star" })
                    .tooltip(if favorite { "Remove from favorites" } else { "Show it first in the list" })
                    .on_click(cx.listener(move |this, _, _, cx| this.gh_set_favorite(&fav_name, !favorite, cx))),
            )
            .children(first.map(|c| {
                Button::new("gh-repo-session", "New session")
                    .icon("square-terminal")
                    .style(ButtonStyle::Primary)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let prefill = Prefill { repo: Some(c.repo.clone().unwrap_or_default()), ..Default::default() };
                        this.show_new_session_prefilled(Some(c.machine_id.clone()), Some(c.path.clone()), prefill, window, cx)
                    }))
            }))
            .child(Button::new("gh-repo-open", "Open on GitHub").icon("external-link").on_click(move |_, _, cx| cx.open_url(&url)));

        let tab = self.gh.tab;
        let tab_button = |t: RepoTab, label: String, cx: &mut Context<Self>| {
            let active = tab == t;
            div()
                .id(SharedString::from(label.clone()))
                .h(px(28.))
                .px(px(12.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(if active { theme.text } else { theme.text_muted })
                .when(active, |d| d.bg(theme.surface).border_1().border_color(theme.border).shadow_sm())
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.gh.tab = t;
                    cx.notify()
                }))
        };
        let tabs = div()
            .flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(9.))
            .bg(theme.element)
            .border_1()
            .border_color(theme.border_variant)
            .child(tab_button(RepoTab::Issues, format!("Issues  {}", issues.len()), cx))
            .child(tab_button(RepoTab::Pulls, format!("Pull requests  {}", pulls.len()), cx))
            .child(tab_button(RepoTab::Actions, "Actions".into(), cx))
            .child(tab_button(RepoTab::Checkouts, format!("On your machines  {}", checkouts.len()), cx));

        let content = match tab {
            RepoTab::Issues | RepoTab::Pulls => {
                let list = if tab == RepoTab::Issues { &issues } else { &pulls };
                let rows: Vec<AnyElement> = list.iter().enumerate().map(|(i, it)| self.gh_item_row(theme, it, false, i, cx)).collect();
                if rows.is_empty() {
                    card(theme).child(kit::empty_state(theme, "circle-check", if tab == RepoTab::Issues { "No open issues." } else { "No open pull requests." })).into_any_element()
                } else {
                    card(theme).children(rows).into_any_element()
                }
            }
            RepoTab::Actions => runs_list(theme, &runs),
            RepoTab::Checkouts => self.gh_checkouts(theme, name, &checkouts, cx),
        };
        div().flex().flex_col().gap(px(20.)).child(header).child(tabs).child(content).into_any_element()
    }

    fn gh_checkouts(&mut self, theme: &Theme, repo: &str, checkouts: &[Checkout], cx: &mut Context<Self>) -> AnyElement {
        if checkouts.is_empty() {
            return card(theme)
                .child(kit::empty_state(
                    theme,
                    "laptop",
                    format!("{repo} isn't checked out in your code folders on any machine. Agents you start from an issue can clone it."),
                ))
                .into_any_element();
        }
        let rows: Vec<AnyElement> = checkouts
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let machine = self.snapshot.machines.iter().find(|m| m.id == c.machine_id);
                let machine_name = machine.map(|m| m.name.clone()).unwrap_or_else(|| c.machine_id.clone());
                let (from, to) = machine.map(format::machine_tile).unwrap_or(kit::tiles::OTHER);
                let icon_name = machine.map(format::machine_icon).unwrap_or("server");
                let sync = match (c.upstream.is_some(), c.ahead, c.behind) {
                    (false, _, _) => "no upstream".to_string(),
                    (true, 0, 0) => "up to date".into(),
                    (true, a, 0) => format!("↑ {a} to push"),
                    (true, 0, b) => format!("↓ {b} to pull"),
                    (true, a, b) => format!("↑ {a}  ↓ {b}"),
                };
                let busy = self.gh.git_busy.contains(&format!("{}:{}", c.machine_id, c.path));
                let (pull, push, fetch, term) = (c.clone(), c.clone(), c.clone(), c.clone());
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(10.))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .child(kit::tile(icon_name, from, to, 26.))
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
                                    .text_size(px(13.))
                                    .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.text).child(machine_name))
                                    .child(div().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_muted).child(c.path.clone())),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .text_size(px(11.5))
                                    .text_color(theme.text_placeholder)
                                    .child(kit::color_icon(theme, "git-branch", 11., theme.text_placeholder))
                                    .child(div().font_family(mono_font()).child(c.branch.clone()))
                                    .child(format!("· {sync}"))
                                    .child(if c.dirty == 0 { "· clean".to_string() } else { format!("· {} changed", c.dirty) }),
                            ),
                    )
                    .when(busy, |d| d.child(spinner(SharedString::from(format!("gh-git-{i}")), 13., theme.accent)))
                    .child(Button::new(("gh-fetch", i), "Fetch").disabled(busy).on_click(cx.listener(move |this, _, _, cx| this.gh_git(&fetch, "fetch", cx))))
                    .child(Button::new(("gh-pull", i), "Pull").icon("arrow-down").disabled(busy || c.behind == 0).on_click(cx.listener(move |this, _, _, cx| this.gh_git(&pull, "pull", cx))))
                    .child(Button::new(("gh-push", i), "Push").icon("arrow-up").disabled(busy || c.ahead == 0).on_click(cx.listener(move |this, _, _, cx| this.gh_git(&push, "push", cx))))
                    .child(Button::icon_only(("gh-term", i), "square-terminal").tooltip("New session here").on_click(cx.listener(move |this, _, window, cx| {
                        let prefill = Prefill { repo: term.repo.clone(), ..Default::default() };
                        this.show_new_session_prefilled(Some(term.machine_id.clone()), Some(term.path.clone()), prefill, window, cx)
                    })))
                    .into_any_element()
            })
            .collect();
        card(theme).children(rows).into_any_element()
    }

    fn gh_item(&mut self, theme: &Theme, repo: &str, number: u64, cx: &mut Context<Self>) -> AnyElement {
        let d = match self.gh.items.get(&(repo.to_string(), number)) {
            Some(Load::Ready(d)) => d.clone(),
            Some(Load::Failed(e)) => return failed(theme, e),
            _ => return loading(theme, "Loading…"),
        };
        let it = d.item.clone();
        let width = (self.content_width + 56. - LIST_WIDTH - 56. - 34.).max(320.);
        let (state_label, state_color) = match (it.state.as_str(), it.pull_request, it.draft) {
            ("open", true, true) => ("Draft", theme.text_placeholder),
            ("open", _, _) => ("Open", theme.success),
            _ => ("Closed", theme.text_placeholder),
        };
        let repo_name = it.repo.clone();
        let (url, url2) = (it.html_url.clone(), it.html_url.clone());
        let detail = d.clone();
        let actions = div()
            .flex()
            .flex_wrap()
            .gap(px(8.))
            .child(
                Button::new("gh-agent", if it.pull_request { "Review with agent" } else { "Fix with agent" })
                    .icon("bot")
                    .style(ButtonStyle::Primary)
                    .on_click(cx.listener(move |this, _, window, cx| this.gh_to_agent(&detail, false, window, cx))),
            )
            .child(Button::new("gh-open", "Open on GitHub").icon("external-link").on_click(move |_, _, cx| cx.open_url(&url)))
            .child(Button::new("gh-copy", "Copy link").icon("copy").style(ButtonStyle::Ghost).on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(url2.clone()));
                this.notify_user("Copied the link.", false, cx);
            })));

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .id("gh-back")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .hover(|st| st.text_color(theme.text))
                    .child(icon("arrow-left", 13., theme.text_placeholder))
                    .child(repo_name.clone())
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::GithubRepo(repo_name.clone()), window, cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().w(px(width)).text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(format!("{}  #{}", it.title, it.number)))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .text_color(theme.text_placeholder)
                            .child(kit::pill(state_label, state_color))
                            .child(format!("{} opened {} · {} comments", it.author, format::ago(Some(it.created_at)), it.comments))
                            .children(it.labels.iter().map(|l| label(theme, l))),
                    ),
            )
            .child(actions);

        if let Some(p) = &d.pull {
            let checks = if p.checks.total == 0 {
                "No checks".to_string()
            } else {
                format!("{} passed · {} failed · {} pending", p.checks.passed, p.checks.failed, p.checks.pending)
            };
            let reviews = if p.reviews.is_empty() {
                "No reviews yet".to_string()
            } else {
                p.reviews.iter().map(|r| format!("{} {}", r.author, r.state.to_lowercase().replace('_', " "))).collect::<Vec<_>>().join(" · ")
            };
            body = body.child(
                card(theme)
                    .px(px(16.))
                    .py(px(8.))
                    .child(kit::kv(theme, "Branches", div().font_family(mono_font()).child(format!("{} → {}", p.head, p.base))))
                    .child(kit::kv(theme, "Size", div().child(format!("+{} −{} in {} files", p.additions, p.deletions, p.changed_files))))
                    .child(kit::kv(
                        theme,
                        "Checks",
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_color(if p.checks.failed > 0 { theme.error } else { theme.text }).child(checks))
                            .children((!p.checks.failing.is_empty()).then(|| div().text_size(px(11.5)).text_color(theme.text_muted).child(format!("Failing: {}", p.checks.failing.join(", "))))),
                    ))
                    .child(kit::kv(theme, "Reviews", div().child(reviews)))
                    .child(kit::kv(theme, "Mergeable", div().child(match p.mergeable {
                        Some(true) => "Yes",
                        Some(false) => "No (conflicts)",
                        None => "Checking…",
                    }))),
            );
        }
        body = body.child(card(theme).p(px(16.)).child(markdown(theme, &d.body, width - 32.)));
        for (i, c) in d.comments.iter().enumerate() {
            body = body.child(
                card(theme)
                    .p(px(16.))
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(c.author.clone()))
                            .child(div().text_color(theme.text_placeholder).child(format::ago(Some(c.created_at))))
                            .child(div().flex_1())
                            .child(div().text_size(px(11.)).text_color(theme.text_placeholder).child(format!("#{}", i + 1))),
                    )
                    .child(markdown(theme, &c.body, width - 32.)),
            );
        }
        body.into_any_element()
    }
}

fn runs_list(theme: &Theme, runs: &[Run]) -> AnyElement {
    if runs.is_empty() {
        return card(theme).child(kit::empty_state(theme, "play", "No GitHub Actions runs.")).into_any_element();
    }
    let rows: Vec<AnyElement> = runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (glyph, color) = match (r.status.as_str(), r.conclusion.as_deref()) {
                ("completed", Some("success")) => ("circle-check", theme.success),
                ("completed", Some("failure" | "timed_out" | "startup_failure")) => ("circle-x", theme.error),
                ("completed", _) => ("circle", theme.text_placeholder),
                _ => ("clock", theme.warning),
            };
            let url = r.html_url.clone();
            div()
                .id(("gh-run", i))
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(9.))
                .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                .cursor_pointer()
                .hover(|st| st.bg(theme.ghost_hover))
                .child(icon(glyph, 15., color))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .child(div().text_size(px(13.)).text_color(theme.text).child(if r.title.is_empty() { r.name.clone() } else { r.title.clone() }))
                        .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!(
                            "{} · {} · {} · {}",
                            r.name,
                            r.branch,
                            r.event,
                            format::ago(Some(r.created_at))
                        ))),
                )
                .child(kit::color_icon(theme, "external-link", 12., theme.text_placeholder))
                .on_click(move |_, _, cx| cx.open_url(&url))
                .into_any_element()
        })
        .collect();
    card(theme).children(rows).into_any_element()
}

/// A title cut to `max` characters at a word, with an ellipsis.
fn short_title(title: &str, max: usize) -> String {
    let title = title.trim();
    if title.chars().count() <= max {
        return title.to_string();
    }
    let cut: String = title.chars().take(max).collect();
    let at_word = cut.rsplit_once(' ').map(|(a, _)| a).filter(|a| a.chars().count() > max / 2).unwrap_or(&cut);
    format!("{}…", at_word.trim_end_matches([' ', ',', '.', ':', ';', '-']))
}

/// A branch name for fixing an issue: "issue-910-content-excalidraw-searched".
fn issue_branch(number: u64, title: &str) -> String {
    const SKIP: &[&str] = &["a", "an", "the", "to", "of", "in", "on", "for", "and", "or", "is", "be", "can", "cannot", "feature", "bug", "request", "feat", "fix"];
    let words: Vec<String> = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|w| !SKIP.contains(&w.as_str()))
        .take(4)
        .collect();
    let mut branch = format!("issue-{number}");
    for w in words {
        if branch.len() + w.len() + 1 > 48 {
            break;
        }
        branch.push('-');
        branch.push_str(&w);
    }
    branch
}

/// A small heading in the repository list, with an optional control at the right.
fn list_heading(theme: &Theme, text: &'static str, control: Option<AnyElement>) -> AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(26.))
        .px(px(18.))
        .mt(px(12.))
        .mb(px(4.))
        .text_size(px(11.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_placeholder)
        .child(text)
        .child(div().flex_1())
        .children(control)
        .into_any_element()
}

fn loading(theme: &Theme, what: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .p(px(16.))
        .text_size(px(12.5))
        .text_color(theme.text_muted)
        .child(spinner(SharedString::from(what), 13., theme.text_muted))
        .child(what)
        .into_any_element()
}

fn failed(theme: &Theme, error: &str) -> AnyElement {
    card(theme)
        .p(px(16.))
        .flex_row()
        .items_start()
        .gap(px(10.))
        .child(icon("circle-alert", 15., theme.error))
        .child(div().flex_1().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text).child(error.to_string()))
        .into_any_element()
}

/// GitHub-flavored Markdown, simplified for reading: paragraphs, headings,
/// lists, quotes and code blocks; links and emphasis as plain text; images
/// and HTML comments left out.
fn markdown(theme: &Theme, text: &str, width: f32) -> AnyElement {
    let text = strip_comments(text);
    let mut blocks: Vec<AnyElement> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut code: Option<Vec<String>> = None;
    let flush = |paragraph: &mut Vec<String>, blocks: &mut Vec<AnyElement>| {
        if !paragraph.is_empty() {
            let text = paragraph.join(" ");
            blocks.push(div().w(px(width)).text_size(px(13.)).text_color(theme.text).child(inline(&text)).into_any_element());
            paragraph.clear();
        }
    };
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match code.take() {
                Some(lines) => blocks.push(
                    div()
                        .w(px(width))
                        .p(px(10.))
                        .rounded(px(6.))
                        .bg(theme.element)
                        .font_family(mono_font())
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .children(lines.into_iter().map(|l| div().child(if l.is_empty() { " ".to_string() } else { l })))
                        .into_any_element(),
                ),
                None => {
                    flush(&mut paragraph, &mut blocks);
                    code = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(lines) = code.as_mut() {
            lines.push(line.to_string());
            continue;
        }
        let t = line.trim();
        if t.is_empty() {
            flush(&mut paragraph, &mut blocks);
        } else if let Some(h) = ["###### ", "##### ", "#### ", "### ", "## ", "# "].iter().find_map(|p| t.strip_prefix(p)) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(div().w(px(width)).pt(px(4.)).text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(inline(h)).into_any_element());
        } else if let Some(item) = list_item(t) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(
                div()
                    .w(px(width))
                    .flex()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(div().flex_none().text_color(theme.text_placeholder).child(item.0))
                    .child(div().flex_1().min_w(px(0.)).child(inline(&item.1)))
                    .into_any_element(),
            );
        } else if let Some(q) = t.strip_prefix('>') {
            flush(&mut paragraph, &mut blocks);
            blocks.push(
                div()
                    .w(px(width))
                    .pl(px(10.))
                    .border_l_2()
                    .border_color(theme.border)
                    .text_size(px(13.))
                    .text_color(theme.text_muted)
                    .child(inline(q.trim()))
                    .into_any_element(),
            );
        } else {
            paragraph.push(t.to_string());
        }
    }
    if let Some(lines) = code {
        blocks.push(div().w(px(width)).font_family(mono_font()).text_size(px(12.)).children(lines.into_iter().map(|l| div().child(l))).into_any_element());
    }
    flush(&mut paragraph, &mut blocks);
    if blocks.is_empty() {
        blocks.push(div().text_size(px(12.5)).text_color(theme.text_placeholder).child("No description.").into_any_element());
    }
    div().flex().flex_col().gap(px(8.)).children(blocks).into_any_element()
}

fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// "- [x] done" -> ("☑", "done"), "1. step" -> ("1.", "step"), "* x" -> ("•", "x").
fn list_item(t: &str) -> Option<(String, String)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return Some(match rest {
                r if r.starts_with("[ ] ") => ("☐".into(), r[4..].to_string()),
                r if r.starts_with("[x] ") || r.starts_with("[X] ") => ("☑".into(), r[4..].to_string()),
                r => ("•".into(), r.to_string()),
            });
        }
    }
    let (num, rest) = t.split_once(". ")?;
    (num.len() <= 3 && num.chars().all(|c| c.is_ascii_digit())).then(|| (format!("{num}."), rest.to_string()))
}

/// Links as their text, images as their description, emphasis and code marks dropped.
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let image = chars[i] == '!' && chars.get(i + 1) == Some(&'[');
        if chars[i] == '[' || image {
            let open = if image { i + 1 } else { i };
            if let Some(close) = chars[open..].iter().position(|c| *c == ']').map(|p| open + p) {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = chars[close..].iter().position(|c| *c == ')').map(|p| close + p) {
                        let label: String = chars[open + 1..close].iter().collect();
                        if image {
                            out.push_str(&format!("[image{}]", if label.is_empty() { String::new() } else { format!(": {label}") }));
                        } else {
                            out.push_str(&label);
                        }
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        if chars[i] == '<' {
            // Inline HTML (<br>, <img ...>, <details>): drop the tag.
            if let Some(end) = chars[i..].iter().position(|c| *c == '>') {
                let tag: String = chars[i..i + end + 1].iter().collect();
                if tag.starts_with("<img") {
                    out.push_str("[image]");
                }
                i += end + 1;
                continue;
            }
        }
        if chars[i] == '`' {
            // Inline code stays as written (it may contain <angle brackets>).
            if let Some(end) = chars[i + 1..].iter().position(|c| *c == '`').map(|p| i + 1 + p) {
                out.extend(&chars[i + 1..end]);
                i = end + 1;
                continue;
            }
        }
        if matches!(chars[i], '*' | '`') || (chars[i] == '_' && chars.get(i + 1) == Some(&'_')) {
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_titles_at_words() {
        assert_eq!(short_title("[Feature] The content in excalidraw cannot be searched globally.", 48), "[Feature] The content in excalidraw cannot be…");
        assert_eq!(short_title("Short one", 48), "Short one");
        assert_eq!(short_title(&"x".repeat(60), 48), format!("{}…", "x".repeat(48)));
    }

    #[test]
    fn names_issue_branches() {
        assert_eq!(issue_branch(910, "[Feature] The content in excalidraw cannot be searched globally."), "issue-910-content-excalidraw-searched-globally");
        assert_eq!(issue_branch(7, "支持中文"), "issue-7");
        assert!(issue_branch(1, &"averyveryverylongwordindeed ".repeat(5)).len() <= 48);
    }

    #[test]
    fn simplifies_markdown() {
        assert_eq!(inline("See [the docs](https://x.y) and **bold** `code`"), "See the docs and bold code");
        assert_eq!(inline("commands like `:tag <tag_name>` work<br>"), "commands like :tag <tag_name> work");
        assert_eq!(inline("![screenshot](a.png)"), "[image: screenshot]");
        assert_eq!(list_item("- [x] done"), Some(("☑".into(), "done".into())));
        assert_eq!(list_item("12. step"), Some(("12.".into(), "step".into())));
        assert_eq!(strip_comments("a<!-- hidden -->b"), "ab");
    }
}
