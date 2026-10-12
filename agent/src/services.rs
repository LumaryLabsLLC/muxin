//! A machine's services and containers, and starting, stopping and
//! restarting them (the app's Services page):
//! - Linux: systemd services, the system's (as root, through sudo) and the
//!   user's own (`systemctl --user`), with whether each starts at boot;
//! - a Mac: Homebrew's (`brew services`: Postgres, Redis, nginx, ...);
//! - both: Docker containers.

use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::logs::quick;
use crate::root::{as_root, tool, wants_password};

/// `{"op": "services"}`: services and containers with their state and what
/// they use (CPU as a percent of one core, memory in bytes), Docker's images
/// and Compose projects.
pub fn list(req: &Value) -> Value {
    let mac = cfg!(target_os = "macos");
    let docker = tool("docker");
    // What they use takes a moment to measure (Docker samples for a couple
    // of seconds): `"stats": false` answers at once without it.
    let stats = req["stats"].as_bool() != Some(false);
    let ((services, services_note), (containers, containers_note), images, compose) = std::thread::scope(|scope| {
        let svc = scope.spawn(|| if cfg!(windows) { crate::win::services(stats) } else if mac { brew(stats) } else { systemd(stats) });
        let ctr = scope.spawn(|| containers(stats));
        let img = scope.spawn(|| docker.as_deref().map(images).unwrap_or_default());
        let cmp = scope.spawn(|| docker.as_deref().map(compose_projects).unwrap_or_default());
        (svc.join().unwrap_or_default(), ctr.join().unwrap_or_default(), img.join().unwrap_or_default(), cmp.join().unwrap_or_default())
    });
    json!({
        "ok": true,
        "services": services,
        "services_note": services_note,
        "containers": containers,
        "containers_note": containers_note,
        "images": images,
        "compose": compose,
        "docker": docker.is_some(),
    })
}

/// CPU (percent of one core) and memory by systemd unit: two looks a moment
/// apart, for the CPU.
fn unit_usage(units: &[String], user: bool) -> std::collections::HashMap<String, (f64, u64)> {
    let mut out = std::collections::HashMap::new();
    if units.is_empty() {
        return out;
    }
    let look = || {
        let mut args: Vec<&str> = if user { vec!["--user"] } else { Vec::new() };
        args.extend(["show", "-p", "Id,MemoryCurrent,CPUUsageNSec"]);
        args.extend(units.iter().map(String::as_str));
        let mut seen = std::collections::HashMap::new();
        for block in quick("systemctl", &args).unwrap_or_default().split("\n\n") {
            let field = |k: &str| block.lines().find_map(|l| l.strip_prefix(k)).map(str::trim).unwrap_or("");
            let id = field("Id=").to_string();
            let mem = field("MemoryCurrent=").parse::<u64>().ok();
            let cpu = field("CPUUsageNSec=").parse::<u64>().ok();
            if !id.is_empty() {
                seen.insert(id, (mem, cpu));
            }
        }
        (std::time::Instant::now(), seen)
    };
    let (t0, first) = look();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (t1, second) = look();
    let elapsed = (t1 - t0).as_nanos() as f64;
    for (id, (mem, cpu)) in second {
        let before = first.get(&id).and_then(|(_, c)| *c);
        let pct = match (before, cpu) {
            (Some(a), Some(b)) if b >= a && elapsed > 0. => (b - a) as f64 / elapsed * 100.,
            _ => 0.,
        };
        out.insert(id, (pct, mem.unwrap_or(0)));
    }
    out
}

/// Docker's images.
/// Docker's images, each with the containers that use it (and whether they run).
fn images(docker: &str) -> Vec<Value> {
    let mut users: std::collections::HashMap<String, Vec<Value>> = std::collections::HashMap::new();
    let ids = quick(docker, &["ps", "-aq", "--no-trunc"]).unwrap_or_default();
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if !ids.is_empty() {
        let mut args = vec!["inspect", "--format", "{{.Image}}\t{{.State.Running}}\t{{.Name}}"];
        args.extend(ids.iter().copied());
        for l in quick(docker, &args).unwrap_or_default().lines() {
            let f: Vec<&str> = l.split('\t').collect();
            let [image, running, name] = f[..] else { continue };
            // `docker images` gives the image's id cut to 12 characters.
            let id: String = image.trim_start_matches("sha256:").chars().take(12).collect();
            users.entry(id).or_default().push(json!({ "name": name.trim_start_matches('/'), "running": running == "true" }));
        }
    }
    quick(docker, &["images", "--format", "{{.ID}}\t{{.Repository}}\t{{.Tag}}\t{{.Size}}\t{{.CreatedSince}}"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let [id, repo, tag, size, created] = f[..] else { return None };
            let name = if repo == "<none>" { id.to_string() } else if tag == "<none>" { repo.to_string() } else { format!("{repo}:{tag}") };
            let used_by = users.get(id).cloned().unwrap_or_default();
            Some(json!({ "id": id, "name": name, "repository": repo, "tag": tag, "size": size, "created": created, "used_by": used_by }))
        })
        .collect()
}

