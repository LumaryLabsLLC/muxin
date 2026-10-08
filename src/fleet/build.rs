//! Merges the inventory, the last machine checks and live GitHub data into a
//! `Snapshot`. Pure: everything it needs is passed in, so it's easy to test.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use super::model::*;
use crate::config::{Inventory, MachineSpec, Settings};
use crate::github::{is_github_hosted_label, scope_from_runner_url, GitHubData, JobData, RunnerData};
use crate::probe::{ProbeResult, ProbedRunner};
use crate::util;

const LOW_DISK_BYTES: u64 = 20_000_000_000;
const LOW_DISK_FRACTION: f64 = 0.10;
const HIGH_MEMORY_FRACTION: f64 = 0.95;
/// A Docker runner restarted this often is crash-looping.
const CRASH_LOOP_RESTARTS: u32 = 5;
/// A queued job nobody can pick up gets flagged after this long.
const STUCK_JOB_SECS: i64 = 120;

pub struct Inputs<'a> {
    pub now: DateTime<Utc>,
    pub demo: bool,
    pub inventory: &'a Inventory,
    pub inventory_info: InventoryInfo,
    pub settings: &'a Settings,
    pub probes: &'a HashMap<String, ProbeResult>,
    pub checking: &'a HashSet<String>,
    pub check: CheckInfo,
    pub github: Option<&'a GitHubData>,
    pub github_syncing: bool,
    pub token_source: String,
}

