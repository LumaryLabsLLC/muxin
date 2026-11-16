//! Recipes: one way to set up any machine. A component is one thing a machine
//! can have (git, Node.js, an agent CLI): a `check` that says whether it's
//! there, an `install` and an optional `update`, what it requires, what it
//! can't go with and why, the systems it works on, and whether its steps need
//! root. A recipe is a list of component ids, never paths. Muxin's own
//! components are the TOML files in components/, built into the binary.
//!
//! Muxin checks machines with them (every check of a recipe in one script,
//! `recipe_check`, changing nothing) and applies them (`recipe_apply`, in
//! apply.rs: installs what's missing and, when asked, updates what's behind),
//! also to the cloud machines and VMs it makes (`recipe_setup`).

mod apply;
pub use apply::Applied;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::fleet::Engine;
use crate::fleet::sessions::agent;
use crate::ops::remote::{sh_quote, shell_for};
use crate::probe::Shell;

/// How long all the checks of a recipe may take together.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// The systems component steps run on (they're sh), named as the machine check names them.
const SYSTEMS: &[&str] = &["linux", "darwin"];

/// Muxin's components, in the order they're listed: git, then what cloud
/// machines can come with (cloud::setup::EXTRAS), each after what it requires.
const BUILT_IN: &[(&str, &str)] = &[
    ("git.toml", include_str!("components/git.toml")),
    ("docker.toml", include_str!("components/docker.toml")),
    ("gh.toml", include_str!("components/gh.toml")),
    ("node.toml", include_str!("components/node.toml")),
    ("uv.toml", include_str!("components/uv.toml")),
    ("python.toml", include_str!("components/python.toml")),
    ("go.toml", include_str!("components/go.toml")),
    ("rust.toml", include_str!("components/rust.toml")),
    ("php.toml", include_str!("components/php.toml")),
    ("ruby.toml", include_str!("components/ruby.toml")),
    ("java.toml", include_str!("components/java.toml")),
    ("unzip.toml", include_str!("components/unzip.toml")),
    ("bun.toml", include_str!("components/bun.toml")),
    ("deno.toml", include_str!("components/deno.toml")),
    ("dotnet.toml", include_str!("components/dotnet.toml")),
    ("claude.toml", include_str!("components/claude.toml")),
    ("codex.toml", include_str!("components/codex.toml")),
    ("gemini.toml", include_str!("components/gemini.toml")),
    ("opencode.toml", include_str!("components/opencode.toml")),
    ("amp.toml", include_str!("components/amp.toml")),
    ("pi.toml", include_str!("components/pi.toml")),
    ("tailscale.toml", include_str!("components/tailscale.toml")),
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    /// Lowercase letters, digits and "-" ("source/id" is kept for components from elsewhere).
    pub id: String,
    pub name: String,
    /// Components this one needs, by id. They come first.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Components it can't be on a machine with, and why.
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    /// The systems its steps work on; all of SYSTEMS when left out.
    #[serde(default = "all_systems")]
    pub os: Vec<String>,
    /// Its steps need root: on Linux they run through `sudo -n`. (On a Mac they
    /// run as the user, as Homebrew wants.) Steps never call sudo themselves.
    #[serde(default)]
    pub root: bool,
    /// With root: a test, run as the user like `check` and after the prelude
    /// like the steps, that lets the steps run as the user when it exits 0.
    /// For tools that live where whoever installed them put them: npm's global
    /// folder is the user's own when their Node.js is (mise, nvm, Homebrew),
    /// and only root's when it's the system's.
    pub root_unless: Option<String>,
    /// Exits 0 when the component is there. Fast, no network, changes nothing.
    pub check: String,
    pub install: String,
    pub update: Option<String>,
    /// Goes with `update`: exits 0 when there's nothing newer than what's
    /// there, and then updating leaves it alone. It may ask the network (it
    /// runs only when updating), can use what steps use (prelude.sh), and
    /// changes nothing.
    pub up_to_date: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conflict {
    pub id: String,
    pub reason: String,
}

fn all_systems() -> Vec<String> {
    SYSTEMS.iter().map(|s| s.to_string()).collect()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('-') && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Whether a step runs sudo itself (a word "sudo", as in `sudo apt-get` or `/usr/bin/sudo`).
fn calls_sudo(step: &str) -> bool {
    step.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).any(|w| w == "sudo")
}

/// Reads one component file; `file` names it in errors.
pub fn parse(file: &str, text: &str) -> Result<Component, String> {
    let c: Component = toml::from_str(text).map_err(|e| match e.span() {
        Some(span) => format!("{file}, line {}: {}", text[..span.start].matches('\n').count() + 1, e.message()),
        None => format!("{file}: {}", e.message()),
    })?;
    let bad = |why: String| Err(format!("{file}: {why}"));
    if !valid_id(&c.id) {
        return bad(format!("\"{}\" isn't a component id. Use lowercase letters, digits and \"-\".", c.id));
    }
    if c.name.trim().is_empty() {
        return bad(format!("{} has no name.", c.id));
    }
    if c.check.trim().is_empty() || c.install.trim().is_empty() || c.update.as_ref().is_some_and(|u| u.trim().is_empty()) {
        return bad(format!("{} needs a check and an install step, and an update step can't be empty.", c.id));
    }
    if c.os.is_empty() {
        return bad(format!("{} works on no system; leave os out for all of them.", c.id));
    }
    if let Some(os) = c.os.iter().find(|os| !SYSTEMS.contains(&os.as_str())) {
        return bad(format!("{} names \"{os}\"; components run on {}.", c.id, SYSTEMS.join(" and ")));
    }
    if let Some(r) = c.requires.iter().find(|r| !valid_id(r) || **r == c.id) {
        return bad(format!("{} can't require \"{r}\".", c.id));
    }
    for x in &c.conflicts {
        if x.id == c.id || c.requires.contains(&x.id) {
            return bad(format!("{} can't conflict with \"{}\", which it is or requires.", c.id, x.id));
        }
        if x.reason.trim().is_empty() {
            return bad(format!("{} conflicts with {} without saying why.", c.id, x.id));
        }
    }
    if c.root_unless.as_ref().is_some_and(|test| !c.root || test.trim().is_empty()) {
        return bad(format!("{}'s root_unless needs root = true and a test.", c.id));
    }
    if [Some(&c.install), c.update.as_ref(), c.root_unless.as_ref()].into_iter().flatten().any(|s| calls_sudo(s)) {
        return bad(if c.root {
            format!("{}'s steps run as root already; leave out sudo.", c.id)
        } else {
            format!("{} calls sudo. Set root = true instead, so Muxin can tell first whether sudo would ask for a password.", c.id)
        });
    }
    if c.update.is_some() != c.up_to_date.is_some() || c.up_to_date.as_ref().is_some_and(|t| t.trim().is_empty()) {
        return bad(format!("{} needs an update step and an up_to_date test together, or neither: updating leaves alone what's up to date.", c.id));
    }
    Ok(c)
}

#[derive(Debug)]
pub struct Catalog {
    components: Vec<Component>,
}

impl Catalog {
    /// Reads component files and checks them together: no id twice, every
    /// requirement there, none in a circle, none needing what it conflicts with.
    pub fn load(files: &[(&str, &str)]) -> Result<Catalog, String> {
        let mut components: Vec<Component> = Vec::new();
        for (file, text) in files {
            let c = parse(file, text)?;
            if components.iter().any(|o| o.id == c.id) {
                return Err(format!("{file}: there's another component called \"{}\".", c.id));
            }
            components.push(c);
        }
        let catalog = Catalog { components };
        for c in &catalog.components {
            let needs = catalog.order(&[c])?;
            if let Some(x) = c.conflicts.iter().find(|x| needs.iter().any(|n| n.id == x.id)) {
                return Err(format!("{} conflicts with {}, which it needs.", c.id, x.id));
            }
        }
        Ok(catalog)
    }

    pub fn get(&self, id: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.id == id)
    }

    pub fn ids(&self) -> Vec<&str> {
        self.components.iter().map(|c| c.id.as_str()).collect()
    }

    /// What a recipe brings, in the order it goes: each requirement before what
    /// needs it, otherwise in the order given. No ids: every component.
    pub fn resolve(&self, ids: &[String]) -> Result<Vec<&Component>, String> {
        let picked: Vec<&Component> = if ids.is_empty() {
            self.components.iter().collect()
        } else {
            ids.iter().map(|id| self.get(id.trim()).ok_or_else(|| format!("No component called \"{id}\"."))).collect::<Result<_, _>>()?
        };
        let order = self.order(&picked)?;
        for (i, a) in order.iter().enumerate() {
            for b in &order[i + 1..] {
                let conflict = a.conflicts.iter().find(|x| x.id == b.id).or_else(|| b.conflicts.iter().find(|x| x.id == a.id));
                if let Some(x) = conflict {
                    return Err(format!("{} and {} can't go together: {}", a.name, b.name, x.reason));
                }
            }
        }
        Ok(order)
    }

    /// The other components that can't be on a machine with any of `picked`,
    /// whichever side says so: applying looks for them first.
    pub fn conflicting(&self, picked: &[&Component]) -> Vec<&Component> {
        let apart = |a: &Component, b: &Component| a.conflicts.iter().any(|x| x.id == b.id) || b.conflicts.iter().any(|x| x.id == a.id);
        self.components.iter().filter(|o| !picked.iter().any(|p| p.id == o.id) && picked.iter().any(|p| apart(p, o))).collect()
    }

    fn order<'a>(&'a self, picked: &[&'a Component]) -> Result<Vec<&'a Component>, String> {
        let (mut done, mut path) = (Vec::new(), Vec::new());
        for c in picked {
            self.visit(c, &mut path, &mut done)?;
        }
        Ok(done)
    }

    /// Adds `c` to `done` after its requirements. `path` is the chain of
    /// components being added, to find a circle.
    fn visit<'a>(&'a self, c: &'a Component, path: &mut Vec<&'a str>, done: &mut Vec<&'a Component>) -> Result<(), String> {
        if done.iter().any(|d| d.id == c.id) {
            return Ok(());
        }
        if let Some(at) = path.iter().position(|p| *p == c.id) {
            let circle: Vec<&str> = path[at..].iter().copied().chain([c.id.as_str()]).collect();
            return Err(format!("Components require each other in a circle: {}.", circle.join(" → ")));
        }
        path.push(&c.id);
        for r in &c.requires {
            let required = self.get(r).ok_or_else(|| format!("{} requires \"{r}\", which isn't a component.", c.id))?;
            self.visit(required, path, done)?;
        }
        path.pop();
        done.push(c);
        Ok(())
    }
}