/// Compose projects Docker knows (running or not), with their files.
fn compose_projects(docker: &str) -> Vec<Value> {
    let text = quick(docker, &["compose", "ls", "--all", "--format", "json"]).unwrap_or_default();
    let list: Vec<Value> = serde_json::from_str(text.trim()).unwrap_or_default();
    list.iter()
        .filter_map(|p| {
            let name = p["Name"].as_str()?;
            let files = p["ConfigFiles"].as_str().unwrap_or("");
            Some(json!({ "name": name, "status": p["Status"].as_str().unwrap_or(""), "files": files }))
        })
        .collect()
}

/// "45.6MiB" as bytes.
fn bytes(text: &str) -> Option<u64> {
    let t = text.trim();
    let split = t.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = t.split_at(split);
    let n: f64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "B" => 1.,
        "kB" | "KB" => 1e3,
        "KiB" => 1024.,
        "MB" => 1e6,
        "MiB" => 1024. * 1024.,
        "GB" => 1e9,
        "GiB" => 1024. * 1024. * 1024.,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// systemd's services: (name, description, state, at boot), failed first.
fn systemd(stats: bool) -> (Vec<Value>, Option<String>) {
    if tool("systemctl").is_none() {
        return (Vec::new(), Some("This machine doesn't use systemd.".into()));
    }
    let mut out = Vec::new();
    for user in [false, true] {
        let scope: &[&str] = if user { &["--user"] } else { &[] };
        // Which start at boot ("enabled"), from their unit files.
        let mut boot = std::collections::HashMap::new();
        let files = [scope, &["list-unit-files", "--type=service", "--no-legend", "--plain", "--no-pager"]].concat();
        for line in quick("systemctl", &files).unwrap_or_default().lines() {
            let mut f = line.split_whitespace();
            if let (Some(unit), Some(state)) = (f.next(), f.next()) {
                boot.insert(unit.to_string(), state.to_string());
            }
        }
        let units = [scope, &["list-units", "--type=service", "--all", "--no-legend", "--plain", "--no-pager"]].concat();
        let listing = quick("systemctl", &units).unwrap_or_default();
        let running: Vec<String> = listing.lines().filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            (p.len() > 3 && p[3] == "running").then(|| p[0].to_string())
        }).collect();
        let usage = if stats { unit_usage(&running, user) } else { Default::default() };
        for line in listing.lines() {
            // UNIT LOAD ACTIVE SUB DESCRIPTION...
            let parts: Vec<&str> = line.split_whitespace().collect();
            let [unit, load, active, sub, ..] = parts[..] else { continue };
            if load != "loaded" {
                continue;
            }
            let state = if active == "failed" { "failed" } else { sub };
            // Templates' instances (getty@tty1) go by their template's file.
            let file = unit.split_once('@').map(|(t, _)| format!("{t}@.service")).unwrap_or_else(|| unit.to_string());
            let at_boot = boot.get(unit).or_else(|| boot.get(&file)).cloned().unwrap_or_default();
            let (cpu, memory) = usage.get(unit).copied().map(|(c, m)| (Some(c), Some(m))).unwrap_or((None, None));
            out.push(json!({
                "kind": "service",
                "target": unit,
                "name": unit.trim_end_matches(".service"),
                "detail": parts[4..].join(" "),
                "state": state,
                "at_boot": at_boot,
                "user": user,
                "cpu": cpu,
                "memory": memory,
            }));
        }
    }
    out.sort_by_key(|u| (rank(u["state"].as_str()), u["name"].as_str().unwrap_or("").to_lowercase()));
    (out, None)
}