pub fn build(input: &Inputs) -> Snapshot {
    let gh = input.github;
    let configured = !input.settings.github.orgs.is_empty() || !input.settings.github.repos.is_empty();

    // Every runner GitHub reported, by lowercase name.
    let mut gh_runners: HashMap<String, (String, &RunnerData)> = HashMap::new();
    if let Some(gh) = gh {
        for scope in gh.scopes.iter().filter(|s| s.error.is_none()) {
            let scope_id = format!("{}:{}", scope.kind.as_str(), scope.name);
            for r in &scope.runners {
                gh_runners.entry(r.name.to_ascii_lowercase()).or_insert((scope_id.clone(), r));
            }
        }
    }
    let github_live = gh.is_some_and(|g| g.scopes.iter().any(|s| s.error.is_none()));
    let latest_version = gh.and_then(|g| g.latest_runner_version.as_deref());
    let running_jobs: Vec<&JobData> =
        gh.map(|g| g.jobs.iter().filter(|j| j.status == "in_progress").collect()).unwrap_or_default();

    let mut claimed: HashSet<String> = HashSet::new();
    let mut machines = Vec::new();
    let mut runners = Vec::new();

    for spec in &input.inventory.machines {
        let probe = input.probes.get(&spec.id);
        let probe_ok = probe.filter(|p| p.ok);
        let mut diagnostics = Vec::new();

        // Runner names on this machine: declared first, then discovered.
        let mut names: Vec<(String, bool)> = spec.runners.iter().map(|n| (n.clone(), true)).collect();
        if let Some(p) = probe_ok {
            if spec.discover {
                let mut found: Vec<&ProbedRunner> = p.runners.iter().collect();
                found.sort_by(|a, b| a.name.cmp(&b.name));
                for r in found {
                    if !names.iter().any(|(n, _)| n.eq_ignore_ascii_case(&r.name)) {
                        names.push((r.name.clone(), false));
                    }
                }
            }
        }

        let mut runner_ids = Vec::new();
        for (name, declared) in names {
            let key = name.to_ascii_lowercase();
            let id = format!("{}/{}", spec.id, name);
            let local = probe_ok.and_then(|p| p.runners.iter().find(|r| r.name.eq_ignore_ascii_case(&name)));
            let gh_entry = if claimed.contains(&key) { None } else { gh_runners.get(&key) };
            if gh_entry.is_some() {
                claimed.insert(key.clone());
            }

            let (state, source) = match (gh_entry, local, probe_ok) {
                (Some((_, r)), _, _) => (github_state(r), "github"),
                (None, Some(l), _) => (local_state(l), "probe"),
                (None, None, Some(_)) => ("offline", "probe"),
                (None, None, None) => ("unknown", "none"),
            };

            runner_diagnostics(&mut diagnostics, &id, &name, declared, local, probe_ok, gh_entry.map(|e| e.1), github_live, latest_version);

            let job = running_jobs.iter().find(|j| j.runner_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&name)));
            runners.push(Runner {
                id: id.clone(),
                name: name.clone(),
                machine_id: Some(spec.id.clone()),
                state: state.into(),
                source: source.into(),
                declared,
                labels: gh_entry.map(|(_, r)| r.labels.clone()).unwrap_or_default(),
                github: gh_entry.map(|(scope, r)| github_runner(scope, r)),
                local: local.map(local_runner),
                job_id: job.map(|j| j.id),
            });
            runner_ids.push(id);
        }

        let status = match probe {
            _ if !spec.is_checkable() => "manual",
            None => "unchecked",
            Some(p) if p.ok => "online",
            Some(_) => "offline",
        };
        machine_diagnostics(&mut diagnostics, spec, probe);
        diagnostics.sort_by_key(|d| level_rank(&d.level));

        let platform = probe_ok
            .map(|p| util::platform(p.os.as_deref().unwrap_or(""), p.arch.as_deref().unwrap_or("")))
            .filter(|p| !p.is_empty())
            .or_else(|| spec.platform.clone())
            .unwrap_or_default();

        let summary = summarize(runners.iter().filter(|r| runner_ids.contains(&r.id)));
        machines.push(Machine {
            id: spec.id.clone(),
            name: spec.name.clone(),
            host: spec.host.trim().to_string(),
            local: spec.is_local(),
            platform,
            kind: if runner_ids.is_empty() && spec.runners.is_empty() { "other" } else { "runner" }.into(),
            status: status.into(),
            checking: input.checking.contains(&spec.id),
            notes: spec.notes.clone(),
            runner_ids,
            summary,
            probe: probe.map(to_probe),
            diagnostics,
            workspaces_allowed: spec.workspaces && spec.is_checkable(),
            agents: Vec::new(),
            agent_version: None,
            sessions_error: None,
            gh: None,
        });
    }

    // Runners GitHub knows about that no machine claimed.
    let mut unassigned: Vec<(&String, &RunnerData)> = gh_runners
        .iter()
        .filter(|(key, _)| !claimed.contains(*key))
        .map(|(_, (scope, r))| (scope, *r))
        .collect();
    unassigned.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    for (scope, r) in &unassigned {
        let job = running_jobs.iter().find(|j| j.runner_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&r.name)));
        runners.push(Runner {
            id: format!("github/{}", r.name),
            name: r.name.clone(),
            machine_id: None,
            state: github_state(r).into(),
            source: "github".into(),
            declared: false,
            labels: r.labels.clone(),
            github: Some(github_runner(scope, r)),
            local: None,
            job_id: job.map(|j| j.id),
        });
    }

    let jobs = build_jobs(gh, &runners);
    let counts = {
        let s = summarize(runners.iter());
        Counts {
            machines: machines.len(),
            runner_machines: machines.iter().filter(|m| m.kind == "runner").count(),
            other_machines: machines.iter().filter(|m| m.kind == "other").count(),
            runners: runners.len(),
            working: s.working,
            idle: s.idle,
            offline: s.offline,
            unknown: s.unknown,
            queued: jobs.queued.len(),
            running: jobs.running.len(),
        }
    };

    let detected = detected_scopes(input.settings, input.probes);
    let github = GitHubInfo {
        configured,
        live: github_live,
        syncing: input.github_syncing,
        token_source: gh.map(|g| g.token_source.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| input.token_source.clone()),
        login: gh.and_then(|g| g.login.clone()),
        last_sync_at: gh.and_then(|g| g.synced_at),
        error: gh.and_then(|g| g.error.clone()),
        scopes: gh
            .map(|g| {
                g.scopes
                    .iter()
                    .map(|s| ScopeStatus {
                        kind: s.kind.as_str().into(),
                        name: s.name.clone(),
                        runners: s.runners.len(),
                        synced_at: s.synced_at,
                        error: s.error.clone(),
                        hint: s.hint.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        rate_limit: gh.and_then(|g| g.rate).map(|r| RateLimit { limit: r.limit, remaining: r.remaining, reset_at: r.reset_at }),
        detected,
        scanned_repos: gh.map(|g| g.scanned_repos.clone()).unwrap_or_default(),
        poll_seconds: input.settings.github_poll_seconds,
    };

    let attention = attention(input, &machines, &runners, &jobs, &github, unassigned.len());
    let updated_at = [input.check.finished_at, github.last_sync_at].into_iter().flatten().max();

    Snapshot {
        version: 0,
        generated_at: input.now,
        updated_at,
        demo: input.demo,
        inventory: input.inventory_info.clone(),
        check: input.check.clone(),
        github,
        counts,
        machines,
        runners,
        jobs,
        attention,
        operations: Vec::new(),
        workspaces: Vec::new(),
        sessions: Vec::new(),
        notifications: Vec::new(),
        checkouts: Vec::new(),
    }
}

fn github_state(r: &RunnerData) -> &'static str {
    match (r.status.as_str(), r.busy) {
        ("online", true) => "working",
        ("online", false) => "idle",
        _ => "offline",
    }
}

fn local_state(r: &ProbedRunner) -> &'static str {
    match (r.listener_pid, r.worker_pid) {
        (_, Some(_)) => "working",
        (Some(_), None) => "idle",
        _ => "offline",
    }
}

fn github_runner(scope: &str, r: &RunnerData) -> GitHubRunner {
    GitHubRunner {
        id: r.id,
        status: r.status.clone(),
        busy: r.busy,
        os: r.os.clone(),
        scope: scope.to_string(),
        labels: r.labels.clone(),
    }
}

fn local_runner(r: &ProbedRunner) -> LocalRunner {
    LocalRunner {
        dir: r.dir.clone(),
        url: r.url.clone(),
        version: r.version.clone(),
        service: r.service.clone(),
        service_state: r.service_state.clone(),
        listener: r.listener_pid.is_some(),
        worker: r.worker_pid.is_some(),
        listener_pid: r.listener_pid,
        worker_pid: r.worker_pid,
        container: r.container.clone(),
        image: r.image.clone(),
        container_state: r.container_state.clone(),
        restarts: r.restarts,
    }
}

fn to_probe(p: &ProbeResult) -> Probe {
    Probe {
        checked_at: p.checked_at,
        duration_ms: p.duration_ms,
        ok: p.ok,
        error: p.error.clone(),
        hostname: p.hostname.clone(),
        os: p.os.clone(),
        arch: p.arch.clone(),
        os_version: p.os_version.clone(),
        cores: p.cores,
        cpu_pct: p.cpu_pct,
        mem_total: p.mem_total,
        mem_used: p.mem_used,
        disk_total: p.disk_total,
        disk_free: p.disk_free,
        disk_path: p.disk_path.clone(),
        uptime_sec: p.uptime_sec,
        load: p.load.clone(),
        docker: p.docker.clone(),
        sudo: p.sudo.clone(),
    }
}

fn summarize<'a>(runners: impl Iterator<Item = &'a Runner>) -> RunnerSummary {
    let mut s = RunnerSummary::default();
    for r in runners {
        s.total += 1;
        match r.state.as_str() {
            "working" => s.working += 1,
            "idle" => s.idle += 1,
            "offline" => s.offline += 1,
            _ => s.unknown += 1,
        }
    }
    s
}

