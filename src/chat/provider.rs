//! Connecting a provider with its API key (Settings › AI, `muxin chat
//! --connect`). Connecting asks the provider for its models with the key:
//! that proves the key, and it's the list the pickers offer. Then the key
//! goes where secrets go (keys.rs), the list into Muxin's folder
//! (brain/catalog.rs), and `[settings.chat.providers]` says it's connected.

use std::sync::Arc;

use super::brain::catalog::{self, Model};
use super::brain::{self, Dialect, Kind};
use super::keys;
use crate::fleet::Engine;

/// The provider called `id`: one reached with a key, not a program.
pub fn named(id: &str) -> Result<&'static Kind, String> {
    let known = || brain::KINDS.iter().filter(|k| k.api().is_some()).map(|k| k.id).collect::<Vec<_>>().join(", ");
    match brain::kind(id) {
        Some(kind) if kind.api().is_some() => Ok(kind),
        Some(kind) => Err(format!("{} is a program on this computer, not a provider to connect. The providers are: {}.", kind.name, known())),
        None => Err(format!("\"{id}\" isn't a provider Muxin knows. The providers are: {}.", known())),
    }
}

/// The models `kind` lists at `base`, asked with `key`.
pub async fn list(kind: &Kind, base: &str, key: &str) -> Result<Vec<Model>, String> {
    match kind.api().map(|api| api.dialect) {
        Some(Dialect::Anthropic) => brain::anthropic::models(kind, base, key).await,
        Some(Dialect::OpenAi) => brain::openai::models(kind, base, key).await,
        None => Err(format!("{} isn't reached with a key.", kind.name)),
    }
}

/// Where `kind` is reached: the address given, else its own.
fn address(kind: &Kind, given: &str) -> Result<String, String> {
    let own = kind.api().map(|api| api.base).unwrap_or_default();
    let base = Some(given.trim()).filter(|g| !g.is_empty()).unwrap_or(own).trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err(format!("{} needs the address it's reached at, like https://example.com/v1.", kind.name));
    }
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err(format!("{base} isn't an address. It starts with https://, or http:// for one on this computer."));
    }
    Ok(base)
}

/// Connects the provider `id` with `key`, at `base` when it isn't at its own
/// address ("" otherwise). Nothing is kept unless the provider answers with
/// its models. Gives back that list.
pub async fn connect(engine: &Arc<Engine>, id: &str, key: &str, base: &str) -> Result<Vec<Model>, String> {
    let kind = named(id)?;
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("{} needs its API key.{}", kind.name, if kind.get.is_empty() { String::new() } else { format!(" Get one at {}.", kind.get) }));
    }
    let reached = address(kind, base)?;
    let models = list(kind, &reached, key).await?;
    let dir = engine.paths().dir.clone();
    keys::set(&keys::Place::of(&dir, engine.is_demo()), kind.id, key)?;
    catalog::keep(&dir, kind.id, &models);
    let mut settings = engine.settings();
    // Only an address that isn't the provider's own is kept.
    let own = kind.api().map(|api| api.base.trim_end_matches('/')).unwrap_or_default();
    settings.chat.providers.insert(kind.id.to_string(), crate::config::ChatProvider { base: if reached == own { String::new() } else { reached } });
    engine.update_settings(settings)?;
    Ok(models)
}

/// Asks a connected provider for its models again, and keeps the new list.
pub async fn refresh(engine: &Arc<Engine>, id: &str) -> Result<Vec<Model>, String> {
    let kind = named(id)?;
    let settings = engine.settings().chat;
    let connected = settings.providers.get(kind.id).ok_or_else(|| format!("{} isn't connected.", kind.name))?;
    let dir = engine.paths().dir.clone();
    let key = keys::get(&keys::Place::of(&dir, engine.is_demo()), kind.id).ok_or_else(|| format!("{}'s API key isn't kept on this computer anymore. Connect it again.", kind.name))?;
    let models = list(kind, &address(kind, &connected.base)?, &key).await?;
    catalog::keep(&dir, kind.id, &models);
    Ok(models)
}

/// Disconnects the provider `id`: its key, its list of models and what was
/// picked for it are forgotten. Chats it answered are kept.
pub fn disconnect(engine: &Arc<Engine>, id: &str) -> Result<(), String> {
    let kind = named(id)?;
    let dir = engine.paths().dir.clone();
    keys::forget(&keys::Place::of(&dir, engine.is_demo()), kind.id);
    catalog::forget(&dir, kind.id);
    let mut settings = engine.settings();
    let chat = &mut settings.chat;
    chat.providers.remove(kind.id);
    chat.picked.remove(kind.id);
    let its = format!("{}/", kind.id);
    chat.hidden_models.retain(|m| !m.starts_with(&its));
    chat.shown_models.retain(|m| !m.starts_with(&its));
    if chat.brain == kind.id {
        chat.brain.clear();
    }
    engine.update_settings(settings)?;
    Ok(())
}

