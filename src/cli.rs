//! Plain-text renderings of API responses for people at a terminal. Agents
//! should use `--json` (or the MCP server) instead; these are free to change.

use std::fmt::Write;

use crate::api::{
    ActionResponse, AddMachineResponse, ApiError, CommandOut, FileOut, FilesOut, JobOut, JobsResponse, MachineDetail,
    MachineSummary, RunnerOut, ScopesResponse, SettingsOut, StatusResponse, WorkspaceCreated, WorkspaceOut,
    WorkspacesResponse,
};
use crate::util;

pub fn status(s: &StatusResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", s.summary);
    let g = &s.github;
    let gh = if g.live {
        let names: Vec<&str> = g.watching.iter().map(|w| w.name.as_str()).collect();
        format!("live from {}", names.join(", "))
    } else if g.configured {
        "configured, not reachable".to_string()
    } else {
        "not watching any org".to_string()
    };
    let _ = writeln!(out, "GitHub: {gh}{}", g.login.as_ref().map(|l| format!(" (as {l})")).unwrap_or_default());
    if let Some(h) = &g.hint {
        let _ = writeln!(out, "  hint: {h}");
    }
    out.push('\n');
    out.push_str(&machines(&s.machines));
    if !s.attention.is_empty() {
        let _ = writeln!(out, "\nNeeds attention:");
        for a in &s.attention {
            let mark = match a.level.as_str() {
                "error" => "✖",
                "warning" => "!",
                _ => "·",
            };
            let _ = writeln!(out, "  {mark} {}", a.title);
            if !a.detail.is_empty() {
                let _ = writeln!(out, "    {}", a.detail);
            }
        }
    }
    if let Some(h) = &s.hint {
        let _ = writeln!(out, "\n{h}");
    }
    out
}

pub fn machines(list: &[MachineSummary]) -> String {
    if list.is_empty() {
        return "No machines. Add one with `muxin add-machine --name <name> --host <ssh-host>`.\n".into();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|m| {
            vec![
                m.id.clone(),
                m.status.clone(),
                dash(&m.platform),
                m.runners.clone(),
                m.cpu_pct.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into()),
                m.mem_pct.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into()),
                m.disk_free.clone().unwrap_or_else(|| "—".into()),
                m.uptime.clone().unwrap_or_else(|| "—".into()),
                match (m.problems, m.worst.as_deref()) {
                    (0, Some("unknown")) => "unknown".into(),
                    (0, _) => "ok".into(),
                    (n, _) => format!("{n} problem{}", if n == 1 { "" } else { "s" }),
                },
            ]
        })
        .collect();
    table(&["MACHINE", "STATUS", "PLATFORM", "RUNNERS", "CPU", "MEM", "DISK FREE", "UPTIME", "HEALTH"], &rows)
}

pub fn machine(m: &MachineDetail) -> String {
    let s = &m.summary;
    let mut out = String::new();
    let _ = writeln!(out, "{} ({})  {}", s.name, s.id, s.status);
    let _ = writeln!(
        out,
        "{}  ·  {}{}",
        if s.host.is_empty() { "no host" } else { &s.host },
        dash(&s.platform),
        m.os_version.as_ref().map(|v| format!("  ·  {v}")).unwrap_or_default()
    );
    match (&s.unreachable, &s.error) {
        (Some(u), _) => {
            let _ = writeln!(out, "\n{}. {}", u.title, u.detail);
            if let Some(t) = s.last_seen_at {
                let _ = writeln!(out, "Last reached {}.", ago(t));
            }
            if let Some(h) = &u.hint {
                let _ = writeln!(out, "Try: {h}");
            }
            let _ = writeln!(out, "ssh said: {}", u.error);
        }
        (None, Some(e)) => {
            let _ = writeln!(out, "error: {e}");
        }
        _ => {}
    }
    if s.status == "online" {
        let _ = writeln!(
            out,
            "cpu {}  mem {}  disk free {}  up {}",
            s.cpu_pct.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into()),
            s.mem_pct.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into()),
            s.disk_free.clone().unwrap_or_else(|| "—".into()),
            s.uptime.clone().unwrap_or_else(|| "—".into())
        );
    }
    out.push('\n');
    out.push_str(&runners(&m.runners));
    // Being unreachable is explained above.
    let diags: Vec<_> = m.diagnostics.iter().filter(|d| d.code != "unreachable" || s.unreachable.is_none()).collect();
    if !diags.is_empty() {
        let _ = writeln!(out, "\nDiagnostics:");
        for d in diags {
            let _ = writeln!(out, "  [{}] {}", d.level, d.message);
            if let Some(h) = &d.hint {
                let _ = writeln!(out, "      {h}");
            }
        }
    }
    out
}

pub fn added_machine(r: &AddMachineResponse) -> String {
    let mut out = format!("Added {}", r.machine_id);
    if let Some(m) = &r.machine {
        let _ = write!(out, ": {}  {}  {}", m.summary.status, dash(&m.summary.platform), m.summary.runners);
        match (&m.summary.unreachable, &m.summary.error) {
            (Some(u), _) => {
                let _ = write!(out, "\n  {}. {}", u.title, u.detail);
                if let Some(h) = &u.hint {
                    let _ = write!(out, "\n  Try: {h}");
                }
            }
            (None, Some(e)) => {
                let _ = write!(out, "\n  {e}");
            }
            _ => {}
        }
    }
    let _ = writeln!(out, "\n{}", r.next);
    out
}

