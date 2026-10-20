//! Agent sessions in the window: the session page (a live terminal attached to
//! the session through the machine's agent, with its controls), starting new sessions, and the
//! sidebar rows that list them under their machine.

use std::sync::Arc;
use std::time::Duration;

use gpui::{AnyElement, ClipboardItem, Context, Focusable, FontWeight, SharedString, Window, div, prelude::*, px};

use super::dialogs::{ConfirmDialog, NewSessionDialog};
use super::format;
use super::kit::{self, Button, ButtonStyle, card, icon, section_title, spinner};
use super::panes::Panes;
use super::root::{Modal, Root, Route};
use super::terminal::{TerminalEvent, TerminalView};
use super::theme::{ActiveTheme, Theme, mono_font};
use crate::fleet::model::{Machine, Snapshot};
use crate::fleet::model::Machine as FleetMachine;
use crate::fleet::sessions::git::GitStatus;
use crate::fleet::sessions::{AGENTS, Control, SessionInfo, StartSession};

/// Toolbar plus the session header, subtracted when sizing a new session.
const CHROME_HEIGHT: f32 = 52. + 60.;

impl Root {
    pub(super) fn show_new_session(&mut self, machine: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.show_new_session_with(machine, None, window, cx);
    }

    /// The New Session dialog, set up by whatever opened it (a folder, a title,
    /// a first message for the agent, or a command to run).
    pub(super) fn show_new_session_prefilled(
        &mut self,
        machine: Option<String>,
        cwd: Option<String>,
        prefill: super::dialogs::Prefill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_new_session_with(machine, None, window, cx);
        if let Some(Modal::NewSession(d)) = &self.modal {
            d.update(cx, |d, cx| d.prefill(cwd, prefill, cx));
        }
    }

    /// A session in a workspace's folder; for a Docker workspace, a shell in its container.
    pub(super) fn new_workspace_session(&mut self, w: &crate::fleet::model::WorkspaceInfo, window: &mut Window, cx: &mut Context<Self>) {
        let prefill = match w.container.clone().filter(|_| w.isolation == "docker") {
            Some(c) => super::dialogs::Prefill {
                title: Some(format!("{} (container)", w.name)),
                command: Some((
                    format!(
                        "docker start {c} >/dev/null 2>&1; exec docker exec -it -w /workspace {c} sh -c 'if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh -l; fi'"
                    ),
                    format!("A shell inside the workspace's container ({}).", w.image.clone().unwrap_or_else(|| "Docker".into())),
                )),
                ..Default::default()
            },
            None => super::dialogs::Prefill { title: Some(w.name.clone()), ..Default::default() },
        };
        self.show_new_session_prefilled(Some(w.machine_id.clone()), Some(w.path.clone()), prefill, window, cx);
    }

