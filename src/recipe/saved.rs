//! Recipes saved in the window, like snippets: a name and the ids of its
//! components. They're kept in recipes.json in Muxin's folder, not in
//! config.toml, where a build without recipes would drop them the next time it
//! saved the settings. The demo keeps them in memory. A machine's page saves
//! what's picked there and applies one; Add machine applies one once the new
//! machine answers. Either way it's `recipe_apply`, as `muxin recipe apply` runs it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::Catalog;
use crate::fleet::Engine;

/// How long a machine just added may take to answer before its recipe is given up.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRecipe {
    pub name: String,
    /// Component ids, in the order picked. One this Muxin doesn't know is kept
    /// (a newer Muxin may), and left out when it's applied.
    pub components: Vec<String>,
}

impl SavedRecipe {
    /// Its ids `catalog` has, and those it hasn't, each in order.
    pub fn split(&self, catalog: &Catalog) -> (Vec<String>, Vec<String>) {
        self.components.iter().cloned().partition(|id| catalog.get(id).is_some())
    }
}

/// The recipes saved in `path`. A missing or broken file is none.
pub fn read(path: &Path) -> Vec<SavedRecipe> {
    std::fs::read_to_string(path).ok().and_then(|text| parse(&text)).unwrap_or_default()
}

/// The recipes in a file's text, or None when it isn't a list of them. One with
/// no name or no components is dropped; a name already taken gets a number.
fn parse(text: &str) -> Option<Vec<SavedRecipe>> {
    let list: Vec<SavedRecipe> = serde_json::from_str(text).ok()?;
    let mut out: Vec<SavedRecipe> = Vec::new();
    for r in list {
        let (name, components) = (r.name.trim(), unique_ids(&r.components));
        if name.is_empty() || components.is_empty() {
            continue;
        }
        let mut unique = name.to_string();
        for n in 2.. {
            if !out.iter().any(|o| o.name.eq_ignore_ascii_case(&unique)) {
                break;
            }
            unique = format!("{name} {n}");
        }
        out.push(SavedRecipe { name: unique, components });
    }
    Some(out)
}

