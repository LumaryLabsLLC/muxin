//! Muxin's hooks in Claude Code's and Codex's settings, so they tell the
//! agent what they're doing (see states.rs). Turning them on and off is
//! idempotent and touches only Muxin's own entries: its command runs
//! `muxen-agent report`, which nothing else does. Other tools' hooks (Rex's,
//! herdr's) stay as they are, in their places.
//!
//! Each hook does nothing outside a Muxin session, and can't break its agent:
//! a quiet `|| true` wrapper with a short timeout, as Rex does it.
//!
//! - Claude Code: `settings.json` in its config folder (`CLAUDE_CONFIG_DIR`,
//!   else `~/.claude`).
//! - Codex: `hooks.json` in `CODEX_HOME` (else `~/.codex`). Codex runs a
//!   user's hook only once it's trusted, by its hash under `[hooks.state]` in
//!   `config.toml`; Muxin trusts its own there, as `/hooks` would.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// Seconds a hook may take (ours takes milliseconds): 5, as Rex's; 3 for
/// the events Codex caps there (it trusts a hook by its capped timeout).
fn timeout(event: &str) -> u64 {
    if matches!(event, "SessionEnd" | "Interrupt") { 3 } else { 5 }
}

/// Claude Code's events we listen to: ones every recent version has (an
/// event it doesn't know could make it doubt the whole settings file).
const CLAUDE_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Notification", "Stop", "SessionEnd"];
/// Codex's; it skips names it doesn't know.
const CODEX_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PreToolUse", "PermissionRequest", "PostToolUse", "Stop", "Interrupt", "SessionEnd"];

/// Where the agent CLIs keep their settings, as the user's sessions see it.
#[derive(Debug, Clone, Default)]
pub struct Dirs {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
}

impl Dirs {
    /// From the login shell's `CLAUDE_CONFIG_DIR` and `CODEX_HOME` (empty:
    /// not set), else the defaults under `home`.
    pub fn new(home: &Path, claude: Option<&str>, codex: Option<&str>) -> Dirs {
        let set = |v: Option<&str>| v.map(str::trim).filter(|v| !v.is_empty()).map(crate::sys::expand_home);
        Dirs {
            claude: Some(set(claude).unwrap_or_else(|| home.join(".claude"))),
            // Codex itself resolves a CODEX_HOME it's given (its trust keys use that path).
            codex: Some(set(codex).map(|p| std::fs::canonicalize(&p).unwrap_or(p)).unwrap_or_else(|| home.join(".codex"))),
        }
    }
}

/// The agent's own path, where Muxin installs it.
fn agent_path() -> PathBuf {
    crate::home_dir().join("bin").join(if cfg!(windows) { "muxen-agent.exe" } else { "muxen-agent" })
}

