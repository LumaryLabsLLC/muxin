//! `muxen --demo`: simulated machines with jobs arriving, running and
//! finishing, so the app can be explored without any real machines.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::config::{Inventory, MachineSpec, Settings};
use crate::github::{GitHubData, JobData, Rate, RunnerData, ScopeData, ScopeKind, ScopeTest};
use crate::probe::{ProbeResult, ProbedRunner, Shell};

const ORG: &str = "acme";

/// The demo's machines: a small studio's Macs, a Linux server and laptop, a
/// NAS, a MacBook that's usually asleep, and a cloud machine created days ago.
fn inventory_text(now: DateTime<Utc>) -> String {
    let created = (now - chrono::Duration::days(3) - chrono::Duration::hours(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let vm_created = (now - chrono::Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!(
        r#"[[machine]]
name = "Forge"
host = "ci@forge.local"
runners = ["forge-01", "forge-02", "forge-03"]
notes = "The Mac Studio under the desk."

[[machine]]
name = "Orbit"
host = "ci@orbit.local"
runners = ["orbit-01", "orbit-02"]

[[machine]]
name = "Beacon"
host = "beacon"
runners = ["beacon-01"]

[[machine]]
name = "Pebble"
host = "pebble"
runners = ["pebble-01"]

[[machine]]
name = "dev-1"
host = "root@203.0.113.24"
cloud = {{ provider = "digitalocean", id = 512034871, region = "sfo3", size = "s-2vcpu-4gb", price_hourly = 0.03571, created_at = "{created}" }}

[[machine]]
name = "Harbor"
host = "harbor.local"
platform = "darwin-arm64"
notes = "The travel MacBook, usually asleep."

[[machine]]
name = "Vault"
host = "vault"
notes = "Build cache and backups."

[[machine]]
name = "win-test"
host = "127.0.0.1"
user = "ci"
port = 2201
ssh_options = ["-J", "beacon", "-o", "HostKeyAlias=muxen-win-test"]
direct_address = "beacon:47830"
vm = {{ host = "beacon", os = "windows", version = "windows-11", container = "muxen-win-test", web_port = 8007, created_at = "{vm_created}" }}

[[machine]]
name = "sandbox"
host = "127.0.0.1"
user = "ci"
port = 2202
ssh_options = ["-J", "ci@forge.local", "-o", "HostKeyAlias=muxen-sandbox"]
direct_address = "forge.local:47831"
vm = {{ host = "forge", os = "linux", version = "ubuntu-24.04", container = "muxen-sandbox", created_at = "{vm_created}" }}
"#
    )
}

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
    /// Has a desktop to show (servers and the NAS don't).
    desktop: bool,
    /// Docker server version, if the machine has Docker.
    docker: Option<&'static str>,
    /// Linux: "passwordless" | "password" (systemd services need passwordless sudo).
    sudo: Option<&'static str>,
}

const HARDWARE: &[Hardware] = &[
    Hardware { id: "forge", os: "darwin", arch: "arm64", os_version: "macOS 26.1", cores: 16, mem_gb: 64, base_mem: 0.27, disk_total_gb: 2000, disk_free_gb: 1240.0, up_hours: 9 * 24 + 4, reachable: true, desktop: true, docker: Some("29.0.1"), sudo: None },
    Hardware { id: "orbit", os: "darwin", arch: "arm64", os_version: "macOS 26.1", cores: 12, mem_gb: 24, base_mem: 0.44, disk_total_gb: 512, disk_free_gb: 203.0, up_hours: 2 * 24 + 18, reachable: true, desktop: true, docker: None, sudo: None },
    Hardware { id: "beacon", os: "linux", arch: "x86_64", os_version: "Ubuntu 24.04.3 LTS", cores: 12, mem_gb: 96, base_mem: 0.14, disk_total_gb: 2000, disk_free_gb: 1610.0, up_hours: 31 * 24 + 2, reachable: true, desktop: false, docker: Some("29.1.3"), sudo: Some("password") },
    Hardware { id: "pebble", os: "linux", arch: "x86_64", os_version: "Fedora Linux 42 (Workstation Edition)", cores: 8, mem_gb: 32, base_mem: 0.36, disk_total_gb: 1000, disk_free_gb: 512.0, up_hours: 6, reachable: true, desktop: true, docker: Some("28.5.2"), sudo: Some("passwordless") },
    Hardware { id: "dev-1", os: "linux", arch: "x86_64", os_version: "Ubuntu 24.04.3 LTS", cores: 2, mem_gb: 4, base_mem: 0.31, disk_total_gb: 80, disk_free_gb: 61.0, up_hours: 3 * 24 + 5, reachable: true, desktop: false, docker: Some("29.1.3"), sudo: Some("passwordless") },
    Hardware { id: "harbor", os: "darwin", arch: "arm64", os_version: "macOS 26.1", cores: 12, mem_gb: 36, base_mem: 0.3, disk_total_gb: 1000, disk_free_gb: 470.0, up_hours: 3, reachable: false, desktop: true, docker: None, sudo: None },
    Hardware { id: "win-test", os: "windows", arch: "AMD64", os_version: "Microsoft Windows 11 Pro", cores: 4, mem_gb: 8, base_mem: 0.38, disk_total_gb: 64, disk_free_gb: 38.0, up_hours: 22, reachable: true, desktop: true, docker: None, sudo: None },
    Hardware { id: "sandbox", os: "linux", arch: "aarch64", os_version: "Ubuntu 24.04.3 LTS", cores: 4, mem_gb: 8, base_mem: 0.12, disk_total_gb: 2000, disk_free_gb: 1240.0, up_hours: 20, reachable: true, desktop: false, docker: None, sudo: Some("passwordless") },
    Hardware { id: "vault", os: "linux", arch: "aarch64", os_version: "Debian GNU/Linux 12 (bookworm)", cores: 4, mem_gb: 8, base_mem: 0.22, disk_total_gb: 4000, disk_free_gb: 41.0, up_hours: 54 * 24 + 11, reachable: true, desktop: false, docker: Some("28.5.1"), sudo: Some("password") },
    // Not in the demo's machines; tests add it to try Windows.
    Hardware { id: "test-windows", os: "windows", arch: "AMD64", os_version: "Microsoft Windows 11 Pro", cores: 8, mem_gb: 32, base_mem: 0.33, disk_total_gb: 1000, disk_free_gb: 700.0, up_hours: 30, reachable: true, desktop: false, docker: None, sudo: None },
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
    Template { repo: "acme/web", workflow: "CI", name: "e2e (chromium)", labels: &["self-hosted", "Linux", "X64"], min_secs: 90, max_secs: 240 },
    Template { repo: "acme/web", workflow: "CI", name: "lint", labels: &["self-hosted", "Linux"], min_secs: 20, max_secs: 60 },
    Template { repo: "acme/mobile", workflow: "Release", name: "archive (ios)", labels: &["self-hosted", "macOS", "ARM64"], min_secs: 180, max_secs: 420 },
    Template { repo: "acme/models", workflow: "Nightly", name: "train (small)", labels: &["self-hosted", "Linux", "X64"], min_secs: 120, max_secs: 360 },
];

const BRANCHES: &[&str] = &["main", "main", "main", "feat/machine-cards", "fix/flaky-e2e", "release/2.4"];
const ACTORS: &[&str] = &["dependabot[bot]", "maya", "theo", "sam", "priya"];

struct SimRunner {
    name: String,
    machine: Option<String>,
    labels: Vec<String>,
    online: bool,
    job: Option<u64>,
    version: &'static str,
    /// Installed as a Docker container (muxen-<name>).
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
    /// config.toml for the demo, edited in memory by add/remove machine and installs.
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
    desktop: true,
    docker: Some("29.1.3"),
    sudo: Some("password"),
};

/// The containers the demo's VMs run in, on their hosts.
fn demo_containers(machine_id: &str) -> Vec<crate::probe::PublishedContainer> {
    use crate::probe::{PublishedContainer, PublishedPort};
    let port = |ip: &str, host_port: u16, target: &str| PublishedPort { host_ip: ip.into(), host_port, target: target.into() };
    match machine_id {
        "beacon" => vec![PublishedContainer {
            name: "muxen-win-test".into(),
            state: "running".into(),
            image: "dockurr/windows".into(),
            ports: vec![port("127.0.0.1", 2201, "22/tcp"), port("127.0.0.1", 8007, "8006/tcp"), port("", 47830, "47821/udp")],
        }],
        "forge" => vec![PublishedContainer {
            name: "muxen-sandbox".into(),
            state: "running".into(),
            image: "ubuntu:24.04".into(),
            ports: vec![port("127.0.0.1", 2202, "22/tcp"), port("", 47831, "47821/udp")],
        }],
        _ => Vec::new(),
    }
}

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
        for i in 1..=3 {
            add(&format!("forge-{i:02}"), Some("forge"), mac(), "2.329.0");
        }
        add("orbit-01", Some("orbit"), mac(), "2.329.0");
        add("orbit-02", Some("orbit"), mac(), "2.325.0");
        add("beacon-01", Some("beacon"), labels(&["self-hosted", "Linux", "X64"]), "2.329.0");
        add("pebble-01", Some("pebble"), labels(&["self-hosted", "Linux", "X64"]), "2.329.0");
        // Registered on GitHub, but not in config.toml.
        add("spot-x64-7", None, labels(&["self-hosted", "Linux", "X64", "spot"]), "2.329.0");

        let mut sim = Sim { rng: now.timestamp_nanos_opt().unwrap_or(1) as u64 | 1, last_tick: now, next_id: 9_000_100, runners, jobs: Vec::new() };
        // A stopped runner, to show what that looks like.
        if let Some(r) = sim.runners.iter_mut().find(|r| r.name == "forge-03") {
            r.online = false;
        }
        // Start mid-afternoon: a few jobs already running, one that no runner can take.
        for (t, runner, ago) in [(0, "forge-01", 70), (7, "forge-02", 140), (1, "orbit-01", 35), (3, "beacon-01", 20)] {
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
            inventory: Mutex::new(inventory_text(now)),
            workspace_files: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn inventory(&self) -> Inventory {
        let text = self.inventory.lock().unwrap();
        Inventory::parse(&text).unwrap_or_else(|_| Inventory::parse(&inventory_text(Utc::now())).expect("demo inventory parses"))
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
                os: Some(hw.os.into()),
                arch: Some(hw.arch.into()),
                os_version: Some(hw.os_version.into()),
                last_seen_at: Some(Utc::now() - chrono::Duration::minutes(47)),
                // From when it was on: how to wake it.
                mac: Some("3c:22:fb:9a:41:07".into()),
                lan_ip: Some("192.168.1.42".into()),
                lan_broadcast: Some("192.168.1.255".into()),
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
                    let container = format!("muxen-{name}");
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
            form_factor: Some(if matches!(hw.id, "pebble" | "harbor") { "laptop" } else { "desktop" }.into()),
            desktop: Some(hw.desktop),
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
            last_seen_at: Some(now),
            // A network card each (Harbor, asleep, can be woken), and a reboot waiting on Pebble.
            mac: Some(format!("3c:22:fb:{:02x}:{:02x}:{:02x}", hw.id.len() * 7, hw.cores, hw.disk_total_gb % 256)),
            lan_ip: Some(format!("192.168.1.{}", 10 + hw.id.len())),
            lan_broadcast: Some("192.168.1.255".into()),
            reboot_required: hw.id == "pebble",
            failed_units: Vec::new(),
            // The demo's VMs: Windows in Beacon, a Linux container on Forge.
            virt: match hw.id {
                "win-test" => Some("qemu".into()),
                "sandbox" => Some("docker".into()),
                _ => None,
            },
            published: demo_containers(hw.id),
            route: None,
            kvm: hw.id == "beacon",
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

    /// Applies an edit to the demo's config.toml text.
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
            "windows" => "test (windows)",
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

// ---------------------------------------------------------------------------
// Agent sessions (see fleet::sessions)

use crate::fleet::sessions::{AgentKind, Control, MachineSessions, SessionInfo};

impl World {
    /// Seeded sessions: a busy Claude and an idle Codex on the Mac Studio, OpenCode on the NUC.
    pub fn sessions(&self, machine_id: &str) -> MachineSessions {
        let now = Utc::now();
        let session = |name: &str, agent: &str, agent_name: &str, title: &str, cwd: &str, busy: bool, mins: i64, clients: u32| SessionInfo {
            id: format!("{machine_id}/{name}"),
            machine_id: machine_id.into(),
            name: name.into(),
            agent: agent.into(),
            agent_name: agent_name.into(),
            title: title.into(),
            cwd: cwd.into(),
            status: "running".into(),
            exit_code: None,
            busy,
            attention: (name == "codex-19be").then(|| "waiting".to_string()),
            clients,
            created_at: Some(now - chrono::Duration::minutes(mins)),
            last_output_at: Some(now - chrono::Duration::seconds(if busy { 1 } else { 140 })),
            attach_command: format!("ssh -t {machine_id} ~/.muxen/bin/muxen-agent attach {name}"),
            persistent: true,
            git: None,
        };
        let (agents, sessions): (Vec<&str>, Vec<SessionInfo>) = match machine_id {
            "forge" => (
                vec!["claude", "codex", "opencode"],
                vec![
                    session("claude-7f3a", "claude", "Claude Code", "Checkout fix", "/Users/ci/code/checkout", true, 12, 1),
                    session("shell-2b81", "shell", "Shell", "Shell · checkout", "/Users/ci/code/checkout", false, 31, 0),
                ],
            ),
            "orbit" => (vec!["claude", "codex"], vec![session("codex-19be", "codex", "Codex", "Codex · ios-app", "/Users/ci/code/ios-app", false, 48, 0)]),
            "beacon" => (
                vec!["claude", "opencode"],
                vec![session("opencode-c40d", "opencode", "OpenCode", "OpenCode · api", "/home/ci/api", true, 5, 0)],
            ),
            "pebble" | "dev-1" | "sandbox" => (vec!["claude", "codex"], Vec::new()),
            "win-test" => (vec!["claude"], Vec::new()),
            _ => (Vec::new(), Vec::new()),
        };
        MachineSessions {
            checked_at: Some(now),
            agent_version: Some("0.1.0".into()),
            agents: agents.into_iter().map(String::from).collect(),
            agents_checked_at: Some(now),
            gh: Some(crate::fleet::model::GhCli { installed: true, logged_in: machine_id != "beacon", account: (machine_id != "beacon").then(|| "octocat".into()), path: None }),
            sessions,
            events: Vec::new(),
            error: None,
        }
    }

    /// Git checkouts on the machines, where the sessions work and next to them.
    pub fn checkouts(&self, machine_id: &str) -> Vec<crate::fleet::model::Checkout> {
        let checkout = |path: &str, repo: &str, branch: &str, dirty: u32, subject: &str, mins: i64| crate::fleet::model::Checkout {
            machine_id: machine_id.into(),
            path: path.into(),
            repo: Some(repo.into()),
            remote: Some(format!("git@github.com:{repo}.git")),
            branch: branch.into(),
            upstream: Some(format!("origin/{branch}")),
            ahead: 0,
            behind: 0,
            dirty,
            last_commit_at: Some(Utc::now() - chrono::Duration::minutes(mins)),
            last_commit: subject.into(),
        };
        match machine_id {
            "forge" => vec![
                checkout("/Users/ci/code/checkout", "acme/shop", "fix-coupon-total", 1, "Apply coupons before tax", 40),
                checkout("/Users/ci/code/web", "acme/web", "main", 0, "Faster product pages", 300),
            ],
            "orbit" => vec![checkout("/Users/ci/code/ios-app", "acme/ios-app", "main", 0, "Offline mode, first cut", 120)],
            "beacon" => vec![checkout("/home/ci/api", "acme/api", "rate-limits", 0, "Rate limits per API key", 25)],
            _ => Vec::new(),
        }
    }

    /// What the seeded sessions' work changed: a pull request with checks
    /// still running, a worktree with no pull request yet, one failing a check.
    pub fn git(&self, machine_id: &str, folders: &[String]) -> Vec<(String, crate::fleet::sessions::git::GitStatus)> {
        use crate::fleet::sessions::git::GitStatus;
        use crate::github::hub::{BranchPull, Checks};
        let pull = |number: u64, title: &str, repo: &str, checks: Checks| BranchPull {
            number,
            title: title.into(),
            state: "open".into(),
            url: format!("https://github.com/{repo}/pull/{number}"),
            checks,
        };
        let status = |branch: &str, repo: &str, (files, added, removed, commits, uncommitted): (u32, u32, u32, u32, u32), worktree: bool, pull: Option<BranchPull>| GitStatus {
            root: String::new(),
            branch: branch.into(),
            base: Some("origin/main".into()),
            ahead: commits as i64,
            behind: 0,
            uncommitted,
            files,
            added,
            removed,
            commits,
            worktree,
            repo: Some(repo.into()),
            pull,
        };
        folders
            .iter()
            .filter_map(|f| {
                let s = match (machine_id, f.as_str()) {
                    ("forge", "/Users/ci/code/checkout") => status(
                        "fix-coupon-total",
                        "acme/shop",
                        (4, 86, 23, 2, 1),
                        false,
                        Some(pull(142, "Fix the checkout total when a coupon is applied", "acme/shop", Checks { total: 6, passed: 5, pending: 1, ..Default::default() })),
                    ),
                    ("orbit", "/Users/ci/code/ios-app") => status("offline-mode", "acme/ios-app", (7, 312, 45, 3, 0), true, None),
                    ("orbit", "/Users/ci/muxen/workspaces/ws-9c4e71") => GitStatus {
                        pull: Some(BranchPull { state: "merged".into(), ..pull(77, "Share a list from the share sheet", "acme/ios-app", Checks { total: 3, passed: 3, ..Default::default() }) }),
                        ..status("share-sheet", "acme/ios-app", (6, 204, 31, 3, 0), true, None)
                    },
                    ("beacon", "/home/ci/api") => status(
                        "rate-limits",
                        "acme/api",
                        (5, 140, 12, 4, 0),
                        false,
                        Some(pull(318, "Rate limits for the public API", "acme/api", Checks { total: 4, passed: 3, failed: 1, failing: vec!["lint".into()], ..Default::default() })),
                    ),
                    _ => return None,
                };
                Some((f.clone(), s))
            })
            .collect()
    }

    pub fn start_session(&self, spec: &MachineSpec, kind: &AgentKind, name: &str, cwd: &str, title: &str) -> SessionInfo {
        SessionInfo {
            id: format!("{}/{name}", spec.id),
            machine_id: spec.id.clone(),
            name: name.into(),
            agent: kind.id.into(),
            agent_name: kind.name.into(),
            title: title.into(),
            cwd: cwd.into(),
            status: "running".into(),
            exit_code: None,
            busy: true,
            attention: None,
            clients: 0,
            created_at: Some(Utc::now()),
            last_output_at: Some(Utc::now()),
            attach_command: format!("ssh -t {} ~/.muxen/bin/muxen-agent attach {name}", spec.host),
            persistent: true,
            git: None,
        }
    }

    pub fn control_session(&self, _machine_id: &str, name: &str, action: &Control) -> Result<String, String> {
        Ok(match action {
            Control::Read { .. } => demo_screen_text(name),
            _ => String::new(),
        })
    }
}

/// What a demo session's screen says, for `session read`.
pub fn demo_screen_text(name: &str) -> String {
    match name.split('-').next().unwrap_or("") {
        "claude" => "> Fix the flaky checkout test\n\n⏺ Reading e2e/checkout.spec.ts\n⏺ The discount is applied after tax; moving it before.\n⏺ Running npm test -- e2e/checkout.spec.ts\n  ✓ 12 passed\n\n✻ Thinking…".into(),
        "codex" => "› Add a dark mode toggle to Settings\n\n• Updated SettingsView.swift (+42 -3)\n• Build succeeded\n\nWould you like to run the following command?\n\n  $ xcodebuild -scheme App -destination 'platform=iOS Simulator,name=iPhone 17' test\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for this command (a)\n  3. No, and tell Codex what to do differently (esc)\n\n  Press enter to confirm or esc to cancel\n".into(),
        _ => "$ ".into(),
    }
}

// ---------------------------------------------------------------------------
// GitHub, simulated: the demo never reads the user's real account.

pub mod github {
    use chrono::{Duration as Ago, Utc};

    use crate::github::hub::{Checks, Comment, Detail, Inbox, Item, Label, PullInfo, Repo, RepoItems, Review, Run, Viewer};

    pub const LOGIN: &str = "jordan";

    fn label(name: &str, color: &str) -> Label {
        Label { name: name.into(), color: color.into() }
    }

    #[allow(clippy::too_many_arguments)]
    fn item(repo: &str, number: u64, title: &str, pull: bool, author: &str, labels: Vec<Label>, comments: u32, minutes_ago: i64) -> Item {
        let updated = Utc::now() - Ago::minutes(minutes_ago);
        Item {
            repo: repo.into(),
            number,
            title: title.into(),
            pull_request: pull,
            draft: false,
            state: "open".into(),
            author: author.into(),
            assignees: if pull { Vec::new() } else { vec![LOGIN.into()] },
            labels,
            comments,
            created_at: updated - Ago::hours(30),
            updated_at: updated,
            html_url: format!("https://github.com/{repo}/{}/{number}", if pull { "pull" } else { "issues" }),
        }
    }

    fn repo(full_name: &str, description: &str, private: bool, language: &str, stars: u32, open_issues: u32, hours_ago: i64) -> Repo {
        Repo {
            full_name: full_name.into(),
            description: Some(description.into()),
            private,
            fork: false,
            archived: false,
            default_branch: "main".into(),
            language: Some(language.into()),
            stars,
            open_issues,
            pushed_at: Some(Utc::now() - Ago::hours(hours_ago)),
            html_url: format!("https://github.com/{full_name}"),
            owner_is_org: full_name.starts_with("acme/"),
            favorite: false,
        }
    }

    pub fn viewer() -> Viewer {
        Viewer { login: LOGIN.into(), name: Some("Jordan Lee".into()), html_url: format!("https://github.com/{LOGIN}") }
    }

    pub fn repos() -> Vec<Repo> {
        vec![
            repo("acme/api", "Billing, accounts and the public API", true, "Go", 48, 12, 1),
            repo("acme/web", "The customer dashboard", true, "TypeScript", 23, 8, 2),
            repo("acme/desktop", "The desktop app for macOS, Windows and Linux", false, "Rust", 1310, 41, 5),
            repo("acme/ios", "The iPhone and iPad app", true, "Swift", 12, 6, 20),
            repo("acme/infra", "Terraform and deploy scripts", true, "HCL", 4, 2, 48),
            repo("jordan/dotfiles", "My shell, editor and terminal setup", false, "Shell", 31, 0, 96),
            repo("jordan/blog", "jordanlee.dev", false, "TypeScript", 6, 1, 200),
        ]
    }

    pub fn inbox() -> Inbox {
        Inbox {
            viewer: viewer(),
            review_requests: vec![
                item("acme/web", 91, "Checkout: show tax before the total", true, "mira", vec![label("frontend", "1d76db")], 3, 40),
                item("acme/api", 418, "Retry failed webhooks with backoff", true, "devon", vec![label("backend", "5319e7")], 1, 130),
                item("acme/desktop", 1302, "Remember each window's size per display", true, "lena", vec![label("enhancement", "a2eeef")], 6, 1500),
            ],
            assigned: vec![
                item("acme/api", 412, "Coupon total is wrong after tax", false, "sam", vec![label("bug", "d73a4a"), label("billing", "fbca04")], 5, 190),
                item("acme/web", 87, "Empty state for the orders page", false, "mira", vec![label("design", "c5def5")], 2, 1300),
                item("acme/desktop", 1290, "Crash when a monitor is unplugged", false, "kai", vec![label("bug", "d73a4a"), label("crash", "b60205")], 9, 2900),
            ],
            authored: vec![item("acme/api", 415, "Cache the plan lookup per request", true, LOGIN, vec![label("performance", "0e8a16")], 0, 300)],
        }
    }

    /// Everything in the inbox and a few more, by repository.
    fn all() -> Vec<Item> {
        let i = inbox();
        let mut out: Vec<Item> = i.review_requests.into_iter().chain(i.assigned).chain(i.authored).collect();
        out.push(item("acme/api", 409, "Rate limits per API key", false, "devon", vec![label("enhancement", "a2eeef")], 4, 4000));
        out.push(item("acme/desktop", 1287, "Tray icon is blurry on Linux", false, "noor", vec![label("bug", "d73a4a"), label("linux", "fef2c0")], 2, 5000));
        out
    }

    pub fn repo_items(full_name: &str) -> RepoItems {
        let (pulls, issues) = all().into_iter().filter(|i| i.repo.eq_ignore_ascii_case(full_name)).partition(|i| i.pull_request);
        RepoItems { issues, pulls }
    }

    pub fn repo_named(full_name: &str) -> Option<Repo> {
        repos().into_iter().find(|r| r.full_name.eq_ignore_ascii_case(full_name))
    }

    pub fn runs(full_name: &str) -> Vec<Run> {
        let run = |name: &str, conclusion: Option<&str>, title: &str, minutes: i64| Run {
            name: name.into(),
            status: if conclusion.is_some() { "completed".into() } else { "in_progress".into() },
            conclusion: conclusion.map(str::to_string),
            branch: "main".into(),
            event: "push".into(),
            title: title.into(),
            created_at: Utc::now() - Ago::minutes(minutes),
            html_url: format!("https://github.com/{full_name}/actions"),
        };
        vec![run("CI", None, "Cache the plan lookup per request", 4), run("CI", Some("success"), "Retry failed webhooks with backoff", 95), run("Deploy", Some("success"), "Release 2.14", 600)]
    }

    pub fn detail(full_name: &str, number: u64) -> Option<Detail> {
        let item = all().into_iter().find(|i| i.repo.eq_ignore_ascii_case(full_name) && i.number == number)?;
        let body = if item.pull_request {
            format!("{}.\n\nTested on staging; screenshots below.", item.title)
        } else {
            "Steps:\n1. Add a 10% coupon to an order over $100\n2. Check out with tax on\n\nThe coupon comes off after tax instead of before, so the total is a few cents off.".to_string()
        };
        // As many comments as the list says it has.
        let said = [("sam", "Seen on three orders today."), ("devon", "The discount runs after tax in pricing.go."), ("mira", "Same thing on the web checkout."), ("kai", "Support has two tickets about it."), ("sam", "Can we get this into Friday's release?")];
        let comments = said.iter().cycle().take(item.comments as usize).enumerate().map(|(k, (author, body))| Comment { author: author.to_string(), body: body.to_string(), created_at: Utc::now() - Ago::hours(20 - 3 * k as i64) }).collect();
        let pull = item.pull_request.then(|| PullInfo {
            head: "feature".into(),
            head_repo: Some(item.repo.clone()),
            base: "main".into(),
            mergeable: Some(true),
            additions: 84,
            deletions: 17,
            changed_files: 6,
            checks: Checks { total: 4, passed: 4, ..Default::default() },
            reviews: vec![Review { author: "devon".into(), state: "APPROVED".into() }],
        });
        Some(Detail { item, body, comments, pull })
    }
}
