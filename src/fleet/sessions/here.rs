//! Which of Muxin's sessions this process runs in, if any: Muxin's CLI or
//! MCP server run by an agent in a session (Codex in `beelink/codex-3f9a`
//! runs `muxin mcp`). A session it starts from there is that session's helper.
//!
//! The session server sets `MUXEN_SESSION` (and `MUXIN_SESSION`) in every
//! session. Agent CLIs don't always pass it on: Codex gives MCP servers only a
//! few variables of its own environment. So, failing that, this looks up the
//! processes it runs under: one whose environment has it (Linux), or the
//! agent CLI whose hooks reported to a session (`~/.muxen/state/<name>.json`
//! holds its pid).

use serde_json::Value;

/// The name of the session this process runs in (on this computer).
pub fn current() -> Option<String> {
    let from_env = ["MUXIN_SESSION", "MUXEN_SESSION"].iter().find_map(|v| std::env::var(v).ok().filter(|n| valid(n)));
    from_env.or_else(|| by_ancestors(&ancestors(std::process::id())))
}

/// A session's name as the session server takes it.
fn valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= 200 && !name.contains(['/', '\\', '\0', ' ', '='])
}

fn by_ancestors(pids: &[u32]) -> Option<String> {
    if pids.is_empty() {
        return None;
    }
    if let Some(name) = pids.iter().find_map(|p| env_of(*p)) {
        return Some(name);
    }
    let reports = reports();
    owner(pids, &reports)
}

/// The session whose agent CLI is one of `pids` (closest first), by its reports.
fn owner(pids: &[u32], reports: &[(String, Value)]) -> Option<String> {
    pids.iter().find_map(|pid| reports.iter().find(|(_, r)| r["pid"].as_u64() == Some(*pid as u64)).map(|(name, _)| name.clone()))
}

/// What each session's agent last reported (agent/src/states.rs).
fn reports() -> Vec<(String, Value)> {
    let Some(dir) = dirs::home_dir().map(|h| h.join(".muxen").join("state")) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().to_string();
            let name = file.strip_suffix(".json")?.to_string();
            let report = std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())?;
            valid(&name).then_some((name, report))
        })
        .collect()
}

/// This process's parent, its parent, ... (not this one; at most 16).
fn ancestors(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut at = pid;
    while out.len() < 16 {
        match parent_of(at) {
            Some(p) if p > 1 && !out.contains(&p) => {
                out.push(p);
                at = p;
            }
            _ => break,
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (name) state ppid ...": the name can hold spaces and parentheses.
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
    (got == size).then_some(info.pbi_ppid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

/// `MUXEN_SESSION` in another process's environment (Linux: the user's own).
#[cfg(target_os = "linux")]
fn env_of(pid: u32) -> Option<String> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    session_in_environ(&env)
}

#[cfg(not(target_os = "linux"))]
fn env_of(_pid: u32) -> Option<String> {
    None
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn session_in_environ(env: &[u8]) -> Option<String> {
    env.split(|b| *b == 0).find_map(|var| {
        let var = std::str::from_utf8(var).ok()?;
        let name = var.strip_prefix("MUXIN_SESSION=").or_else(|| var.strip_prefix("MUXEN_SESSION="))?;
        valid(name).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_session_is_the_one_its_agent_reported_to() {
        let reports = vec![("claude-aaaa".to_string(), json!({ "pid": 40 })), ("codex-bbbb".to_string(), json!({ "pid": 41 })), ("shell-cccc".to_string(), json!({}))];
        // muxin mcp ← codex (41) ← the session's shell (12): the closest wins.
        assert_eq!(owner(&[41, 12, 1], &reports).as_deref(), Some("codex-bbbb"));
        assert_eq!(owner(&[99, 40], &reports).as_deref(), Some("claude-aaaa"));
        assert_eq!(owner(&[99, 98], &reports), None);
    }

    #[test]
    fn environments_name_the_session() {
        assert_eq!(session_in_environ(b"HOME=/root\0MUXEN_SESSION=codex-3f9a\0TERM=xterm\0").as_deref(), Some("codex-3f9a"));
        assert_eq!(session_in_environ(b"MUXEN_SESSION=\0").as_deref(), None);
        assert_eq!(session_in_environ(b"MUXEN_SESSION=a/b\0").as_deref(), None);
        assert_eq!(session_in_environ(b"PATH=/bin\0").as_deref(), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_has_ancestors() {
        let up = ancestors(std::process::id());
        assert!(!up.is_empty() && !up.contains(&std::process::id()), "{up:?}");
    }
}
