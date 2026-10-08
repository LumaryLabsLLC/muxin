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
        return "No machines. Add one with `openrunner add-machine --name <name> --host <ssh-host>`.\n".into();
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
    if let Some(e) = &s.error {
        let _ = writeln!(out, "error: {e}");
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
    if !m.diagnostics.is_empty() {
        let _ = writeln!(out, "\nDiagnostics:");
        for d in &m.diagnostics {
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
        if let Some(e) = &m.summary.error {
            let _ = write!(out, "\n  {e}");
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
        out.push_str("Create one with `openrunner workspace create [--machine <id>] [--repo owner/repo]`.\n");
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
                w.owner.clone().unwrap_or_else(|| "—".into()),
                format!("{}", w.commands),
                ago(w.last_used_at),
            ]
        })
        .collect();
    out.push_str(&table(&["WORKSPACE", "NAME", "STATUS", "MACHINE", "REPO", "OWNER", "CMDS", "USED"], &rows));
    out
}

pub fn workspace_created(r: &WorkspaceCreated) -> String {
    let w = &r.workspace;
    format!(
        "✓ {} ({}) on {} · {}\n  shell: {}\nRun commands with `openrunner workspace exec {} -- <command>`; destroy it with `openrunner workspace rm {}`.\n",
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