pub fn runners(list: &[RunnerOut]) -> String {
    if list.is_empty() {
        return "No runners.\n".into();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|r| {
            vec![
                r.id.clone(),
                r.state.clone(),
                match r.source.as_str() {
                    "github" => "live".into(),
                    "probe" => "last check".into(),
                    _ => "—".into(),
                },
                r.version.clone().unwrap_or_else(|| "—".into()),
                r.service_state.clone().unwrap_or_else(|| "—".into()),
                r.job.as_ref().map(|j| format!("{} ({})", j.name, j.repo)).unwrap_or_default(),
            ]
        })
        .collect();
    table(&["RUNNER", "STATE", "SOURCE", "VERSION", "SERVICE", "JOB"], &rows)
}

pub fn jobs(j: &JobsResponse) -> String {
    let mut out = format!("{}\n", j.summary);
    let line = |out: &mut String, job: &JobOut, running: bool| {
        let age = util::short_duration(job.age_secs.max(0) as u64);
        let _ = writeln!(
            out,
            "  {} {}  {} · {}  [{}]  {}{}",
            if running { "▶" } else { "…" },
            job.name,
            job.repo,
            job.workflow,
            job.labels.join(", "),
            if running { format!("running {age}") } else { format!("waiting {age}") },
            match (&job.runner_id, running) {
                (Some(r), true) => format!(" on {r}"),
                (_, false) if job.matching_online == 0 => " — no online runner matches".into(),
                _ => String::new(),
            }
        );
    };
    for job in &j.running {
        line(&mut out, job, true);
    }
    for job in &j.queued {
        line(&mut out, job, false);
    }
    if let Some(h) = &j.hint {
        let _ = writeln!(out, "{h}");
    }
    out
}

pub fn cloud_usage(u: &crate::cloud::usage::CloudUsage) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Now ${:.3}/h · this month ${:.2} · by the month's end ${:.2} if kept", u.hourly_now, u.this_month, u.month_if_kept);
    match (&u.account, &u.account_error) {
        (Some(b), _) => {
            let _ = writeln!(out, "DigitalOcean bill this month: ${:.2} (balance ${:.2})", b.month_to_date_usage, b.month_to_date_balance);
        }
        (None, Some(e)) => {
            let _ = writeln!(out, "DigitalOcean bill: {e}");
        }
        _ => {}
    }
    if !u.machines.is_empty() {
        let _ = writeln!(out, "\n{:<20} {:<10} {:<6} {:<16} {:>9} {:>7} {:>11} {:>10}", "MACHINE", "POWER", "REGION", "SIZE", "RATE", "CAP", "THIS MONTH", "SINCE NEW");
        for m in &u.machines {
            let _ = writeln!(
                out,
                "{:<20} {:<10} {:<6} {:<16} {:>9} {:>7} {:>11} {:>10}",
                m.name,
                m.power.as_deref().unwrap_or("?"),
                m.region,
                m.size,
                format!("${:.3}/h", m.price_hourly),
                format!("${:.0}/mo", m.price_monthly),
                format!("${:.2}", m.this_month),
                format!("${:.2}", m.since_created)
            );
        }
    }
    let _ = writeln!(out, "\n{}", u.billing);
    out
}

pub fn schedules(list: &Vec<crate::fleet::schedules::MachineSchedules>) -> String {
    let mut out = String::new();
    for m in list {
        if m.schedules.is_empty() && m.error.is_none() && m.problem.is_none() {
            continue;
        }
        out.push_str(&format!("{}\n", m.machine));
        if let Some(e) = &m.error {
            out.push_str(&format!("  (couldn't ask it: {e})\n"));
        }
        if let Some(p) = &m.problem {
            out.push_str(&format!("  ! {p}\n"));
        }
        for s in &m.schedules {
            let what = match s.what.kind.as_str() {
                "agent" => format!("{}: {}", s.what.agent.clone().unwrap_or_default(), s.what.prompt.clone().unwrap_or_default()),
                _ => s.what.command.clone().unwrap_or_default(),
            };
            let last = match &s.last_run {
                _ if s.running => "running now".to_string(),
                Some(r) if r.ok => format!("last ok {}", ago_secs(r.started)),
                Some(r) => format!("last failed {}{}", ago_secs(r.started), r.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default()),
                None => "not run yet".to_string(),
            };
            let off = if s.enabled { "" } else { " (off)" };
            out.push_str(&format!("  {:<10} {:<22} {:<24} {last}{off}\n    {what}\n", s.id, s.name, s.when_text));
        }
    }
    if out.is_empty() {
        out.push_str("No schedules. Add one with `muxin schedule add <machine> --name ... --every day --at 09:00 --command ...`.\n");
    }
    out
}

pub fn schedule_log(l: &crate::fleet::schedules::ScheduleLog) -> String {
    let mut out = String::new();
    for r in &l.runs {
        let how = match (&r.session, r.ok) {
            (Some(s), _) => format!("started session {s}"),
            (None, true) => "ok".into(),
            (None, false) => r.error.clone().unwrap_or_else(|| format!("failed{}", r.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default())),
        };
        out.push_str(&format!("{}  {how}{}\n", ago_secs(r.started), r.seconds.map(|s| format!(" in {s}s")).unwrap_or_default()));
    }
    if !l.output.is_empty() {
        out.push_str("\n── newest run ──\n");
        out.push_str(&l.output);
        if !l.output.ends_with('\n') {
            out.push('\n');
        }
    }
    if out.is_empty() {
        out.push_str("Not run yet.\n");
    }
    out
}

