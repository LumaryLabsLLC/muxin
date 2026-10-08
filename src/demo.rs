//! `openrunner serve --demo`: a simulated fleet with jobs arriving, running and
//! finishing, so the dashboard can be explored without any real machines.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::config::{Inventory, MachineSpec, Settings};
use crate::github::{GitHubData, JobData, Rate, RunnerData, ScopeData, ScopeKind, ScopeTest};
use crate::probe::{ProbeResult, ProbedRunner, Shell};

const ORG: &str = "acme";

const INVENTORY: &str = r#"
[[machine]]
name = "Mac Mini"
host = "ci@mac-mini.local"
runners = ["mac-mini-01"]

[[machine]]
name = "Mac Studio"
host = "ci@mac-studio.local"
runners = ["mac-studio-01", "mac-studio-02", "mac-studio-03", "mac-studio-04", "mac-studio-05", "mac-studio-06", "mac-studio-07"]

[[machine]]
name = "Omarchy Laptop"
host = "omarchy"
runners = ["omarchy-01"]

[[machine]]
name = "Tahoe Macbook"
host = "tahoe.local"
runners = ["tahoe-01", "tahoe-02"]

[[machine]]
name = "Ubuntu NUC"
host = "nuc-ubuntu"
runners = ["minis-01-01"]

[[machine]]
name = "Windows Laptop"
host = "win-laptop"
shell = "powershell"
runners = ["windows-01"]

[[machine]]
name = "Windows NUC"
host = "nuc-win"
shell = "powershell"
runners = ["minis-02-01"]

[[machine]]
name = "Laravel Macbook"
host = "laravel-mbp.local"
platform = "darwin-arm64"
notes = "Loaner laptop, usually asleep."

[[machine]]
name = "StudioNAS"
host = "nas"
notes = "Build artifact cache."
"#;

struct Hardware {
    id: &'static str,
    os: &'static str,
    arch: &'static str,
    os_version: &'static str,
    cores: u32,
    mem_gb: u64,
    base_mem: f64,
    disk_total_gb: u64,
    disk_free_gb: f64,
    up_hours: u64,
    reachable: bool,
    /// Docker server version, if the machine has Docker.
    docker: Option<&'static str>,
    /// Linux: "passwordless" | "password" (systemd services need passwordless sudo).
    sudo: Option<&'static str>,
}

const HARDWARE: &[Hardware] = &[
    Hardware { id: "mac-mini", os: "darwin", arch: "arm64", os_version: "macOS 26.1", cores: 10, mem_gb: 32, base_mem: 0.38, disk_total_gb: 1000, disk_free_gb: 381.0, up_hours: 23, reachable: true, docker: None, sudo: None },
    Hardware { id: "mac-studio", os: "darwin", arch: "arm64", os_version: "macOS 26.1", cores: 24, mem_gb: 128, base_mem: 0.18, disk_total_gb: 1000, disk_free_gb: 106.0, up_hours: 116 * 24, reachable: true, docker: Some("29.0.1"), sudo: None },
    Hardware { id: "omarchy-laptop", os: "linux", arch: "x86_64", os_version: "Arch Linux", cores: 16, mem_gb: 32, base_mem: 0.45, disk_total_gb: 1000, disk_free_gb: 328.0, up_hours: 37 * 24 + 15, reachable: true, docker: Some("29.1.3"), sudo: Some("passwordless") },
    Hardware { id: "tahoe-macbook", os: "darwin", arch: "arm64", os_version: "macOS 26.0", cores: 12, mem_gb: 36, base_mem: 0.41, disk_total_gb: 500, disk_free_gb: 122.0, up_hours: 67 * 24 + 20, reachable: true, docker: None, sudo: None },
    Hardware { id: "ubuntu-nuc", os: "linux", arch: "x86_64", os_version: "Ubuntu 24.04.3 LTS", cores: 8, mem_gb: 64, base_mem: 0.10, disk_total_gb: 1000, disk_free_gb: 914.0, up_hours: 43, reachable: true, docker: Some("29.1.3"), sudo: Some("password") },
    Hardware { id: "windows-laptop", os: "windows", arch: "ARM64", os_version: "Microsoft Windows 11 Pro", cores: 12, mem_gb: 32, base_mem: 0.40, disk_total_gb: 512, disk_free_gb: 159.0, up_hours: 4 * 24 + 20, reachable: true, docker: None, sudo: None },
    Hardware { id: "windows-nuc", os: "windows", arch: "AMD64", os_version: "Microsoft Windows 11 Pro", cores: 8, mem_gb: 32, base_mem: 0.33, disk_total_gb: 1000, disk_free_gb: 920.0, up_hours: 37, reachable: true, docker: None, sudo: None },
    Hardware { id: "laravel-macbook", os: "darwin", arch: "arm64", os_version: "macOS 26.1", cores: 12, mem_gb: 36, base_mem: 0.3, disk_total_gb: 1000, disk_free_gb: 400.0, up_hours: 3, reachable: false, docker: None, sudo: None },
    Hardware { id: "studionas", os: "linux", arch: "x86_64", os_version: "Debian GNU/Linux 12 (bookworm)", cores: 4, mem_gb: 16, base_mem: 0.18, disk_total_gb: 8000, disk_free_gb: 6.8, up_hours: 87 * 24 + 7, reachable: true, docker: Some("28.5.1"), sudo: Some("password") },
];

