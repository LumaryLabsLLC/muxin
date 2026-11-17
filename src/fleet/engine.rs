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

mod roaming;

use super::build::{build, Inputs};
use super::model::{CheckInfo, InventoryInfo, Operation, Snapshot};
use super::sessions::{self, DirectSession, MachineSessions, SessionInfo, SessionNotice};
use super::workspaces::{self as workspaces, WorkspaceRecord};
use crate::config::{default_inventory, display_path, write_atomic, Inventory, MachineSpec, Paths, PersistedState, Settings};
use crate::demo::World;
use crate::github::{self, GitHubData, ScopeTest};
use crate::probe::{self, ProbeResult};

const DEMO_POLL: Duration = Duration::from_secs(3);
const MAX_OPERATIONS: usize = 30;
const MAX_OP_LOG_LINES: usize = 2000;
const MAX_NOTICES: usize = 50;
/// How often to look for an offline machine coming back.
const OFFLINE_RECHECK: Duration = Duration::from_secs(30);
/// How long a machine check waits for the cloud catch-up it started (see `run_check`).
const CATCH_UP_WAIT: Duration = Duration::from_secs(20);
/// How long a machine's promise to say when it wakes is taken at its word:
/// past that, a check of every machine looks at it again, this often.
const PROMISE_HOLDS: Duration = Duration::from_secs(2 * 3600);
/// A machine whose connection for saying it woke keeps ending is looked at,
/// or has that connection made again, this often at most.
const HEAR_AGAIN_EVERY: Duration = Duration::from_secs(600);
/// How long a machine counts as having just woken: its link may still be on
/// its way back to the direct connection (`Engine::just_woke`).
const JUST_WOKE: Duration = Duration::from_secs(120);
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
    /// by another Muxin process (the window, the CLI, an agent's MCP
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
    /// What the work in sessions' and workspaces' folders changed, per machine
    /// and folder, and when each machine was last looked at (see sessions::git).
    git: HashMap<String, Vec<(String, sessions::git::GitStatus)>>,
    git_looked: HashMap<String, Instant>,
    /// Branches' pull requests, by (repository, branch), and when GitHub was asked.
    pulls: HashMap<(String, String), (Instant, Option<crate::github::hub::BranchPull>)>,
    /// Machines on the user's tailnet, as Tailscale last saw them (see roaming.rs).
    tailnet: HashMap<String, super::model::TailnetDevice>,
    /// Tailscale on this computer; None until read, or when it isn't installed.
    tailscale: Option<super::model::TailscaleStatus>,
    /// Cloud machines' power as the provider last reported it ("active", "off", ...).
    droplets: HashMap<String, String>,
    /// Why the provider couldn't be asked about cloud machines that wait for
    /// their address, when the user has to do something about it (sign in again).
    cloud_trouble: Option<String>,
    /// Machines' monitors, as last listed (see fleet::desktop).
    displays: HashMap<String, Vec<super::model::DisplayInfo>>,
    /// Monitors' sizes from before Muxin first changed them, by machine and
    /// monitor, until they're back at them.
    display_before: HashMap<(String, String), (u32, u32)>,
    /// Stop flags of the operations that can be cancelled.
    cancels: HashMap<u64, Arc<std::sync::atomic::AtomicBool>>,
    /// Machines asleep whose agents tell this app when they wake (see
    /// fleet::quiet): they aren't checked meanwhile.
    wake_notices: HashSet<String>,
    /// This app stays open and hears them wake (`start`), unless the user
    /// turned that off.
    takes_notices: bool,
    /// When each machine was last looked at because its agent could no
    /// longer say it woke (`notice_lost`).
    looked_for_lost: HashMap<String, Instant>,
    /// When the connection a machine that's up would say it woke on was last
    /// made again (`hear_again`).
    heard_again: HashMap<String, Instant>,
    /// When a machine last woke from a sleep, as far as this app saw.
    woke_at: HashMap<String, Instant>,
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

/// Tests never touch the user's own home. An engine with a machine of host
/// "local" runs this computer's agent from ~/.muxen (installing the build from
/// the checkout there) and puts hooks in ~/.claude and ~/.codex; in a test
/// that's a folder of the test run's own, from the first engine on.
#[cfg(test)]
fn isolate_home() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Short: the agent's socket goes in it, and a socket's path is limited to about 100 bytes.
        let home = std::env::temp_dir().join(format!("mxt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        // SAFETY: once per test run, before this engine runs anything; Rust's own reads of the
        // environment (Command, env::var) take the same lock as set_var.
        unsafe {
            std::env::set_var("HOME", &home);
            for var in ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "COPILOT_HOME", "QWEN_HOME", "KIMI_CODE_HOME", "XDG_CONFIG_HOME"] {
                std::env::remove_var(var);
            }
        }
    });
}