/// Muxin's components, read once.
pub fn catalog() -> Result<&'static Catalog, String> {
    static CATALOG: OnceLock<Result<Catalog, String>> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog::load(BUILT_IN)).as_ref().map_err(|e| format!("Muxin's components don't load: {e}"))
}

/// What the checks found on a machine.
#[derive(Debug, Serialize)]
pub struct MachineCheck {
    /// The machine's system, as the machine check names it: "linux", "darwin", "windows".
    pub os: String,
    pub components: Vec<ComponentState>,
}

#[derive(Debug, Serialize)]
pub struct ComponentState {
    pub id: String,
    pub name: String,
    /// "present" | "missing" | "unsupported" (it has no steps for this system)
    pub result: &'static str,
}

fn state(c: &Component, result: &'static str) -> ComponentState {
    ComponentState { id: c.id.clone(), name: c.name.clone(), result }
}

/// One script for every check. It prints `os=<system>`, then
/// `check.<id>=present|missing|unsupported` for each component. Each check runs
/// in a shell of its own with nothing on stdin, so it can't read the rest of
/// the script, end it, or break it with a mistake of its own.
fn check_script(components: &[&Component]) -> String {
    let mut s = String::from("os=$(uname -s 2>/dev/null | tr 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' 'abcdefghijklmnopqrstuvwxyz')\necho \"os=$os\"\n");
    for c in components {
        s.push_str(&format!(
            "case \"$os\" in {}) if sh -c {} </dev/null >/dev/null 2>&1; then echo check.{id}=present; else echo check.{id}=missing; fi ;; *) echo check.{id}=unsupported ;; esac\n",
            c.os.join("|"),
            sh_quote(&c.check),
            id = c.id
        ));
    }
    s
}

