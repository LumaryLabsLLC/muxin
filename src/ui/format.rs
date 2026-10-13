use chrono::{DateTime, Utc};

use crate::fleet::model::Machine;
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

/// Whether Muxen can show the machine's desktop (Linux with X11, macOS).
pub fn has_desktop(m: &Machine) -> bool {
    // A Linux machine needs an X server; one checked before checks said is given the benefit of the doubt.
    m.platform.starts_with("darwin") || ((m.platform.starts_with("linux") || m.platform.starts_with("windows")) && m.probe.as_ref().and_then(|p| p.desktop) != Some(false))
}

/// Gradient for a machine's icon tile, by OS.
pub fn machine_tile(m: &Machine) -> (u32, u32) {
    use super::kit::tiles;
    if m.cloud.as_ref().is_some_and(|c| c.provider == "digitalocean") {
        tiles::DIGITALOCEAN
    } else if m.platform.starts_with("darwin") {
        tiles::MACOS
    } else if m.platform.starts_with("linux") {
        tiles::LINUX
    } else if m.platform.starts_with("windows") {
        tiles::WINDOWS
    } else {
        tiles::OTHER
    }
}

/// The line under a machine's name ("Ubuntu 26.04 · x64", "Windows 11 · VM
/// on Beelink"), with a color role: "muted" or "error".
pub fn machine_subtitle(m: &Machine) -> (String, &'static str) {
    machine_subtitle_in(m, false)
}

/// The same; `nested` under the machine it runs on, which needn't be named
/// again ("Windows 11 · VM").
pub fn machine_subtitle_in(m: &Machine, nested: bool) -> (String, &'static str) {
    if m.is_disconnected() {
        return ("Disconnected".into(), "muted");
    }
    if m.checking && m.probe.is_none() {
        return ("Connecting…".into(), "muted");
    }
    if m.status == "offline" {
        let seen = m.unreachable.as_ref().and_then(|u| u.last_seen_at);
        if is_powered_off(m) {
            return ("Powered off".into(), "muted");
        }
        return match (m.unreachable.as_ref().map(|u| u.kind.as_str()), is_just_offline(m), seen) {
            (Some("stopped" | "host-down"), _, _) => (unreachable_reason(m), "muted"),
            (_, true, Some(t)) => (format!("Offline · seen {}", ago(Some(t))), "muted"),
            (_, true, None) => ("Offline".into(), "muted"),
            (_, false, _) => (unreachable_reason(m), "error"),
        };
    }
    // Where it runs takes the place of its architecture.
    let place = m.parent.as_ref().map(|p| match (nested, p.relation.as_str()) {
        (true, "vm") => "VM".to_string(),
        (true, "container") => "Container".to_string(),
        (true, _) => String::new(),
        (false, _) => p.label(),
    });
    let arch = place.unwrap_or_else(|| m.platform.split_once('-').map(|(_, a)| a).unwrap_or("").to_string());
    let os = os_label(m);
    let line = match (os.is_empty(), arch.is_empty()) {
        (false, false) => format!("{os} · {arch}"),
        (false, true) => os,
        (true, false) => arch,
        (true, true) if m.host.is_empty() || m.local => "Not checked".into(),
        (true, true) => m.host.clone(),
    };
    (line, "muted")
}

/// "Ubuntu 26.04", "macOS 27", "Windows 11", else the OS family from the platform.
fn os_label(m: &Machine) -> String {
    match m.probe.as_ref().and_then(|p| p.os_version.as_deref()).filter(|v| !v.is_empty()) {
        Some(v) => short_os(v),
        None if m.platform.starts_with("darwin") => "macOS".into(),
        None if m.platform.starts_with("linux") => "Linux".into(),
        None if m.platform.starts_with("windows") => "Windows".into(),
        None => String::new(),
    }
}

