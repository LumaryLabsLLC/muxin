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
        return "No machines. Add one with `muxen add-machine --name <name> --host <ssh-host>`.\n".into();
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
        out.push_str("Create one with `muxen workspace create [--machine <id>] [--repo owner/repo]`.\n");
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
        "✓ {} ({}) on {} · {}\n  shell: {}\nRun commands with `muxen workspace exec {} -- <command>`; destroy it with `muxen workspace rm {}`.\n",
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
        out.push_str("No agent sessions. Start one with `muxen session start <machine> claude [--cwd ~/code/app]`.\n");
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
                vec![s.id.clone(), s.agent_name.clone(), state, s.cwd.clone(), s.created_at.map(ago).unwrap_or_else(|| "—".into())]
            })
            .collect();
        out.push_str(&table(&["SESSION", "AGENT", "STATE", "FOLDER", "STARTED"], &rows));
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

pub fn session_started(s: &crate::fleet::sessions::SessionInfo) -> String {
    format!(
        "Started {} ({}) in {}.\nWatch it in the Muxen app, or attach here: {}\n",
        s.id, s.agent_name, s.cwd, s.attach_command
    )
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
            Some(m) => format!("Muxen uses {l} (the GitHub CLI login on {}).\n", m.machine),
            None => format!("Muxen uses {l}.\n"),
        },
        (Some(l), var) => format!("Muxen uses {l} (the token in ${var}).\n"),
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
        out.push_str("\nSwitch with `muxen github use <login>` or `--machine <id>`; add an account with `gh auth login`.\n");
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
        out.push_str(&format!("\nConnect directly with `muxen connection {} --direct on`.\n", c.machine_id));
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
        fixes.push(format!("muxen git setup {} --identity{who}", g.machine_id));
    }
    if g.push != "gh" {
        fixes.push(match &g.gh_account {
            Some(_) => format!("muxen git setup {} --credentials", g.machine_id),
            None => format!("sign in to gh on {} (gh auth login), then: muxen git setup {} --credentials", g.machine_id, g.machine_id),
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