/// The models of `brain` that the pickers offer (see `ChatSettings::offers`).
pub fn offered(engine: &Engine, brain: &str) -> Vec<Model> {
    let chat = engine.settings().chat;
    let all = catalog::known(&engine.paths().dir, brain);
    let listed = all.len();
    all.into_iter().filter(|m| chat.offers(brain, &m.id, listed)).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::chat::testing::{self, Asked, Server};
    use crate::config::Paths;
    use serde_json::json;

    /// The keys go to a file of the test's own, not the Keychain.
    fn keys_in(name: &str) -> std::path::PathBuf {
        let file = std::env::temp_dir().join(format!("muxin-provider-keys-{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        file
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_is_connected_only_when_it_answers_with_its_models() {
        let api = Server::start(|asked: &Asked| match (asked.path.as_str(), asked.headers.get("authorization").map(String::as_str)) {
            ("/v1/models", Some("Bearer sk-good")) => testing::json(200, json!({"data": [{"id": "small"}, {"id": "large"}]})),
            ("/v1/models", _) => testing::json(401, json!({"error": {"message": "bad key"}})),
            _ => (404, "text/plain", String::new()),
        })
        .await;
        let file = keys_in("connect");
        // SAFETY: tests of this module are the only ones that read this variable, and they agree on its value's shape.
        unsafe { std::env::set_var("MUXIN_AI_KEYS", &file) };
        let dir = std::env::temp_dir().join(format!("muxin-provider-connect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), false);
        let base = format!("{}/v1", api.url);

        // A key the provider turns away: said, and nothing is kept.
        assert_eq!(connect(&engine, "custom", "sk-bad", &base).await.unwrap_err(), "OpenAI-compatible didn't accept this API key. Connect it again in Settings › AI.");
        let place = keys::Place::of(&dir, false);
        assert_eq!(place, keys::Place::File(file.clone()));
        assert!(engine.settings().chat.providers.is_empty() && keys::get(&place, "custom").is_none());
        // What it needs, said before anything is asked.
        assert!(connect(&engine, "custom", " ", &base).await.unwrap_err().starts_with("OpenAI-compatible needs its API key."));
        assert!(connect(&engine, "custom", "sk-good", "").await.unwrap_err().contains("needs the address it's reached at"));
        assert!(connect(&engine, "custom", "sk-good", "example.com/v1").await.unwrap_err().contains("isn't an address"));
        assert!(connect(&engine, "codex", "sk-good", "").await.unwrap_err().starts_with("Codex is a program on this computer"));
        assert!(connect(&engine, "nope", "sk-good", "").await.unwrap_err().contains("isn't a provider Muxin knows"));

        // A good one: the key is kept apart from the settings, which only say it's connected and where.
        let models = connect(&engine, "custom", " sk-good\n", &format!("{base}/")).await.unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["small", "large"]);
        assert_eq!(keys::get(&place, "custom").as_deref(), Some("sk-good"));
        assert_eq!(engine.settings().chat.providers["custom"].base, base);
        let settings = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(settings.contains("[settings.chat.providers.custom]") && !settings.contains("sk-good"), "{settings}");
        // It can answer now, after the programs; and its list is there without asking again.
        assert!(brain::available(&engine.settings().chat).iter().any(|k| k.id == "custom"));
        assert_eq!(offered(&engine, "custom").len(), 2);
        // A model left out of the pickers isn't offered.
        let mut changed = engine.settings();
        changed.chat.offer("custom", "small", false);
        engine.update_settings(changed).unwrap();
        assert_eq!(offered(&engine, "custom").iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["large"]);
        assert_eq!(refresh(&engine, "custom").await.unwrap().len(), 2);

        // Disconnected: the key, the list and what was set for it are gone.
        disconnect(&engine, "custom").unwrap();
        assert!(keys::get(&place, "custom").is_none() && offered(&engine, "custom").is_empty());
        let chat = engine.settings().chat;
        assert!(chat.providers.is_empty() && chat.hidden_models.is_empty());
        assert!(refresh(&engine, "custom").await.unwrap_err().contains("isn't connected"));
    }

    #[test]
    fn a_long_list_is_offered_only_as_far_as_its_shown() {
        let mut chat = crate::config::ChatSettings::default();
        // A short list: whole, less what's hidden.
        assert!(chat.offers("openai", "gpt-a", 12));
        chat.offer("openai", "gpt-a", false);
        assert!(!chat.offers("openai", "gpt-a", 12) && chat.offers("openai", "gpt-b", 12));
        // A long one: nothing, plus what's shown.
        assert!(!chat.offers("openrouter", "x/a", 300));
        chat.offer("openrouter", "x/a", true);
        assert!(chat.offers("openrouter", "x/a", 300) && !chat.offers("openrouter", "x/b", 300));
        // Turned back, each is where it was.
        chat.offer("openai", "gpt-a", true);
        chat.offer("openrouter", "x/a", false);
        assert!(chat.offers("openai", "gpt-a", 12) && !chat.offers("openrouter", "x/a", 300));
    }
}
