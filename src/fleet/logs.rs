//! A machine's logs (see the agent's logs.rs): what there is to read, and
//! reading it. The app's log viewer follows one over the machine's link.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Engine;

/// What a machine has to read, by kind.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LogSources {
    #[serde(default)]
    pub system: Vec<LogSource>,
    /// Why some of it may be missing (the journal without its group).
    #[serde(default)]
    pub system_note: Option<String>,
    #[serde(default)]
    pub files: Vec<LogSource>,
    #[serde(default)]
    pub docker: Vec<LogSource>,
    #[serde(default)]
    pub docker_note: Option<String>,
    #[serde(default)]
    pub services: Vec<LogSource>,
    #[serde(default)]
    pub services_note: Option<String>,
}

/// One log: the system log, a file, a container, a service.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LogSource {
    /// system, kernel, file, docker or service.
    pub kind: String,
    /// The file's path, the container's id, the unit; empty for the system log
    /// (on a Mac, a process name narrows it).
    #[serde(default)]
    pub target: String,
    pub name: String,
    #[serde(default)]
    pub detail: String,
    /// A service of the user's own (systemctl --user).
    #[serde(default)]
    pub user: bool,
    /// A file the user can't read (root's).
    #[serde(default)]
    pub readable: Option<bool>,
    /// A container that's running.
    #[serde(default)]
    pub running: Option<bool>,
    /// A service's state: running, failed, exited, dead, ...
    #[serde(default)]
    pub state: Option<String>,
    /// The Compose project a container is part of (and a project's own name).
    #[serde(default)]
    pub project: Option<String>,
}

impl LogSource {
    /// What the agent needs to find it.
    pub fn request(&self) -> Value {
        json!({ "kind": self.kind, "target": self.target, "user": self.user })
    }

    /// A file at `path`, to open by hand.
    pub fn file(path: &str) -> LogSource {
        let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string();
        LogSource { kind: "file".into(), target: path.into(), name, detail: path.into(), ..Default::default() }
    }
}

impl Engine {
    /// What there is to read on a machine.
    pub async fn log_sources(self: &Arc<Self>, machine_id: &str) -> Result<LogSources, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(demo_sources());
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "log_sources" })).await?;
        serde_json::from_value(reply).map_err(|_| format!("{} listed its logs in a way Muxen can't read.", spec.name))
    }

    /// A log's last `lines` (holding `filter`, ignoring case, when given).
    pub async fn read_logs(self: &Arc<Self>, machine_id: &str, source: &LogSource, lines: u64, filter: Option<&str>) -> Result<Vec<String>, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(DEMO_LINES.iter().map(|l| l.to_string()).collect());
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let request = json!({ "op": "logs_read", "source": source.request(), "lines": lines, "filter": filter.unwrap_or("") });
        let reply = super::sessions::agent::ctl(&spec, &request).await?;
        Ok(reply["lines"].as_array().map(|l| l.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default())
    }
}

fn demo_sources() -> LogSources {
    let src = |kind: &str, target: &str, name: &str, detail: &str| LogSource { kind: kind.into(), target: target.into(), name: name.into(), detail: detail.into(), ..Default::default() };
    LogSources {
        system: vec![src("system", "", "Everything", "The systemd journal"), src("kernel", "", "Kernel", "The kernel's messages")],
        files: vec![src("file", "/var/log/syslog", "syslog", "/var/log/syslog"), src("file", "/srv/api/storage/logs/laravel.log", "laravel.log", "/srv/api/storage/logs/laravel.log")],
        docker: vec![src("docker", "4f2a", "postgres", "postgres:17 · Up 3 days"), src("docker", "9c1e", "redis", "redis:8 · Up 3 days")],
        services: vec![src("service", "nginx.service", "nginx", "A high performance web server"), src("service", "actions.runner.beacon-01.service", "actions.runner.beacon-01", "GitHub Actions Runner")],
        ..Default::default()
    }
}

/// What a log shows in the demo.
pub const DEMO_LINES: &[&str] = &[
    "2026-10-12T09:12:01-05:00 beacon nginx[1201]: 10.0.0.12 - - \"GET /health HTTP/1.1\" 200 2",
    "2026-10-12T09:12:03-05:00 beacon api[2210]: INFO  request completed path=/v1/jobs status=200 ms=41",
    "2026-10-12T09:12:04-05:00 beacon api[2210]: WARN  slow query took=812ms table=runs",
    "2026-10-12T09:12:07-05:00 beacon api[2210]: ERROR db timeout after 5000ms (pool exhausted)",
    "2026-10-12T09:12:09-05:00 beacon systemd[1]: Started actions.runner.beacon-01.service - GitHub Actions Runner.",
    "2026-10-12T09:12:11-05:00 beacon Runner.Listener[3120]: Listening for Jobs",
];
