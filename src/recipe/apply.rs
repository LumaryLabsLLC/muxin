//! Applying a recipe to a machine: check, install in order only what's
//! missing, check each again. Asked to update too, it also runs the update
//! step of what's there and behind (its up_to_date test fails), and tests it
//! again. It's an operation like installing runners, one per machine at a
//! time. Steps run in the user's login shell, as the checks do and as
//! `run_command` runs commands, so they find the same tools (a Mac's
//! Homebrew, a user's mise or nvm). Nothing is removed or rolled back.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::oneshot;

use super::{Component, MachineCheck};
use crate::config::MachineSpec;
use crate::demo::World;
use crate::fleet::Engine;
use crate::fleet::sessions::agent;
use crate::fleet::workspaces::transport::{self, Limits};
use crate::ops::remote::{Param, sh_quote, with_params};
use crate::probe::{self, ProbeResult, Shell};

const APPLY_SH: &str = include_str!("scripts/apply.sh");
const PRELUDE_SH: &str = include_str!("scripts/prelude.sh");
/// How long one component's install may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long the up_to_date tests of a recipe may take together (they may ask the network).
const UP_TO_DATE_TIMEOUT: Duration = Duration::from_secs(120);
/// apply.sh's exit status when a step needs root and sudo wants a password: nothing ran.
const NEEDS_ROOT: i32 = 77;
/// What a Linux machine in the demo has to begin with, like Muxin's VMs. A Mac there has everything.
const DEMO_LINUX: &[&str] = &["git", "gh"];

/// What applying did with one component.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub id: String,
    pub name: String,
    /// "already" | "installed" | "failed" | "skipped" (not tried; `error` says why) | "unsupported" (no steps for this system),
    /// and when updating: "current" (there, and its up_to_date test passes) | "updated"
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Applied {
    fn new(c: &Component, result: &'static str, error: Option<String>, hint: Option<String>) -> Applied {
        Applied { id: c.id.clone(), name: c.name.clone(), result, error, hint }
    }

    fn there(&self) -> bool {
        matches!(self.result, "already" | "installed" | "current" | "updated")
    }

    /// Its line in the operation's log.
    fn line(&self) -> String {
        match self.result {
            "already" => format!("✓ {} was already there", self.name),
            "installed" => format!("✓ {} is installed", self.name),
            "current" => format!("✓ {} is up to date", self.name),
            "updated" => format!("✓ {} is updated", self.name),
            "unsupported" => format!("! {} has no steps for this system", self.name),
            other => format!("! {} {other}: {}", self.name, self.error.as_deref().unwrap_or_default()),
        }
    }
}

/// Its steps go through sudo: only on Linux (a Mac's run as the user, as Homebrew wants).
fn root_here(c: &Component, os: &str) -> bool {
    c.root && os == "linux"
}

fn root_refusal(c: &Component, machine: &str) -> (String, Option<String>) {
    (
        format!("It needs root, and sudo asks for a password on {machine}."),
        Some(format!("Allow passwordless sudo for your user on {machine}, or install {} there yourself.", c.name)),
    )
}

/// Why `c`, missing, can't be installed: one of `others` that it can't go
/// with is on the machine, a requirement isn't there, or it needs root that
/// sudo won't give without a password. Nothing of it runs then.
fn refusal(c: &Component, machine: &str, found: &MachineCheck, others: &[Component], done: &[Applied], sudo: Option<&str>) -> Option<(String, Option<String>)> {
    for o in others.iter().filter(|o| found.components.iter().any(|s| s.id == o.id && s.result == "present")) {
        if let Some(x) = c.conflicts.iter().find(|x| x.id == o.id).or_else(|| o.conflicts.iter().find(|x| x.id == c.id)) {
            return Some((format!("{} is on {machine}, and they can't go together: {}", o.name, x.reason), Some(format!("Remove {} from {machine} first, or leave {} out.", o.name, c.id))));
        }
    }
    if let Some(r) = done.iter().find(|d| c.requires.contains(&d.id) && !d.there()) {
        let why = match r.result {
            "unsupported" => "has no steps for this system",
            "failed" => "didn't install",
            _ => "wasn't installed",
        };
        return Some((format!("It needs {}, which {why}.", r.name), None));
    }
    // With root_unless, whether it needs root is only known on the machine, when it runs (apply.sh).
    if root_here(c, &found.os) && c.root_unless.is_none() && sudo == Some("password") {
        return Some(root_refusal(c, machine));
    }
    None
}

enum Outcome {
    Done,
    NeedsRoot,
    Failed(String),
}

/// Where steps run: on the machine, or in the demo's make-believe.
enum Target {
    Machine,
    Demo(Arc<World>),
}

