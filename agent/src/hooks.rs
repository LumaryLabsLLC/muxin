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
//! - Copilot CLI: a file of Muxin's own, `hooks/muxin.json` in `COPILOT_HOME`
//!   (else `~/.copilot`).
//! - Qwen Code: `settings.json` in `QWEN_HOME` (else `~/.qwen`), in Claude
//!   Code's format.
//! - Kimi Code: `[[hooks]]` tables in `config.toml` in `KIMI_CODE_HOME` (else
//!   `~/.kimi-code`).
//!
//! Droid and Cursor Agent have hooks too, but they weren't tried yet (both
//! need an account to run a turn), so they're left to the screen guess.

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
/// Copilot CLI's, by their Claude Code names: those get Claude Code's fields
/// (its camelCase ones get others). The event rides along in the command, as
/// its PermissionRequest doesn't say which it is.
const COPILOT_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PermissionRequest", "Notification", "Stop", "SessionEnd"];
/// Qwen Code's (tried with 0.26).
const QWEN_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PermissionRequest", "Notification", "Stop", "SessionEnd"];
/// Kimi Code's (tried with 2.5).
const KIMI_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PermissionRequest", "Notification", "Stop", "Interrupt", "SessionEnd"];

/// Where the agent CLIs keep their settings, as the user's sessions see it.
#[derive(Debug, Clone, Default)]
pub struct Dirs {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
    pub copilot: Option<PathBuf>,
    pub qwen: Option<PathBuf>,
    pub kimi: Option<PathBuf>,
}

impl Dirs {
    /// From the login shell's variables (`var`: `CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME`, ...; empty is not set), else the defaults under `home`.
    pub fn new(home: &Path, var: impl Fn(&str) -> Option<String>) -> Dirs {
        let set = |name: &str| var(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).map(|v| crate::sys::expand_home(&v));
        Dirs {
            claude: Some(set("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"))),
            // Codex itself resolves a CODEX_HOME it's given (its trust keys use that path).
            codex: Some(set("CODEX_HOME").map(|p| std::fs::canonicalize(&p).unwrap_or(p)).unwrap_or_else(|| home.join(".codex"))),
            copilot: Some(set("COPILOT_HOME").unwrap_or_else(|| home.join(".copilot"))),
            qwen: Some(set("QWEN_HOME").unwrap_or_else(|| home.join(".qwen"))),
            kimi: Some(set("KIMI_CODE_HOME").unwrap_or_else(|| home.join(".kimi-code"))),
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
    command_for(agent, agent_path, None)
}

/// `command`, naming `event` for a CLI whose hooks don't always say which
/// event they are.
fn command_for(agent: &str, agent_path: &Path, event: Option<&str>) -> String {
    let path = agent_path.display().to_string();
    let quoted = if path.chars().any(|c| matches!(c, '"' | '$' | '`' | '\\' | '\'' | '!')) { sh_quote(&path) } else { format!("\"{path}\"") };
    let event = event.map(|e| format!(" {e}")).unwrap_or_default();
    let inner = if agent == "claude" {
        // Claude's desktop app runs Claude Code too, outside Muxin's sessions (apps.rs).
        format!("[ -n \"$MUXEN_SESSION\" ] || [ \"$CLAUDE_CODE_ENTRYPOINT\" = claude-desktop ] || exit 0; {quoted} report {agent}{event} 2>/dev/null || true")
    } else {
        format!("[ -z \"$MUXEN_SESSION\" ] || {quoted} report {agent}{event} 2>/dev/null || true")
    };
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
        return json!({ "claude": "none", "codex": "none", "copilot": "none", "qwen": "none", "kimi": "none" });
    }
    let agent = agent_path();
    let each = |dir: &Option<PathBuf>, change: &dyn Fn(&Path) -> Result<(), String>| match dir {
        Some(dir) if dir.is_dir() => match change(dir) {
            Ok(()) => if on { "on" } else { "off" }.to_string(),
            Err(e) => e,
        },
        _ => "none".to_string(),
    };
    let json_hooks = |agent_id: &str, events: &'static [&'static str]| {
        let command = command(agent_id, &agent);
        move |dir: &Path| edit_json(&dir.join("settings.json"), false, |root| set_hooks(root, events, &command, on)).map(|_| ())
    };
    let mut out = Map::new();
    out.insert("claude".into(), json!(each(&dirs.claude, &json_hooks("claude", CLAUDE_EVENTS))));
    out.insert("codex".into(), json!(each(&dirs.codex, &|dir| codex(dir, &command("codex", &agent), on))));
    out.insert("copilot".into(), json!(each(&dirs.copilot, &|dir| copilot(dir, &agent, on))));
    out.insert("qwen".into(), json!(each(&dirs.qwen, &json_hooks("qwen", QWEN_EVENTS))));
    out.insert("kimi".into(), json!(each(&dirs.kimi, &|dir| kimi(dir, &command("kimi", &agent), on))));
    Value::Object(out)
}