fn read_checks(out: &str, components: &[&Component]) -> Result<MachineCheck, String> {
    let value = |key: &str| out.lines().find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('=').map(str::to_string));
    let mut states = Vec::new();
    for c in components {
        let result = match value(&format!("check.{}", c.id)).as_deref() {
            Some("present") => "present",
            Some("missing") => "missing",
            Some("unsupported") => "unsupported",
            _ => return Err(format!("The check didn't say whether {} is there.", c.name)),
        };
        states.push(state(c, result));
    }
    Ok(MachineCheck { os: value("os").unwrap_or_default(), components: states })
}

impl Engine {
    /// Runs the checks of `components` (from `Catalog::resolve`) on a machine,
    /// all in one script, in the user's login shell as their terminals see it.
    /// Changes nothing.
    pub async fn recipe_check(self: &Arc<Self>, machine_id: &str, components: &[&Component]) -> Result<MachineCheck, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if matches!(shell_for(&spec, probe.as_ref()), Shell::Powershell) {
            // Steps are sh: no component has any for Windows.
            return Ok(MachineCheck { os: "windows".into(), components: components.iter().map(|c| state(c, "unsupported")).collect() });
        }
        if let Some(world) = self.demo_world() {
            // A CLI run hasn't checked its machines yet: ask the demo's check.
            let os = match probe.and_then(|p| p.os) {
                Some(os) => os,
                None => world.probe(&spec).await.os.unwrap_or_default(),
            };
            let components = components.iter().map(|c| state(c, apply::demo_state(world, &spec.id, &os, c))).collect();
            return Ok(MachineCheck { os, components });
        }
        if !spec.is_checkable() {
            return Err(format!("{} has no host to connect to.", spec.name));
        }
        let out = agent::run_sh(&spec, &check_script(components), CHECK_TIMEOUT).await?;
        read_checks(&out, components)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::mcp::{Session, call_tool};
    use serde_json::json;