/// "5m ago" from Unix seconds.
fn ago_secs(t: i64) -> String {
    let secs = (chrono::Utc::now().timestamp() - t).max(0) as u64;
    format!("{} ago", crate::util::short_duration(secs))
}

pub fn vm_options(o: &crate::ops::vm::VmOptions) -> String {
    let mut out = String::from("Machines:\n");
    for h in &o.hosts {
        let can = if h.systems.is_empty() { h.reason.clone().unwrap_or_else(|| "can't run VMs".into()) } else if h.systems.iter().any(|s| s.starts_with("windows")) { "Windows and Linux".into() } else { "Linux".into() };
        out.push_str(&format!("  {:<20} {can}\n", h.machine_id));
    }
    out.push_str("Systems:\n");
    for s in &o.systems {
        out.push_str(&format!("  {:<22} {}\n", s.id, s.name));
    }
    out.push_str(&format!("Defaults: {} CPUs, {} GB memory, {} GB disk (Windows)\n", o.default_cpus, o.default_memory_gb, o.default_disk_gb));
    out
}

pub fn cloud_options(o: &crate::cloud::CloudOptions) -> String {
    let mut out = String::new();
    let method = match o.method.as_str() {
        "oauth" => "browser sign-in",
        "doctl" => "doctl",
        _ => "API token",
    };
    match (&o.account, o.signed_in) {
        (Some(a), _) => out.push_str(&format!("DigitalOcean: {} ({method})\n", a.email)),
        (None, true) => out.push_str(&format!("DigitalOcean: signed in ({method})\n")),
        (None, false) => out.push_str("DigitalOcean: not signed in. Run `muxin cloud login --token -` with the token on stdin (or --doctl, --browser).\n"),
    }
    if let Some(e) = &o.error {
        out.push_str(&format!("  {e}\n"));
    }
    if !o.regions.is_empty() {
        out.push_str(&format!("Regions: {} (default {})\n", o.regions.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>().join(", "), o.default_region));
    }
    if !o.sizes.is_empty() {
        out.push_str("Sizes:\n");
        for s in &o.sizes {
            let mark = if s.slug == o.default_size { "*" } else { " " };
            out.push_str(&format!(
                " {mark} {:<18} {:>2} vCPU  {:>3} GB  {:>4} GB disk  ${:.3}/h  ${:.0}/mo\n",
                s.slug,
                s.vcpus,
                s.memory_mb / 1024,
                s.disk_gb,
                s.price_hourly,
                s.price_monthly
            ));
        }
    }
    out
}

pub fn action(r: &ActionResponse) -> String {
    let o = &r.operation;
    let mut out = String::new();
    match o.status.as_str() {
        "succeeded" => {
            let _ = writeln!(out, "✓ {} (operation {})", o.title, o.id);
        }
        "running" => {
            let _ = writeln!(out, "… {} is still running (operation {})", o.title, o.id);
        }
        _ => {
            let _ = writeln!(out, "✖ {} failed: {}", o.title, o.error.clone().unwrap_or_default());
        }
    }
    // The log already streamed to stderr while we waited; logs are the exception people want on stdout.
    if o.kind == "logs" {
        for l in &o.log_tail {
            let _ = writeln!(out, "{l}");
        }
    }
    for runner in &r.runners {
        let _ = writeln!(out, "  {}  {}", runner.id, runner.state);
    }
    if let Some(n) = &r.next {
        let _ = writeln!(out, "{n}");
    }
    out
}

pub fn scopes(r: &ScopesResponse) -> String {
    let mut out = String::new();
    for res in &r.results {
        let _ = write!(out, "{}: {}", res.scope, res.result.replace('_', " "));
        if let Some(n) = res.runners {
            let _ = write!(out, " ({n} runner{})", if n == 1 { "" } else { "s" });
        }
        if let Some(e) = &res.error {
            let _ = write!(out, " — {e}");
        }
        out.push('\n');
        if let Some(h) = &res.hint {
            let _ = writeln!(out, "  hint: {h}");
        }
    }
    let watching: Vec<String> = r.watching.iter().map(|w| format!("{} ({})", w.name, w.runners)).collect();
    let _ = writeln!(out, "Watching: {}", if watching.is_empty() { "nothing".into() } else { watching.join(", ") });
    out
}

pub fn settings(s: &SettingsOut) -> String {
    format!(
        "orgs                    {}\nrepos                   {}\ngithub poll             {}s\nscan repos per org      {}\ncheck interval          {}\napi url                 {}\nconfig dir              {}\ninventory               {}\n",
        dash(&s.github_orgs.join(", ")),
        dash(&s.github_repos.join(", ")),
        s.github_poll_seconds,
        s.scan_repos,
        if s.check_interval_minutes == 0 { "off".to_string() } else { format!("{}m", s.check_interval_minutes) },
        s.api_url,
        s.config_dir,
        s.inventory
    )
}