    /// The New Session dialog, optionally with the agent already chosen.
    pub(super) fn show_new_session_with(&mut self, machine: Option<String>, agent: Option<&'static str>, window: &mut Window, cx: &mut Context<Self>) {
        // Learn what's installed while the dialog is open.
        if let Some(id) = machine.clone() {
            let engine = self.engine.clone();
            engine.runtime().clone().spawn(async move {
                let _ = engine.refresh_sessions(&id).await;
            });
        }
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| {
            let d = NewSessionDialog::new(engine, machine, window, cx);
            match agent {
                Some(a) => d.with_agent(a),
                None => d,
            }
        });
        self.show_modal(Modal::NewSession(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    /// Starts a session sized to the page, then opens it.
    /// Columns and rows a session's terminal gets in this window (its share of
    /// the width when the desktop is beside it).
    fn terminal_fit(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> (u16, u16) {
        match self.pane_layout(id).and_then(|p| p.share(id)) {
            Some(share) => self.fit_share(id, share, window, cx),
            None => self.fit_area(id, 1., 1., window, cx),
        }
    }

    /// The size of a pane with this share of the page (under its title bar).
    fn fit_share(&self, id: &str, (w, h): (f32, f32), window: &mut Window, cx: &mut Context<Self>) -> (u16, u16) {
        self.fit_area(id, w, h, window, cx)
    }

    fn fit_area(&self, id: &str, w: f32, h: f32, window: &mut Window, cx: &mut Context<Self>) -> (u16, u16) {
        let viewport = window.viewport_size();
        let sidebar = if self.sidebar_hidden { 0. } else { self.sidebar_width };
        let mut width = f32::from(viewport.width) - sidebar;
        if self.page_split(id).is_some() {
            width *= self.split_ratio;
        }
        let height = f32::from(viewport.height) - CHROME_HEIGHT;
        // A pane: its share, less its border and title bar.
        let (width, height) = if (w, h) == (1., 1.) && self.pane_layout(id).is_none() { (width, height) } else { (width * w - 2., height * h - 26.) };
        TerminalView::fit(width, height, window, cx)
    }

    pub(super) fn start_session(&mut self, req: StartSession, window: &mut Window, cx: &mut Context<Self>) {
        self.start_session_then(req, None, window, cx);
    }

    /// Starts a session, then shows `then` (what to do in it), if any.
    pub(super) fn start_session_then(&mut self, mut req: StartSession, then: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let viewport = window.viewport_size();
        let (cols, rows) = TerminalView::fit(f32::from(viewport.width) - self.sidebar_width, f32::from(viewport.height) - CHROME_HEIGHT, window, cx);
        req.cols = cols;
        req.rows = rows;
        let agent = crate::fleet::sessions::agent(&req.agent).map(|a| a.name).unwrap_or("Session");
        let machine = self.snapshot.machines.iter().find(|m| m.id == req.machine_id).map(|m| m.name.clone()).unwrap_or_default();
        self.notify_user(format!("Starting {agent} on {machine}…"), false, cx);
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.start_session(req).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(s) => {
                    this.notice = None;
                    this.navigate(Route::Session(s.id), window, cx);
                    if let Some(t) = then {
                        this.notify_user(t, false, cx);
                    }
                }
                Err(e) => this.notify_user(e, true, cx),
            });
        });
        self.track(task);
    }

    /// Attaches a terminal to the session, unless one is already attached.
    pub(super) fn open_terminal(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Keys go to the session the page is on (the focused pane of a split).
        let shown = self.route == Route::Session(id.to_string());
        if let Some(t) = self.terminals.get(id) {
            if !t.read(cx).exited() {
                if shown {
                    window.focus(&t.focus_handle(cx));
                }
                return;
            }
        }
        let persistent = self.snapshot.sessions.iter().find(|s| s.id == id).is_none_or(|s| s.persistent);
        if !persistent {
            self.engine.set_direct_status(id, "running");
        }
        let mut failed = None;
        let (machine_id, name) = id.split_once('/').unwrap_or((id, ""));
        // Over the machine's live link when it's up: no ssh of its own.
        let link = persistent.then(|| crate::fleet::link::current(machine_id)).flatten();
        let view = if self.engine.is_demo() {
            let agent = self.snapshot.sessions.iter().find(|s| s.id == id).map(|s| s.agent.clone()).unwrap_or_default();
            cx.new(|cx| TerminalView::replay(&demo_screen(&agent), window, cx))
        } else if let Some(link) = link {
            // The size it will have, so the session is redrawn for it exactly.
            let size = self.terminal_fit(id, window, cx);
            let (io, output) = attach_over_link(&self.engine, link, name, size);
            cx.new(|cx| TerminalView::remote(io, output, size, window, cx))
        } else {
            let command = self.engine.attach_command(id);
            cx.new(|cx| {
                let spawned = command.and_then(|c| TerminalView::spawn(c.program, c.args, window, cx));
                spawned.unwrap_or_else(|e| {
                    failed = Some(e);
                    TerminalView::replay(b"", window, cx)
                })
            })
        };
        if let Some(e) = failed {
            self.notify_user(e, true, cx);
            return;
        }
        // When the attach ends (detached, the session ended, the network dropped), re-read the machine.
        let machine = id.split('/').next().unwrap_or_default().to_string();
        let sid = id.to_string();
        let shell = self.snapshot.sessions.iter().find(|s| s.id == id).is_some_and(|s| s.agent == "shell");
        self._subscriptions.push(cx.subscribe(&view, move |this, _, event: &TerminalEvent, cx| match event {
            TerminalEvent::Exited(code) => {
                // A session without the agent ends with its terminal: a shell that
                // exits or an agent that quits cleanly closes; a crash stays.
                if !persistent {
                    if shell || *code == Some(0) {
                        this.engine.remove_direct(&sid);
                    } else {
                        this.engine.set_direct_status(&sid, "exited");
                    }
                }
                this.refresh_machine_sessions(&machine);
                cx.notify();
            }
        }));
        if shown {
            window.focus(&view.focus_handle(cx));
        }
        self.terminals.insert(id.to_string(), view);
        self.refresh_machine_sessions(id.split('/').next().unwrap_or_default());
    }

    // -----------------------------------------------------------------------
    // Panes: terminals split side by side or one above the other (panes.rs).

    /// The layout a session is in, if it's split.
    pub(super) fn pane_layout(&self, id: &str) -> Option<&Panes> {
        self.panes.iter().find(|p| p.contains(id))
    }

    /// The session of a page whose desktop is shown beside it: `id`, or
    /// another pane of its layout.
    pub(super) fn page_split(&self, id: &str) -> Option<String> {
        match self.pane_layout(id) {
            Some(p) => p.ids().into_iter().find(|x| self.split.contains(*x)).map(str::to_string),
            None => self.split.contains(id).then(|| id.to_string()),
        }
    }

    fn save_panes(&self, cx: &mut Context<Self>) {
        let saved: Vec<String> = self.panes.iter().map(Panes::encode).collect();
        super::theme::remember(cx, |a| a.panes = saved);
    }

    /// Splits the shown session's pane: a new shell on its machine, in its
    /// folder, to its right or below it.
    pub(super) fn split_session(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Route::Session(id) = self.route.clone() else { return };
        let Some(info) = self.snapshot.sessions.iter().find(|s| s.id == id).cloned() else { return };
        if !info.persistent || self.engine.is_demo() {
            self.notify_user("This session can't be split.", true, cx);
            return;
        }
        // Exactly the size its pane will have (half of this one), so the shell
        // isn't resized while it starts (zsh would mark its prompts with a stray %).
        let (w, h) = self.pane_layout(&id).and_then(|p| p.share(&id)).unwrap_or((1., 1.));
        let share = if down { (w, h / 2.) } else { (w / 2., h) };
        let (cols, rows) = self.fit_share(&id, share, window, cx);
        let req = StartSession { machine_id: info.machine_id.clone(), agent: "shell".into(), cwd: info.cwd.clone(), cols, rows, ..Default::default() };
        let engine = self.engine.clone();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.start_session(req).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(s) => {
                    match this.panes.iter_mut().find(|p| p.contains(&id)) {
                        Some(p) => {
                            p.split(&id, s.id.clone(), down);
                        }
                        None => {
                            let mut p = Panes::One(id.clone());
                            p.split(&id, s.id.clone(), down);
                            this.panes.push(p);
                        }
                    }
                    this.save_panes(cx);
                    this.focus_pane(&s.id, window, cx);
                }
                Err(e) => this.notify_user(e, true, cx),
            });
        });
        self.track(task);
    }

    /// Shows another pane of the layout: keys go there.
    pub(super) fn focus_pane(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let route = Route::Session(id.to_string());
        if self.route != route {
            // The same page: no step in Back.
            self.route = route;
            self.on_route_changed(window, cx);
        }
    }

    /// The next (or previous) pane of the shown layout.
    pub(super) fn step_pane(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Route::Session(id) = self.route.clone() else { return };
        let Some(ids) = self.pane_layout(&id).map(|p| p.ids().into_iter().map(str::to_string).collect::<Vec<_>>()) else { return };
        let at = ids.iter().position(|x| *x == id).unwrap_or(0) as isize;
        let next = ids[(at + step).rem_euclid(ids.len() as isize) as usize].clone();
        self.focus_pane(&next, window, cx);
    }

    /// Takes a pane out of its layout; the session goes on (in the sidebar).
    fn close_pane(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.panes.iter().position(|p| p.contains(id)) else { return };
        let layout = self.panes.remove(i);
        let rest = layout.without(id);
        let next = rest.as_ref().and_then(|p| p.ids().first().map(|x| x.to_string()));
        if let Some(rest) = rest.filter(|p| p.ids().len() > 1) {
            self.panes.insert(i, rest);
        }
        self.save_panes(cx);
        if self.route == Route::Session(id.to_string())
            && let Some(next) = next
        {
            self.focus_pane(&next, window, cx);
        }
        cx.notify();
    }

    /// Sessions that ended leave their layouts (once their machine has been
    /// asked, so nothing goes while sessions are still being found).
    pub(super) fn prune_panes(&mut self, snapshot: &Snapshot, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let gone = |id: &str| {
            let machine = id.split('/').next().unwrap_or_default();
            !snapshot.sessions.iter().any(|s| s.id == id) && engine.sessions_of(machine).is_some_and(|f| f.checked_at.is_some())
        };
        let before = self.panes.clone();
        self.panes = std::mem::take(&mut self.panes)
            .into_iter()
            .filter_map(|p| {
                let ids: Vec<String> = p.ids().into_iter().map(str::to_string).collect();
                ids.iter().filter(|id| gone(id)).try_fold(p, |p, id| p.without(id))
            })
            .filter(|p| p.ids().len() > 1)
            .collect();
        if self.panes != before {
            self.save_panes(cx);
        }
    }

    /// Re-attaches a terminal whose connection dropped, a few times over about
    /// ten seconds. True while it's still trying.
    fn resume_terminal(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        const TRIES: u32 = 8;
        let (tries, waiting) = self.resuming.entry(id.to_string()).or_insert((0, false));
        if *tries >= TRIES {
            return false;
        }
        if *waiting {
            return true;
        }
        *tries += 1;
        *waiting = true;
        let delay = Duration::from_millis(300 * *tries as u64).min(Duration::from_secs(2));
        let (engine, sid) = (self.engine.clone(), id.to_string());
        let machine = id.split('/').next().unwrap_or_default().to_string();
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // The machine's connection first (it may be coming back the other
            // way), so the terminal goes over it.
            let job = engine.runtime().spawn({
                let engine = engine.clone();
                async move {
                    match engine.machine(&machine) {
                        Some((spec, _)) => crate::fleet::link::get(&spec).await.is_some(),
                        None => false,
                    }
                }
            });
            let linked = job.await.unwrap_or(false);
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some((_, waiting)) = this.resuming.get_mut(&sid) {
                    *waiting = false;
                }
                // Still cut off (and still on screen): attach again.
                if linked && this.terminals.get(&sid).is_some_and(|t| t.read(cx).exited()) {
                    this.open_terminal(&sid, window, cx);
                }
                cx.notify();
            });
        });
        self.track(task);
        true
    }

    pub(super) fn refresh_machine_sessions(&self, machine_id: &str) {
        let engine = self.engine.clone();
        let id = machine_id.to_string();
        engine.runtime().clone().spawn(async move {
            let _ = engine.refresh_sessions(&id).await;
        });
    }

    pub(super) fn restart_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let sid = id.to_string();
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            let sid = sid.clone();
            async move { engine.control_session(&sid, Control::Restart).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(_) => this.open_terminal(&sid, window, cx),
                Err(e) => this.notify_user(e, true, cx),
            });
        });
        self.track(task);
    }

    pub(super) fn confirm_end_session(&mut self, s: &SessionInfo, window: &mut Window, cx: &mut Context<Self>) {
        let sid = s.id.clone();
        let detail = if s.agent == "shell" || s.agent.is_empty() {
            "The shell and anything still running in it stop.".to_string()
        } else {
            format!("{} and anything it started in this terminal stop. Its conversation ends.", s.agent_name)
        };
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("End {}?", s.title),
                detail,
                "End session",
                None,
                move |_, engine| {
                    let engine = engine.clone();
                    let sid = sid.clone();
                    engine.runtime().clone().spawn(async move {
                        if let Err(e) = engine.control_session(&sid, Control::Stop).await {
                            tracing::warn!("ending {sid}: {e}");
                        }
                    });
                    Ok(None)
                },
                self.engine.clone(),
                window,
                cx,
            )
        });
        self.show_modal(Modal::Confirm(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
    }

    // -----------------------------------------------------------------------
    // The page

    pub(super) fn render_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let Some(info) = s.sessions.iter().find(|x| x.id == id).cloned() else {
            // Sessions are discovered over ssh; until then, don't call it ended.
            let machine = id.split('/').next().unwrap_or_default();
            let known = self.engine.sessions_of(machine).is_some_and(|f| f.checked_at.is_some());
            if !known {
                if self.sessions_requested.insert(machine.to_string()) {
                    self.refresh_machine_sessions(machine);
                }
                return looking(&theme);
            }
            return ended(&theme, id, cx);
        };
        // Attaching to a machine that's offline only waits for ssh to give up.
        let reachable = s.machines.iter().any(|m| m.id == info.machine_id && !m.is_down());
        if !self.terminals.contains_key(id) && reachable {
            self.open_terminal(id, window, cx);
        }
        if info.attention.is_some() && window.is_window_active() {
            self.engine.ack_session(id);
        }
        let host = s.machines.iter().find(|m| m.id == info.machine_id).cloned();
        // Can't reach the machine: what we show is from before, and nothing runs we can see.
        let offline = host.as_ref().filter(|m| m.is_down()).cloned();
        let machine = host.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| info.machine_id.clone());
        let machine_id = info.machine_id.clone();
        // Desktops: Linux (X11) machines so far.
        let desktop_capable = info.persistent && s.machines.iter().any(|m| m.id == info.machine_id && format::has_desktop(m) && !m.local);
        let split = self.page_split(&info.id).is_some();
        let (icon_name, (from, to)) = kit::agent_look(&info.agent);
        let exited = info.status == "exited";

        let status = if exited {
            kit::pill(format!("Exited{}", info.exit_code.map(|c| format!(" ({c})")).unwrap_or_default()), theme.text_placeholder).into_any_element()
        } else if let Some(m) = &offline {
            kit::pill(format::unreachable_word(m), theme.machine_status(m)).into_any_element()
        } else if info.busy {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(theme.text_accent)
                .child(spinner("session-busy", 12., theme.accent))
                .child("Working")
                .into_any_element()
        } else {
            div().text_size(px(12.)).text_color(theme.text_placeholder).child(if info.agent == "shell" { "Idle" } else { "Waiting for you" }).into_any_element()
        };

        let attach = info.attach_command.clone();
        let sid = info.id.clone();
        let sid2 = info.id.clone();
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .px(px(20.))
            .py(px(12.))
            .border_b_1()
            .border_color(theme.border_variant)
            .child(kit::tile(icon_name, from, to, 32.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(info.title.clone()))
                    // A folder too long to fit goes onto its own line (and wraps at
                    // its slashes) instead of running under the buttons.
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_x(px(6.))
                            .gap_y(px(2.))
                            .text_size(px(12.))
                            .text_color(theme.text_placeholder)
                            .child(info.agent_name.clone())
                            .child("·")
                            .child(machine)
                            .when(!info.cwd.is_empty(), |d| {
                                d.child(
                                    div()
                                        .flex()
                                        .items_start()
                                        .gap(px(4.))
                                        .min_w(px(0.))
                                        .child(kit::icon("folder", 12., theme.text_placeholder).flex_none().mt(px(2.)))
                                        .child(div().flex_1().min_w(px(0.)).font_family(mono_font()).text_size(px(11.5)).child(info.cwd.clone())),
                                )
                            }),
                    )
                    .children(info.git.as_ref().map(|g| git_line(&theme, &info.id, g, true).mt(px(2.)))),
            )
            .when(!info.persistent, |d| d.child(kit::pill("Ends when you close Muxin", theme.warning)))
            .child(status)
            .when(info.agent != "shell" && !info.agent.is_empty() && !exited && offline.is_none(), |d| {
                let sid = info.id.clone();
                d.child(
                    Button::new("session-actions", "Actions")
                        .icon("zap")
                        .tooltip("Ask the agent to commit, open a pull request, review its work…")
                        .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| this.open_menu(super::menu::MenuTarget::SessionActions(sid.clone()), e.position(), cx))),
                )
            })
            .when(info.persistent, |d| {
                d.child(
                    Button::icon_only("session-copy-attach", "copy")
                        .tooltip("Copy the command to open it in your own terminal")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(attach.clone()));
                            this.notify_user("Copied. Paste it into any terminal to attach.", false, cx);
                        })),
                )
            })
            // Its folder in your editor.
            .when(offline.is_none() && !info.cwd.is_empty(), |d| {
                let (mid, cwd) = (info.machine_id.clone(), info.cwd.clone());
                d.child(
                    Button::icon_only("session-editor", "code")
                        .tooltip(format!("Open {} in an editor", info.cwd))
                        .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| this.pick_editor(&mid, &cwd, e.position(), cx))),
                )
            })
            // Another shell beside it or below it, like splitting a terminal.
            .when(info.persistent && offline.is_none() && !self.engine.is_demo(), |d| {
                d.child(
                    Button::icon_only("session-split-right", "columns-2")
                        .tooltip("Split right (⌘D)")
                        .on_click(cx.listener(|this, _, window, cx| this.split_session(false, window, cx))),
                )
                .child(
                    Button::icon_only("session-split-down", "rows-2")
                        .tooltip("Split down (⇧⌘D)")
                        .on_click(cx.listener(|this, _, window, cx| this.split_session(true, window, cx))),
                )
            })
            .when(exited, |d| {
                d.child(
                    Button::new("session-restart", "Restart")
                        .icon("rotate-ccw")
                        .on_click(cx.listener(move |this, _, window, cx| this.restart_session(&sid, window, cx))),
                )
            })
            // The machine's desktop beside the terminal, to check what the agent did.
            .when(desktop_capable, |d| {
                let sid = info.id.clone();
                d.child(
                    Button::new("session-desktop", "Desktop")
                        .icon("panel-right")
                        .style(if split { ButtonStyle::Outlined } else { ButtonStyle::Ghost })
                        .tooltip(if split { "Hide the desktop" } else { "Show the machine's desktop beside the terminal" })
                        .on_click(cx.listener(move |this, _, window, cx| this.toggle_split(&sid, window, cx))),
                )
            })
            // Others watching (or typing) by link, peer to peer.
            .when(info.persistent && !exited && offline.is_none() && !self.engine.is_demo(), |d| d.child(self.share_button(&info, cx)))
            .child(Button::new("session-end", "End").icon("power").style(ButtonStyle::Ghost).disabled(offline.is_some()).on_click(cx.listener(move |this, _, window, cx| {
                if let Some(s) = this.snapshot.sessions.iter().find(|x| x.id == sid2).cloned() {
                    this.confirm_end_session(&s, window, cx);
                }
            })));

        // The terminal, or the panes of its split.
        let body = match self.pane_layout(id).cloned() {
            Some(layout) => self.render_panes(&layout, id, window, cx),
            None => self.session_body(&info, window, cx).into_any_element(),
        };
        let body = div().relative().flex_1().min_w(px(0.)).min_h(px(0.)).flex().child(body);
        let panel = self.split_desktop(&info.id).map(|view| {
            let (pop, close) = (machine_id.clone(), info.id.clone());
            let to_sidebar = info.id.clone();
            let extra = vec![
                Button::icon_only("split-sidebar", "panel-left")
                    .tooltip("Move to the sidebar")
                    .on_click(cx.listener(move |this, _, window, cx| this.desktop_to_sidebar(&to_sidebar, window, cx)))
                    .into_any_element(),
                Button::icon_only("split-window", "external-link")
                    .tooltip("Open in a window")
                    .on_click(cx.listener(move |this, _, window, cx| this.pop_out_desktop(&pop, window, cx)))
                    .into_any_element(),
                Button::icon_only("split-close", "x")
                    .tooltip("Hide the desktop")
                    .on_click(cx.listener(move |this, _, window, cx| this.toggle_split(&close, window, cx)))
                    .into_any_element(),
            ];
            div()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .flex()
                .flex_col()
                .child(super::desktop::toolbar(&view, extra, cx))
                .child(div().flex_1().min_h(px(0.)).child(view))
        });
        let body = match panel {
            Some(panel) => {
                // Terminal | divider (drag to resize, double-click for half and half) | desktop
                let resizing = self.split_resizing;
                let hover = theme.border;
                let divider = div()
                    .id("split-divider")
                    .group("split-divider")
                    .flex_none()
                    .w(px(6.))
                    .h_full()
                    .flex()
                    .justify_center()
                    .cursor_col_resize()
                    .child(
                        div()
                            .w(px(if resizing { 2. } else { 1. }))
                            .h_full()
                            .when(resizing, |d| d.bg(theme.accent))
                            .when(!resizing, |d| d.bg(theme.border_variant).group_hover("split-divider", |st| st.bg(hover).w(px(2.)))),
                    )
                    .on_drag(super::root::DraggedSplit, |_, _, _, cx| cx.new(|_| super::root::DraggedSplit))
                    .on_click(cx.listener(|this, e: &gpui::ClickEvent, _, cx| {
                        if e.click_count() == 2 {
                            this.split_ratio = 0.5;
                            this.finish_split_resize(cx);
                        }
                    }));
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .on_drag_move(cx.listener(Self::resize_split))
                    .on_drop(cx.listener(|this, _: &super::root::DraggedSplit, _, cx| this.finish_split_resize(cx)))
                    // The terminal pane was made to fill the page (flex_1: a zero basis);
                    // in a split its basis is its share of the width.
                    .child(body.flex_none().flex_basis(gpui::relative(self.split_ratio)).min_w(px(0.)))
                    .child(divider)
                    .child(panel)
                    .into_any_element()
            }
            None => body.into_any_element(),
        };
        let body = div().flex_1().min_h(px(0.)).flex().child(body);
        div().size_full().flex().flex_col().child(header).child(body).into_any_element()
    }

    /// A session's terminal, under what covers it while it can't be used
    /// (its machine offline, reconnecting, disconnected).
    fn session_body(&mut self, info: &SessionInfo, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let s = self.snapshot.clone();
        if !self.terminals.contains_key(&info.id) && s.machines.iter().any(|m| m.id == info.machine_id && !m.is_down()) {
            self.open_terminal(&info.id, window, cx);
        }
        let offline = s.machines.iter().find(|m| m.id == info.machine_id && m.is_down()).cloned();
        let terminal = self.terminals.get(&info.id).cloned();
        let detached = terminal.as_ref().is_some_and(|t| t.read(cx).exited());
        let exited = info.status == "exited";
        // Cut off while the machine is up (another network, a hiccup): attach
        // again by itself for a while before calling it disconnected.
        let resuming = if detached && !exited && info.persistent && offline.is_none() {
            self.resume_terminal(&info.id, window, cx)
        } else {
            if !detached {
                self.resuming.remove(&info.id);
            }
            false
        };
        let sid3 = info.id.clone();
        div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .children(terminal.map(|t| t.into_any_element()))
            .when_some(offline.clone().filter(|_| !exited && info.persistent), |d, m| {
                let disconnected = m.is_disconnected();
                let title = if disconnected { format!("Disconnected from {}", m.name) } else { m.unreachable.as_ref().map(|u| u.title.clone()).unwrap_or_else(|| format!("Can't reach {}", m.name)) };
                let mid = m.id.clone();
                d.child(
                    div().absolute().inset_0().bg(theme.scrim).flex().items_center().justify_center().child(
                        card(&theme)
                            .items_center()
                            .gap(px(10.))
                            .p(px(20.))
                            .max_w(px(420.))
                            .bg(theme.elevated)
                            .child(kit::color_tile(if format::is_just_offline(&m) { "power" } else { "unplug" }, 36.))
                            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(title))
                            .child(div().text_size(px(12.5)).text_color(theme.text_muted).text_center().child(if disconnected {
                                "This session keeps running there. Connect to see it again."
                            } else {
                                "Shut down: this session ended. Asleep: it's still there."
                            }))
                            .child(if disconnected {
                                Button::new("session-connect", "Connect").icon("plug").on_click(cx.listener(move |this, _, _, cx| this.set_machine_connected(&mid, true, cx)))
                            } else {
                                Button::new("session-offline-check", "Try again")
                                    .icon("refresh-cw")
                                    .loading(m.checking)
                                    .on_click(cx.listener(move |this, _, _, _| this.engine.trigger_check(Some(mid.clone()))))
                            }),
                    ),
                )
            })
            .when(resuming, |d| {
                d.child(
                    div().absolute().inset_0().bg(theme.scrim.opacity(0.5)).flex().items_center().justify_center().child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(14.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .bg(theme.elevated)
                            .border_1()
                            .border_color(theme.border_variant)
                            .text_size(px(12.5))
                            .text_color(theme.text_muted)
                            .child(spinner("session-resume", 13., theme.text_muted))
                            .child("Reconnecting…"),
                    ),
                )
            })
            .when(detached && !exited && info.persistent && offline.is_none() && !resuming, |d| {
                d.child(
                    div().absolute().inset_0().bg(theme.scrim).flex().items_center().justify_center().child(
                        card(&theme)
                            .items_center()
                            .gap(px(10.))
                            .p(px(20.))
                            .bg(theme.elevated)
                            .child(kit::color_tile("unplug", 36.))
                            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child("Disconnected"))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(theme.text_muted)
                                    .child("The session is still running on the machine."),
                            )
                            .child(
                                Button::new("session-reconnect", "Reconnect")
                                    .icon("plug")
                                    .style(ButtonStyle::Primary)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.resuming.remove(&sid3);
                                        this.open_terminal(&sid3, window, cx)
                                    })),
                            ),
                    ),
                )
            })
    }

    /// A layout of panes: each session's terminal under a slim title bar, the
    /// one keys go to outlined. A click picks a pane.
    fn render_panes(&mut self, layout: &Panes, focused: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        match layout {
            Panes::One(id) => {
                let Some(info) = self.snapshot.sessions.iter().find(|s| &s.id == id).cloned() else {
                    return div().flex_1().into_any_element();
                };
                let on = id == focused;
                let body = self.session_body(&info, window, cx);
                let (pick, close) = (id.clone(), id.clone());
                let bar = div()
                    .flex_none()
                    .h(px(24.))
                    .pl(px(10.))
                    .pr(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .bg(if on { theme.element } else { theme.surface })
                    .border_b_1()
                    .border_color(theme.border_variant)
                    .text_size(px(11.5))
                    .text_color(if on { theme.text } else { theme.text_placeholder })
                    .child(div().flex_1().min_w(px(0.)).overflow_hidden().whitespace_nowrap().child(info.title.clone()))
                    .child(
                        div()
                            .id(SharedString::from(format!("pane-close-{id}")))
                            .flex_none()
                            .p(px(3.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|st| st.bg(theme.ghost_hover))
                            .tooltip(|window, cx| kit::Tooltip::view("Close the pane (the session keeps running)".into(), window, cx))
                            .child(icon("x", 11., theme.text_placeholder))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_pane(&close, window, cx)
                            })),
                    );
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(if on { theme.accent.opacity(0.55) } else { gpui::transparent_black() })
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, _, window, cx| this.focus_pane(&pick, window, cx)))
                    .child(bar)
                    .child(body)
                    .into_any_element()
            }
            Panes::Split { down, a, b } => {
                let (a, b) = (self.render_panes(a, focused, window, cx), self.render_panes(b, focused, window, cx));
                let line = div().flex_none().bg(theme.border_variant);
                let line = if *down { line.h(px(1.)).w_full() } else { line.w(px(1.)).h_full() };
                let half = |el: AnyElement| div().flex_1().min_w(px(0.)).min_h(px(0.)).flex().child(el);
                div().flex_1().min_w(px(0.)).min_h(px(0.)).flex().when(*down, |d| d.flex_col()).child(half(a)).child(line).child(half(b)).into_any_element()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Sidebar

    pub(super) fn session_row(&self, theme: &Theme, s: &SessionInfo, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = s.id.clone();
        let (icon_name, (from, to)) = kit::agent_look(&s.agent);
        let exited = s.status == "exited";
        let offline = self.snapshot.machines.iter().any(|m| m.id == s.machine_id && m.is_down());
        // Explicit width: GPUI 0.2 only ellipsizes text whose width it knows up front.
        let shared = self.share_of(&s.id);
        let knocking = shared.is_some_and(|sh| sh.guests.iter().any(|g| g.state == "waiting"));
        let watched = shared.is_some_and(|sh| sh.guests.iter().any(|g| g.state == "in"));
        let trailing = if exited { 40. } else if s.busy || s.attention.is_some() { 20. } else { 0. } + if knocking || watched { 18. } else { 0. };
        let trailing = trailing + if exited && s.attention.is_some() { 14. } else { 0. };
        let text_w = (self.row_width() - 20. - 22. - 8. - 16. - 8. - trailing).max(40.);
        div()
            .id(SharedString::from(format!("side-s-{}", s.id)))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .mx(px(10.))
            .pl(px(22.))
            .pr(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)))
            .child(kit::tile(icon_name, from, to, 16.))
            .child(
                div()
                    .w(px(text_w))
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(if exited || offline { theme.text_placeholder } else if selected { theme.text } else { theme.text_muted })
                    .child(s.title.clone()),
            )
            .child(div().flex_1())
            // Shared: someone's asking to join, or watching.
            .when(watched && !knocking, |d| d.child(kit::icon("eye", 12., theme.text_accent)))
            .when(knocking, |d| d.child(div().size(px(8.)).rounded_full().bg(theme.accent)))
            .when(s.busy && !exited, |d| d.child(spinner(SharedString::from(format!("side-s-spin-{}", s.id)), 11., theme.accent)))
            // Unread: it finished, rang the bell, or exited. Cleared by opening it.
            .when(!s.busy && !selected, |d| d.children(s.attention.as_deref().map(|kind| attention_dot(theme, kind, s.exit_code))))
            .when(exited, |d| d.child(div().text_size(px(11.)).text_color(theme.text_placeholder).child("exited")))
            .on_mouse_down(gpui::MouseButton::Right, {
                let sid = s.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Session(sid.clone()), cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Session(id.clone()), window, cx)))
    }
}

