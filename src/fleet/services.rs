//! A machine's services and containers (see the agent's services.rs), and
//! starting, stopping and restarting them.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::Engine;
use super::desktop::{AddDisplayError, password_for, refusal};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServiceList {
    #[serde(default)]
    pub services: Vec<Service>,
    /// Why there are none (no systemd, no Homebrew).
    #[serde(default)]
    pub services_note: Option<String>,
    #[serde(default)]
    pub containers: Vec<Service>,
    #[serde(default)]
    pub containers_note: Option<String>,
    #[serde(default)]
    pub images: Vec<Image>,
    #[serde(default)]
    pub compose: Vec<ComposeProject>,
    /// Docker is installed.
    #[serde(default)]
    pub docker: bool,
}

/// A Docker image.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Image {
    pub id: String,
    /// repository:tag (or the id, untagged).
    pub name: String,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub created: String,
    /// The containers made from it, running or not.
    #[serde(default)]
    pub used_by: Vec<ImageUser>,
}

/// A container made from an image.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImageUser {
    pub name: String,
    #[serde(default)]
    pub running: bool,
}

impl Image {
    /// Whether a running container uses it.
    pub fn running(&self) -> bool {
        self.used_by.iter().any(|u| u.running)
    }
}

/// A Docker Compose project, with its files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ComposeProject {
    pub name: String,
    /// "running(3)", "exited(1)", ...
    #[serde(default)]
    pub status: String,
    /// Its compose files, comma-separated.
    #[serde(default)]
    pub files: String,
}

impl ComposeProject {
    pub fn running(&self) -> bool {
        self.status.contains("running")
    }

    pub fn first_file(&self) -> &str {
        self.files.split(',').next().unwrap_or("").trim()
    }

    pub fn folder(&self) -> &str {
        let f = self.first_file();
        f.rsplit_once('/').map(|(d, _)| d).unwrap_or(f)
    }

    pub fn log_source(&self) -> super::logs::LogSource {
        super::logs::LogSource { kind: "compose".into(), target: self.name.clone(), name: self.name.clone(), detail: self.first_file().into(), ..Default::default() }
    }
}

/// A Compose project's parts (the agent's compose_describe): what the app
/// draws.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ComposeGraph {
    pub name: String,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub services: Vec<ComposeService>,
    #[serde(default)]
    pub networks: Vec<ComposeNamed>,
    #[serde(default)]
    pub volumes: Vec<ComposeNamed>,
}

/// A network or volume the project declares.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ComposeNamed {
    pub name: String,
    /// Made outside the project (it uses one that's there).
    #[serde(default)]
    pub external: bool,
}

/// One of a project's services.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ComposeService {
    pub name: String,
    #[serde(default)]
    pub image: String,
    /// The folder it's built from (./api), when it is.
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub ports: Vec<ComposePort>,
    /// What it waits for.
    #[serde(default)]
    pub depends_on: Vec<ComposeDependency>,
    /// host, none, service:x, ... (then no networks).
    #[serde(default)]
    pub network_mode: String,
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<ComposeMount>,
    #[serde(default)]
    pub healthcheck: bool,
    #[serde(default)]
    pub restart: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub replicas: Option<u64>,
    /// How many environment variables it gets (their values stay on the machine).
    #[serde(default)]
    pub env: usize,
    #[serde(default)]
    pub containers: Vec<ComposeContainer>,
}

/// A port open on the machine: host_ip:published, into the container's target.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ComposePort {
    #[serde(default)]
    pub published: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub host_ip: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ComposeDependency {
    pub name: String,
    /// service_started, service_healthy or service_completed_successfully.
    #[serde(default)]
    pub condition: String,
}

/// A named volume, a folder or file of the machine's (bind), or tmpfs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ComposeMount {
    /// volume, bind, tmpfs.
    pub kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

/// One of a service's containers, as it is now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ComposeContainer {
    pub name: String,
    /// running, exited, restarting, created, ...
    #[serde(default)]
    pub state: String,
    /// healthy, unhealthy, starting, or empty.
    #[serde(default)]
    pub health: String,
    #[serde(default)]
    pub status: String,
}

impl ComposeService {
    /// running, partly running ("1 of 2"), exited, ... or "not created".
    pub fn state(&self) -> String {
        let running = self.containers.iter().filter(|c| c.state == "running").count();
        match (self.containers.len(), running) {
            (0, _) => "not created".into(),
            (n, r) if r == n => "running".into(),
            (n, r) if r > 0 => format!("{r} of {n} running"),
            _ => self.containers[0].state.clone(),
        }
    }

