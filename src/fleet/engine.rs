//! Owns fleet state, runs the GitHub poll loop and scheduled machine checks, and
//! publishes a fresh `Snapshot` (as JSON) every time something changes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::runtime::Handle;
use tokio::sync::{watch, Notify};

use super::build::{build, Inputs};
use super::model::{CheckInfo, InventoryInfo, Operation, Snapshot};
use super::sessions::{self, DirectSession, MachineSessions, SessionNotice};
use super::workspaces::{self as workspaces, WorkspaceRecord};
use crate::config::{default_inventory, display_path, write_atomic, Inventory, MachineSpec, Paths, PersistedState, Settings};
use crate::demo::World;
use crate::github::{self, GitHubData, ScopeTest};
use crate::probe::{self, ProbeResult};

const DEMO_POLL: Duration = Duration::from_secs(3);
const MAX_OPERATIONS: usize = 30;
const MAX_OP_LOG_LINES: usize = 2000;
const MAX_NOTICES: usize = 50;
/// Operation output can be chatty; coalesce snapshot publishes to this rate.
const OP_PUBLISH_INTERVAL: Duration = Duration::from_millis(120);

struct State {
    inventory: Inventory,
    inventory_info: InventoryInfo,
    inventory_mtime: Option<SystemTime>,
    settings: Settings,
    probes: HashMap<String, ProbeResult>,
    checking: HashSet<String>,
    check_running: bool,
    check_started_at: Option<DateTime<Utc>>,
    check_finished_at: Option<DateTime<Utc>>,
    github: Option<GitHubData>,
    github_syncing: bool,
    token_source: String,
    version: u64,
    operations: VecDeque<Operation>,
    next_op: u64,
    last_publish: Option<Instant>,
    publish_scheduled: bool,
    /// Modification times of the files we last read or wrote, so changes made
    /// by another Muxen process (the window, the CLI, an agent's MCP
    /// server) are noticed. See `sync_from_disk`.
    settings_mtime: Option<SystemTime>,
    state_mtime: Option<SystemTime>,
    /// Agent workspaces (see fleet::workspaces), shared through workspaces.json.
    workspaces: Vec<WorkspaceRecord>,
    workspaces_mtime: Option<SystemTime>,
    /// Agent sessions per machine, from the last discovery (see fleet::sessions).
    sessions: HashMap<String, MachineSessions>,
    /// Newest machine event seen per machine; older ones never notify again.
    event_marks: HashMap<String, i64>,
    /// Session notices, newest first, and the unread one per session.
    notices: VecDeque<SessionNotice>,
    next_notice: u64,
    attention: HashMap<String, String>,
    /// Sessions running without the agent, in this process's terminals.
    direct: Vec<DirectSession>,
    /// Git checkouts per machine, from the last look (see fleet::code).
    checkouts: HashMap<String, Vec<super::model::Checkout>>,
}

pub struct Engine {
    paths: Paths,
    demo: Option<Arc<World>>,
    github: github::Client,
    rt: Handle,
    state: Mutex<State>,
    tx: watch::Sender<Arc<Snapshot>>,
    sync_now: Notify,
    settings_changed: Notify,
}