fn diag(level: &str, code: &str, message: String, hint: Option<String>, runner_id: Option<&str>) -> Diagnostic {
    Diagnostic { level: level.into(), code: code.into(), message, hint, runner_id: runner_id.map(str::to_string) }
}

fn level_rank(level: &str) -> u8 {
    match level {
        "error" => 0,
        "warning" => 1,
        "unknown" => 2,
        _ => 3,
    }
}

#[allow(clippy::too_many_arguments)]
fn runner_diagnostics(
    out: &mut Vec<Diagnostic>,
    id: &str,
    name: &str,
    declared: bool,
    local: Option<&ProbedRunner>,
    probe_ok: Option<&ProbeResult>,
    gh: Option<&RunnerData>,
    github_live: bool,
    latest_version: Option<&str>,
) {
    let rid = Some(id);
    if let Some(p) = probe_ok {
        match local {
            None if declared => out.push(diag(
                "warning",
                "runner-missing",
                format!("{name} isn't installed here: no runner folder with that name was found."),
                Some(if p.os.as_deref() == Some("windows") {
                    "If it lives somewhere unusual, add its folder to runner_dirs in fleet.toml.".into()
                } else {
                    "If it lives somewhere unusual, add its folder to runner_dirs in fleet.toml (globs work).".into()
                }),
                rid,
            )),
            Some(l) if l.container.is_some() && (l.container_state.as_deref() == Some("restarting") || l.restarts.unwrap_or(0) >= CRASH_LOOP_RESTARTS) => {
                let c = l.container.as_deref().unwrap_or_default();
                out.push(diag(
                    "warning",
                    "container-restarting",
                    format!("{name}'s container keeps restarting ({} restarts).", l.restarts.unwrap_or(0)),
                    Some(format!("docker logs --tail 50 {c}")),
                    rid,
                ));
            }
            Some(l) if l.container.is_some() && l.listener_pid.is_none() => {
                let c = l.container.as_deref().unwrap_or_default();
                let state = l.container_state.as_deref().unwrap_or("stopped");
                out.push(diag("warning", "runner-stopped", format!("{name}'s container is {state}."), Some(format!("docker start {c}")), rid));
            }
            Some(l) if l.listener_pid.is_none() => {
                let hint = match (l.service.is_some(), p.os.as_deref()) {
                    (true, Some("linux")) => format!("cd {} && sudo ./svc.sh start", l.dir),
                    (true, Some("windows")) => format!("Start-Service {}", l.service.as_deref().unwrap_or_default()),
                    (true, _) => format!("cd {} && ./svc.sh start", l.dir),
                    (false, Some("windows")) => format!("cd {}; .\\run.cmd", l.dir),
                    (false, _) => format!("cd {} && ./run.sh", l.dir),
                };
                out.push(diag("warning", "runner-stopped", format!("{name} is installed but not running."), Some(hint), rid));
            }
            Some(l) if gh.is_some_and(|g| g.status == "offline") => out.push(diag(
                "warning",
                "github-offline",
                format!("{name} is running here, but GitHub reports it offline."),
                Some(match &l.container {
                    Some(c) => format!("docker logs --tail 50 {c}"),
                    None => format!("Check the newest log in {}/_diag", l.dir),
                }),
                rid,
            )),
            _ => {}
        }
        if let (Some(l), Some(latest)) = (local, latest_version) {
            if let Some(v) = l.version.as_deref() {
                if version_lt(v, latest) {
                    out.push(diag("info", "update-available", format!("{name} runs {v}; {latest} is available."), None, rid));
                }
            }
        }
    } else if gh.is_some_and(|g| g.status == "offline") {
        out.push(diag("warning", "github-offline", format!("{name} is offline on GitHub."), None, rid));
    }

    if github_live && gh.is_none() && (local.is_some() || declared) {
        out.push(diag(
            "unknown",
            "not-on-github",
            format!("{name} wasn't found in the GitHub orgs and repos you watch."),
            local
                .and_then(|l| l.url.as_deref())
                .and_then(scope_from_runner_url)
                .map(|s| format!("It's registered to {s}. Add it in Settings.")),
            rid,
        ));
    }
}