pub fn workspaces(r: &WorkspacesResponse) -> String {
    let mut out = format!("{}\n", r.summary);
    if r.workspaces.is_empty() {
        out.push_str("Create one with `muxin workspace create [--machine <id>] [--repo owner/repo]`.\n");
        return out;
    }
    out.push('\n');
    let rows: Vec<Vec<String>> = r
        .workspaces
        .iter()
        .map(|w| {
            vec![
                w.id.clone(),
                w.name.clone(),
                w.status.clone(),
                w.machine_id.clone(),
                w.repo.clone().map(|r| match w.branch.as_ref().or(w.git_ref.as_ref()) {
                    Some(at) => format!("{r}@{at}"),
                    None => r,
                }).unwrap_or_else(|| "—".into()),
                w.git.as_ref().map(|g| g.short()).unwrap_or_else(|| "—".into()),
                w.owner.clone().unwrap_or_else(|| "—".into()),
                format!("{}", w.commands),
                ago(w.last_used_at),
            ]
        })
        .collect();
    out.push_str(&table(&["WORKSPACE", "NAME", "STATUS", "MACHINE", "REPO", "GIT", "OWNER", "CMDS", "USED"], &rows));
    out
}

pub fn workspace_created(r: &WorkspaceCreated) -> String {
    let w = &r.workspace;
    format!(
        "✓ {} ({}) on {} · {}\n  shell: {}\nRun commands with `muxin workspace exec {} -- <command>`; destroy it with `muxin workspace rm {}`.\n",
        w.id, w.name, w.machine_name, w.path, w.shell, w.id, w.id
    )
}

pub fn workspace(w: &WorkspaceOut) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} ({})  {}", w.name, w.id, w.status);
    let _ = writeln!(out, "{} ({})  ·  {}  ·  {}", w.machine_name, w.machine_id, dash(&w.platform), w.path);
    if let Some(repo) = &w.repo {
        let at = w.branch.as_ref().or(w.git_ref.as_ref()).map(|r| format!(" @ {r}")).unwrap_or_default();
        let _ = writeln!(out, "repo {repo}{at}");
    }
    let _ = writeln!(
        out,
        "owner {}  ·  created {}  ·  used {}  ·  {} command{}",
        w.owner.as_deref().unwrap_or("—"),
        ago(w.created_at),
        ago(w.last_used_at),
        w.commands,
        if w.commands == 1 { "" } else { "s" }
    );
    if let Some(e) = &w.error {
        let _ = writeln!(out, "error: {e}");
    }
    let _ = writeln!(out, "shell: {}", w.shell);
    if !w.history.is_empty() {
        let _ = writeln!(out, "\nRecent commands:");
        for c in &w.history {
            let _ = writeln!(out, "  {} {}", exit_mark(c), c.command);
        }
    }
    out
}

fn exit_mark(c: &CommandOut) -> String {
    match (c.exit_code, c.timed_out, c.background) {
        (_, true, _) => "[timeout]".into(),
        (_, _, true) => "[bg]".into(),
        (Some(code), _, _) => format!("[{code}]"),
        (None, _, _) => "[…]".into(),
    }
}

pub fn file(f: &FileOut) -> String {
    if f.binary {
        return format!("{} is a binary file ({} bytes); not shown.\n", f.path, f.size);
    }
    let mut out = f.content.clone();
    if f.truncated {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("[truncated: {} of {} bytes shown]\n", f.content.len(), f.size));
    }
    out
}

pub fn files(f: &FilesOut) -> String {
    if f.entries.is_empty() {
        return format!("{} is empty.\n", f.path);
    }
    let rows: Vec<Vec<String>> = f
        .entries
        .iter()
        .map(|e| vec![if e.kind == "dir" { format!("{}/", e.path) } else { e.path.clone() }, if e.kind == "file" { util::bytes(e.size) } else { e.kind.clone() }])
        .collect();
    table(&["PATH", "SIZE"], &rows)
}

fn ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - t).num_seconds().max(0) as u64;
    if secs < 10 { "just now".into() } else { format!("{} ago", util::short_duration(secs)) }
}

pub fn error(e: &ApiError) -> String {
    let mut out = format!("error: {}\n", e.message);
    if let Some(h) = &e.hint {
        let _ = writeln!(out, "hint: {h}");
    }
    out
}

fn dash(s: &str) -> String {
    if s.is_empty() { "—".into() } else { s.to_string() }
}

fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let fmt = |cells: Vec<String>| -> String {
        let line: Vec<String> = cells.iter().enumerate().map(|(i, c)| format!("{c:<w$}", w = widths[i])).collect();
        format!("{}\n", line.join("  ").trim_end())
    };
    let mut out = fmt(headers.iter().map(|h| h.to_string()).collect());
    for r in rows {
        out.push_str(&fmt(r.clone()));
    }
    out
}

