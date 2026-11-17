//! Recipes in the palette (recipes.rs, saved_recipes.rs): each saved recipe,
//! the machine page's Components card, and its Update. With a machine on
//! screen they're about that one. Elsewhere a recipe or Components asks for a
//! machine first, in the palette, from the online Linux and Mac ones: one row
//! per recipe, not one per recipe and machine. Nothing installs until an item
//! is picked, as with the card's buttons.

use gpui::{App, Context, Window};

use super::{Glyph, Item, Run};
use crate::fleet::Engine;
use crate::fleet::model::Machine;
use crate::recipe::Catalog;
use crate::recipe::saved::SavedRecipe;
use crate::ui::format;
use crate::ui::root::{Root, Route};
use crate::ui::saved_recipes::names;

const COMPONENTS: &str = "layout-grid";
const RECIPE: &str = "list";
const UPDATE: &str = "arrow-up";
/// What an empty machine list says.
const NO_MACHINE: &str = "No online Linux or Mac machine like that.";

/// What a recipe item does.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum RecipeRun {
    /// Applies a saved recipe to a machine, as the card's Apply does.
    Apply(SavedRecipe, String),
    /// Updates a machine's agent CLIs (these ids), as the card's Update does.
    Update(String, Vec<String>),
    /// A machine's page, at its Components card.
    Components(String),
    /// Asks for the machine first.
    Pick(Pick),
}

/// What a machine is being picked for.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Pick {
    Apply(SavedRecipe),
    Components,
}

/// The palette's part: the saved recipes, read as it opens, and a machine
/// being picked for one.
pub(super) struct State {
    saved: Vec<SavedRecipe>,
    pick: Option<Pick>,
}

impl State {
    pub(super) fn new(engine: &Engine) -> State {
        State { saved: engine.saved_recipes(), pick: None }
    }

    pub(super) fn picking(&self) -> bool {
        self.pick.is_some()
    }

    /// While a machine is picked: the search box's icon, the keys, and what an empty list says.
    pub(super) fn words(&self) -> Option<(&'static str, &'static str, &'static str)> {
        Some(match self.pick.as_ref()? {
            Pick::Apply(_) => (RECIPE, "↑↓ move   ↵ apply   esc close", NO_MACHINE),
            Pick::Components => (COMPONENTS, "↑↓ move   ↵ open   esc close", NO_MACHINE),
        })
    }
}

/// A machine a recipe can go on: online, and Linux or a Mac (components
/// have steps for nothing else).
fn takes_recipes(m: &Machine) -> bool {
    m.status == "online" && !m.host.is_empty() && (m.platform.starts_with("linux") || m.platform.starts_with("darwin"))
}

fn item(title: impl Into<String>, detail: impl Into<String>, side: impl Into<String>, icon: &'static str, run: RecipeRun, also: impl Into<String>) -> Item {
    let mut i = Item::new(title, detail, side, Glyph::Icon(icon), Run::Recipe(run));
    i.also = also.into();
    i
}

/// For the machine on screen (`here`): its Components card, and Update when
/// its card's check found agent CLIs to update (`updatable`).
fn about_machine(here: Option<&Machine>, updatable: &[String]) -> Vec<Item> {
    let Some(m) = here else { return Vec::new() };
    let mut out = vec![item(format!("Components on {}", m.name), "Install and update its tools", "", COMPONENTS, RecipeRun::Components(m.id.clone()), "recipe install tools update")];
    if !updatable.is_empty() {
        out.push(item(format!("Update agent CLIs on {}", m.name), names(updatable), "", UPDATE, RecipeRun::Update(m.id.clone(), updatable.to_vec()), "recipe upgrade tools components"));
    }
    out
}

/// Each saved recipe: applied to the machine on screen (`here`), else to one
/// picked next, with Components on a machine first. Nothing when there's no
/// machine on screen and none can take a recipe (`any`). A recipe with no
/// component this Muxin has is left out: there'd be nothing to apply.
fn saved_items(here: Option<&Machine>, any: bool, saved: &[SavedRecipe], catalog: &Catalog) -> Vec<Item> {
    if here.is_none() && !any {
        return Vec::new();
    }
    let mut out = Vec::new();
    if here.is_none() {
        out.push(item("Components on a machine…", "Install and update a machine's tools", "", COMPONENTS, RecipeRun::Pick(Pick::Components), "recipe install tools update"));
    }
    for r in saved {
        let (known, _) = r.split(catalog);
        if known.is_empty() {
            continue;
        }
        let also = format!("recipe install {}", known.join(" "));
        out.push(match here {
            Some(m) => item(format!("Apply recipe: {}", r.name), names(&known), format!("On {}", m.name), RECIPE, RecipeRun::Apply(r.clone(), m.id.clone()), also),
            None => item(format!("Apply recipe: {}…", r.name), names(&known), "Recipe", RECIPE, RecipeRun::Pick(Pick::Apply(r.clone())), also),
        });
    }
    out
}