fn machine_diagnostics(out: &mut Vec<Diagnostic>, spec: &MachineSpec, probe: Option<&ProbeResult>) {
    if !spec.is_checkable() {
        out.push(diag(
            "info",
            "no-host",
            "No host configured, so this machine isn't checked.".into(),
            Some("Set host in fleet.toml (an ssh destination, or \"local\").".into()),
            None,
        ));
        return;
    }
    let Some(p) = probe else { return };
    if !p.ok {
        let reason = p.error.clone().unwrap_or_else(|| "The check failed.".into());
        let hint = (!spec.is_local()).then(|| format!("ssh {}  # must work without a password prompt", spec.host.trim()));
        out.push(diag("error", "unreachable", format!("Unreachable: {reason}"), hint, None));
        return;
    }
    if let (Some(total), Some(free)) = (p.disk_total, p.disk_free) {
        if free < LOW_DISK_BYTES || (free as f64) < total as f64 * LOW_DISK_FRACTION {
            out.push(diag(
                "warning",
                "low-disk",
                format!("Low disk space: {} free of {}.", util::bytes(free), util::bytes(total)),
                Some("Runner _work folders and toolcaches grow over time; clean them up or add space.".into()),
                None,
            ));
        }
    }
    if let Some(err) = &p.docker_error {
        out.push(diag(
            "info",
            "docker-unusable",
            format!("Docker is installed but can't be used: {err}"),
            Some("sudo usermod -aG docker $USER  # then log in again".into()),
            None,
        ));
    }
    if let (Some(total), Some(used)) = (p.mem_total, p.mem_used) {
        if used as f64 > total as f64 * HIGH_MEMORY_FRACTION {
            out.push(diag(
                "warning",
                "high-memory",
                format!("Memory is nearly full: {} of {} used.", util::bytes(used), util::bytes(total)),
                None,
                None,
            ));
        }
    }
}