impl Target {
    fn of(engine: &Engine) -> Target {
        match engine.demo_world() {
            Some(w) => Target::Demo(w.clone()),
            None => Target::Machine,
        }
    }

    /// "root" | "passwordless" | "password", from a machine check run now (the last one may be old).
    async fn sudo(&self, spec: &MachineSpec, probe: Option<ProbeResult>) -> Option<String> {
        match self {
            Target::Machine => probe::probe(spec, probe.and_then(|p| p.shell)).await.sudo,
            Target::Demo(w) => w.probe(spec).await.sudo,
        }
    }

    /// Runs `step`, `c`'s install or update.
    async fn run(&self, engine: &Arc<Engine>, op: u64, spec: &MachineSpec, c: &Component, step: &str, os: &str) -> Outcome {
        match self {
            Target::Machine => run_step(engine, op, spec, c, step, os).await,
            Target::Demo(w) => demo_run(w, engine, op, spec, c, os).await,
        }
    }

    /// The ids of `components` (there, each with an up_to_date test) that are behind.
    async fn behind(&self, spec: &MachineSpec, components: &[&Component]) -> Result<Vec<String>, String> {
        match self {
            Target::Machine => read_up_to_date(&agent::run_sh(spec, &up_to_date_script(components), UP_TO_DATE_TIMEOUT).await?, components),
            Target::Demo(w) => Ok(components.iter().filter(|c| !demo_current(w, &spec.id, c)).map(|c| c.id.clone()).collect()),
        }
    }
}

/// One script for the up_to_date tests of `components`, each in a shell of its
/// own as checks run, after the prelude as steps run: prints
/// `up_to_date.<id>=yes|no`. One without a test is never behind.
fn up_to_date_script(components: &[&Component]) -> String {
    let test = |c: &&Component| Some(format!("if sh -c {} </dev/null >/dev/null 2>&1; then echo up_to_date.{id}=yes; else echo up_to_date.{id}=no; fi\n", sh_quote(&format!("{PRELUDE_SH}\n{}", c.up_to_date.as_ref()?)), id = c.id));
    components.iter().filter_map(test).collect()
}

/// The ids the tests found behind.
fn read_up_to_date(out: &str, components: &[&Component]) -> Result<Vec<String>, String> {
    let mut behind = Vec::new();
    for c in components.iter().filter(|c| c.up_to_date.is_some()) {
        let key = format!("up_to_date.{}=", c.id);
        match out.lines().find_map(|l| l.trim().strip_prefix(&key)) {
            Some("yes") => {}
            Some("no") => behind.push(c.id.clone()),
            _ => return Err(format!("The test didn't say whether {} is up to date.", c.name)),
        }
    }
    Ok(behind)
}

/// Runs apply.sh with `step` on the machine, its output streaming into the operation's log.
async fn run_step(engine: &Arc<Engine>, op: u64, spec: &MachineSpec, c: &Component, step: &str, os: &str) -> Outcome {
    let params = [
        ("OR_ID", Param::Str(&c.id)),
        ("OR_PRELUDE", Param::Str(PRELUDE_SH)),
        ("OR_STEP", Param::Str(step)),
        ("OR_ROOT", Param::Bool(root_here(c, os))),
        ("OR_UNLESS", Param::Str(c.root_unless.as_deref().unwrap_or_default())),
    ];
    let script = with_params(Shell::Sh, &params, APPLY_SH);
    let marker = transport::new_marker();
    let log = engine.clone();
    let on_line = move |l: &str| log.op_log(op, l.to_string());
    match transport::run(spec, false, &marker, transport::with_marker(&marker, false, &script), STEP_TIMEOUT, Limits::OUTPUT, &[], Some(&on_line)).await {
        Err(e) => Outcome::Failed(e),
        Ok(o) if o.timed_out => Outcome::Failed(format!("Still running after {} minutes: stopped waiting.", STEP_TIMEOUT.as_secs() / 60)),
        Ok(o) if !o.started => Outcome::Failed(format!("{} is unreachable: {}", spec.name, o.reason())),
        Ok(o) => match o.exit {
            Some(0) => Outcome::Done,
            Some(NEEDS_ROOT) => Outcome::NeedsRoot,
            _ => Outcome::Failed(o.reason()),
        },
    }
}