impl Engine {
    /// Must be called from within a tokio runtime; background work runs there.
    pub fn new(paths: Paths, demo: bool) -> Arc<Engine> {
        let world = demo.then(|| Arc::new(World::new()));
        let (inventory, inventory_info, settings, persisted) = match &world {
            Some(w) => (
                w.inventory(),
                InventoryInfo { path: "demo://fleet.toml".into(), loaded_at: Some(Utc::now()), ..Default::default() },
                w.settings(),
                PersistedState::default(),
            ),
            None => {
                ensure_inventory(&paths);
                (
                    Inventory::default(),
                    InventoryInfo { path: display_path(&paths.inventory), ..Default::default() },
                    Settings::load(&paths.settings),
                    PersistedState::load(&paths.state),
                )
            }
        };
        let state = State {
            inventory,
            inventory_info,
            inventory_mtime: None,
            settings,
            probes: persisted.probes,
            checking: HashSet::new(),
            check_running: false,
            check_started_at: persisted.check_started_at,
            check_finished_at: persisted.check_finished_at,
            github: None,
            github_syncing: false,
            token_source: String::new(),
            version: 0,
            operations: VecDeque::new(),
            next_op: 1,
            last_publish: None,
            publish_scheduled: false,
            settings_mtime: mtime(&paths.settings),
            state_mtime: mtime(&paths.state),
            workspaces: match &world {
                Some(w) => workspaces::demo_seed(w),
                None => workspaces::load_registry(&paths.workspaces),
            },
            workspaces_mtime: mtime(&paths.workspaces),
            sessions: HashMap::new(),
            event_marks: HashMap::new(),
            notices: VecDeque::new(),
            next_notice: 1,
            attention: HashMap::new(),
            direct: Vec::new(),
            checkouts: HashMap::new(),
        };
        let (tx, _) = watch::channel(Arc::new(Snapshot::default()));
        super::direct::init(paths.dir.clone());
        let engine = Arc::new(Engine {
            paths,
            demo: world,
            github: github::Client::new(),
            rt: Handle::current(),
            state: Mutex::new(state),
            tx,
            sync_now: Notify::new(),
            settings_changed: Notify::new(),
        });
        // Each machine's live link tells us when its sessions change.
        let weak = Arc::downgrade(&engine);
        super::link::set_on_connect(move |link| {
            if let Some(engine) = weak.upgrade() {
                let rt = engine.rt.clone();
                rt.spawn(engine.follow_link(link));
            }
        });
        let weak = Arc::downgrade(&engine);
        engine.github.set_token_fetcher(Arc::new(move |machine: String, host: String| {
            let weak = weak.clone();
            Box::pin(async move {
                let engine = weak.upgrade()?;
                engine.machine_gh_token(&machine, &host).await.map_err(|e| tracing::warn!("GitHub login on {machine}: {e}")).ok()
            })
        }));
        {
            let mut st = engine.state.lock().unwrap();
            if engine.demo.is_none() {
                engine.reload_inventory(&mut st, true);
                let known: HashSet<&String> = st.inventory.machines.iter().map(|m| &m.id).collect();
                let stale: Vec<String> = st.probes.keys().filter(|id| !known.contains(id)).cloned().collect();
                for id in stale {
                    st.probes.remove(&id);
                }
            }
            if let Some(w) = &engine.demo {
                let ids: Vec<String> = st.inventory.machines.iter().map(|m| m.id.clone()).collect();
                for id in ids {
                    st.sessions.insert(id.clone(), w.sessions(&id));
                }
            }
            engine.publish(&mut st);
        }
        engine
    }

    pub fn is_demo(&self) -> bool {
        self.demo.is_some()
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// The tokio runtime the engine runs on. UI code uses it to run async engine calls.
    pub fn runtime(&self) -> &Handle {
        &self.rt
    }

    /// Starts the background loops. Call once.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        self.rt.spawn(async move { me.github_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.check_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.sessions_loop().await });
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.tx.subscribe()
    }

    pub fn current(&self) -> Arc<Snapshot> {
        self.tx.borrow().clone()
    }

    pub fn settings(&self) -> Settings {
        self.state.lock().unwrap().settings.clone()
    }

    pub fn update_settings(&self, new: Settings) -> Result<Settings, String> {
        let new = new.normalized()?;
        if self.demo.is_none() {
            new.save(&self.paths.settings).map_err(|e| format!("Couldn't save settings: {e:#}"))?;
        }
        let favorites_only = {
            let mut st = self.state.lock().unwrap();
            st.settings_mtime = mtime(&self.paths.settings);
            let scopes_changed = st.settings.github != new.github;
            let favorites_only = Settings { favorite_repos: new.favorite_repos.clone(), ..st.settings.clone() } == new;
            st.settings = new.clone();
            if scopes_changed && new.github.orgs.is_empty() && new.github.repos.is_empty() {
                st.github = None;
            }
            self.publish(&mut st);
            favorites_only
        };
        // Starring a repository needn't re-poll GitHub or reschedule checks.
        if !favorites_only {
            self.settings_changed.notify_waiters();
            self.sync_now.notify_one();
        }
        Ok(new)
    }

    pub async fn test_scope(&self, scope: &str) -> ScopeTest {
        let settings = self.settings();
        match &self.demo {
            Some(w) => w.test_scope(scope),
            None => self.github.test_scope(&settings.github, scope).await,
        }
    }