/// Keeps the name and a short version: "Ubuntu 26.04.1 LTS" -> "Ubuntu 26.04",
/// "macOS 27.0.1" -> "macOS 27", "Microsoft Windows 11 Pro" -> "Windows 11".
fn short_os(v: &str) -> String {
    let v = v.trim().trim_start_matches("Microsoft ");
    let keep = if v.starts_with("macOS") { 1 } else { 2 };
    let mut words = Vec::new();
    for word in v.split_whitespace() {
        if word.starts_with(|c: char| c.is_ascii_digit()) {
            words.push(word.split('.').take(keep).collect::<Vec<_>>().join("."));
            break;
        }
        words.push(word.to_string());
    }
    words.join(" ")
}

pub fn machine_icon(m: &Machine) -> &'static str {
    if m.cloud.as_ref().is_some_and(|c| c.provider == "digitalocean") {
        return "brand-digitalocean";
    }
    let name = m.name.to_ascii_lowercase();
    let laptop = m.probe.as_ref().and_then(|p| p.form_factor.as_deref()) == Some("laptop");
    if name.contains("nas") || name.contains("storage") {
        "hard-drive"
    } else if laptop || name.contains("laptop") || name.contains("macbook") || name.contains("book") {
        "laptop"
    } else if m.platform.starts_with("windows") || m.platform.starts_with("darwin") {
        // A Mac or PC on a desk; Linux boxes are mostly servers.
        "monitor"
    } else {
        "server"
    }
}

#[cfg(test)]
mod tests {
    use super::short_os;

    #[test]
    fn short_os_names() {
        assert_eq!(short_os("Ubuntu 26.04.1 LTS"), "Ubuntu 26.04");
        assert_eq!(short_os("macOS 27.0.1"), "macOS 27");
        assert_eq!(short_os("Microsoft Windows 11 Pro"), "Windows 11");
        assert_eq!(short_os("Debian GNU/Linux 12 (bookworm)"), "Debian GNU/Linux 12");
        assert_eq!(short_os("Arch Linux"), "Arch Linux");
    }
}

/// Offline in the everyday sense (shut down, asleep, off the network), as
/// opposed to on but refusing us: nothing for the user to fix.
pub fn is_just_offline(m: &Machine) -> bool {
    m.status == "offline" && m.unreachable.as_ref().is_some_and(|u| matches!(u.kind.as_str(), "offline" | "powered-off" | "stopped" | "host-down"))
}

/// A cloud machine that's powered off (and still billed).
pub fn is_powered_off(m: &Machine) -> bool {
    m.status == "offline" && m.unreachable.as_ref().is_some_and(|u| u.kind == "powered-off")
}

/// A VM or container on another machine that's stopped there.
pub fn is_stopped(m: &Machine) -> bool {
    m.status == "offline" && m.unreachable.as_ref().is_some_and(|u| u.kind == "stopped")
}

/// A machine that can't be reached, in a word or two: "Offline", "Can't connect".
pub fn unreachable_word(m: &Machine) -> &'static str {
    if m.is_disconnected() {
        "Disconnected"
    } else if is_powered_off(m) {
        "Powered off"
    } else if is_stopped(m) {
        "Stopped"
    } else if is_just_offline(m) {
        "Offline"
    } else {
        "Can't connect"
    }
}

/// Why it can't be reached, short enough for the sidebar.
pub fn unreachable_reason(m: &Machine) -> String {
    let parent = m.parent.as_ref().map(|p| p.name.as_str()).unwrap_or("Its host");
    match m.unreachable.as_ref().map(|u| u.kind.as_str()) {
        Some("stopped") => return "Stopped".into(),
        Some("host-down") => return m.unreachable.as_ref().map(|u| u.title.clone()).unwrap_or_else(|| format!("{parent} is offline")),
        _ => {}
    }
    match m.unreachable.as_ref().map(|u| u.kind.as_str()) {
        Some("offline") => "Offline",
        Some("powered-off") => "Powered off",
        Some("refused") => "Not accepting ssh",
        Some("auth") => "ssh key not accepted",
        Some("unknown-host") => "Host not found",
        Some("host-key") => "Host key changed",
        Some("tailscale-off") => "Tailscale is off here",
        _ => "Can't connect",
    }
    .into()
}