/// Failed first (what needs looking at), then running, then the rest.
pub(crate) fn rank(state: Option<&str>) -> u8 {
    match state {
        Some("failed" | "error") => 0,
        Some("running" | "started") => 1,
        _ => 2,
    }
}

/// Homebrew's services on a Mac.
fn brew(stats: bool) -> (Vec<Value>, Option<String>) {
    let Some(brew) = tool("brew") else {
        return (Vec::new(), Some("Services on a Mac are Homebrew's (brew services); this Mac has no Homebrew.".into()));
    };
    let Some(text) = quick(&brew, &["services", "list", "--json"]) else {
        return (Vec::new(), Some("Homebrew didn't list its services.".into()));
    };
    let list: Vec<Value> = serde_json::from_str(&text).unwrap_or_default();
    // What each uses: its process (launchd's homebrew.mxcl.<name>), from ps.
    let mut pids = std::collections::HashMap::new();
    for line in quick("launchctl", &["list"]).unwrap_or_default().lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if let [pid, _, label] = f[..]
            && let (Ok(pid), Some(name)) = (pid.parse::<u32>(), label.strip_prefix("homebrew.mxcl."))
        {
            pids.insert(name.to_string(), pid);
        }
    }
    let mut usage = std::collections::HashMap::new();
    if stats && !pids.is_empty() {
        let list: Vec<String> = pids.values().map(|p| p.to_string()).collect();
        for line in quick("ps", &["-o", "pid=,rss=,%cpu=", "-p", &list.join(",")]).unwrap_or_default().lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if let [pid, rss, cpu] = f[..]
                && let (Ok(pid), Ok(rss), Ok(cpu)) = (pid.parse::<u32>(), rss.parse::<u64>(), cpu.parse::<f64>())
            {
                usage.insert(pid, (cpu, rss * 1024));
            }
        }
    }
    let mut out: Vec<Value> = list
        .iter()
        .filter_map(|s| {
            let name = s["name"].as_str()?;
            let status = s["status"].as_str().unwrap_or("none");
            let state = match status {
                "started" => "running",
                "error" => "failed",
                "none" | "stopped" => "stopped",
                other => other,
            };
            let (cpu, memory) = pids.get(name).and_then(|p| usage.get(p)).map(|(c, m)| (Some(*c), Some(*m))).unwrap_or((None, None));
            Some(json!({ "kind": "brew", "target": name, "name": name, "detail": format!("Homebrew · {status}"), "state": state, "at_boot": if status == "started" { "enabled" } else { "" }, "user": true, "cpu": cpu, "memory": memory }))
        })
        .collect();
    out.sort_by_key(|u| (rank(u["state"].as_str()), u["name"].as_str().unwrap_or("").to_lowercase()));
    (out, None)
}

/// Docker's containers, running first.
fn containers(stats: bool) -> (Vec<Value>, Option<String>) {
    let Some(docker) = tool("docker") else { return (Vec::new(), None) };
    let Some(text) = quick(&docker, &["ps", "-a", "--format", "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}\t{{.Ports}}"]) else {
        return (Vec::new(), Some("Docker didn't answer: is it running, and may this user use it (the docker group)?".into()));
    };
    // What the running ones use (docker samples for a moment).
    let mut usage = std::collections::HashMap::new();
    let sampled = if stats { quick(&docker, &["stats", "--no-stream", "--format", "{{.ID}}\t{{.CPUPerc}}\t{{.MemUsage}}"]).unwrap_or_default() } else { String::new() };
    for line in sampled.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if let [id, cpu, mem] = f[..] {
            let cpu = cpu.trim_end_matches('%').trim().parse::<f64>().ok();
            let used = mem.split('/').next().and_then(bytes);
            usage.insert(id.to_string(), (cpu, used));
        }
    }
    let mut out: Vec<Value> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let [id, name, image, state, status, ports] = f[..] else { return None };
            let (cpu, memory) = usage.get(id).copied().unwrap_or((None, None));
            Some(json!({ "kind": "docker", "target": id, "name": name, "detail": image, "state": state, "status": status, "ports": ports, "cpu": cpu, "memory": memory }))
        })
        .collect();
    out.sort_by_key(|c| (rank(c["state"].as_str()), c["name"].as_str().unwrap_or("").to_lowercase()));
    (out, None)
}

