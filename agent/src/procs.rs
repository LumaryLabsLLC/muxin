//! What's running on this machine (the app's Processes): each process with
//! the CPU and memory it uses, and the machine's GPUs, busy and memory:
//! NVIDIA (nvidia-smi, with each process's GPU memory), AMD on Linux (its
//! sysfs counters) and Apple's (ioreg). Stopping one: SIGTERM, or SIGKILL;
//! another user's takes root (the user's password, as for services).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::logs::quick;
use crate::root::{as_root, tool};

/// Processes sent back: the busiest.
const MOST: usize = 300;

/// `{"op": "processes"}`: processes (busiest first) and GPUs.
pub fn list() -> Value {
    let me = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
    let gpu_mem = nvidia_apps();
    let mut procs = ps();
    let total = procs.len();
    procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.rss.cmp(&a.rss)));
    procs.truncate(MOST);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let list: Vec<Value> = procs
        .iter()
        .map(|p| {
            json!({
                "pid": p.pid, "ppid": p.ppid, "user": p.user, "name": p.name, "command": p.command,
                "cpu": p.cpu, "memory": p.rss * 1024, "seconds": p.seconds,
                "gpu_memory": gpu_mem.get(&p.pid), "yours": !me.is_empty() && p.user == me,
            })
        })
        .collect();
    json!({ "ok": true, "processes": list, "total": total, "cores": cores, "gpus": gpus() })
}

struct Proc {
    pid: u32,
    ppid: u32,
    user: String,
    cpu: f64,
    rss: u64,
    seconds: u64,
    name: String,
    command: String,
}

/// `ps`, twice: the numbers with the command line, and each one's program
/// name (a Mac's may have spaces in its path).
fn ps() -> Vec<Proc> {
    let mac = cfg!(target_os = "macos");
    let fields = if mac { "pid=,ppid=,user=,pcpu=,rss=,etime=,args=" } else { "pid=,ppid=,user=,pcpu=,rss=,etimes=,args=" };
    let all = if mac { "-axo" } else { "-eo" };
    let names: HashMap<u32, String> = quick("ps", &[all, "pid=,comm="])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let (pid, comm) = l.split_once(char::is_whitespace)?;
            let comm = comm.trim();
            Some((pid.parse().ok()?, comm.rsplit('/').next().unwrap_or(comm).to_string()))
        })
        .collect();
    quick("ps", &[all, fields]).unwrap_or_default().lines().filter_map(|l| parse_ps(l, &names, mac)).collect()
}

fn parse_ps(line: &str, names: &HashMap<u32, String>, mac: bool) -> Option<Proc> {
    let mut it = line.split_whitespace();
    let pid: u32 = it.next()?.parse().ok()?;
    let ppid: u32 = it.next()?.parse().ok()?;
    let user = it.next()?.to_string();
    let cpu: f64 = it.next()?.parse().ok()?;
    let rss: u64 = it.next()?.parse().ok()?;
    let when = it.next()?;
    let seconds = if mac { etime(when)? } else { when.parse().ok()? };
    let command = it.collect::<Vec<_>>().join(" ");
    let name = names.get(&pid).cloned().unwrap_or_else(|| command.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("").trim_matches(['[', ']']).to_string());
    Some(Proc { pid, ppid, user, cpu, rss, seconds, name, command })
}

/// "1-02:03:04", "02:03:04", "03:04" as seconds.
fn etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let (h, m, sec) = match parts[..] {
        [h, m, s] => (h, m, s),
        [m, s] => (0, m, s),
        _ => return None,
    };
    Some(days * 86_400 + h * 3600 + m * 60 + sec)
}

/// NVIDIA's processes' GPU memory, by pid (bytes).
fn nvidia_apps() -> HashMap<u32, u64> {
    let Some(smi) = tool("nvidia-smi") else { return HashMap::new() };
    quick(&smi, &["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (pid, mem) = l.split_once(',')?;
            Some((pid.trim().parse().ok()?, mem.trim().parse::<u64>().ok()? * 1024 * 1024))
        })
        .collect()
}

/// The machine's GPUs: name, how busy (%), memory used and total (bytes),
/// and temperature and power when the GPU says.
pub fn gpus() -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(smi) = tool("nvidia-smi") {
        for l in quick(&smi, &["--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw", "--format=csv,noheader,nounits"]).unwrap_or_default().lines() {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok());
            out.push(json!({ "vendor": "nvidia", "name": f.first().unwrap_or(&""), "busy": num(1), "memory_used": num(2).map(|m| m as u64 * 1024 * 1024), "memory_total": num(3).map(|m| m as u64 * 1024 * 1024), "temperature": num(4), "power": num(5) }));
        }
    }
    if cfg!(target_os = "linux") {
        out.extend(amd());
    }
    if cfg!(target_os = "macos") {
        out.extend(apple());
    }
    out
}