/// The machines to pick from, in `machines`' order: picking one applies the
/// recipe there, or shows its Components card.
fn machine_choices(machines: &[&Machine], pick: &Pick) -> Vec<Item> {
    machines
        .iter()
        .filter(|m| takes_recipes(m))
        .map(|m| {
            let run = match pick {
                Pick::Apply(r) => RecipeRun::Apply(r.clone(), m.id.clone()),
                Pick::Components => RecipeRun::Components(m.id.clone()),
            };
            Item::new(m.name.clone(), format::machine_subtitle(m).0, "", Glyph::Machine(format::machine_icon(m), format::machine_tile(m)), Run::Recipe(run))
        })
        .collect()
}

impl Root {
    /// The machine on screen, when a recipe can go on it. Not a box's
    /// session's: its agent runs in a container there.
    fn recipe_machine(&self) -> Option<&Machine> {
        if let Route::Session(id) = &self.route
            && self.snapshot.sessions.iter().any(|s| &s.id == id && s.box_id.is_some())
        {
            return None;
        }
        let id = self.machine_on_screen()?;
        self.snapshot.machines.iter().find(|m| m.id == id).filter(|m| takes_recipes(m))
    }

    /// The command list's recipe items: those about the machine on screen,
    /// and the saved recipes.
    pub(super) fn recipe_items(&self) -> (Vec<Item>, Vec<Item>) {
        let (Some(p), Ok(catalog)) = (&self.palette, crate::recipe::catalog()) else { return Default::default() };
        let here = self.recipe_machine();
        let updatable = here.map(|m| self.recipes.updatable(&m.id)).unwrap_or_default();
        let any = self.snapshot.machines.iter().any(takes_recipes);
        (about_machine(here, &updatable), saved_items(here, any, &p.recipes.saved, catalog))
    }

    /// While a machine is picked: the ones that can take a recipe, in the sidebar's order.
    pub(super) fn recipe_machine_items(&self, state: &State, cx: &App) -> Vec<Item> {
        let Some(pick) = &state.pick else { return Vec::new() };
        machine_choices(&Root::machine_order(&self.snapshot, cx), pick)
    }

    pub(super) fn run_recipe_item(&mut self, run: RecipeRun, window: &mut Window, cx: &mut Context<Self>) {
        let run = match run {
            RecipeRun::Pick(pick) => return self.pick_recipe_machine(pick, cx),
            run => run,
        };
        self.close_palette(window, cx);
        match run {
            RecipeRun::Apply(recipe, machine) => self.apply_saved_recipe(&machine, &recipe, cx),
            RecipeRun::Update(machine, ids) => self.apply_components(&machine, ids, true, cx),
            RecipeRun::Components(machine) => self.show_components(&machine, window, cx),
            RecipeRun::Pick(_) => {}
        }
    }

