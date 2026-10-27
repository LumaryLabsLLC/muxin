//! Right-click menus for machines, runners, sessions and repositories.

use gpui::{AnyElement, ClipboardItem, Context, FontWeight, MouseButton, Pixels, Point, Window, anchored, deferred, div, prelude::*, px};

use super::kit::{self, icon};
use super::root::{Root, Route};
use super::theme::Theme;
use crate::fleet::sessions::{AGENTS, StartSession};
use crate::ops::RunnerAction;

pub enum MenuTarget {
    Machine(String),
    /// A machine page's ··· menu: what isn't on the page (install a runner, remove it).
    MachineMore(String),
    /// A runner, by id ("beelink/beelink-01").
    Runner(String),
    /// A queued or running job, by GitHub id.
    Job(u64),
    Session(String),
    /// The things to ask a session's agent (commit, open a pull request, ...).
    SessionActions(String),
    /// The agents a session's work can be handed to.
    Handoff(String),
    /// A GitHub repository, "owner/name".
    Repo(String),
    /// An issue or pull request.
    Item { repo: String, number: u64, pull: bool, url: String },
    /// The GitHub accounts to switch between.
    GithubAccounts,
    /// An agent workspace, by id.
    Workspace(String),
    /// One of a machine's monitors.
    Display { machine: String, display: String },
    /// A machine's apps' windows, to open one here.
    Apps(String),
    /// The editors to open a machine's folder in.
    Editors { machine: String, path: String },
    /// A shared session's guests and link (by session id).
    Share(String),
    /// A share someone gave you (by its id here), one of its machine's
    /// sessions, its desktop.
    Joined(String),
    JoinedSession(String, String),
    JoinedDesktop(String),
}

pub struct ContextMenu {
    pub target: MenuTarget,
    /// Where the click was, in window coordinates.
    pub position: Point<Pixels>,
}

