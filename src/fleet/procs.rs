//! What's running on a machine (the agent's procs.rs): its processes, with
//! the CPU and memory each uses, and its GPUs. Stopping one goes through
//! Engine::service_action (kind "process"), so another user's asks for the
//! password the same way a system service does.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::Engine;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Process {
    pub pid: u32,
    #[serde(default)]
    pub ppid: u32,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    /// Percent of one core.
    #[serde(default)]
    pub cpu: f64,
    /// Bytes.
    #[serde(default)]
    pub memory: u64,
    /// How long it has run.
    #[serde(default)]
    pub seconds: u64,
    /// Its GPU memory (NVIDIA), bytes.
    #[serde(default)]
    pub gpu_memory: Option<u64>,
    /// The user Muxin's agent runs as owns it (no password to stop it).
    #[serde(default)]
    pub yours: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Gpu {
    /// "nvidia", "amd" or "apple".
    pub vendor: String,
    pub name: String,
    /// How busy, percent.
    #[serde(default)]
    pub busy: Option<f64>,
    #[serde(default)]
    pub memory_used: Option<u64>,
    /// None for a Mac's (it shares the machine's memory).
    #[serde(default)]
    pub memory_total: Option<u64>,
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Watts.
    #[serde(default)]
    pub power: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcessList {
    /// The busiest few hundred.
    #[serde(default)]
    pub processes: Vec<Process>,
    /// How many there are in all.
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub cores: usize,
    #[serde(default)]
    pub gpus: Vec<Gpu>,
}

impl Engine {
    /// A machine's processes (busiest first) and GPUs.
    pub async fn processes(self: &Arc<Self>, machine_id: &str) -> Result<ProcessList, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(demo());
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "processes" })).await?;
        serde_json::from_value(reply).map_err(|_| format!("{} listed its processes in a way Muxin can't read.", spec.name))
    }

    /// Just a machine's GPUs (for its page).
    pub async fn gpus(self: &Arc<Self>, machine_id: &str) -> Result<Vec<Gpu>, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(if machine_id == "beacon" { demo().gpus } else { Vec::new() });
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "gpus" })).await?;
        serde_json::from_value(reply["gpus"].clone()).map_err(|_| format!("{} listed its GPUs in a way Muxin can't read.", spec.name))
    }
}

fn demo() -> ProcessList {
    let p = |pid: u32, name: &str, command: &str, cpu: f64, mb: u64, gpu: Option<u64>| Process {
        pid,
        ppid: 1,
        user: "ci".into(),
        name: name.into(),
        command: command.into(),
        cpu,
        memory: mb << 20,
        seconds: 3600 * 5,
        gpu_memory: gpu.map(|g| g << 20),
        yours: true,
    };
    ProcessList {
        processes: vec![
            p(4121, "python3", "python3 train.py --epochs 40", 312.0, 8200, Some(14_300)),
            p(2210, "node", "node /srv/api/dist/server.js", 18.5, 420, None),
            p(1201, "nginx", "nginx: worker process", 1.2, 24, None),
            p(912, "postgres", "postgres: 17/main: checkpointer", 0.4, 180, None),
        ],
        total: 214,
        cores: 16,
        gpus: vec![Gpu { vendor: "nvidia".into(), name: "NVIDIA GeForce RTX 4090".into(), busy: Some(87.), memory_used: Some(14_800 << 20), memory_total: Some(24_564 << 20), temperature: Some(71.), power: Some(312.) }],
    }
}