/// The demo's install or update: made-up output, and it's there and up to date
/// afterwards. The demo's Node.js is the system's, so root_unless never lets a
/// step off root there.
async fn demo_run(w: &World, engine: &Arc<Engine>, op: u64, spec: &MachineSpec, c: &Component, os: &str) -> Outcome {
    pause(300).await;
    if root_here(c, os) {
        if w.probe(spec).await.sudo.as_deref() == Some("password") {
            engine.op_log(op, format!("error: {} needs root here, and sudo won't run without a password.", c.id));
            return Outcome::NeedsRoot;
        }
        engine.op_log(op, "As root (sudo -n)");
    }
    for line in [format!("Fetching {}…", c.name), format!("Setting up {}", c.id)] {
        engine.op_log(op, line);
        pause(200).await;
    }
    w.recipes.lock().unwrap().insert((spec.id.clone(), c.id.clone()));
    Outcome::Done
}

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// What a demo machine has: a Mac everything, a Linux machine DEMO_LINUX, and
/// both what was applied to them.
pub(super) fn demo_state(world: &World, machine_id: &str, os: &str, c: &Component) -> &'static str {
    if !c.os.iter().any(|o| o == os) {
        return "unsupported";
    }
    if os == "darwin" || DEMO_LINUX.contains(&c.id.as_str()) || demo_current(world, machine_id, c) { "present" } else { "missing" }
}

/// What the demo applied is up to date; what a machine had to begin with isn't.
fn demo_current(world: &World, machine_id: &str, c: &Component) -> bool {
    world.recipes.lock().unwrap().contains(&(machine_id.to_string(), c.id.clone()))
}

fn step(engine: &Arc<Engine>, op: u64, text: impl std::fmt::Display) {
    engine.op_log(op, format!("▸ {text}"));
}

/// What became of `c` once its install step ran, or its update step (`updating`
/// on `target`: then its up_to_date test runs again too).
async fn after(engine: &Arc<Engine>, spec: &MachineSpec, c: &Component, outcome: Outcome, updating: Option<&Target>) -> Applied {
    let (verb, done) = if updating.is_some() { ("update", "updated") } else { ("install", "installed") };
    match outcome {
        Outcome::NeedsRoot => {
            let (error, hint) = root_refusal(c, &spec.name);
            Applied::new(c, "skipped", Some(error), hint)
        }
        Outcome::Failed(e) => Applied::new(c, "failed", Some(e), None),
        Outcome::Done => match engine.recipe_check(&spec.id, &[c]).await {
            Ok(again) if again.components.iter().all(|s| s.result == "present") => match updating {
                None => Applied::new(c, done, None, None),
                Some(target) => match target.behind(spec, &[c]).await {
                    Ok(behind) if behind.is_empty() => Applied::new(c, done, None, None),
                    Ok(_) => Applied::new(c, "failed", Some("Its update step finished, but its up_to_date test still fails.".into()), c.up_to_date.as_ref().map(|t| format!("Its up_to_date test is: {}", t.trim()))),
                    Err(e) => Applied::new(c, "failed", Some(format!("Its update step finished, but testing it again didn't work: {e}")), None),
                },
            },
            Ok(_) => Applied::new(c, "failed", Some(format!("Its {verb} step finished, but its check still fails.")), Some(format!("Its check is: {}", c.check.trim()))),
            Err(e) => Applied::new(c, "failed", Some(format!("Its {verb} step finished, but checking it again didn't work: {e}")), None),
        },
    }
}

/// Checks, then installs each missing component in order and checks it again.
/// With `update`, what's there and behind is updated and tested again too.
#[allow(clippy::too_many_arguments)]
async fn apply(engine: &Arc<Engine>, op: u64, spec: &MachineSpec, probe: Option<ProbeResult>, components: &[Component], others: &[Component], update: bool, target: &Target) -> Result<Vec<Applied>, String> {
    step(engine, op, format!("Checking what {} has", spec.name));
    if let Target::Demo(w) = target {
        w.reachable(&spec.id, spec.host.trim())?;
    }
    let everything: Vec<&Component> = components.iter().chain(others).collect();
    let found = engine.recipe_check(&spec.id, &everything).await?;
    let state = |id: &str| found.components.iter().find(|s| s.id == id).map_or("missing", |s| s.result);
    let updatable: Vec<&Component> = components.iter().filter(|c| update && c.up_to_date.is_some() && state(&c.id) == "present").collect();
    let behind = if updatable.is_empty() {
        Vec::new()
    } else {
        step(engine, op, "Looking for updates");
        target.behind(spec, &updatable).await?
    };
    let root_wanted = components.iter().any(|c| root_here(c, &found.os) && (state(&c.id) == "missing" || behind.contains(&c.id)));
    let sudo = if root_wanted { target.sudo(spec, probe).await } else { None };

    let mut done: Vec<Applied> = Vec::new();
    for c in components {
        let a = match state(&c.id) {
            "present" if behind.contains(&c.id) =>match refusal(c, &spec.name, &found, &[], &done, sudo.as_deref()) {
                Some((error, hint)) => Applied::new(c, "skipped", Some(error), hint),
                None => {
                    step(engine, op, format!("Updating {}", c.name));
                    let outcome = target.run(engine, op, spec, c, c.update.as_deref().unwrap_or_default(), &found.os).await;
                    after(engine, spec, c, outcome, Some(target)).await
                }
            },
            "present" if updatable.iter().any(|u| u.id == c.id) => Applied::new(c, "current", None, None),
            "present" => Applied::new(c, "already", None, None),
            "unsupported" => Applied::new(c, "unsupported", None, None),
            _ => match refusal(c, &spec.name, &found, others, &done, sudo.as_deref()) {
                Some((error, hint)) => Applied::new(c, "skipped", Some(error), hint),
                None => {
                    step(engine, op, format!("Installing {}", c.name));
                    let outcome = target.run(engine, op, spec, c, &c.install, &found.os).await;
                    after(engine, spec, c, outcome, None).await
                }
            },
        };
        engine.op_log(op, a.line());
        done.push(a);
    }
    Ok(done)
}