/// The hook's command for `agent`: nothing outside a Muxin session, never a
/// failure, never a word on stdout.
fn command(agent: &str, agent_path: &Path) -> String {
    let path = agent_path.display().to_string();
    let quoted = if path.chars().any(|c| matches!(c, '"' | '$' | '`' | '\\' | '\'' | '!')) { sh_quote(&path) } else { format!("\"{path}\"") };
    let inner = format!("[ -z \"$MUXEN_SESSION\" ] || {quoted} report {agent} 2>/dev/null || true");
    format!("/bin/sh -c {}", sh_quote(&inner))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Ours: runs `muxen-agent report` in a Muxin session.
fn is_ours(command: &str) -> bool {
    command.contains("MUXEN_SESSION") && command.contains("muxen-agent") && command.contains(" report ")
}

fn handler(event: &str, command: &str) -> Value {
    json!({ "type": "command", "command": command, "timeout": timeout(event) })
}

/// Turns Muxin's hooks on or off for each CLI whose settings folder is there
/// (it's made the first time the CLI runs). Says, per CLI, "on", "off",
/// "none" (not set up here) or what went wrong.
pub fn apply(on: bool, dirs: &Dirs) -> Value {
    if cfg!(windows) {
        return json!({ "claude": "none", "codex": "none" });
    }
    let agent = agent_path();
    let mut out = Map::new();
    let claude = match &dirs.claude {
        Some(dir) if dir.is_dir() => match edit_json(&dir.join("settings.json"), false, |root| set_hooks(root, CLAUDE_EVENTS, &command("claude", &agent), on)) {
            Ok(_) => if on { "on" } else { "off" }.to_string(),
            Err(e) => e,
        },
        _ => "none".to_string(),
    };
    out.insert("claude".into(), json!(claude));
    let codex = match &dirs.codex {
        Some(dir) if dir.is_dir() => match codex(dir, &command("codex", &agent), on) {
            Ok(()) => if on { "on" } else { "off" }.to_string(),
            Err(e) => e,
        },
        _ => "none".to_string(),
    };
    out.insert("codex".into(), json!(codex));
    Value::Object(out)
}

/// Codex: the hooks, then their trust.
fn codex(dir: &Path, command: &str, on: bool) -> Result<(), String> {
    let hooks_file = dir.join("hooks.json");
    let mut groups = Map::new();
    edit_json(&hooks_file, true, |root| {
        let changed = set_hooks(root, CODEX_EVENTS, command, on)?;
        // Where each of ours ended up: the trust key names its place.
        if let Some(hooks) = root.get("hooks").and_then(Value::as_object) {
            for (event, list) in hooks {
                for (i, g) in list.as_array().into_iter().flatten().enumerate() {
                    if is_our_group(g) {
                        groups.insert(event.clone(), json!(i));
                    }
                }
            }
        }
        Ok(changed)
    })?;
    let prefix = format!("{}:", hooks_file.display());
    let wanted: Vec<(String, String)> = if on {
        CODEX_EVENTS.iter().filter_map(|e| groups.get(*e).and_then(Value::as_u64).map(|g| (format!("{prefix}{}:{g}:0", snake(e)), trust_hash(e, command)))).collect()
    } else {
        Vec::new()
    };
    let ours: HashSet<String> = CODEX_EVENTS.iter().map(|e| trust_hash(e, command)).collect();
    let config = dir.join("config.toml");
    let text = match std::fs::read_to_string(&config) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("couldn't read {}: {e}", config.display())),
    };
    match trust(&text, &prefix, &wanted, &ours)? {
        // Only ours were in it (Muxin made it): none, as before.
        Some(new) if new.trim().is_empty() => {
            let real = std::fs::canonicalize(&config).unwrap_or(config);
            let _ = std::fs::remove_file(real);
        }
        Some(new) => write_keeping(&config, &new)?,
        None => {}
    }
    Ok(())
}

/// Reads a JSON settings file (none yet: `{}`), changes it, and writes it
/// back only when something changed, keeping its order (and a symlink to it).
fn edit_json(path: &Path, drop_if_empty: bool, change: impl FnOnce(&mut Map<String, Value>) -> Result<bool, String>) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("couldn't read {}: {e}", path.display())),
    };
    let mut root = match text.as_deref().map(str::trim) {
        None | Some("") => Map::new(),
        Some(t) => match serde_json::from_str::<Value>(t) {
            Ok(Value::Object(m)) => m,
            _ => return Err(format!("{} isn't JSON Muxin can read, so it was left alone", path.display())),
        },
    };
    if !change(&mut root)? {
        return Ok(false);
    }
    // Nothing of ours and nothing else: no file needed.
    if root.is_empty() && text.is_none() {
        return Ok(false);
    }
    // A hooks file with no hooks left (it held only ours): gone, as before.
    if root.is_empty() && drop_if_empty {
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        std::fs::remove_file(&real).map_err(|e| format!("couldn't remove {}: {e}", real.display()))?;
        return Ok(true);
    }
    let out = format_like(&Value::Object(root), text.as_deref().unwrap_or(""));
    write_keeping(path, &out)?;
    Ok(true)
}