/// Copilot CLI: a hooks file of Muxin's own, or none.
fn copilot(dir: &Path, agent: &Path, on: bool) -> Result<(), String> {
    let file = dir.join("hooks").join("muxin.json");
    let before = std::fs::read_to_string(&file).ok();
    if let Some(text) = &before {
        let commands = serde_json::from_str::<Value>(text).map(|v| copilot_commands(&v)).unwrap_or_default();
        if commands.is_empty() || !commands.iter().all(|c| is_ours(c)) {
            return Err(format!("{} isn't Muxin's, so it was left alone", file.display()));
        }
    }
    if !on {
        if before.is_some() {
            std::fs::remove_file(&file).map_err(|e| format!("couldn't remove {}: {e}", file.display()))?;
        }
        return Ok(());
    }
    let hooks: Map<String, Value> = COPILOT_EVENTS
        .iter()
        .map(|e| (e.to_string(), json!([{ "type": "command", "bash": command_for("copilot", agent, Some(e)), "timeoutSec": timeout(e) }])))
        .collect();
    let text = serde_json::to_string_pretty(&json!({ "version": 1, "hooks": hooks })).unwrap_or_default() + "\n";
    if before.as_deref() == Some(text.as_str()) {
        return Ok(());
    }
    std::fs::create_dir_all(dir.join("hooks")).map_err(|e| format!("couldn't make {}: {e}", dir.join("hooks").display()))?;
    write_keeping(&file, &text)
}

/// The commands in a Copilot CLI hooks file.
fn copilot_commands(v: &Value) -> Vec<String> {
    let lists: Vec<&Value> = v["hooks"].as_object().map(|h| h.values().collect()).unwrap_or_default();
    lists
        .iter()
        .flat_map(|l| l.as_array().into_iter().flatten())
        .flat_map(|h| [&h["bash"], &h["command"], &h["powershell"]])
        .filter_map(|c| c.as_str().map(str::to_string))
        .collect()
}

/// Kimi Code: Muxin's `[[hooks]]` tables in its config.toml.
fn kimi(dir: &Path, command: &str, on: bool) -> Result<(), String> {
    let config = dir.join("config.toml");
    let text = match std::fs::read_to_string(&config) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("couldn't read {}: {e}", config.display())),
    };
    match kimi_hooks(&text, command, on)? {
        // Only ours were in it (Muxin made it): none, as before.
        Some(new) if new.trim().is_empty() => {
            let real = std::fs::canonicalize(&config).unwrap_or(config);
            std::fs::remove_file(&real).map_err(|e| format!("couldn't remove {}: {e}", real.display()))
        }
        Some(new) => write_keeping(&config, &new),
        None => Ok(()),
    }
}