    fn component(id: &str, extra: &str) -> String {
        format!("id = \"{id}\"\nname = \"{}\"\ncheck = \"true\"\ninstall = \"true\"\n{extra}\n", id.to_uppercase())
    }

    fn load(files: &[String]) -> Result<Catalog, String> {
        let named: Vec<(String, &str)> = files.iter().enumerate().map(|(i, t)| (format!("{i}.toml"), t.as_str())).collect();
        let refs: Vec<(&str, &str)> = named.iter().map(|(n, t)| (n.as_str(), *t)).collect();
        Catalog::load(&refs)
    }

    fn ids(list: &[&Component]) -> Vec<String> {
        list.iter().map(|c| c.id.clone()).collect()
    }

    /// Muxin's components, in their order.
    const ALL: [&str; 22] = [
        "git", "docker", "gh", "node", "uv", "python", "go", "rust", "php", "ruby", "java", "unzip", "bun", "deno", "dotnet", "claude", "codex", "gemini", "opencode", "amp", "pi", "tailscale",
    ];

    #[test]
    fn muxins_components_load() {
        let catalog = catalog().unwrap();
        assert_eq!(catalog.ids(), ALL);
        let claude = catalog.get("claude").unwrap();
        assert_eq!((claude.name.as_str(), claude.requires.as_slice(), claude.root), ("Claude Code", &["node".to_string()][..], true));
        assert!(claude.update.is_some() && catalog.get("git").unwrap().update.is_none());
        assert!(catalog.get("gh").unwrap().install.contains("cli.github.com"), "ported from cloud setup");
        let linux_only: Vec<&str> = catalog.components.iter().filter(|c| c.os != SYSTEMS).map(|c| c.id.as_str()).collect();
        assert_eq!(linux_only, ["docker", "python", "go", "php", "ruby", "java", "dotnet", "tailscale"]);
        assert!(catalog.components.iter().all(|c| c.os == SYSTEMS || c.os == ["linux"]));
        assert!(catalog.components.iter().all(|c| c.conflicts.is_empty()), "none yet");
        // Checks are fast and stay on the machine.
        assert!(catalog.components.iter().all(|c| !c.check.contains("curl") && !c.check.contains("http")));
    }