struct Template {
    repo: &'static str,
    workflow: &'static str,
    name: &'static str,
    labels: &'static [&'static str],
    min_secs: u64,
    max_secs: u64,
}

const TEMPLATES: &[Template] = &[
    Template { repo: "acme/desktop", workflow: "CI", name: "build (macos-arm64)", labels: &["self-hosted", "macOS", "ARM64"], min_secs: 90, max_secs: 300 },
    Template { repo: "acme/desktop", workflow: "CI", name: "test (macos)", labels: &["self-hosted", "macOS"], min_secs: 60, max_secs: 180 },
    Template { repo: "acme/desktop", workflow: "Release", name: "notarize", labels: &["self-hosted", "macOS", "ARM64"], min_secs: 120, max_secs: 240 },
    Template { repo: "acme/api", workflow: "CI", name: "test", labels: &["self-hosted", "Linux", "X64"], min_secs: 45, max_secs: 150 },
    Template { repo: "acme/api", workflow: "Deploy", name: "deploy (staging)", labels: &["self-hosted", "Linux"], min_secs: 30, max_secs: 90 },
    Template { repo: "acme/web", workflow: "CI", name: "e2e (windows)", labels: &["self-hosted", "Windows"], min_secs: 90, max_secs: 240 },
    Template { repo: "acme/web", workflow: "CI", name: "lint", labels: &["self-hosted", "Linux"], min_secs: 20, max_secs: 60 },
    Template { repo: "acme/mobile", workflow: "Release", name: "archive (ios)", labels: &["self-hosted", "macOS", "ARM64"], min_secs: 180, max_secs: 420 },
    Template { repo: "acme/infra", workflow: "Nightly", name: "integration", labels: &["self-hosted", "Linux", "X64"], min_secs: 120, max_secs: 360 },
];

const BRANCHES: &[&str] = &["main", "main", "main", "feat/fleet-view", "fix/flaky-e2e", "release/2.4"];
const ACTORS: &[&str] = &["dependabot[bot]", "maya", "theo", "sam", "priya"];

struct SimRunner {
    name: String,
    machine: Option<String>,
    labels: Vec<String>,
    online: bool,
    job: Option<u64>,
    version: &'static str,
    /// Installed as a Docker container (openrunner-<name>).
    docker: bool,
}