pub fn sessions(r: &crate::api::SessionsOut) -> String {
    let mut out = String::new();
    if r.sessions.is_empty() {
        out.push_str("No agent sessions. Start one with `muxin session start <machine> claude [--cwd ~/code/app]`.\n");
    } else {
        let rows: Vec<Vec<String>> = r
            .sessions
            .iter()
            .map(|s| {
                let state = match (s.status.as_str(), s.busy) {
                    ("exited", _) => format!("exited ({})", s.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())),
                    (_, true) => "working".into(),
                    _ => "idle".into(),
                };
                vec![s.id.clone(), s.agent_name.clone(), state, s.cwd.clone(), s.git.as_ref().map(|g| g.short()).unwrap_or_else(|| "—".into()), s.created_at.map(ago).unwrap_or_else(|| "—".into())]
            })
            .collect();
        out.push_str(&table(&["SESSION", "AGENT", "STATE", "FOLDER", "GIT", "STARTED"], &rows));
    }
    out.push('\n');
    for m in &r.machines {
        let detail = match (&m.error, &m.agent) {
            (Some(e), _) => e.clone(),
            (None, None) => "not set up yet".into(),
            (None, Some(_)) if m.agents.is_empty() => "no agent CLIs found".into(),
            (None, Some(_)) => m.agents.join(", "),
        };
        out.push_str(&format!("{}: {detail}\n", m.machine_id));
    }
    out
}

pub fn boxes(r: &crate::api::BoxesOut) -> String {
    let mut out = String::new();
    if r.boxes.is_empty() {
        out.push_str("No boxes. Hand a task to an agent with `muxin box new <owner/repo> \"<what to do>\"`.\n");
    } else {
        let rows: Vec<Vec<String>> = r
            .boxes
            .iter()
            .map(|b| {
                let state = match (b.state.as_str(), b.busy, b.attention.as_deref()) {
                    ("running", _, Some("waiting")) => "waiting".to_string(),
                    ("running", true, _) => "working".into(),
                    ("running", false, _) => "idle".into(),
                    (other, _, _) => other.to_string(),
                };
                let git = b.git.as_ref().map(|g| g.short()).unwrap_or_else(|| b.branch.clone());
                vec![b.id.clone(), b.name.clone(), b.agent_name.clone(), state, b.machine_id.clone(), b.repo.clone(), git, b.created_at.map(ago).unwrap_or_else(|| "—".into())]
            })
            .collect();
        out.push_str(&table(&["BOX", "NAME", "AGENT", "STATE", "MACHINE", "REPO", "GIT", "MADE"], &rows));
    }
    for e in &r.errors {
        out.push_str(&format!("{}: {}\n", e.machine_id, e.error.clone().unwrap_or_default()));
    }
    out
}

pub fn box_one(b: &crate::fleet::boxes::BoxInfo) -> String {
    let state = match (b.state.as_str(), b.busy) {
        ("running", true) => "working",
        (s, _) => s,
    };
    format!("{} ({}) on {}: {state}, branch {}\n", b.id, b.name, b.machine_id, b.branch)
}

pub fn box_created(r: &crate::api::BoxCreatedOut) -> String {
    let o = &r.operation;
    let mut out = match o.status.as_str() {
        "succeeded" => format!("✓ {} is working on it\n", r.box_id),
        "running" => format!("… {} is still being made (operation {})\n", r.box_id, o.id),
        _ => format!("✖ {} failed: {}\n", o.title, o.error.clone().unwrap_or_default()),
    };
    if let Some(n) = &r.next {
        out.push_str(&format!("{n}\n"));
    }
    out
}

pub fn session_started(s: &crate::fleet::sessions::SessionInfo) -> String {
    format!(
        "Started {} ({}) in {}.\nWatch it in the Muxin app, or attach here: {}\n",
        s.id, s.agent_name, s.cwd, s.attach_command
    )
}

pub fn handed_off(h: &crate::fleet::sessions::handoff::HandedOff) -> String {
    let from = match h.source.as_str() {
        "transcript" => "the conversation so far",
        "terminal" => "what its terminal showed (no transcript Muxin can read)",
        _ => "the git state only (no conversation found)",
    };
    let mut out = format!("Handed off to {} ({}) in {}.\nThe note, from {from}: {}\n", s_id(h), h.session.agent_name, h.session.cwd, h.note);
    for w in &h.warnings {
        out.push_str(&format!("! {w}\n"));
    }
    if let Some(ws) = &h.workspace {
        out.push_str(&format!("The work moved to workspace {ws} there; the folder it left is as it was.\n"));
    }
    if let Some(k) = &h.kept {
        out.push_str(&format!("{k} is still running.\n"));
    }
    out.push_str(&format!("Watch it in the Muxin app, or attach here: {}\n", h.session.attach_command));
    out
}

fn s_id(h: &crate::fleet::sessions::handoff::HandedOff) -> String {
    match &h.box_id {
        Some(b) => format!("{} in {b}", h.session.id),
        None => h.session.id.clone(),
    }
}

fn item_rows(items: &[crate::github::hub::Item]) -> Vec<Vec<String>> {
    items
        .iter()
        .map(|i| {
            let kind = if i.pull_request { if i.draft { "draft PR" } else { "PR" } } else { "issue" };
            vec![format!("{}#{}", i.repo, i.number), kind.into(), i.title.clone(), ago(i.updated_at)]
        })
        .collect()
}

pub fn github_inbox(r: &crate::github::hub::Inbox) -> String {
    let mut out = format!("Signed in as {}\n", r.viewer.login);
    for (title, items) in [("Waiting for your review", &r.review_requests), ("Assigned to you", &r.assigned), ("Your pull requests", &r.authored)] {
        out.push_str(&format!("\n{title} ({})\n", items.len()));
        if !items.is_empty() {
            out.push_str(&table(&["ITEM", "KIND", "TITLE", "UPDATED"], &item_rows(items)));
        }
    }
    out
}

