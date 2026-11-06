//! What the user chose for the agents on this machine, in Settings › Agents
//! of any of their Muxins: `hooks` (hooks.rs) and `resume`, whether agents
//! continue their conversations after a restart (daemon.rs); starting
//! helpers later. Each app sends its own choices with when they
//! were made, `{"hooks": {"on": false, "at": 1793979274}}` (0: a default
//! nobody changed). The newest wins and is kept here, `~/.muxen/settings.json`,
//! and goes back in `discover`'s reply: so a Mac and an iPad agree, and a
//! machine that was away when a choice was made gets it once it's back.

use serde_json::{Map, Value, json};

fn path() -> std::path::PathBuf {
    crate::home_dir().join("settings.json")
}

fn at(v: &Value) -> i64 {
    v["at"].as_i64().unwrap_or(0)
}

/// The choices in effect, given what's kept and what an app sent; and what's
/// to keep now, when that changed. A choice someone made (`at` > 0) is kept;
/// a default is only used.
fn merge_with(kept: &Map<String, Value>, sent: &Map<String, Value>) -> (Map<String, Value>, Option<Map<String, Value>>) {
    let mut now = kept.clone();
    let mut changed = false;
    let mut out = Map::new();
    for (key, choice) in sent {
        if !choice["on"].is_boolean() {
            continue;
        }
        let newer = kept.get(key).is_none_or(|k| at(choice) > at(k));
        if newer && at(choice) > 0 {
            now.insert(key.clone(), json!({ "on": choice["on"], "at": at(choice) }));
            changed = true;
        }
        let used = if newer { choice } else { &kept[key] };
        out.insert(key.clone(), json!({ "on": used["on"], "at": at(used) }));
    }
    // Ones this app doesn't know (a newer Muxin's): kept, and said.
    for (key, choice) in kept {
        out.entry(key.clone()).or_insert_with(|| choice.clone());
    }
    (out, changed.then_some(now))
}

/// The choices kept here (what the user chose in any of their Muxins).
pub fn kept() -> Value {
    Value::Object(read_kept())
}

fn read_kept() -> Map<String, Value> {
    std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

/// Merges an app's choices (`discover`'s `"settings"`) into the kept ones; the
/// choices in effect.
pub fn merge(sent: &Value) -> Value {
    let kept = read_kept();
    let sent = sent.as_object().cloned().unwrap_or_default();
    let (out, keep) = merge_with(&kept, &sent);
    if let Some(keep) = keep {
        let tmp = path().with_extension(format!("json.{}", std::process::id()));
        if std::fs::write(&tmp, Value::Object(keep).to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, path());
        }
    }
    Value::Object(out)
}

/// Whether a choice is on (its default when nobody said).
pub fn on(choices: &Value, key: &str, default: bool) -> bool {
    choices[key]["on"].as_bool().unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn the_newest_choice_wins_and_defaults_dont_override_one() {
        // Nothing kept, a default: used, not kept.
        let (out, keep) = merge_with(&Map::new(), &map(json!({ "hooks": { "on": true, "at": 0 } })));
        assert_eq!((on(&Value::Object(out), "hooks", false), keep), (true, None));
        // Someone turned them off: kept.
        let (out, keep) = merge_with(&Map::new(), &map(json!({ "hooks": { "on": false, "at": 100 } })));
        assert!(!on(&Value::Object(out), "hooks", true));
        let kept = keep.unwrap();
        // Another app with the default (never changed there): the choice stands, and it hears of it.
        let (out, keep) = merge_with(&kept, &map(json!({ "hooks": { "on": true, "at": 0 } })));
        assert_eq!(out["hooks"], json!({ "on": false, "at": 100 }));
        assert!(keep.is_none());
        // A newer choice from it: that one.
        let (out, keep) = merge_with(&kept, &map(json!({ "hooks": { "on": true, "at": 200 } })));
        assert_eq!(out["hooks"], json!({ "on": true, "at": 200 }));
        assert_eq!(keep.unwrap()["hooks"]["at"], 200);
        // An older one changes nothing.
        let (out, _) = merge_with(&kept, &map(json!({ "hooks": { "on": true, "at": 50 } })));
        assert_eq!(out["hooks"]["on"], false);
        // A choice only a newer Muxin knows is said back as it is; nonsense is ignored.
        let kept = map(json!({ "resume": { "on": false, "at": 9 } }));
        let (out, _) = merge_with(&kept, &map(json!({ "hooks": { "on": "yes" } })));
        assert_eq!(out.get("resume"), Some(&json!({ "on": false, "at": 9 })));
        assert!(out.get("hooks").is_none());
    }
}
