//! Virtual machines on the user's own machines. Windows runs in Docker with
//! KVM (dockur/windows) on a Linux machine; Linux runs in a container with
//! sshd on any machine with Docker. Muxin creates one, adds it to the user's
//! machines (under the one it runs on), waits until it answers ssh, and
//! destroys it again. Starting and stopping are in power.rs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::{inventory_edit, remote};
use crate::config::{MachineSpec, VmRef};
use crate::fleet::model::Machine;
use crate::fleet::Engine;
use crate::probe::{lf_script, Shell};
use remote::{Param, with_params};

const CREATE_SH: &str = include_str!("scripts/vm_create.sh");
const LINUX_SETUP_SH: &str = include_str!("scripts/vm_linux_setup.sh");
const DESTROY_SH: &str = include_str!("scripts/vm_destroy.sh");

/// What a VM can run.
#[derive(Debug, Clone, Serialize)]
pub struct VmSystem {
    /// What to pass as `system`: "windows-11", "ubuntu-24.04", ...
    pub id: &'static str,
    /// "windows" | "linux"
    pub os: &'static str,
    pub name: &'static str,
    /// What its first start involves.
    pub note: &'static str,
    #[serde(skip)]
    version: &'static str,
    #[serde(skip)]
    image: &'static str,
    /// Linux images that only exist for x86-64.
    #[serde(skip)]
    x86_only: bool,
}

const fn windows(id: &'static str, version: &'static str, name: &'static str) -> VmSystem {
    VmSystem { id, os: "windows", name, note: "Downloads Windows (about 6 GB) and installs it by itself: 15–30 minutes the first time. It runs unactivated until you enter a key.", version, image: "dockurr/windows", x86_only: true }
}

const fn linux(id: &'static str, image: &'static str, name: &'static str, x86_only: bool) -> VmSystem {
    VmSystem { id, os: "linux", name, note: "A container with ssh: ready in under a minute. It shares its host's kernel (no systemd).", version: id, image, x86_only }
}

pub const SYSTEMS: &[VmSystem] = &[
    windows("windows-11", "11", "Windows 11 Pro"),
    windows("windows-10", "10", "Windows 10 Pro"),
    windows("windows-server-2025", "2025", "Windows Server 2025"),
    windows("windows-server-2022", "2022", "Windows Server 2022"),
    linux("ubuntu-26.04", "ubuntu:26.04", "Ubuntu 26.04", false),
    linux("ubuntu-24.04", "ubuntu:24.04", "Ubuntu 24.04", false),
    linux("debian-13", "debian:13", "Debian 13", false),
    linux("fedora", "fedora:latest", "Fedora", false),
    linux("alpine", "alpine:latest", "Alpine", false),
    linux("arch", "archlinux:latest", "Arch Linux", true),
];

pub fn system(id: &str) -> Option<&'static VmSystem> {
    SYSTEMS.iter().find(|s| s.id.eq_ignore_ascii_case(id))
}

/// A machine VMs can run on, and which.
#[derive(Debug, Clone, Serialize)]
pub struct VmHost {
    pub machine_id: String,
    pub name: String,
    pub platform: String,
    pub online: bool,
    pub docker: bool,
    /// Linux with /dev/kvm: Windows VMs can run here.
    pub kvm: bool,
    pub cores: Option<u32>,
    pub memory_free: Option<u64>,
    pub disk_free: Option<u64>,
    /// The systems it can run (ids from `systems`).
    pub systems: Vec<String>,
    /// Why it can't run any, when it can't.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmOptions {
    pub hosts: Vec<VmHost>,
    pub systems: Vec<VmSystem>,
    pub default_cpus: u32,
    pub default_memory_gb: u32,
    pub default_disk_gb: u32,
}

/// A VM to create.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmCreate {
    pub name: String,
    /// A system id from `vm_options` ("windows-11", "ubuntu-24.04", ...).
    pub system: String,
    /// The machine to run it on; default: the one with the most free memory that can.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub cpus: Option<u32>,
    #[serde(default)]
    pub memory_gb: Option<u32>,
    /// Windows' disk (it grows as needed, up to this).
    #[serde(default)]
    pub disk_gb: Option<u32>,
    /// Agent CLIs to install (Linux): "claude", "codex", "gemini", "opencode", "amp", "pi".
    #[serde(default)]
    pub install: Vec<String>,
}

