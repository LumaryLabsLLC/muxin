//! Powering machines on and off: a cloud machine through its provider, and a
//! VM (or container) that runs in Docker on another of the user's machines by
//! starting or stopping that container there.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::MachineSpec;
use crate::fleet::model::ParentMachine;
use crate::fleet::Engine;
use crate::probe::{ProbeResult, Shell};

use super::remote;

/// How long a stopping container gets to shut down cleanly (Windows takes a
/// while) before Docker ends it.
const STOP_GRACE_SECS: u32 = 120;

impl Engine {
    /// Powers a machine on or off, however that's done for it.
    pub fn machine_power(self: &Arc<Self>, machine_id: &str, on: bool) -> Result<u64, String> {
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in Muxen anymore.")?;
        if spec.cloud.is_some() {
            return self.cloud_power(&spec.id, on);
        }
        let parent = self.current().machines.iter().find(|m| m.id == spec.id).and_then(|m| m.parent.clone()).filter(|p| p.container.is_some());
        match parent {
            Some(p) => self.vm_power(&spec, p, on),
            None => Err(format!("Muxen can't power {} on or off: it isn't a cloud machine, or a VM in a Docker container on another of your machines.", spec.name)),
        }
    }

    /// Starts or stops the Docker container a machine runs in, on the machine
    /// it runs on. Stopping gives it time to shut down cleanly.
    fn vm_power(self: &Arc<Self>, spec: &MachineSpec, parent: ParentMachine, on: bool) -> Result<u64, String> {
        let container = parent.container.clone().unwrap_or_default();
        if !valid_container(&container) {
            return Err(format!("\"{container}\" isn't a container name Muxen can use."));
        }
        let (host, host_probe) = self.machine(&parent.machine_id).ok_or_else(|| format!("{} isn't in Muxen anymore.", parent.name))?;
        host.refuse_if_disconnected()?;
        if self.machine_busy(&spec.id) || self.machine_busy(&host.id) {
            return Err("An operation is still running on this machine. Wait for it to finish.".into());
        }
        if self.demo_world().is_some() {
            return Err("The demo doesn't start or stop machines.".into());
        }
        let title = format!("{} {}", if on { "Start" } else { "Stop" }, spec.name);
        let op = self.begin_op(if on { "vm-start" } else { "vm-stop" }, title, Some(spec.id.clone()), None);
        let (me, spec) = (self.clone(), spec.clone());
        self.runtime().spawn(async move {
            let result = me.run_vm_power(op, &spec, &host, host_probe.as_ref(), &container, on).await;
            // Both, so the page shows the container and the machine as they are now.
            me.run_check(Some(vec![host.id.clone(), spec.id.clone()])).await;
            super::finish(&me, op, result);
        });
        Ok(op)
    }

    async fn run_vm_power(self: &Arc<Self>, op: u64, spec: &MachineSpec, host: &MachineSpec, host_probe: Option<&ProbeResult>, container: &str, on: bool) -> Result<(), String> {
        let shell = remote::shell_for(host, host_probe);
        let command = if on { format!("docker start {container}") } else { format!("docker stop -t {STOP_GRACE_SECS} {container}") };
        let script = match shell {
            // Docker Desktop's docker isn't on a non-login PATH on a Mac.
            Shell::Sh => format!("PATH=\"$PATH:/usr/local/bin:/opt/homebrew/bin\"\n{command}\n"),
            Shell::Powershell => format!("{command}\n"),
        };
        self.op_log(
            op,
            if on {
                format!("▸ Starting its container {container} on {}", host.name)
            } else {
                format!("▸ Shutting {} down: stopping its container {container} on {} (it gets up to two minutes)", spec.name, host.name)
            },
        );
        remote::run(self, op, host, shell, script, &[], Duration::from_secs(STOP_GRACE_SECS as u64 + 60)).await?;
        if on {
            self.op_log(op, format!("▸ Waiting for {} to answer ssh", spec.name));
            let started = Instant::now();
            loop {
                let ok = tokio::process::Command::new("ssh").args(crate::probe::ssh_args(spec)).arg("exit").output().await.is_ok_and(|o| o.status.success());
                if ok {
                    break;
                }
                if started.elapsed() > Duration::from_secs(240) {
                    return Err(format!("Its container started, but {} didn't answer ssh within 4 minutes. It may still be starting up.", spec.name));
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            self.op_log(op, format!("✓ {} is running", spec.name));
        } else {
            self.op_log(op, format!("✓ {} is stopped", spec.name));
        }
        Ok(())
    }
}

/// A Docker container name: letters, digits and _.- (nothing a shell reads).
fn valid_container(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.starts_with(['-', '.']) && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

#[cfg(test)]
mod tests {
    #[test]
    fn container_names_are_safe() {
        assert!(super::valid_container("zn-win11"));
        assert!(!super::valid_container("a;rm -rf /"));
        assert!(!super::valid_container("--rm"));
        assert!(!super::valid_container(""));
    }
}