/// JSON written the way `before` was: on one line if it was, else indented
/// as it was (two spaces, as Claude Code writes it, when there's nothing to go by).
fn format_like(value: &Value, before: &str) -> String {
    let trimmed = before.trim();
    let mut out = if !trimmed.is_empty() && !trimmed.contains('\n') {
        value.to_string()
    } else {
        let indent: String = trimmed.lines().nth(1).map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect()).filter(|i: &String| !i.is_empty()).unwrap_or_else(|| "  ".into());
        // Pretty JSON indents by two spaces a level (and its strings hold no line breaks).
        let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
        pretty
            .lines()
            .map(|l| {
                let spaces = l.len() - l.trim_start_matches(' ').len();
                format!("{}{}", indent.repeat(spaces / 2), &l[spaces..])
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    if before.is_empty() || before.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Writes a file in one step (a temporary file renamed over it), through a
/// symlink if it's one (dotfiles), with its permissions.
fn write_keeping(path: &Path, text: &str) -> Result<(), String> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = real.with_extension(format!("muxin-{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("couldn't write {}: {e}", real.display()))?;
    if let Ok(meta) = std::fs::metadata(&real) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &real).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("couldn't write {}: {e}", real.display())
    })
}

fn is_our_group(g: &Value) -> bool {
    let handlers = g["hooks"].as_array().map(Vec::as_slice).unwrap_or_default();
    handlers.len() == 1 && handlers[0]["command"].as_str().is_some_and(is_ours)
}

/// Puts one group of ours under each of `events` (where one is there and
/// right, it stays where it is) and none elsewhere, or takes ours out
/// everywhere. True when anything changed.
fn set_hooks(root: &mut Map<String, Value>, events: &[&str], command: &str, on: bool) -> Result<bool, String> {
    let had_hooks = root.contains_key("hooks");
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err("its \"hooks\" isn't what Muxin expects, so it was left alone".into());
    };
    let mut changed = false;
    let names: Vec<String> = hooks.keys().cloned().chain(events.iter().map(|e| e.to_string())).collect::<Vec<_>>();
    let mut seen = HashSet::new();
    // Lists made or emptied here; the user's own empty ones stay.
    let mut touched: HashSet<String> = HashSet::new();
    for event in names.into_iter().filter(|n| seen.insert(n.clone())) {
        let wanted_here = on && events.contains(&event.as_str());
        let want = json!({ "hooks": [handler(&event, command)] });
        if !hooks.contains_key(&event) {
            touched.insert(event.clone());
        }
        let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
        let Some(list) = list.as_array_mut() else { continue };
        let ours: Vec<usize> = list.iter().enumerate().filter(|(_, g)| g["hooks"].as_array().is_some_and(|h| h.iter().any(|x| x["command"].as_str().is_some_and(is_ours)))).map(|(i, _)| i).collect();
        let right = wanted_here && ours.len() == 1 && list[ours[0]] == want;
        if !right && !ours.is_empty() {
            // Ours alone in a group go with it; ours beside others' leave the group.
            for i in ours.into_iter().rev() {
                let handlers = list[i]["hooks"].as_array_mut().unwrap();
                handlers.retain(|x| !x["command"].as_str().is_some_and(is_ours));
                if handlers.is_empty() {
                    list.remove(i);
                }
            }
            touched.insert(event.clone());
            changed = true;
        }
        if wanted_here && !right {
            list.push(want.clone());
            changed = true;
        }
    }
    // Leave no empty lists behind.
    hooks.retain(|k, v| !(touched.contains(k) && v.as_array().is_some_and(Vec::is_empty)));
    if hooks.is_empty() && (!had_hooks || changed) {
        root.remove("hooks");
    }
    Ok(changed)
}

/// Codex's name for an event in trust keys: "PreToolUse" → "pre_tool_use".
fn snake(event: &str) -> String {
    let mut out = String::new();
    for (i, c) in event.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// The hash Codex trusts a hook by: SHA-256 of its normalized identity, as
/// compact JSON with sorted keys (checked against an entry Codex 0.166 wrote).
fn trust_hash(event: &str, command: &str) -> String {
    // Keys in sorted order (this crate's JSON keeps insertion order).
    let mut handler = Map::new();
    handler.insert("async".into(), json!(false));
    handler.insert("command".into(), json!(command));
    handler.insert("timeout".into(), json!(timeout(event)));
    handler.insert("type".into(), json!("command"));
    let mut identity = Map::new();
    identity.insert("event_name".into(), json!(snake(event)));
    identity.insert("hooks".into(), json!([Value::Object(handler)]));
    let bytes = serde_json::to_vec(&Value::Object(identity)).unwrap_or_default();
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    format!("sha256:{}", digest.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// A TOML basic string's text.
fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The key of a `[hooks.state."<key>"]` header line.
fn state_key(line: &str) -> Option<String> {
    let inner = line.trim().strip_prefix('[')?.trim_start();
    let rest = inner.strip_prefix("hooks.state.")?.trim_start().strip_prefix('"')?;
    let mut key = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let after: String = chars.collect();
                return after.trim_start().starts_with(']').then_some(key);
            }
            '\\' => match chars.next()? {
                '"' => key.push('"'),
                '\\' => key.push('\\'),
                _ => return None,
            },
            c => key.push(c),
        }
    }
    None
}