    pub fn running(&self) -> bool {
        self.containers.iter().any(|c| c.state == "running")
    }

    /// healthy, unhealthy or starting (the worst of its containers).
    pub fn health(&self) -> &str {
        ["unhealthy", "starting", "healthy"].into_iter().find(|h| self.containers.iter().any(|c| c.health == *h)).unwrap_or("")
    }
}

/// What `docker run` gets.
#[derive(Debug, Clone, Default)]
pub struct DockerRun {
    pub image: String,
    pub name: String,
    /// host:container, each.
    pub ports: Vec<String>,
    /// KEY=value, each.
    pub env: Vec<String>,
    /// host path:container path, each.
    pub volumes: Vec<String>,
    /// no, always, unless-stopped, on-failure.
    pub restart: String,
    /// What to run instead of the image's own command.
    pub command: String,
}

/// A service (systemd's, Homebrew's) or a Docker container.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Service {
    /// service (systemd), brew or docker.
    pub kind: String,
    /// The unit, Homebrew formula or container id.
    pub target: String,
    pub name: String,
    /// Its description, or the container's image.
    #[serde(default)]
    pub detail: String,
    /// running, failed, exited, dead, stopped, ...
    #[serde(default)]
    pub state: String,
    /// A container's own status ("Up 3 days", "Exited (0) 2 months ago").
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub ports: String,
    /// enabled, disabled, static, ... (systemd: whether it starts at boot).
    #[serde(default)]
    pub at_boot: String,
    /// CPU, as a percent of one core, and memory in bytes (running ones).
    #[serde(default)]
    pub cpu: Option<f64>,
    #[serde(default)]
    pub memory: Option<u64>,
    /// One of the user's own (systemctl --user, Homebrew).
    #[serde(default)]
    pub user: bool,
}

impl Service {
    pub fn running(&self) -> bool {
        matches!(self.state.as_str(), "running" | "started")
    }

    /// Its log, in the log viewer.
    pub fn log_source(&self) -> super::logs::LogSource {
        // Homebrew's (a Mac): the system log, narrowed to its process.
        let kind = match self.kind.as_str() {
            "docker" => "docker",
            "brew" => "system",
            _ => "service",
        };
        super::logs::LogSource { kind: kind.into(), target: self.target.clone(), name: self.name.clone(), detail: self.detail.clone(), user: self.user, ..Default::default() }
    }

    /// Whether stopping it could cut Muxen off from the machine.
    pub fn keeps_muxen_connected(&self) -> bool {
        let n = self.name.to_lowercase();
        ["ssh", "sshd", "tailscaled", "networkmanager", "systemd-networkd", "networking", "systemd-resolved", "muxen-agent", "dbus", "wpa_supplicant"].contains(&n.as_str())
    }
}

impl Engine {
    /// A starting compose.yaml in a folder on the machine (~/compose/<name>
    /// unless given); its path.
    pub async fn compose_new(self: &Arc<Self>, machine_id: &str, name: &str, folder: Option<&str>) -> Result<String, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "compose_new", "name": name, "folder": folder.unwrap_or("") })).await?;
        reply["file"].as_str().map(str::to_string).ok_or_else(|| "The machine didn't say where it put the file.".into())
    }

    /// A Compose project's parts and its containers' state.
    pub async fn compose_describe(self: &Arc<Self>, machine_id: &str, project: &ComposeProject) -> Result<ComposeGraph, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(demo_compose());
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "compose_describe", "project": project.name, "files": project.files })).await?;
        serde_json::from_value(reply).map_err(|_| format!("{} described {} in a way Muxen can't read.", spec.name, project.name))
    }

    /// A machine's services and containers; with what they use when `stats`
    /// (a second or two slower: it's measured).
    pub async fn services(self: &Arc<Self>, machine_id: &str, stats: bool) -> Result<ServiceList, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(demo());
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "services", "stats": stats })).await?;
        serde_json::from_value(reply).map_err(|_| format!("{} listed its services in a way Muxen can't read.", spec.name))
    }

    /// Starts, stops or restarts one (`enable`, `disable`: whether a systemd
    /// service starts at boot). The system's services take the user's
    /// password (the app asks for it once, then keeps it in memory).
    pub async fn service_action(self: &Arc<Self>, machine_id: &str, service: &Service, action: &str) -> Result<(), AddDisplayError> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Err("The demo doesn't change services.".into());
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let password = password_for(&spec.id);
        // A process: stopped (TERM) or killed (KILL).
        let request = if service.kind == "process" {
            json!({ "op": "process_signal", "pid": service.target.parse::<u64>().unwrap_or(0), "signal": if action == "kill" { "KILL" } else { "TERM" }, "password": password.as_deref().unwrap_or("") })
        } else {
            json!({ "op": "service_action", "kind": service.kind, "target": service.target, "user": service.user, "action": action, "password": password.as_deref().unwrap_or("") })
        };
        let reply = super::sessions::agent::ctl_reply(&spec, &request).await?;
        if reply["ok"].as_bool() != Some(true) {
            return Err(refusal(&spec.id, &reply, "The machine couldn't do that."));
        }
        Ok(())
    }
}