#[derive(Clone)]
enum Action {
    NewTerminal(String),
    ShowDesktop(String),
    /// The machine's own remote desktop (RDP, VNC), in this computer's app for it.
    OpenRemoteDesktop(String),
    ShowLogs(String),
    ShowServices(String),
    CopyFiles(String),
    NewAgent(String, &'static str),
    Check(String),
    /// Connect to (true) or disconnect from (false) a machine.
    Connect(String, bool),
    /// A machine's folder, in an editor (by id).
    OpenInEditor(String, String, &'static str),
    /// Wake on LAN.
    Wake(String),
    /// The Run page, with just this machine picked.
    RunOn(String),
    InstallRunner(String),
    Remove(String),
    Runner(String, RunnerAction),
    RemoveRunner(String),
    FavoriteMachine(String, bool),
    CloudPower(String, bool),
    /// Start (true) or stop a VM or container that runs on another machine.
    MachinePower(String, bool),
    /// The New VM dialog, for this machine.
    NewVm(String),
    /// The New schedule dialog, for this machine.
    NewSchedule(String),
    ShowRunner(String),
    /// Install a runner with these labels on this machine.
    InstallWith(String, Vec<String>),
    CancelRun(u64),
    CopyAttach(String),
    Restart(String),
    End(String),
    Favorite(String, bool),
    OpenUrl(String),
    CopyUrl(String),
    ItemToAgent(String, u64, super::github::Handoff),
    UseAccount(String, Option<String>),
    WorkspaceSession(String),
    DeleteWorkspace(String),
    AddAccount,
    RenameMachine(String),
    RenameSession(String),
    /// One of the session actions (sessions::actions), for a session.
    SessionAction(String, &'static str),
    /// A session's work, to this agent.
    Handoff(String, &'static str),
    /// A session's work, to another machine (the Move dialog).
    MoveTo(String, String),
    /// Settings, where the actions' prompts are.
    EditPrompts,
    AddDisplay(String),
    ShowDisplay(String, String),
    /// One of a machine's apps' windows, as a window of its own here.
    OpenApp(String, crate::fleet::desktop::AppWindowInfo),
    RenameDisplay(String, String),
    RemoveDisplay(String, String),
    /// A monitor's size or scale.
    SetDisplay(String, String, Option<(u32, u32)>, Option<f64>),
    /// A share's guest: let in (or not), let type (or not), removed.
    ShareAdmit(String, u64, bool),
    ShareTyping(String, u64, bool),
    ShareControl(String, u64, bool),
    ShareManage(String, u64, bool),
    ShareRemove(String, u64),
    ShareExtend(String),
    ShareStop(String),
    /// The Share dialog of a session (its link and options).
    ShareOptions(String),
    /// Something to do with a share someone gave you.
    Joined(String, super::share::JoinedAction),
}

enum Item {
    Action { label: String, glyph: Glyph, action: Action, danger: bool },
    /// An action that can't be done right now: shown dimmed, not clickable.
    Off { label: String, glyph: Glyph },
    /// Text that isn't clickable.
    Note(String),
    Separator,
}

enum Glyph {
    Icon(&'static str),
    Agent(&'static str),
    /// An app's own icon (a PNG file), else this glyph.
    App(Option<std::path::PathBuf>, &'static str),
    /// The one picked (a plain check).
    Selected,
    None,
}

/// The things to ask a session's agent: the ones that fit its branch, the
/// others dimmed, then where to change their prompts.
fn session_actions(x: &crate::fleet::sessions::SessionInfo) -> Vec<Item> {
    use crate::fleet::sessions::actions::ACTIONS;
    let mut items: Vec<Item> = ACTIONS
        .iter()
        .map(|a| match a.fits(x.git.as_ref()) {
            true => Item::Action { label: a.label_for(x.git.as_ref()), glyph: Glyph::Icon(a.icon), action: Action::SessionAction(x.id.clone(), a.id), danger: false },
            false => Item::Off { label: a.label.to_string(), glyph: Glyph::Icon(a.icon) },
        })
        .collect();
    items.push(Item::Separator);
    items.push(Item::Action { label: "Edit the prompts…".into(), glyph: Glyph::Icon("pencil"), action: Action::EditPrompts, danger: false });
    items
}

/// One item per editor here ("Zed", or "Open in Zed" when `long`), each with its app's icon.
fn editor_items(chosen: &str, machine: &str, path: &str, long: bool) -> Vec<Item> {
    crate::fleet::editor::by_preference(chosen)
        .into_iter()
        .map(|e| Item::Action {
            label: if long { format!("Open in {}", e.name) } else { e.name.to_string() },
            glyph: Glyph::App(crate::fleet::editor::icon(e.id), "code"),
            action: Action::OpenInEditor(machine.to_string(), path.to_string(), e.id),
            danger: false,
        })
        .collect()
}

impl Root {
    /// The editors to open a machine's folder in, as a menu where the click was
    /// (or straight in the only one there is).
    pub(super) fn pick_editor(&mut self, machine: &str, path: &str, position: Point<Pixels>, cx: &mut Context<Self>) {
        let editors = crate::fleet::editor::installed();
        match editors.as_slice() {
            [only] => self.open_in_editor(machine, path, only.id, cx),
            _ => self.open_menu(MenuTarget::Editors { machine: machine.to_string(), path: path.to_string() }, position, cx),
        }
    }

    pub(super) fn open_menu(&mut self, target: MenuTarget, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.context_menu = Some(ContextMenu { target, position });
        cx.notify();
    }

    fn menu_items(&self, cx: &gpui::App) -> Vec<Item> {
        let Some(menu) = &self.context_menu else { return Vec::new() };
        let s = &self.snapshot;
        let act = |label: &str, icon_name: &'static str, action: Action| Item::Action { label: label.into(), glyph: Glyph::Icon(icon_name), action, danger: false };
        match &menu.target {
            MenuTarget::Machine(id) => {
                let Some(m) = s.machines.iter().find(|m| &m.id == id) else { return Vec::new() };
                let mut items = Vec::new();
                if !m.host.is_empty() {
                    // Asleep: wake it first.
                    if m.status == "offline" && super::format::is_just_offline(m) && self.engine.can_wake(&m.id) {
                        items.push(act("Wake", "power", Action::Wake(id.clone())));
                        items.push(Item::Separator);
                    }
                    // Disconnected on purpose: connecting comes first.
                    if m.is_disconnected() {
                        items.push(act("Connect", "plug", Action::Connect(id.clone(), true)));
                        items.push(Item::Separator);
                    }
                    // These need the machine up: dimmed while it's off, unreachable or disconnected.
                    let up = !m.is_down();
                    let needs_up = |item: Item| match item {
                        Item::Action { label, glyph, .. } if !up => Item::Off { label, glyph },
                        other => other,
                    };
                    items.push(needs_up(act("New terminal", "square-terminal", Action::NewTerminal(id.clone()))));
                    for a in AGENTS.iter().filter(|a| a.id != "shell" && m.agents.iter().any(|i| i == a.id)) {
                        items.push(needs_up(Item::Action { label: format!("New {} session…", a.name), glyph: Glyph::Agent(a.id), action: Action::NewAgent(id.clone(), a.id), danger: false }));
                    }
                    if super::format::has_desktop(m) && !m.local {
                        items.push(needs_up(act("Show desktop", "monitor", Action::ShowDesktop(id.clone()))));
                    }
                    if super::format::has_desktop(m) && !m.local && !s.demo {
                        items.push(needs_up(act(crate::fleet::remote_desktop::open_label(&m.platform), "external-link", Action::OpenRemoteDesktop(id.clone()))));
                    }
                    if super::format::has_desktop(m) && !s.demo && !m.platform.starts_with("windows") {
                        items.push(needs_up(act("Add virtual monitor…", "monitor", Action::AddDisplay(id.clone()))));
                    }
                    items.push(needs_up(act("Logs", "scroll-text", Action::ShowLogs(id.clone()))));
                    items.push(needs_up(act("Services", "layers", Action::ShowServices(id.clone()))));
                    items.push(needs_up(act("Copy files…", "folder", Action::CopyFiles(id.clone()))));
                    items.push(needs_up(act("Run a command…", "zap", Action::RunOn(id.clone()))));
                    items.push(Item::Separator);
                    if !m.is_disconnected() {
                        items.push(act("Refresh", "refresh-cw", Action::Check(id.clone())));
                    }
                    items.push(needs_up(act("Install runner…", "plus", Action::InstallRunner(id.clone()))));
                    if !m.is_disconnected() && !m.local {
                        items.push(act("Disconnect", "unplug", Action::Connect(id.clone(), false)));
                    }
                    items.push(Item::Separator);
                }
                items.push(act("Rename…", "pencil", Action::RenameMachine(id.clone())));
                if Root::is_favorite(cx, id) {
                    items.push(act("Remove from favorites", "star", Action::FavoriteMachine(id.clone(), false)));
                } else {
                    items.push(act("Add to favorites", "star", Action::FavoriteMachine(id.clone(), true)));
                }
                items.push(Item::Separator);
                // A cloud machine is powered and destroyed; removing it alone would leave it running (and billed).
                if m.cloud.is_some() {
                    let off = m.status == "offline";
                    let busy = s.operations.iter().any(|o| o.status == "running" && o.machine_id.as_deref() == Some(id.as_str()) && o.kind != "logs");
                    let label = if off { "Power on" } else { "Power off" };
                    items.push(if busy { Item::Off { label: label.into(), glyph: Glyph::Icon("power") } } else { act(label, "power", Action::CloudPower(id.clone(), off)) });
                    items.push(Item::Action { label: "Destroy…".into(), glyph: Glyph::Icon("trash-2"), action: Action::Remove(id.clone()), danger: true });
                } else {
                    // A VM or container on another machine: started and stopped there.
                    if let Some(p) = m.parent.as_ref().filter(|p| p.container.is_some() && s.machines.iter().any(|h| h.id == p.machine_id && !h.is_down())) {
                        let running = p.container_state.as_deref() == Some("running");
                        let busy = s.operations.iter().any(|o| o.status == "running" && o.machine_id.as_deref() == Some(id.as_str()) && o.kind != "logs");
                        let label = if running { "Stop" } else { "Start" };
                        items.push(if busy { Item::Off { label: label.into(), glyph: Glyph::Icon("power") } } else { act(label, "power", Action::MachinePower(id.clone(), !running)) });
                    }
                    if m.status == "online" {
                        items.push(act("New schedule here…", "clock", Action::NewSchedule(id.clone())));
                    }
                    // A machine that can run VMs: make one here.
                    if self.engine.vm_options().hosts.iter().any(|h| &h.machine_id == id && !h.systems.is_empty()) {
                        items.push(act("New VM here…", "box", Action::NewVm(id.clone())));
                    }
                    // A VM Muxin made goes with its container and disk.
                    let label = if m.vm.is_some() { "Destroy…" } else { "Remove from Muxin…" };
                    items.push(Item::Action { label: label.into(), glyph: Glyph::Icon("trash-2"), action: Action::Remove(id.clone()), danger: true });
                }
                items
            }
            MenuTarget::MachineMore(id) => {
                let Some(m) = s.machines.iter().find(|m| &m.id == id) else { return Vec::new() };
                let busy = s.operations.iter().any(|o| o.status == "running" && o.machine_id.as_deref() == Some(id.as_str()) && o.kind != "logs");
                let install = "Install runner…";
                let mut items = vec![match m.host.is_empty() || busy || m.is_down() {
                    true => Item::Off { label: install.into(), glyph: Glyph::Icon("plus") },
                    false => act(install, "plus", Action::InstallRunner(id.clone())),
                }];
                items.push(Item::Separator);
                // A cloud machine or a VM Muxin made goes with its disk.
                let label = if m.cloud.is_some() || m.vm.is_some() { "Destroy…" } else { "Remove from Muxin…" };
                items.push(Item::Action { label: label.into(), glyph: Glyph::Icon("trash-2"), action: Action::Remove(id.clone()), danger: true });
                items
            }
            MenuTarget::Runner(id) => {
                let Some(r) = s.runners.iter().find(|r| &r.id == id) else { return Vec::new() };
                let mut items = Vec::new();
                // Start, stop and the rest need its machine (GitHub-only runners can only be removed).
                if r.local.is_some() && r.machine_id.is_some() {
                    let running = r.local.as_ref().is_some_and(|l| l.listener);
                    if running {
                        items.push(act("Stop", "square", Action::Runner(id.clone(), RunnerAction::Stop)));
                    } else {
                        items.push(act("Start", "play", Action::Runner(id.clone(), RunnerAction::Start)));
                    }
                    items.push(act("Restart", "rotate-ccw", Action::Runner(id.clone(), RunnerAction::Restart)));
                    items.push(act("Show logs", "scroll-text", Action::Runner(id.clone(), RunnerAction::Logs)));
                    items.push(Item::Separator);
                }
                if let Some(job) = r.job_id.and_then(|j| s.jobs.running.iter().find(|x| x.id == j)) {
                    items.push(act("Open its job on GitHub", "external-link", Action::OpenUrl(job.url.clone())));
                }
                if let Some(url) = r.github.as_ref().and_then(runner_settings_url) {
                    items.push(act("Open on GitHub", "agent-github", Action::OpenUrl(url)));
                }
                items.push(act("Copy name", "copy", Action::CopyUrl(r.name.clone())));
                items.push(Item::Separator);
                items.push(Item::Action { label: "Remove runner…".into(), glyph: Glyph::Icon("trash-2"), action: Action::RemoveRunner(id.clone()), danger: true });
                items
            }
            MenuTarget::Job(id) => {
                let Some(j) = s.jobs.queued.iter().chain(s.jobs.running.iter()).find(|j| j.id == *id) else { return Vec::new() };
                let mut items = Vec::new();
                if let Some(rid) = &j.runner_id {
                    items.push(act("Show its runner", "server", Action::ShowRunner(rid.clone())));
                }
                // Stuck: a runner with what it needs, on a machine of the right OS and CPU.
                if j.status != "in_progress" && j.matching_online == 0 {
                    let extra: Vec<String> = j.labels.iter().filter(|l| platform_label(l).is_none() && !l.eq_ignore_ascii_case("self-hosted")).cloned().collect();
                    let fits: Vec<&crate::fleet::model::Machine> =
                        s.machines.iter().filter(|m| !m.host.is_empty() && m.status == "online" && j.labels.iter().all(|l| platform_label(l).is_none_or(|p| m.platform.contains(p)))).collect();
                    if !extra.is_empty() {
                        items.push(Item::Note(format!("Needs a runner with {}", extra.join(", "))));
                    }
                    if fits.is_empty() {
                        items.push(Item::Note("None of your machines fits it.".into()));
                    }
                    for m in fits {
                        items.push(Item::Action {
                            label: format!("Add runner on {}", m.name),
                            glyph: Glyph::Icon("plus"),
                            action: Action::InstallWith(m.id.clone(), extra.clone()),
                            danger: false,
                        });
                    }
                }
                if !items.is_empty() {
                    items.push(Item::Separator);
                }
                items.push(act("Open on GitHub", "external-link", Action::OpenUrl(j.url.clone())));
                items.push(act("Copy link", "copy", Action::CopyUrl(j.url.clone())));
                items.push(Item::Separator);
                items.push(Item::Action { label: "Cancel run…".into(), glyph: Glyph::Icon("circle-x"), action: Action::CancelRun(*id), danger: true });
                items
            }
            MenuTarget::SessionActions(id) => {
                let Some(x) = s.sessions.iter().find(|x| &x.id == id) else { return Vec::new() };
                session_actions(x)
            }
            MenuTarget::Handoff(id) => {
                let Some(x) = s.sessions.iter().find(|x| &x.id == id) else { return Vec::new() };
                // A box's image has every agent a box runs; a machine, the ones found on it.
                let in_box = x.box_id.as_deref() == Some(x.name.as_str());
                let installed = |a: &str| match in_box {
                    true => crate::fleet::boxes::BOX_AGENTS.contains(&a),
                    false => s.machines.iter().find(|m| m.id == x.machine_id).is_some_and(|m| m.agents.iter().any(|i| i == a)),
                };
                let mut items = vec![Item::Note(if in_box { "Another agent takes over in the box, with a note of the work so far".into() } else { "Another agent takes over here, with a note of the work so far. This session ends".into() })];
                for a in AGENTS.iter().filter(|a| !a.command.is_empty() && a.id != x.agent && installed(a.id)) {
                    items.push(Item::Action { label: format!("Hand off to {}", a.name), glyph: Glyph::Agent(a.id), action: Action::Handoff(id.clone(), a.id), danger: false });
                }
                // The same agent, fresh: for a conversation that's grown too long.
                if let Some(a) = AGENTS.iter().find(|a| a.id == x.agent && !a.command.is_empty()) {
                    items.push(Item::Separator);
                    items.push(Item::Action { label: format!("A fresh {} session", a.name), glyph: Glyph::Agent(a.id), action: Action::Handoff(id.clone(), a.id), danger: false });
                }
                // The work to another machine (not a box's: it stays in its box).
                let others: Vec<&crate::fleet::model::Machine> =
                    s.machines.iter().filter(|m| m.id != x.machine_id && !m.platform.starts_with("windows") && !m.host.is_empty()).collect();
                if !in_box && !others.is_empty() {
                    items.push(Item::Separator);
                    items.push(Item::Note("On another machine, with its work".into()));
                    for m in others {
                        let label = format!("Move to {}…", m.name);
                        items.push(match m.is_down() {
                            true => Item::Off { label, glyph: Glyph::Icon("server") },
                            false => Item::Action { label, glyph: Glyph::Icon(if m.local { "laptop" } else { "server" }), action: Action::MoveTo(id.clone(), m.id.clone()), danger: false },
                        });
                    }
                }
                items
            }
            MenuTarget::Session(id) => {
                let Some(x) = s.sessions.iter().find(|x| &x.id == id) else { return Vec::new() };
                let mut items = Vec::new();
                if x.agent != "shell" && !x.agent.is_empty() && x.status == "running" {
                    items.push(Item::Note("Ask the agent".into()));
                    items.extend(session_actions(x));
                    items.push(Item::Separator);
                }
                items.push(act("Rename…", "pencil", Action::RenameSession(id.clone())));
                if x.persistent {
                    items.push(act("Copy attach command", "copy", Action::CopyAttach(id.clone())));
                }
                if !x.cwd.is_empty() {
                    items.extend(editor_items(&self.engine.settings().editor, &x.machine_id, &x.cwd, true));
                }
                if x.status == "exited" {
                    items.push(act("Restart", "rotate-ccw", Action::Restart(id.clone())));
                }
                items.push(Item::Separator);
                // A box's agent: ending it pauses the box (see confirm_end_session).
                items.push(match &x.box_id {
                    Some(b) if *b == x.name => act("Pause box", "pause", Action::End(id.clone())),
                    _ => Item::Action { label: "End session…".into(), glyph: Glyph::Icon("power"), action: Action::End(id.clone()), danger: true },
                });
                items
            }
            MenuTarget::Joined(id) | MenuTarget::JoinedSession(id, _) | MenuTarget::JoinedDesktop(id) => self
                .joined_menu(&menu.target)
                .into_iter()
                .map(|item| {
                    use super::share::JoinedItem;
                    let glyph_of = |glyph: &'static str| if glyph.is_empty() { Glyph::None } else { Glyph::Icon(glyph) };
                    match item {
                        JoinedItem::Do { label, glyph, action, danger, picked } => {
                            Item::Action { label, glyph: if picked { Glyph::Selected } else { glyph_of(glyph) }, action: Action::Joined(id.clone(), action), danger }
                        }
                        JoinedItem::Agent { label, agent, action: Some(action) } => Item::Action { label, glyph: Glyph::Agent(agent), action: Action::Joined(id.clone(), action), danger: false },
                        JoinedItem::Agent { label, agent, action: None } => Item::Off { label, glyph: Glyph::Agent(agent) },
                        JoinedItem::Off { label, glyph } => Item::Off { label, glyph: glyph_of(glyph) },
                        JoinedItem::Note(text) => Item::Note(text),
                        JoinedItem::Separator => Item::Separator,
                    }
                })
                .collect(),
            MenuTarget::Share(sid) => {
                let Some(share) = self.share_of(sid).cloned() else { return Vec::new() };
                let mut items = vec![Item::Note(format!("Ends in {}", super::share::time_left(share.ends_at)))];
                if let Some(problem) = &share.problem {
                    items.insert(0, Item::Note(format!("Guests can't reach {} right now: {problem}", share.target.machine_name)));
                }
                if share.guests.is_empty() {
                    items.push(Item::Note("No one has joined yet.".into()));
                }
                for g in &share.guests {
                    let state = if g.state == "waiting" {
                        "waiting"
                    } else if g.can_manage {
                        "can start sessions"
                    } else if g.can_control {
                        "using the desktop"
                    } else if g.can_type {
                        "typing"
                    } else {
                        "watching"
                    };
                    let how = if g.client == "browser" { "browser" } else { "Muxin" };
                    let path = if g.path.is_empty() { String::new() } else { format!(" · {}", g.path) };
                    items.push(Item::Separator);
                    items.push(Item::Note(format!("{} · {state} · {how}{path}", g.name)));
                    if g.state == "waiting" {
                        items.push(act(&format!("Let {} in (code {})", g.name, g.code), "user-check", Action::ShareAdmit(share.id.clone(), g.id, true)));
                        items.push(Item::Action { label: format!("Deny {}", g.name), glyph: Glyph::Icon("user-x"), action: Action::ShareAdmit(share.id.clone(), g.id, false), danger: true });
                    } else {
                        let label = if g.can_type { format!("Stop {} typing", g.name) } else { format!("Let {} type", g.name) };
                        items.push(act(&label, "keyboard", Action::ShareTyping(share.id.clone(), g.id, !g.can_type)));
                        if share.target.is_machine() && g.can_type {
                            let label = if g.can_manage { format!("Stop {} starting sessions", g.name) } else { format!("Let {} start and end sessions", g.name) };
                            items.push(act(&label, "square-terminal", Action::ShareManage(share.id.clone(), g.id, !g.can_manage)));
                        }
                        if share.desktop == crate::share::host::Desktop::Control {
                            let label = if g.can_control { format!("Stop {} using the desktop", g.name) } else { format!("Let {} use the desktop", g.name) };
                            items.push(act(&label, "monitor", Action::ShareControl(share.id.clone(), g.id, !g.can_control)));
                        }
                        items.push(Item::Action { label: format!("Remove {}", g.name), glyph: Glyph::Icon("user-x"), action: Action::ShareRemove(share.id.clone(), g.id), danger: true });
                    }
                }
                items.push(Item::Separator);
                items.push(act("Link and options…", "sliders-horizontal", Action::ShareOptions(sid.clone())));
                items.push(act("Copy link", "copy", Action::CopyUrl(share.link.clone())));
                items.push(act("Add an hour", "clock", Action::ShareExtend(share.id.clone())));
                items.push(Item::Action { label: "Stop sharing".into(), glyph: Glyph::Icon("power"), action: Action::ShareStop(share.id.clone()), danger: true });
                items
            }
            MenuTarget::Apps(machine) => {
                let mut items = vec![Item::Note("Open in a window of its own".into())];
                match self.app_windows.get(machine) {
                    None | Some(None) => items.push(Item::Note("Looking for its apps…".into())),
                    Some(Some(Err(e))) => items.push(Item::Note(e.clone())),
                    Some(Some(Ok(list))) if list.is_empty() => items.push(Item::Note("No apps are open there.".into())),
                    Some(Some(Ok(list))) => {
                        for w in list {
                            let label = match w.minimized {
                                true => format!("{} (minimized)", w.title),
                                false => w.title.clone(),
                            };
                            items.push(Item::Action { label, glyph: Glyph::Icon("monitor"), action: Action::OpenApp(machine.clone(), w.clone()), danger: false });
                        }
                    }
                }
                items
            }
            MenuTarget::Editors { machine, path } => {
                let mut items = vec![Item::Note("Open it in".into())];
                items.extend(editor_items(&self.engine.settings().editor, machine, path, false));
                if items.len() == 1 {
                    items.push(Item::Note("No editor here: Muxin opens folders in Zed, Cursor or VS Code.".into()));
                }
                items
            }
            MenuTarget::Display { machine, display } => {
                let Some(d) = s.machines.iter().find(|m| &m.id == machine).and_then(|m| m.displays.iter().find(|d| &d.id == display)) else { return Vec::new() };
                let mut items = vec![
                    act("Show", "monitor", Action::ShowDisplay(machine.clone(), display.clone())),
                    act("Rename…", "pencil", Action::RenameDisplay(machine.clone(), display.clone())),
                ];
                // Every size it offers (from 1024 wide), and its scale.
                let current = (d.width.round() as u32, d.height.round() as u32);
                let sizes: Vec<(u32, u32)> = d.modes.iter().copied().filter(|(w, _)| *w >= 1024).collect();
                if sizes.len() > 1 {
                    items.push(Item::Separator);
                    items.push(Item::Note("Resolution".into()));
                    for size in sizes {
                        let glyph = if size == current { Glyph::Selected } else { Glyph::None };
                        let action = Action::SetDisplay(machine.clone(), display.clone(), Some(size), None);
                        items.push(Item::Action { label: format!("{} × {}", size.0, size.1), glyph, action, danger: false });
                    }
                }
                if [1., 2.].iter().all(|s| d.scales.iter().any(|x| (x - s).abs() < 0.01)) {
                    items.push(Item::Separator);
                    items.push(Item::Note("Scale".into()));
                    for (scale, label) in [(1., "100%"), (2., "200% (twice as big, as sharp)")] {
                        let glyph = if (d.scale - scale).abs() < 0.01 { Glyph::Selected } else { Glyph::None };
                        let action = Action::SetDisplay(machine.clone(), display.clone(), None, Some(scale));
                        items.push(Item::Action { label: label.into(), glyph, action, danger: false });
                    }
                }
                if d.is_virtual {
                    items.push(Item::Separator);
                    items.push(Item::Action { label: "Remove monitor".into(), glyph: Glyph::Icon("trash-2"), action: Action::RemoveDisplay(machine.clone(), display.clone()), danger: true });
                }
                items
            }
            MenuTarget::Repo(name) => {
                let favorite = self.gh_is_favorite(name);
                vec![
                    if favorite {
                        act("Remove from favorites", "star", Action::Favorite(name.clone(), false))
                    } else {
                        act("Add to favorites", "star-filled", Action::Favorite(name.clone(), true))
                    },
                    Item::Separator,
                    act("Open on GitHub", "external-link", Action::OpenUrl(format!("https://github.com/{name}"))),
                    act("Copy link", "copy", Action::CopyUrl(format!("https://github.com/{name}"))),
                ]
            }
            MenuTarget::Item { repo, number, pull, url } => {
                use super::github::Handoff;
                let mut items = if *pull {
                    vec![
                        act("Review with agent…", "bot", Action::ItemToAgent(repo.clone(), *number, Handoff::Session)),
                        act("Review in a new workspace…", "layers", Action::ItemToAgent(repo.clone(), *number, Handoff::Workspace)),
                    ]
                } else {
                    vec![
                        act("Work on it with an agent…", "bot", Action::ItemToAgent(repo.clone(), *number, Handoff::Session)),
                        act("Hand off in a box…", "box", Action::ItemToAgent(repo.clone(), *number, Handoff::Box)),
                    ]
                };
                items.extend([
                    Item::Separator,
                    act("Open on GitHub", "external-link", Action::OpenUrl(url.clone())),
                    act("Copy link", "copy", Action::CopyUrl(url.clone())),
                ]);
                items
            }
            MenuTarget::Workspace(id) => {
                let Some(w) = s.workspaces.iter().find(|w| &w.id == id) else { return Vec::new() };
                let mut items = Vec::new();
                if w.status == "ready" {
                    items.push(act("New session here", "square-terminal", Action::WorkspaceSession(id.clone())));
                }
                items.push(act("Copy shell command", "copy", Action::CopyUrl(w.shell.clone())));
                items.push(Item::Separator);
                items.push(Item::Action { label: "Delete workspace…".into(), glyph: Glyph::Icon("trash-2"), action: Action::DeleteWorkspace(id.clone()), danger: true });
                items
            }
            MenuTarget::GithubAccounts => {
                use super::github::Load;
                let mut items = Vec::new();
                match &self.gh.accounts {
                    Some(Load::Ready(a)) if !a.token_source.is_empty() && a.token_source != "gh" => {
                        items.push(Item::Note(format!("Using the token in ${}", a.token_source)));
                        items.push(Item::Note("Unset it to switch accounts.".into()));
                        return items;
                    }
                    Some(Load::Ready(a)) => {
                        let using = |login: &str, machine: Option<&str>| a.machine_id.as_deref() == machine && a.using.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(login));
                        if !a.machines.is_empty() && !a.accounts.is_empty() {
                            items.push(Item::Note("On this computer".into()));
                        }
                        for x in &a.accounts {
                            let label = if x.ok { format!("@{}", x.login) } else { format!("@{} (sign in again)", x.login) };
                            let glyph = if using(&x.login, None) { Glyph::Selected } else { Glyph::None };
                            items.push(Item::Action { label, glyph, action: Action::UseAccount(x.login.clone(), None), danger: false });
                        }
                        if !a.machines.is_empty() {
                            items.push(Item::Note("Signed in on your machines".into()));
                        }
                        for m in &a.machines {
                            let glyph = if using(&m.login, Some(&m.machine_id)) { Glyph::Selected } else { Glyph::None };
                            let label = format!("@{} · {}", m.login, m.machine);
                            items.push(Item::Action { label, glyph, action: Action::UseAccount(m.login.clone(), Some(m.machine_id.clone())), danger: false });
                        }
                    }
                    Some(Load::Failed(e)) => items.push(Item::Note(e.clone())),
                    _ => items.push(Item::Note("Loading accounts…".into())),
                }
                items.push(Item::Separator);
                items.push(act("Add another account…", "plus", Action::AddAccount));
                items
            }
        }
    }

    fn run_menu_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        match action {
            Action::NewTerminal(machine) => {
                // A shell in the home folder, right away.
                let direct = self.snapshot.machines.iter().find(|m| m.id == machine).is_some_and(super::sessions::runs_in_app);
                let req = StartSession { machine_id: machine, agent: "shell".into(), cwd: "~".into(), direct, ..Default::default() };
                self.start_session(req, window, cx);
            }
            Action::NewAgent(machine, agent) => self.show_new_session_with(Some(machine), Some(agent), window, cx),
            Action::ShowDesktop(machine) => self.navigate(Route::Desktop(machine), window, cx),
            Action::OpenRemoteDesktop(machine) => self.open_remote_desktop(&machine, cx),
            Action::ShowLogs(machine) => self.navigate(Route::Logs(machine), window, cx),
            Action::ShowServices(machine) => self.navigate(Route::Services(machine), window, cx),
            Action::CopyFiles(machine) => self.show_copy_files(Some(machine), None, window, cx),
            Action::Check(machine) => self.engine.trigger_check(Some(machine)),
            Action::Connect(machine, on) => self.set_machine_connected(&machine, on, cx),
            Action::OpenInEditor(machine, path, editor) => self.open_in_editor(&machine, &path, editor, cx),
            Action::Wake(machine) => self.wake_machine(&machine, cx),
            Action::RunOn(machine) => {
                self.run_picked = Some([machine].into());
                self.navigate(Route::Run, window, cx);
            }
            Action::Runner(id, action) => self.runner_action(&id, action, cx),
            Action::FavoriteMachine(id, favorite) => self.set_favorite_machine(&id, favorite, cx),
            Action::AddDisplay(id) => self.show_add_display(id, window, cx),
            Action::ShowDisplay(machine, display) => self.show_display(machine, Some(display), window, cx),
            Action::OpenApp(machine, app) => self.open_app_window(machine, app, cx),
            Action::RenameDisplay(machine, display) => self.show_rename(super::dialogs::RenameTarget::Display(machine, display), window, cx),
            Action::RemoveDisplay(machine, display) => self.remove_display(machine, display, window, cx),
            Action::SetDisplay(machine, display, size, scale) => self.set_display(machine, display, size, scale, window, cx),
            Action::RenameMachine(id) => self.show_rename(super::dialogs::RenameTarget::Machine(id), window, cx),
            Action::RenameSession(id) => self.show_rename(super::dialogs::RenameTarget::Session(id), window, cx),
            Action::SessionAction(id, action) => self.run_session_action(&id, action, cx),
            Action::Handoff(id, agent) => self.handoff_session(&id, agent, None, false, window, cx),
            Action::MoveTo(id, machine) => self.show_move_dialog(&id, machine, window, cx),
            Action::EditPrompts => self.open_settings_tab(super::settings::SettingsTab::Prompts, window, cx),
            Action::CloudPower(id, on) => self.cloud_power(&id, on, cx),
            Action::MachinePower(id, on) => self.machine_power(&id, on, cx),
            Action::NewVm(id) => self.show_vm_dialog(Some(id), window, cx),
            Action::NewSchedule(id) => self.show_schedule_dialog(Some(id), window, cx),
            Action::ShowRunner(id) => self.navigate(Route::Runner(id), window, cx),
            Action::InstallWith(machine, labels) => self.show_install_dialog_with(&machine, &labels, window, cx),
            Action::CancelRun(job) => self.confirm_cancel_run(job, window, cx),
            Action::RemoveRunner(id) => {
                if let Some(r) = self.snapshot.runners.iter().find(|r| r.id == id).cloned() {
                    self.confirm_remove_runner(&r, window, cx);
                }
            }
            Action::InstallRunner(machine) => self.show_install_dialog(&machine, window, cx),
            Action::Remove(machine) => {
                if let Some(m) = self.snapshot.machines.iter().find(|m| m.id == machine).cloned() {
                    self.confirm_remove_machine(&m, window, cx);
                }
            }
            Action::CopyAttach(id) => {
                if let Some(x) = self.snapshot.sessions.iter().find(|x| x.id == id) {
                    cx.write_to_clipboard(ClipboardItem::new_string(x.attach_command.clone()));
                    self.notify_user("Copied. Paste it into any terminal to attach.", false, cx);
                }
            }
            Action::Restart(id) => {
                self.navigate(Route::Session(id.clone()), window, cx);
                self.restart_session(&id, window, cx);
            }
            Action::End(id) => {
                if let Some(x) = self.snapshot.sessions.iter().find(|x| x.id == id).cloned() {
                    self.confirm_end_session(&x, window, cx);
                }
            }
            Action::Favorite(name, favorite) => self.gh_set_favorite(&name, favorite, cx),
            Action::UseAccount(login, machine) => {
                let using = matches!(&self.gh.accounts, Some(super::github::Load::Ready(a))
                    if a.machine_id == machine && a.using.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(&login)));
                if !using {
                    self.gh_use_account(login, machine, cx)
                }
            }
            Action::AddAccount => {
                // Signing in makes the new account the CLI's active one; stay on
                // this one until the user switches.
                if let Some(super::github::Load::Ready(a)) = &self.gh.accounts {
                    let mut settings = self.engine.settings();
                    if let (true, Some(using)) = (settings.github.account.is_empty(), a.using.clone()) {
                        settings.github.account = using;
                        let _ = self.engine.update_settings(settings);
                    }
                }
                window.dispatch_action(Box::new(super::root::SignInGitHub), cx)
            }
            Action::ItemToAgent(repo, number, how) => self.gh_item_to_agent(repo, number, how, window, cx),
            Action::OpenUrl(url) => cx.open_url(&url),
            Action::WorkspaceSession(id) => {
                if let Some(w) = self.snapshot.workspaces.iter().find(|w| w.id == id).cloned() {
                    self.new_workspace_session(&w, window, cx)
                }
            }
            Action::DeleteWorkspace(id) => {
                if let Some(w) = self.snapshot.workspaces.iter().find(|w| w.id == id).cloned() {
                    self.confirm_destroy_workspace(&w.id, &w.name, window, cx)
                }
            }
            Action::CopyUrl(url) => {
                cx.write_to_clipboard(ClipboardItem::new_string(url));
                self.notify_user("Copied the link.", false, cx);
            }
            Action::ShareAdmit(share, guest, yes) => crate::share::host::admit(&share, guest, yes),
            Action::ShareTyping(share, guest, allowed) => crate::share::host::set_typing(&share, guest, allowed),
            Action::ShareControl(share, guest, allowed) => crate::share::host::set_control(&share, guest, allowed),
            Action::ShareManage(share, guest, allowed) => crate::share::host::set_managing(&share, guest, allowed),
            Action::ShareRemove(share, guest) => crate::share::host::remove(&share, guest),
            Action::ShareExtend(share) => crate::share::host::extend(&share, std::time::Duration::from_secs(3600)),
            Action::ShareOptions(session) => self.show_share(&session, window, cx),
            Action::Joined(id, action) => self.run_joined(&id, action, window, cx),
            Action::ShareStop(share) => {
                crate::share::host::stop(&share);
                self.notify_user("Stopped sharing. The link doesn't work anymore.", false, cx);
            }
        }
        cx.notify();
    }

    /// The open menu, drawn above everything, with a backdrop that closes it.
    pub(super) fn render_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let position = self.context_menu.as_ref()?.position;
        let items = self.menu_items(cx);
        if items.is_empty() {
            self.context_menu = None;
            return None;
        }
        // As wide as its longest line (about 7.4px a character at 13px, plus the
        // glyph and padding), within reason.
        let longest = items
            .iter()
            .map(|i| match i {
                Item::Action { label, .. } | Item::Off { label, .. } => label.chars().count() as f32 * 7.4 + 64.,
                Item::Note(text) => text.chars().count() as f32 * 6.8 + 32.,
                Item::Separator => 0.,
            })
            .fold(0., f32::max);
        let width = longest.clamp(200., 420.);
        let close = |this: &mut Root, _: &gpui::MouseDownEvent, _: &mut Window, cx: &mut Context<Root>| {
            this.context_menu = None;
            cx.notify();
        };
        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(i, item)| match item {
                Item::Separator => div().h(px(1.)).my(px(4.)).mx(px(8.)).bg(theme.border_variant).into_any_element(),
                Item::Note(text) => div().px(px(12.)).py(px(5.)).text_size(px(12.)).text_color(theme.text_placeholder).child(text).into_any_element(),
                Item::Off { label, glyph } => div()
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .h(px(28.))
                    .mx(px(4.))
                    .px(px(8.))
                    .opacity(0.4)
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(div().w(px(18.)).flex().justify_center().child(menu_glyph(theme, glyph)))
                    .child(div().w(px(width - 56.)).truncate().child(label))
                    .into_any_element(),
                Item::Action { label, glyph, action, danger } => {
                    let color = if danger { theme.error } else { theme.text };
                    let glyph = menu_glyph(theme, glyph);
                    div()
                        .id(("menu-item", i))
                        .flex()
                        .items_center()
                        .gap(px(9.))
                        .h(px(28.))
                        .mx(px(4.))
                        .px(px(8.))
                        .rounded(crate::ui::kit::corner(6.))
                        .cursor_pointer()
                        .hover(|st| st.bg(theme.element_hover))
                        .text_size(px(13.))
                        .text_color(color)
                        .child(div().w(px(18.)).flex().justify_center().child(glyph))
                        // Margins, padding, glyph and gap; past the widest menu, "…".
                        .child(div().w(px(width - 56.)).truncate().child(label))
                        .on_click(cx.listener(move |this, _, window, cx| this.run_menu_action(action.clone(), window, cx)))
                        .into_any_element()
                }
            })
            .collect();
        let menu = div()
            .id("context-menu")
            .occlude()
            .w(px(width))
            .py(px(4.))
            .rounded(crate::ui::kit::corner(9.))
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
            .when(!crate::ui::kit::square(), |d| d.shadow_lg())
            .font_weight(FontWeight::NORMAL)
            .children(rows);
        Some(
            div()
                .absolute()
                .inset_0()
                .child(div().id("menu-backdrop").absolute().inset_0().occlude().on_mouse_down(MouseButton::Left, cx.listener(close)).on_mouse_down(MouseButton::Right, cx.listener(close)))
                .child(deferred(anchored().position(position).snap_to_window_with_margin(px(8.)).child(menu)).with_priority(1))
                .into_any_element(),
        )
    }
}

/// Makes a sidebar row open its menu on right-click.
pub(super) fn on_right_click(target: impl Fn() -> MenuTarget + 'static, cx: &mut Context<Root>) -> impl Fn(&gpui::MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| this.open_menu(target(), e.position, cx))
}

/// The runner's page in its repository's or organization's settings on GitHub.
fn runner_settings_url(g: &crate::fleet::model::GitHubRunner) -> Option<String> {
    match g.scope.split_once(':')? {
        ("repo", repo) => Some(format!("https://github.com/{repo}/settings/actions/runners/{}", g.id)),
        ("org", org) => Some(format!("https://github.com/organizations/{org}/settings/actions/runners/{}", g.id)),
        _ => None,
    }
}

/// What a GitHub runner label says about the platform, as it appears in a
/// machine's "linux-x64": GitHub adds these itself, so they aren't extra labels.
fn platform_label(label: &str) -> Option<&'static str> {
    match label.to_ascii_lowercase().as_str() {
        "linux" => Some("linux"),
        "macos" => Some("darwin"),
        "windows" => Some("windows"),
        "x64" => Some("x64"),
        "arm64" => Some("arm64"),
        "arm" => Some("arm"),
        _ => None,
    }
}

fn menu_glyph(theme: &Theme, glyph: Glyph) -> AnyElement {
    match glyph {
        Glyph::Icon(name) => kit::color_tile(name, 18.).into_any_element(),
        Glyph::Agent(agent) => {
            let (name, (from, to)) = kit::agent_look(agent);
            kit::tile(name, from, to, 18.).into_any_element()
        }
        Glyph::App(Some(png), _) => gpui::img(png.clone()).size(px(18.)).flex_none().into_any_element(),
        Glyph::App(None, name) => kit::color_tile(name, 18.).into_any_element(),
        Glyph::Selected => icon("check", 14., theme.accent).into_any_element(),
        Glyph::None => div().into_any_element(),
    }
}