impl Root {
    /// The machine page's list of agent sessions, with a way to start one.
    pub(super) fn machine_sessions(&self, theme: &Theme, s: &Snapshot, m: &Machine, cx: &mut Context<Self>) -> AnyElement {
        let sessions: Vec<&SessionInfo> = s.sessions.iter().filter(|x| x.machine_id == m.id).collect();
        let installed: Vec<&str> = AGENTS.iter().filter(|a| m.agents.iter().any(|i| i == a.id)).map(|a| a.name).collect();
        let mid = m.id.clone();
        let title = div()
            .flex()
            .items_center()
            .child(section_title(theme, "Agent sessions", Some(sessions.len())))
            .child(div().flex_1())
            .when(!installed.is_empty(), |d| {
                d.child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(format!("Installed: {}", installed.join(", "))))
            });


        let content = if sessions.is_empty() {
            let text = match runs_in_app(m) {
                _ if m.is_disconnected() => format!("Connect to {} to start Claude Code, Codex or a shell there.", m.name),
                _ if m.is_down() => format!("Start Claude Code, Codex or a shell here once {} is back.", m.name),
                true => format!("Run Claude Code, Codex or a shell here. {} so sessions run while Muxin is open.", m.sessions_error.clone().unwrap_or_default().trim_end_matches('.')),
                false => "Run Claude Code, Codex, OpenCode or a shell here. Sessions keep working on the machine after you close Muxin.".to_string(),
            };
            card(theme)
                .p(px(16.))
                .flex_row()
                .items_center()
                .gap(px(12.))
                .child(kit::color_tile("square-terminal", 28.))
                .child(div().flex_1().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text_muted).child(text))
                .child(
                    Button::new("m-session-empty", "New session")
                        .icon("plus")
                        .disabled(m.is_down())
                        .on_click(cx.listener(move |this, _, window, cx| this.show_new_session(Some(mid.clone()), window, cx))),
                )
                .into_any_element()
        } else {
            let rows: Vec<AnyElement> = sessions.iter().enumerate().map(|(i, x)| self.machine_session_row(theme, x, i, false, cx)).collect();
            card(theme).children(rows).into_any_element()
        };
        div().flex().flex_col().gap(px(10.)).child(title).child(content).into_any_element()
    }

    /// A session in a list; `with_machine` names its machine too (lists that span machines).
    pub(super) fn machine_session_row(&self, theme: &Theme, x: &SessionInfo, i: usize, with_machine: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = x.id.clone();
        let machine = with_machine.then(|| self.snapshot.machines.iter().find(|m| m.id == x.machine_id).map(|m| m.name.clone())).flatten();
        let (icon_name, (from, to)) = kit::agent_look(&x.agent);
        let offline = self.snapshot.machines.iter().any(|m| m.id == x.machine_id && m.is_down());
        let state = if x.status == "exited" {
            div().text_size(px(12.)).text_color(theme.text_placeholder).child(format!("Exited{}", x.exit_code.map(|c| format!(" ({c})")).unwrap_or_default())).into_any_element()
        } else if offline {
            div().text_size(px(12.)).text_color(theme.text_placeholder).child(if self.snapshot.machines.iter().any(|m| m.id == x.machine_id && m.is_disconnected()) { "Disconnected" } else { "Machine offline" }).into_any_element()
        } else if x.busy {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(theme.text_accent)
                .child(spinner(SharedString::from(format!("ms-spin-{}", x.id)), 12., theme.accent))
                .child("Working")
                .into_any_element()
        } else if let Some(kind) = x.attention.as_deref() {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(theme.text)
                .child(attention_dot(theme, kind, x.exit_code))
                .child(if kind == "bell" { "Needs you" } else { "Waiting for you" })
                .into_any_element()
        } else {
            div().text_size(px(12.)).text_color(theme.text_placeholder).child(if x.agent == "shell" { "Idle" } else { "Waiting for you" }).into_any_element()
        };
        div()
            .id(SharedString::from(format!("ms-{}", x.id)))
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(14.))
            .py(px(10.))
            .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
            .cursor_pointer()
            .hover(|st| st.bg(theme.ghost_hover))
            .child(kit::tile(icon_name, from, to, 26.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(1.))
                    .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.text).child(x.title.clone()))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .text_color(theme.text_placeholder)
                            .children(machine.map(|m| div().text_color(theme.text_muted).child(m)))
                            .when(with_machine, |d| d.child("·"))
                            .child(x.agent_name.clone())
                            .child("·")
                            .child(div().font_family(mono_font()).child(x.cwd.clone()))
                            .child("·")
                            .child(format!("started {}", format::ago(x.created_at))),
                    )
                    .children(x.git.as_ref().map(|g| git_line(theme, &format!("row-{}", x.id), g, false).mt(px(3.)))),
            )
            .child(state)
            .child(icon("chevron-right", 14., theme.text_placeholder))
            .on_mouse_down(gpui::MouseButton::Right, {
                let sid = x.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Session(sid.clone()), cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Session(id.clone()), window, cx)))
            .into_any_element()
    }
}

