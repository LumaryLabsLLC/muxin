use chrono::{DateTime, Utc};

use crate::fleet::model::{Machine, Snapshot};
use crate::util;

pub fn ago(t: Option<DateTime<Utc>>) -> String {
    match t {
        None => "never".into(),
        Some(t) => {
            let secs = (Utc::now() - t).num_seconds().max(0) as u64;
            if secs < 10 { "just now".into() } else { format!("{} ago", util::short_duration(secs)) }
        }
    }
}

pub fn until(t: DateTime<Utc>) -> String {
    let secs = (t - Utc::now()).num_seconds();
    if secs <= 0 { "now".into() } else { format!("in {}", util::short_duration(secs as u64)) }
}

/// Live elapsed time like "4m 07s", for running jobs.
pub fn elapsed(since: DateTime<Utc>) -> String {
    let secs = (Utc::now() - since).num_seconds().max(0) as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub fn bytes(n: u64) -> String {
    util::bytes(n)
}

pub fn uptime(secs: u64) -> String {
    util::uptime(secs)
}

pub fn mem_fraction(m: &Machine) -> Option<f32> {
    let p = m.probe.as_ref().filter(|p| p.ok)?;
    Some((p.mem_used? as f64 / p.mem_total?.max(1) as f64) as f32)
}

pub fn disk_used_fraction(m: &Machine) -> Option<f32> {
    let p = m.probe.as_ref().filter(|p| p.ok)?;
    let total = p.disk_total?.max(1);
    Some(1.0 - (p.disk_free? as f64 / total as f64) as f32)
}

pub fn disk_low(m: &Machine) -> bool {
    m.diagnostics.iter().any(|d| d.code == "low-disk")
}

/// "3/7 working", "2 idle", "1/2 offline", ... for a machine with runners.
pub fn runner_summary(m: &Machine) -> String {
    let s = &m.summary;
    if s.total == 0 {
        return match m.status.as_str() {
            "online" => "connected".into(),
            "offline" => "unreachable".into(),
            _ => "not checked".into(),
        };
    }
    if s.working > 0 {
        format!("{}/{} working", s.working, s.total)
    } else if s.idle == s.total {
        format!("{} idle", s.idle)
    } else if s.offline > 0 {
        format!("{}/{} offline", s.offline, s.total)
    } else if s.unknown == s.total {
        format!("{} unknown", s.unknown)
    } else {
        format!("{} idle", s.idle)
    }
}

/// "No warnings", "2 warnings", "1 unknown", "Unreachable", "Not checked".
pub fn diagnostics_summary(m: &Machine) -> (String, &'static str) {
    if m.checking && m.probe.is_none() {
        return ("Checking…".into(), "muted");
    }
    match m.status.as_str() {
        "offline" => return ("Unreachable".into(), "error"),
        "unchecked" | "manual" => return ("Not checked".into(), "muted"),
        _ => {}
    }
    let warnings = m.diagnostics.iter().filter(|d| matches!(d.level.as_str(), "warning" | "error")).count();
    let unknown = m.diagnostics.iter().filter(|d| d.level == "unknown").count();
    if warnings > 0 {
        (format!("{warnings} warning{}", if warnings == 1 { "" } else { "s" }), "warning")
    } else if unknown > 0 {
        (format!("{unknown} unknown"), "muted")
    } else {
        ("No warnings".into(), "muted")
    }
}

pub fn machine_icon(m: &Machine) -> &'static str {
    let name = m.name.to_ascii_lowercase();
    if m.kind == "other" {
        "hard-drive"
    } else if name.contains("laptop") || name.contains("macbook") || name.contains("book") {
        "laptop"
    } else if m.platform.starts_with("windows") {
        "monitor"
    } else {
        "server"
    }
}

/// The org/repo shown next to the app name.
pub fn scope_label(s: &Snapshot) -> Option<String> {
    s.github.scopes.first().map(|sc| sc.name.clone()).or_else(|| s.github.detected.first().cloned())
}