struct SimJob {
    id: u64,
    run_id: u64,
    template: usize,
    labels: Vec<String>,
    branch: &'static str,
    actor: &'static str,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    runner: Option<String>,
    duration: i64,
}

struct Sim {
    rng: u64,
    last_tick: DateTime<Utc>,
    next_id: u64,
    runners: Vec<SimRunner>,
    jobs: Vec<SimJob>,
}

pub struct World {
    sim: Mutex<Sim>,
    /// fleet.toml for the demo, edited in memory by add/remove machine and installs.
    inventory: Mutex<String>,
    /// Files in the demo's agent workspaces, by workspace id (see fleet::workspaces).
    pub(crate) workspace_files: Mutex<std::collections::HashMap<String, crate::fleet::workspaces::DemoTree>>,
}

/// Machines added in the demo get these specs.
const GENERIC: Hardware = Hardware {
    id: "",
    os: "linux",
    arch: "x86_64",
    os_version: "Ubuntu 24.04.3 LTS",
    cores: 8,
    mem_gb: 32,
    base_mem: 0.12,
    disk_total_gb: 500,
    disk_free_gb: 410.0,
    up_hours: 2,
    reachable: true,
    docker: Some("29.1.3"),
    sudo: Some("password"),
};

fn hardware(machine_id: &str) -> &'static Hardware {
    HARDWARE.iter().find(|h| h.id == machine_id).unwrap_or(&GENERIC)
}

impl World {
    pub fn new() -> World {
        let now = Utc::now();
        let mac = || labels(&["self-hosted", "macOS", "ARM64"]);
        let mut runners: Vec<SimRunner> = Vec::new();
        let mut add = |name: &str, machine: Option<&str>, l: Vec<String>, version: &'static str| {
            runners.push(SimRunner { name: name.into(), machine: machine.map(String::from), labels: l, online: true, job: None, version, docker: false });
        };
        add("mac-mini-01", Some("mac-mini"), mac(), "2.325.0");
        for i in 1..=7 {
            add(&format!("mac-studio-{i:02}"), Some("mac-studio"), mac(), "2.329.0");
        }
        add("omarchy-01", Some("omarchy-laptop"), labels(&["self-hosted", "Linux", "X64"]), "2.329.0");
        add("tahoe-01", Some("tahoe-macbook"), mac(), "2.329.0");
        add("tahoe-02", Some("tahoe-macbook"), mac(), "2.329.0");
        add("minis-01-01", Some("ubuntu-nuc"), labels(&["self-hosted", "Linux", "X64"]), "2.329.0");
        add("windows-01", Some("windows-laptop"), labels(&["self-hosted", "Windows", "ARM64"]), "2.329.0");
        add("minis-02-01", Some("windows-nuc"), labels(&["self-hosted", "Windows", "X64"]), "2.329.0");
        // Registered on GitHub, but not in fleet.toml.
        add("linux-spot-01", None, labels(&["self-hosted", "Linux", "X64", "spot"]), "2.329.0");

