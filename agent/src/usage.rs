//! What the agent CLIs on this machine used: tokens by hour, model and
//! folder, from their own logs, so the app can say what agents cost.
//! - Claude Code: `~/.claude/projects/*/*.jsonl`. Each response is logged
//!   once per content block with the same usage: counted once, by message
//!   and request id.
//! - Codex: `~/.codex/sessions/**/rollout-*.jsonl`, its `token_count` events
//!   (the last response's tokens), with the model from the turn before.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Tokens, as they're priced: fresh input, cache reads, cache writes (5
/// minutes and an hour), output.
#[derive(Default, Clone, Copy)]
struct Sum {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cache_write_1h: u64,
    requests: u64,
}

impl Sum {
    fn add(&mut self, o: &Sum) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cache_write_1h += o.cache_write_1h;
        self.requests += o.requests;
    }

    fn json(&self) -> Value {
        json!({ "input": self.input, "output": self.output, "cache_read": self.cache_read, "cache_write": self.cache_write, "cache_write_1h": self.cache_write_1h, "requests": self.requests })
    }
}

#[derive(Default)]
struct Tally {
    /// (hour "YYYY-MM-DDTHH" UTC, tool, model).
    hours: HashMap<(String, &'static str, String), Sum>,
    /// (tool, folder): its tokens, last use and models.
    folders: HashMap<(&'static str, String), (Sum, String, HashSet<String>)>,
}

impl Tally {
    fn add(&mut self, at: &str, tool: &'static str, model: &str, cwd: &str, sum: Sum) {
        let hour = at.get(..13).unwrap_or(at).to_string();
        self.hours.entry((hour, tool, model.to_string())).or_default().add(&sum);
        let f = self.folders.entry((tool, cwd.to_string())).or_default();
        f.0.add(&sum);
        if at > f.1.as_str() {
            f.1 = at.to_string();
        }
        f.2.insert(model.to_string());
    }
}

/// `{"op": "usage", "days"?}`: tokens the agent CLIs here used in the last
/// `days` (30 by default), by hour (UTC), tool and model, and by folder.
pub fn usage(req: &Value) -> Value {
    let days = req["days"].as_u64().unwrap_or(30).clamp(1, 400);
    let since = SystemTime::now() - Duration::from_secs(days * 86_400);
    let cutoff = utc_hour(since);
    // The user's home (crate::home_dir is the agent's own ~/.muxen).
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut tally = Tally::default();
    let mut found = Vec::new();

    let claude_dirs = [std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from), Some(home.join(".claude")), Some(home.join(".config/claude"))];
    let mut seen = HashSet::new();
    for dir in claude_dirs.into_iter().flatten() {
        let files = jsonl_files(&dir.join("projects"), 2, since);
        if !files.is_empty() && !found.contains(&"claude") {
            found.push("claude");
        }
        for f in files {
            claude_file(&f, &cutoff, &mut seen, &mut tally);
        }
    }
    let codex = jsonl_files(&home.join(".codex/sessions"), 4, since);
    if !codex.is_empty() {
        found.push("codex");
    }
    for f in codex {
        codex_file(&f, &cutoff, &mut tally);
    }

    let mut hours: Vec<Value> = tally
        .hours
        .iter()
        .map(|((hour, tool, model), s)| {
            let mut v = s.json();
            v["hour"] = json!(hour);
            v["tool"] = json!(tool);
            v["model"] = json!(model);
            v
        })
        .collect();
    hours.sort_by(|a, b| a["hour"].as_str().cmp(&b["hour"].as_str()));
    let mut folders: Vec<Value> = tally
        .folders
        .iter()
        .map(|((tool, cwd), (s, last, models))| {
            let mut v = s.json();
            v["tool"] = json!(tool);
            v["cwd"] = json!(cwd);
            v["last"] = json!(last);
            let mut m: Vec<&String> = models.iter().collect();
            m.sort();
            v["models"] = json!(m);
            v
        })
        .collect();
    folders.sort_by(|a, b| b["last"].as_str().cmp(&a["last"].as_str()));
    json!({ "ok": true, "days": days, "tools": found, "hours": hours, "folders": folders })
}

/// `.jsonl` files under `dir` (down to `depth` folders) changed since `since`.
fn jsonl_files(dir: &Path, depth: usize, since: SystemTime) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            if depth > 1 {
                out.extend(jsonl_files(&path, depth - 1, since));
            }
        } else if path.extension().is_some_and(|x| x == "jsonl") && meta.modified().is_ok_and(|m| m >= since) {
            out.push(path);
        }
    }
    out
}

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn claude_file(path: &Path, cutoff: &str, seen: &mut HashSet<String>, tally: &mut Tally) {
    let Ok(file) = std::fs::File::open(path) else { return };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        // A response (with its usage); the fields needed are read straight off
        // the line, which also carries the response's whole text.
        if !line.contains("\"usage\":{") || !line.contains("\"role\":\"assistant\"") {
            continue;
        }
        let at = field(&line, "\"timestamp\":\"").unwrap_or("");
        let model = field(&line, "\"model\":\"").unwrap_or("");
        if at < cutoff || model.is_empty() || model.starts_with('<') {
            continue;
        }
        // The same response, once per content block: count it once.
        let id = line.find("\"id\":\"msg_").map(|i| &line[i + 6..]).and_then(|r| r.split('"').next()).unwrap_or("");
        let key = format!("{id}:{}", field(&line, "\"requestId\":\"").unwrap_or(""));
        if key != ":" && !seen.insert(key) {
            continue;
        }
        let Some(u) = line.find("\"usage\":{").and_then(|i| serde_json::Deserializer::from_str(&line[i + 8..]).into_iter::<Value>().next()).and_then(Result::ok) else { continue };
        let created = n(&u["cache_creation_input_tokens"]);
        let hour = n(&u["cache_creation"]["ephemeral_1h_input_tokens"]).min(created);
        let sum = Sum { input: n(&u["input_tokens"]), output: n(&u["output_tokens"]), cache_read: n(&u["cache_read_input_tokens"]), cache_write: created - hour, cache_write_1h: hour, requests: 1 };
        tally.add(at, "claude", model, field(&line, "\"cwd\":\"").unwrap_or(""), sum);
    }
}