/// The systems a machine can run, or why none.
fn host_systems(m: &Machine, probe: Option<&crate::probe::ProbeResult>) -> (Vec<String>, Option<String>) {
    if m.status != "online" {
        return (Vec::new(), Some(format!("{} isn't online.", m.name)));
    }
    let Some(p) = probe.filter(|p| p.ok) else { return (Vec::new(), Some("Not checked yet.".into())) };
    if p.docker.is_none() {
        let why = p.docker_error.clone().map(|e| format!("Docker isn't usable: {e}")).unwrap_or_else(|| "Docker isn't installed.".into());
        return (Vec::new(), Some(why));
    }
    // A VM inside a VM: not here.
    if m.parent.as_ref().is_some_and(|x| x.inside()) {
        return (Vec::new(), Some(format!("{} is itself a VM.", m.name)));
    }
    let x86 = matches!(p.arch.as_deref(), Some("x86_64" | "amd64" | "AMD64"));
    let linux = p.os.as_deref() == Some("linux");
    let systems = SYSTEMS
        .iter()
        .filter(|s| match s.os {
            "windows" => linux && x86 && p.kvm,
            _ => p.os.as_deref() != Some("windows") && (x86 || !s.x86_only),
        })
        .map(|s| s.id.to_string())
        .collect();
    (systems, None)
}

impl Engine {
    /// The machines VMs can run on, what they can run, and the defaults.
    pub fn vm_options(self: &Arc<Self>) -> VmOptions {
        let s = self.current();
        let hosts = s
            .machines
            .iter()
            .filter(|m| !m.host.is_empty() && !m.is_disconnected())
            .map(|m| {
                let probe = self.machine(&m.id).and_then(|(_, p)| p);
                let (systems, reason) = host_systems(m, probe.as_ref());
                let p = probe.as_ref().filter(|p| p.ok);
                VmHost {
                    machine_id: m.id.clone(),
                    name: m.name.clone(),
                    platform: m.platform.clone(),
                    online: m.status == "online",
                    docker: p.is_some_and(|p| p.docker.is_some()),
                    kvm: p.is_some_and(|p| p.kvm),
                    cores: p.and_then(|p| p.cores),
                    memory_free: p.and_then(|p| p.mem_total.zip(p.mem_used).map(|(t, u)| t.saturating_sub(u))),
                    disk_free: p.and_then(|p| p.disk_free),
                    systems,
                    reason,
                }
            })
            .collect();
        VmOptions { hosts, systems: SYSTEMS.to_vec(), default_cpus: 4, default_memory_gb: 8, default_disk_gb: 64 }
    }