/// Sessions here can't live in Muxin's agent (a platform it has no build
/// for), so they run in this window and end with it.
pub(super) fn runs_in_app(m: &FleetMachine) -> bool {
    m.agent_version.is_none() && m.sessions_error.as_deref().is_some_and(|e| e.contains("no agent"))
}

/// The unread marker: amber when the agent rang for you, the accent when it
/// finished and is waiting, red when it exited with an error.
pub(super) fn attention_dot(theme: &Theme, kind: &str, exit_code: Option<i32>) -> gpui::Div {
    let color = match kind {
        "bell" => theme.warning,
        "exited" if exit_code.is_some_and(|c| c != 0) => theme.error,
        "exited" => theme.text_placeholder,
        _ => theme.accent,
    };
    kit::dot(color, 8.).flex_none()
}

fn looking(theme: &Theme) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .pb(px(80.))
        .text_size(px(12.5))
        .text_color(theme.text_muted)
        .child(spinner("session-looking", 14., theme.text_muted))
        .child("Looking for the session on the machine…")
        .into_any_element()
}

impl Root {
    /// The GitHub CLI on a machine (what agents there use for issues and pull
    /// requests): who it's signed in as, or a way to sign in.
    /// The machine page's Git card: whether commits and pushes to GitHub work
    /// there, with a button for each fix.
    pub(super) fn machine_git(&mut self, theme: &Theme, m: &Machine, cx: &mut Context<Self>) -> AnyElement {
        use super::github::Load;
        if !self.machine_git.contains_key(&m.id) {
            self.load_machine_git(&m.id, cx);
        }
        let mid = m.id.clone();
        let fixing = self.git_fixing.contains(&m.id);
        let title = div()
            .flex()
            .items_center()
            .child(section_title(theme, "Git", None))
            .child(div().flex_1())
            .child(Button::icon_only("m-git-refresh", "refresh-cw").tooltip("Refresh").on_click(cx.listener({
                let mid = mid.clone();
                move |this, _, _, cx| this.load_machine_git(&mid, cx)
            })));
        let row = |glyph: &'static str, name: &'static str, ok: Option<bool>, value: String, detail: Option<String>| {
            let (mark, color) = match ok {
                Some(true) => ("circle-check", theme.success),
                Some(false) => ("circle-alert", theme.warning),
                None => ("circle", theme.text_placeholder),
            };
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(10.))
                .child(kit::color_tile(glyph, 22.))
                .child(div().w(px(130.)).flex_none().text_size(px(12.5)).text_color(theme.text_muted).child(name))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(div().flex().items_center().gap(px(6.)).child(icon(mark, 13., color)).child(div().text_size(px(13.)).text_color(theme.text).child(value)))
                        .children(detail.map(|d| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(d))),
                )
        };

        // The GitHub CLI there: what pushes and agents use for GitHub.
        let gh = m.gh.clone();
        let gh_row = match gh.as_ref().map(|g| (g.installed, g.logged_in, g.account.clone())) {
            Some((true, true, Some(a))) => row("agent-github", "GitHub CLI", Some(true), format!("Signed in as @{a}"), None).into_any_element(),
            Some((true, _, _)) => row("agent-github", "GitHub CLI", Some(false), "Not signed in".into(), Some("Pushes and agents there use it to reach GitHub.".into()))
                .child(Button::new("m-gh-login", "Sign in").on_click(cx.listener({
                    let mid = mid.clone();
                    move |this, _, window, cx| this.gh_cli_sign_in(&mid, window, cx)
                })))
                .into_any_element(),
            Some((false, _, _)) => row("agent-github", "GitHub CLI", None, "Not installed".into(), Some("Install it from cli.github.com to push and use GitHub from here.".into())).into_any_element(),
            None => row("agent-github", "GitHub CLI", None, "Checking…".into(), None).into_any_element(),
        };
        let gh_signed_in = gh.as_ref().is_some_and(|g| g.logged_in);

        let rows: Vec<AnyElement> = match self.machine_git.get(&m.id) {
            Some(Load::Ready(g)) if g.setup.git.is_none() => {
                vec![row("git-branch", "git", Some(false), format!("git isn't installed on {}", m.name), Some("Install it with the system's package manager.".into())).into_any_element()]
            }
            Some(Load::Ready(g)) => {
                let identity = match (&g.setup.name, &g.setup.email) {
                    (Some(n), Some(e)) => row("git-branch", "Commits as", Some(true), format!("{n} <{e}>"), None).into_any_element(),
                    _ => {
                        let sets = match (&g.suggested_name, &g.suggested_email) {
                            (Some(n), Some(e)) => format!("Sets {n} <{e}>, GitHub's private address for you."),
                            _ => "Uses your GitHub name and private address.".into(),
                        };
                        row("git-branch", "Commits as", Some(false), "No name or email: commits will fail".into(), Some(sets))
                            .child(Button::new("m-git-identity", "Use my GitHub identity").loading(fixing).on_click(cx.listener({
                                let mid = mid.clone();
                                move |this, _, _, cx| this.fix_machine_git(&mid, true, false, cx)
                            })))
                            .into_any_element()
                    }
                };
                let push = match g.push.as_str() {
                    "gh" => row("arrow-up", "Pushes to GitHub", Some(true), "With the GitHub CLI's login".into(), None).into_any_element(),
                    other => {
                        let (value, ok) = match other {
                            "other" => (format!("With {}", g.setup.helpers.iter().chain(g.setup.ssh_keys.iter()).cloned().collect::<Vec<_>>().join(", ")), None),
                            _ => ("No GitHub login: pushes will fail".to_string(), Some(false)),
                        };
                        let detail = if gh_signed_in { "Using the GitHub CLI's login works for any GitHub repository." } else { "Sign in to the GitHub CLI first, then git can use its login." };
                        row("arrow-up", "Pushes to GitHub", ok, value, Some(detail.into()))
                            .child(Button::new("m-git-credentials", "Use the GitHub CLI").disabled(!gh_signed_in).loading(fixing).on_click(cx.listener({
                                let mid = mid.clone();
                                move |this, _, _, cx| this.fix_machine_git(&mid, false, true, cx)
                            })))
                            .into_any_element()
                    }
                };
                vec![identity, push]
            }
            Some(Load::Failed(e)) => vec![row("git-branch", "git", None, e.clone(), None).into_any_element()],
            _ => vec![div().flex().items_center().gap(px(10.)).px(px(14.)).py(px(10.)).child(spinner("m-git-spin", 13., theme.text_muted)).child(div().text_size(px(12.5)).text_color(theme.text_muted).child("Checking git…")).into_any_element()],
        };
        let all: Vec<AnyElement> = std::iter::once(gh_row).chain(rows).collect();
        let count = all.len();
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(title)
            .child(card(theme).children(all.into_iter().enumerate().map(|(i, r)| div().when(i > 0 && i < count, |d| d.border_t_1().border_color(theme.border_variant)).child(r))))
            .into_any_element()
    }

    /// The machine page's Connection card: direct (QUIC) or over ssh, how fast,
    /// and turning direct connections on or off.
    pub(super) fn machine_connection(&mut self, theme: &Theme, m: &Machine, cx: &mut Context<Self>) -> AnyElement {
        use super::github::Load;
        if !self.machine_conn.contains_key(&m.id) {
            self.load_machine_connection(&m.id, cx);
        }
        let mid = m.id.clone();
        let changing = self.conn_changing.contains(&m.id);
        // Reached through another machine (a VM on it, or behind it).
        let via = m.parent.as_ref().map(|p| p.name.clone());
        let (glyph, value, detail, direct) = match self.machine_conn.get(&m.id) {
            Some(Load::Ready(c)) => {
                let speed = c.latency_ms.map(|ms| format!(" · {ms} ms")).unwrap_or_default();
                // Through Tailscale: straight to the machine, or through its relay.
                let path = match (c.network.as_deref(), c.tailnet.as_ref().map(|d| d.path.as_str())) {
                    (Some("Tailscale"), Some("direct")) => " · peer-to-peer",
                    (Some("Tailscale"), Some("relay")) => " · through a relay",
                    _ => "",
                };
                let net = c.network.clone().map(|n| format!(" · {n}{path}")).unwrap_or_default();
                let at = c.peer.clone().map(|p| format!(" ({p})")).unwrap_or_default();
                // A port its host forwards to it: "adib-ser:47822 (UDP, forwarded)".
                let forwarded = c.address.as_deref().filter(|a| a.ends_with("forwarded)")).and_then(|a| a.split_whitespace().next()).and_then(|a| a.rsplit_once(':')).map(|(_, port)| port.to_string());
                match c.via.as_str() {
                    "direct" => (
                        "zap",
                        format!("Direct{net}{speed}"),
                        c.note.clone().unwrap_or_else(|| match (&via, forwarded) {
                            (Some(v), Some(port)) => format!("QUIC through {v}'s forwarded port {port}, no ssh in between."),
                            _ => format!("QUIC to {}{at}, no ssh in between.", c.address.clone().unwrap_or_default()),
                        }),
                        true,
                    ),
                    "ssh" => (
                        "network",
                        format!("Over ssh{net}{speed}"),
                        c.note.clone().unwrap_or_else(|| match &via {
                            Some(v) => format!("One ssh connection through {v}, kept open."),
                            None => "One ssh connection, kept open.".into(),
                        }),
                        c.direct,
                    ),
                    _ => ("unplug", "Not connected".into(), c.note.clone().unwrap_or_else(|| "The agent there didn't answer.".into()), c.direct),
                }
            }
            Some(Load::Failed(e)) => ("unplug", "Not connected".into(), e.clone(), false),
            _ => ("network", "Checking…".into(), String::new(), false),
        };
        // Not on the tailnet (while this computer is): offer to set it up there.
        let offer_tailscale = m.platform.starts_with("linux")
            && via.is_none()
            && matches!(self.machine_conn.get(&m.id), Some(Load::Ready(c)) if c.tailnet.is_none() && c.tailscale.as_ref().is_some_and(|t| t.running));
        // Whether it's reachable away from its own network.
        let tailnet_line = match self.machine_conn.get(&m.id) {
            Some(Load::Ready(c)) => match (&c.tailnet, &c.tailscale) {
                (Some(d), _) => Some(format!("On your tailnet as {}.", d.name)),
                // Where it can be reached is up to the machine it's reached through.
                (None, _) if via.is_some() => via.as_ref().map(|v| format!("Through {v}: reachable wherever {v} is.")),
                (None, Some(t)) if t.running => Some("Not on your tailnet: reachable only on its network.".to_string()),
                _ => None,
            },
            _ => None,
        };
        // Inside another machine with nothing forwarded to it: there's no way in for direct.
        let shut_in = m.parent.as_ref().is_some_and(|p| p.inside()) && m.direct_address.is_none();
        let button = if shut_in && !direct {
            None
        } else if direct {
            Some(Button::new("m-direct-off", "Use ssh only").style(ButtonStyle::Ghost).loading(changing).on_click(cx.listener({
                let mid = mid.clone();
                move |this, _, _, cx| this.set_machine_direct(&mid, false, cx)
            })))
        } else {
            Some(Button::new("m-direct-on", "Connect directly").loading(changing).on_click(cx.listener({
                let mid = mid.clone();
                move |this, _, _, cx| this.set_machine_direct(&mid, true, cx)
            })))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(section_title(theme, "Connection", None))
                    .child(div().flex_1())
                    .child(Button::new("m-disconnect", "Disconnect").icon("unplug").style(ButtonStyle::Ghost).tooltip("Stop connecting to it until you connect again. Its sessions keep running there.").on_click(cx.listener({
                        let mid = mid.clone();
                        move |this, _, _, cx| this.set_machine_connected(&mid, false, cx)
                    })))
                    .child(Button::icon_only("m-conn-refresh", "refresh-cw").tooltip("Refresh").on_click(cx.listener({
                        let mid = mid.clone();
                        move |this, _, _, cx| this.load_machine_connection(&mid, cx)
                    }))),
            )
            .child(
                card(theme)
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(12.))
                    .child(kit::color_tile(glyph, 28.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(div().text_size(px(13.)).text_color(theme.text).child(value))
                            .when(!detail.is_empty(), |d| d.child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(detail)))
                            .children(tailnet_line.map(|t| div().text_size(px(11.5)).text_color(theme.text_placeholder).child(t))),
                    )
                    .when(offer_tailscale, |d| {
                        d.child(Button::new("m-tailscale", "Set up Tailscale").style(ButtonStyle::Ghost).on_click(cx.listener({
                            let mid = mid.clone();
                            move |this, _, window, cx| this.setup_tailscale(&mid, window, cx)
                        })))
                    })
                    .children(button),
            )
            .into_any_element()
    }

    fn load_machine_connection(&mut self, machine_id: &str, cx: &mut Context<Self>) {
        use super::github::Load;
        self.machine_conn.entry(machine_id.to_string()).or_insert(Load::Loading);
        let (engine, id) = (self.engine.clone(), machine_id.to_string());
        let job = engine.runtime().spawn({
            let (engine, id) = (engine.clone(), id.clone());
            async move { engine.connection(&id).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.machine_conn.insert(id, match result {
                    Ok(c) => Load::Ready(c),
                    Err(e) => Load::Failed(e),
                });
                cx.notify()
            });
        });
        self.track(task);
    }

    /// Pairs for a direct connection (installs the listener there), or goes back to ssh.
    fn set_machine_direct(&mut self, machine_id: &str, enabled: bool, cx: &mut Context<Self>) {
        use super::github::Load;
        if !self.conn_changing.insert(machine_id.to_string()) {
            return;
        }
        cx.notify();
        let (engine, id) = (self.engine.clone(), machine_id.to_string());
        let job = engine.runtime().spawn({
            let (engine, id) = (engine.clone(), id.clone());
            async move { engine.set_direct(&id, enabled).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.conn_changing.remove(&id);
                match result {
                    Ok(c) => {
                        let text = match (enabled, c.via.as_str()) {
                            (true, "direct") => "Connected directly.".to_string(),
                            (true, _) => c.note.clone().unwrap_or_else(|| "Set up, but the direct connection didn't come up; using ssh.".into()),
                            (false, _) => "Back to ssh only; the listener there was removed.".to_string(),
                        };
                        this.notify_user(text, enabled && c.via != "direct", cx);
                        this.machine_conn.insert(id, Load::Ready(c));
                    }
                    Err(e) => this.notify_user(e, true, cx),
                }
            });
        });
        self.track(task);
    }

    fn load_machine_git(&mut self, machine_id: &str, cx: &mut Context<Self>) {
        use super::github::Load;
        if !self.machine_git.contains_key(machine_id) {
            self.machine_git.insert(machine_id.to_string(), Load::Loading);
        }
        let (engine, id) = (self.engine.clone(), machine_id.to_string());
        let job = engine.runtime().spawn({
            let (engine, id) = (engine.clone(), id.clone());
            async move { crate::api::git_status(&engine, &id).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(crate::api::ApiError::invalid(e.to_string())));
            let _ = this.update(cx, |this, cx| {
                this.machine_git.insert(id, match result {
                    Ok(g) => Load::Ready(g),
                    Err(e) => Load::Failed(e.message),
                });
                cx.notify()
            });
        });
        self.track(task);
    }

    /// Sets git's identity (from the GitHub account) and/or its GitHub login (the GitHub CLI's).
    fn fix_machine_git(&mut self, machine_id: &str, identity: bool, credentials: bool, cx: &mut Context<Self>) {
        use super::github::Load;
        if !self.git_fixing.insert(machine_id.to_string()) {
            return;
        }
        cx.notify();
        let (engine, id) = (self.engine.clone(), machine_id.to_string());
        let req = crate::api::GitSetupRequest { machine_id: id.clone(), identity, name: None, email: None, credentials };
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { crate::api::git_setup(&engine, req).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(crate::api::ApiError::invalid(e.to_string())));
            let _ = this.update(cx, |this, cx| {
                this.git_fixing.remove(&id);
                match result {
                    Ok(g) => {
                        let done = if identity { "Commits there now carry your GitHub identity." } else { "git there now uses the GitHub CLI's login." };
                        this.machine_git.insert(id, Load::Ready(g));
                        this.notify_user(done, false, cx)
                    }
                    Err(e) => this.notify_user(e.message, true, cx),
                }
            });
        });
        self.track(task);
    }

    /// Opens a terminal on the machine running `gh auth login` in the browser
    /// flow (it shows a code to enter at github.com/login/device).
    pub(super) fn gh_cli_sign_in(&mut self, machine_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.gh_cli_run(machine_id, "auth login --hostname github.com --git-protocol https --web", "Sign in to GitHub", window, cx);
    }

    /// Runs a GitHub CLI command (sign-in, granting a permission) in a terminal
    /// on a machine, where its browser steps can be followed.
    pub(super) fn gh_cli_run(&mut self, machine_id: &str, args: &str, title: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Windows runs it in PowerShell, which has no printf or read; and gh
        // by the path the agent found it at, in case the shell's PATH is older.
        let windows = self.snapshot.machines.iter().any(|m| m.id == machine_id && m.platform.starts_with("windows"));
        let command = if windows {
            let gh = self.engine.sessions_of(machine_id).and_then(|s| s.gh).and_then(|g| g.path).map(|p| crate::ops::remote::ps_quote(&p)).unwrap_or_else(|| "gh".into());
            format!("& {gh} {args}; Write-Host ''; [void](Read-Host 'Press Enter to close')")
        } else {
            format!("gh {args}; printf '\\nPress Enter to close.'; read _")
        };
        let req = StartSession {
            machine_id: machine_id.to_string(),
            agent: "shell".into(),
            cwd: "~".into(),
            title: Some(title.into()),
            command: Some(command),
            ..Default::default()
        };
        self.notify_user("Follow the steps in the terminal: copy the code, then open github.com/login/device.", false, cx);
        self.start_session(req, window, cx);
    }

    /// Gives a machine this computer's ssh key: a terminal here with
    /// `ssh-copy-id` typed in (making a key first if there's none), for the
    /// user to run and type their password for that machine once.
    pub(super) fn setup_ssh_access(&mut self, machine_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((spec, probe)) = self.engine.machine(machine_id) else { return };
        // The account to sign in as, so ssh-copy-id doesn't guess another one.
        let user = crate::probe::ssh_user(&spec, probe.as_ref());
        let Some(local) = self.snapshot.machines.iter().find(|m| m.local).map(|m| m.id.clone()) else {
            let cmd = crate::probe::copy_id_command(&spec, user.as_deref(), None);
            cx.write_to_clipboard(ClipboardItem::new_string(cmd));
            self.notify_user("Copied. Run it in Terminal, then check again.", false, cx);
            return;
        };
        let job = self.engine.runtime().spawn({
            let spec = spec.clone();
            async move { crate::probe::ssh_key_for(&spec).await }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let key = job.await.ok().flatten();
            let typed = match key {
                Some(k) => crate::probe::copy_id_command(&spec, user.as_deref(), Some(&k)),
                None => format!(
                    "ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519 -N '' && {}",
                    crate::probe::copy_id_command(&spec, user.as_deref(), Some("~/.ssh/id_ed25519.pub"))
                ),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let req = StartSession {
                    machine_id: local,
                    agent: "shell".into(),
                    cwd: "~".into(),
                    title: Some(format!("Set up access to {}", spec.name)),
                    typed: Some(typed),
                    ..Default::default()
                };
                let then = match &user {
                    Some(u) => format!("Press Enter and type your password for {u} on {}, then check again.", spec.name),
                    None => format!("Press Enter and type your password for {}, then check again.", spec.name),
                };
                this.start_session_then(req, Some(then), window, cx);
            });
        });
        self.track(task);
    }

    /// Opens a shell on a Linux machine with Tailscale's install command typed
    /// in, for the user to run: it asks for their password there, then shows
    /// a link to sign in to their tailnet.
    pub(super) fn setup_tailscale(&mut self, machine_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let req = StartSession {
            machine_id: machine_id.to_string(),
            agent: "shell".into(),
            cwd: "~".into(),
            title: Some("Set up Tailscale".into()),
            typed: Some("curl -fsSL https://tailscale.com/install.sh | sh && sudo tailscale up".into()),
            ..Default::default()
        };
        let then = "Press Enter to install Tailscale, then follow the sign-in link.";
        self.start_session_then(req, Some(then.into()), window, cx);
    }

    /// Where the GitHub login in use lives: the machine chosen for it, else this computer.
    pub(super) fn gh_login_machine(&self) -> Option<String> {
        let chosen = self.engine.settings().github.account_machine;
        if !chosen.is_empty() && self.snapshot.machines.iter().any(|m| m.id == chosen) {
            return Some(chosen);
        }
        self.snapshot.machines.iter().find(|m| m.host == "local").map(|m| m.id.clone())
    }
}