/// `config.toml` with Muxin's trust entries as `wanted` (key, hash), its
/// stale ones (under `prefix`, with one of `ours` hashes) gone, and
/// everything else as it was; `None` when it's right already. Leaves a file
/// alone that keeps hooks in a shape it doesn't edit (inline tables).
fn trust(text: &str, prefix: &str, wanted: &[(String, String)], ours: &HashSet<String>) -> Result<Option<String>, String> {
    let lines: Vec<&str> = text.lines().collect();
    // The table each line is in: none yet (the top), "[hooks]", or another.
    let mut table: Option<&str> = None;
    for l in &lines {
        let t = l.trim();
        if t.starts_with('[') {
            table = Some(t);
            continue;
        }
        let top_level = table.is_none() && (t.starts_with("hooks.") || t.starts_with("hooks =") || t.starts_with("hooks="));
        if top_level || (table == Some("[hooks]") && t.starts_with("state")) {
            return Err("Codex's config.toml keeps hooks in a way Muxin doesn't change, so Codex will ask to trust its hooks".into());
        }
    }
    // Each table: its header line and the lines up to the next header.
    let mut out: Vec<String> = Vec::new();
    let mut found: HashSet<String> = HashSet::new();
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let Some(key) = state_key(lines[i]).filter(|k| k.starts_with(prefix)) else {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        };
        let mut end = i + 1;
        while end < lines.len() && !lines[end].trim_start().starts_with('[') {
            end += 1;
        }
        // Blank lines and comments at its end belong to what follows.
        while end > i + 1 && (lines[end - 1].trim().is_empty() || lines[end - 1].trim_start().starts_with('#')) {
            end -= 1;
        }
        let body = &lines[i + 1..end];
        let hash = body.iter().find_map(|l| l.trim().strip_prefix("trusted_hash").map(|v| v.trim_start().trim_start_matches('=').trim().trim_matches('"').to_string()));
        match wanted.iter().find(|(k, _)| *k == key) {
            Some((_, want)) => {
                found.insert(key.clone());
                out.push(lines[i].to_string());
                if hash.as_deref() != Some(want.as_str()) {
                    changed = true;
                }
                let mut wrote = false;
                for l in body {
                    if l.trim().starts_with("trusted_hash") {
                        if !wrote {
                            out.push(format!("trusted_hash = {}", toml_string(want)));
                            wrote = true;
                        }
                    } else {
                        out.push(l.to_string());
                    }
                }
                if !wrote {
                    out.push(format!("trusted_hash = {}", toml_string(want)));
                }
            }
            // One of ours from before (another place, or hooks now off): it goes.
            None if hash.as_deref().is_some_and(|h| ours.contains(h)) => changed = true,
            None => out.extend(lines[i..end].iter().map(|l| l.to_string())),
        }
        i = end;
    }
    let missing: Vec<&(String, String)> = wanted.iter().filter(|(k, _)| !found.contains(k)).collect();
    if !changed && missing.is_empty() {
        return Ok(None);
    }
    // Don't leave a run of blank lines where tables went.
    let mut tidy: Vec<String> = Vec::new();
    for l in out {
        if l.trim().is_empty() && tidy.last().is_some_and(|p: &String| p.trim().is_empty()) {
            continue;
        }
        tidy.push(l);
    }
    while tidy.last().is_some_and(|l| l.trim().is_empty()) {
        tidy.pop();
    }
    for (key, hash) in missing {
        if !tidy.is_empty() {
            tidy.push(String::new());
        }
        tidy.push(format!("[hooks.state.{}]", toml_string(key)));
        tidy.push(format!("trusted_hash = {}", toml_string(hash)));
    }
    let mut new = tidy.join("\n");
    if !new.is_empty() {
        new.push('\n');
    }
    Ok(Some(new))
}

/// `muxen-agent hooks on|off|status`, for trying them by hand.
pub fn run(args: &[String]) -> i32 {
    let dirs = Dirs::new(&crate::sys::user_home(), std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(), std::env::var("CODEX_HOME").ok().as_deref());
    match args.first().map(String::as_str) {
        Some("on") => println!("{}", apply(true, &dirs)),
        Some("off") => println!("{}", apply(false, &dirs)),
        Some("status") => println!("{}", status(&dirs)),
        _ => {
            eprintln!("usage: muxen-agent hooks on | off | status");
            return 2;
        }
    }
    0
}

