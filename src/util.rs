/// "Mac Studio (M2)" -> "mac-studio-m2"
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "machine".into() } else { out }
}

/// ("Darwin", "arm64") -> "darwin-arm64", ("Linux", "x86_64") -> "linux-x64", ("windows", "AMD64") -> "windows-x64"
pub fn platform(os: &str, arch: &str) -> String {
    let os = os.trim().to_ascii_lowercase();
    let arch = match arch.trim().to_ascii_lowercase().as_str() {
        "arm64" | "aarch64" => "arm64".to_string(),
        "x86_64" | "amd64" | "x64" => "x64".to_string(),
        "i386" | "i686" | "x86" => "x86".to_string(),
        a if a.starts_with("armv7") || a == "arm" => "arm".to_string(),
        a => a.to_string(),
    };
    match (os.is_empty(), arch.is_empty()) {
        (true, _) => String::new(),
        (false, true) => os,
        (false, false) => format!("{os}-{arch}"),
    }
}

/// 381_000_000_000 -> "381 GB" (decimal units, like Finder and `df -H`)
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 && unit > 0 { format!("{v:.1} {}", UNITS[unit]) } else { format!("{v:.0} {}", UNITS[unit]) }
}

/// 83_400 -> "23h 10m", 10_022_400 -> "116d 0h", 95 -> "1m"
pub fn uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// 42 -> "42s", 754 -> "12m", 5400 -> "1h 30m"
pub fn short_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if m == 0 { format!("{h}h") } else { format!("{h}h {m}m") }
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(slug("Mac Studio (M2)"), "mac-studio-m2");
        assert_eq!(slug("  --  "), "machine");
        assert_eq!(platform("Darwin", "arm64"), "darwin-arm64");
        assert_eq!(platform("linux", "x86_64"), "linux-x64");
        assert_eq!(platform("windows", "AMD64"), "windows-x64");
        assert_eq!(bytes(381_000_000_000), "381 GB");
        assert_eq!(bytes(6_800_000_000), "6.8 GB");
        assert_eq!(uptime(83_400), "23h 10m");
        assert_eq!(uptime(116 * 86_400), "116d 0h");
        assert_eq!(short_duration(754), "12m");
    }
}
