//! The models a brain can answer with, and how hard each can be asked to
//! think: what the chat's two pickers offer.
//!
//! For the programs, nobody is asked over the network. Codex and Grok Build
//! each keep the list their own picker shows in a file beside their
//! settings, kept up to date by the program itself; that file is read.
//! Claude Code has no such list: it takes a model by a short name that
//! always means the newest of its kind, and those names are few.
//!
//! A provider reached with an API key is asked for its list (anthropic.rs,
//! openai.rs) when it's connected and when the user asks again, and what it
//! answered is kept in Muxin's folder (`models/<provider>.json`), so the
//! pickers don't wait on the network or read the key.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A model a brain can answer with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// What the brain is told to use.
    pub id: String,
    /// What it's shown as.
    pub name: String,
    /// How hard it can be asked to think, least first. Empty: it has no such choice.
    pub efforts: Vec<Effort>,
    /// The effort it uses when none is chosen.
    #[serde(default)]
    pub effort: Option<String>,
    /// It can be run a faster way, for more ("fast").
    #[serde(default)]
    pub fast: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effort {
    pub id: String,
    pub name: String,
}

impl Effort {
    pub fn of(id: &str) -> Effort {
        Effort { id: id.to_string(), name: effort_name(id) }
    }
}

/// An effort level in words: "xhigh" is "Extra high".
pub fn effort_name(id: &str) -> String {
    match id {
        "none" => "None".into(),
        "minimal" => "Minimal".into(),
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "Extra high".into(),
        "max" => "Max".into(),
        "ultra" => "Ultra".into(),
        other => {
            let mut letters = other.chars();
            match letters.next() {
                Some(first) => first.to_uppercase().chain(letters).collect(),
                None => String::new(),
            }
        }
    }
}

/// Least to most, for a list that came in another order.
pub(super) fn rank(effort: &str) -> usize {
    ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"].iter().position(|e| *e == effort).unwrap_or(usize::MAX)
}