impl Engine {
    /// Starts applying `components` (from `Catalog::resolve`, in that order)
    /// to a machine. `others` are the components they can't go with
    /// (`Catalog::conflicting`): finding one on the machine skips what it
    /// conflicts with. With `update`, what's there and behind is updated too.
    /// Returns the operation id, and each component's result once it's done
    /// (the operation fails when any is failed or skipped).
    pub fn recipe_apply(self: &Arc<Self>, machine_id: &str, components: Vec<Component>, others: Vec<Component>, update: bool) -> Result<(u64, oneshot::Receiver<Vec<Applied>>), String> {
        let (spec, probe) = self.machine(machine_id).ok_or("That machine isn't in config.toml anymore.")?;
        if !spec.is_checkable() {
            return Err(format!("{} has no host in config.toml, so Muxin can't reach it.", spec.name));
        }
        if self.machine_busy(&spec.id) {
            return Err(format!("Another operation is already changing {}. Wait for it to finish.", spec.name));
        }
        let ids: Vec<&str> = components.iter().map(|c| c.id.as_str()).collect();
        let title = if update { format!("Apply and update {} on {}", ids.join(", "), spec.name) } else { format!("Apply {} to {}", ids.join(", "), spec.name) };
        let op = self.begin_op("recipe", title, Some(spec.id.clone()), None);
        let target = Target::of(self);
        let (tx, rx) = oneshot::channel();
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = apply(&me, op, &spec, probe, &components, &others, update, &target).await.and_then(|done| {
                let missed: Vec<String> = done.iter().filter(|a| matches!(a.result, "failed" | "skipped")).map(|a| a.name.clone()).collect();
                let _ = tx.send(done);
                let verb = if update { "install or update" } else { "install" };
                if missed.is_empty() { Ok(()) } else { Err(format!("{} didn't {verb}.", missed.join(", "))) }
            });
            if let Err(e) = &result {
                me.op_log(op, format!("error: {e}"));
            }
            me.op_finish(op, result);
        });
        Ok((op, rx))
    }

    /// Applies `ids` to a machine Muxin has just made and added (cloud_create,
    /// vm_create), inside the operation making it, once it answers ssh. As
    /// with a cloud machine's extras, what doesn't install is a warning in
    /// that log, and the machine is still ready.
    pub async fn recipe_setup(self: &Arc<Self>, op: u64, machine_id: &str, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let done = async {
            let catalog = super::catalog()?;
            let components = catalog.resolve(ids)?;
            let others: Vec<Component> = catalog.conflicting(&components).into_iter().cloned().collect();
            let components: Vec<Component> = components.into_iter().cloned().collect();
            let (spec, probe) = self.machine(machine_id).ok_or("The new machine went missing from config.toml.")?;
            apply(self, op, &spec, probe, &components, &others, false, &Target::of(self)).await
        }
        .await;
        match done {
            Ok(done) => {
                let missed: Vec<&Applied> = done.iter().filter(|a| matches!(a.result, "failed" | "skipped")).collect();
                if !missed.is_empty() {
                    let names: Vec<&str> = missed.iter().map(|a| a.name.as_str()).collect();
                    let ids: Vec<&str> = missed.iter().map(|a| a.id.as_str()).collect();
                    self.op_log(op, format!("! {} didn't install. Try again with `muxin recipe apply {machine_id} {}`.", names.join(", "), ids.join(" ")));
                }
            }
            Err(e) => self.op_log(op, format!("! Nothing was installed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::mcp::{Session, call_tool};
    use crate::recipe::{ComponentState, catalog, parse};
    use serde_json::{Value, json};

    fn component(id: &str, extra: &str) -> Component {
        parse("x.toml", &format!("id = \"{id}\"\nname = \"{}\"\ncheck = \"true\"\ninstall = \"true\"\n{extra}\n", id.to_uppercase())).unwrap()
    }

    fn found(os: &str, states: &[(&str, &'static str)]) -> MachineCheck {
        MachineCheck { os: os.into(), components: states.iter().map(|(id, result)| ComponentState { id: id.to_string(), name: id.to_uppercase(), result }).collect() }
    }

    #[test]
    fn refusals_come_before_anything_runs() {
        let docker = component("docker", "conflicts = [{ id = \"podman\", reason = \"both own the docker command\" }]");
        let podman = component("podman", "");
        let on_it = found("linux", &[("docker", "missing"), ("podman", "present")]);
        let (error, hint) = refusal(&docker, "box", &on_it, std::slice::from_ref(&podman), &[], None).unwrap();
        assert_eq!(error, "PODMAN is on box, and they can't go together: both own the docker command");
        assert_eq!(hint.as_deref(), Some("Remove PODMAN from box first, or leave docker out."));
        // Declared on the other side, it counts the same; with podman missing, nothing stands in the way.
        assert!(refusal(&podman, "box", &found("linux", &[("docker", "present")]), std::slice::from_ref(&docker), &[], None).is_some());
        assert!(refusal(&docker, "box", &found("linux", &[("podman", "missing")]), &[podman], &[], None).is_none());

        let app = component("app", "requires = [\"lib\"]");
        let lib = |result| Applied { id: "lib".into(), name: "LIB".into(), result, error: None, hint: None };
        assert_eq!(refusal(&app, "box", &found("linux", &[]), &[], &[lib("failed")], None).unwrap().0, "It needs LIB, which didn't install.");
        assert!(refusal(&app, "box", &found("linux", &[]), &[], &[lib("already")], Some("password")).is_none());

        let root = component("tool", "root = true");
        let (error, hint) = refusal(&root, "box", &found("linux", &[]), &[], &[], Some("password")).unwrap();
        assert_eq!(error, "It needs root, and sudo asks for a password on box.");
        assert!(hint.unwrap().starts_with("Allow passwordless sudo for your user on box"));
        assert!(refusal(&root, "box", &found("linux", &[]), &[], &[], Some("passwordless")).is_none());
        assert!(refusal(&root, "box", &found("darwin", &[]), &[], &[], Some("password")).is_none(), "a Mac's steps run as the user");
        let npm = component("tool", "root = true\nroot_unless = \"test -w /x\"");
        assert!(refusal(&npm, "box", &found("linux", &[]), &[], &[], Some("password")).is_none(), "decided when it runs");
    }

    /// apply.sh, run here by /bin/sh with stand-ins for sudo and apt-get: who
    /// a step runs as, that it can't eat the script, stops at its first failure,
    /// and what pkg_install does. Nothing real is installed.
    #[cfg(unix)]
    #[test]
    fn the_step_runs_as_the_user_as_root_or_not_at_all() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("muxen-recipe-apply-{}", std::process::id()));
        let bin = dir.join("bin");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&bin).unwrap();
        let write = |name: &str, text: &str| {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{text}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // Only these on PATH, so no real package manager can run.
        for tool in ["sh", "id", "cat", "touch", "echo"] {
            let real = ["/bin", "/usr/bin"].iter().map(|d| std::path::Path::new(d).join(tool)).find(|p| p.exists()).unwrap();
            std::os::unix::fs::symlink(real, bin.join(tool)).unwrap();
        }
        let log = dir.join("log");
        let sudo_ok = format!("echo \"sudo $*\" >> {0}\n[ \"$1\" = -n ] && shift\n[ \"$1\" = true ] && exit 0\nexec \"$@\"", log.display());
        // apt-get that can't find anything until its lists are updated.
        write("apt-get", &format!("echo \"apt-get $*\" >> {0}\ncase \"$*\" in *update*) touch {0}.updated ;; *install*) [ -f {0}.updated ] ;; esac", log.display()));

        let run = |root: bool, unless: &str, step: &str| -> (String, String) {
            let script = with_params(Shell::Sh, &[("OR_ID", Param::Str("x")), ("OR_PRELUDE", Param::Str(PRELUDE_SH)), ("OR_STEP", Param::Str(step)), ("OR_ROOT", Param::Bool(root)), ("OR_UNLESS", Param::Str(unless))], APPLY_SH);
            let _ = std::fs::remove_file(&log);
            let mut sh = std::process::Command::new("/bin/sh").arg("-s").env_clear().env("PATH", &bin).env("OR_MARK", "M").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            sh.stdin.take().unwrap().write_all(format!("{script}echo after-the-step\n").as_bytes()).unwrap();
            let out = sh.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let exit = String::from_utf8_lossy(&out.stderr).lines().find_map(|l| l.strip_prefix("M:exit=").map(str::to_string)).unwrap_or_default();
            (format!("{stdout}{}", std::fs::read_to_string(&log).unwrap_or_default()), exit)
        };

        // As the user: it can't read the script (stdin is empty), and it stops at its first failure.
        let (out, exit) = run(false, "", "echo one\ncat\nfalse\necho two");
        assert_eq!(exit, "1", "{out}");
        assert!(out.contains("one") && !out.contains("two") && out.contains("after-the-step"), "{out}");
        // root_unless that passes: as the user, sudo never called.
        write("sudo", &sudo_ok);
        let (out, exit) = run(true, "true", "echo ran");
        assert_eq!(exit, "0");
        assert!(out.contains("As ") && out.contains("ran") && !out.contains("sudo "), "{out}");
        // Root through sudo -n, the prelude with it.
        let (out, exit) = run(true, "false", "pkg_install git");
        assert_eq!(exit, "0", "{out}");
        assert!(out.contains("As root (sudo -n)") && out.contains("sudo -n sh -ec "), "{out}");
        assert!(out.contains("apt-get -o DPkg::Lock::Timeout=600 install -y git\napt-get -o DPkg::Lock::Timeout=600 update\napt-get -o DPkg::Lock::Timeout=600 install -y git"), "lists refreshed once: {out}");
        // sudo wants a password: 77, and the step didn't run.
        write("sudo", &format!("echo \"sudo $*\" >> {}\nexit 1", log.display()));
        let (out, exit) = run(true, "", "echo ran");
        assert_eq!(exit, "77");
        assert!(!out.contains("ran") && out.contains("sudo -n true"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Boxed: a debug build's dispatch future is big, and the test awaits it many times.
    async fn apply_tool(engine: &Arc<Engine>, args: Value) -> Value {
        Box::pin(call_tool(engine, &Session::default(), "recipe_apply", args)).await
    }

    fn results(v: &Value) -> Vec<(String, String)> {
        v["components"].as_array().unwrap().iter().map(|c| (c["id"].as_str().unwrap().to_string(), c["result"].as_str().unwrap().to_string())).collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[tokio::test]
    async fn recipe_apply_in_the_demo() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-recipe-apply-test")), None), true);
        engine.run_check(None).await;
        let tool = crate::mcp::tools().into_iter().find(|t| t["name"] == "recipe_apply").expect("listed");
        assert_eq!((tool["annotations"]["readOnlyHint"].as_bool(), tool["annotations"]["idempotentHint"].as_bool()), (Some(false), Some(true)));
        assert_eq!(tool["inputSchema"]["required"], json!(["machine_id", "components"]));
        assert!(tool["description"].as_str().unwrap().contains("confirm with the user"));

        // A Linux machine with passwordless sudo: Node.js first, then Claude Code; a second run changes nothing.
        let first = apply_tool(&engine, json!({"machine_id": "dev-1", "components": ["claude"]})).await;
        assert_eq!(first["isError"], false, "{first}");
        assert_eq!(results(&first["structuredContent"]), pairs(&[("node", "installed"), ("claude", "installed")]));
        let log: Vec<&str> = first["structuredContent"]["operation"]["log_tail"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        assert!(log.contains(&"▸ Installing Node.js") && log.contains(&"✓ Claude Code is installed"), "{log:?}");
        let again = apply_tool(&engine, json!({"machine_id": "dev-1", "components": ["git", "claude"]})).await;
        assert_eq!(results(&again["structuredContent"]), pairs(&[("git", "already"), ("node", "already"), ("claude", "already")]));
        let check = Box::pin(call_tool(&engine, &Session::default(), "recipe_check", json!({"machine_id": "dev-1", "components": ["claude"]}))).await;
        assert_eq!(check["structuredContent"]["components"][1]["result"], "present");

        // sudo wants a password: Node.js is skipped up front, and Claude Code with it.
        let refused = apply_tool(&engine, json!({"machine_id": "beacon", "components": ["claude"]})).await;
        let err = &refused["structuredContent"]["error"];
        assert_eq!((refused["isError"].as_bool(), err["code"].as_str()), (Some(true), Some("operation_failed")), "{refused}");
        assert_eq!(err["message"], "Node.js, Claude Code didn't install.");
        assert!(err["hint"].as_str().unwrap().starts_with("Allow passwordless sudo for your user on Beacon"));
        assert_eq!(results(&err["details"]), pairs(&[("node", "skipped"), ("claude", "skipped")]));
        assert_eq!(err["details"]["components"][1]["error"], "It needs Node.js, which wasn't installed.");
        assert!(!err["details"]["operation"]["log_tail"].as_array().unwrap().iter().any(|l| l == "▸ Installing Node.js"));

        // Windows: nothing to run there. A Mac in the demo has everything.
        let windows = apply_tool(&engine, json!({"machine_id": "win-test", "components": ["git"]})).await;
        assert_eq!(results(&windows["structuredContent"]), pairs(&[("git", "unsupported")]));
        let mac = apply_tool(&engine, json!({"machine_id": "forge", "components": ["gh"]})).await;
        assert_eq!(results(&mac["structuredContent"]), pairs(&[("gh", "already")]));
        let away = apply_tool(&engine, json!({"machine_id": "harbor", "components": ["gh"]})).await;
        assert_eq!(away["structuredContent"]["error"]["code"], "operation_failed", "{away}");

        // Refused before any operation starts.
        let none = apply_tool(&engine, json!({"machine_id": "dev-1", "components": []})).await;
        assert_eq!(none["structuredContent"]["error"]["code"], "invalid");
        assert!(none["structuredContent"]["error"]["hint"].as_str().unwrap().starts_with("Known components: git, docker, gh, node,"), "{none}");
        let unknown = apply_tool(&engine, json!({"machine_id": "dev-1", "components": ["podman"]})).await;
        assert_eq!(unknown["structuredContent"]["error"]["code"], "invalid");
        let no_wait = apply_tool(&engine, json!({"machine_id": "pebble", "components": ["claude"], "wait": false})).await;
        assert_eq!((no_wait["structuredContent"]["finished"].as_bool(), results(&no_wait["structuredContent"]).len()), (Some(false), 0));
        let busy = apply_tool(&engine, json!({"machine_id": "pebble", "components": ["gh"]})).await;
        assert_eq!(busy["structuredContent"]["error"]["code"], "busy", "{busy}");
        assert!(catalog().unwrap().get("claude").unwrap().root_unless.is_some());
    }

    /// The up_to_date tests' script, run here: each answers for itself, and one
    /// without a test is never behind.
    #[cfg(unix)]
    #[test]
    fn one_script_runs_every_up_to_date_test() {
        use std::io::Write;
        let tested = |id: &str, test: &str| component(id, &format!("update = \"true\"\nup_to_date = \"{test}\""));
        let list = [tested("new", "true"), tested("old", "exit 1"), tested("eats-stdin", "cat; true"), tested("broken", "if then fi ("), tested("prelude", "command -v npm_prefix"), component("no-test", "")];
        let all: Vec<&Component> = list.iter().collect();
        let mut sh = std::process::Command::new("/bin/sh").arg("-s").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        sh.stdin.take().unwrap().write_all(up_to_date_script(&all).as_bytes()).unwrap();
        let out = sh.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(read_up_to_date(&String::from_utf8_lossy(&out.stdout), &all).unwrap(), ["old", "broken"]);
        assert_eq!(read_up_to_date("up_to_date.new=yes\n", &all[..2]).unwrap_err(), "The test didn't say whether OLD is up to date.");
    }

    /// The prelude's npm_prefix, run here with a stand-in npm: the prefix of an
    /// npm copy found on PATH (where a new Node.js's npm wouldn't look), else npm's own.
    #[cfg(unix)]
    #[test]
    fn npm_prefix_finds_the_copy_that_runs() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("muxen-recipe-npm-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (old, own) = (dir.join("old"), dir.join("own"));
        let package = old.join("lib/node_modules/@x/tool/bin");
        for d in [&package, &old.join("bin"), &own] {
            std::fs::create_dir_all(d).unwrap();
        }
        let executable = |p: std::path::PathBuf, text: &str| {
            std::fs::write(&p, format!("#!/bin/sh\n{text}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        executable(package.join("tool.js"), "true");
        symlink("../lib/node_modules/@x/tool/bin/tool.js", old.join("bin/tool")).unwrap();
        executable(own.join("npm"), "[ \"$*\" = \"prefix -g\" ] && echo /npm-own");
        executable(own.join("plain"), "true");
        let path = format!("{}:{}:/usr/bin:/bin", old.join("bin").display(), own.display());
        let script = format!("{PRELUDE_SH}\nnpm_prefix tool; npm_prefix plain; npm_prefix missing");
        let out = std::process::Command::new("/bin/sh").args(["-ec", &script]).env("PATH", path).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let old = std::fs::canonicalize(&old).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{}\n/npm-own\n/npm-own\n", old.display()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In the demo, what a machine had to begin with is behind, and what was applied is up to date.
    #[tokio::test]
    async fn recipe_apply_updates_only_whats_behind_in_the_demo() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-recipe-update-test")), None), true);
        engine.run_check(None).await;
        let tool = crate::mcp::tools().into_iter().find(|t| t["name"] == "recipe_apply").expect("listed");
        assert_eq!(tool["inputSchema"]["properties"]["update"]["type"], "boolean");
        let log = |v: &Value| v["operation"]["log_tail"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect::<Vec<_>>();

        // A Mac in the demo has every component: those with an update step are updated, the rest left alone.
        let first = apply_tool(&engine, json!({"machine_id": "forge", "components": ["git", "claude", "codex"], "update": true})).await;
        assert_eq!(first["isError"], false, "{first}");
        let out = &first["structuredContent"];
        assert_eq!(results(out), pairs(&[("git", "already"), ("node", "already"), ("claude", "updated"), ("codex", "updated")]));
        assert_eq!(out["operation"]["title"], "Apply and update git, node, claude, codex on Forge");
        let lines = log(out);
        assert!(lines.contains(&"▸ Looking for updates".into()) && lines.contains(&"▸ Updating Codex".into()) && lines.contains(&"✓ Claude Code is updated".into()), "{lines:?}");
        // A second run changes nothing; without update, nothing is even asked.
        let again = apply_tool(&engine, json!({"machine_id": "forge", "components": ["claude", "codex"], "update": true})).await;
        assert_eq!(results(&again["structuredContent"]), pairs(&[("node", "already"), ("claude", "current"), ("codex", "current")]));
        assert!(!log(&again["structuredContent"]).iter().any(|l| l.starts_with("▸ Updating")), "{again}");
        assert!(log(&again["structuredContent"]).contains(&"✓ Claude Code is up to date".into()));
        let plain = apply_tool(&engine, json!({"machine_id": "forge", "components": ["pi"]})).await;
        assert_eq!(results(&plain["structuredContent"]), pairs(&[("node", "already"), ("pi", "already")]));
        assert!(!log(&plain["structuredContent"]).contains(&"▸ Looking for updates".into()));

        // What's missing is installed, not updated, and then it's up to date.
        let linux = apply_tool(&engine, json!({"machine_id": "dev-1", "components": ["claude"], "update": true})).await;
        assert_eq!(results(&linux["structuredContent"]), pairs(&[("node", "installed"), ("claude", "installed")]));
        let linux = apply_tool(&engine, json!({"machine_id": "dev-1", "components": ["claude"], "update": true})).await;
        assert_eq!(results(&linux["structuredContent"]), pairs(&[("node", "already"), ("claude", "current")]));
    }

    /// The log of the operation `op`.
    fn op_log(engine: &Arc<Engine>, op: u64) -> Vec<String> {
        engine.republish();
        engine.current().operations.iter().find(|o| o.id == op).map(|o| o.log.clone()).unwrap_or_default()
    }

    /// As cloud_create and vm_create run it, inside their own operation.
    #[tokio::test]
    async fn a_new_machine_gets_its_recipe_inside_the_operation_making_it() {
        let engine = Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-recipe-setup-test")), None), true);
        engine.run_check(None).await;
        let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let op = engine.begin_op("vm-create", "Create dev-1".into(), None, None);
        Box::pin(engine.recipe_setup(op, "dev-1", &ids(&["codex", "gh"]))).await;
        let log = op_log(&engine, op);
        let at = |line: &str| log.iter().position(|l| l == line).unwrap_or_else(|| panic!("no {line:?} in {log:?}"));
        assert!(at("✓ Node.js is installed") < at("✓ Codex is installed"), "{log:?}");
        assert!(log.contains(&"✓ GitHub CLI was already there".to_string()) && !log.iter().any(|l| l.starts_with('!')), "{log:?}");
        assert_eq!(engine.current().operations.iter().find(|o| o.id == op).unwrap().status, "running", "the caller finishes it");

        // sudo wants a password: a warning with the way to try again, and nothing ran.
        let op = engine.begin_op("cloud-create", "Create beacon".into(), None, None);
        Box::pin(engine.recipe_setup(op, "beacon", &ids(&["pi"]))).await;
        let log = op_log(&engine, op);
        assert_eq!(log.last().map(String::as_str), Some("! Node.js, Pi didn't install. Try again with `muxin recipe apply beacon node pi`."), "{log:?}");
        assert!(!log.iter().any(|l| l.starts_with("▸ Installing")), "{log:?}");

        // Nothing asked for: nothing to do, not even a check. Unknown ids were refused before the machine was made.
        let op = engine.begin_op("vm-create", "Create pebble".into(), None, None);
        Box::pin(engine.recipe_setup(op, "pebble", &[])).await;
        assert!(op_log(&engine, op).is_empty());
        Box::pin(engine.recipe_setup(op, "pebble", &ids(&["podman"]))).await;
        assert_eq!(op_log(&engine, op), ["! Nothing was installed: No component called \"podman\"."]);
    }
}