/// AMD GPUs on Linux: amdgpu's counters in sysfs.
fn amd() -> Vec<Value> {
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
    let mut out = Vec::new();
    for c in cards.flatten() {
        let name = c.file_name().to_string_lossy().to_string();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = c.path().join("device");
        let read = |f: &str| std::fs::read_to_string(dev.join(f)).ok().map(|s| s.trim().to_string());
        if read("vendor").as_deref() != Some("0x1002") {
            continue;
        }
        let Some(busy) = read("gpu_busy_percent").and_then(|b| b.parse::<f64>().ok()) else { continue };
        // Its name: lspci's, by its PCI slot ("…[Radeon 840M / 860M Graphics]").
        let slot = read("uevent").and_then(|u| u.lines().find_map(|l| l.strip_prefix("PCI_SLOT_NAME=").map(str::to_string)));
        let pretty = slot.and_then(|s| quick("lspci", &["-s", &s])).and_then(|l| pci_name(l.lines().next()?));
        let num = |f: &str| read(f).and_then(|v| v.parse::<u64>().ok());
        out.push(json!({ "vendor": "amd", "name": format!("AMD {}", pretty.unwrap_or_else(|| "Radeon".into())), "busy": busy, "memory_used": num("mem_info_vram_used"), "memory_total": num("mem_info_vram_total") }));
    }
    out
}

/// lspci's name for a device: the last bracketed part ("Radeon 840M /
/// 860M Graphics"), else what follows its class.
fn pci_name(line: &str) -> Option<String> {
    let line = line.split(" (rev").next()?.trim();
    match (line.rfind('['), line.ends_with(']')) {
        (Some(i), true) => Some(line[i + 1..line.len() - 1].to_string()),
        _ => line.split_once(": ").map(|(_, n)| n.to_string()),
    }
}

/// A Mac's GPU: how busy, and the unified memory it holds.
fn apple() -> Vec<Value> {
    let text = quick("ioreg", &["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"]).unwrap_or_default();
    let num = |key: &str| text.split(&format!("\"{key}\"=")).nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse::<u64>().ok());
    let Some(busy) = num("Device Utilization %") else { return Vec::new() };
    let chip = quick("sysctl", &["-n", "machdep.cpu.brand_string"]).map(|s| s.trim().to_string()).unwrap_or_else(|| "Apple".into());
    vec![json!({ "vendor": "apple", "name": format!("{chip} GPU"), "busy": busy as f64, "memory_used": num("In use system memory"), "memory_total": Value::Null })]
}

/// `{"op": "process_signal", "pid", "signal": "TERM"|"KILL", "password"?}`.
pub fn signal(req: &Value) -> Value {
    let Some(pid) = req["pid"].as_u64().filter(|p| *p > 1).map(|p| p as i32) else {
        return json!({ "ok": false, "error": "Which process?" });
    };
    let (sig, name) = match req["signal"].as_str().unwrap_or("TERM") {
        "KILL" => (libc::SIGKILL, "KILL"),
        _ => (libc::SIGTERM, "TERM"),
    };
    if pid as u32 == std::process::id() {
        return json!({ "ok": false, "error": "That's Muxen's agent itself." });
    }
    if unsafe { libc::kill(pid, sig) } == 0 {
        return json!({ "ok": true });
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => json!({ "ok": false, "error": "It has already ended." }),
        // Another user's: as root.
        Some(libc::EPERM) => match as_root("exec kill -s \"$1\" \"$2\"", &[name, &pid.to_string()], req["password"].as_str().filter(|p| !p.is_empty())) {
            Ok(()) => json!({ "ok": true }),
            Err(e) => json!({ "ok": false, "error": e }),
        },
        _ => json!({ "ok": false, "error": std::io::Error::last_os_error().to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ps() {
        let names: HashMap<u32, String> = [(812, "node".to_string()), (90, "Google Chrome Helper".to_string())].into();
        let p = parse_ps("  812     1 adib      12.5  204800    3600 node /srv/app/server.js --port 3000", &names, false).unwrap();
        assert_eq!((p.pid, p.ppid, p.user.as_str(), p.cpu, p.rss, p.seconds, p.name.as_str()), (812, 1, "adib", 12.5, 204_800, 3600, "node"));
        assert_eq!(p.command, "node /srv/app/server.js --port 3000");
        let mac = parse_ps("90 1 adib 3.0 1024 1-02:03:04 /Applications/Google Chrome.app/Contents/Frameworks/Helper --type=renderer", &names, true).unwrap();
        assert_eq!((mac.seconds, mac.name.as_str()), (93_784, "Google Chrome Helper"));
        let kernel = parse_ps("2 0 root 0.0 0 99 [kthreadd]", &HashMap::new(), false).unwrap();
        assert_eq!(kernel.name, "kthreadd");
    }

    #[test]
    fn names_a_gpu() {
        assert_eq!(pci_name("64:00.0 Display controller: Advanced Micro Devices, Inc. [AMD/ATI] Krackan [Radeon 840M / 860M Graphics] (rev c2)").as_deref(), Some("Radeon 840M / 860M Graphics"));
        assert_eq!(pci_name("00:02.0 VGA compatible controller: Intel Corporation Device 7d55").as_deref(), Some("Intel Corporation Device 7d55"));
    }

    #[test]
    fn reads_elapsed_time() {
        assert_eq!(etime("03:04"), Some(184));
        assert_eq!(etime("02:03:04"), Some(7384));
        assert_eq!(etime("1-00:00:01"), Some(86_401));
        assert_eq!(etime("x"), None);
    }

    #[test]
    fn refuses_odd_pids() {
        assert_eq!(signal(&json!({ "pid": 1 }))["ok"], false);
        assert_eq!(signal(&json!({ "pid": std::process::id() }))["ok"], false);
    }
}