/// `ids` trimmed, each once, none empty.
fn unique_ids(ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids.iter().map(|id| id.trim()).filter(|id| !id.is_empty()) {
        if !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// Writes them to `path`, readable by this user only. A file there that
/// doesn't read (not JSON, or not even text: a hand edit saved as UTF-16) is
/// kept beside it first, as recipes.json.broken, so a hand edit gone wrong
/// isn't lost. One kept from before stays: the next goes to
/// recipes.json.broken-2, and so on.
pub fn write(path: &Path, list: &[SavedRecipe]) -> Result<(), String> {
    if let Ok(bytes) = std::fs::read(path)
        && std::str::from_utf8(&bytes).ok().and_then(parse).is_none()
    {
        let aside = aside(path);
        std::fs::rename(path, &aside).map_err(|e| format!("Couldn't move the old {} aside: {e}", path.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
    crate::config::write_atomic(path, &bytes).map_err(|e| format!("Couldn't save the recipes: {e:#}"))
}

/// Where a file that doesn't read is kept: recipes.json.broken, or the first
/// recipes.json.broken-N that isn't taken.
fn aside(path: &Path) -> std::path::PathBuf {
    let first = path.with_extension("json.broken");
    if !first.exists() {
        return first;
    }
    (2u32..).map(|n| path.with_extension(format!("json.broken-{n}"))).find(|p| !p.exists()).unwrap_or(first)
}

/// Adds `recipe`, or puts it in place of the one called `replacing` (renaming
/// it); the one saved. Names are unique, ignoring case.
pub fn save(list: &mut Vec<SavedRecipe>, replacing: Option<&str>, recipe: SavedRecipe) -> Result<SavedRecipe, String> {
    let recipe = SavedRecipe { name: recipe.name.trim().to_string(), components: unique_ids(&recipe.components) };
    if recipe.name.is_empty() {
        return Err("Give it a name.".into());
    }
    if recipe.components.is_empty() {
        return Err("Pick at least one component for it.".into());
    }
    let at = match replacing {
        Some(old) => Some(position(list, old)?),
        None => None,
    };
    if list.iter().enumerate().any(|(i, r)| Some(i) != at && r.name.eq_ignore_ascii_case(&recipe.name)) {
        return Err(format!("There's already a recipe called \"{}\".", recipe.name));
    }
    match at {
        Some(i) => list[i] = recipe.clone(),
        None => list.push(recipe.clone()),
    }
    Ok(recipe)
}

/// Removes the recipe called `name`; the one removed.
pub fn remove(list: &mut Vec<SavedRecipe>, name: &str) -> Result<SavedRecipe, String> {
    let at = position(list, name)?;
    Ok(list.remove(at))
}

fn position(list: &[SavedRecipe], name: &str) -> Result<usize, String> {
    list.iter().position(|r| r.name.eq_ignore_ascii_case(name.trim())).ok_or_else(|| format!("There's no recipe called \"{}\".", name.trim()))
}

impl Engine {
    fn saved_recipes_path(&self) -> PathBuf {
        self.paths().dir.join("recipes.json")
    }

    /// The recipes saved in the window.
    pub fn saved_recipes(&self) -> Vec<SavedRecipe> {
        match self.demo_world() {
            Some(w) => w.saved_recipes.lock().unwrap().clone(),
            None => read(&self.saved_recipes_path()),
        }
    }

    /// Changes the saved recipes with `edit` (`save`, `remove`) and keeps them.
    pub fn edit_saved_recipes<T>(&self, edit: impl FnOnce(&mut Vec<SavedRecipe>) -> Result<T, String>) -> Result<T, String> {
        let mut list = self.saved_recipes();
        let out = edit(&mut list)?;
        match self.demo_world() {
            Some(w) => *w.saved_recipes.lock().unwrap() = list,
            None => write(&self.saved_recipes_path(), &list)?,
        }
        Ok(out)
    }

    /// Starts applying a saved recipe to a machine: `recipe_apply` with its
    /// components, what they need first. Ids this Muxin doesn't know are left
    /// out, and its log says so. Returns the operation.
    pub fn apply_saved_recipe(self: &Arc<Self>, machine_id: &str, recipe: &SavedRecipe) -> Result<u64, String> {
        let catalog = super::catalog()?;
        let (known, unknown) = recipe.split(catalog);
        if known.is_empty() {
            // Resolving no ids would mean every component.
            return Err(format!("None of {}'s components is in this Muxin: {}.", recipe.name, unknown.join(", ")));
        }
        let components = catalog.resolve(&known)?;
        let others = catalog.conflicting(&components).into_iter().cloned().collect();
        let (op, _) = self.recipe_apply(machine_id, components.into_iter().cloned().collect(), others, false)?;
        if !unknown.is_empty() {
            self.op_log(op, format!("! Left out {}: this Muxin has no component called that.", unknown.join(", ")));
        }
        Ok(op)
    }

    /// For a machine just added: applies `recipe` once its first check finds
    /// it online (`apply_saved_recipe`). When it doesn't answer, or applying
    /// can't start, a failed operation says so, and nothing is installed.
    pub fn apply_recipe_once_up(self: &Arc<Self>, machine_id: &str, recipe: SavedRecipe) {
        let (me, id) = (self.clone(), machine_id.to_string());
        // A check from before (a machine removed and added again) doesn't count.
        let since = chrono::Utc::now();
        self.runtime().spawn(async move {
            let mut rx = me.subscribe();
            let answered = tokio::time::timeout(ANSWER_TIMEOUT, async {
                loop {
                    match rx.borrow_and_update().machines.iter().find(|m| m.id == id) {
                        None => return None,
                        Some(m) if m.checking || !m.probe.as_ref().is_some_and(|p| p.checked_at >= since) => {}
                        Some(m) => return Some(m.status == "online"),
                    }
                    if rx.changed().await.is_err() {
                        return None;
                    }
                }
            })
            .await;
            let name = me.machine(&id).map(|(spec, _)| spec.name).unwrap_or_else(|| id.clone());
            let started = match answered {
                // Removed meanwhile: there's nothing to apply it to.
                Ok(None) => return,
                Ok(Some(true)) => me.apply_saved_recipe(&id, &recipe).map(|_| ()),
                Ok(Some(false)) | Err(_) => Err(format!("{name} didn't answer, so nothing was installed. Apply {} from its page once it's up.", recipe.name)),
            };
            if let Err(e) = started {
                let op = me.begin_op("recipe", format!("Apply {} to {name}", recipe.components.join(", ")), Some(id.clone()), None);
                me.op_log(op, format!("error: {e}"));
                me.op_finish(op, Err(e));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::ops::NewMachine;

    fn recipe(name: &str, ids: &[&str]) -> SavedRecipe {
        SavedRecipe { name: name.into(), components: ids.iter().map(|s| s.to_string()).collect() }
    }

    fn folder(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxen-saved-recipes-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn saved_recipes_survive_a_restart() {
        let dir = folder("file");
        let path = dir.join("recipes.json");
        assert!(read(&path).is_empty(), "no file: none");
        let list = vec![recipe("Agents", &["claude", "codex"]), recipe("Web", &["node", "bun"])];
        write(&path, &list).unwrap();
        assert_eq!(read(&path), list);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // Broken: none, and saving again keeps the broken one aside.
        std::fs::write(&path, b"[{\"name\": \"Agents\", ").unwrap();
        assert!(read(&path).is_empty());
        write(&path, &list[..1]).unwrap();
        assert_eq!(read(&path), list[..1]);
        assert_eq!(std::fs::read_to_string(dir.join("recipes.json.broken")).unwrap(), "[{\"name\": \"Agents\", ");
        std::fs::write(&path, b"{\"name\": \"not a list\"}").unwrap();
        assert!(read(&path).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that isn't text at all (a hand edit saved as UTF-16) is kept
    /// aside too, byte for byte; and a second broken one doesn't take the
    /// place of the first.
    #[test]
    fn every_broken_file_is_kept_aside() {
        let dir = folder("aside");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipes.json");
        let list = vec![recipe("Agents", &["claude"])];
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("[{\"name\": \"Web\"}]".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        std::fs::write(&path, &utf16).unwrap();
        assert!(read(&path).is_empty());
        write(&path, &list).unwrap();
        assert_eq!(read(&path), list);
        assert_eq!(std::fs::read(dir.join("recipes.json.broken")).unwrap(), utf16, "kept as it was");

        // Two more broken edits: each gets a name of its own.
        for (n, text) in [(2, "[{"), (3, "not json")] {
            std::fs::write(&path, text).unwrap();
            write(&path, &list).unwrap();
            assert_eq!(std::fs::read_to_string(dir.join(format!("recipes.json.broken-{n}"))).unwrap(), text);
        }
        assert_eq!(std::fs::read(dir.join("recipes.json.broken")).unwrap(), utf16, "the first one is still there");
        // A file that reads is simply replaced: nothing more is kept.
        write(&path, &list[..0]).unwrap();
        assert!(!dir.join("recipes.json.broken-4").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recipes_edited_by_hand_still_read() {
        // Trimmed, each id once; a name given twice gets a number; no name or no components: dropped.
        let text = r#"[
            {"name": " Agents ", "components": ["claude", " codex", "claude", ""]},
            {"name": "agents", "components": ["pi"]},
            {"name": "", "components": ["git"]},
            {"name": "Empty", "components": []},
            {"name": "Later", "components": ["podman", "git"]}
        ]"#;
        let list = parse(text).unwrap();
        assert_eq!(list, [recipe("Agents", &["claude", "codex"]), recipe("agents 2", &["pi"]), recipe("Later", &["podman", "git"])]);
        // An id this Muxin doesn't know is kept, and told apart.
        let (known, unknown) = list[2].split(super::super::catalog().unwrap());
        assert_eq!((known, unknown), (vec!["git".to_string()], vec!["podman".to_string()]));
    }

    #[test]
    fn saving_renaming_and_removing() {
        let mut list = Vec::new();
        assert_eq!(save(&mut list, None, recipe("  Agents ", &["claude", "claude", "codex"])).unwrap(), recipe("Agents", &["claude", "codex"]));
        assert_eq!(save(&mut list, None, recipe("agents", &["pi"])).unwrap_err(), "There's already a recipe called \"agents\".");
        assert_eq!(save(&mut list, None, recipe(" ", &["pi"])).unwrap_err(), "Give it a name.");
        assert_eq!(save(&mut list, None, recipe("Pi", &[" "])).unwrap_err(), "Pick at least one component for it.");
        save(&mut list, None, recipe("Web", &["node"])).unwrap();
        // Renamed in place, keeping its components; the same name in other letters is fine for itself.
        save(&mut list, Some("agents"), recipe("CLIs", &["claude", "codex"])).unwrap();
        save(&mut list, Some("web"), recipe("WEB", &["node"])).unwrap();
        assert_eq!(list, [recipe("CLIs", &["claude", "codex"]), recipe("WEB", &["node"])]);
        assert_eq!(save(&mut list, Some("Agents"), recipe("X", &["git"])).unwrap_err(), "There's no recipe called \"Agents\".");
        assert_eq!(save(&mut list, Some("WEB"), recipe("clis", &["git"])).unwrap_err(), "There's already a recipe called \"clis\".");
        assert_eq!(remove(&mut list, " clis ").unwrap().name, "CLIs");
        assert_eq!(remove(&mut list, "CLIs").unwrap_err(), "There's no recipe called \"CLIs\".");
        assert_eq!(list, [recipe("WEB", &["node"])]);
    }

    /// The operation on `machine` the last one started, once it's done: its status and log.
    async fn finished(engine: &Arc<Engine>, machine: &str) -> (String, String, Vec<String>) {
        for _ in 0..200 {
            engine.republish();
            let op = engine.current().operations.iter().find(|o| o.machine_id.as_deref() == Some(machine) && o.kind == "recipe").cloned();
            if let Some(op) = op.filter(|o| o.status != "running") {
                return (op.status, op.title, op.log);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no recipe operation on {machine} finished");
    }

    #[tokio::test]
    async fn the_demo_keeps_them_and_applies_them() {
        let engine = Engine::new(Paths::resolve(Some(folder("demo")), None), true);
        engine.run_check(None).await;
        assert!(engine.saved_recipes().is_empty());
        engine.edit_saved_recipes(|list| save(list, None, recipe("Tools", &["uv", "podman", "rust"]))).unwrap();
        assert_eq!(engine.saved_recipes(), [recipe("Tools", &["uv", "podman", "rust"])]);
        assert!(!engine.saved_recipes_path().exists(), "the demo writes nothing");

        // On a machine's page: what Muxin knows is applied, the rest left out and said so.
        let op = engine.apply_saved_recipe("dev-1", &engine.saved_recipes()[0]).unwrap();
        let (status, title, log) = finished(&engine, "dev-1").await;
        assert_eq!((status.as_str(), title.as_str()), ("succeeded", "Apply uv, rust to dev-1"), "{log:?}");
        assert!(log.contains(&"! Left out podman: this Muxin has no component called that.".to_string()), "{log:?}");
        assert!(log.contains(&"✓ uv is installed".to_string()), "{log:?}");
        assert!(engine.current().operations.iter().any(|o| o.id == op));
        // Nothing it knows: refused, rather than applying every component.
        let err = engine.apply_saved_recipe("dev-1", &recipe("Later", &["podman"])).unwrap_err();
        assert_eq!(err, "None of Later's components is in this Muxin: podman.");
    }

    #[tokio::test]
    async fn add_machine_applies_one_once_it_answers() {
        let engine = Engine::new(Paths::resolve(Some(folder("added")), None), true);
        engine.run_check(None).await;

        // It answers: the recipe is applied, as recipe apply does.
        let id = engine.add_machine(NewMachine { name: "Spare".into(), host: "spare.local".into(), port: None }).unwrap();
        engine.apply_recipe_once_up(&id, recipe("Tools", &["uv"]));
        engine.run_check(Some(vec![id.clone()])).await;
        let (status, title, log) = finished(&engine, &id).await;
        assert_eq!((status.as_str(), title.as_str()), ("succeeded", "Apply uv to Spare"), "{log:?}");

        // It doesn't answer: a failed operation says so, and nothing ran.
        engine.remove_machine("harbor").unwrap();
        let id = engine.add_machine(NewMachine { name: "Harbor".into(), host: "harbor.local".into(), port: None }).unwrap();
        engine.apply_recipe_once_up(&id, recipe("Tools", &["uv"]));
        engine.run_check(Some(vec![id.clone()])).await;
        let (status, title, log) = finished(&engine, &id).await;
        assert_eq!((status.as_str(), title.as_str()), ("failed", "Apply uv to Harbor"));
        assert_eq!(log, ["error: Harbor didn't answer, so nothing was installed. Apply Tools from its page once it's up."]);
    }
}