/// A terminal's keys and size, sent over the machine's link.
struct LinkIo {
    link: Arc<crate::fleet::link::Link>,
    channel: u32,
}

impl super::terminal::RemoteIo for LinkIo {
    fn write(&self, bytes: &[u8]) {
        self.link.send(self.channel, crate::fleet::link::DATA, bytes);
    }

    fn resize(&self, cols: u16, rows: u16) {
        let mut size = Vec::with_capacity(4);
        size.extend(cols.to_be_bytes());
        size.extend(rows.to_be_bytes());
        self.link.send(self.channel, crate::fleet::link::RESIZE, &size);
    }

    fn close(&self) {
        self.link.close_channel(self.channel);
    }
}

/// Attaches to a session over the machine's link: a channel for it, the
/// attach request, and its output turned into what the terminal reads.
fn attach_over_link(
    engine: &Arc<crate::fleet::Engine>,
    link: Arc<crate::fleet::link::Link>,
    name: &str,
    (cols, rows): (u16, u16),
) -> (Arc<dyn super::terminal::RemoteIo>, futures::channel::mpsc::UnboundedReceiver<super::terminal::RemoteOutput>) {
    use super::terminal::RemoteOutput;
    use crate::fleet::link::{DATA, EXIT};
    let (channel, mut frames) = link.open_channel();
    let (tx, output) = futures::channel::mpsc::unbounded();
    let rt = engine.runtime().clone();
    rt.spawn(async move {
        while let Some((kind, bytes)) = frames.recv().await {
            match kind {
                DATA => {
                    let _ = tx.unbounded_send(RemoteOutput::Bytes(bytes));
                }
                EXIT => {
                    let code = (bytes.len() == 4).then(|| i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
                    let _ = tx.unbounded_send(RemoteOutput::Ended(code));
                    return;
                }
                _ => {}
            }
        }
        // The channel closed: detached, the session's gone, or the link dropped.
        let _ = tx.unbounded_send(RemoteOutput::Ended(None));
    });
    let request = serde_json::json!({ "op": "attach", "name": name, "channel": channel, "cols": cols, "rows": rows });
    let attaching = link.clone();
    rt.spawn(async move {
        let ok = attaching.request(request, std::time::Duration::from_secs(15)).await.is_ok_and(|r| r["ok"].as_bool() == Some(true));
        if !ok {
            attaching.close_channel(channel);
        }
    });
    (Arc::new(LinkIo { link, channel }), output)
}

/// The session is gone (ended here, by an agent, or on the machine).
fn ended(theme: &Theme, id: &str, cx: &mut Context<Root>) -> AnyElement {
    let machine = id.split('/').next().unwrap_or_default().to_string();
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .pb(px(80.))
        .child(kit::color_tile("square-terminal", 44.))
        .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("This session has ended"))
        .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(id.to_string()))
        .child(
            Button::new("session-back", "Back to the machine")
                .icon("arrow-left")
                .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(machine.clone()), window, cx))),
        )
        .into_any_element()
}