    /// Creates a VM on one of the user's machines and adds it to them.
    pub fn vm_create(self: &Arc<Self>, req: VmCreate) -> Result<u64, String> {
        if self.demo_world().is_some() {
            return Err("The demo doesn't create VMs.".into());
        }
        let name = req.name.trim().to_string();
        if name.is_empty() || name.len() > 60 {
            return Err("Give the VM a name (up to 60 characters).".into());
        }
        if self.current().machines.iter().any(|m| m.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("You already have a machine called \"{name}\"."));
        }
        let system = system(&req.system).ok_or_else(|| format!("Muxin doesn't know the system \"{}\". It knows: {}.", req.system, SYSTEMS.iter().map(|s| s.id).collect::<Vec<_>>().join(", ")))?;
        let options = self.vm_options();
        let host = match req.host.as_deref().filter(|h| !h.trim().is_empty()) {
            Some(h) => {
                let h = options.hosts.iter().find(|x| x.machine_id == h || x.name.eq_ignore_ascii_case(h)).ok_or_else(|| format!("No machine called {h}."))?;
                if !h.systems.iter().any(|s| s == system.id) {
                    return Err(match &h.reason {
                        Some(r) => format!("{} can't run VMs: {r}", h.name),
                        None if system.os == "windows" => format!("{} can't run Windows VMs: they need a Linux machine (x86-64) with KVM.", h.name),
                        None => format!("{} can't run {}.", h.name, system.name),
                    });
                }
                h.clone()
            }
            None => options
                .hosts
                .iter()
                .filter(|h| h.systems.iter().any(|s| s == system.id))
                .max_by_key(|h| h.memory_free.unwrap_or(0))
                .cloned()
                .ok_or_else(|| {
                    if system.os == "windows" {
                        "None of your machines can run a Windows VM: it needs a Linux machine (x86-64) with Docker and KVM.".to_string()
                    } else {
                        "None of your machines can run it: it needs one with Docker.".to_string()
                    }
                })?,
        };
        let (host_spec, _) = self.machine(&host.machine_id).ok_or("Its host isn't in Muxin anymore.")?;
        if self.machine_busy(&host_spec.id) {
            return Err(format!("An operation is still running on {}. Wait for it to finish.", host_spec.name));
        }
        let key_path = crate::probe::default_ssh_key().ok_or("There's no SSH key on this computer to sign in with. Make one with `ssh-keygen -t ed25519`.")?;
        let op = self.begin_op("vm-create", format!("Create {name} on {}", host_spec.name), Some(host_spec.id.clone()), None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = me.run_vm_create(op, &name, system, &host_spec, &key_path, &req).await;
            super::finish(&me, op, result);
        });
        Ok(op)
    }

    async fn run_vm_create(self: &Arc<Self>, op: u64, name: &str, system: &VmSystem, host: &MachineSpec, key_path: &str, req: &VmCreate) -> Result<(), String> {
        let home = dirs::home_dir().ok_or("No home folder")?;
        let key = std::fs::read_to_string(home.join(key_path.trim_start_matches("~/"))).map_err(|e| format!("Couldn't read {key_path}: {e}"))?;
        let key = key.trim().to_string();
        let user = crate::cloud::setup::user_name(&std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default());
        let slug = crate::util::slug(name);
        let container = format!("muxen-{slug}");
        let windows = system.os == "windows";
        let password = if windows { random_password() } else { String::new() };
        let cpus = req.cpus.unwrap_or(4).clamp(1, 64);
        let memory = req.memory_gb.unwrap_or(8).clamp(1, 512);
        let disk = req.disk_gb.unwrap_or(64).clamp(32, 4096);
        // The agent CLIs asked for, as npm packages (Linux; Windows has its own ways).
        let npm = if windows { String::new() } else { req.install.iter().filter_map(|a| crate::cloud::setup::npm_package(a)).collect::<Vec<_>>().join(" ") };

        let setup = lf_script(LINUX_SETUP_SH);
        let create = lf_script(CREATE_SH);
        let script = with_params(
            Shell::Sh,
            &[
                ("OR_OS", Param::Str(system.os)),
                ("OR_NAME", Param::Str(&container)),
                ("OR_IMAGE", Param::Str(system.image)),
                ("OR_VERSION", Param::Str(system.version)),
                ("OR_CPUS", Param::Str(&cpus.to_string())),
                ("OR_MEM", Param::Str(&if windows { format!("{memory}G") } else { format!("{memory}g") })),
                ("OR_DISK", Param::Str(&format!("{disk}G"))),
                ("OR_USER", Param::Str(&user)),
                ("OR_PASS", Param::Str(&password)),
                ("OR_KEY", Param::Str(&key)),
                ("OR_HOSTNAME", Param::Str(&slug)),
                ("OR_SETUP", Param::Str(&setup)),
                ("OR_NPM", Param::Str(&npm)),
            ],
            &create,
        );
        let secrets: Vec<String> = [password.clone()].into_iter().filter(|p| !p.is_empty()).collect();
        let printed = remote::run_lines(self, op, host, Shell::Sh, script, &secrets, Duration::from_secs(30 * 60)).await?;
        let port = |key: &str| printed.iter().find_map(|l| l.strip_prefix(&format!("{key}="))).and_then(|p| p.trim().parse::<u16>().ok());
        let ssh_port = port("ssh_port").ok_or("The VM started, but Muxin didn't learn its ssh port.")?;

        // Into the user's machines: ssh goes through its host to the port its
        // container publishes there, with a host key of its own.
        let mut ssh_options = Vec::new();
        if !host.is_local() {
            ssh_options.extend(["-J".to_string(), jump(host)]);
        }
        ssh_options.extend(["-o".to_string(), format!("HostKeyAlias={container}")]);
        let vm = VmRef { host: host.id.clone(), os: system.os.into(), version: system.id.into(), container: container.clone(), web_port: port("web_port"), created_at: Utc::now().to_rfc3339() };
        // Direct connections go to its host's address, on the UDP port forwarded to it.
        let host_address = if host.is_local() {
            "127.0.0.1".to_string()
        } else {
            let route = self.machine(&host.id).and_then(|(_, p)| p).and_then(|p| p.route);
            route.map(|r| r.hostname).unwrap_or_else(|| host.host.rsplit('@').next().unwrap_or(&host.host).to_string())
        };
        let direct = port("udp_port").map(|p| format!("{host_address}:{p}"));
        let mut new_id = None;
        let edit = |text: &str| {
            let (out, id) = inventory_edit::add_machine(text, name, "127.0.0.1")?;
            let out = inventory_edit::set_machine_vm(&out, &id, ssh_port, &user, &ssh_options, direct.as_deref(), &vm)?;
            new_id = Some(id);
            Ok(out)
        };
        inventory_edit::edit_file(self.inventory_path(), edit)?;
        let id = new_id.ok_or("The VM wasn't added.")?;
        self.reload_inventory_now();
        if windows {
            let _ = crate::cloud::secrets::set(&self.paths().dir, &password_key(&id), &password);
        }
        self.op_log(op, format!("✓ Added {name} to your machines"));

        let spec = self.machine(&id).map(|(s, _)| s).ok_or("The new VM went missing from config.toml.")?;
        if windows {
            self.op_log(op, "▸ Windows is installing itself (15–30 minutes). Watch it from its page.");
            self.follow_windows_install(op, host, &container, &spec).await?;
        } else {
            self.op_log(op, if npm.is_empty() { "▸ Waiting for it to answer ssh".to_string() } else { format!("▸ Installing {} and waiting for it to answer ssh (a few minutes)", req.install.join(", ")) });
            wait_for_ssh(&spec, Duration::from_secs(12 * 60)).await.map_err(|_| format!("{name} didn't answer ssh within 12 minutes. Its log: `docker logs {container}` on {}.", host.name))?;
        }

        // Muxin's agent, so terminals, sessions and files work right away.
        self.run_check(Some(vec![id.clone(), host.id.clone()])).await;
        let probe = self.machine(&id).and_then(|(_, p)| p);
        match crate::fleet::sessions::agent::ensure(&spec, probe.as_ref()).await {
            Ok(()) => self.op_log(op, "✓ Agent installed"),
            Err(e) => self.op_log(op, format!("! The agent didn't install yet ({e}); Muxin tries again when you open a terminal")),
        }
        self.op_log(op, format!("✓ {name} is ready"));
        Ok(())
    }

    /// Follows Windows' install in its container's log until it answers ssh.
    async fn follow_windows_install(self: &Arc<Self>, op: u64, host: &MachineSpec, container: &str, spec: &MachineSpec) -> Result<(), String> {
        let started = Instant::now();
        let mut seen: Vec<String> = Vec::new();
        let tail = format!("PATH=\"$PATH:/usr/local/bin\"\ndocker logs --tail 4 {container} 2>&1\n");
        while started.elapsed() < Duration::from_secs(90 * 60) {
            if let Ok(out) = remote::output(host, Shell::Sh, tail.clone(), Duration::from_secs(30)).await {
                for line in out.lines().map(|l| remote::strip_ansi(remote::clean_line(l, &[]).trim()).trim().to_string()) {
                    // Its own steps ("❯ Downloading Windows 11..."), not the progress counters.
                    let step = line.trim_start_matches('❯').trim().to_string();
                    if line.contains("ERROR") {
                        return Err(format!("Its installer stopped: {step}"));
                    }
                    if line.starts_with('❯') && !step.contains('%') && !seen.contains(&step) {
                        self.op_log(op, format!("  ▸ {step}"));
                        seen.push(step);
                    }
                }
            }
            if wait_for_ssh(spec, Duration::from_secs(1)).await.is_ok() {
                self.op_log(op, "✓ Windows is installed and answers ssh");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
        Err(format!("Windows didn't finish installing within 90 minutes. Watch its screen from {}'s page, or remove it.", spec.name))
    }

    /// Deletes a VM Muxin made: its container and disk on its host, and the
    /// machine from Muxin.
    pub fn vm_destroy(self: &Arc<Self>, machine_id: &str) -> Result<u64, String> {
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in Muxin anymore.")?;
        let vm = spec.vm.clone().ok_or_else(|| format!("{} isn't a VM Muxin made. Remove it from Muxin instead.", spec.name))?;
        if !valid_container(&vm.container) {
            return Err(format!("\"{}\" isn't a container Muxin made.", vm.container));
        }
        let (host, _) = self.machine(&vm.host).ok_or_else(|| format!("The machine {} runs on isn't in Muxin anymore; remove {} instead.", spec.name, spec.name))?;
        if self.machine_busy(&spec.id) || self.machine_busy(&host.id) {
            return Err("An operation is still running on this machine. Wait for it to finish.".into());
        }
        if self.demo_world().is_some() {
            return Err("The demo doesn't destroy VMs.".into());
        }
        let op = self.begin_op("vm-destroy", format!("Destroy {}", spec.name), Some(spec.id.clone()), None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = async {
                let script = with_params(Shell::Sh, &[("OR_NAME", Param::Str(&vm.container))], &lf_script(DESTROY_SH));
                remote::run(&me, op, &host, Shell::Sh, script, &[], Duration::from_secs(5 * 60)).await?;
                crate::fleet::link::reset(&spec.id);
                // Not remove_machine: this operation is the one keeping the machine busy.
                me.forget_machine(&spec.id)?;
                crate::cloud::secrets::delete(&me.paths().dir, &password_key(&spec.id));
                let _ = tokio::process::Command::new("ssh-keygen").args(["-R", &vm.container]).output().await;
                me.op_log(op, format!("✓ {} is destroyed", spec.name));
                me.trigger_check(Some(host.id.clone()));
                Ok(())
            }
            .await;
            super::finish(&me, op, result);
        });
        Ok(op)
    }

    /// A Windows VM's account password (Muxin made it; it's kept in the Keychain).
    pub fn vm_password(&self, machine_id: &str) -> Option<String> {
        crate::cloud::secrets::get(&self.paths().dir, &password_key(machine_id))
    }
}