/// A brain's models, as this computer knows them. Empty when its program
/// hasn't written its list yet.
pub fn of(brain: &str) -> Vec<Model> {
    let home = dirs::home_dir().unwrap_or_default();
    match brain {
        "claude" => claude(),
        "codex" => read(&home.join(".codex/models_cache.json")).map(|list| openai(&list)).unwrap_or_default(),
        "grok" => {
            let grok_home = std::env::var_os("GROK_HOME").map(Into::into).unwrap_or_else(|| home.join(".grok"));
            read(&grok_home.join("models_cache.json")).map(|list| grok(&list)).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn read(file: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

/// Claude Code's short names: each is the newest model of its kind, so the
/// list doesn't age with the models. And the efforts its `--effort` takes.
fn claude() -> Vec<Model> {
    let efforts: Vec<Effort> = ["low", "medium", "high", "xhigh", "max"].map(Effort::of).to_vec();
    [("fable", "Fable"), ("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")]
        .into_iter()
        .map(|(id, name)| Model { id: id.into(), name: name.into(), efforts: efforts.clone(), effort: None, fast: false })
        .collect()
}

/// The list as OpenAI gives it to Codex (its `models_cache.json`):
/// `{"models": [{slug,
/// display_name, visibility, default_reasoning_level,
/// supported_reasoning_levels: [{effort}]}]}`. The ones meant to be shown,
/// in the order given.
fn openai(list: &Value) -> Vec<Model> {
    list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| {
            let id = m["slug"].as_str()?.to_string();
            let mut efforts: Vec<Effort> = m["supported_reasoning_levels"].as_array().into_iter().flatten().filter_map(|l| l["effort"].as_str()).map(Effort::of).collect();
            efforts.sort_by_key(|e| rank(&e.id));
            // A faster tier, where Codex says the model has one.
            let fast = m["additional_speed_tiers"].as_array().is_some_and(|tiers| tiers.iter().any(|t| t == "fast"));
            Some(Model { name: m["display_name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&id).to_string(), id, efforts, effort: m["default_reasoning_level"].as_str().map(str::to_string), fast })
        })
        .collect()
}

/// Grok Build's `models_cache.json`: `{"models": {"<id>": {"info": {id,
/// name, hidden, reasoning_efforts: [{value, label, default}]}}}}`.
fn grok(list: &Value) -> Vec<Model> {
    let Some(models) = list["models"].as_object() else { return Vec::new() };
    models
        .values()
        .map(|m| &m["info"])
        .filter(|info| info["hidden"] != true)
        .filter_map(|info| {
            let id = info["id"].as_str()?.to_string();
            let levels: Vec<&Value> = info["reasoning_efforts"].as_array().into_iter().flatten().collect();
            let mut efforts: Vec<Effort> = levels
                .iter()
                .filter_map(|l| Some(Effort { id: l["value"].as_str()?.to_string(), name: l["label"].as_str().map(str::to_string).unwrap_or_else(|| effort_name(l["value"].as_str().unwrap_or_default())) }))
                .collect();
            efforts.sort_by_key(|e| rank(&e.id));
            let effort = levels.iter().find(|l| l["default"] == true).and_then(|l| l["value"].as_str()).or(info["reasoning_effort"].as_str()).map(str::to_string);
            // Grok's faster models are models of their own, picked like any other.
            Some(Model { name: info["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&id).to_string(), id, efforts, effort, fast: false })
        })
        .collect()
}

/// Where the list a provider answered with is kept, in Muxin's folder.
fn kept_at(dir: &Path, provider: &str) -> PathBuf {
    dir.join("models").join(format!("{provider}.json"))
}

/// A brain's models: a program's as `of` reads them, a provider's as it
/// last answered (`keep`). `dir` is Muxin's folder.
pub fn known(dir: &Path, brain: &str) -> Vec<Model> {
    match super::kind(brain).and_then(|kind| kind.api()) {
        Some(_) => read(&kept_at(dir, brain)).and_then(|list| serde_json::from_value(list).ok()).unwrap_or_default(),
        None => of(brain),
    }
}

/// Keeps the list a provider answered with.
pub fn keep(dir: &Path, provider: &str, models: &[Model]) {
    let path = kept_at(dir, provider);
    if let Some(folder) = path.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    let _ = crate::config::write_atomic(&path, &serde_json::to_vec(models).unwrap_or_default());
}

/// How long ago a provider's list was kept. None when there isn't one.
pub fn age(dir: &Path, provider: &str) -> Option<std::time::Duration> {
    std::fs::metadata(kept_at(dir, provider)).ok()?.modified().ok()?.elapsed().ok()
}

/// Forgets it, when the provider is disconnected.
pub fn forget(dir: &Path, provider: &str) {
    let _ = std::fs::remove_file(kept_at(dir, provider));
}

/// A model by its id, among `models`.
pub fn find<'a>(models: &'a [Model], id: &str) -> Option<&'a Model> {
    models.iter().find(|m| m.id == id)
}

/// The one of `all` the user means by `said`: its id or its name, whatever
/// the letters' case, or the only one that starts with it.
fn meant<'a, T>(all: &'a [T], said: &str, id: impl Fn(&T) -> &str, name: impl Fn(&T) -> &str) -> Option<&'a T> {
    let said = said.trim().to_lowercase();
    if said.is_empty() {
        return None;
    }
    let exact = all.iter().find(|x| id(x).to_lowercase() == said || name(x).to_lowercase() == said);
    exact.or_else(|| {
        let mut starts = all.iter().filter(|x| id(x).to_lowercase().starts_with(&said) || name(x).to_lowercase().starts_with(&said));
        let first = starts.next()?;
        starts.next().is_none().then_some(first)
    })
}

/// The model the user means by `said` ("opus", "GPT-6.1-Sol", "grok-4.7").
pub fn named<'a>(models: &'a [Model], said: &str) -> Option<&'a Model> {
    meant(models, said, |m| &m.id, |m| &m.name)
}

impl Model {
    /// The effort the user means by `said` ("high", "Extra high", "x").
    pub fn effort_named(&self, said: &str) -> Option<&Effort> {
        meant(&self.efforts, said, |e| &e.id, |e| &e.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codexs_list_is_the_models_meant_to_be_shown_with_their_efforts_least_first() {
        // As Codex 0.166 keeps it, shortened.
        let list = json!({"fetched_at": "2026-11-13T10:00:00Z", "models": [
            {"slug": "gpt-6.1-sol", "display_name": "GPT-6.1-Sol", "visibility": "list", "default_reasoning_level": "low", "supported_reasoning_levels": [{"effort": "low", "description": "Fast"}, {"effort": "xhigh"}, {"effort": "medium"}, {"effort": "high"}]},
            {"slug": "gpt-6-chat-thinking", "display_name": "GPT-6 Chat Thinking", "visibility": "hide", "supported_reasoning_levels": [{"effort": "low"}]},
            {"slug": "gpt-6-luna", "display_name": "", "visibility": "list"},
        ]});
        let models = openai(&list);
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-6.1-sol", "gpt-6-luna"]);
        assert_eq!(models[0].name, "GPT-6.1-Sol");
        assert_eq!(models[0].efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["low", "medium", "high", "xhigh"]);
        assert_eq!(models[0].efforts[3].name, "Extra high");
        assert_eq!(models[0].effort.as_deref(), Some("low"));
        // One without a name is shown by its id; one without efforts has no such choice.
        assert_eq!((models[1].name.as_str(), models[1].efforts.len(), models[1].effort.as_deref()), ("gpt-6-luna", 0, None));
        assert!(openai(&json!({})).is_empty());
    }

    #[test]
    fn groks_list_is_read_with_its_own_words_for_each_effort() {
        // As Grok Build 1.0.77 keeps it, shortened.
        let list = json!({"grok_version": "1.0.77", "models": {
            "grok-4.7": {"info": {"id": "grok-4.7", "name": "Grok 4.7", "hidden": false, "reasoning_effort": "high", "reasoning_efforts": [
                {"id": "xhigh", "value": "xhigh", "label": "Extra High", "default": false}, {"id": "high", "value": "high", "label": "High", "default": true}, {"id": "low", "value": "low", "label": "Low", "default": false}]}, "api_key": null},
            "grok-old": {"info": {"id": "grok-old", "name": "Grok Old", "hidden": true}},
        }});
        let models = grok(&list);
        assert_eq!(models.len(), 1);
        assert_eq!((models[0].id.as_str(), models[0].name.as_str(), models[0].effort.as_deref()), ("grok-4.7", "Grok 4.7", Some("high")));
        assert_eq!(models[0].efforts.iter().map(|e| (e.id.as_str(), e.name.as_str())).collect::<Vec<_>>(), [("low", "Low"), ("high", "High"), ("xhigh", "Extra High")]);
        assert!(grok(&json!({"models": []})).is_empty());
    }

    #[test]
    fn claude_code_goes_by_its_short_names() {
        let models = claude();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["fable", "opus", "sonnet", "haiku"]);
        assert_eq!(models[1].efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(find(&models, "sonnet").map(|m| m.name.as_str()), Some("Sonnet"));
        // As the user would say it: by id or name, or the only one that starts that way.
        assert_eq!(named(&models, " OPUS ").map(|m| m.id.as_str()), Some("opus"));
        assert_eq!(named(&models, "son").map(|m| m.id.as_str()), Some("sonnet"));
        assert!(named(&models, "gpt").is_none() && named(&models, "").is_none());
        let opus = &models[1];
        assert_eq!(opus.effort_named("Extra high").map(|e| e.id.as_str()), Some("xhigh"));
        assert_eq!(opus.effort_named("x").map(|e| e.id.as_str()), Some("xhigh"));
        // "m" could be medium or max: neither.
        assert!(opus.effort_named("m").is_none());
        assert!(of("nothing").is_empty());
        assert_eq!((effort_name("minimal"), effort_name("deep"), effort_name("")), ("Minimal".to_string(), "Deep".to_string(), String::new()));
    }
}