pub fn github_repos(r: &[crate::github::hub::Repo]) -> String {
    let rows: Vec<Vec<String>> = r
        .iter()
        .map(|x| {
            vec![
                if x.favorite { format!("★ {}", x.full_name) } else { x.full_name.clone() },
                if x.private { "private".into() } else { "public".into() },
                x.open_issues.to_string(),
                x.pushed_at.map(ago).unwrap_or_else(|| "—".into()),
            ]
        })
        .collect();
    table(&["REPOSITORY", "VISIBILITY", "OPEN", "PUSHED"], &rows)
}

pub fn github_accounts(a: &crate::api::AccountsOut) -> String {
    let mut out = match (&a.using, a.token_source.as_str()) {
        (Some(l), "gh") => match a.machines.iter().find(|m| Some(&m.machine_id) == a.machine_id.as_ref()) {
            Some(m) => format!("Muxin uses {l} (the GitHub CLI login on {}).\n", m.machine),
            None => format!("Muxin uses {l}.\n"),
        },
        (Some(l), var) => format!("Muxin uses {l} (the token in ${var}).\n"),
        (None, _) => "Not signed in to GitHub. Run `gh auth login`.\n".into(),
    };
    if !a.accounts.is_empty() {
        let rows: Vec<Vec<String>> = a
            .accounts
            .iter()
            .map(|x| {
                let using = a.machine_id.is_none() && a.using.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(&x.login));
                vec![
                    format!("{} {}", if using { "*" } else { " " }, x.login),
                    if x.gh_active { "yes".into() } else { "".into() },
                    if x.ok { "ok".into() } else { "needs `gh auth login`".into() },
                ]
            })
            .collect();
        out.push('\n');
        out.push_str(&table(&["ACCOUNT", "GH ACTIVE", "LOGIN"], &rows));
    }
    if !a.machines.is_empty() {
        let rows: Vec<Vec<String>> = a
            .machines
            .iter()
            .map(|m| {
                let using = a.machine_id.as_deref() == Some(m.machine_id.as_str());
                vec![format!("{} {}", if using { "*" } else { " " }, m.login), m.machine.clone(), m.machine_id.clone()]
            })
            .collect();
        out.push_str("\nOn your machines\n");
        out.push_str(&table(&["ACCOUNT", "MACHINE", "ID"], &rows));
    }
    if !a.accounts.is_empty() || !a.machines.is_empty() {
        out.push_str("\nSwitch with `muxin github use <login>` or `--machine <id>`; add an account with `gh auth login`.\n");
    }
    out
}

pub fn github_favorites(f: &crate::api::FavoritesOut) -> String {
    if f.favorites.is_empty() {
        return "No favorite repositories.\n".into();
    }
    format!("Favorites:\n{}", f.favorites.iter().map(|r| format!("  ★ {r}\n")).collect::<String>())
}

pub fn github_repo(r: &crate::api::RepoOut) -> String {
    let mut out = format!("{} · {}\n", r.repo.full_name, r.repo.description.clone().unwrap_or_default());
    out.push_str(&format!("\nIssues ({})\n", r.issues.len()));
    out.push_str(&table(&["ITEM", "KIND", "TITLE", "UPDATED"], &item_rows(&r.issues)));
    out.push_str(&format!("\nPull requests ({})\n", r.pulls.len()));
    out.push_str(&table(&["ITEM", "KIND", "TITLE", "UPDATED"], &item_rows(&r.pulls)));
    if !r.checkouts.is_empty() {
        out.push_str("\nOn your machines\n");
        out.push_str(&checkouts(&r.checkouts));
    }
    out
}

pub fn github_item(d: &crate::github::hub::Detail) -> String {
    let i = &d.item;
    let mut out = format!("{}#{} {}\n{} · {} · by {}\n{}\n\n{}\n", i.repo, i.number, i.title, if i.pull_request { "pull request" } else { "issue" }, i.state, i.author, i.html_url, d.body.trim());
    if let Some(p) = &d.pull {
        out.push_str(&format!(
            "\n{} → {} · +{} −{} in {} files · checks: {} passed, {} failed, {} pending\n",
            p.head, p.base, p.additions, p.deletions, p.changed_files, p.checks.passed, p.checks.failed, p.checks.pending
        ));
    }
    for c in &d.comments {
        out.push_str(&format!("\n— {} ({}):\n{}\n", c.author, ago(c.created_at), c.body.trim()));
    }
    out
}

pub fn ipads(o: &crate::api::IpadsOut) -> String {
    if o.ipads.is_empty() {
        return "No iPads are paired. Pair one with `muxin ipad pair`.\n".into();
    }
    o.ipads.iter().map(|i| format!("{}  paired {}  with {}\n", i.name, ago(i.paired_at), i.machines.join(", "))).collect()
}

pub fn ipad_removed(o: &crate::api::IpadRemovedOut) -> String {
    let mut out = format!("Unpaired {}.\n", o.ipad.name);
    if !o.kept.is_empty() {
        out.push_str(&format!("{} couldn't be reached and still trust it; unpair it again once they're on.\n", o.kept.join(", ")));
    }
    out
}