/// A string field's value, found by its key (`"name":"`) in a JSON line.
/// The first one: the record's own (its fields come before its content).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(key)? + key.len();
    let rest = &line[at..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn codex_file(path: &Path, cutoff: &str, tally: &mut Tally) {
    let Ok(file) = std::fs::File::open(path) else { return };
    let (mut cwd, mut model, mut total) = (String::new(), String::new(), 0u64);
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let p = &v["payload"];
        match v["type"].as_str().unwrap_or("") {
            "session_meta" => cwd = p["cwd"].as_str().unwrap_or("").to_string(),
            "turn_context" => {
                model = p["model"].as_str().unwrap_or(&model).to_string();
                if let Some(c) = p["cwd"].as_str() {
                    cwd = c.to_string();
                }
            }
            "event_msg" if p["type"] == "token_count" => {
                let info = &p["info"];
                if info.is_null() {
                    continue;
                }
                // Repeated with nothing new (a rate-limit update): skip.
                let now = n(&info["total_token_usage"]["total_tokens"]);
                if now == total {
                    continue;
                }
                total = now;
                let at = v["timestamp"].as_str().unwrap_or("");
                if at < cutoff {
                    continue;
                }
                let u = &info["last_token_usage"];
                let cached = n(&u["cached_input_tokens"]);
                let sum = Sum {
                    input: n(&u["input_tokens"]).saturating_sub(cached),
                    output: n(&u["output_tokens"]),
                    cache_read: cached,
                    cache_write: n(&u["cache_write_input_tokens"]),
                    cache_write_1h: 0,
                    requests: 1,
                };
                tally.add(at, "codex", if model.is_empty() { "unknown" } else { &model }, &cwd, sum);
            }
            _ => {}
        }
    }
}

/// A time as "YYYY-MM-DDTHH" in UTC (how the logs' times start).
fn utc_hour(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, hour) = (secs / 86_400, (secs % 86_400) / 3600);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_utc_hours() {
        assert_eq!(utc_hour(UNIX_EPOCH), "1970-01-01T00");
        assert_eq!(utc_hour(UNIX_EPOCH + Duration::from_secs(1_791_220_000)), "2026-10-05T17");
        assert_eq!(utc_hour(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00");
    }

    #[test]
    fn counts_claude_once_and_codex_by_turn() {
        let dir = std::env::temp_dir().join(format!("muxen-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let claude = dir.join("c.jsonl");
        let line = r#"{"type":"assistant","timestamp":"2026-10-12T17:00:01.000Z","cwd":"/w","requestId":"r1","message":{"model":"claude-opus-5-5","id":"msg_1","type":"message","role":"assistant","usage":{"input_tokens":5,"output_tokens":100,"cache_read_input_tokens":1000,"cache_creation_input_tokens":300,"cache_creation":{"ephemeral_1h_input_tokens":200}}}}"#;
        std::fs::write(&claude, format!("{line}\n{line}\n{{\"type\":\"user\"}}\n")).unwrap();
        let codex = dir.join("x.jsonl");
        std::fs::write(
            &codex,
            concat!(
                r#"{"timestamp":"2026-10-12T18:00:00Z","type":"session_meta","payload":{"cwd":"/w2"}}"#, "\n",
                r#"{"timestamp":"2026-10-12T18:00:01Z","type":"turn_context","payload":{"model":"gpt-6.1-sol"}}"#, "\n",
                r#"{"timestamp":"2026-10-12T18:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":900},"last_token_usage":{"input_tokens":800,"cached_input_tokens":600,"output_tokens":100}}}}"#, "\n",
                r#"{"timestamp":"2026-10-12T18:00:06Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":900},"last_token_usage":{"input_tokens":800,"cached_input_tokens":600,"output_tokens":100}}}}"#, "\n",
            ),
        )
        .unwrap();
        let mut tally = Tally::default();
        let mut seen = HashSet::new();
        claude_file(&claude, "2026-10-01T00", &mut seen, &mut tally);
        codex_file(&codex, "2026-10-01T00", &mut tally);
        let c = tally.hours[&("2026-10-12T17".to_string(), "claude", "claude-opus-5-5".to_string())];
        assert_eq!((c.input, c.output, c.cache_read, c.cache_write, c.cache_write_1h, c.requests), (5, 100, 1000, 100, 200, 1));
        let x = tally.hours[&("2026-10-12T18".to_string(), "codex", "gpt-6.1-sol".to_string())];
        assert_eq!((x.input, x.cache_read, x.output, x.requests), (200, 600, 100, 1));
        assert_eq!(tally.folders[&("codex", "/w2".to_string())].1, "2026-10-12T18:00:05Z");
        // Too old: not counted.
        let mut old = Tally::default();
        claude_file(&claude, "2026-10-13T00", &mut HashSet::new(), &mut old);
        assert!(old.hours.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