    /// Kicks off a machine check (all machines, or one) plus a GitHub refresh.
    pub fn trigger_check(self: &Arc<Self>, machine: Option<String>) {
        let me = self.clone();
        self.rt.spawn(async move { me.run_check(machine.map(|m| vec![m])).await });
        self.sync_now.notify_one();
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn trigger_sync(&self) {
        self.sync_now.notify_one();
    }

    /// One-shot check for the CLI: probe every machine, sync GitHub once, return the snapshot.
    pub async fn check_once(self: &Arc<Self>, with_github: bool) -> Snapshot {
        self.run_check(None).await;
        if with_github {
            self.sync_github().await;
        }
        (*self.current()).clone()
    }

    /// Picks up what other Muxen processes wrote to the shared config
    /// directory: fleet.toml edits, settings.json changes, and newer machine
    /// checks in state.json. Cheap when nothing changed (three `stat` calls).
    pub fn refresh_from_disk(&self) {
        if self.demo.is_some() {
            return;
        }
        let settings_changed = {
            let mut st = self.state.lock().unwrap();
            let inventory_changed = self.reload_inventory(&mut st, false);
            let settings_changed = self.reload_settings(&mut st);
            let state_changed = self.merge_disk_state(&mut st);
            let workspaces_changed = self.reload_workspaces(&mut st);
            if inventory_changed || settings_changed || state_changed || workspaces_changed {
                self.publish(&mut st);
            }
            settings_changed
        };
        if settings_changed {
            self.settings_changed.notify_waiters();
            self.sync_now.notify_one();
        }
    }

    // -----------------------------------------------------------------------
    // Plumbing for control operations (see crate::ops)

    pub(crate) fn github(&self) -> &github::Client {
        &self.github
    }

    pub(crate) fn demo_world(&self) -> Option<&Arc<World>> {
        self.demo.as_ref()
    }

    pub(crate) fn inventory_path(&self) -> &Path {
        &self.paths.inventory
    }

    /// The machine's inventory entry and its last check.
    pub(crate) fn machine(&self, id: &str) -> Option<(MachineSpec, Option<ProbeResult>)> {
        let st = self.state.lock().unwrap();
        let spec = st.inventory.machines.iter().find(|m| m.id == id)?.clone();
        Some((spec, st.probes.get(id).cloned()))
    }

    /// Re-reads fleet.toml right away (after Muxen edited it) and publishes.
    pub(crate) fn reload_inventory_now(&self) {
        let mut st = self.state.lock().unwrap();
        match &self.demo {
            Some(w) => {
                st.inventory = w.inventory();
                st.inventory_info.loaded_at = Some(Utc::now());
            }
            None => {
                self.reload_inventory(&mut st, true);
            }
        }
        let known: HashSet<String> = st.inventory.machines.iter().map(|m| m.id.clone()).collect();
        st.probes.retain(|id, _| known.contains(id));
        self.publish(&mut st);
    }

    /// Starts tracking an operation and returns its id.
    pub(crate) fn begin_op(&self, kind: &str, title: String, machine_id: Option<String>, runner_id: Option<String>) -> u64 {
        let mut st = self.state.lock().unwrap();
        let id = st.next_op;
        st.next_op += 1;
        st.operations.push_front(Operation {
            id,
            kind: kind.into(),
            title,
            machine_id,
            runner_id,
            status: "running".into(),
            started_at: Utc::now(),
            finished_at: None,
            log: Vec::new(),
            error: None,
        });
        // Drop the oldest finished operations.
        while st.operations.len() > MAX_OPERATIONS {
            match st.operations.iter().rposition(|o| o.status != "running") {
                Some(i) => {
                    st.operations.remove(i);
                }
                None => break,
            }
        }
        self.publish(&mut st);
        id
    }

    /// Appends output to an operation. Publishing is coalesced.
    pub(crate) fn op_log(self: &Arc<Self>, id: u64, line: impl Into<String>) {
        let mut st = self.state.lock().unwrap();
        let Some(op) = st.operations.iter_mut().find(|o| o.id == id) else { return };
        op.log.push(line.into());
        if op.log.len() > MAX_OP_LOG_LINES {
            let extra = op.log.len() - MAX_OP_LOG_LINES;
            op.log.drain(..extra);
        }
        let due = st.last_publish.is_none_or(|t| t.elapsed() >= OP_PUBLISH_INTERVAL);
        if due {
            self.publish(&mut st);
        } else if !st.publish_scheduled {
            st.publish_scheduled = true;
            let me = self.clone();
            self.rt.spawn(async move {
                tokio::time::sleep(OP_PUBLISH_INTERVAL).await;
                let mut st = me.state.lock().unwrap();
                st.publish_scheduled = false;
                me.publish(&mut st);
            });
        }
    }

    pub(crate) fn op_finish(&self, id: u64, result: Result<(), String>) {
        let mut st = self.state.lock().unwrap();
        if let Some(op) = st.operations.iter_mut().find(|o| o.id == id) {
            op.finished_at = Some(Utc::now());
            match result {
                Ok(()) => op.status = "succeeded".into(),
                Err(e) => {
                    op.status = "failed".into();
                    op.error = Some(e);
                }
            }
        }
        self.publish(&mut st);
    }

    /// True while another operation is changing this machine.
    pub(crate) fn machine_busy(&self, machine_id: &str) -> bool {
        let st = self.state.lock().unwrap();
        st.operations
            .iter()
            .any(|o| o.status == "running" && o.kind != "logs" && o.machine_id.as_deref() == Some(machine_id))
    }

    /// Forgets finished operations (the activity panel's "Clear").
    /// The stored workspaces (see fleet::workspaces).
    pub(crate) fn sessions_of(&self, machine_id: &str) -> Option<MachineSessions> {
        self.state.lock().unwrap().sessions.get(machine_id).cloned()
    }

    pub(crate) fn all_sessions(&self) -> Vec<(String, MachineSessions)> {
        self.state.lock().unwrap().sessions.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub(crate) fn set_sessions(&self, machine_id: &str, found: MachineSessions) {
        let mut st = self.state.lock().unwrap();
        let newest = found.events.iter().map(|e| e.at).max().unwrap_or(0);
        // The first look at a machine sets the baseline: old events don't notify.
        if let (Some(before), Some(&since)) = (st.sessions.get(machine_id), st.event_marks.get(machine_id)) {
            let name = st.inventory.machines.iter().find(|m| m.id == machine_id).map(|m| m.name.clone()).unwrap_or_else(|| machine_id.to_string());
            for (session_id, kind, title, message) in sessions::new_notices(before, &found, since, &name) {
                let id = st.next_notice;
                st.next_notice += 1;
                st.attention.insert(session_id.clone(), kind.clone());
                st.notices.push_front(SessionNotice { id, session_id, machine_id: machine_id.to_string(), kind, title, message, at: Utc::now() });
            }
            st.notices.truncate(MAX_NOTICES);
        }
        let mark = st.event_marks.entry(machine_id.to_string()).or_insert(0);
        *mark = (*mark).max(newest);
        // Forget unread marks for sessions that are gone.
        let gone: Vec<String> = st
            .attention
            .keys()
            .filter(|id| id.starts_with(&format!("{machine_id}/")) && !found.sessions.iter().any(|s| &&s.id == id))
            .cloned()
            .collect();
        for id in gone {
            st.attention.remove(&id);
        }
        st.sessions.insert(machine_id.to_string(), found);
        self.publish(&mut st);
    }

    pub(crate) fn checkouts_of(&self, machine_id: &str) -> Vec<super::model::Checkout> {
        self.state.lock().unwrap().checkouts.get(machine_id).cloned().unwrap_or_default()
    }

    pub(crate) fn set_checkouts(&self, machine_id: &str, found: Vec<super::model::Checkout>) {
        let mut st = self.state.lock().unwrap();
        st.checkouts.insert(machine_id.to_string(), found);
        self.publish(&mut st);
    }

    pub(crate) fn direct_session(&self, id: &str) -> Option<DirectSession> {
        self.state.lock().unwrap().direct.iter().find(|d| d.info.id == id).cloned()
    }

    pub(crate) fn add_direct(&self, d: DirectSession) {
        let mut st = self.state.lock().unwrap();
        st.direct.insert(0, d);
        self.publish(&mut st);
    }

    pub fn remove_direct(&self, id: &str) {
        let mut st = self.state.lock().unwrap();
        st.direct.retain(|d| d.info.id != id);
        self.publish(&mut st);
    }

    /// A direct session's terminal exited ("exited") or was reopened ("running").
    pub fn set_direct_status(&self, id: &str, status: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st.direct.iter_mut().find(|d| d.info.id == id) {
            d.info.status = status.to_string();
            self.publish(&mut st);
        }
    }

    /// The user looked at the session: clear its unread notice.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn ack_session(&self, session_id: &str) {
        let mut st = self.state.lock().unwrap();
        if st.attention.remove(session_id).is_some() {
            self.publish(&mut st);
        }
    }

    pub(crate) fn workspace_records(&self) -> Vec<WorkspaceRecord> {
        self.state.lock().unwrap().workspaces.clone()
    }

    /// Changes the workspaces: in demo mode in memory, otherwise in workspaces.json
    /// under its lock (re-read first, so other processes' changes survive).
    pub(crate) fn update_workspaces<R>(&self, f: impl FnOnce(&mut Vec<WorkspaceRecord>) -> R) -> Result<R, String> {
        if self.demo.is_some() {
            let mut st = self.state.lock().unwrap();
            let result = f(&mut st.workspaces);
            self.publish(&mut st);
            return Ok(result);
        }
        let (result, list, modified) = workspaces::update_registry(&self.paths.workspaces, f)?;
        let mut st = self.state.lock().unwrap();
        st.workspaces = list;
        st.workspaces_mtime = modified;
        self.publish(&mut st);
        Ok(result)
    }

    /// Re-reads workspaces.json if another process changed it. Returns true if it did.
    fn reload_workspaces(&self, st: &mut State) -> bool {
        if self.demo.is_some() {
            return false;
        }
        let modified = mtime(&self.paths.workspaces);
        if modified == st.workspaces_mtime {
            return false;
        }
        st.workspaces_mtime = modified;
        st.workspaces = workspaces::load_registry(&self.paths.workspaces);
        true
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn clear_operations(&self) {
        let mut st = self.state.lock().unwrap();
        st.operations.retain(|o| o.status == "running");
        self.publish(&mut st);
    }

    // -----------------------------------------------------------------------

    async fn github_loop(self: Arc<Self>) {
        loop {
            self.sync_github().await;
            let poll = if self.demo.is_some() {
                DEMO_POLL
            } else {
                Duration::from_secs(self.state.lock().unwrap().settings.github_poll_seconds as u64)
            };
            tokio::select! {
                _ = tokio::time::sleep(poll) => {}
                _ = self.sync_now.notified() => {}
            }
            // Pick up fleet.toml edits (and settings/checks from other Muxen
            // processes) without waiting for the next full check.
            let before = self.state.lock().unwrap().inventory.machines.len();
            self.refresh_from_disk();
            let new_machines: Vec<String> = {
                let st = self.state.lock().unwrap();
                if st.inventory.machines.len() != before {
                    st.inventory.machines.iter().filter(|m| !st.probes.contains_key(&m.id)).map(|m| m.id.clone()).collect()
                } else {
                    Vec::new()
                }
            };
            if !new_machines.is_empty() {
                let me = self.clone();
                self.rt.spawn(async move { me.run_check(Some(new_machines)).await });
            }
        }
    }

    pub(crate) async fn sync_github(&self) {
        let settings = {
            let mut st = self.state.lock().unwrap();
            st.github_syncing = true;
            self.publish(&mut st);
            st.settings.clone()
        };
        let configured = !settings.github.orgs.is_empty() || !settings.github.repos.is_empty();
        let (data, token_source) = match &self.demo {
            Some(w) => (Some(w.github(&settings)), "gh".to_string()),
            None if configured => {
                let data = self.github.sync(&settings.github).await;
                let source = data.token_source.clone();
                (Some(data), source)
            }
            None => (None, self.github.token_source().await),
        };
        let mut st = self.state.lock().unwrap();
        // Settings may have changed while we were syncing; don't resurrect removed scopes.
        if st.settings.github == settings.github {
            st.github = data;
        }
        st.token_source = token_source;
        st.github_syncing = false;
        self.publish(&mut st);
    }

    /// Keeps session lists fresh on machines that have sessions, so the sidebar
    /// shows which agents are working.
    /// Refreshes a machine's sessions whenever its agent says they changed
    /// (one started or ended, started or stopped printing, rang the bell).
    async fn follow_link(self: Arc<Self>, link: Arc<super::link::Link>) {
        // A paired machine's local addresses change (DHCP, another network):
        // keep them current, so the next direct connection takes the short way.
        if let Some(mut paired) = super::direct::paired(&link.machine) {
            if let Ok(r) = link.request(serde_json::json!({ "op": "addresses" }), Duration::from_secs(10)).await {
                let lan: Vec<String> = r["addresses"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                if r["ok"].as_bool() == Some(true) && lan != paired.lan {
                    paired.lan = lan;
                    let _ = super::direct::save(&link.machine, Some(paired));
                    // Reconnect the short way, unless this link already is.
                    if link.transport() != "direct" || link.network() != Some("Local network") {
                        super::link::reset(&link.machine);
                    }
                }
            }
        }
        let mut events = link.subscribe();
        if link.request(serde_json::json!({ "op": "watch" }), Duration::from_secs(10)).await.is_err() {
            return;
        }
        loop {
            match events.recv().await {
                Ok(e) if e["event"] == "sessions" => {
                    // A burst of changes is one refresh.
                    while events.try_recv().is_ok() {}
                    let _ = self.refresh_sessions(&link.machine).await;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    }

    async fn sessions_loop(self: Arc<Self>) {
        if self.demo.is_some() {
            return;
        }
        // At startup, look everywhere the last check reached.
        let reachable: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.inventory.machines.iter().filter(|m| m.is_checkable() && st.probes.get(&m.id).is_some_and(|p| p.ok)).map(|m| m.id.clone()).collect()
        };
        let mut first: FuturesUnordered<_> = reachable.iter().map(|id| self.refresh_sessions(id)).collect();
        while first.next().await.is_some() {}
        loop {
            tokio::time::sleep(sessions::REFRESH_EVERY).await;
            let ids = self.machines_with_sessions();
            let mut pending: FuturesUnordered<_> = ids.iter().map(|id| self.refresh_sessions(id)).collect();
            while pending.next().await.is_some() {}
        }
    }

    async fn check_loop(self: Arc<Self>) {
        loop {
            let (interval, last) = {
                let st = self.state.lock().unwrap();
                (st.settings.check_interval_minutes, st.check_finished_at)
            };
            let wait = match (interval, last) {
                // Never checked: check once at startup even when automatic checks are off.
                (_, None) => Some(Duration::ZERO),
                (0, Some(_)) => None,
                (m, Some(t)) => {
                    let due = t + chrono::Duration::minutes(m as i64);
                    Some((due - Utc::now()).to_std().unwrap_or(Duration::ZERO))
                }
            };
            match wait {
                Some(d) => {
                    tokio::select! {
                        _ = tokio::time::sleep(d) => self.run_check(None).await,
                        _ = self.settings_changed.notified() => {}
                    }
                }
                None => self.settings_changed.notified().await,
            }
        }
    }

    /// Probes the given machines (or all), publishing as each one finishes.
    pub(crate) async fn run_check(self: &Arc<Self>, only: Option<Vec<String>>) {
        let full = only.is_none();
        let jobs: Vec<(MachineSpec, Option<probe::Shell>)> = {
            let mut st = self.state.lock().unwrap();
            if full && st.check_running {
                return;
            }
            if self.demo.is_none() {
                self.reload_inventory(&mut st, false);
            }
            let specs: Vec<MachineSpec> = st
                .inventory
                .machines
                .iter()
                .filter(|m| m.is_checkable())
                .filter(|m| only.as_ref().is_none_or(|ids| ids.contains(&m.id)))
                .filter(|m| !st.checking.contains(&m.id))
                .cloned()
                .collect();
            if full {
                st.check_running = true;
                st.check_started_at = Some(Utc::now());
            }
            for s in &specs {
                st.checking.insert(s.id.clone());
            }
            self.publish(&mut st);
            specs.into_iter().map(|s| {
                let shell = st.probes.get(&s.id).and_then(|p| p.shell);
                (s, shell)
            }).collect()
        };

        let mut pending: FuturesUnordered<_> = jobs
            .into_iter()
            .map(|(spec, shell)| {
                let demo = self.demo.clone();
                async move {
                    let result = match demo {
                        Some(w) => w.probe(&spec).await,
                        None => probe::probe(&spec, shell).await,
                    };
                    (spec.id, result)
                }
            })
            .collect();

        while let Some((id, result)) = pending.next().await {
            let reachable = result.ok;
            let mut st = self.state.lock().unwrap();
            st.checking.remove(&id);
            st.probes.insert(id.clone(), result);
            self.publish(&mut st);
            drop(st);
            // Sessions and installed agents come along with every machine check.
            if reachable && self.demo.is_none() {
                let me = self.clone();
                self.rt.spawn(async move {
                    let _ = me.refresh_sessions(&id).await;
                });
            }
        }

        let mut st = self.state.lock().unwrap();
        if full {
            st.check_running = false;
            st.check_finished_at = Some(Utc::now());
            let known: HashSet<String> = st.inventory.machines.iter().map(|m| m.id.clone()).collect();
            st.probes.retain(|id, _| known.contains(id));
            self.settings_changed.notify_waiters(); // reschedule the next automatic check
        }
        if self.demo.is_none() {
            // Another process may have checked machines since we last looked;
            // fold those in first so this write doesn't throw them away.
            self.merge_disk_state(&mut st);
            let persisted = PersistedState {
                probes: st.probes.clone(),
                check_started_at: st.check_started_at,
                check_finished_at: st.check_finished_at,
            };
            match persisted.save(&self.paths.state) {
                Ok(()) => st.state_mtime = mtime(&self.paths.state),
                Err(e) => tracing::warn!("saving state: {e:#}"),
            }
        }
        self.publish(&mut st);
    }

    /// Re-reads settings.json if another process changed it. Returns true if the settings changed.
    fn reload_settings(&self, st: &mut State) -> bool {
        let modified = mtime(&self.paths.settings);
        if modified == st.settings_mtime {
            return false;
        }
        st.settings_mtime = modified;
        let loaded = Settings::load(&self.paths.settings);
        if loaded == st.settings {
            return false;
        }
        if loaded.github != st.settings.github && loaded.github.orgs.is_empty() && loaded.github.repos.is_empty() {
            st.github = None;
        }
        st.settings = loaded;
        true
    }

    /// Merges state.json into our probes when another process wrote it: per
    /// machine, the newer check wins, and machines we're checking right now are
    /// left alone. Returns true if anything changed.
    fn merge_disk_state(&self, st: &mut State) -> bool {
        let modified = mtime(&self.paths.state);
        if modified == st.state_mtime {
            return false;
        }
        st.state_mtime = modified;
        let disk = PersistedState::load(&self.paths.state);
        let known: HashSet<&str> = st.inventory.machines.iter().map(|m| m.id.as_str()).collect();
        let mut newer = Vec::new();
        for (id, probe) in disk.probes {
            let ours = st.probes.get(&id).map(|p| p.checked_at);
            if known.contains(id.as_str()) && !st.checking.contains(&id) && ours.is_none_or(|t| probe.checked_at > t) {
                newer.push((id, probe));
            }
        }
        let mut changed = !newer.is_empty();
        st.probes.extend(newer);
        if disk.check_finished_at > st.check_finished_at {
            st.check_started_at = disk.check_started_at;
            st.check_finished_at = disk.check_finished_at;
            changed = true;
        }
        changed
    }

    /// Re-reads fleet.toml when it changed (or `force`). Returns true if the inventory changed.
    /// On a parse error the last good inventory stays in place and the error is shown.
    fn reload_inventory(&self, st: &mut State, force: bool) -> bool {
        let path = &self.paths.inventory;
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if !force && mtime == st.inventory_mtime {
            return false;
        }
        st.inventory_mtime = mtime;
        st.inventory_info.path = display_path(path);
        match Inventory::load(path) {
            Ok(inv) => {
                st.inventory_info.error = None;
                st.inventory_info.warnings = inv.warnings.clone();
                st.inventory_info.loaded_at = Some(Utc::now());
                st.inventory = inv;
                true
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if st.inventory_info.error.as_deref() != Some(&msg) {
                    tracing::warn!("{msg}");
                }
                st.inventory_info.error = Some(msg);
                true
            }
        }
    }

    fn publish(&self, st: &mut State) {
        st.version += 1;
        st.last_publish = Some(Instant::now());
        let interval = st.settings.check_interval_minutes;
        let next_at = match (interval, st.check_finished_at) {
            (0, _) => None,
            (m, Some(t)) => Some(t + chrono::Duration::minutes(m as i64)),
            (_, None) => None,
        };
        let mut snapshot = build(&Inputs {
            now: Utc::now(),
            demo: self.demo.is_some(),
            inventory: &st.inventory,
            inventory_info: st.inventory_info.clone(),
            settings: &st.settings,
            probes: &st.probes,
            checking: &st.checking,
            check: CheckInfo {
                running: st.check_running,
                started_at: st.check_started_at,
                finished_at: st.check_finished_at,
                next_at,
                interval_minutes: interval,
            },
            github: st.github.as_ref(),
            github_syncing: st.github_syncing,
            token_source: st.token_source.clone(),
        });
        snapshot.version = st.version;
        snapshot.operations = st.operations.iter().cloned().collect();
        snapshot.workspaces = workspaces::snapshot_infos(&st.workspaces, &snapshot.machines, &st.inventory.machines);
        for m in &mut snapshot.machines {
            if let Some(found) = st.sessions.get(&m.id) {
                m.agents = found.agents.clone();
                m.agent_version = found.agent_version.clone();
                m.gh = found.gh.clone();
                m.sessions_error = found.error.clone();
                snapshot.sessions.extend(found.sessions.iter().cloned().map(|mut s| {
                    s.attention = st.attention.get(&s.id).cloned().or(s.attention);
                    s
                }));
            }
        }
        // Sessions without the agent come after the machine's agent ones.
        for d in &st.direct {
            let at = snapshot.sessions.iter().rposition(|s| s.machine_id == d.info.machine_id).map(|i| i + 1).unwrap_or(snapshot.sessions.len());
            snapshot.sessions.insert(at, d.info.clone());
        }
        snapshot.notifications = st.notices.iter().cloned().collect();
        let known: Vec<&String> = st.inventory.machines.iter().map(|m| &m.id).collect();
        snapshot.checkouts = st.checkouts.iter().filter(|(id, _)| known.contains(id)).flat_map(|(_, c)| c.iter().cloned()).collect();
        snapshot.checkouts.sort_by(|a, b| a.repo.cmp(&b.repo).then(a.machine_id.cmp(&b.machine_id)).then(a.path.cmp(&b.path)));
        self.tx.send_replace(Arc::new(snapshot));
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Writes a starter fleet.toml (this computer only) the first time Muxen runs.
pub fn ensure_inventory(paths: &Paths) {
    if paths.inventory.exists() {
        return;
    }
    match write_atomic(&paths.inventory, default_inventory().as_bytes()) {
        Ok(()) => tracing::info!("created {}", display_path(&paths.inventory)),
        Err(e) => tracing::warn!("couldn't create {}: {e:#}", display_path(&paths.inventory)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two Muxen processes (say, the window and an agent's MCP server) share one config dir.
    #[tokio::test(flavor = "multi_thread")]
    async fn picks_up_changes_from_other_processes() {
        let dir = std::env::temp_dir().join(format!("muxen-xproc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fleet.toml"), "[[machine]]\nname = \"Box\"\nhost = \"box.invalid\"\n").unwrap();
        let paths = Paths::resolve(Some(dir.clone()), None);
        let window = Engine::new(paths.clone(), false);
        let agent = Engine::new(paths.clone(), false);

        // Settings saved by one show up in the other.
        let mut s = agent.settings();
        s.github.orgs = vec!["acme".into()];
        agent.update_settings(s).unwrap();
        window.refresh_from_disk();
        assert_eq!(window.settings().github.orgs, vec!["acme"]);

        // A newer check written by one replaces the other's; an older one doesn't.
        let probe = |ok: bool, mins_ago: i64| ProbeResult {
            checked_at: Utc::now() - chrono::Duration::minutes(mins_ago),
            ok,
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            ..Default::default()
        };
        let write = |p: ProbeResult| {
            let mut state = PersistedState::default();
            state.check_finished_at = Some(p.checked_at);
            state.probes.insert("box".into(), p);
            state.save(&paths.state).unwrap();
        };
        write(probe(true, 1));
        window.refresh_from_disk();
        assert_eq!(window.current().machines[0].status, "online");
        assert_eq!(window.current().machines[0].platform, "linux-x64");

        write(probe(false, 30));
        std::thread::sleep(Duration::from_millis(5));
        window.refresh_from_disk();
        assert_eq!(window.current().machines[0].status, "online", "an older check must not win");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
