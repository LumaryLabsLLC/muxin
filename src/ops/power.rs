//! Powering machines on and off: a cloud machine through its provider, a VM
//! (or container) that runs in Docker on another of the user's machines by
//! starting or stopping that container there, and any other machine by asking
//! its own system to shut down or restart, through its agent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::MachineSpec;
use crate::fleet::desktop::AddDisplayError;
use crate::fleet::model::ParentMachine;
use crate::fleet::Engine;
use crate::probe::{ProbeResult, Shell};

use super::remote;

/// How long a stopping container gets to shut down cleanly (Windows takes a
/// while) before Docker ends it.
const STOP_GRACE_SECS: u32 = 120;
/// How long a machine gets to stop answering once it's been told to go down.
const GOING_DOWN_SECS: u64 = 180;
/// How long a restarting machine gets to go down and come back.
const RESTART_SECS: u64 = 420;
/// How often it's asked in the meantime.
const WATCH_EVERY: Duration = Duration::from_secs(5);

/// What a machine that's up can be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsPower {
    Shutdown,
    Restart,
}

impl OsPower {
    /// What the agent calls it.
    pub fn word(self) -> &'static str {
        match self {
            OsPower::Shutdown => "shutdown",
            OsPower::Restart => "restart",
        }
    }

    /// For a button, a menu or a dialog's title.
    pub fn label(self) -> &'static str {
        match self {
            OsPower::Shutdown => "Shut down",
            OsPower::Restart => "Restart",
        }
    }
}

/// What's done to a VM's container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmPower {
    Start,
    Stop,
    Restart,
}

impl Engine {
    /// Powers a machine on or off, however that's done for it.
    pub fn machine_power(self: &Arc<Self>, machine_id: &str, on: bool) -> Result<u64, String> {
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in Muxin anymore.")?;
        if spec.cloud.is_some() {
            return self.cloud_power(&spec.id, on);
        }
        match self.vm_parent(&spec.id) {
            Some(p) => self.vm_power(&spec, p, if on { VmPower::Start } else { VmPower::Stop }),
            None => Err(format!("Muxin can't turn {} on from here: wake it over its network, if it's set up for that.", spec.name)),
        }
    }

    /// The machine a VM's container runs on.
    fn vm_parent(&self, machine_id: &str) -> Option<ParentMachine> {
        self.current().machines.iter().find(|m| m.id == machine_id).and_then(|m| m.parent.clone()).filter(|p| p.container.is_some())
    }