fn password_key(machine_id: &str) -> String {
    format!("vm-{machine_id}-password")
}

/// How ssh's -J reaches the host: its destination as config.toml has it.
fn jump(host: &MachineSpec) -> String {
    let h = host.host.trim();
    let with_user = match host.user.as_deref().filter(|u| !u.is_empty() && !h.contains('@')) {
        Some(u) => format!("{u}@{h}"),
        None => h.to_string(),
    };
    match host.port {
        Some(p) => format!("{with_user}:{p}"),
        None => with_user,
    }
}

async fn wait_for_ssh(spec: &MachineSpec, timeout: Duration) -> Result<(), ()> {
    let started = Instant::now();
    loop {
        let ok = tokio::process::Command::new("ssh").args(crate::probe::ssh_args(spec)).arg("exit").output().await.is_ok_and(|o| o.status.success());
        if ok {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// 20 letters and digits from the system's random source.
fn random_password() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    const CHARS: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 20];
    let _ = SystemRandom::new().fill(&mut bytes);
    bytes.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

/// A container Muxin made: "muxen-" and a slug.
fn valid_container(name: &str) -> bool {
    name.strip_prefix("muxen-").is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_its_systems() {
        assert_eq!(system("windows-11").map(|s| s.version), Some("11"));
        assert_eq!(system("Ubuntu-24.04").map(|s| s.image), Some("ubuntu:24.04"));
        assert!(system("macos").is_none());
    }

    #[test]
    fn only_destroys_its_own_containers() {
        assert!(valid_container("muxen-win-dev"));
        assert!(!valid_container("zn-win11"));
        assert!(!valid_container("muxen-../../x"));
        assert!(!valid_container("muxen-"));
    }

    #[test]
    fn passwords_are_random_and_plain() {
        let (a, b) = (random_password(), random_password());
        assert_eq!(a.len(), 20);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn jumps_through_the_host_as_configured() {
        let inv = crate::config::Inventory::parse("[[machine]]\nname = \"Box\"\nhost = \"box\"\nuser = \"ci\"\nport = 2200\n").unwrap();
        assert_eq!(jump(&inv.machines[0]), "ci@box:2200");
    }
}