/// `{"op": "service_action", "kind", "target", "user"?, "action", "password"?}`:
/// start, stop, restart; `enable` and `disable` (starts at boot or not) for
/// systemd services.
pub fn act(req: &Value) -> Value {
    let kind = req["kind"].as_str().unwrap_or("");
    let target = req["target"].as_str().unwrap_or("");
    let action = req["action"].as_str().unwrap_or("");
    let user = req["user"].as_bool() == Some(true);
    let password = req["password"].as_str();
    let fine = |c: char| c.is_ascii_alphanumeric() || "@._-:".contains(c);
    if target.is_empty() || target.starts_with('-') || !target.chars().all(fine) {
        return json!({ "ok": false, "error": "That name has odd characters." });
    }
    let done = match (kind, action) {
        ("service", "start" | "stop" | "restart" | "enable" | "disable") if cfg!(windows) => crate::win::service_action(target, action),
        ("service", "start" | "stop" | "restart" | "enable" | "disable") if user => run("systemctl", &["--user", action, "--", target]),
        // The system's: as root. The action and unit arrive as $1 and $2.
        ("service", "start" | "stop" | "restart" | "enable" | "disable") => as_root("exec systemctl \"$1\" -- \"$2\"", &[action, target], password),
        ("brew", "start" | "stop" | "restart") => run(&tool("brew").unwrap_or_else(|| "brew".into()), &["services", action, target]),
        ("docker", "start" | "stop" | "restart") => run(&tool("docker").unwrap_or_else(|| "docker".into()), &[action, target]),
        ("docker", "remove") => run(&tool("docker").unwrap_or_else(|| "docker".into()), &["rm", "-f", target]),
        ("image", "remove") => run(&tool("docker").unwrap_or_else(|| "docker".into()), &["rmi", target]),
        // Images no container uses ("unused", any target).
        ("image", "prune") => run(&tool("docker").unwrap_or_else(|| "docker".into()), &["image", "prune", "-a", "-f"]),
        _ => Err(format!("Can't {action} a {kind}.")),
    };
    match done {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "password": wants_password(&e), "error": e }),
    }
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).output().map_err(|e| format!("couldn't run {program}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("It didn't work.").trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_odd_names_and_actions() {
        let r = act(&json!({ "kind": "docker", "target": "--rm", "action": "stop" }));
        assert!(r["error"].as_str().unwrap().contains("odd"));
        let r = act(&json!({ "kind": "docker", "target": "a;rm -rf", "action": "stop" }));
        assert!(r["error"].as_str().unwrap().contains("odd"));
        let r = act(&json!({ "kind": "docker", "target": "web", "action": "enable" }));
        assert_eq!(r["error"], "Can't enable a docker.");
        assert_eq!(rank(Some("failed")), 0);
        assert_eq!(bytes("45.6MiB"), Some(47_815_065));
        assert_eq!(bytes("1.5GiB"), Some(1_610_612_736));
        assert_eq!(bytes("812kB"), Some(812_000));
        assert_eq!(bytes("0B"), Some(0));
        assert_eq!(bytes("--"), None);
    }
}

/// `{"op": "compose_new", "folder", "name"}`: a folder with a starting
/// compose.yaml (an nginx to change), for the user to edit and bring up.
/// Replies with the file's path; an existing file is left as it is.
pub fn compose_new(req: &Value) -> Value {
    let name = req["name"].as_str().unwrap_or("").trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
        return json!({ "ok": false, "error": "Use letters, numbers, - and _ for the project's name." });
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let folder = req["folder"].as_str().map(str::trim).filter(|f| !f.is_empty()).map(|f| f.replacen('~', &home, 1)).unwrap_or_else(|| format!("{home}/compose/{name}"));
    if let Err(e) = std::fs::create_dir_all(&folder) {
        return json!({ "ok": false, "error": format!("Couldn't make {folder}: {e}") });
    }
    let file = format!("{folder}/compose.yaml");
    if !std::path::Path::new(&file).exists() {
        let template = format!("# {name}: docker compose up -d (or Up in Muxen)\nname: {name}\nservices:\n  web:\n    image: nginx:alpine\n    ports:\n      - \"8080:80\"\n    restart: unless-stopped\n");
        if let Err(e) = std::fs::write(&file, template) {
            return json!({ "ok": false, "error": format!("Couldn't write {file}: {e}") });
        }
    }
    json!({ "ok": true, "file": file, "folder": folder })
}