    /// The palette lists machines for `pick`, as Theme: Choose… lists themes.
    fn pick_recipe_machine(&mut self, pick: Pick, cx: &mut Context<Self>) {
        let placeholder = match &pick {
            Pick::Apply(r) => format!("Apply {} to which machine?", r.name),
            Pick::Components => "Which machine's components?".to_string(),
        };
        let Some(p) = &mut self.palette else { return };
        p.recipes.pick = Some(pick);
        p.query = String::new();
        p.picked = None;
        p.input.update(cx, |i, cx| {
            i.set_placeholder(placeholder, cx);
            i.set_text("", cx);
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(id: &str, platform: &str, status: &str) -> Machine {
        Machine {
            id: id.into(),
            name: format!("{}{}", id[..1].to_uppercase(), &id[1..]),
            host: id.into(),
            local: false,
            platform: platform.into(),
            kind: "other".into(),
            status: status.into(),
            checking: false,
            notes: None,
            runner_ids: Vec::new(),
            summary: Default::default(),
            probe: None,
            diagnostics: Vec::new(),
            workspaces_allowed: true,
            agents: Vec::new(),
            agent_version: None,
            sessions_error: None,
            gh: None,
            agent_hooks: None,
            unreachable: None,
            tailnet: None,
            cloud: None,
            displays: Vec::new(),
            parent: None,
            vm: None,
            direct_address: None,
            ignored_warnings: Vec::new(),
        }
    }

    fn recipe(name: &str, ids: &[&str]) -> SavedRecipe {
        SavedRecipe { name: name.into(), components: ids.iter().map(|s| s.to_string()).collect() }
    }

    fn titles(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.title.as_str()).collect()
    }

    fn run(i: &Item) -> &RecipeRun {
        match &i.run {
            Run::Recipe(r) => r,
            _ => panic!("{} isn't a recipe item", i.title),
        }
    }

    fn catalog() -> &'static Catalog {
        crate::recipe::catalog().unwrap()
    }

    #[test]
    fn only_online_linux_and_macs_take_recipes() {
        assert!(takes_recipes(&machine("beacon", "linux-x64", "online")));
        assert!(takes_recipes(&machine("forge", "darwin-arm64", "online")));
        assert!(!takes_recipes(&machine("win", "windows-x64", "online")));
        for status in ["offline", "asleep", "disconnected", "unchecked", "manual"] {
            assert!(!takes_recipes(&machine("beacon", "linux-x64", status)), "{status}");
        }
        // A cloud machine that hasn't started has no host yet.
        let mut starting = machine("dev-1", "linux-x64", "online");
        starting.host.clear();
        assert!(!takes_recipes(&starting));
    }

    #[test]
    fn on_a_machines_page_they_go_on_it() {
        let forge = machine("forge", "darwin-arm64", "online");
        let saved = [recipe("Agent CLIs", &["claude", "codex"])];
        let about = about_machine(Some(&forge), &["claude".into(), "codex".into()]);
        assert_eq!(titles(&about), ["Components on Forge", "Update agent CLIs on Forge"]);
        assert_eq!(run(&about[0]), &RecipeRun::Components("forge".into()));
        assert_eq!(run(&about[1]), &RecipeRun::Update("forge".into(), vec!["claude".into(), "codex".into()]));
        assert_eq!(about[1].detail, "Claude Code, Codex");

        let list = saved_items(Some(&forge), true, &saved, catalog());
        assert_eq!(titles(&list), ["Apply recipe: Agent CLIs"], "no picker with a machine on screen");
        assert_eq!(list[0].side, "On Forge");
        assert_eq!(list[0].detail, "Claude Code, Codex");
        assert_eq!(run(&list[0]), &RecipeRun::Apply(saved[0].clone(), "forge".into()));
    }

    #[test]
    fn update_only_where_the_card_found_agent_clis() {
        let forge = machine("forge", "darwin-arm64", "online");
        assert_eq!(titles(&about_machine(Some(&forge), &[])), ["Components on Forge"]);
        assert!(about_machine(None, &["claude".into()]).is_empty(), "no Update without a machine on screen");
    }

    #[test]
    fn elsewhere_they_ask_for_a_machine() {
        let saved = [recipe("Agent CLIs", &["claude", "codex"]), recipe("Web", &["node", "bun"])];
        let list = saved_items(None, true, &saved, catalog());
        assert_eq!(titles(&list), ["Components on a machine…", "Apply recipe: Agent CLIs…", "Apply recipe: Web…"]);
        assert_eq!(run(&list[0]), &RecipeRun::Pick(Pick::Components));
        assert_eq!(run(&list[1]), &RecipeRun::Pick(Pick::Apply(saved[0].clone())));
        assert_eq!(list[1].side, "Recipe");
        // No machine can take one: nothing to pick from, so nothing's listed.
        assert!(saved_items(None, false, &saved, catalog()).is_empty());
    }

    #[test]
    fn a_recipe_with_nothing_this_muxin_has_is_left_out() {
        let forge = machine("forge", "darwin-arm64", "online");
        let saved = [recipe("Future", &["warp-drive"]), recipe("Mixed", &["git", "warp-drive"])];
        let list = saved_items(Some(&forge), true, &saved, catalog());
        assert_eq!(titles(&list), ["Apply recipe: Mixed"]);
        assert_eq!(list[0].detail, "git", "what it doesn't have isn't named");
    }

    #[test]
    fn the_picker_lists_machines_that_can_take_one() {
        let (beacon, win, forge, harbor) = (machine("beacon", "linux-x64", "online"), machine("win", "windows-x64", "online"), machine("forge", "darwin-arm64", "online"), machine("harbor", "darwin-arm64", "offline"));
        let machines = [&beacon, &win, &forge, &harbor];
        let web = recipe("Web", &["node"]);
        let list = machine_choices(&machines, &Pick::Apply(web.clone()));
        assert_eq!(titles(&list), ["Beacon", "Forge"], "in the order given");
        assert_eq!(run(&list[1]), &RecipeRun::Apply(web, "forge".into()));
        let list = machine_choices(&machines, &Pick::Components);
        assert_eq!(run(&list[0]), &RecipeRun::Components("beacon".into()));
    }

    #[test]
    fn found_by_the_words_people_type() {
        let forge = machine("forge", "darwin-arm64", "online");
        let saved = [recipe("Agent CLIs", &["claude", "codex"])];
        let mut all = about_machine(Some(&forge), &["claude".into()]);
        all.extend(saved_items(Some(&forge), true, &saved, catalog()));
        all.extend(saved_items(None, true, &saved, catalog()));
        let found = |q: &str| all.iter().filter(|i| i.matches(q).is_some()).map(|i| i.title.as_str()).collect::<Vec<_>>();
        for q in ["components", "install", "tools", "recipe"] {
            assert!(found(q).contains(&"Components on Forge"), "{q}: {:?}", found(q));
            assert!(found(q).contains(&"Components on a machine…"), "{q}: {:?}", found(q));
        }
        assert!(found("update").contains(&"Update agent CLIs on Forge"));
        assert!(found("recipe").contains(&"Apply recipe: Agent CLIs"));
        // Its components' names find a recipe too.
        assert!(found("codex").contains(&"Apply recipe: Agent CLIs…"));
    }
}