impl Engine {
    /// Must be called from within a tokio runtime; background work runs there.
    pub fn new(paths: Paths, demo: bool) -> Arc<Engine> {
        #[cfg(test)]
        isolate_home();
        let world = demo.then(|| Arc::new(World::new()));
        let (inventory, inventory_info, settings, persisted) = match &world {
            Some(w) => (
                w.inventory(),
                InventoryInfo { path: "demo://config.toml".into(), loaded_at: Some(Utc::now()), ..Default::default() },
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
        for (id, p) in &persisted.probes {
            crate::fleet::sessions::agent::note_os(id, p.os.as_deref());
        }
        // Prompt rules fetched before (sessions::rules).
        if world.is_none() {
            sessions::rules::load(&paths.dir);
        }
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
            // The demo's are there from the start (a real machine's are looked for).
            checkouts: match &world {
                Some(w) => w.inventory().machines.iter().map(|m| (m.id.clone(), w.checkouts(&m.id))).filter(|(_, c)| !c.is_empty()).collect(),
                None => HashMap::new(),
            },
            git: HashMap::new(),
            git_looked: HashMap::new(),
            pulls: HashMap::new(),
            tailnet: persisted.tailnet,
            tailscale: None,
            droplets: persisted.droplets,
            cloud_trouble: None,
            displays: HashMap::new(),
            display_before: HashMap::new(),
            cancels: HashMap::new(),
            wake_notices: HashSet::new(),
            takes_notices: false,
            looked_for_lost: HashMap::new(),
            heard_again: HashMap::new(),
            woke_at: HashMap::new(),
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
        // A machine that's asleep says so to whatever reaches for it.
        let weak = Arc::downgrade(&engine);
        super::asleep::set_on_told(move |machine_id, told| {
            if let Some(engine) = weak.upgrade() {
                let _ = engine.machine_slept(machine_id, told);
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
                engine.reload_inventory(&mut st, true, None);
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
        if self.demo.is_none() {
            self.take_wake_notices();
            // The connections machines asleep would have said they woke on
            // went with the Muxin that ran before: one look at each (which
            // makes a new one).
            let me = self.clone();
            self.rt.spawn(async move {
                let asleep = me.asleep_machines();
                if !asleep.is_empty() {
                    me.run_check(Some(asleep)).await;
                }
            });
        }
        let me = self.clone();
        self.rt.spawn(async move { me.github_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.check_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.sessions_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.offline_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.tailnet_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.rules_loop().await });
        // A cloud machine whose create was cut off last time: found now, not
        // at the next check (there's none while automatic checks are off).
        let me = self.clone();
        self.rt.spawn(async move { me.cloud_catch_up().await });
        let me = self.clone();
        self.rt.spawn(async move { me.network_loop().await });
        // Monitors aren't kept between runs: ask the machines with a desktop
        // now, not at the next check (which may be minutes away).
        let me = self.clone();
        self.rt.spawn(async move {
            if me.demo.is_some() {
                return;
            }
            let snap = me.current();
            let ids: Vec<String> = snap
                .machines
                .iter()
                .filter(|m| !m.is_down() && !m.host.is_empty())
                .filter(|m| m.platform.starts_with("darwin") || m.probe.as_ref().and_then(|p| p.desktop) == Some(true))
                .map(|m| m.id.clone())
                .collect();
            futures::future::join_all(ids.iter().map(|id| me.displays(id))).await;
        });
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
        let mut new = new.normalized()?;
        // When each agent choice changed: machines keep the newest (config::AgentSettings).
        let before = self.settings().agents;
        new.agents.stamp(&before, Utc::now().timestamp());
        if self.demo.is_none() {
            new.save(&self.paths.settings).map_err(|e| format!("Couldn't save settings: {e:#}"))?;
        }
        let favorites_only = {
            let mut st = self.state.lock().unwrap();
            st.settings_mtime = mtime(&self.paths.settings);
            let scopes_changed = st.settings.github != new.github;
            if st.settings.wake_notices != new.wake_notices {
                self.hear_wakes(&mut st, new.wake_notices);
            }
            if st.settings.share_clipboard != new.share_clipboard {
                if new.share_clipboard {
                    for m in st.inventory.machines.iter().filter(|m| !m.is_local()) {
                        if let Some(l) = super::link::current(&m.id) {
                            self.rt.spawn(super::clipboard::share(l));
                        }
                    }
                } else {
                    super::clipboard::stop_all();
                }
            }
            let favorites_only = Settings { favorite_repos: new.favorite_repos.clone(), snippets: new.snippets.clone(), ..st.settings.clone() } == new;
            st.settings = new.clone();
            if scopes_changed && new.github.orgs.is_empty() && new.github.repos.is_empty() {
                st.github = None;
            }
            self.publish(&mut st);
            favorites_only
        };
        // Starring a repository or saving a snippet needn't re-poll GitHub or reschedule checks.
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

    /// An app that stays open hears from machines that wake (on a connection
    /// it made to each beforehand: fleet::quiet), so it needn't look at them
    /// while they sleep. Unless the user turned that off
    /// (Settings::wake_notices). `start` calls this, once.
    pub(crate) fn take_wake_notices(self: &Arc<Self>) {
        let (woke, lost) = (Arc::downgrade(self), Arc::downgrade(self));
        super::quiet::take(
            move |machine_id| {
                if let Some(engine) = woke.upgrade() {
                    let id = machine_id.to_string();
                    let rt = engine.rt.clone();
                    rt.spawn(async move { engine.machine_woke(&id).await });
                }
            },
            move |machine_id| {
                if let Some(engine) = lost.upgrade() {
                    let id = machine_id.to_string();
                    let rt = engine.rt.clone();
                    rt.spawn(async move { engine.notice_lost(&id).await });
                }
            },
        );
        let mut st = self.state.lock().unwrap();
        st.takes_notices = st.settings.wake_notices;
        super::quiet::set_on(st.takes_notices);
    }

    /// A machine's agent said it's going to sleep (or, to something that
    /// reached for it, that it's asleep): Muxin lets go of it, and leaves it
    /// alone until it wakes. False for a machine it doesn't apply to (this
    /// computer): nothing changed, and its link stays.
    pub(crate) fn machine_slept(&self, machine_id: &str, told: &super::asleep::Told) -> bool {
        {
            let mut st = self.state.lock().unwrap();
            // This computer runs Muxin: it isn't asleep.
            if !st.inventory.machines.iter().any(|m| m.id == machine_id && !m.is_local()) {
                return false;
            }
            let result = ProbeResult::asleep(Instant::now(), told).after(st.probes.get(machine_id));
            self.keep_probe(&mut st, machine_id, result);
            if self.demo.is_none() {
                self.save_state(&mut st);
            }
            self.publish(&mut st);
        }
        // While it still hears: a goodbye later would wake it.
        super::link::reset(machine_id);
        true
    }

    /// A machine's agent said it has woken: look at it now. Only one that
    /// was asleep (an agent can't have Muxin check its machine at will).
    pub(crate) async fn machine_woke(self: &Arc<Self>, machine_id: &str) {
        let was_asleep = {
            let mut st = self.state.lock().unwrap();
            // It said it's awake, and its promise to say so is kept: a look
            // that gets no answer now finds it offline, not still asleep.
            let was = st.probes.get_mut(machine_id).filter(|p| p.asleep).map(|p| (p.asleep, p.wake_notice) = (false, None)).is_some();
            if was {
                st.wake_notices.remove(machine_id);
            }
            // Whether a check is on its way to the machine already.
            was.then(|| st.checking.contains(machine_id))
        };
        let Some(in_flight) = was_asleep else {
            // Up all along, as far as this app knew: it slept and woke
            // between two words from it. The connection it said so on is
            // used up, and its next sleep needs one.
            self.hear_again(machine_id);
            return;
        };
        tracing::debug!("{machine_id} woke; checking it");
        self.note_woke(machine_id);
        let me = vec![machine_id.to_string()];
        self.run_check(Some(me.clone())).await;
        // That check was reaching the machine as it woke, and what it brought
        // back is from before: asleep, it says. Looked at again, now.
        if in_flight && self.state.lock().unwrap().probes.get(machine_id).is_some_and(|p| p.asleep) {
            tracing::debug!("{machine_id} woke while a check was reaching it; checking it again");
            self.run_check(Some(me)).await;
        }
        self.direct_again_soon(machine_id);
    }

    /// The machine woke just now (its agent said so, or Wake woke it). Its
    /// direct way is tried at once by whatever links to it next: a try that
    /// got no answer while it slept, or as it was being woken, isn't held
    /// against it.
    pub(crate) fn note_woke(&self, machine_id: &str) {
        super::direct::retry_now(machine_id);
        self.state.lock().unwrap().woke_at.insert(machine_id.to_string(), Instant::now());
    }

    /// Whether the machine woke a moment ago. A link to it over ssh then
    /// isn't a sign that something blocks the direct way: it was made as
    /// the machine woke, and moves back by itself (`direct_again_soon`).
    pub(crate) fn just_woke(&self, machine_id: &str) -> bool {
        self.state.lock().unwrap().woke_at.get(machine_id).is_some_and(|at| at.elapsed() < JUST_WOKE)
    }

    /// After a wake, a paired machine's link may have come up over ssh all
    /// the same: its direct way didn't answer yet while it woke. Tried again
    /// in a moment, twice, so the link is direct (and the connection for its
    /// next wake is made: only a direct link has one) without waiting for
    /// the next look at the network.
    pub(crate) fn direct_again_soon(self: &Arc<Self>, machine_id: &str) {
        let over_ssh = |id: &str| super::direct::paired(id).is_some() && super::link::current(id).is_some_and(|l| l.transport() == "ssh");
        if self.demo.is_some() || !over_ssh(machine_id) {
            return;
        }
        let (me, id) = (self.clone(), machine_id.to_string());
        self.rt.spawn(async move {
            for wait in [3, 12] {
                tokio::time::sleep(Duration::from_secs(wait)).await;
                if !over_ssh(&id) {
                    return;
                }
                tracing::debug!("{id} woke and is linked over ssh; trying its direct connection again");
                me.move_home(&id).await;
            }
        });
    }

    /// The connection a machine asleep would have said it woke on is gone,
    /// without a word (its agent started again, the machine is shutting
    /// down, this computer left the network). Nobody will say it woke now,
    /// so checks look at it again; and once now, since something happened
    /// there. Not again and again, should that go wrong each time.
    ///
    /// Lost while the machine is up (something on the way dropped it), it's
    /// made again over the machine's link: its next sleep needs one.
    pub(crate) async fn notice_lost(self: &Arc<Self>, machine_id: &str) {
        let look = {
            let mut st = self.state.lock().unwrap();
            st.wake_notices.remove(machine_id);
            let promised = st.probes.get_mut(machine_id).filter(|p| p.asleep && p.wake_notice.is_some()).map(|p| p.wake_notice = None).is_some();
            if !promised {
                let up = st.probes.get(machine_id).is_some_and(|p| !p.asleep);
                drop(st);
                if up {
                    self.hear_again(machine_id);
                }
                return;
            }
            if self.demo.is_none() {
                self.save_state(&mut st);
            }
            self.publish(&mut st);
            let before = st.looked_for_lost.insert(machine_id.to_string(), Instant::now());
            before.is_none_or(|at| at.elapsed() > HEAR_AGAIN_EVERY)
        };
        if look {
            tracing::debug!("{machine_id} can no longer say when it wakes; checking it");
            self.run_check(Some(vec![machine_id.to_string()])).await;
        }
    }

    /// The connection a machine would say it woke on ended while the machine
    /// is up. Made again over its link, in the app that hears machines wake.
    /// Not again and again, should it end each time: then its next sleep has
    /// none, and checks look at it.
    fn hear_again(self: &Arc<Self>, machine_id: &str) {
        let spec = {
            let mut st = self.state.lock().unwrap();
            if !st.takes_notices || st.heard_again.get(machine_id).is_some_and(|at| at.elapsed() < HEAR_AGAIN_EVERY) {
                return;
            }
            let Some(spec) = st.inventory.machines.iter().find(|m| m.id == machine_id && m.is_connected() && !m.is_local()).cloned() else { return };
            st.heard_again.insert(machine_id.to_string(), Instant::now());
            spec
        };
        tracing::debug!("{machine_id} lost the connection it would say it woke on; making it again");
        self.rt.spawn(async move {
            let _ = super::link::wait_to_hear_again(&spec).await;
        });
    }

    /// Stores a machine's check, and whether its wake notice comes here.
    fn keep_probe(&self, st: &mut State, machine_id: &str, mut result: ProbeResult) {
        // In the app that hears machines wake, a promise is the connection
        // it holds for the machine, no more and no less. What an earlier
        // check wrote down (the Muxin that ran before this one, then a look
        // that got no answer) is none without it: nobody would say it woke.
        // And only while a word would still arrive on it (`quiet::holds`).
        if result.asleep && st.takes_notices {
            result.wake_notice = super::quiet::holds(machine_id);
        }
        if result.asleep && result.wake_notice.is_some() && st.takes_notices {
            st.wake_notices.insert(machine_id.to_string());
        } else {
            st.wake_notices.remove(machine_id);
        }
        st.probes.insert(machine_id.to_string(), result);
    }

    /// The machines asleep now.
    pub(crate) fn asleep_machines(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        st.inventory.machines.iter().filter(|m| m.is_connected() && st.probes.get(&m.id).is_some_and(|p| p.asleep)).map(|m| m.id.clone()).collect()
    }

    /// Those whose wake notice would no longer arrive (this computer moved to
    /// another network): they have to be looked at.
    pub(crate) fn asleep_out_of_reach(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        st.wake_notices.iter().filter(|id| !st.probes.get(*id).and_then(|p| p.wake_notice.as_deref()).is_some_and(super::quiet::arrives)).cloned().collect()
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

    /// Picks up what other Muxin processes wrote to the shared config
    /// directory: config.toml edits (machines and settings), and newer machine
    /// checks in state.json. A machine one of them disconnected or removed
    /// loses its link here (`reload_inventory`). Cheap when nothing changed
    /// (three `stat` calls).
    pub fn refresh_from_disk(&self) {
        if self.demo.is_some() {
            return;
        }
        let settings_changed = {
            let mut st = self.state.lock().unwrap();
            let inventory_changed = self.reload_inventory(&mut st, false, None);
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

    /// Publishes the snapshot again (when something outside the state changed, like idle times).
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub(crate) fn republish(&self) {
        let mut st = self.state.lock().unwrap();
        self.publish(&mut st);
    }

    /// Re-reads config.toml right away (after Muxin edited it) and publishes.
    pub(crate) fn reload_inventory_now(&self) {
        self.reload_inventory_keeping(None);
    }

    /// The same, after this process took `keep` out of config.toml while its
    /// agent is still to be told (`unpair_removed`): that machine's link
    /// stays up for it, and is the caller's to drop.
    pub(crate) fn reload_inventory_keeping(&self, keep: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        match &self.demo {
            Some(w) => {
                st.inventory = w.inventory();
                st.inventory_info.loaded_at = Some(Utc::now());
            }
            None => {
                self.reload_inventory(&mut st, true, keep);
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
            progress: None,
            cancellable: false,
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

    /// Makes an operation stoppable; the flag turns true when it's cancelled.
    pub(crate) fn op_cancellable(&self, id: u64) -> Arc<std::sync::atomic::AtomicBool> {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut st = self.state.lock().unwrap();
        st.cancels.insert(id, flag.clone());
        if let Some(op) = st.operations.iter_mut().find(|o| o.id == id) {
            op.cancellable = true;
        }
        self.publish(&mut st);
        flag
    }

    /// Asks a running operation to stop. False when it can't be (or already ended).
    pub fn cancel_op(&self, id: u64) -> bool {
        let st = self.state.lock().unwrap();
        match st.cancels.get(&id) {
            Some(flag) => {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// How far along an operation is. Publishing is coalesced.
    pub(crate) fn op_progress(self: &Arc<Self>, id: u64, progress: super::model::OpProgress) {
        let mut st = self.state.lock().unwrap();
        let Some(op) = st.operations.iter_mut().find(|o| o.id == id) else { return };
        op.progress = Some(progress);
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
        st.cancels.remove(&id);
        if let Some(op) = st.operations.iter_mut().find(|o| o.id == id) {
            op.finished_at = Some(Utc::now());
            op.cancellable = false;
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

    /// Records a cloud machine's power as the provider reports it ("active", "off", ...).
    pub(crate) fn set_droplet_status(&self, machine_id: &str, status: &str) {
        let mut st = self.state.lock().unwrap();
        if st.droplets.get(machine_id).map(String::as_str) != Some(status) {
            st.droplets.insert(machine_id.to_string(), status.to_string());
            self.save_state(&mut st);
            self.publish(&mut st);
        }
    }

    /// Notes why the provider can't be asked about cloud machines that wait
    /// for their address (None: it can).
    pub(crate) fn set_cloud_trouble(&self, trouble: Option<String>) {
        let mut st = self.state.lock().unwrap();
        if st.cloud_trouble != trouble {
            st.cloud_trouble = trouble;
            self.publish(&mut st);
        }
    }

    /// Remembers a destroyed cloud machine for this month's costs (and the
    /// next one's, for a while), and forgets its power state.
    pub(crate) fn record_destroyed(&self, machine_id: &str, entry: crate::config::DestroyedCloud) {
        if let Err(e) = crate::config::add_cloud_history(&self.paths().cloud_history, entry) {
            tracing::warn!("couldn't write {}: {e}", self.paths().cloud_history.display());
        }
        let mut st = self.state.lock().unwrap();
        st.droplets.remove(machine_id);
        self.save_state(&mut st);
    }

    pub(crate) fn cloud_history(&self) -> Vec<crate::config::DestroyedCloud> {
        crate::config::read_cloud_history(&self.paths().cloud_history)
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

    /// Takes newer agent choices a machine kept (made in another of the
    /// user's Muxins), so this one shows and sends them too.
    pub(crate) fn adopt_agent_settings(&self, kept: &serde_json::Value) {
        let mut st = self.state.lock().unwrap();
        let mut settings = st.settings.clone();
        if !settings.agents.adopt(kept) {
            return;
        }
        if self.demo.is_none() {
            if let Err(e) = settings.save(&self.paths.settings) {
                tracing::warn!("couldn't save settings: {e:#}");
                return;
            }
            st.settings_mtime = mtime(&self.paths.settings);
        }
        st.settings = settings;
        self.publish(&mut st);
    }

    pub(crate) fn git_looked_at(&self, machine_id: &str) -> Option<Instant> {
        self.state.lock().unwrap().git_looked.get(machine_id).copied()
    }

    pub(crate) fn set_git_looked(&self, machine_id: &str) {
        self.state.lock().unwrap().git_looked.insert(machine_id.to_string(), Instant::now());
    }

    /// What the work in a machine's folders changed (see sessions::git).
    pub(crate) fn set_git(&self, machine_id: &str, found: Vec<(String, sessions::git::GitStatus)>) {
        let mut st = self.state.lock().unwrap();
        if st.git.get(machine_id) == Some(&found) {
            return;
        }
        st.git.insert(machine_id.to_string(), found);
        self.publish(&mut st);
    }

    /// A branch's pull request as GitHub said it within `fresh`.
    pub(crate) fn cached_pull(&self, key: &(String, String), fresh: std::time::Duration) -> Option<Option<crate::github::hub::BranchPull>> {
        self.state.lock().unwrap().pulls.get(key).filter(|(at, _)| at.elapsed() < fresh).map(|(_, p)| p.clone())
    }

    pub(crate) fn cache_pull(&self, key: (String, String), pull: Option<crate::github::hub::BranchPull>) {
        let mut st = self.state.lock().unwrap();
        st.pulls.retain(|_, (at, _)| at.elapsed() < std::time::Duration::from_secs(3600));
        st.pulls.insert(key, (Instant::now(), pull));
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

    /// Keeps a monitor's size, as last listed, before Muxin changes it; the
    /// first one only, so a few changes in a row still lead back to where it
    /// was. False when the monitor hasn't been listed.
    pub(crate) fn remember_display_size(&self, machine_id: &str, display: &str) -> bool {
        let mut st = self.state.lock().unwrap();
        let Some(size) = st.displays.get(machine_id).and_then(|all| all.iter().find(|d| d.id == display)).map(|d| d.size()) else { return false };
        st.display_before.entry((machine_id.to_string(), display.to_string())).or_insert(size);
        true
    }

    /// What the machine's monitors are now (see fleet::desktop), each with
    /// its size from before Muxin changed it, while it's at another.
    pub(crate) fn set_displays(&self, machine_id: &str, mut displays: Vec<super::model::DisplayInfo>) {
        let mut st = self.state.lock().unwrap();
        for d in displays.iter_mut() {
            let key = (machine_id.to_string(), d.id.clone());
            d.before = st.display_before.get(&key).copied();
            if d.before == Some(d.size()) {
                // Back where it was: nothing to go back to.
                st.display_before.remove(&key);
                d.before = None;
            }
        }
        if st.displays.get(machine_id) != Some(&displays) {
            st.displays.insert(machine_id.to_string(), displays);
            self.publish(&mut st);
        }
    }

    /// Renames a direct session; its info afterwards.
    pub(crate) fn set_direct_title(&self, id: &str, title: &str) -> Option<SessionInfo> {
        let mut st = self.state.lock().unwrap();
        let d = st.direct.iter_mut().find(|d| d.info.id == id)?;
        d.info.title = title.to_string();
        let info = d.info.clone();
        self.publish(&mut st);
        Some(info)
    }

    /// A direct session's terminal exited ("exited") or was reopened ("running").
    pub fn set_direct_status(&self, id: &str, status: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st.direct.iter_mut().find(|d| d.info.id == id) {
            d.info.status = status.to_string();
            self.publish(&mut st);
        }
    }

    /// The user looked at the session: clear its unread notice (a "done"
    /// goes with it). The demo's sessions come with theirs.
    #[cfg_attr(not(any(feature = "gui", target_os = "ios")), allow(dead_code))]
    pub fn ack_session(&self, session_id: &str) {
        let mut st = self.state.lock().unwrap();
        let mut changed = st.attention.remove(session_id).is_some();
        for found in st.sessions.values_mut() {
            for s in found.sessions.iter_mut().filter(|s| s.id == session_id && s.attention.is_some()) {
                s.attention = None;
                changed = true;
            }
        }
        if changed {
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
            // Pick up config.toml edits (and settings/checks from other Muxin
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
        // The shared clipboard rides along (machines with a desktop; not this computer).
        let share = self.state.lock().unwrap().settings.share_clipboard;
        if share && self.machine(&link.machine).is_some_and(|(spec, _)| !spec.is_local()) {
            self.rt.spawn(super::clipboard::share(link.clone()));
        }
        // A paired machine's local addresses change (DHCP, another network):
        // keep them current, so the next direct connection takes the short way.
        if let Some(mut paired) = super::direct::paired(&link.machine) {
            if let Ok(r) = link.request(serde_json::json!({ "op": "addresses" }), Duration::from_secs(10)).await {
                let list = |k: &str| -> Vec<String> { r[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default() };
                // Agents before 0.2.1 don't report Tailscale addresses: keep what we had.
                let (lan, tailnet) = (list("addresses"), if r["tailnet"].is_array() { list("tailnet") } else { paired.tailnet.clone() });
                if r["ok"].as_bool() == Some(true) && (lan != paired.lan || tailnet != paired.tailnet) {
                    let lan_changed = lan != paired.lan;
                    paired.lan = lan;
                    paired.tailnet = tailnet;
                    let _ = super::direct::save(&link.machine, Some(paired));
                    // Reconnect the short way, unless this link already is.
                    if lan_changed && (link.transport() != "direct" || link.network() != Some("Local network")) {
                        super::link::reset(&link.machine);
                    }
                }
            }
            // Through Tailscale while the machine may be right here: look again
            // in a moment, once the dust settles.
            if link.alive() && link.transport() == "direct" && link.network() != Some("Local network") {
                let (me, id) = (self.clone(), link.machine.clone());
                self.rt.spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    me.move_home(&id).await;
                });
            }
        }
        let mut events = link.subscribe();
        if link.request(serde_json::json!({ "op": "watch" }), Duration::from_secs(10)).await.is_ok() {
            // What came in a burst of session changes and wasn't one (the
            // word that it's going to sleep, or woke): each keeps its turn.
            let mut kept: VecDeque<serde_json::Value> = VecDeque::new();
            loop {
                let e = match kept.pop_front() {
                    Some(e) => e,
                    None => tokio::select! {
                        e = events.recv() => match e {
                            Ok(e) => e,
                            // Some were lost on the way here, maybe the word
                            // that it's going to sleep: its next answer says
                            // (see `Link`'s reader), so ask for one.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let _ = link.request(serde_json::json!({ "op": "ping" }), Duration::from_secs(5)).await;
                                continue;
                            }
                            Err(_) => break,
                        },
                        _ = link.closed() => break,
                    },
                };
                match e["event"].as_str() {
                    Some("sessions") => {
                        // A burst of changes is one refresh.
                        kept.extend(drain_sessions(&mut events));
                        // Going to sleep: let go first, without one more look.
                        if kept.iter().any(|k| k["event"] == "sleep") {
                            continue;
                        }
                        let _ = self.refresh_sessions(&link.machine).await;
                    }
                    // Going to sleep: let go of it now, and leave it alone.
                    // It says it woke on the connection made for that, when
                    // there is one. (This computer's own agent says so too
                    // when the Mac sleeps; its link stays, and goes on being
                    // followed.)
                    Some("sleep") => {
                        let told = super::asleep::Told { notice: super::quiet::holds(&link.machine), ..super::asleep::Told::from(&e) };
                        if self.machine_slept(&link.machine, &told) {
                            break;
                        }
                    }
                    Some("awake") => self.machine_woke(&link.machine).await,
                    _ => {}
                }
            }
        }
        link.closed().await;
        // Lost, not dropped on purpose: the machine may have been shut down,
        // gone to sleep or left the network. Look now instead of at the next
        // scheduled check, so the app says so within seconds.
        if !link.retired() && !super::asleep::is(&link.machine) {
            tracing::debug!("lost the link to {}; checking it", link.machine);
            self.run_check(Some(vec![link.machine.clone()])).await;
        }
    }

    /// Newer prompt rules from muxin.dev, at most once a day (sessions::rules).
    async fn rules_loop(self: Arc<Self>) {
        if self.demo.is_some() {
            return;
        }
        loop {
            if sessions::rules::due(&self.paths.dir) {
                match sessions::rules::refresh(&self.paths.dir).await {
                    Ok(true) => tracing::info!("using newer prompt rules from muxin.dev"),
                    Ok(false) => {}
                    Err(e) => tracing::debug!("prompt rules: {e}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    }

    async fn sessions_loop(self: Arc<Self>) {
        if self.demo.is_some() {
            // What the demo's sessions changed, once.
            for id in self.machines_with_sessions() {
                let _ = self.look_at_git(&id).await;
            }
            return;
        }
        // At startup, look everywhere the last check reached.
        let reachable: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.inventory.machines.iter().filter(|m| m.is_connected() && st.probes.get(&m.id).is_some_and(|p| p.ok)).map(|m| m.id.clone()).collect()
        };
        let mut first: FuturesUnordered<_> = reachable.iter().map(|id| self.refresh_sessions(id)).collect();
        while first.next().await.is_some() {}
        loop {
            tokio::time::sleep(sessions::REFRESH_EVERY).await;
            let ids: Vec<String> = self.machines_with_sessions().into_iter().filter(|id| self.machine(id).is_some_and(|(spec, p)| !spec.disconnected && !p.is_some_and(|p| p.asleep))).collect();
            // A slow machine doesn't hold up the rest for long.
            let mut pending: FuturesUnordered<_> = ids.iter().map(|id| tokio::time::timeout(Duration::from_secs(15), self.refresh_sessions(id))).collect();
            while pending.next().await.is_some() {}
        }
    }

    /// Checks machines that went offline (or are still starting up) every
    /// little while, so one that's turned back on shows up again by itself.
    /// Off along with automatic checks.
    async fn offline_loop(self: Arc<Self>) {
        if self.demo.is_some() {
            return;
        }
        loop {
            tokio::time::sleep(OFFLINE_RECHECK).await;
            let ids: Vec<String> = {
                let st = self.state.lock().unwrap();
                if st.settings.check_interval_minutes == 0 || st.check_running {
                    continue;
                }
                st.inventory
                    .machines
                    .iter()
                    .filter(|m| m.is_connected() && !m.is_local() && !st.checking.contains(&m.id))
                    .filter(|m| {
                        // Tailscale says when a machine on the tailnet is back
                        // (see roaming.rs); those only need an occasional look.
                        let watched = st.tailscale.as_ref().is_some_and(|t| t.running) && st.tailnet.contains_key(&m.id);
                        st.probes.get(&m.id).is_some_and(|p| {
                            !p.ok
                                // Asleep: looking would wake it.
                                && !p.asleep
                                && super::build::comes_back_by_itself(super::build::unreachable_kind(p.error.as_deref().unwrap_or("")))
                                && (!watched || Utc::now() - p.checked_at > chrono::Duration::minutes(3))
                        })
                    })
                    .map(|m| m.id.clone())
                    .collect()
            };
            if !ids.is_empty() {
                self.run_check(Some(ids)).await;
            }
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
    /// A machine that another check is already probing isn't probed twice:
    /// this waits for that check instead, so every machine asked about has
    /// been looked at when it returns.
    pub(crate) async fn run_check(self: &Arc<Self>, only: Option<Vec<String>>) {
        let full = only.is_none();
        let (jobs, mut in_flight): (Vec<(MachineSpec, Option<probe::Shell>)>, Vec<String>) = {
            let mut st = self.state.lock().unwrap();
            if full && st.check_running {
                return;
            }
            if self.demo.is_none() {
                self.reload_inventory(&mut st, false, None);
            }
            let (in_flight, specs): (Vec<MachineSpec>, Vec<MachineSpec>) = st
                .inventory
                .machines
                .iter()
                .filter(|m| m.is_connected())
                .filter(|m| only.as_ref().is_none_or(|ids| ids.contains(&m.id)))
                // A check of every machine lets one sleep when an app is told
                // of its waking (this one, or the one that's open): only a
                // check of it by name looks. For a while: a notice can be
                // lost with nothing to show for it (the machine woke on
                // another network than the one its connection was made on,
                // or lost power; the app that held the connection quit), so
                // past that the check looks all the same, once in a while.
                //
                // Whose word a promise is taken on: in the app that hears
                // machines wake, its own (it holds the connection). Anywhere
                // else (a command, an agent's MCP server), that app's, for
                // as long as it runs: once it quit, nobody is left to hear.
                .filter(|m| {
                    !full
                        || !st.probes.get(&m.id).is_some_and(|p| {
                            p.asleep
                                && p.wake_notice.is_some()
                                && Utc::now() - p.checked_at < chrono::Duration::from_std(PROMISE_HOLDS).unwrap_or_default()
                                && if st.takes_notices { st.wake_notices.contains(&m.id) } else { super::asleep::app_open(&self.paths.dir) }
                        })
                })
                .cloned()
                .partition(|m| st.checking.contains(&m.id));
            if full {
                st.check_running = true;
                st.check_started_at = Some(Utc::now());
            }
            for s in &specs {
                st.checking.insert(s.id.clone());
            }
            self.publish(&mut st);
            let jobs = specs.into_iter().map(|s| {
                let shell = st.probes.get(&s.id).and_then(|p| p.shell);
                (s, shell)
            }).collect();
            (jobs, in_flight.into_iter().map(|m| m.id).collect())
        };
        // A cloud machine whose create was cut off: found, or given its
        // address, alongside the check, and checked then like the others.
        // With a check of every machine, and with one of a machine that
        // waits for its address.
        let named: Vec<String> = only.iter().flatten().filter(|id| self.current().machines.iter().any(|m| m.awaits_address() && m.id == **id)).cloned().collect();
        let catching_up = (self.demo.is_none() && (full || !named.is_empty())).then(|| {
            let me = self.clone();
            self.rt.spawn(async move { me.cloud_catch_up_and_check(named).await })
        });
        // These are looked at on purpose, asleep or not.
        let _looking: Vec<super::asleep::Looking> = jobs.iter().map(|(s, _)| super::asleep::look(&s.id)).collect();

        let mut pending: FuturesUnordered<_> = jobs
            .into_iter()
            .map(|(spec, shell)| {
                let demo = self.demo.clone();
                async move {
                    let result = match demo {
                        Some(w) => w.probe(&spec).await,
                        None => probe::probe(&spec, shell).await,
                    };
                    (spec.id, spec.cloud.as_ref().map(|c| c.id), result)
                }
            })
            .collect();

        while let Some((id, droplet, result)) = pending.next().await {
            let reachable = result.ok;
            if let Some(droplet) = droplet.filter(|_| self.demo.is_none()) {
                if reachable {
                    self.set_droplet_status(&id, "active");
                } else {
                    let (me, id) = (self.clone(), id.clone());
                    self.rt.spawn(async move { me.refresh_droplet_status(&id, droplet).await });
                }
            }
            let mut st = self.state.lock().unwrap();
            st.checking.remove(&id);
            let result = result.after(st.probes.get(&id));
            // Its monitors are worth knowing when it has a desktop to show.
            let desktop = result.os.as_deref() == Some("darwin") || result.desktop == Some(true);
            crate::fleet::sessions::agent::note_os(&id, result.os.as_deref());
            let asleep = result.asleep;
            self.keep_probe(&mut st, &id, result);
            self.publish(&mut st);
            drop(st);
            // Found asleep over a link that was up (an agent from before it
            // could say so): let go of it, so it can sleep.
            if asleep {
                super::link::reset(&id);
            }
            // Sessions and installed agents come along with every machine check.
            if reachable && self.demo.is_none() {
                let me = self.clone();
                self.rt.spawn(async move {
                    let _ = me.refresh_sessions(&id).await;
                    if desktop {
                        let _ = me.displays(&id).await;
                    }
                });
            }
        }

        // Machines another check was already probing: wait for that check.
        // (Each finished probe is published, which wakes this up to look again.)
        let mut rx = self.subscribe();
        loop {
            {
                let st = self.state.lock().unwrap();
                in_flight.retain(|id| st.checking.contains(id));
            }
            if in_flight.is_empty() || rx.changed().await.is_err() {
                break;
            }
        }

        {
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
                self.save_state(&mut st);
            }
            self.publish(&mut st);
        }
        // What the catch-up found belongs to this check: a command (`muxin
        // check`, `status --refresh`) exits once this returns, and would cut
        // it off. Waited for a bounded time, so a slow DigitalOcean doesn't
        // hold a check up (it goes on by itself then).
        if let Some(task) = catching_up {
            let _ = tokio::time::timeout(CATCH_UP_WAIT, task).await;
        }
    }

    /// Re-reads [settings] if another process changed config.toml. Returns true if the settings changed.
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
        if loaded.wake_notices != st.settings.wake_notices {
            self.hear_wakes(st, loaded.wake_notices);
        }
        st.settings = loaded;
        true
    }

    /// The user turned hearing machines wake on or off (in the app that
    /// stays open: no other hears them).
    fn hear_wakes(&self, st: &mut State, on: bool) {
        if self.demo.is_some() || !super::quiet::taking() {
            return;
        }
        st.takes_notices = on;
        super::quiet::set_on(on);
        if on {
            // The machines linked now can say so from here on; one that's
            // asleep is asked at its next check.
            super::link::wait_to_hear_all(&st.inventory.machines);
        } else {
            // Nobody hears them any more: they're looked at, at each check.
            st.wake_notices.clear();
            for p in st.probes.values_mut().filter(|p| p.asleep) {
                p.wake_notice = None;
            }
            self.save_state(st);
        }
    }

    /// Writes state.json (checks and what Tailscale knows).
    fn save_state(&self, st: &mut State) {
        // Power states of machines no longer in the fleet (destroyed ones) go.
        let ids: HashSet<String> = st.inventory.machines.iter().map(|m| m.id.clone()).collect();
        st.droplets.retain(|id, _| ids.contains(id));
        let persisted = PersistedState {
            probes: st.probes.clone(),
            tailnet: st.tailnet.clone(),
            droplets: st.droplets.clone(),
            check_started_at: st.check_started_at,
            check_finished_at: st.check_finished_at,
        };
        match persisted.save(&self.paths.state) {
            Ok(()) => st.state_mtime = mtime(&self.paths.state),
            Err(e) => tracing::warn!("saving state: {e:#}"),
        }
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
        for (id, device) in disk.tailnet {
            if known.contains(id.as_str()) && st.tailnet.get(&id).is_none_or(|d| device.read_at > d.read_at) {
                st.tailnet.insert(id, device);
                changed = true;
            }
        }
        if disk.check_finished_at > st.check_finished_at {
            st.check_started_at = disk.check_started_at;
            st.check_finished_at = disk.check_finished_at;
            changed = true;
        }
        changed
    }

    /// Re-reads config.toml when it changed (or `force`). Returns true if the inventory changed.
    /// On a parse error the last good inventory stays in place and the error is shown.
    ///
    /// A machine that was within reach and no longer is (disconnected, or
    /// removed) loses its link in this process, whoever changed the file:
    /// this process, another one (the CLI, an agent's MCP server) or an
    /// editor. What rides on the link ends with it: terminals, the shared
    /// clipboard, and the keep-alives that would go on waking a sleeping Mac.
    /// Not `keep`'s: the caller still has a word for that machine's agent.
    fn reload_inventory(&self, st: &mut State, force: bool, keep: Option<&str>) -> bool {
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
                let left: Vec<String> = st
                    .inventory
                    .machines
                    .iter()
                    .filter(|was| was.is_connected() && keep != Some(was.id.as_str()))
                    .filter(|was| !inv.machines.iter().any(|m| m.id == was.id && m.is_connected()))
                    .map(|was| was.id.clone())
                    .collect();
                // The new inventory first, so nothing connects again in between.
                st.inventory = inv;
                for id in left {
                    super::link::reset(&id);
                    // And what it would have said it woke on: this app no
                    // longer has the machine. (Letting go sends nothing.)
                    super::quiet::let_go(&id);
                }
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
        // The machines to leave alone, for everything that would reach for one.
        super::asleep::sync(self as *const Engine as usize, st.probes.iter().filter(|(_, p)| p.asleep && !p.ok).map(|(id, _)| id));
        let interval = st.settings.check_interval_minutes;
        let next_at = match (interval, st.check_finished_at) {
            (0, _) => None,
            (m, Some(t)) => Some(t + chrono::Duration::minutes(m as i64)),
            (_, None) => None,
        };
        // How each machine is reached right now, for suggesting direct connections.
        let links: HashMap<String, (&'static str, Option<&'static str>)> =
            st.inventory.machines.iter().filter_map(|m| super::link::current(&m.id).map(|l| (m.id.clone(), (l.transport(), l.network())))).collect();
        let (paired, ssh_only) = if self.demo.is_some() { Default::default() } else { (super::direct::paired_ids(), super::direct::ssh_only()) };
        // Droplets asked for and not heard of (a file that's there only while one is).
        let cloud_asked = if self.demo.is_none() && crate::cloud::pending::any(&self.paths.dir) { crate::cloud::pending::read(&self.paths.dir) } else { Vec::new() };
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
            tailnet: &st.tailnet,
            tailscale: st.tailscale.as_ref(),
            droplets: &st.droplets,
            cloud_asked: &cloud_asked,
            cloud_trouble: st.cloud_trouble.as_deref(),
            links: &links,
            paired: &paired,
            ssh_only: &ssh_only,
        });
        snapshot.version = st.version;
        snapshot.operations = st.operations.iter().cloned().collect();
        snapshot.workspaces = workspaces::snapshot_infos(&st.workspaces, &snapshot.machines, &st.inventory.machines);
        for w in &mut snapshot.workspaces {
            w.git = st.git.get(&w.machine_id).and_then(|g| g.iter().find(|(path, _)| *path == w.path)).map(|(_, status)| status.clone());
        }
        for m in &mut snapshot.machines {
            if let Some(d) = st.displays.get(&m.id) {
                m.displays = d.clone();
            }
            if let Some(found) = st.sessions.get(&m.id) {
                m.agents = found.agents.clone();
                m.agent_version = found.agent_version.clone();
                m.gh = found.gh.clone();
                m.agent_hooks = found.hooks.clone();
                m.sessions_error = found.error.clone();
                // What we last saw there; nothing is working while it's offline
                // (or disconnected: nothing we can see).
                let offline = m.is_down();
                let git = st.git.get(&m.id);
                snapshot.sessions.extend(found.sessions.iter().cloned().map(|mut s| {
                    s.attention = st.attention.get(&s.id).cloned().or(s.attention);
                    if offline && s.state == "working" {
                        s.state = "idle".into();
                    }
                    s.busy &= !offline;
                    s.git = git.and_then(|g| g.iter().find(|(path, _)| *path == s.cwd)).map(|(_, status)| status.clone());
                    s
                }));
                // A box shows its agent's session: working, waiting, its git.
                snapshot.boxes.extend(found.boxes.iter().cloned().map(|mut b| {
                    let session = found.sessions.iter().find(|s| s.name == b.id && s.status == "running");
                    b.session_id = session.map(|s| s.id.clone());
                    b.busy = session.is_some_and(|s| s.busy) && !offline;
                    b.agent_state = session.map(|s| if offline && s.state == "working" { "idle".to_string() } else { s.state.clone() });
                    b.attention = session.and_then(|s| st.attention.get(&s.id).cloned());
                    b.git = git.and_then(|g| g.iter().find(|(path, _)| *path == b.path)).map(|(_, status)| status.clone());
                    b
                }));
            }
        }
        // Sessions without the agent come after the machine's agent ones.
        for d in &st.direct {
            let at = snapshot.sessions.iter().rposition(|s| s.machine_id == d.info.machine_id).map(|i| i + 1).unwrap_or(snapshot.sessions.len());
            snapshot.sessions.insert(at, d.info.clone());
        }
        snapshot.boxes.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        snapshot.notifications = st.notices.iter().cloned().collect();
        let known: Vec<&String> = st.inventory.machines.iter().map(|m| &m.id).collect();
        snapshot.checkouts = st.checkouts.iter().filter(|(id, _)| known.contains(id)).flat_map(|(_, c)| c.iter().cloned()).collect();
        snapshot.checkouts.sort_by(|a, b| a.repo.cmp(&b.repo).then(a.machine_id.cmp(&b.machine_id)).then(a.path.cmp(&b.path)));
        self.tx.send_replace(Arc::new(snapshot));
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Nothing is asleep on this engine's word any more.
        super::asleep::sync(self as *const Engine as usize, std::iter::empty());
    }
}

/// The rest of a burst of session changes, taken off the queue: one refresh
/// answers them all. What else was in it is given back, in order: a sleep
/// or a wake said in the same breath isn't one to lose.
fn drain_sessions(events: &mut tokio::sync::broadcast::Receiver<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut kept = Vec::new();
    while let Ok(next) = events.try_recv() {
        if next["event"] != "sessions" {
            kept.push(next);
        }
    }
    kept
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Writes a starter config.toml (this computer only) the first time Muxin runs.
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

    /// Two Muxin processes (say, the window and an agent's MCP server) share one config dir.
    #[tokio::test(flavor = "multi_thread")]
    async fn picks_up_changes_from_other_processes() {
        let dir = std::env::temp_dir().join(format!("muxen-xproc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "[[machine]]\nname = \"Box\"\nhost = \"box.invalid\"\n").unwrap();
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

    /// One Muxin process disconnects a machine, or removes one, while another
    /// has a link to it: the other's link closes when it reads config.toml
    /// again, and what rode on it ends (the shared clipboard).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_machine_disconnected_or_removed_elsewhere_loses_its_link_here() {
        use super::super::{asleep, clipboard, link, quiet};
        let dir = std::env::temp_dir().join(format!("muxen-xlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let machine = |name: &str| format!("[[machine]]\nname = \"Xlink {name}\"\nhost = \"xlink-{}.invalid\"\n\n", name.to_lowercase());
        let text = format!("[settings]\nshare_clipboard = true\n\n{}", ["Mac", "Box", "Nas", "Nap", "Doze", "Stay"].map(machine).concat());
        std::fs::write(dir.join("config.toml"), text).unwrap();
        let paths = Paths::resolve(Some(dir.clone()), None);
        let window = Engine::new(paths.clone(), false);
        let cli = Engine::new(paths.clone(), false);
        let spec = |id: &str| window.machine(id).map(|(spec, _)| spec);
        // A change to config.toml shows in its modification time.
        let later = || tokio::time::sleep(Duration::from_millis(20));

        // Closed here, and let go of by everything: the agent's end closes too.
        async fn gone(id: &str, l: Arc<link::Link>, mut agent: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>) {
            assert!(link::current(id).is_none() && !l.alive() && l.retired(), "{id} kept its link");
            drop(l);
            let closed = async { while agent.recv().await.is_some() {} };
            tokio::time::timeout(Duration::from_secs(10), closed).await.unwrap_or_else(|_| panic!("something still holds the link to {id}"));
        }

        // A machine nobody touches keeps its link through all of it.
        let (stay, _stay_agent) = link::piped(&spec("xlink-stay").unwrap()).await;

        // The CLI disconnects the Mac. (A process has one table of links, and
        // the two engines here share it: the window's link is put up after
        // the CLI's own reset, while the window still has the Mac as connected.)
        later().await;
        cli.set_connected("xlink-mac", false).unwrap();
        let mac = spec("xlink-mac").unwrap();
        assert!(mac.is_connected(), "the window hasn't read config.toml yet");
        let (l, agent) = link::piped(&mac).await;
        tokio::spawn(window.clone().follow_link(l.clone()));
        for _ in 0..500 {
            if clipboard::sharing("xlink-mac") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(clipboard::sharing("xlink-mac"));
        // And the connection the Mac would have said it woke on, made by
        // the window like its link.
        let wakes = quiet::tests::fake_agent();
        quiet::open("xlink-mac", &wakes.paired, wakes.at).await.unwrap();
        assert!(quiet::held("xlink-mac").is_some());
        window.refresh_from_disk();
        assert_eq!(window.current().machines.iter().find(|m| m.id == "xlink-mac").unwrap().status, "disconnected");
        gone("xlink-mac", l, agent).await;
        assert!(!clipboard::sharing("xlink-mac"));
        assert!(quiet::held("xlink-mac").is_none(), "the window still waits to hear a machine it no longer has");

        // The CLI removes the box.
        later().await;
        cli.remove_machine("xlink-box").unwrap();
        let (l, agent) = link::piped(&spec("xlink-box").unwrap()).await;
        window.refresh_from_disk();
        assert!(spec("xlink-box").is_none());
        gone("xlink-box", l, agent).await;

        // One removed here goes with its link too (it isn't paired: its
        // agent has nothing to be told).
        let (l, agent) = link::piped(&spec("xlink-nas").unwrap()).await;
        window.remove_machine("xlink-nas").unwrap();
        gone("xlink-nas", l, agent).await;

        // Asleep makes no difference. Muxin has let go of such a machine, but
        // a link to it can be up all the same (it's being looked at, or
        // woken): disconnected or removed elsewhere, that one closes too.
        let status = |id: &str| window.current().machines.iter().find(|m| m.id == id).map(|m| m.status.clone());
        for id in ["xlink-nap", "xlink-doze"] {
            assert!(window.machine_slept(id, &Default::default()) && asleep::is(id));
            assert_eq!(status(id).as_deref(), Some("asleep"));
        }
        later().await;
        cli.set_connected("xlink-nap", false).unwrap();
        cli.remove_machine("xlink-doze").unwrap();
        let (nap, nap_agent) = link::piped(&spec("xlink-nap").unwrap()).await;
        let (doze, doze_agent) = link::piped(&spec("xlink-doze").unwrap()).await;
        window.refresh_from_disk();
        assert_eq!((status("xlink-nap").as_deref(), status("xlink-doze")), (Some("disconnected"), None));
        gone("xlink-nap", nap, nap_agent).await;
        gone("xlink-doze", doze, doze_agent).await;

        assert!(stay.alive() && link::current("xlink-stay").is_some_and(|l| Arc::ptr_eq(&l, &stay)));
        link::reset("xlink-stay");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A machine whose agent says it's going to sleep is asleep, here and in
    /// Muxin's other processes, and nothing reaches for it: not a check of
    /// every machine, not a request. Its agent's word that it woke, or a
    /// check of it by name, looks at it again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_machine_that_sleeps_is_left_alone_until_it_wakes() {
        use super::super::asleep::{self, Told};
        let dir = std::env::temp_dir().join(format!("muxen-asleep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        std::fs::write(
            dir.join("config.toml"),
            format!("[[machine]]\nname = \"Nap {pid}\"\nhost = \"nap-{pid}.invalid\"\n\n[[machine]]\nname = \"Doze {pid}\"\nhost = \"doze-{pid}.invalid\"\n\n[[machine]]\nname = \"Here {pid}\"\nhost = \"local\"\n"),
        )
        .unwrap();
        let paths = Paths::resolve(Some(dir.clone()), None);
        let engine = Engine::new(paths.clone(), false);
        let (nap, doze, here) = (format!("nap-{pid}"), format!("doze-{pid}"), format!("here-{pid}"));
        let status = |e: &Engine, id: &str| e.current().machines.iter().find(|m| m.id == id).map(|m| m.status.clone()).unwrap();
        // They were up a minute ago.
        {
            let mut st = engine.state.lock().unwrap();
            for id in [&nap, &doze, &here] {
                let up = ProbeResult { ok: true, checked_at: Utc::now() - chrono::Duration::minutes(1), os: Some("darwin".into()), arch: Some("arm64".into()), ..Default::default() };
                st.probes.insert(id.to_string(), up.after(None));
            }
            engine.publish(&mut st);
        }
        assert_eq!((status(&engine, &nap), asleep::is(&nap)), ("online".to_string(), false));

        // Nap's agent will say when it wakes; Doze's can't (it's reached over ssh).
        assert!(engine.machine_slept(&nap, &Told { notice: Some("192.168.1.20:61234".into()), lid_closed: true }));
        assert!(engine.machine_slept(&doze, &Told::default()));
        // This computer runs Muxin: it isn't asleep, whatever its agent says
        // (it says so when this computer sleeps), and its link is kept.
        assert!(!engine.machine_slept(&here, &Told::default()));
        assert_eq!((status(&engine, &nap), status(&engine, &doze), status(&engine, &here)), ("asleep".to_string(), "asleep".to_string(), "online".to_string()));
        let s = engine.current();
        let m = s.machines.iter().find(|m| m.id == nap).unwrap();
        let u = m.unreachable.as_ref().unwrap();
        assert_eq!((u.kind.as_str(), u.detail.as_str()), ("asleep", "Its lid is closed. Muxin lets it sleep, and reconnects when it wakes."));
        assert!(u.last_seen_at.is_some(), "when it was last up");
        assert!(asleep::is(&nap) && asleep::is(&doze) && !asleep::is(&here));
        let (spec, _) = engine.machine(&nap).unwrap();
        assert_eq!(spec.refuse_if_disconnected().unwrap_err(), format!("Nap {pid} is asleep. Wake it first."));
        // A copy says so too, to or from it (it used to fall back to plain
        // ssh), and so does making a workspace there (it used to check it).
        let (this, _) = engine.machine(&here).unwrap();
        let still = std::sync::atomic::AtomicBool::new(false);
        for (from, to) in [(&spec, &this), (&this, &spec)] {
            let refused = engine.run_copy(0, from, "~", &["notes.txt".to_string()], to, "~", &still, false).await.unwrap_err();
            assert_eq!(refused, format!("Nap {pid} is asleep. Wake it first."));
        }
        let before = engine.machine(&nap).unwrap().1.unwrap().checked_at;
        let refused = engine.create_workspace(workspaces::CreateWorkspace { machine: Some(nap.clone()), ..Default::default() }).await.unwrap_err();
        assert_eq!(refused, format!("Nap {pid} is asleep. Wake it first."));
        assert_eq!(engine.machine(&nap).unwrap().1.unwrap().checked_at, before, "and it wasn't looked at for it");

        // Muxin's other processes (the CLI, an agent's MCP server) read the same.
        let other = Engine::new(paths.clone(), false);
        assert_eq!((status(&other, &nap), status(&other, &doze)), ("asleep".to_string(), "asleep".to_string()));
        // The Muxin that stays open, which made the promise's connection and
        // hears Nap wake: it holds this for as long as it runs.
        let app = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("app.lock")).unwrap();
        app.lock().unwrap();
        assert!(asleep::app_open(&dir));

        // A check of every machine lets Nap sleep (its app is told when it
        // wakes), and looks at Doze, the only way to find it awake. Here it
        // gets no answer, as from a Mac that wakes too slowly for the look:
        // Doze is still asleep, not offline (and not reached for again
        // every 30 seconds, as an offline machine is).
        let checked = |e: &Engine, id: &str| e.machine(id).unwrap().1.unwrap().checked_at;
        let (before, doze_before) = (checked(&engine, &nap), checked(&engine, &doze));
        other.run_check(None).await;
        assert_eq!((status(&other, &nap), status(&other, &doze)), ("asleep".to_string(), "asleep".to_string()));
        engine.run_check(None).await;
        assert_eq!((status(&engine, &nap), checked(&engine, &nap)), ("asleep".to_string(), before));
        assert_eq!((status(&engine, &doze), asleep::is(&doze)), ("asleep".to_string(), true));
        assert!(checked(&engine, &doze) > doze_before, "it was looked at");

        // A promise is taken at its word for a while only (a notice can be
        // lost: the machine woke somewhere else, or lost power). Past that, the
        // check of every machine looks at Nap too, and it keeps its promise
        // when it doesn't answer.
        {
            let mut st = engine.state.lock().unwrap();
            st.probes.get_mut(&nap).unwrap().checked_at = Utc::now() - chrono::Duration::hours(3);
        }
        engine.run_check(None).await;
        assert!(Utc::now() - checked(&engine, &nap) < chrono::Duration::minutes(1), "looked at after 3 hours");
        let (_, p) = engine.machine(&nap).unwrap();
        assert_eq!((status(&engine, &nap), p.unwrap().wake_notice.as_deref()), ("asleep".to_string(), Some("192.168.1.20:61234")));
        let before = checked(&engine, &nap);
        engine.run_check(None).await;
        assert_eq!(checked(&engine, &nap), before, "and left alone again after");
        // The app quits while Nap sleeps: nobody is left to hear it wake, so
        // the promise it made counts for nothing in the processes that
        // remain. Their check of every machine looks at Nap again.
        drop(app);
        assert!(!asleep::app_open(&dir));
        other.run_check(None).await;
        assert!(checked(&other, &nap) > before, "looked at once the app is gone");
        assert_eq!(status(&other, &nap), "asleep");
        engine.refresh_from_disk();

        // A machine that isn't asleep can't have itself checked by saying it woke.
        engine.run_check(Some(vec![here.clone()])).await;
        let before = checked(&engine, &here);
        engine.machine_woke(&here).await;
        assert_eq!(checked(&engine, &here), before);
        // Nap says it woke: it's looked at. It said it's awake, so not
        // finding it means offline, not still asleep. The other process
        // reads that before it does anything next.
        engine.machine_woke(&nap).await;
        assert_eq!(status(&engine, &nap), "offline");
        assert!(asleep::is(&nap), "as far as the other process knows");
        other.refresh_from_disk();
        assert_eq!((status(&other, &nap), asleep::is(&nap)), ("offline".to_string(), false));
        assert!(spec.refuse_if_disconnected().is_ok());
        drop(other);

        // Asleep again, a check of it by name looks all the same.
        engine.machine_slept(&nap, &Told { notice: Some("192.168.1.20:61234".into()), lid_closed: false });
        assert_eq!(status(&engine, &nap), "asleep");
        let before = checked(&engine, &nap);
        engine.run_check(Some(vec![nap.clone()])).await;
        assert!(checked(&engine, &nap) > before);
        assert_eq!(status(&engine, &nap), "asleep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The connection a sleeping machine would have said it woke on ended
    /// without a word: nobody will say it woke, so it's looked at again, now
    /// (once) and at each check.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_machine_that_can_no_longer_say_it_woke_is_looked_at_again() {
        use super::super::asleep::{self, Told};
        let dir = std::env::temp_dir().join(format!("muxen-notice-lost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        std::fs::write(dir.join("config.toml"), format!("[[machine]]\nname = \"Nod {pid}\"\nhost = \"nod-{pid}.invalid\"\n\n[[machine]]\nname = \"Up {pid}\"\nhost = \"up-{pid}.invalid\"\n")).unwrap();
        let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), false);
        let (nod, up) = (format!("nod-{pid}"), format!("up-{pid}"));
        let seen = |e: &Engine, id: &str| e.current().machines.iter().find(|m| m.id == id).map(|m| (m.status.clone(), m.unreachable.as_ref().map(|u| u.detail.clone()).unwrap_or_default())).unwrap();
        let checked = |e: &Engine, id: &str| e.machine(id).unwrap().1.unwrap().checked_at;
        {
            let mut st = engine.state.lock().unwrap();
            for id in [&nod, &up] {
                let was_up = ProbeResult { ok: true, checked_at: Utc::now() - chrono::Duration::minutes(1), os: Some("darwin".into()), ..Default::default() };
                st.probes.insert(id.to_string(), was_up.after(None));
            }
            engine.publish(&mut st);
        }
        let waiting = Told { notice: Some("192.168.1.20:61234".into()), lid_closed: false };
        engine.machine_slept(&nod, &waiting);
        assert_eq!(seen(&engine, &nod), ("asleep".to_string(), "Muxin lets it sleep, and reconnects when it wakes.".to_string()));

        // A machine that's up loses nothing by it: it isn't looked at.
        let before = checked(&engine, &up);
        engine.notice_lost(&up).await;
        assert_eq!(checked(&engine, &up), before);
        // Asleep: looked at right away. Here it gets no answer, as from a
        // Mac that wakes too slowly for the look: it's still asleep, and the
        // words say that checks look from here on.
        let before = checked(&engine, &nod);
        engine.notice_lost(&nod).await;
        assert!(checked(&engine, &nod) > before, "it was looked at");
        assert_eq!(seen(&engine, &nod), ("asleep".to_string(), "Muxin lets it sleep, and looks again at each check.".to_string()));
        // Lost again a moment later: not looked at twice in a row.
        assert!(engine.machine_slept(&nod, &waiting));
        assert_eq!(seen(&engine, &nod).1, "Muxin lets it sleep, and reconnects when it wakes.");
        let before = checked(&engine, &nod);
        engine.notice_lost(&nod).await;
        assert_eq!(checked(&engine, &nod), before);
        assert_eq!(seen(&engine, &nod), ("asleep".to_string(), "Muxin lets it sleep, and looks again at each check.".to_string()));
        assert!(asleep::is(&nod));
        // Muxin's other processes read the same.
        let other = Engine::new(Paths::resolve(Some(dir.clone()), None), false);
        assert_eq!(seen(&other, &nod).1, "Muxin lets it sleep, and looks again at each check.");
        drop(other);
        // And a check of every machine looks at it again, with no two hours
        // to wait out: nothing is promised any more.
        engine.run_check(None).await;
        assert!(checked(&engine, &nod) > before, "looked at by the check of every machine");
        assert_eq!(seen(&engine, &nod).0, "asleep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The app that hears machines wake takes a promise for what it is: the
    /// connection it holds. One written down by the Muxin that ran before
    /// it, kept by a look that got no answer, is none: checks look again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_promise_with_no_connection_behind_it_is_none() {
        use super::super::asleep::Told;
        let dir = std::env::temp_dir().join(format!("muxen-stale-promise-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        std::fs::write(dir.join("config.toml"), format!("[[machine]]\nname = \"Kip {pid}\"\nhost = \"kip-{pid}.invalid\"\n")).unwrap();
        let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), false);
        let kip = format!("kip-{pid}");
        let seen = |e: &Engine| e.current().machines.iter().find(|m| m.id == kip).map(|m| (m.status.clone(), m.unreachable.as_ref().map(|u| u.detail.clone()).unwrap_or_default())).unwrap();
        let checked = |e: &Engine| e.machine(&kip).unwrap().1.unwrap().checked_at;
        {
            let mut st = engine.state.lock().unwrap();
            let was_up = ProbeResult { ok: true, checked_at: Utc::now() - chrono::Duration::minutes(1), os: Some("darwin".into()), ..Default::default() };
            st.probes.insert(kip.clone(), was_up.after(None));
            engine.publish(&mut st);
        }
        // Asleep, with a promise written down: the Muxin before this one made it.
        assert!(engine.machine_slept(&kip, &Told { notice: Some("192.168.1.20:61234".into()), lid_closed: false }));
        assert_eq!(seen(&engine), ("asleep".to_string(), "Muxin lets it sleep, and reconnects when it wakes.".to_string()));
        // This one hears machines wake, and holds no connection to Kip. Its
        // look as it starts gets no answer (a Mac that wakes too slowly for
        // it): still asleep, and nothing is promised.
        engine.state.lock().unwrap().takes_notices = true;
        engine.run_check(Some(vec![kip.clone()])).await;
        assert_eq!(seen(&engine), ("asleep".to_string(), "Muxin lets it sleep, and looks again at each check.".to_string()));
        // So the check of every machine looks at it, with no two hours to wait.
        let before = checked(&engine);
        engine.run_check(None).await;
        assert!(checked(&engine) > before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The app that stays open hears a machine wake on the connection it
    /// made, through what `start` sets up: the machine is looked at at once.
    /// A connection that ends without a word leaves nothing promised.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_app_that_stays_open_hears_a_machine_wake() {
        use super::super::asleep::Told;
        use super::super::quiet;
        let dir = std::env::temp_dir().join(format!("muxen-hears-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        std::fs::write(dir.join("config.toml"), format!("[[machine]]\nname = \"Wink {pid}\"\nhost = \"wink-{pid}.invalid\"\n")).unwrap();
        let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), false);
        let wink = format!("wink-{pid}");
        let seen = |e: &Engine| e.current().machines.iter().find(|m| m.id == wink).map(|m| (m.status.clone(), m.unreachable.as_ref().map(|u| u.detail.clone()).unwrap_or_default())).unwrap();
        let checked = |e: &Engine| e.machine(&wink).unwrap().1.unwrap().checked_at;
        // Waits until the machine has been looked at since `before` (and
        // that look has come back), with nobody here asking for one.
        let looked_at = |engine: Arc<Engine>, before: chrono::DateTime<Utc>| {
            let checked = &checked;
            async move {
                let deadline = Instant::now() + Duration::from_secs(15);
                while checked(&engine) <= before && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                checked(&engine) > before
            }
        };
        {
            let mut st = engine.state.lock().unwrap();
            let was_up = ProbeResult { ok: true, checked_at: Utc::now() - chrono::Duration::minutes(1), os: Some("darwin".into()), ..Default::default() };
            st.probes.insert(wink.clone(), was_up.after(None));
            engine.publish(&mut st);
        }
        // What `start` does for an app that stays open (without its loops).
        engine.take_wake_notices();
        assert!(engine.state.lock().unwrap().takes_notices && quiet::wanted());

        // Its agent's end of the connection, and the app's call to it while
        // the machine is up.
        let agent = quiet::tests::fake_agent();
        quiet::open(&wink, &agent.paired, agent.at).await.unwrap();
        // It goes to sleep. What's promised is the connection this app
        // holds, whatever the word about it says.
        assert!(engine.machine_slept(&wink, &Told { notice: Some("192.0.2.7:61234".into()), lid_closed: false }));
        assert_eq!(seen(&engine), ("asleep".to_string(), "Muxin lets it sleep, and reconnects when it wakes.".to_string()));
        assert_eq!(engine.machine(&wink).unwrap().1.unwrap().wake_notice, quiet::holds(&wink));
        let before = checked(&engine);
        engine.run_check(None).await;
        assert_eq!(checked(&engine), before, "left alone while it sleeps");

        // Its agent says it woke: looked at, with nobody asking. It said
        // it's awake, so not finding it (here) means offline.
        agent.say.send(Some("awake\n")).unwrap();
        assert!(looked_at(engine.clone(), before).await, "it said it woke, and wasn't looked at");
        assert_eq!(seen(&engine).0, "offline");

        // Asleep again, and this time the connection ends without a word
        // (its agent stopped): nothing is promised any more, it's looked at
        // once, and the words say that checks look from here on.
        quiet::open(&wink, &agent.paired, agent.at).await.unwrap();
        assert!(engine.machine_slept(&wink, &Told::default()));
        assert_eq!(seen(&engine).1, "Muxin lets it sleep, and reconnects when it wakes.");
        let before = checked(&engine);
        agent.say.send(None).unwrap();
        assert!(looked_at(engine.clone(), before).await, "its connection ended, and it wasn't looked at");
        assert_eq!(seen(&engine), ("asleep".to_string(), "Muxin lets it sleep, and looks again at each check.".to_string()));
        assert!(quiet::held(&wink).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A machine that wakes is reached the direct way at once. A try at it
    /// that got no answer earlier (it was going to sleep, or was being
    /// woken) would keep its link on ssh for a minute otherwise, with no
    /// connection for its next wake: only a direct link has one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_machine_that_wakes_is_tried_the_direct_way_at_once() {
        use super::super::asleep::Told;
        use super::super::direct;
        let dir = std::env::temp_dir().join(format!("muxen-rise-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        std::fs::write(dir.join("config.toml"), format!("[[machine]]\nname = \"Rise {pid}\"\nhost = \"rise-{pid}.invalid\"\n")).unwrap();
        let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), false);
        let rise = format!("rise-{pid}");
        {
            let mut st = engine.state.lock().unwrap();
            let was_up = ProbeResult { ok: true, checked_at: Utc::now() - chrono::Duration::minutes(1), os: Some("darwin".into()), ..Default::default() };
            st.probes.insert(rise.clone(), was_up.after(None));
            engine.publish(&mut st);
        }
        // Its direct way didn't answer a moment ago, and it's asleep.
        direct::test_hold_back(&rise);
        assert!(engine.machine_slept(&rise, &Told::default()));
        assert!(direct::held_back(&rise) && !engine.just_woke(&rise));
        // It says it woke: nothing is held against its direct way when it's
        // looked at, and for a moment it counts as having just woken (what
        // `connection` says of a link over ssh then).
        engine.machine_woke(&rise).await;
        assert!(!direct::held_back(&rise), "its direct way is still left alone after it woke");
        assert!(engine.just_woke(&rise));

        // One that says it woke without having been asleep here changes
        // nothing: an agent can't have its machine's direct way retried at will.
        direct::test_hold_back(&rise);
        engine.state.lock().unwrap().woke_at.clear();
        engine.machine_woke(&rise).await;
        assert!(direct::held_back(&rise) && !engine.just_woke(&rise));
        direct::retry_now(&rise);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A machine says it woke while a check is on its way to it. What that
    /// check brings back is from before the wake (asleep, it says): the
    /// machine is looked at once more, not left asleep on that word.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_wake_said_while_a_check_reaches_the_machine_is_looked_at_again() {
        use super::super::asleep::Told;
        let dir = std::env::temp_dir().join(format!("muxen-stir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        std::fs::write(dir.join("config.toml"), format!("[[machine]]\nname = \"Stir {pid}\"\nhost = \"stir-{pid}.invalid\"\n")).unwrap();
        let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), false);
        let stir = format!("stir-{pid}");
        {
            let mut st = engine.state.lock().unwrap();
            let was_up = ProbeResult { ok: true, checked_at: Utc::now() - chrono::Duration::minutes(1), os: Some("darwin".into()), ..Default::default() };
            st.probes.insert(stir.clone(), was_up.after(None));
            engine.publish(&mut st);
        }
        assert!(engine.machine_slept(&stir, &Told::default()));
        // A check is reaching it (as `run_check` marks one), and comes back
        // in a moment with what the machine said before it woke.
        engine.state.lock().unwrap().checking.insert(stir.clone());
        let reaching = {
            let (engine, stir) = (engine.clone(), stir.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let mut st = engine.state.lock().unwrap();
                st.checking.remove(&stir);
                let from_before = ProbeResult::asleep(Instant::now(), &Told::default()).after(st.probes.get(&stir));
                engine.keep_probe(&mut st, &stir, from_before);
                engine.publish(&mut st);
            })
        };
        engine.machine_woke(&stir).await;
        reaching.await.unwrap();
        // The last word on it is a look of its own, after that one (here it
        // gets no answer, which says so itself), not the word from before.
        let (_, p) = engine.machine(&stir).unwrap();
        let p = p.unwrap();
        assert_ne!(p.error.as_deref(), Some(probe::ASLEEP), "left on a word from before it woke");
        assert!(p.error.is_some_and(|e| e.contains("stir-")), "looked at by name");

        // A wake said with no check on its way is one look, as ever.
        assert!(engine.machine_slept(&stir, &Told::default()));
        let before = engine.machine(&stir).unwrap().1.unwrap().checked_at;
        engine.machine_woke(&stir).await;
        assert!(engine.machine(&stir).unwrap().1.unwrap().checked_at > before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session changing as the machine goes to sleep: the word that it's
    /// going to sleep comes in the same burst, and isn't thrown away with it.
    #[tokio::test]
    async fn a_sleep_said_among_session_changes_is_kept() {
        let (tx, mut events) = tokio::sync::broadcast::channel::<serde_json::Value>(16);
        let say = |event: &str| tx.send(serde_json::json!({ "event": event })).unwrap();
        // The first of the burst is the one in hand; these came with it.
        for e in ["sessions", "sleep", "sessions", "awake", "sessions"] {
            say(e);
        }
        let kept = drain_sessions(&mut events);
        assert_eq!(kept.iter().map(|k| k["event"].as_str().unwrap()).collect::<Vec<_>>(), vec!["sleep", "awake"]);
        assert!(drain_sessions(&mut events).is_empty());
    }

    /// Adding a machine starts a check of it; checking it right after must
    /// wait for that one, not return before the machine has been looked at.
    #[tokio::test(flavor = "multi_thread")]
    async fn checking_a_machine_waits_for_a_check_already_running() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-check-wait")), None), true);
        let mut rx = engine.subscribe();
        engine.trigger_check(Some("beacon".into()));
        let started = |s: &Snapshot| s.machines.iter().any(|m| m.id == "beacon" && m.checking);
        while !started(&rx.borrow_and_update()) {
            rx.changed().await.unwrap();
        }
        engine.run_check(Some(vec!["beacon".into()])).await;
        let s = engine.current();
        let beacon = s.machines.iter().find(|m| m.id == "beacon").unwrap();
        assert!(!beacon.checking && beacon.probe.is_some(), "run_check returned while beacon was still being checked");
        assert_eq!(beacon.platform, "linux-x64");
    }

    /// A monitor Muxin resized keeps the size it had as `before`, through
    /// more changes, until it's back at it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_resized_monitor_remembers_its_size() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-display-before")), None), true);
        let at = |w: u32, h: u32| -> Vec<super::super::model::DisplayInfo> { vec![serde_json::from_value(serde_json::json!({ "id": "2", "name": "BenQ", "width": w, "height": h })).unwrap()] };
        let before = |engine: &Engine| engine.state.lock().unwrap().displays["m"][0].before;
        assert!(!engine.remember_display_size("m", "2"), "a monitor never listed");
        engine.set_displays("m", at(2560, 1707));
        assert!(engine.remember_display_size("m", "2"));
        engine.set_displays("m", at(1920, 1080));
        assert_eq!(before(&engine), Some((2560, 1707)));
        // Another change: still the first size.
        assert!(engine.remember_display_size("m", "2"));
        engine.set_displays("m", at(1280, 720));
        assert_eq!(before(&engine), Some((2560, 1707)));
        // Back: forgotten.
        engine.set_displays("m", at(2560, 1707));
        assert_eq!(before(&engine), None);
        engine.set_displays("m", at(1920, 1080));
        assert_eq!(before(&engine), None);
    }
}