        let mut sim = Sim { rng: now.timestamp_nanos_opt().unwrap_or(1) as u64 | 1, last_tick: now, next_id: 9_000_100, runners, jobs: Vec::new() };
        // A stopped runner, to show what that looks like.
        if let Some(r) = sim.runners.iter_mut().find(|r| r.name == "mac-studio-06") {
            r.online = false;
        }
        // Start mid-afternoon: a few jobs already running, one that no runner can take.
        for (t, runner, ago) in [(0, "mac-studio-01", 70), (7, "mac-studio-05", 140), (1, "mac-mini-01", 35), (4, "minis-01-01", 20), (5, "minis-02-01", 50)] {
            let id = sim.new_job(t, now - chrono::Duration::seconds(ago + 5));
            let job = sim.jobs.iter_mut().find(|j| j.id == id).unwrap();
            job.started_at = Some(now - chrono::Duration::seconds(ago));
            job.runner = Some(runner.into());
            sim.runners.iter_mut().find(|r| r.name == runner).unwrap().job = Some(id);
        }
        let gpu = sim.new_job(8, now - chrono::Duration::minutes(6));
        if let Some(j) = sim.jobs.iter_mut().find(|j| j.id == gpu) {
            j.labels = labels(&["self-hosted", "Linux", "gpu"]);
        }
        World {
            sim: Mutex::new(sim),
            inventory: Mutex::new(INVENTORY.trim_start().to_string()),
            workspace_files: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn inventory(&self) -> Inventory {
        let text = self.inventory.lock().unwrap();
        Inventory::parse(&text).unwrap_or_else(|_| Inventory::parse(INVENTORY).expect("demo inventory parses"))
    }

    pub fn settings(&self) -> Settings {
        let mut s = Settings::default();
        s.github.orgs = vec![ORG.into()];
        s.check_interval_minutes = 5;
        s
    }

    pub async fn probe(&self, spec: &MachineSpec) -> ProbeResult {
        let delay = { self.sim.lock().unwrap().range(400, 1800) };
        tokio::time::sleep(Duration::from_millis(delay)).await;

        let hw = hardware(&spec.id);
        if !hw.reachable {
            return ProbeResult {
                checked_at: Utc::now(),
                duration_ms: 8000,
                ok: false,
                error: Some(format!("ssh: connect to host {} port 22: Operation timed out", spec.host)),
                ..Default::default()
            };
        }

        let mut sim = self.sim.lock().unwrap();
        sim.tick(Utc::now());
        let now = Utc::now();
        let mine: Vec<(String, bool, bool, &'static str, bool)> = sim
            .runners
            .iter()
            .filter(|r| r.machine.as_deref() == Some(spec.id.as_str()))
            .map(|r| (r.name.clone(), r.online, r.job.is_some(), r.version, r.docker))
            .collect();
        let busy = mine.iter().filter(|r| r.2).count() as f64;
        let cpu = (3.0 + busy * 38.0 / (hw.cores as f64 / 8.0).max(1.0) + sim.noise(6.0)).clamp(1.0, 99.0);
        let mem_total = hw.mem_gb * 1024 * 1024 * 1024;
        let mem = (hw.base_mem + busy * 0.07 + sim.noise(0.02)).clamp(0.05, 0.97);
        let mut pid = 3000 + sim.range(0, 500) as u32;
        let runners = mine
            .iter()
            .map(|(name, online, working, version, docker)| {
                pid += 37;
                if *docker {
                    let container = format!("openrunner-{name}");
                    return ProbedRunner {
                        dir: crate::probe::container_dir(&container),
                        name: name.clone(),
                        url: Some(format!("https://github.com/{ORG}")),
                        version: Some(version.to_string()),
                        listener_pid: online.then_some(pid),
                        worker_pid: working.then_some(pid + 11),
                        image: Some(format!("ghcr.io/actions/actions-runner:{version}")),
                        container_state: Some(if *online { "running" } else { "exited" }.into()),
                        restarts: Some(0),
                        container: Some(container),
                        ..Default::default()
                    };
                }
                let dir = match hw.os {
                    "windows" => format!("C:\\actions-runner\\{name}"),
                    "linux" => format!("/home/ci/actions-runner/{name}"),
                    _ => format!("/Users/ci/actions-runner/{name}"),
                };
                ProbedRunner {
                    dir,
                    name: name.clone(),
                    url: Some(format!("https://github.com/{ORG}")),
                    version: Some(version.to_string()),
                    service: Some(format!("actions.runner.{ORG}.{name}")),
                    service_state: Some(if *online { "active" } else { "inactive" }.into()),
                    listener_pid: online.then_some(pid),
                    worker_pid: working.then_some(pid + 11),
                    ..Default::default()
                }
            })
            .collect();
        ProbeResult {
            checked_at: now,
            duration_ms: delay,
            ok: true,
            error: None,
            shell: Some(if hw.os == "windows" { Shell::Powershell } else { Shell::Sh }),
            hostname: Some(spec.id.clone()),
            os: Some(hw.os.into()),
            arch: Some(hw.arch.into()),
            os_version: Some(hw.os_version.into()),
            cores: Some(hw.cores),
            cpu_pct: Some((cpu * 10.0).round() / 10.0),
            mem_total: Some(mem_total),
            mem_used: Some((mem_total as f64 * mem) as u64),
            disk_total: Some(hw.disk_total_gb * 1_000_000_000),
            disk_free: Some((hw.disk_free_gb * 1e9) as u64),
            disk_path: Some(match hw.os {
                "windows" => "C:".into(),
                "darwin" => "/System/Volumes/Data".into(),
                _ => "/".into(),
            }),
            uptime_sec: Some(hw.up_hours * 3600 + (now.timestamp() % 3600) as u64),
            load: if hw.os == "windows" { Vec::new() } else { vec![busy * 1.6 + 0.4, busy * 1.4 + 0.5, busy * 1.2 + 0.6] },
            runners,
            docker: hw.docker.map(String::from),
            docker_error: None,
            sudo: hw.sudo.map(String::from),
        }
    }

    pub fn github(&self, settings: &Settings) -> GitHubData {
        let now = Utc::now();
        let mut sim = self.sim.lock().unwrap();
        sim.tick(now);
        let watching_acme = settings.github.orgs.iter().any(|o| o.eq_ignore_ascii_case(ORG));
        let scopes = settings
            .github
            .orgs
            .iter()
            .map(|o| (ScopeKind::Org, o.clone()))
            .chain(settings.github.repos.iter().map(|r| (ScopeKind::Repo, r.clone())))
            .map(|(kind, name)| ScopeData {
                runners: if kind == ScopeKind::Org && name.eq_ignore_ascii_case(ORG) {
                    sim.runners
                        .iter()
                        .enumerate()
                        .map(|(i, r)| RunnerData {
                            id: 100 + i as u64,
                            name: r.name.clone(),
                            os: os_for(&r.labels).into(),
                            status: if r.online { "online" } else { "offline" }.into(),
                            busy: r.job.is_some(),
                            labels: r.labels.clone(),
                        })
                        .collect()
                } else {
                    Vec::new()
                },
                kind,
                name,
                synced_at: Some(now),
                error: None,
                hint: None,
            })
            .collect();
        let jobs = if watching_acme {
            sim.jobs
                .iter()
                .map(|j| {
                    let t = &TEMPLATES[j.template];
                    JobData {
                        id: j.id,
                        run_id: j.run_id,
                        name: t.name.into(),
                        workflow: t.workflow.into(),
                        repo: t.repo.into(),
                        branch: Some(j.branch.into()),
                        event: Some(if j.branch == "main" { "push" } else { "pull_request" }.into()),
                        actor: Some(j.actor.into()),
                        url: format!("https://github.com/{}/actions/runs/{}/job/{}", t.repo, j.run_id, j.id),
                        status: if j.started_at.is_some() { "in_progress" } else { "queued" }.into(),
                        labels: j.labels.clone(),
                        runner_name: j.runner.clone(),
                        created_at: j.created_at,
                        started_at: j.started_at,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        let remaining = 4800 - (now.timestamp() % 600) as u32;
        GitHubData {
            synced_at: Some(now),
            token_source: "gh".into(),
            login: Some("you".into()),
            error: None,
            scopes,
            jobs,
            scanned_repos: if watching_acme {
                ["acme/desktop", "acme/api", "acme/web", "acme/mobile", "acme/infra"].map(String::from).to_vec()
            } else {
                Vec::new()
            },
            rate: Some(Rate { limit: 5000, remaining, reset_at: now + chrono::Duration::minutes(37) }),
            latest_runner_version: Some("2.329.0".into()),
        }
    }

    // -----------------------------------------------------------------------
    // Control operations (see crate::ops), simulated.

    /// Applies an edit to the demo's fleet.toml text.
    pub fn edit_inventory(&self, edit: impl FnOnce(&str) -> Result<String, String>) -> Result<(), String> {
        let mut text = self.inventory.lock().unwrap();
        *text = edit(&text)?;
        Ok(())
    }

    /// Err with an ssh-style message when the demo machine is "asleep".
    pub fn reachable(&self, machine_id: &str, host: &str) -> Result<(), String> {
        if hardware(machine_id).reachable {
            Ok(())
        } else {
            Err(format!("ssh: connect to host {host} port 22: Operation timed out"))
        }
    }

    /// ("osx" | "linux" | "win", "arm64" | "x64") as the runner download names them.
    pub fn runner_platform(&self, machine_id: &str) -> (&'static str, &'static str) {
        let hw = hardware(machine_id);
        let os = match hw.os {
            "darwin" => "osx",
            "windows" => "win",
            _ => "linux",
        };
        let arch = if hw.arch.eq_ignore_ascii_case("arm64") { "arm64" } else { "x64" };
        (os, arch)
    }

    /// Adds runners to the simulation; `docker` makes them container runners.
    pub fn install_runners(&self, machine_id: &str, names: &[String], extra_labels: &[String], docker: bool) {
        let hw = hardware(machine_id);
        let os = match hw.os {
            "darwin" => "macOS",
            "windows" => "Windows",
            _ => "Linux",
        };
        let arch = if hw.arch.eq_ignore_ascii_case("arm64") { "ARM64" } else { "X64" };
        let mut sim = self.sim.lock().unwrap();
        for name in names {
            // A container runner's OS is Linux whatever the host is.
            let mut l = if docker { labels(&["self-hosted", "Linux", arch]) } else { labels(&["self-hosted", os, arch]) };
            l.extend(extra_labels.iter().cloned());
            sim.runners.push(SimRunner {
                name: name.clone(),
                machine: Some(machine_id.to_string()),
                labels: l,
                online: true,
                job: None,
                version: "2.329.0",
                docker,
            });
        }
    }

    /// Start (online) or stop (offline) a runner. Stopping hands its job back to the queue.
    pub fn set_runner_online(&self, name: &str, online: bool) {
        let mut sim = self.sim.lock().unwrap();
        if let Some(i) = sim.runners.iter().position(|r| r.name.eq_ignore_ascii_case(name)) {
            if !online {
                sim.requeue(i);
            }
            sim.runners[i].online = online;
        }
    }

    pub fn remove_runner(&self, name: &str) {
        let mut sim = self.sim.lock().unwrap();
        if let Some(i) = sim.runners.iter().position(|r| r.name.eq_ignore_ascii_case(name)) {
            sim.requeue(i);
            sim.runners.remove(i);
        }
    }

    /// A plausible tail of a runner's _diag/Runner_*.log.
    pub fn diag_log(&self, name: &str) -> Vec<String> {
        let now = Utc::now();
        let sim = self.sim.lock().unwrap();
        let runner = sim.runners.iter().find(|r| r.name.eq_ignore_ascii_case(name));
        let online = runner.is_some_and(|r| r.online);
        let job = runner.and_then(|r| r.job).and_then(|id| sim.jobs.iter().find(|j| j.id == id));
        let hw = runner.and_then(|r| r.machine.as_deref()).map(hardware).unwrap_or(&GENERIC);
        let arm = hw.arch.eq_ignore_ascii_case("arm64");
        let (bin, built, runtime) = match hw.os {
            "windows" => (
                format!("C:\\actions-runner\\{name}\\bin"),
                format!("Windows ({}) - win-{}", if arm { "ARM64" } else { "X64" }, if arm { "arm64" } else { "x64" }),
                "Microsoft Windows 10.0.26100".to_string(),
            ),
            "linux" => (
                format!("/home/ci/actions-runner/{name}/bin"),
                format!("Linux ({}) - linux-{}", if arm { "ARM64" } else { "X64" }, if arm { "arm64" } else { "x64" }),
                "Linux 6.8.0-45-generic #45-Ubuntu SMP".to_string(),
            ),
            _ => (
                format!("/Users/ci/actions-runner/{name}/bin"),
                "macOS (ARM64) - osx-arm64".to_string(),
                "Darwin 25.1.0 Darwin Kernel Version 25.1.0".to_string(),
            ),
        };
        let at = |secs_ago: i64| (now - chrono::Duration::seconds(secs_ago)).format("%Y-%m-%d %H:%M:%SZ").to_string();
        let mut lines = vec![
            format!("[{} INFO HostContext] Well known directory 'Bin': '{bin}'", at(5400)),
            format!("[{} INFO Listener] Runner is built for {built}.", at(5400)),
            format!("[{} INFO Listener] RuntimeInformation: {runtime}.", at(5400)),
            format!("[{} INFO Listener] Version: 2.329.0", at(5400)),
            format!("[{} INFO Listener] Commit: 6b5f7cf8a5e0c4c3b1c0d6e2", at(5400)),
            format!("[{} INFO ConfigurationStore] HasCredentials()", at(5399)),
            format!("[{} INFO CredentialManager] GetCredentialProvider", at(5399)),
            format!("[{} INFO RunnerServer] EstablishVssConnection", at(5398)),
            format!("[{} INFO MessageListener] Connect to server.", at(5398)),
            format!("[{} INFO MessageListener] Create session.", at(5397)),
            format!("[{} INFO MessageListener] Session created.", at(5396)),
            format!("[{} INFO Terminal] WRITE LINE: √ Connected to GitHub", at(5396)),
            format!("[{} INFO Terminal] WRITE LINE: Current runner version: '2.329.0'", at(5395)),
            format!("[{} INFO Terminal] WRITE LINE: Listening for Jobs", at(5395)),
        ];
        let past_job = match hw.os {
            "windows" => "e2e (windows)",
            "linux" => "test",
            _ => "build (macos-arm64)",
        };
        for k in 0..4 {
            let t = 4200 - k * 900;
            lines.push(format!("[{} INFO MessageListener] Message '{}' received from session.", at(t), 1000 + k));
            lines.push(format!("[{} INFO JobDispatcher] Job request {} for plan 6f3c… received.", at(t), 18_000_200 + k));
            lines.push(format!("[{} INFO JobDispatcher] Start renew job request {}.", at(t - 1), 18_000_200 + k));
            lines.push(format!("[{} INFO Terminal] WRITE LINE: Running job: {past_job}", at(t - 1)));
            lines.push(format!("[{} INFO JobDispatcher] Worker finished for job. Code: 100", at(t - 240)));
            lines.push(format!("[{} INFO Terminal] WRITE LINE: Job {past_job} completed with result: Succeeded", at(t - 240)));
        }
        match (online, job) {
            (false, _) => lines.push(format!("[{} INFO Listener] Runner execution has been cancelled. Exiting.", at(30))),
            (true, Some(j)) => {
                let t = &TEMPLATES[j.template];
                lines.push(format!("[{} INFO JobDispatcher] Job request {} for plan 9a1d… received.", at(20), j.id));
                lines.push(format!("[{} INFO Terminal] WRITE LINE: Running job: {}", at(19), t.name));
            }
            (true, None) => lines.push(format!("[{} INFO MessageListener] No message retrieved from session, keep polling.", at(15))),
        }
        lines
    }

    pub fn test_scope(&self, scope: &str) -> ScopeTest {
        let name = crate::config::strip_github_url(scope);
        let kind = if name.contains('/') { ScopeKind::Repo } else { ScopeKind::Org };
        let runners = if name.eq_ignore_ascii_case(ORG) { self.sim.lock().unwrap().runners.len() } else { 0 };
        ScopeTest { kind, name, runners, error: None, hint: None }
    }
}

impl Sim {
    /// Puts the runner's job (if any) back in the queue.
    fn requeue(&mut self, runner: usize) {
        if let Some(id) = self.runners[runner].job.take() {
            if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                j.started_at = None;
                j.runner = None;
            }
        }
    }

    fn next(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo).max(1)
    }

    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 10_000) as f64 / 10_000.0 < p
    }

    fn noise(&mut self, amp: f64) -> f64 {
        ((self.next() % 2001) as f64 / 1000.0 - 1.0) * amp
    }

    fn new_job(&mut self, template: usize, created_at: DateTime<Utc>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let t = &TEMPLATES[template];
        let duration = self.range(t.min_secs, t.max_secs) as i64;
        let branch = BRANCHES[self.range(0, BRANCHES.len() as u64) as usize];
        let actor = ACTORS[self.range(0, ACTORS.len() as u64) as usize];
        self.jobs.push(SimJob {
            id,
            run_id: 18_000_000 + id,
            template,
            labels: labels(t.labels),
            branch,
            actor,
            created_at,
            started_at: None,
            runner: None,
            duration,
        });
        id
    }

    /// Advances the simulation: finish jobs, enqueue new ones, hand queued jobs to idle runners.
    fn tick(&mut self, now: DateTime<Utc>) {
        let dt = (now - self.last_tick).num_milliseconds().max(0) as f64 / 1000.0;
        self.last_tick = now;

        let finished: Vec<u64> = self
            .jobs
            .iter()
            .filter(|j| j.started_at.is_some_and(|s| (now - s).num_seconds() >= j.duration))
            .map(|j| j.id)
            .collect();
        self.jobs.retain(|j| !finished.contains(&j.id));
        for r in &mut self.runners {
            if r.job.is_some_and(|id| finished.contains(&id)) {
                r.job = None;
            }
        }

        // Roughly one new job every 15 seconds, sometimes a burst from a push.
        if self.chance((dt / 15.0).min(0.9)) {
            let n = if self.chance(0.25) { 3 } else { 1 };
            for _ in 0..n {
                let t = self.range(0, TEMPLATES.len() as u64) as usize;
                self.new_job(t, now);
            }
        }

        let mut queued: Vec<usize> = (0..self.jobs.len()).filter(|&i| self.jobs[i].started_at.is_none()).collect();
        queued.sort_by_key(|&i| self.jobs[i].created_at);
        for i in queued {
            // Runners take a few seconds to pick a job up.
            if (now - self.jobs[i].created_at).num_seconds() < 4 {
                continue;
            }
            let job_labels = self.jobs[i].labels.clone();
            let candidates: Vec<usize> = (0..self.runners.len())
                .filter(|&r| {
                    let rr = &self.runners[r];
                    rr.online && rr.job.is_none() && job_labels.iter().all(|l| rr.labels.iter().any(|x| x.eq_ignore_ascii_case(l)))
                })
                .collect();
            if candidates.is_empty() {
                continue;
            }
            let pick = candidates[self.range(0, candidates.len() as u64) as usize];
            let job_id = self.jobs[i].id;
            self.runners[pick].job = Some(job_id);
            self.jobs[i].started_at = Some(now);
            self.jobs[i].runner = Some(self.runners[pick].name.clone());
        }
    }
}

fn labels(l: &[&str]) -> Vec<String> {
    l.iter().map(|s| s.to_string()).collect()
}

fn os_for(labels: &[String]) -> &'static str {
    if labels.iter().any(|l| l == "macOS") {
        "macOS"
    } else if labels.iter().any(|l| l == "Windows") {
        "Windows"
    } else {
        "Linux"
    }
}