    /// The same ids give the same things as a cloud machine's extras and a
    /// VM's agent CLIs, now in one place.
    #[test]
    fn every_extra_is_a_component() {
        use crate::cloud::setup::{EXTRAS, npm_package};
        let catalog = catalog().unwrap();
        for (id, name) in EXTRAS {
            let c = catalog.get(id).unwrap_or_else(|| panic!("{id} is an extra but no component"));
            assert_eq!(c.name, *name);
            if let Some(package) = npm_package(id) {
                assert_eq!(c.install, format!("npm install -g {package}"), "{id}");
                // Updating looks at the copy on PATH, the command being the id.
                let at = format!("--prefix \"$(npm_prefix {id})\"");
                assert_eq!(c.update.as_deref(), Some(format!("npm install -g {at} {package}@latest").as_str()), "{id}");
                let name = package.rsplit(' ').next().unwrap();
                assert_eq!(c.up_to_date.as_deref(), Some(format!("npm outdated -g {at} {name}").as_str()), "{id}");
                // Root unless the user can write where the step writes: the copy on PATH, else npm's own folder.
                let root_unless = format!("test -w \"$(npm_prefix {id})\"");
                assert_eq!((c.requires.as_slice(), c.root, c.root_unless.as_deref()), (&["node".to_string()][..], true, Some(root_unless.as_str())), "{id}");
            }
        }
        // What the extras brought besides themselves.
        assert_eq!(catalog.get("python").unwrap().requires, ["uv"]);
        assert_eq!((catalog.get("bun").unwrap().requires.as_slice(), catalog.get("deno").unwrap().requires.as_slice()), (&["unzip".to_string()][..], &["unzip".to_string()][..]));
        // Pi's package is the one that isn't deprecated.
        assert!(catalog.get("pi").unwrap().install.ends_with(" @earendil-works/pi-coding-agent"));
    }