fn demo() -> ServiceList {
    let svc = |name: &str, detail: &str, state: &str, at_boot: &str| Service {
        kind: "service".into(),
        target: format!("{name}.service"),
        name: name.into(),
        detail: detail.into(),
        state: state.into(),
        at_boot: at_boot.into(),
        ..Default::default()
    };
    let ctr = |name: &str, image: &str, state: &str, status: &str| Service { kind: "docker".into(), target: name.into(), name: name.into(), detail: image.into(), state: state.into(), status: status.into(), ..Default::default() };
    ServiceList {
        services: vec![
            svc("api", "Beacon API", "failed", "enabled"),
            svc("nginx", "A high performance web server", "running", "enabled"),
            svc("postgresql", "PostgreSQL RDBMS", "running", "enabled"),
            svc("actions.runner.beacon-01", "GitHub Actions Runner", "running", "enabled"),
            svc("redis-server", "Advanced key-value store", "dead", "disabled"),
        ],
        containers: vec![ctr("postgres", "postgres:17", "running", "Up 3 days"), ctr("minio", "minio/minio", "exited", "Exited (0) 2 weeks ago")],
        ..Default::default()
    }
}

/// The demo's shop: a web front, an API, a worker, Postgres and Redis.
fn demo_compose() -> ComposeGraph {
    let up = |name: &str, health: &str| vec![ComposeContainer { name: format!("shop-{name}-1"), state: "running".into(), health: health.into(), status: "Up 3 days".into() }];
    let dep = |name: &str, condition: &str| ComposeDependency { name: name.into(), condition: condition.into() };
    let vol = |kind: &str, source: &str, target: &str| ComposeMount { kind: kind.into(), source: source.into(), target: target.into(), read_only: kind == "bind" };
    let svc = |name: &str, image: &str| ComposeService { name: name.into(), image: image.into(), networks: vec!["back".into()], restart: "unless-stopped".into(), containers: up(name, ""), ..Default::default() };
    ComposeGraph {
        name: "shop".into(),
        folder: "/srv/shop".into(),
        files: vec!["/srv/shop/compose.yaml".into()],
        services: vec![
            ComposeService { ports: vec![ComposePort { published: "8080".into(), target: "80".into(), protocol: "tcp".into(), host_ip: String::new() }], depends_on: vec![dep("api", "service_started")], networks: vec!["front".into()], volumes: vec![vol("bind", "./nginx.conf", "/etc/nginx/conf.d/default.conf")], ..svc("web", "nginx:alpine") },
            ComposeService { depends_on: vec![dep("db", "service_healthy"), dep("cache", "service_started")], networks: vec!["front".into(), "back".into()], env: 2, ..svc("api", "traefik/whoami") },
            ComposeService { depends_on: vec![dep("cache", "service_healthy")], ..svc("worker", "redis:8-alpine") },
            ComposeService { volumes: vec![vol("volume", "db-data", "/var/lib/postgresql/data")], healthcheck: true, env: 3, containers: up("db", "healthy"), ..svc("db", "postgres:17-alpine") },
            ComposeService { volumes: vec![vol("volume", "cache-data", "/data")], healthcheck: true, containers: up("cache", "healthy"), ..svc("cache", "redis:8-alpine") },
        ],
        networks: vec![ComposeNamed { name: "front".into(), external: false }, ComposeNamed { name: "back".into(), external: false }],
        volumes: vec![ComposeNamed { name: "db-data".into(), external: false }, ComposeNamed { name: "cache-data".into(), external: false }],
    }
}