/// "2.317.0" < "2.321.0"
fn version_lt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> { v.trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parse(a) < parse(b)
}

fn labels_satisfied(job: &[String], runner: &[String]) -> bool {
    !runner.is_empty() && job.iter().all(|l| runner.iter().any(|r| r.eq_ignore_ascii_case(l)))
}

fn build_jobs(gh: Option<&GitHubData>, runners: &[Runner]) -> Jobs {
    let Some(gh) = gh else { return Jobs::default() };
    let by_name: HashMap<String, &Runner> = runners.iter().map(|r| (r.name.to_ascii_lowercase(), r)).collect();
    let mut jobs = Jobs::default();

    for j in &gh.jobs {
        let runner = j.runner_name.as_deref().and_then(|n| by_name.get(&n.to_ascii_lowercase()));
        let self_hosted_label = j.labels.iter().any(|l| l.eq_ignore_ascii_case("self-hosted"));
        let matching: Vec<&Runner> = if j.labels.is_empty() {
            Vec::new()
        } else {
            runners.iter().filter(|r| labels_satisfied(&j.labels, &r.labels)).collect()
        };

        let keep = match j.status.as_str() {
            "in_progress" => runner.is_some() || self_hosted_label,
            _ => {
                self_hosted_label
                    || !matching.is_empty()
                    || (!j.labels.is_empty() && !j.labels.iter().all(|l| is_github_hosted_label(l)))
            }
        };
        if !keep {
            continue;
        }

        let job = Job {
            id: j.id,
            run_id: j.run_id,
            name: j.name.clone(),
            workflow: j.workflow.clone(),
            repo: j.repo.clone(),
            branch: j.branch.clone(),
            event: j.event.clone(),
            actor: j.actor.clone(),
            url: j.url.clone(),
            status: j.status.clone(),
            labels: j.labels.clone(),
            runner_name: j.runner_name.clone(),
            runner_id: runner.map(|r| r.id.clone()),
            created_at: j.created_at,
            started_at: j.started_at,
            matching_online: matching.iter().filter(|r| matches!(r.state.as_str(), "working" | "idle")).count(),
            matching: matching.iter().map(|r| r.id.clone()).collect(),
        };
        if j.status == "in_progress" { jobs.running.push(job) } else { jobs.queued.push(job) }
    }
    jobs.queued.sort_by_key(|j| j.created_at);
    jobs.running.sort_by_key(|j| j.started_at.unwrap_or(j.created_at));
    jobs
}

/// Orgs/repos named in runner configs that aren't being watched yet.
fn detected_scopes(settings: &Settings, probes: &HashMap<String, ProbeResult>) -> Vec<String> {
    let watched: HashSet<String> =
        settings.github.orgs.iter().chain(settings.github.repos.iter()).map(|s| s.to_ascii_lowercase()).collect();
    let mut found: Vec<String> = Vec::new();
    for p in probes.values().filter(|p| p.ok) {
        for r in &p.runners {
            if let Some(scope) = r.url.as_deref().and_then(scope_from_runner_url) {
                let owner = scope.split('/').next().unwrap_or_default().to_ascii_lowercase();
                let lower = scope.to_ascii_lowercase();
                if !watched.contains(&lower) && !watched.contains(&owner) && !found.iter().any(|f| f.eq_ignore_ascii_case(&scope)) {
                    found.push(scope);
                }
            }
        }
    }
    found.sort();
    found
}