    #[test]
    fn requirements_come_first() {
        let catalog = catalog().unwrap();
        let resolve = |list: &[&str]| ids(&catalog.resolve(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap());
        assert_eq!(resolve(&["claude"]), ["node", "claude"]);
        assert_eq!(resolve(&["claude", "git", "node", "claude"]), ["node", "claude", "git"]);
        assert_eq!(resolve(&[]), ALL);
        assert_eq!(resolve(&["pi", "bun", "python", "deno", "codex"]), ["node", "pi", "unzip", "bun", "uv", "python", "deno", "codex"]);
        let a = load(&[component("a", "requires = [\"c\", \"b\"]"), component("b", "requires = [\"c\"]"), component("c", "")]).unwrap();
        assert_eq!(ids(&a.resolve(&["a".into()]).unwrap()), ["c", "b", "a"]);
        assert_eq!(catalog.resolve(&["podman".into()]).unwrap_err(), "No component called \"podman\".");
    }

    #[test]
    fn unknown_fields_are_refused_with_their_line() {
        let err = parse("x.toml", &component("x", "version = \"1\"")).unwrap_err();
        assert!(err.starts_with("x.toml, line 5: unknown field `version`"), "{err}");
        let err = parse("x.toml", &component("x", "conflicts = [{ id = \"y\", why = \"z\" }]")).unwrap_err();
        assert!(err.contains("line 5") && err.contains("unknown field `why`"), "{err}");
        assert!(parse("x.toml", "id = \"x\"\nname = \"X\"\ninstall = \"true\"\n").unwrap_err().contains("missing field `check`"));
    }

    #[test]
    fn ids_names_systems_and_steps_are_checked() {
        let refused = |text: String| parse("x.toml", &text).unwrap_err();
        assert!(refused(component("Git", "")).contains("isn't a component id"));
        assert!(refused(component("src/x", "")).contains("isn't a component id"), "source/id is kept for later");
        assert!(refused(component("x", "").replace("name = \"X\"", "name = \" \"")).contains("has no name"));
        assert!(refused(component("x", "update = \"\"")).contains("update step can't be empty"));
        // An update runs only where its up_to_date test fails, so it needs one.
        for extra in ["update = \"true\"", "up_to_date = \"true\"", "update = \"true\"\nup_to_date = \" \""] {
            assert!(refused(component("x", extra)).contains("an update step and an up_to_date test together, or neither"), "{extra}");
        }
        assert!(parse("x.toml", &component("x", "update = \"true\"\nup_to_date = \"true\"")).is_ok());
        assert!(refused(component("x", "").replace("check = \"true\"", "check = \"\"")).contains("needs a check"));
        assert!(refused(component("x", "os = [\"windows\"]")).contains("components run on linux and darwin"));
        assert!(refused(component("x", "os = []")).contains("works on no system"));
        assert!(refused(component("x", "requires = [\"x\"]")).contains("can't require \"x\""));
        assert_eq!(parse("x.toml", &component("x", "os = [\"darwin\"]")).unwrap().os, ["darwin"]);
    }

    #[test]
    fn steps_leave_sudo_to_root() {
        let err = parse("x.toml", &component("x", "update = \"sudo apt-get install -y x\"")).unwrap_err();
        assert!(err.contains("Set root = true instead"), "{err}");
        let err = parse("x.toml", &component("x", "root = true").replace("install = \"true\"", "install = \"/usr/bin/sudo -n apk add x\"")).unwrap_err();
        assert!(err.contains("run as root already"), "{err}");
        assert!(parse("x.toml", &component("x", "root = true").replace("install = \"true\"", "install = \"echo x > /etc/sudoers.d/x\"")).is_ok());
        let refused = |extra: &str| parse("x.toml", &component("x", extra)).unwrap_err();
        assert!(refused("root_unless = \"test -w /x\"").contains("needs root = true and a test"));
        assert!(refused("root = true\nroot_unless = \" \"").contains("needs root = true and a test"));
        assert!(refused("root = true\nroot_unless = \"sudo -n true\"").contains("run as root already"));
        assert_eq!(parse("x.toml", &component("x", "root = true\nroot_unless = \"test -w /x\"")).unwrap().root_unless.as_deref(), Some("test -w /x"));
    }

    #[test]
    fn catalogs_refuse_twins_unknown_requirements_and_circles() {
        let err = load(&[component("a", ""), component("a", "")]).unwrap_err();
        assert_eq!(err, "1.toml: there's another component called \"a\".");
        let err = load(&[component("a", "requires = [\"b\"]")]).unwrap_err();
        assert_eq!(err, "a requires \"b\", which isn't a component.");
        let err = load(&[component("a", "requires = [\"b\"]"), component("b", "requires = [\"c\"]"), component("c", "requires = [\"a\"]")]).unwrap_err();
        assert_eq!(err, "Components require each other in a circle: a → b → c → a.");
    }

    #[test]
    fn conflicts_have_a_reason_and_keep_components_apart() {
        let refused = |extra: &str| parse("x.toml", &component("x", extra)).unwrap_err();
        assert!(refused("conflicts = [{ id = \"y\", reason = \"\" }]").contains("without saying why"));
        assert!(refused("conflicts = [{ id = \"x\", reason = \"r\" }]").contains("which it is or requires"));
        assert!(refused("requires = [\"y\"]\nconflicts = [{ id = \"y\", reason = \"r\" }]").contains("which it is or requires"));
        let err = load(&[component("a", "requires = [\"b\"]\nconflicts = [{ id = \"c\", reason = \"r\" }]"), component("b", "requires = [\"c\"]"), component("c", "")]).unwrap_err();
        assert_eq!(err, "a conflicts with c, which it needs.");
        let reason = "conflicts = [{ id = \"b\", reason = \"both own the docker command\" }, { id = \"elsewhere\", reason = \"not in Muxin\" }]";
        let catalog = load(&[component("a", reason), component("b", ""), component("c", "requires = [\"b\"]")]).unwrap();
        assert_eq!(catalog.resolve(&["c".into(), "a".into()]).unwrap_err(), "B and A can't go together: both own the docker command");
        assert_eq!(ids(&catalog.resolve(&["a".into()]).unwrap()), ["a"]);
        // Applying looks for what a recipe can't go with, whichever side says so.
        let pick = |id: &str| vec![catalog.get(id).unwrap()];
        assert_eq!(ids(&catalog.conflicting(&pick("a"))), ["b"]);
        assert_eq!(ids(&catalog.conflicting(&pick("b"))), ["a"]);
        assert!(catalog.conflicting(&pick("c")).is_empty(), "c requires b; only a conflicts with b");
    }

    #[test]
    fn reads_what_the_checks_print() {
        let catalog = catalog().unwrap();
        let all = catalog.resolve(&["git".into(), "gh".into(), "claude".into()]).unwrap();
        let out = "Last login: today\nos=linux\ncheck.git=present\ncheck.gh=missing\ncheck.node=present\ncheck.claude=unsupported\n";
        let found = read_checks(out, &all).unwrap();
        assert_eq!(found.os, "linux");
        let results: Vec<(&str, &str)> = found.components.iter().map(|c| (c.id.as_str(), c.result)).collect();
        assert_eq!(results, [("git", "present"), ("gh", "missing"), ("node", "present"), ("claude", "unsupported")]);
        let err = read_checks("os=linux\ncheck.git=present\n", &all).unwrap_err();
        assert_eq!(err, "The check didn't say whether GitHub CLI is there.");
    }

    /// The script, run here: checks that fail, read stdin, exit or don't parse
    /// only answer for themselves.
    #[cfg(unix)]
    #[test]
    fn one_script_runs_every_check() {
        use std::io::Write;
        let here = if cfg!(target_os = "macos") { "darwin" } else { "linux" };
        let elsewhere = if here == "darwin" { "linux" } else { "darwin" };
        let files = [
            component("eats-stdin", "").replace("check = \"true\"", "check = \"cat; true\""),
            component("missing", "").replace("check = \"true\"", "check = \"exit 3\""),
            component("broken", "").replace("check = \"true\"", "check = \"if then fi (\""),
            component("there", "").replace("check = \"true\"", "check = '''\ncommand -v sh\n[ \"x$HOME\" != x ]\n'''"),
            component("other-system", &format!("os = [\"{elsewhere}\"]")),
        ];
        let catalog = load(&files).unwrap();
        let all = catalog.resolve(&[]).unwrap();
        let mut sh = std::process::Command::new("/bin/sh").arg("-s").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        sh.stdin.take().unwrap().write_all(check_script(&all).as_bytes()).unwrap();
        let out = sh.wait_with_output().unwrap();
        assert!(out.status.success());
        let found = read_checks(&String::from_utf8_lossy(&out.stdout), &all).unwrap();
        assert_eq!(found.os, here);
        let results: Vec<&str> = found.components.iter().map(|c| c.result).collect();
        assert_eq!(results, ["present", "missing", "missing", "present", "unsupported"]);
    }

    #[tokio::test]
    async fn recipe_check_in_the_demo() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-recipe-test")), None), true);
        engine.run_check(None).await;
        let session = Session::default();
        let tool = crate::mcp::tools().into_iter().find(|t| t["name"] == "recipe_check").expect("listed");
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["inputSchema"]["properties"]["components"]["items"]["enum"], json!(ALL));