pub fn connection(c: &crate::fleet::connection::Connection) -> String {
    let how = match c.via.as_str() {
        "direct" => "directly (QUIC)".to_string(),
        "ssh" => "over a live ssh link".to_string(),
        _ => "not connected right now".to_string(),
    };
    let mut out = format!("{}: {how}", c.machine_id);
    if let Some(n) = &c.network {
        out.push_str(&format!(", {n}"));
    }
    if let Some(ms) = c.latency_ms {
        out.push_str(&format!(", {ms} ms round trip"));
    }
    out.push('\n');
    if let Some(a) = &c.address {
        out.push_str(&format!("  direct address  {a}\n"));
    }
    if let Some(s) = &c.service {
        out.push_str(&format!("  listener runs   as a {s} service\n"));
    }
    if let Some(n) = &c.note {
        out.push_str(&format!("  note            {n}\n"));
    }
    if !c.direct {
        out.push_str(&format!("\nConnect directly with `muxin connection {} --direct on`.\n", c.machine_id));
    }
    out
}

pub fn git_setup(g: &crate::api::GitSetupOut) -> String {
    let Some(version) = &g.setup.git else {
        return format!("git isn't installed on {}.\n", g.machine_id);
    };
    let identity = match (&g.setup.name, &g.setup.email) {
        (Some(n), Some(e)) => format!("{n} <{e}>"),
        _ => "not set: commits will fail".into(),
    };
    let push = match g.push.as_str() {
        "gh" => format!("through the GitHub CLI (@{})", g.gh_account.clone().unwrap_or_default()),
        "other" => format!("through {} (may or may not have a GitHub login)", g.setup.helpers.iter().chain(g.setup.ssh_keys.iter()).cloned().collect::<Vec<_>>().join(", ")),
        _ => "no GitHub login: pushes will fail".into(),
    };
    let mut out = format!("git {version} on {}\n  commits as  {identity}\n  pushes      {push}\n", g.machine_id);
    let mut fixes = Vec::new();
    if !g.can_commit {
        let who = match (&g.suggested_name, &g.suggested_email) {
            (Some(n), Some(e)) => format!(" (sets {n} <{e}>)"),
            _ => String::new(),
        };
        fixes.push(format!("muxin git setup {} --identity{who}", g.machine_id));
    }
    if g.push != "gh" {
        fixes.push(match &g.gh_account {
            Some(_) => format!("muxin git setup {} --credentials", g.machine_id),
            None => format!("sign in to gh on {} (gh auth login), then: muxin git setup {} --credentials", g.machine_id, g.machine_id),
        });
    }
    if !fixes.is_empty() {
        out.push_str(&format!("\nTo fix:\n{}", fixes.iter().map(|f| format!("  {f}\n")).collect::<String>()));
    }
    out
}

pub fn checkouts(r: &[crate::fleet::model::Checkout]) -> String {
    let rows: Vec<Vec<String>> = r
        .iter()
        .map(|c| {
            let sync = match (c.ahead, c.behind) {
                (0, 0) => "up to date".to_string(),
                (a, 0) => format!("{a} ahead"),
                (0, b) => format!("{b} behind"),
                (a, b) => format!("{a} ahead, {b} behind"),
            };
            vec![
                c.machine_id.clone(),
                c.path.clone(),
                c.repo.clone().unwrap_or_else(|| "—".into()),
                c.branch.clone(),
                if c.upstream.is_some() { sync } else { "no upstream".into() },
                if c.dirty == 0 { "clean".into() } else { format!("{} changed", c.dirty) },
            ]
        })
        .collect();
    table(&["MACHINE", "FOLDER", "REPOSITORY", "BRANCH", "SYNC", "CHANGES"], &rows)
}

pub fn listing(l: &crate::fleet::files::Listing) -> String {
    let mut out = String::new();
    let shown = match l.path.strip_prefix(&l.home) {
        Some(rest) if !l.home.is_empty() => format!("~{rest}"),
        _ => l.path.clone(),
    };
    let _ = writeln!(out, "{}:{shown}", l.machine_id);
    if l.entries.is_empty() {
        out.push_str("(empty)\n");
    }
    let rows: Vec<Vec<String>> = l
        .entries
        .iter()
        .map(|e| {
            let name = if e.folder { format!("{}/", e.name) } else if e.kind == "link" { format!("{}@", e.name) } else { e.name.clone() };
            let modified = e.modified.and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
            vec![name, e.size.map(util::bytes).unwrap_or_default(), modified]
        })
        .collect();
    if !rows.is_empty() {
        out.push_str(&table(&["NAME", "SIZE", "MODIFIED"], &rows));
    }
    if l.more {
        out.push_str("(more not shown)\n");
    }
    out
}

pub fn copied(r: &crate::api::CopyFilesResponse) -> String {
    let mut out = String::new();
    for o in &r.operations {
        let _ = writeln!(out, "{}: {}", o.title, o.status);
    }
    if let Some(n) = &r.next {
        let _ = writeln!(out, "{n}");
    }
    out
}