/// `{"op": "compose_describe", "project", "files"}`: a Compose project's
/// parts, for the app's picture of it: its services (image, ports, what each
/// waits for, networks, volumes and folders, health check) with their
/// containers' state, and its networks and volumes. From `docker compose
/// config` (the files as Docker reads them) and `ps`. Environment values
/// stay on the machine: only how many there are.
pub fn compose_describe(req: &Value) -> Value {
    let Some(docker) = tool("docker") else {
        return json!({ "ok": false, "error": "Docker isn't installed on this machine." });
    };
    let project = req["project"].as_str().unwrap_or("").trim();
    if project.is_empty() || !project.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
        return json!({ "ok": false, "error": "That isn't a Compose project's name." });
    }
    let mut files: Vec<String> = req["files"].as_str().unwrap_or("").split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_string).collect();
    if files.is_empty() {
        let known = compose_projects(&docker);
        files = known.iter().find(|p| p["name"] == project).and_then(|p| p["files"].as_str()).unwrap_or("").split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_string).collect();
    }
    let Some(first) = files.first() else {
        return json!({ "ok": false, "error": format!("Docker doesn't know where {project}'s compose file is.") });
    };
    let folder = first.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into());
    let mut args: Vec<&str> = vec!["compose", "-p", project];
    for f in &files {
        args.extend(["-f", f.as_str()]);
    }
    args.extend(["config", "--format", "json"]);
    let out = match Command::new(&docker).args(&args).current_dir(&folder).stdin(Stdio::null()).output() {
        Ok(out) => out,
        Err(e) => return json!({ "ok": false, "error": format!("Couldn't run docker: {e}") }),
    };
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("its compose file doesn't read").trim().to_string();
        return json!({ "ok": false, "error": format!("Docker can't read {project}: {why}") });
    }
    let config: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let ps = quick(&docker, &["compose", "-p", project, "ps", "--all", "--format", "json"]).unwrap_or_default();
    let mut reply = describe(&config, &ps, &folder);
    reply.insert("ok".into(), json!(true));
    reply.insert("name".into(), json!(project));
    reply.insert("folder".into(), json!(folder));
    reply.insert("files".into(), json!(files));
    Value::Object(reply)
}