        let mac = call_tool(&engine, &session, "recipe_check", json!({"machine_id": "forge", "components": ["claude"]})).await;
        assert_eq!(mac["isError"], false, "{mac}");
        assert_eq!(mac["structuredContent"]["os"], "darwin");
        assert_eq!(mac["structuredContent"]["components"], json!([{"id": "node", "name": "Node.js", "result": "present"}, {"id": "claude", "name": "Claude Code", "result": "present"}]));
        let windows = call_tool(&engine, &session, "recipe_check", json!({"machine_id": "win-test"})).await;
        let results: Vec<&str> = windows["structuredContent"]["components"].as_array().unwrap().iter().map(|c| c["result"].as_str().unwrap()).collect();
        assert_eq!((windows["structuredContent"]["os"].as_str(), results), (Some("windows"), vec!["unsupported"; ALL.len()]));
        let unknown = call_tool(&engine, &session, "recipe_check", json!({"machine_id": "forge", "components": ["podman"]})).await;
        assert_eq!(unknown["structuredContent"]["error"]["code"], "invalid", "{unknown}");
        assert_eq!(unknown["structuredContent"]["error"]["hint"], format!("Known components: {}", ALL.join(", ")));
        let nowhere = call_tool(&engine, &session, "recipe_check", json!({"machine_id": "nope"})).await;
        assert_eq!(nowhere["structuredContent"]["error"]["code"], "not_found");
    }

    /// As from the CLI: no machine has been checked yet.
    #[tokio::test]
    async fn the_demo_knows_a_machines_system_before_any_check() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-recipe-test-unchecked")), None), true);
        let four = catalog().unwrap().resolve(&["git".into(), "gh".into(), "claude".into(), "docker".into()]).unwrap();
        let mac = engine.recipe_check("forge", &four).await.unwrap();
        let results: Vec<&str> = mac.components.iter().map(|c| c.result).collect();
        assert_eq!((mac.os.as_str(), results), ("darwin", vec!["present", "present", "present", "present", "unsupported"]));
        let linux = engine.recipe_check("dev-1", &four).await.unwrap();
        let results: Vec<&str> = linux.components.iter().map(|c| c.result).collect();
        assert_eq!((linux.os.as_str(), results), ("linux", vec!["present", "present", "missing", "missing", "missing"]));
    }
}