    /// Shuts a machine down or restarts it, however that's done for it. Ordinary
    /// machines are asked through their agent, which may want the user's password
    /// (sudo): `NeedsPassword` is the app's to ask for, and then this again. The
    /// operation it starts follows the machine going down and coming back.
    pub async fn machine_os_power(self: &Arc<Self>, machine_id: &str, action: OsPower) -> Result<u64, AddDisplayError> {
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in Muxin anymore.")?;
        if self.demo_world().is_some() {
            return Err(format!("The demo doesn't {} machines.", if action == OsPower::Shutdown { "shut down" } else { "restart" }).into());
        }
        // Its provider knows how, and the machine page says what it costs.
        if spec.cloud.is_some() && action == OsPower::Shutdown {
            return Ok(self.cloud_power(&spec.id, false)?);
        }
        if let Some(p) = self.vm_parent(&spec.id) {
            return Ok(self.vm_power(&spec, p, if action == OsPower::Shutdown { VmPower::Stop } else { VmPower::Restart })?);
        }
        if spec.is_local() {
            return Err("That's the computer Muxin is running on: use its own menu.".into());
        }
        if self.machine_busy(&spec.id) {
            return Err("An operation is still running on this machine. Wait for it to finish.".into());
        }
        // Its agent build goes by what the last check saw: there may not be one yet.
        if self.machine(&spec.id).is_some_and(|(_, probe)| probe.is_none()) {
            self.run_check(Some(vec![spec.id.clone()])).await;
        }
        self.ask_agent_for_power(&spec.id, action.word()).await?;
        let title = format!("{} {}", action.label(), spec.name);
        let op = self.begin_op(if action == OsPower::Shutdown { "machine-shutdown" } else { "machine-restart" }, title, Some(spec.id.clone()), None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = me.follow_os_power(op, &spec, action).await;
            me.run_check(Some(vec![spec.id.clone()])).await;
            super::finish(&me, op, result);
        });
        Ok(op)
    }

    /// Waits for a machine to stop answering and, restarting, to answer again.
    async fn follow_os_power(self: &Arc<Self>, op: u64, spec: &MachineSpec, action: OsPower) -> Result<(), String> {
        self.op_log(op, format!("▸ {} agreed. Waiting for it to go down", spec.name));
        let started = Instant::now();
        let mut down = false;
        loop {
            tokio::time::sleep(WATCH_EVERY).await;
            let up = remote::answers(spec).await;
            if !up && !down {
                down = true;
                self.run_check(Some(vec![spec.id.clone()])).await;
                if action == OsPower::Shutdown {
                    self.op_log(op, format!("✓ {} is off", spec.name));
                    return Ok(());
                }
                self.op_log(op, format!("▸ {} is down. Waiting for it to come back", spec.name));
            }
            if up && down {
                self.op_log(op, format!("✓ {} is back", spec.name));
                return Ok(());
            }
            let elapsed = started.elapsed().as_secs();
            if !down && elapsed > GOING_DOWN_SECS {
                return Err(format!("{} agreed, but it was still answering after {} minutes.", spec.name, GOING_DOWN_SECS / 60));
            }
            if elapsed > RESTART_SECS {
                return Err(format!("{} went down and hasn't come back after {} minutes. {}", spec.name, RESTART_SECS / 60, self.why_not_back(&spec.id)));
            }
        }
    }

    /// What may keep a machine from coming back by itself.
    fn why_not_back(&self, machine_id: &str) -> &'static str {
        let platform = self.current().machines.iter().find(|m| m.id == machine_id).map(|m| m.platform.clone()).unwrap_or_default();
        if cfg!(target_os = "ios") && !platform.starts_with("linux") {
            "This iPad reaches it again once someone has signed in there."
        } else if platform.starts_with("darwin") {
            "With FileVault on, it's waiting at its own screen for a password."
        } else {
            "It may still be starting up."
        }
    }

    /// What the user is told before a machine goes down: what ends with it, and
    /// what it takes to get it back.
    pub fn os_power_detail(&self, machine_id: &str, action: OsPower) -> String {
        let s = self.current();
        let Some(m) = s.machines.iter().find(|m| m.id == machine_id) else { return String::new() };
        power_words(&PowerFacts {
            name: &m.name,
            action,
            platform: &m.platform,
            sessions: s.sessions.iter().filter(|x| x.machine_id == m.id && x.status != "exited").count(),
            jobs: s.runners.iter().filter(|r| r.machine_id.as_deref() == Some(m.id.as_str()) && r.state == "working").count(),
            guests: s.machines.iter().filter(|g| g.parent.as_ref().is_some_and(|p| p.machine_id == m.id && p.relation != "through")).count(),
            cloud: m.cloud.is_some(),
            vm: m.parent.as_ref().is_some_and(|p| p.container.is_some()),
            on_ipad: cfg!(target_os = "ios"),
        })
    }

    /// Starts, stops or restarts the Docker container a machine runs in, on the
    /// machine it runs on. Stopping gives it time to shut down cleanly.
    fn vm_power(self: &Arc<Self>, spec: &MachineSpec, parent: ParentMachine, what: VmPower) -> Result<u64, String> {
        let container = parent.container.clone().unwrap_or_default();
        if !valid_container(&container) {
            return Err(format!("\"{container}\" isn't a container name Muxin can use."));
        }
        let (host, host_probe) = self.machine(&parent.machine_id).ok_or_else(|| format!("{} isn't in Muxin anymore.", parent.name))?;
        host.refuse_if_disconnected()?;
        if self.machine_busy(&spec.id) || self.machine_busy(&host.id) {
            return Err("An operation is still running on this machine. Wait for it to finish.".into());
        }
        if self.demo_world().is_some() {
            return Err("The demo doesn't start or stop machines.".into());
        }
        let (word, kind) = match what {
            VmPower::Start => ("Start", "vm-start"),
            VmPower::Stop => ("Stop", "vm-stop"),
            VmPower::Restart => ("Restart", "vm-restart"),
        };
        let op = self.begin_op(kind, format!("{word} {}", spec.name), Some(spec.id.clone()), None);
        let (me, spec) = (self.clone(), spec.clone());
        self.runtime().spawn(async move {
            let result = me.run_vm_power(op, &spec, &host, host_probe.as_ref(), &container, what).await;
            // Both, so the page shows the container and the machine as they are now.
            me.run_check(Some(vec![host.id.clone(), spec.id.clone()])).await;
            super::finish(&me, op, result);
        });
        Ok(op)
    }

    async fn run_vm_power(self: &Arc<Self>, op: u64, spec: &MachineSpec, host: &MachineSpec, host_probe: Option<&ProbeResult>, container: &str, what: VmPower) -> Result<(), String> {
        let shell = remote::shell_for(host, host_probe);
        let command = match what {
            VmPower::Start => format!("docker start {container}"),
            VmPower::Stop => format!("docker stop -t {STOP_GRACE_SECS} {container}"),
            VmPower::Restart => format!("docker restart -t {STOP_GRACE_SECS} {container}"),
        };
        let script = match shell {
            // Docker Desktop's docker isn't on a non-login PATH on a Mac.
            Shell::Sh => format!("PATH=\"$PATH:/usr/local/bin:/opt/homebrew/bin\"\n{command}\n"),
            Shell::Powershell => format!("{command}\n"),
        };
        self.op_log(
            op,
            match what {
                VmPower::Start => format!("▸ Starting its container {container} on {}", host.name),
                VmPower::Stop => format!("▸ Shutting {} down: stopping its container {container} on {} (it gets up to two minutes)", spec.name, host.name),
                VmPower::Restart => format!("▸ Restarting its container {container} on {} (it gets up to two minutes to stop)", host.name),
            },
        );
        remote::run(self, op, host, shell, script, &[], Duration::from_secs(STOP_GRACE_SECS as u64 + 60)).await?;
        if what != VmPower::Stop {
            self.op_log(op, format!("▸ Waiting for {} to answer ssh", spec.name));
            let started = Instant::now();
            loop {
                let ok = remote::answers(spec).await;
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

/// What `power_words` says something about.
struct PowerFacts<'a> {
    name: &'a str,
    action: OsPower,
    platform: &'a str,
    sessions: usize,
    jobs: usize,
    guests: usize,
    cloud: bool,
    vm: bool,
    on_ipad: bool,
}

fn power_words(f: &PowerFacts) -> String {
    let count = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let mut words = vec![format!("{} {} now.", f.name, if f.action == OsPower::Shutdown { "shuts down" } else { "restarts" })];
    if f.sessions > 0 {
        words.push(count(f.sessions, "agent session there ends.", "agent sessions there end."));
    }
    if f.jobs > 0 {
        words.push(count(f.jobs, "running job stops.", "running jobs stop."));
    }
    if f.guests > 0 {
        words.push(count(f.guests, "machine that runs on it goes down too.", "machines that run on it go down too."));
    }
    match f.action {
        OsPower::Shutdown if f.cloud => words.push("Turn it on again from Muxin. Its provider still bills it while it's off.".into()),
        OsPower::Shutdown if f.vm => words.push("Start it again from Muxin.".into()),
        OsPower::Shutdown => words.push("Turning it back on takes someone at the machine, or Wake on LAN if it's set up.".into()),
        OsPower::Restart if f.vm => {}
        OsPower::Restart => {
            if f.platform.starts_with("darwin") {
                words.push("With FileVault on, it waits at its own screen for a password before it comes back.".into());
            }
            if f.on_ipad && !f.platform.starts_with("linux") {
                words.push("This iPad reaches it again once someone has signed in there.".into());
            }
        }
    }
    words.join(" ")
}

/// A Docker container name: letters, digits and _.- (nothing a shell reads).
fn valid_container(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.starts_with(['-', '.']) && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::{OsPower, PowerFacts, power_words};

    fn facts<'a>(action: OsPower, platform: &'a str) -> PowerFacts<'a> {
        PowerFacts { name: "Forge", action, platform, sessions: 0, jobs: 0, guests: 0, cloud: false, vm: false, on_ipad: false }
    }

    #[test]
    fn the_warning_says_what_ends_and_how_it_comes_back() {
        assert_eq!(
            power_words(&PowerFacts { sessions: 2, jobs: 1, guests: 1, ..facts(OsPower::Shutdown, "linux-x64") }),
            "Forge shuts down now. 2 agent sessions there end. 1 running job stops. 1 machine that runs on it goes down too. Turning it back on takes someone at the machine, or Wake on LAN if it's set up."
        );
        assert_eq!(power_words(&PowerFacts { sessions: 1, ..facts(OsPower::Restart, "linux-x64") }), "Forge restarts now. 1 agent session there ends.");
        assert_eq!(power_words(&PowerFacts { on_ipad: true, ..facts(OsPower::Restart, "linux-arm64") }), "Forge restarts now.");
    }

    #[test]
    fn the_warning_knows_who_cant_come_back_alone() {
        let mac = power_words(&facts(OsPower::Restart, "darwin-arm64"));
        assert!(mac.contains("FileVault"), "{mac}");
        assert!(!mac.contains("iPad"), "{mac}");
        let on_ipad = power_words(&PowerFacts { on_ipad: true, ..facts(OsPower::Restart, "windows-x64") });
        assert!(on_ipad.ends_with("This iPad reaches it again once someone has signed in there."), "{on_ipad}");
        assert!(!on_ipad.contains("FileVault"), "{on_ipad}");
    }

    #[test]
    fn the_warning_knows_cloud_machines_and_vms() {
        assert!(power_words(&PowerFacts { cloud: true, ..facts(OsPower::Shutdown, "linux-x64") }).contains("bills"));
        assert!(power_words(&PowerFacts { vm: true, ..facts(OsPower::Shutdown, "linux-x64") }).ends_with("Start it again from Muxin."));
        assert_eq!(power_words(&PowerFacts { vm: true, ..facts(OsPower::Restart, "windows-x64") }), "Forge restarts now.");
    }

    #[test]
    fn container_names_are_safe() {
        assert!(super::valid_container("zn-win11"));
        assert!(!super::valid_container("a;rm -rf /"));
        assert!(!super::valid_container("--rm"));
        assert!(!super::valid_container(""));
    }
}