/// A machine's logs, as the flags that open each.
pub fn log_sources(s: &crate::fleet::logs::LogSources) -> String {
    use crate::fleet::logs::LogSource;
    let mut out = String::new();
    let mut section = |title: &str, list: &[LogSource], note: &Option<String>, flag: &dyn Fn(&LogSource) -> String| {
        if list.is_empty() && note.is_none() {
            return;
        }
        out.push_str(&format!("{title}\n"));
        if let Some(n) = note {
            out.push_str(&format!("  ({n})\n"));
        }
        for x in list {
            let mut detail = x.detail.clone();
            if x.readable == Some(false) {
                detail = format!("{detail} (root only)");
            }
            out.push_str(&format!("  {:<44} {detail}\n", flag(x)));
        }
    };
    section("System", &s.system, &s.system_note, &|x| if x.kind == "kernel" { "--kernel".into() } else { "(default)".into() });
    section("Files", &s.files, &None, &|x| format!("--file {}", x.detail));
    section("Containers", &s.docker, &s.docker_note, &|x| format!("--container {}", x.name));
    section("Services", &s.services, &s.services_note, &|x| format!("--unit {}{}", x.name, if x.user { " --user" } else { "" }));
    out
}

/// A machine's services and containers, failed first.
pub fn services(l: &crate::fleet::services::ServiceList) -> String {
    let mut out = String::new();
    for (title, list, note) in [("Services", &l.services, &l.services_note), ("Containers", &l.containers, &l.containers_note)] {
        if list.is_empty() && note.is_none() {
            continue;
        }
        out.push_str(&format!("{title}\n"));
        if let Some(n) = note {
            out.push_str(&format!("  ({n})\n"));
        }
        for s in list {
            let state = if s.kind == "docker" { s.status.clone() } else { s.state.clone() };
            let boot = match s.at_boot.as_str() {
                "enabled" => " · at boot",
                _ => "",
            };
            let yours = if s.user && s.kind == "service" { " (yours)" } else { "" };
            out.push_str(&format!("  {:<36} {:<24} {}{boot}{yours}\n", s.name, state, s.detail));
        }
    }
    out
}

/// A machine's servers and ports, web servers first.
pub fn ports(list: &[crate::fleet::ports::ListeningPort]) -> String {
    if list.is_empty() {
        return "Nothing is listening there.\n".into();
    }
    let mut out = String::new();
    for p in list {
        let what = [Some(p.name()), p.container.clone().filter(|c| Some(c) != p.title.as_ref()).map(|c| format!("container {c}")), (!p.process.is_empty() && p.title.is_some()).then(|| p.process.clone())]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let web = match p.status {
            Some(s) if p.web => format!("http {s}"),
            _ => String::new(),
        };
        let reach = if p.local_only() { "localhost only" } else { "" };
        out.push_str(&format!("  {:<6} {:<48} {:<9} {}\n", p.port, what, web, reach).trim_end());
        out.push('\n');
    }
    out
}

/// What the agents used, by model, and the cost at list prices.
pub fn usage(u: &crate::api::UsageOut) -> String {
    let short = |n: u64| match n {
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.0}K", n as f64 / 1e3),
        n => n.to_string(),
    };
    if u.models.is_empty() {
        return format!("No agent use in the last {} days.\n", u.days);
    }
    let mut out = format!("Last {} days: ${:.2} at API prices ({})\n\n", u.days, u.total_cost, u.prices_as_of);
    out.push_str(&format!("  {:<24} {:<12} {:>9} {:>9} {:>11} {:>10}\n", "model", "agent", "requests", "output", "all tokens", "cost"));
    for m in &u.models {
        let agent = match m.tool.as_str() {
            "claude" => "Claude Code",
            "codex" => "Codex",
            t => t,
        };
        let cost = m.cost.map(|c| format!("${c:.2}")).unwrap_or_else(|| "no price".into());
        out.push_str(&format!("  {:<24} {:<12} {:>9} {:>9} {:>11} {:>10}\n", m.model, agent, m.tokens.requests, short(m.tokens.output), short(m.tokens.total()), cost));
    }
    for e in &u.errors {
        out.push_str(&format!("\n  {}: {}\n", e["machine_id"].as_str().unwrap_or(""), e["error"].as_str().unwrap_or("")));
    }
    out
}

/// A machine's GPUs, then its busiest processes.
pub fn processes(l: &crate::fleet::procs::ProcessList, n: usize) -> String {
    let mb = |b: u64| if b >= 1 << 30 { format!("{:.1} GB", b as f64 / (1u64 << 30) as f64) } else { format!("{} MB", b >> 20) };
    let mut out = String::new();
    for g in &l.gpus {
        let mem = match (g.memory_used, g.memory_total) {
            (Some(u), Some(t)) => format!("{} of {}", mb(u), mb(t)),
            (Some(u), None) => format!("{} in use", mb(u)),
            _ => String::new(),
        };
        out.push_str(&format!("GPU  {}  {:.0}% busy  {mem}\n", g.name, g.busy.unwrap_or(0.)));
    }
    if !l.gpus.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!("  {:>7} {:<10} {:>7} {:>9}  {}\n", "pid", "user", "cpu", "memory", "command"));
    for p in l.processes.iter().take(n) {
        let cmd: String = p.command.chars().take(80).collect();
        out.push_str(&format!("  {:>7} {:<10} {:>6.1}% {:>9}  {}\n", p.pid, p.user.chars().take(10).collect::<String>(), p.cpu, mb(p.memory), cmd));
    }
    out.push_str(&format!("\n{} processes, {} cores.\n", l.total, l.cores));
    out
}