/// The parts, from `config`'s JSON and `ps`'s (one object a line, or a list).
fn describe(config: &Value, ps: &str, folder: &str) -> serde_json::Map<String, Value> {
    let containers: Vec<Value> = match serde_json::from_str::<Vec<Value>>(ps.trim()) {
        Ok(list) => list,
        Err(_) => ps.lines().filter_map(|l| serde_json::from_str(l).ok()).collect(),
    };
    // A path in the project's folder as ./path.
    let short = |p: &str| match p.strip_prefix(folder) {
        Some(rest) if rest.is_empty() => ".".to_string(),
        Some(rest) if rest.starts_with('/') => format!(".{rest}"),
        _ => p.to_string(),
    };
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let mut services = Vec::new();
    if let Some(all) = config["services"].as_object() {
        for (name, s) in all {
            let ports: Vec<Value> = s["ports"]
                .as_array()
                .map(|ports| ports.iter().map(|p| json!({ "published": text(&p["published"]), "target": text(&p["target"]), "protocol": p["protocol"].as_str().unwrap_or("tcp"), "host_ip": p["host_ip"].as_str().unwrap_or("") })).collect())
                .unwrap_or_default();
            let depends_on: Vec<Value> = s["depends_on"].as_object().map(|d| d.iter().map(|(k, v)| json!({ "name": k, "condition": v["condition"].as_str().unwrap_or("service_started") })).collect()).unwrap_or_default();
            let networks: Vec<String> = match &s["networks"] {
                Value::Object(n) => n.keys().cloned().collect(),
                _ if s["network_mode"].is_string() => Vec::new(),
                _ => vec!["default".into()],
            };
            let volumes: Vec<Value> = s["volumes"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .map(|m| {
                            let kind = m["type"].as_str().unwrap_or("volume");
                            let source = m["source"].as_str().unwrap_or("");
                            json!({ "kind": kind, "source": if kind == "bind" { short(source) } else { source.to_string() }, "target": m["target"].as_str().unwrap_or(""), "read_only": m["read_only"].as_bool().unwrap_or(false) })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let command = match &s["command"] {
                Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
                Value::String(c) => c.clone(),
                _ => String::new(),
            };
            let env = s["environment"].as_object().map(|e| e.len()).unwrap_or(0);
            let mine: Vec<Value> = containers
                .iter()
                .filter(|c| c["Service"].as_str() == Some(name))
                .map(|c| json!({ "name": c["Name"].as_str().unwrap_or(""), "state": c["State"].as_str().unwrap_or(""), "health": c["Health"].as_str().unwrap_or(""), "status": c["Status"].as_str().unwrap_or("") }))
                .collect();
            services.push(json!({
                "name": name,
                "image": s["image"].as_str().unwrap_or(""),
                "build": s["build"]["context"].as_str().map(short).unwrap_or_default(),
                "ports": ports,
                "depends_on": depends_on,
                "network_mode": s["network_mode"].as_str().unwrap_or(""),
                "networks": networks,
                "volumes": volumes,
                "healthcheck": s["healthcheck"].is_object() && s["healthcheck"]["disable"].as_bool() != Some(true),
                "restart": s["restart"].as_str().unwrap_or(""),
                "command": command,
                "replicas": s["deploy"]["replicas"].as_u64(),
                "env": env,
                "containers": mine,
            }));
        }
    }
    let named = |key: &str| -> Vec<Value> { config[key].as_object().map(|m| m.iter().map(|(k, v)| json!({ "name": k, "external": v["external"].as_bool().unwrap_or(false) })).collect()).unwrap_or_default() };
    let mut out = serde_json::Map::new();
    out.insert("services".into(), Value::Array(services));
    out.insert("networks".into(), Value::Array(named("networks")));
    out.insert("volumes".into(), Value::Array(named("volumes")));
    out
}

#[cfg(test)]
mod compose_tests {
    use super::*;

    #[test]
    fn describes_a_project() {
        let config = json!({
            "name": "shop",
            "services": {
                "web": { "image": "nginx:alpine", "ports": [{ "published": "8080", "target": 80, "protocol": "tcp", "host_ip": "" }], "depends_on": { "api": { "condition": "service_started" } }, "networks": { "front": null }, "volumes": [{ "type": "bind", "source": "/srv/shop/nginx.conf", "target": "/etc/nginx/conf.d/default.conf", "read_only": true }] },
                "api": { "build": { "context": "/srv/shop/api" }, "environment": { "SECRET": "x", "A": "1" }, "depends_on": { "db": { "condition": "service_healthy" } }, "networks": { "front": null, "back": null }, "command": ["serve", "--port", "8000"] },
                "db": { "image": "postgres:17", "volumes": [{ "type": "volume", "source": "db-data", "target": "/var/lib/postgresql/data" }], "healthcheck": { "test": ["CMD", "pg_isready"] } }
            },
            "networks": { "front": { "name": "shop_front" }, "back": {} },
            "volumes": { "db-data": { "name": "shop_db-data" } }
        });
        let ps = "{\"Service\":\"web\",\"Name\":\"shop-web-1\",\"State\":\"running\",\"Health\":\"\",\"Status\":\"Up 2 minutes\"}\n{\"Service\":\"db\",\"Name\":\"shop-db-1\",\"State\":\"running\",\"Health\":\"healthy\",\"Status\":\"Up 2 minutes (healthy)\"}\n";
        let d = Value::Object(describe(&config, ps, "/srv/shop"));
        let svc = |n: &str| d["services"].as_array().unwrap().iter().find(|s| s["name"] == n).unwrap().clone();
        let (web, api, db) = (svc("web"), svc("api"), svc("db"));
        assert_eq!(web["ports"][0]["published"], "8080");
        assert_eq!(web["ports"][0]["target"], "80");
        assert_eq!(web["volumes"][0]["source"], "./nginx.conf");
        assert_eq!(web["volumes"][0]["read_only"], true);
        assert_eq!(web["containers"][0]["name"], "shop-web-1");
        assert_eq!(api["build"], "./api");
        assert_eq!(api["env"], 2);
        assert!(!api.to_string().contains("\"x\""), "environment values stay on the machine");
        assert_eq!(api["depends_on"][0]["condition"], "service_healthy");
        assert_eq!(api["command"], "serve --port 8000");
        assert_eq!(db["networks"][0], "default");
        assert_eq!(db["healthcheck"], true);
        assert_eq!(db["containers"][0]["health"], "healthy");
        assert_eq!(d["volumes"][0]["name"], "db-data");
        assert_eq!(d["networks"].as_array().unwrap().len(), 2);
    }
}