/// What demo sessions show: a plausible screen for each agent.
fn demo_screen(agent: &str) -> Vec<u8> {
    let dim = "\x1b[2m";
    let reset = "\x1b[0m";
    let orange = "\x1b[38;2;217;119;87m";
    let green = "\x1b[32m";
    let bold = "\x1b[1m";
    let lines: Vec<String> = match agent {
        "claude" => vec![
            format!("{orange}╭───────────────────────────────────────────────────╮{reset}"),
            format!("{orange}│{reset} {orange}✻{reset} Welcome to {bold}Claude Code{reset}!                         {orange}│{reset}"),
            format!("{orange}│{reset}   {dim}cwd: /Users/ci/code/checkout{reset}                    {orange}│{reset}"),
            format!("{orange}╰───────────────────────────────────────────────────╯{reset}"),
            String::new(),
            format!("{dim}>{reset} Fix the flaky checkout test"),
            String::new(),
            format!("{green}⏺{reset} Read({bold}e2e/checkout.spec.ts{reset})"),
            format!("  {dim}⎿  Read 84 lines{reset}"),
            String::new(),
            "⏺ The discount is applied after tax in applyCoupon(); the test".into(),
            "  expects it before. Moving the discount ahead of the tax step.".into(),
            String::new(),
            format!("{green}⏺{reset} Update({bold}src/cart/applyCoupon.ts{reset})"),
            format!("  {dim}⎿  Updated with 3 additions and 2 removals{reset}"),
            String::new(),
            format!("{green}⏺{reset} Bash(npm test -- e2e/checkout.spec.ts)"),
            format!("  {dim}⎿{reset}  {green}✓{reset} 12 passed {dim}(4.1s){reset}"),
            String::new(),
            format!("{orange}✻ Thinking… {dim}(12s · ↓ 1.2k tokens · esc to interrupt){reset}"),
            String::new(),
            format!("{dim}╭───────────────────────────────────────────────────╮{reset}"),
            format!("{dim}│{reset} >                                                 {dim}│{reset}"),
            format!("{dim}╰───────────────────────────────────────────────────╯{reset}"),
            format!("  {dim}? for shortcuts{reset}"),
        ],
        "codex" => vec![
            format!("{bold}>_ OpenAI Codex{reset} {dim}(research preview){reset}"),
            format!("{dim}model: gpt-5-codex   directory: ~/code/ios-app{reset}"),
            String::new(),
            format!("{bold}›{reset} Add a dark mode toggle to Settings"),
            String::new(),
            format!("{green}•{reset} Explored SettingsView.swift, AppTheme.swift"),
            format!("{green}•{reset} Edited SettingsView.swift {green}+42{reset} \x1b[31m-3{reset}"),
            format!("{green}•{reset} Ran xcodebuild -scheme App build {dim}— succeeded{reset}"),
            String::new(),
            "Added a Toggle bound to @AppStorage(\"darkMode\") and applied".into(),
            ".preferredColorScheme at the root view.".into(),
            String::new(),
            format!("{bold}›{reset} "),
        ],
        "shell" => vec![
            format!("{green}ci@forge{reset} {bold}~/code/checkout{reset} % git status --short"),
            format!(" M src/cart/applyCoupon.ts"),
            format!(" M e2e/checkout.spec.ts"),
            format!("{green}ci@forge{reset} {bold}~/code/checkout{reset} % "),
        ],
        _ => vec![
            format!("{green}ci@beacon{reset}:{bold}~/api{reset}$ opencode"),
            String::new(),
            format!("{bold}  opencode{reset}  {dim}v0.15{reset}"),
            String::new(),
            format!("{dim}  Ask anything… {reset}"),
        ],
    };
    lines.join("\r\n").into_bytes()
}