fn attention(
    input: &Inputs,
    machines: &[Machine],
    runners: &[Runner],
    jobs: &Jobs,
    github: &GitHubInfo,
    unassigned: usize,
) -> Vec<Attention> {
    let mut out = Vec::new();
    let action = |label: &str, kind: &str, target: Option<String>| Some(Action { label: label.into(), kind: kind.into(), target });

    if let Some(err) = &input.inventory_info.error {
        out.push(Attention {
            id: "inventory-error".into(),
            level: "error".into(),
            title: "fleet.toml couldn't be read".into(),
            detail: format!("{err} Showing the last good inventory."),
            action: None,
        });
    }

    let no_token = github.token_source.is_empty() && !input.demo;
    if no_token {
        out.push(Attention {
            id: "github-token".into(),
            level: "warning".into(),
            title: "Connect GitHub".into(),
            detail: "Sign in with `gh auth login` (or set GITHUB_TOKEN) to see live runner status and queued jobs.".into(),
            action: action("Sign in with GitHub", "gh-sign-in", None),
        });
    } else if !github.configured {
        let who = github.detected.first().cloned().unwrap_or_else(|| "your runners".into());
        out.push(Attention {
            id: "github-setup".into(),
            level: "warning".into(),
            title: format!("Watch {who} on GitHub live"),
            detail: "Runner state now comes from the last check. Add the organization in Settings (it uses your gh login) for live status and jobs.".into(),
            action: action("Add organization", "settings", Some("github".into())),
        });
    }

    for s in github.scopes.iter().filter(|s| s.error.is_some()) {
        let mut detail = s.error.clone().unwrap_or_default();
        if let Some(h) = &s.hint {
            detail.push_str(&format!(" Run `{h}`."));
        }
        out.push(Attention {
            id: format!("scope-{}-{}", s.kind, s.name),
            level: "error".into(),
            title: format!("Can't read runners for {}", s.name),
            detail,
            // A `gh auth ...` fix runs with one click; anything else is in Settings.
            action: match s.hint.as_deref().and_then(|h| h.strip_prefix("gh ")).filter(|a| a.starts_with("auth ")) {
                Some(args) => action("Grant access", "gh-command", Some(args.to_string())),
                None => action("Settings", "settings", Some("github".into())),
            },
        });
    }

    let unreachable: Vec<&Machine> = machines.iter().filter(|m| m.status == "offline").collect();
    match unreachable.as_slice() {
        [] => {}
        [m] => out.push(Attention {
            id: "unreachable".into(),
            level: "warning".into(),
            title: format!("{} is unreachable", m.name),
            detail: m.probe.as_ref().and_then(|p| p.error.clone()).unwrap_or_default(),
            action: action("View machine", "machine", Some(m.id.clone())),
        }),
        many => out.push(Attention {
            id: "unreachable".into(),
            level: "warning".into(),
            title: format!("{} machines are unreachable", many.len()),
            detail: many.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "),
            action: action("Check again", "check", None),
        }),
    }

    // Runners that should be up but aren't, on machines we could reach (unreachable ones are covered above).
    let down: Vec<&Runner> = runners
        .iter()
        .filter(|r| r.state == "offline")
        .filter(|r| match &r.machine_id {
            Some(mid) => machines.iter().any(|m| &m.id == mid && m.status != "offline"),
            None => true,
        })
        .collect();
    match down.as_slice() {
        [] => {}
        [r] => out.push(Attention {
            id: "runners-offline".into(),
            level: "warning".into(),
            title: format!("{} is offline", r.name),
            detail: machines
                .iter()
                .flat_map(|m| &m.diagnostics)
                .find(|d| d.runner_id.as_deref() == Some(&r.id))
                .map(|d| d.message.clone())
                .unwrap_or_else(|| "It isn't connected to GitHub.".into()),
            action: action("View runner", "runner", Some(r.id.clone())),
        }),
        many => out.push(Attention {
            id: "runners-offline".into(),
            level: "warning".into(),
            title: format!("{} runners are offline", many.len()),
            detail: many.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", "),
            action: None,
        }),
    }

    let stuck: Vec<&Job> = jobs
        .queued
        .iter()
        .filter(|j| j.matching_online == 0 && (input.now - j.created_at).num_seconds() >= STUCK_JOB_SECS)
        .collect();
    if let Some(first) = stuck.first() {
        let waited = util::short_duration((input.now - first.created_at).num_seconds().max(0) as u64);
        let labels = first.labels.join(", ");
        let (title, detail) = if stuck.len() == 1 {
            (
                format!("“{}” can't find a runner", first.name),
                format!("Queued {waited} in {} — no online runner has the labels {labels}.", first.repo),
            )
        } else {
            (
                format!("{} jobs can't find a runner", stuck.len()),
                format!("The oldest, “{}” in {}, has waited {waited} for {labels}.", first.name, first.repo),
            )
        };
        out.push(Attention { id: "stuck-jobs".into(), level: "warning".into(), title, detail, action: action("View queue", "queue", None) });
    }

    for m in machines {
        if let Some(d) = m.diagnostics.iter().find(|d| d.code == "low-disk") {
            out.push(Attention {
                id: format!("low-disk-{}", m.id),
                level: "warning".into(),
                title: format!("{} is low on disk", m.name),
                detail: d.message.clone(),
                action: action("View machine", "machine", Some(m.id.clone())),
            });
        }
    }

    if unassigned > 0 {
        let names: Vec<&str> = runners.iter().filter(|r| r.machine_id.is_none()).map(|r| r.name.as_str()).take(5).collect();
        out.push(Attention {
            id: "unassigned".into(),
            level: "info".into(),
            title: if unassigned == 1 {
                format!("{} isn't in fleet.toml", names[0])
            } else {
                format!("{unassigned} GitHub runners aren't in fleet.toml")
            },
            detail: format!(
                "{} {} registered on GitHub but no machine lists {}. Add {} to a [[machine]]'s runners to track {}.",
                names.join(", "),
                if unassigned == 1 { "is" } else { "are" },
                if unassigned == 1 { "it" } else { "them" },
                if unassigned == 1 { "it" } else { "them" },
                if unassigned == 1 { "its host" } else { "their hosts" },
            ),
            action: None,
        });
    }

    if let Some(rate) = &github.rate_limit {
        if rate.limit > 0 && (rate.remaining as f64) < rate.limit as f64 * 0.1 {
            out.push(Attention {
                id: "rate-limit".into(),
                level: "warning".into(),
                title: "GitHub API rate limit is almost used up".into(),
                detail: format!(
                    "{} of {} requests left until {}. Raise the poll interval in Settings.",
                    rate.remaining,
                    rate.limit,
                    rate.reset_at.with_timezone(&chrono::Local).format("%H:%M")
                ),
                action: action("Settings", "settings", None),
            });
        }
    }

    for (i, w) in input.inventory_info.warnings.iter().enumerate() {
        out.push(Attention {
            id: format!("inventory-warning-{i}"),
            level: "info".into(),
            title: "fleet.toml".into(),
            detail: w.clone(),
            action: None,
        });
    }

    out.sort_by_key(|a| level_rank(&a.level));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{ScopeData, ScopeKind};

    fn inventory(toml: &str) -> Inventory {
        Inventory::parse(toml).unwrap()
    }

    fn probe_with(runners: Vec<ProbedRunner>) -> ProbeResult {
        ProbeResult {
            checked_at: Utc::now(),
            ok: true,
            os: Some("darwin".into()),
            arch: Some("arm64".into()),
            disk_total: Some(1_000_000_000_000),
            disk_free: Some(500_000_000_000),
            runners,
            ..Default::default()
        }
    }

    fn local(name: &str, listener: bool, worker: bool) -> ProbedRunner {
        ProbedRunner {
            dir: format!("/Users/ci/{name}"),
            name: name.into(),
            listener_pid: listener.then_some(10),
            worker_pid: worker.then_some(11),
            url: Some("https://github.com/acme".into()),
            ..Default::default()
        }
    }

    fn gh_runner(name: &str, status: &str, busy: bool, labels: &[&str]) -> RunnerData {
        RunnerData {
            id: 1,
            name: name.into(),
            os: "macOS".into(),
            status: status.into(),
            busy,
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn run(inv: &Inventory, settings: &Settings, probes: &HashMap<String, ProbeResult>, gh: Option<&GitHubData>) -> Snapshot {
        build(&Inputs {
            now: Utc::now(),
            demo: false,
            inventory: inv,
            inventory_info: InventoryInfo::default(),
            settings,
            probes,
            checking: &HashSet::new(),
            check: CheckInfo::default(),
            github: gh,
            github_syncing: false,
            token_source: "gh".into(),
        })
    }

    #[test]
    fn probe_only_states_and_missing_runners() {
        let inv = inventory(
            r#"
            [[machine]]
            name = "Studio"
            host = "studio"
            runners = ["s-01", "s-02", "s-03"]

            [[machine]]
            name = "NAS"
            host = "nas"
            "#,
        );
        let mut probes = HashMap::new();
        probes.insert("studio".to_string(), probe_with(vec![local("s-01", true, true), local("s-02", true, false), local("extra", false, false)]));
        let snap = run(&inv, &Settings::default(), &probes, None);

        let state = |id: &str| snap.runners.iter().find(|r| r.id == id).unwrap().state.clone();
        assert_eq!(state("studio/s-01"), "working");
        assert_eq!(state("studio/s-02"), "idle");
        assert_eq!(state("studio/s-03"), "offline"); // declared, not installed
        assert_eq!(state("studio/extra"), "offline"); // discovered, stopped
        assert_eq!(snap.machines[0].kind, "runner");
        assert_eq!(snap.machines[1].kind, "other");
        assert_eq!(snap.machines[1].status, "unchecked");
        let codes: Vec<&str> = snap.machines[0].diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&"runner-missing"));
        assert!(codes.contains(&"runner-stopped"));
        assert_eq!(snap.counts.working, 1);
        // Not watching GitHub yet, and the runner config names the org.
        assert_eq!(snap.github.detected, vec!["acme"]);
        assert!(snap.attention.iter().any(|a| a.id == "github-setup" && a.title.contains("acme")));
    }

    #[test]
    fn github_state_wins_and_jobs_are_matched() {
        let inv = inventory("[[machine]]\nname = \"Studio\"\nhost = \"studio\"\nrunners = [\"s-01\"]\n");
        let mut probes = HashMap::new();
        probes.insert("studio".to_string(), probe_with(vec![local("s-01", true, false)]));
        let mut settings = Settings::default();
        settings.github.orgs = vec!["acme".into()];
        let now = Utc::now();
        let job = |id: u64, status: &str, labels: &[&str], runner: Option<&str>| JobData {
            id,
            run_id: 1,
            name: format!("job-{id}"),
            workflow: "CI".into(),
            repo: "acme/api".into(),
            branch: None,
            event: None,
            actor: None,
            url: String::new(),
            status: status.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            runner_name: runner.map(str::to_string),
            created_at: now - chrono::Duration::minutes(10),
            started_at: None,
        };
        let gh = GitHubData {
            synced_at: Some(now),
            token_source: "gh".into(),
            scopes: vec![ScopeData {
                kind: ScopeKind::Org,
                name: "acme".into(),
                runners: vec![
                    gh_runner("s-01", "online", true, &["self-hosted", "macOS", "ARM64"]),
                    gh_runner("cloud-01", "offline", false, &["self-hosted", "linux"]),
                ],
                synced_at: Some(now),
                error: None,
                hint: None,
            }],
            jobs: vec![
                job(1, "in_progress", &["self-hosted", "macOS"], Some("s-01")),
                job(2, "queued", &["self-hosted", "macos"], None),
                job(3, "queued", &["self-hosted", "gpu"], None),
                job(4, "queued", &["ubuntu-latest"], None),
                job(5, "in_progress", &["ubuntu-latest"], Some("GitHub Actions 3")),
            ],
            ..Default::default()
        };
        let snap = run(&inv, &settings, &probes, Some(&gh));

        let s01 = snap.runners.iter().find(|r| r.name == "s-01").unwrap();
        assert_eq!(s01.state, "working");
        assert_eq!(s01.source, "github");
        assert_eq!(s01.job_id, Some(1));
        // GitHub runner with no machine.
        let cloud = snap.runners.iter().find(|r| r.name == "cloud-01").unwrap();
        assert!(cloud.machine_id.is_none());
        assert_eq!(cloud.id, "github/cloud-01");

        assert_eq!(snap.jobs.running.iter().map(|j| j.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(snap.jobs.queued.iter().map(|j| j.id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(snap.jobs.queued[0].matching, vec!["studio/s-01"]);
        assert_eq!(snap.jobs.queued[0].matching_online, 1);
        assert_eq!(snap.jobs.queued[1].matching_online, 0);
        assert!(snap.attention.iter().any(|a| a.id == "stuck-jobs"));
        assert!(snap.attention.iter().any(|a| a.id == "unassigned"));
        assert!(snap.github.live);
    }

    #[test]
    fn unreachable_machine_is_an_error_diagnostic() {
        let inv = inventory("[[machine]]\nname = \"Mini\"\nhost = \"mini\"\nrunners = [\"m-01\"]\n");
        let mut probes = HashMap::new();
        probes.insert(
            "mini".to_string(),
            ProbeResult { ok: false, error: Some("ssh: connect timed out".into()), ..Default::default() },
        );
        let snap = run(&inv, &Settings::default(), &probes, None);
        assert_eq!(snap.machines[0].status, "offline");
        assert_eq!(snap.machines[0].diagnostics[0].code, "unreachable");
        assert_eq!(snap.runners[0].state, "unknown");
        assert!(snap.attention.iter().any(|a| a.id == "unreachable"));
    }

    #[test]
    fn compares_versions() {
        assert!(version_lt("2.317.0", "2.321.0"));
        assert!(!version_lt("2.321.0", "2.321.0"));
        assert!(version_lt("2.99.0", "2.100.0"));
    }
}
