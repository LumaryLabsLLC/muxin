//! Plain-text fleet report for `muxen check`.

use std::fmt::Write;

use chrono::Utc;

use super::model::{Machine, Snapshot};
use crate::util;

pub fn render(s: &Snapshot, color: bool) -> String {
    let paint = |code: &str, text: &str| if color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() };
    let dim = |t: &str| paint("2", t);
    let bold = |t: &str| paint("1", t);
    let now = Utc::now();
    let c = &s.counts;
    let mut out = String::new();

    let _ = writeln!(out, "{}  {}", bold("Fleet overview"), dim(&format!("{} machines · {} runners", c.machines, c.runners)));
    let _ = writeln!(
        out,
        "{} working   {} idle   {} offline   {} unknown   {} queued",
        paint("34", &c.working.to_string()),
        paint("32", &c.idle.to_string()),
        c.offline,
        c.unknown,
        c.queued
    );
    let source = if s.github.live { "live from GitHub".to_string() } else { "from the machine check (GitHub not connected)".to_string() };
    let _ = writeln!(out, "{}", dim(&format!("Inventory {} · runner state {source}", s.inventory.path)));
    out.push('\n');

    let headers = ["MACHINE", "PLATFORM", "RUNNERS", "CPU", "MEM", "DISK FREE", "UPTIME", "DIAGNOSTICS"];
    let rows: Vec<[String; 8]> = s.machines.iter().map(|m| row(m)).collect();
    let mut widths = headers.map(str::len);
    for r in &rows {
        for (i, cell) in r.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        cells.iter().enumerate().map(|(i, c)| format!("{c:<w$}", w = widths[i])).collect::<Vec<_>>().join("  ").trim_end().to_string()
    };
    let _ = writeln!(out, "{}", dim(&line(&headers.map(String::from))));
    for (m, r) in s.machines.iter().zip(&rows) {
        let text = line(r);
        let text = match m.status.as_str() {
            "offline" => paint("31", &text),
            "online" => text,
            _ => dim(&text),
        };
        let _ = writeln!(out, "{text}");
    }

    if !s.jobs.running.is_empty() || !s.jobs.queued.is_empty() {
        out.push('\n');
        for j in &s.jobs.running {
            let since = j.started_at.unwrap_or(j.created_at);
            let _ = writeln!(
                out,
                "{} {}  {}  {}  {}",
                paint("34", "▶"),
                j.runner_name.as_deref().unwrap_or("?"),
                j.name,
                dim(&j.repo),
                dim(&util::short_duration((now - since).num_seconds().max(0) as u64))
            );
        }
        for j in &s.jobs.queued {
            let _ = writeln!(
                out,
                "{} {}  {}  {}  {}",
                dim("…"),
                j.name,
                dim(&j.repo),
                dim(&format!("[{}]", j.labels.join(", "))),
                dim(&format!("waiting {}", util::short_duration((now - j.created_at).num_seconds().max(0) as u64)))
            );
        }
    }

    if !s.attention.is_empty() {
        let _ = writeln!(out, "\n{}", bold(&format!("Needs attention ({})", s.attention.len())));
        for a in &s.attention {
            let mark = match a.level.as_str() {
                "error" => paint("31", "✖"),
                "warning" => paint("33", "!"),
                _ => dim("i"),
            };
            let _ = writeln!(out, "  {mark} {}", a.title);
            if !a.detail.is_empty() {
                let _ = writeln!(out, "    {}", dim(&a.detail));
            }
        }
    }
    out
}

fn row(m: &Machine) -> [String; 8] {
    let p = m.probe.as_ref().filter(|p| p.ok);
    let dash = || "—".to_string();
    let runners = if m.kind == "other" {
        match m.status.as_str() {
            "online" => "connected".into(),
            "offline" => "offline".into(),
            _ => "not checked".into(),
        }
    } else {
        let s = &m.summary;
        if s.working > 0 {
            format!("{}/{} working", s.working, s.total)
        } else if s.idle == s.total {
            format!("{} idle", s.idle)
        } else if s.offline > 0 {
            format!("{}/{} offline", s.offline, s.total)
        } else {
            format!("{} unknown", s.unknown)
        }
    };
    let cpu = p.and_then(|p| p.cpu_pct).map(|v| format!("{v:.0}%")).unwrap_or_else(dash);
    let mem = p
        .and_then(|p| Some((p.mem_used? as f64 / p.mem_total? as f64) * 100.0))
        .map(|v| format!("{v:.0}%"))
        .unwrap_or_else(dash);
    let disk = p.and_then(|p| p.disk_free).map(util::bytes).unwrap_or_else(dash);
    let uptime = p.and_then(|p| p.uptime_sec).map(util::uptime).unwrap_or_else(dash);
    let warnings = m.diagnostics.iter().filter(|d| matches!(d.level.as_str(), "warning" | "error")).count();
    let unknown = m.diagnostics.iter().filter(|d| d.level == "unknown").count();
    let diagnostics = match m.status.as_str() {
        "offline" => "unreachable".to_string(),
        "unchecked" | "manual" => "not checked".to_string(),
        _ if warnings > 0 => format!("{warnings} warning{}", if warnings == 1 { "" } else { "s" }),
        _ if unknown > 0 => format!("{unknown} unknown"),
        _ => "no warnings".to_string(),
    };
    [m.name.clone(), if m.platform.is_empty() { dash() } else { m.platform.clone() }, runners, cpu, mem, disk, uptime, diagnostics]
}