impl Root {
    /// Asks a session's agent to do one of the session actions (commit, open
    /// a pull request, ...): its prompt is typed in.
    pub(super) fn run_session_action(&mut self, id: &str, action: &'static str, cx: &mut Context<Self>) {
        let (engine, id) = (self.engine.clone(), id.to_string());
        let label = crate::fleet::sessions::actions::action(action).map(|a| a.label).unwrap_or(action);
        let job = engine.runtime().spawn({
            let engine = engine.clone();
            async move { engine.session_action(&id, action).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| match result {
                Ok(_) => this.notify_user(format!("Asked the agent: {}.", label.to_lowercase()), false, cx),
                Err(e) => this.notify_user(e, true, cx),
            });
        });
        self.track(task);
    }
}

/// What a session's (or a workspace's) work changed, in a line: its branch,
/// lines added and removed, and its pull request with its checks (a click
/// opens it on GitHub). `full` adds files, commits and uncommitted files.
pub(super) fn git_line(theme: &Theme, key: &str, g: &GitStatus, full: bool) -> gpui::Stateful<gpui::Div> {
    let muted = theme.text_placeholder;
    let small = |text: String, color| div().flex_none().text_size(px(11.5)).text_color(color).child(text);
    let mut row = div().id(SharedString::from(format!("git-{key}"))).flex().items_center().gap(px(8.)).min_w(px(0.)).overflow_hidden();
    // The branch (a worktree's, marked so).
    row = row.child(
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .min_w(px(0.))
            .child(icon("git-branch", 12., muted).flex_none())
            .child(div().min_w(px(0.)).truncate().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_muted).child(if g.branch.is_empty() { "no branch".to_string() } else { g.branch.clone() })),
    );
    if g.worktree {
        row = row.child(kit::pill("worktree", theme.info));
    }
    let changed = g.added > 0 || g.removed > 0;
    if changed {
        row = row.child(div().flex().flex_none().gap(px(4.)).font_family(mono_font()).text_size(px(11.5)).child(div().text_color(theme.success).child(format!("+{}", g.added))).child(div().text_color(theme.error).child(format!("−{}", g.removed))));
    } else if g.files > 0 {
        row = row.child(small(format!("{} new file{}", g.files, if g.files == 1 { "" } else { "s" }), muted));
    } else if !g.on_base() {
        row = row.child(small("no changes yet".into(), muted));
    }
    if full {
        if changed && g.files > 0 {
            row = row.child(small(format!("in {} file{}", g.files, if g.files == 1 { "" } else { "s" }), muted));
        }
        if g.commits > 0 {
            row = row.child(small(format!("{} commit{}", g.commits, if g.commits == 1 { "" } else { "s" }), muted));
        }
        if g.uncommitted > 0 {
            row = row.child(small(format!("{} uncommitted", g.uncommitted), theme.warning));
        }
    }
    if let Some(p) = &g.pull {
        let (color, state) = match p.state.as_str() {
            "merged" => (theme.accent2, " merged"),
            "closed" => (theme.error, " closed"),
            "draft" => (muted, " draft"),
            _ => (theme.success, ""),
        };
        let c = &p.checks;
        let checks = match (c.failed, c.pending, c.total) {
            (0, 0, 0) => None,
            (f, _, _) if f > 0 => Some(("circle-x", theme.error, if full { format!("{f} failing") } else { f.to_string() })),
            (_, p, _) if p > 0 => Some(("clock", theme.warning, if full { "checks running".to_string() } else { String::new() })),
            _ => Some(("circle-check", theme.success, if full { "checks pass".to_string() } else { String::new() })),
        };
        let url = p.url.clone();
        row = row.child(
            div()
                .id(SharedString::from(format!("git-pr-{key}")))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .px(px(6.))
                .h(px(18.))
                .rounded(px(4.))
                .bg(color.opacity(0.12))
                .cursor_pointer()
                .hover(|st| st.bg(color.opacity(0.22)))
                .child(icon(if p.state == "merged" { "git-merge" } else if p.state == "draft" { "git-pull-request-draft" } else { "git-pull-request" }, 11., color))
                .child(div().text_size(px(11.)).font_weight(FontWeight::MEDIUM).text_color(color).child(format!("#{}{state}", p.number)))
                .children(checks.map(|(glyph, color, text)| div().flex().items_center().gap(px(3.)).child(icon(glyph, 11., color)).when(!text.is_empty(), |d| d.child(div().text_size(px(11.)).text_color(color).child(text)))))
                .on_click(move |_, _, cx| cx.open_url(&url)),
        );
    }
    row
}