/// Kimi Code's config.toml with Muxin's `[[hooks]]` tables for `KIMI_EVENTS`
/// at its end (or none) and everything else as it was; `None` when it's
/// right already. Kimi refuses a hook with any other field, so they hold
/// only `event`, `command` and `timeout`. Leaves a file alone that sets its
/// hooks another way (`hooks = [...]`).
fn kimi_hooks(text: &str, command: &str, on: bool) -> Result<Option<String>, String> {
    let lines: Vec<&str> = text.lines().collect();
    // The lines inside a multi-line string, where `[` starts no table.
    let mut in_string = vec![false; lines.len()];
    let mut open: Option<&str> = None;
    for (i, l) in lines.iter().enumerate() {
        in_string[i] = open.is_some();
        let mut rest = *l;
        loop {
            let next = match open {
                Some(q) => rest.find(q).map(|at| (at, q)),
                None => ["\"\"\"", "'''"].into_iter().filter_map(|q| rest.find(q).map(|at| (at, q))).min_by_key(|(at, _)| *at),
            };
            let Some((at, q)) = next else { break };
            open = if open.is_some() { None } else { Some(q) };
            rest = &rest[at + 3..];
        }
    }
    let header = |i: usize| !in_string[i] && lines[i].trim_start().starts_with('[');
    for (i, l) in lines.iter().enumerate() {
        if header(i) {
            break;
        }
        let t = l.trim_start();
        if !in_string[i] && t.strip_prefix("hooks").is_some_and(|r| r.trim_start().starts_with(['=', '.'])) {
            return Err("Kimi Code's config.toml sets its hooks in a way Muxin doesn't change, so it was left alone".into());
        }
    }
    // Each [[hooks]] table of ours goes: its header and its lines up to the next header.
    let mut kept: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !header(i) {
            kept.push(lines[i]);
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < lines.len() && !header(end) {
            end += 1;
        }
        // Blank lines and comments at its end belong to what follows.
        let mut body_end = end;
        while body_end > i + 1 && (lines[body_end - 1].trim().is_empty() || lines[body_end - 1].trim_start().starts_with('#')) {
            body_end -= 1;
        }
        let ours = lines[i].trim() == "[[hooks]]" && lines[i + 1..body_end].iter().any(|l| l.trim_start().starts_with("command") && is_ours(l));
        kept.extend(&lines[if ours { body_end } else { i }..end]);
        i = end;
    }
    // Don't leave a run of blank lines where tables went.
    let mut out: Vec<String> = Vec::new();
    for l in kept {
        if l.trim().is_empty() && out.last().is_some_and(|p| p.trim().is_empty()) {
            continue;
        }
        out.push(l.to_string());
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    if on {
        for event in KIMI_EVENTS {
            if !out.is_empty() {
                out.push(String::new());
            }
            out.push("[[hooks]]".into());
            out.push(format!("event = {}", toml_string(event)));
            out.push(format!("command = {}", toml_string(command)));
            out.push(format!("timeout = {}", timeout(event)));
        }
    }
    let mut new = out.join("\n");
    if !new.is_empty() {
        new.push('\n');
    }
    Ok((new != text).then_some(new))
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
    let dirs = Dirs::new(&crate::sys::user_home(), |name| std::env::var(name).ok());
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
    let copilot = match &dirs.copilot {
        Some(d) if d.is_dir() => {
            let v = std::fs::read_to_string(d.join("hooks").join("muxin.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
            if v.is_some_and(|v| COPILOT_EVENTS.iter().all(|e| v["hooks"][*e].as_array().is_some_and(|l| !l.is_empty()))) { "on" } else { "off" }
        }
        _ => "none",
    };
    let kimi = match &dirs.kimi {
        Some(d) if d.is_dir() => {
            let text = std::fs::read_to_string(d.join("config.toml")).unwrap_or_default();
            let ours = text.lines().filter(|l| l.trim_start().starts_with("command") && is_ours(l)).count();
            if ours >= KIMI_EVENTS.len() { "on" } else { "off" }
        }
        _ => "none",
    };
    json!({
        "claude": one(&dirs.claude, "settings.json", CLAUDE_EVENTS),
        "codex": one(&dirs.codex, "hooks.json", CODEX_EVENTS),
        "copilot": copilot,
        "qwen": one(&dirs.qwen, "settings.json", QWEN_EVENTS),
        "kimi": kimi,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "/bin/sh -c '[ -n \"$MUXEN_SESSION\" ] || [ \"$CLAUDE_CODE_ENTRYPOINT\" = claude-desktop ] || exit 0; \"/home/adib/.muxen/bin/muxen-agent\" report claude 2>/dev/null || true'";

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
        for (session, entrypoint, expect) in [("", "", ""), ("claude-1", "", "report claude"), ("", "claude-desktop", "report claude"), ("", "cli", "")] {
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(command("claude", Path::new("/bin/echo"))).env("MUXEN_SESSION", session).env("CLAUDE_CODE_ENTRYPOINT", entrypoint).output().unwrap();
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expect, "{session:?} {entrypoint:?}");
        }
        // The other CLIs: only in a session.
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(command("codex", Path::new("/bin/echo"))).env("MUXEN_SESSION", "").env("CLAUDE_CODE_ENTRYPOINT", "claude-desktop").output().unwrap();
        assert!(out.status.success() && out.stdout.is_empty());
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
        let dirs = Dirs { claude: Some(claude.clone()), codex: Some(codex.clone()), ..Default::default() };
        let none = json!({ "copilot": "none", "qwen": "none", "kimi": "none" });
        let with = |a: &str, b: &str| {
            let mut v = none.clone();
            v["claude"] = json!(a);
            v["codex"] = json!(b);
            v
        };
        assert_eq!(apply(true, &dirs), with("on", "on"));
        assert_eq!(status(&dirs), with("on", "on"));
        assert!(std::fs::symlink_metadata(claude.join("settings.json")).unwrap().file_type().is_symlink());
        let toml = std::fs::read_to_string(codex.join("config.toml")).unwrap();
        assert_eq!(toml.matches("trusted_hash").count(), CODEX_EVENTS.len(), "{toml}");
        assert!(toml.contains(&format!("{}:stop:0:0", codex.join("hooks.json").display())));
        assert_eq!(apply(false, &dirs), with("off", "off"));
        assert_eq!(std::fs::read_to_string(&real).unwrap(), original);
        assert_eq!(std::fs::read_to_string(codex.join("config.toml")).unwrap(), config);
        // Codex's hooks.json had nothing else: none is left.
        assert!(!codex.join("hooks.json").exists());
        // Not set up: nothing made.
        let missing = Dirs { claude: Some(root.join("nope")), ..Default::default() };
        assert_eq!(apply(true, &missing), with("none", "none"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copilot_gets_a_file_of_its_own_naming_each_event() {
        let root = std::env::temp_dir().join(format!("muxin-hooks-copilot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let agent = Path::new("/home/adib/.muxen/bin/muxen-agent");
        copilot(&root, agent, true).unwrap();
        let file = root.join("hooks").join("muxin.json");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        for e in COPILOT_EVENTS {
            let h = &v["hooks"][*e][0];
            assert_eq!((h["type"].as_str(), h["timeoutSec"].as_u64()), (Some("command"), Some(timeout(e))));
            let bash = h["bash"].as_str().unwrap();
            assert!(is_ours(bash) && bash.contains(&format!(" report copilot {e} ")), "{bash}");
        }
        // Again: the same file.
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        copilot(&root, agent, true).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), before);
        // Off: gone, and the folder Copilot made stays.
        copilot(&root, agent, false).unwrap();
        assert!(!file.exists() && root.join("hooks").is_dir());
        copilot(&root, agent, false).unwrap();
        // A file of the user's own with that name stays.
        std::fs::write(&file, r#"{"version":1,"hooks":{"Stop":[{"type":"command","bash":"say done"}]}}"#).unwrap();
        assert!(copilot(&root, agent, true).is_err());
        assert!(copilot(&root, agent, false).is_err());
        assert!(std::fs::read_to_string(&file).unwrap().contains("say done"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kimi_tables_go_at_the_end_and_leave_again() {
        let cmd = command("kimi", Path::new("/home/adib/.muxen/bin/muxen-agent"));
        let theirs = "default_model = \"kimi-k3\"\n\n[providers.kimi]\ntype = \"kimi\"\napi_key = \"sk-x\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"notify-send done\"\ntimeout = 5\n# their note\n\n[tui]\nprompt = \"\"\"\n[[hooks]]\nnot a table\n\"\"\"\n";
        let on = kimi_hooks(theirs, &cmd, true).unwrap().unwrap();
        assert!(on.starts_with(theirs.trim_end()), "{on}");
        assert_eq!(on.matches("[[hooks]]").count(), 2 + KIMI_EVENTS.len());
        let ours = &on[theirs.trim_end().len()..];
        for e in KIMI_EVENTS {
            assert!(ours.contains(&format!("[[hooks]]\nevent = \"{e}\"\ncommand = {}\ntimeout = {}\n", toml_string(&cmd), timeout(e))), "{e}:\n{ours}");
        }
        // Only the three fields Kimi allows.
        assert!(ours.lines().filter(|l| !l.is_empty() && *l != "[[hooks]]").all(|l| ["event = ", "command = ", "timeout = "].iter().any(|k| l.starts_with(k))));
        // Again: nothing to do.
        assert_eq!(kimi_hooks(&on, &cmd, true).unwrap(), None);
        // Off: as it was. And the hooks of an older agent path go too.
        assert_eq!(kimi_hooks(&on, &cmd, false).unwrap().unwrap(), theirs);
        let old = on.replace("/home/adib/.muxen/bin", "/old/.muxen/bin");
        let moved = kimi_hooks(&old, &cmd, true).unwrap().unwrap();
        assert_eq!(moved, on);
        // Ours in the middle (Kimi wrote the file again): moved to the end.
        let middle = format!("{}\n[tui]\ntheme = \"dark\"\n", on.trim_end());
        let tidied = kimi_hooks(&middle, &cmd, true).unwrap().unwrap();
        assert!(tidied.ends_with(&format!("command = {}\ntimeout = 3\n", toml_string(&cmd))), "{tidied}");
        assert!(tidied.contains("[tui]\ntheme = \"dark\""));
        // An empty file gets ours alone; hooks as a key are left alone.
        assert!(kimi_hooks("", &cmd, true).unwrap().unwrap().starts_with("[[hooks]]\nevent = \"SessionStart\""));
        assert!(kimi_hooks("hooks = []\n", &cmd, true).is_err());
        assert!(kimi_hooks("[tui]\nhooks = 1\n", &cmd, true).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn qwen_and_kimi_in_scratch_folders() {
        let root = std::env::temp_dir().join(format!("muxin-hooks-qk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (qwen, kimi_dir, copilot_dir) = (root.join("qwen"), root.join("kimi"), root.join("copilot"));
        for d in [&qwen, &kimi_dir, &copilot_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        let settings = "{\n  \"model\": {\n    \"name\": \"qwen3-coder\"\n  }\n}\n";
        std::fs::write(qwen.join("settings.json"), settings).unwrap();
        let dirs = Dirs { qwen: Some(qwen.clone()), kimi: Some(kimi_dir.clone()), copilot: Some(copilot_dir.clone()), ..Default::default() };
        assert_eq!(apply(true, &dirs), json!({ "claude": "none", "codex": "none", "copilot": "on", "qwen": "on", "kimi": "on" }));
        assert_eq!(status(&dirs), json!({ "claude": "none", "codex": "none", "copilot": "on", "qwen": "on", "kimi": "on" }));
        let s: Value = serde_json::from_str(&std::fs::read_to_string(qwen.join("settings.json")).unwrap()).unwrap();
        assert_eq!(s["model"]["name"], "qwen3-coder");
        assert!(s["hooks"]["PermissionRequest"][0]["hooks"][0]["command"].as_str().is_some_and(|c| c.contains(" report qwen ")));
        assert_eq!(apply(false, &dirs), json!({ "claude": "none", "codex": "none", "copilot": "off", "qwen": "off", "kimi": "off" }));
        assert_eq!(std::fs::read_to_string(qwen.join("settings.json")).unwrap(), settings);
        // Kimi had no config.toml: none is left.
        assert!(!kimi_dir.join("config.toml").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