/// Whether Muxin's hooks are there, per CLI, without changing anything.
pub fn status(dirs: &Dirs) -> Value {
    let has = |path: PathBuf, events: &[&str]| -> &'static str {
        let Ok(text) = std::fs::read_to_string(path) else { return "off" };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { return "off" };
        let all = events.iter().all(|e| v["hooks"][*e].as_array().is_some_and(|l| l.iter().any(is_our_group)));
        if all { "on" } else { "off" }
    };
    let one = |dir: &Option<PathBuf>, file: &str, events: &[&str]| match dir {
        Some(d) if d.is_dir() => has(d.join(file), events),
        _ => "none",
    };
    json!({ "claude": one(&dirs.claude, "settings.json", CLAUDE_EVENTS), "codex": one(&dirs.codex, "hooks.json", CODEX_EVENTS) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "/bin/sh -c '[ -z \"$MUXEN_SESSION\" ] || \"/home/adib/.muxen/bin/muxen-agent\" report claude 2>/dev/null || true'";

    #[test]
    fn the_command_does_nothing_outside_a_session_and_never_fails() {
        assert_eq!(command("claude", Path::new("/home/adib/.muxen/bin/muxen-agent")), CMD);
        assert!(is_ours(CMD));
        // Rex's and herdr's aren't ours.
        assert!(!is_ours("/bin/sh -c 'rex __internal session-start --agent claude 2>/dev/null || true'"));
        assert!(!is_ours("bash '/Users/adib/.claude/hooks/herdr-agent-state.sh' session"));
        // A home with a quote in it still quotes right.
        let odd = command("codex", Path::new("/home/o'neil/.muxen/bin/muxen-agent"));
        assert!(is_ours(&odd));
        // Run as a CLI runs it, with echo for the agent: quiet outside a session.
        #[cfg(unix)]
        for (session, expect) in [("", ""), ("claude-1", "report claude")] {
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(command("claude", Path::new("/bin/echo"))).env("MUXEN_SESSION", session).output().unwrap();
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expect, "{session:?}");
        }
        // A failing agent still succeeds.
        #[cfg(unix)]
        assert!(std::process::Command::new("/bin/sh").arg("-c").arg(command("claude", Path::new("/bin/false"))).env("MUXEN_SESSION", "x").status().unwrap().success());
    }

    fn rex_and_herdr() -> Map<String, Value> {
        serde_json::from_str::<Value>(r#"{
            "model": "opus",
            "hooks": {
                "SessionStart": [
                    { "matcher": "*", "hooks": [{ "type": "command", "command": "bash '/x/herdr-agent-state.sh' session", "timeout": 10 }] },
                    { "hooks": [{ "type": "command", "command": "/bin/sh -c 'rex __internal session-start --agent claude 2>/dev/null || true'", "timeout": 5 }] }
                ]
            },
            "permissions": { "allow": ["Bash(ls)"] }
        }"#).unwrap().as_object().unwrap().clone()
    }

    #[test]
    fn on_and_off_leave_other_hooks_and_settings_as_they_were() {
        let before = rex_and_herdr();
        let mut s = before.clone();
        assert!(set_hooks(&mut s, CLAUDE_EVENTS, CMD, true).unwrap());
        for e in CLAUDE_EVENTS {
            assert_eq!(s["hooks"][*e].as_array().unwrap().iter().filter(|g| is_our_group(g)).count(), 1, "{e}");
        }
        // Others keep their places; ours comes after them.
        assert_eq!(s["hooks"]["SessionStart"][0], before["hooks"]["SessionStart"][0]);
        assert_eq!(s["hooks"]["SessionStart"][1], before["hooks"]["SessionStart"][1]);
        assert!(is_our_group(&s["hooks"]["SessionStart"][2]));
        // Settings keep their order.
        assert_eq!(s.keys().collect::<Vec<_>>(), vec!["model", "hooks", "permissions"]);
        // Again: nothing to do.
        assert!(!set_hooks(&mut s, CLAUDE_EVENTS, CMD, true).unwrap());
        // Off: exactly as it was.
        assert!(set_hooks(&mut s, CLAUDE_EVENTS, CMD, false).unwrap());
        assert_eq!(Value::Object(s.clone()), Value::Object(before.clone()));
        assert!(!set_hooks(&mut s, CLAUDE_EVENTS, CMD, false).unwrap());
        // From nothing and back to nothing.
        let mut empty = Map::new();
        set_hooks(&mut empty, CLAUDE_EVENTS, CMD, true).unwrap();
        set_hooks(&mut empty, CLAUDE_EVENTS, CMD, false).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn an_older_command_of_ours_is_replaced_in_any_group() {
        let old = "/bin/sh -c '[ -z \"$MUXEN_SESSION\" ] || \"/old/muxen-agent\" report claude || true'";
        let mut s: Map<String, Value> = serde_json::from_value(json!({ "hooks": { "Stop": [
            { "hooks": [{ "type": "command", "command": "notify-send done" }, { "type": "command", "command": old }] },
            { "hooks": [{ "type": "command", "command": old }] }
        ] } })).unwrap();
        assert!(set_hooks(&mut s, CLAUDE_EVENTS, CMD, true).unwrap());
        let stop = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"].as_array().unwrap().len(), 1, "the other handler stays, ours leaves its group");
        assert_eq!(stop[1]["hooks"][0]["command"], CMD);
    }

    #[test]
    fn files_keep_their_look() {
        let v = json!({ "a": 1, "b": { "c": [2] } });
        assert_eq!(format_like(&v, "{\"a\":0}"), "{\"a\":1,\"b\":{\"c\":[2]}}");
        assert_eq!(format_like(&v, "{\n    \"a\": 0\n}\n"), "{\n    \"a\": 1,\n    \"b\": {\n        \"c\": [\n            2\n        ]\n    }\n}\n");
        assert!(format_like(&v, "").starts_with("{\n  \"a\": 1"));
        assert!(format_like(&v, "").ends_with("}\n"));
    }

    #[test]
    fn a_hook_list_muxin_cant_read_is_left_alone() {
        let mut s: Map<String, Value> = serde_json::from_value(json!({ "hooks": "nope" })).unwrap();
        assert!(set_hooks(&mut s, CLAUDE_EVENTS, CMD, true).is_err());
    }

    #[test]
    fn trust_hashes_match_codex() {
        // herdr's hook, as Codex 0.166 trusted it on the user's Mac.
        let mut handler = Map::new();
        handler.insert("async".into(), json!(false));
        handler.insert("command".into(), json!("bash '/Users/adibhanna/.codex/herdr-agent-state.sh' session"));
        handler.insert("timeout".into(), json!(10));
        handler.insert("type".into(), json!("command"));
        let mut identity = Map::new();
        identity.insert("event_name".into(), json!("session_start"));
        identity.insert("hooks".into(), json!([Value::Object(handler)]));
        let digest = ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(&Value::Object(identity)).unwrap());
        let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "beb71c6c4224069e7c79b01efded0524653c56bb21f01cda2dffce9cc7114280");
        assert_eq!(snake("PreToolUse"), "pre_tool_use");
        assert_eq!(snake("Stop"), "stop");
        assert!(trust_hash("Stop", CMD).starts_with("sha256:"));
        // Codex caps these two at 3 seconds and trusts what it capped: so do we.
        let mut s = Map::new();
        set_hooks(&mut s, CODEX_EVENTS, CMD, true).unwrap();
        assert_eq!(s["hooks"]["Interrupt"][0]["hooks"][0]["timeout"], 3);
        assert_eq!(s["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["timeout"], 5);
    }

    #[test]
    fn trust_entries_are_added_kept_and_removed() {
        let prefix = "/home/adib/.codex/hooks.json:";
        let others = "model = \"gpt-5\"\n\n[features]\nhooks = true\n\n[hooks.state]\n\n[hooks.state.\"/home/adib/.codex/hooks.json:session_start:0:0\"]\ntrusted_hash = \"sha256:herdr\"\n# <<< zennotes mcp end (managed) <<<\n";
        let ours: HashSet<String> = ["sha256:a".to_string(), "sha256:b".to_string(), "sha256:old".to_string()].into();
        let wanted = vec![(format!("{prefix}session_start:2:0"), "sha256:a".to_string()), (format!("{prefix}stop:0:0"), "sha256:b".to_string())];
        let on = trust(others, prefix, &wanted, &ours).unwrap().unwrap();
        assert!(on.starts_with(others.trim_end()), "{on}");
        assert!(on.contains("[hooks.state.\"/home/adib/.codex/hooks.json:session_start:2:0\"]\ntrusted_hash = \"sha256:a\"\n"));
        assert!(on.contains("[hooks.state.\"/home/adib/.codex/hooks.json:stop:0:0\"]\ntrusted_hash = \"sha256:b\"\n"));
        // Again: nothing to do.
        assert_eq!(trust(&on, prefix, &wanted, &ours).unwrap(), None);
        // Moved (another tool's group came before ours): the old place goes, the new one comes.
        let moved = vec![(format!("{prefix}session_start:3:0"), "sha256:a".to_string()), (format!("{prefix}stop:0:0"), "sha256:b".to_string())];
        let after = trust(&on, prefix, &moved, &ours).unwrap().unwrap();
        assert!(!after.contains("session_start:2:0") && after.contains("session_start:3:0"));
        // A hook the user turned off in Codex stays off; only the hash changes.
        let disabled = on.replace("trusted_hash = \"sha256:b\"", "trusted_hash = \"sha256:old\"\nenabled = false");
        let fixed = trust(&disabled, prefix, &wanted, &ours).unwrap().unwrap();
        assert!(fixed.contains("trusted_hash = \"sha256:b\"\nenabled = false"), "{fixed}");
        // Off: back to exactly what it was.
        let off = trust(&on, prefix, &[], &ours).unwrap().unwrap();
        assert_eq!(off, others);
        // Hooks kept inline: left alone (a feature flag called hooks is fine).
        assert!(trust("hooks = { state = {} }\n", prefix, &wanted, &ours).is_err());
        assert!(trust("hooks.state.x.trusted_hash = \"a\"\n", prefix, &wanted, &ours).is_err());
        assert!(trust("[features]\nhooks = true\n[mcp]\nhooks = 1\n", prefix, &wanted, &ours).is_ok());
        assert!(trust("[hooks]\nstate = {}\n", prefix, &wanted, &ours).is_err());
        // An empty file gets ours alone.
        assert_eq!(trust("", prefix, &wanted[1..], &ours).unwrap().unwrap(), "[hooks.state.\"/home/adib/.codex/hooks.json:stop:0:0\"]\ntrusted_hash = \"sha256:b\"\n");
        assert_eq!(state_key("[hooks.state.\"a\\\"b\"]").as_deref(), Some("a\"b"));
    }

    #[cfg(unix)]
    #[test]
    fn installs_into_scratch_settings_and_removes_again() {
        let root = std::env::temp_dir().join(format!("muxin-hooks-{}", std::process::id()));
        let (claude, codex) = (root.join("claude"), root.join("codex"));
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        // Settings kept in dotfiles: a symlink stays one.
        let real = root.join("dotfiles-settings.json");
        let original = serde_json::to_string_pretty(&Value::Object(rex_and_herdr())).unwrap() + "\n";
        std::fs::write(&real, &original).unwrap();
        std::os::unix::fs::symlink(&real, claude.join("settings.json")).unwrap();
        let config = "model = \"gpt-5\"\n";
        std::fs::write(codex.join("config.toml"), config).unwrap();
        let dirs = Dirs { claude: Some(claude.clone()), codex: Some(codex.clone()) };
        assert_eq!(apply(true, &dirs), json!({ "claude": "on", "codex": "on" }));
        assert_eq!(status(&dirs), json!({ "claude": "on", "codex": "on" }));
        assert!(std::fs::symlink_metadata(claude.join("settings.json")).unwrap().file_type().is_symlink());
        let toml = std::fs::read_to_string(codex.join("config.toml")).unwrap();
        assert_eq!(toml.matches("trusted_hash").count(), CODEX_EVENTS.len(), "{toml}");
        assert!(toml.contains(&format!("{}:stop:0:0", codex.join("hooks.json").display())));
        assert_eq!(apply(false, &dirs), json!({ "claude": "off", "codex": "off" }));
        assert_eq!(std::fs::read_to_string(&real).unwrap(), original);
        assert_eq!(std::fs::read_to_string(codex.join("config.toml")).unwrap(), config);
        // Codex's hooks.json had nothing else: none is left.
        assert!(!codex.join("hooks.json").exists());
        // Not set up: nothing made.
        let missing = Dirs { claude: Some(root.join("nope")), codex: None };
        assert_eq!(apply(true, &missing), json!({ "claude": "none", "codex": "none" }));
        let _ = std::fs::remove_dir_all(&root);
    }
}
