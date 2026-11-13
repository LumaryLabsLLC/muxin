//! The app's keyboard shortcuts: what each one does, its keys by default, and
//! the user's own keys from Settings (saved in config.toml). ⌘1…⌘9 go to the
//! machines in the sidebar; holding ⌘ shows every shortcut where it applies.
//! A preset sets them all at once: herdr's keys, for people coming from it.

use gpui::{App, KeyBinding, Keystroke};

use super::root::{
    AddMachine, CheckNow, ClosePane, CommandPalette, EndSession, FocusFilter, GoBack, GoForward, GoToMachine1, GoToMachine2, GoToMachine3, GoToMachine4, GoToMachine5,
    GoToMachine6, GoToMachine7, GoToMachine8, GoToMachine9, GoToSession1, GoToSession2, GoToSession3, GoToSession4, GoToSession5, GoToSession6, GoToSession7,
    GoToSession8, GoToSession9, NewSession, NextAgent, NextPane, NextSession, OpenSettings, PreviousAgent, PreviousPane, PreviousSession, RenameShown, ShowGithub,
    ShowChat, ShowJobs, ShowRun, ShowSchedules, ShowUsage, ShowOverview, ShowWorkspaces, ShowBoxes, SplitDown, SplitRight, SyncGitHub, QuickOpen, ToggleActivity, ToggleSidebar,
    ZoomPane,
};
use super::theme::Appearance;

/// One shortcut the user can change.
pub struct Shortcut {
    pub id: &'static str,
    pub label: &'static str,
    /// GPUI key syntax ("secondary-r": ⌘R on a Mac, Ctrl+R elsewhere).
    pub default: &'static str,
    bind: fn(&str) -> KeyBinding,
}

macro_rules! shortcut {
    ($id:literal, $label:literal, $default:expr, $action:expr) => {
        Shortcut { id: $id, label: $label, default: $default, bind: |keys| KeyBinding::new(keys, $action, None) }
    };
}

/// The pages: ⌃1…⌃4 on a Mac (⌘1…⌘9 are the machines); Alt elsewhere, where
/// Ctrl is what ⌘ is on a Mac.
const fn page(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") { mac } else { other }
}

/// In the order Settings lists them.
pub const ALL: &[Shortcut] = &[
    shortcut!("quick-open", "Go to a machine, session or desktop", "secondary-p", QuickOpen),
    shortcut!("commands", "Command palette", "secondary-shift-p", CommandPalette),
    shortcut!("chat", "Chat", page("ctrl-0", "alt-0"), ShowChat),
    shortcut!("overview", "Overview", page("ctrl-1", "alt-1"), ShowOverview),
    shortcut!("jobs", "Jobs", page("ctrl-2", "alt-2"), ShowJobs),
    shortcut!("boxes", "Boxes", page("ctrl-3", "alt-3"), ShowBoxes),
    shortcut!("github", "GitHub", page("ctrl-4", "alt-4"), ShowGithub),
    shortcut!("usage", "Usage", page("ctrl-5", "alt-5"), ShowUsage),
    shortcut!("run", "Run on machines", page("ctrl-6", "alt-6"), ShowRun),
    shortcut!("schedules", "Schedules", page("ctrl-7", "alt-7"), ShowSchedules),
    shortcut!("workspaces", "Agents' workspaces", "", ShowWorkspaces),
    shortcut!("activity", "Activity", "secondary-j", ToggleActivity),
    shortcut!("settings", "Settings", "secondary-,", OpenSettings),
    shortcut!("check", "Refresh all machines", "secondary-r", CheckNow),
    shortcut!("sync", "Sync GitHub", "secondary-shift-r", SyncGitHub),
    shortcut!("add-machine", "Add machine", "secondary-n", AddMachine),
    shortcut!("new-session", "New session", "secondary-t", NewSession),
    shortcut!("split-right", "Split the terminal right", "secondary-d", SplitRight),
    shortcut!("split-down", "Split the terminal down", "secondary-shift-d", SplitDown),
    shortcut!("next-pane", "Next pane", "secondary-alt-right", NextPane),
    shortcut!("previous-pane", "Previous pane", "secondary-alt-left", PreviousPane),
    shortcut!("zoom-pane", "Show the pane alone, or the whole split", "secondary-shift-enter", ZoomPane),
    shortcut!("close-pane", "Close the pane (the session goes on)", "secondary-shift-w", ClosePane),
    shortcut!("next-session", "Next session", "secondary-shift-]", NextSession),
    shortcut!("previous-session", "Previous session", "secondary-shift-[", PreviousSession),
    shortcut!("next-agent", "Next agent (not shells)", "", NextAgent),
    shortcut!("previous-agent", "Previous agent", "", PreviousAgent),
    shortcut!("rename", "Rename the machine or session on screen", "", RenameShown),
    shortcut!("end-session", "End the session on screen", "", EndSession),
    shortcut!("filter", "Search machines and sessions", "secondary-f", FocusFilter),
    shortcut!("sidebar", "Hide or show the sidebar", "secondary-b", ToggleSidebar),
    shortcut!("back", "Back", "secondary-[", GoBack),
    shortcut!("forward", "Forward", "secondary-]", GoForward),
];

/// The machines' shortcuts: this modifier with 1…9 (in Appearance under this id).
pub const MACHINES: &str = "machines";
/// The sessions of the machine on screen: this modifier with 1…9 (off unless
/// the user or a preset turns it on).
pub const SESSIONS: &str = "sessions";
/// The shortcuts that are a modifier with 1…9.
pub const GROUPS: [&str; 2] = [MACHINES, SESSIONS];

fn group_default(group: &str) -> &'static str {
    if group == MACHINES { "secondary" } else { "" }
}

pub fn is_group(id: &str) -> bool {
    GROUPS.contains(&id)
}

/// "Machine 3" or "Session 3".
fn group_item(group: &str, n: usize) -> String {
    format!("{} {n}", if group == MACHINES { "Machine" } else { "Session" })
}

/// Choices for a group's modifier: (keys, how Settings shows them).
pub fn machine_modifiers() -> &'static [(&'static str, &'static str)] {
    if cfg!(target_os = "macos") {
        &[("cmd", "⌘"), ("ctrl", "⌃"), ("alt-cmd", "⌥⌘"), ("", "Off")]
    } else {
        &[("ctrl", "Ctrl"), ("alt", "Alt"), ("", "Off")]
    }
}

/// The keys for a shortcut now ("" when it has none).
pub fn keys(cx: &App, id: &str) -> String {
    let a = cx.global::<Appearance>();
    match a.shortcuts.get(id) {
        Some(k) => k.clone(),
        None if is_group(id) => group_default(id).into(),
        None => ALL.iter().find(|s| s.id == id).map(|s| s.default.to_string()).unwrap_or_default(),
    }
}

/// A group's modifier, normalized ("" when it's off).
pub fn group_modifier(cx: &App, group: &str) -> String {
    modifier(&keys(cx, group))
}

fn modifier(m: &str) -> String {
    match m {
        "secondary" if cfg!(target_os = "macos") => "cmd".into(),
        "secondary" => "ctrl".into(),
        other => other.into(),
    }
}

/// The keys that go to the n-th machine (1-based), if any.
pub fn machine_keys(cx: &App, n: usize) -> Option<String> {
    group_keys(cx, MACHINES, n)
}

/// The keys that go to a group's n-th item (1-based), if any.
pub fn group_keys(cx: &App, group: &str, n: usize) -> Option<String> {
    let m = group_modifier(cx, group);
    (!m.is_empty() && (1..=9).contains(&n)).then(|| format!("{m}-{n}"))
}

/// What already uses `keys`, other than `except`: a shortcut's id and label,
/// or (MACHINES, "Machine 3") for a group's.
pub fn used_by(cx: &App, keys_: &str, except: &str) -> Option<(&'static str, String)> {
    let k = normalize(keys_)?;
    for group in GROUPS.into_iter().filter(|g| *g != except) {
        if let Some(n) = (1..=9).find(|n| group_keys(cx, group, *n).and_then(|m| normalize(&m)).as_deref() == Some(k.as_str())) {
            return Some((group, group_item(group, n)));
        }
    }
    ALL.iter().filter(|s| s.id != except).find(|s| normalize(&keys(cx, s.id)).as_deref() == Some(k.as_str())).map(|s| (s.id, s.label.to_string()))
}

/// Sets a shortcut's keys ("" for none), keeping only what differs from the default.
pub fn set(cx: &mut App, id: &str, keys_: &str) {
    let same = is_default(id, keys_);
    super::theme::remember(cx, |a| {
        if same {
            a.shortcuts.remove(id);
        } else {
            a.shortcuts.insert(id.to_string(), keys_.to_string());
        }
    });
}

/// Whether `keys_` are what `id` has by default.
fn is_default(id: &str, keys_: &str) -> bool {
    if is_group(id) {
        modifier(keys_) == modifier(group_default(id))
    } else {
        normalize(keys_) == ALL.iter().find(|s| s.id == id).and_then(|s| normalize(s.default))
    }
}

/// Every id a preset sets: the shortcuts and the groups.
fn all_ids() -> impl Iterator<Item = &'static str> {
    ALL.iter().map(|s| s.id).chain(GROUPS)
}

/// A set of keys for every shortcut: (name, keys by id). A shortcut the
/// preset leaves out keeps its default.
pub struct Preset {
    pub name: &'static str,
    keys: &'static [(&'static str, &'static str)],
}

impl Preset {
    fn keys_for(&self, id: &str) -> &'static str {
        match self.keys.iter().find(|(i, _)| *i == id) {
            Some((_, k)) => k,
            None if is_group(id) => group_default(id),
            None => ALL.iter().find(|s| s.id == id).map(|s| s.default).unwrap_or_default(),
        }
    }
}

/// Muxin's own keys, and herdr's (its desktop app's) where Muxin does the
/// same thing: a herdr space is a machine, a tab a session.
pub const PRESETS: &[Preset] = &[Preset { name: "Muxin", keys: &[] }, Preset { name: "herdr", keys: HERDR }];

const HERDR: &[(&str, &str)] = if cfg!(target_os = "macos") { HERDR_MAC } else { HERDR_OTHER };

/// From herdr-app's defaults/keys.macos.json.
const HERDR_MAC: &[(&str, &str)] = &[
    ("quick-open", "cmd-p"),
    ("commands", "cmd-shift-p"),
    // ⌃1…⌃9 are herdr's spaces (our machines); the pages move to ⌥⌘.
    ("chat", "alt-cmd-0"),
    ("overview", "alt-cmd-1"),
    ("jobs", "alt-cmd-2"),
    ("boxes", "alt-cmd-3"),
    ("github", "alt-cmd-4"),
    ("usage", "alt-cmd-5"),
    ("run", "alt-cmd-6"),
    ("schedules", "alt-cmd-7"),
    ("add-machine", "cmd-n"),
    ("new-session", "cmd-t"),
    ("split-right", "cmd-d"),
    ("split-down", "cmd-shift-d"),
    ("next-pane", "alt-cmd-right"),
    ("previous-pane", "alt-cmd-left"),
    ("zoom-pane", "cmd-shift-enter"),
    ("close-pane", "cmd-shift-w"),
    ("end-session", "cmd-w"),
    ("next-session", "cmd-shift-]"),
    ("previous-session", "cmd-shift-["),
    // ⌘[ and ⌘] are herdr's agents; Back and Forward move to ⌃⌘.
    ("next-agent", "cmd-]"),
    ("previous-agent", "cmd-["),
    ("back", "ctrl-cmd-["),
    ("forward", "ctrl-cmd-]"),
    ("rename", "f2"),
    ("sidebar", "cmd-b"),
    ("settings", "cmd-,"),
    (MACHINES, "ctrl"),
    (SESSIONS, "cmd"),
];

/// From herdr-app's defaults/keys.json (Linux and Windows).
const HERDR_OTHER: &[(&str, &str)] = &[
    ("quick-open", "ctrl-shift-k"),
    ("commands", "ctrl-shift-p"),
    // Alt+1…9 are herdr's spaces (our machines); the pages move to Ctrl+Alt.
    ("overview", "ctrl-alt-1"),
    ("jobs", "ctrl-alt-2"),
    ("boxes", "ctrl-alt-3"),
    ("github", "ctrl-alt-4"),
    ("usage", "ctrl-alt-5"),
    ("run", "ctrl-alt-6"),
    ("schedules", "ctrl-alt-7"),
    ("add-machine", "ctrl-shift-n"),
    ("new-session", "ctrl-shift-t"),
    ("split-right", "ctrl-shift-o"),
    ("split-down", "ctrl-shift-e"),
    ("next-pane", "alt-right"),
    ("previous-pane", "alt-left"),
    ("zoom-pane", "ctrl-shift-z"),
    ("close-pane", "ctrl-shift-x"),
    ("end-session", "ctrl-shift-w"),
    ("next-session", "ctrl-pagedown"),
    ("previous-session", "ctrl-pageup"),
    ("next-agent", "alt-down"),
    ("previous-agent", "alt-up"),
    ("rename", "f2"),
    ("sidebar", "ctrl-shift-b"),
    ("settings", "ctrl-,"),
    (MACHINES, "alt"),
    (SESSIONS, "ctrl"),
];

/// Sets every shortcut to a preset's keys.
pub fn use_preset(cx: &mut App, preset: &Preset) {
    let own: Vec<(String, String)> = all_ids().map(|id| (id, preset.keys_for(id))).filter(|(id, k)| !is_default(id, k)).map(|(id, k)| (id.to_string(), k.to_string())).collect();
    super::theme::remember(cx, |a| {
        a.shortcuts.clear();
        a.shortcuts.extend(own);
    });
}

/// The preset the keys are now, if they're exactly one.
pub fn preset_in_use(cx: &App) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| {
        all_ids().all(|id| {
            let (now, want) = (keys(cx, id), p.keys_for(id));
            if is_group(id) { modifier(&now) == modifier(want) } else { normalize(&now) == normalize(want) }
        })
    })
}

/// Whether the user changed this one.
pub fn changed(cx: &App, id: &str) -> bool {
    cx.global::<Appearance>().shortcuts.contains_key(id)
}

/// A comparable form of a key combination ("secondary-r" and "cmd-r" are the same on a Mac).
fn normalize(keys: &str) -> Option<String> {
    let parts: Option<Vec<String>> = keys.split_whitespace().map(|p| Keystroke::parse(p).ok().map(|k| k.unparse())).collect();
    parts.filter(|p| !p.is_empty()).map(|p| p.join(" "))
}

/// Every shortcut, bound as the user has them. Keys that don't parse are skipped.
pub fn bindings(cx: &App) -> Vec<KeyBinding> {
    let mut out: Vec<KeyBinding> = ALL.iter().filter_map(|s| normalize(&keys(cx, s.id)).map(|k| (s.bind)(&k))).collect();
    let machines: [fn(&str) -> KeyBinding; 9] = [
        |k| KeyBinding::new(k, GoToMachine1, None),
        |k| KeyBinding::new(k, GoToMachine2, None),
        |k| KeyBinding::new(k, GoToMachine3, None),
        |k| KeyBinding::new(k, GoToMachine4, None),
        |k| KeyBinding::new(k, GoToMachine5, None),
        |k| KeyBinding::new(k, GoToMachine6, None),
        |k| KeyBinding::new(k, GoToMachine7, None),
        |k| KeyBinding::new(k, GoToMachine8, None),
        |k| KeyBinding::new(k, GoToMachine9, None),
    ];
    for (i, bind) in machines.iter().enumerate() {
        if let Some(k) = machine_keys(cx, i + 1).and_then(|k| normalize(&k)) {
            out.push(bind(&k));
        }
    }
    let sessions: [fn(&str) -> KeyBinding; 9] = [
        |k| KeyBinding::new(k, GoToSession1, None),
        |k| KeyBinding::new(k, GoToSession2, None),
        |k| KeyBinding::new(k, GoToSession3, None),
        |k| KeyBinding::new(k, GoToSession4, None),
        |k| KeyBinding::new(k, GoToSession5, None),
        |k| KeyBinding::new(k, GoToSession6, None),
        |k| KeyBinding::new(k, GoToSession7, None),
        |k| KeyBinding::new(k, GoToSession8, None),
        |k| KeyBinding::new(k, GoToSession9, None),
    ];
    for (i, bind) in sessions.iter().enumerate() {
        if let Some(k) = group_keys(cx, SESSIONS, i + 1).and_then(|k| normalize(&k)) {
            out.push(bind(&k));
        }
    }
    out
}

/// Every shortcut that uses Ctrl (Linux and Windows hand these to terminals
/// and remote desktops, where Ctrl keys mean something).
pub fn ctrl_keys(cx: &App) -> Vec<String> {
    let mut keys: Vec<String> = ALL.iter().filter_map(|s| normalize(&keys(cx, s.id))).collect();
    keys.extend(GROUPS.into_iter().flat_map(|g| (1..=9).filter_map(move |n| group_keys(cx, g, n).and_then(|k| normalize(&k)))));
    keys.retain(|k| k.split('-').any(|p| p == "ctrl"));
    keys
}

/// The Ctrl shortcuts a terminal program gets instead of the app: not those
/// with Shift, or on PageUp and PageDown, which terminal apps keep for
/// themselves too (herdr's keys on Linux and Windows are Ctrl+Shift ones).
pub fn terminal_ctrl_keys(cx: &App) -> Vec<String> {
    let mut keys = ctrl_keys(cx);
    keys.retain(|k| {
        let parts: Vec<&str> = k.split('-').collect();
        !parts.contains(&"shift") && !matches!(parts.last(), Some(&"pageup") | Some(&"pagedown"))
    });
    keys
}

/// Binds every key in the app again, after the user changed a shortcut, and
/// refreshes the menu bar (which shows them).
pub fn apply(cx: &mut App) {
    cx.clear_key_bindings();
    super::input::bind_keys(cx);
    super::box_dialog::bind_keys(cx);
    super::terminal::bind_keys(cx);
    super::desktop::bind_keys(cx);
    super::root::bind_keys(cx);
    super::desktop::bind_taken_keys(cx);
    cx.set_menus(super::root::menus());
}

/// How a key combination reads: "⌥⌘1" on a Mac, "Ctrl+Alt+1" elsewhere.
pub fn label(keys: &str) -> String {
    keys.split_whitespace().filter_map(|p| Keystroke::parse(p).ok()).map(|k| one(&k)).collect::<Vec<_>>().join(" ")
}

fn one(k: &Keystroke) -> String {
    let m = &k.modifiers;
    let key = match k.key.as_str() {
        "escape" => "Esc".to_string(),
        "enter" => "↵".into(),
        "tab" => "⇥".into(),
        "backspace" => "⌫".into(),
        "delete" => "⌦".into(),
        "space" => "Space".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        other if other.chars().count() == 1 => other.to_uppercase(),
        other => {
            let mut c = other.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    };
    if cfg!(target_os = "macos") {
        let mut s = String::new();
        for (on, sym) in [(m.control, "⌃"), (m.alt, "⌥"), (m.shift, "⇧"), (m.platform, "⌘"), (m.function, "fn ")] {
            if on {
                s.push_str(sym);
            }
        }
        s + &key
    } else {
        let mut parts: Vec<&str> = Vec::new();
        for (on, name) in [(m.control, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift"), (m.platform, "Super"), (m.function, "Fn")] {
            if on {
                parts.push(name);
            }
        }
        parts.push(&key);
        parts.join("+")
    }
}

/// The label for a shortcut now ("" when it has none).
pub fn label_for(cx: &App, id: &str) -> String {
    label(&keys(cx, id))
}

/// A key the user pressed in Settings, as the keys to bind: None when it
/// can't be a shortcut (no ⌘, ⌃ or ⌥, so it would type instead).
pub fn from_press(k: &Keystroke) -> Option<String> {
    let m = &k.modifiers;
    let function_key = k.key.len() > 1 && k.key.starts_with('f') && k.key[1..].parse::<u8>().is_ok();
    if !(m.platform || m.control || m.alt || function_key) || k.key.is_empty() {
        return None;
    }
    Some(k.unparse())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_read_like_the_platform_writes_them() {
        if cfg!(target_os = "macos") {
            assert_eq!(label("secondary-shift-r"), "⇧⌘R");
            assert_eq!(label("alt-secondary-1"), "⌥⌘1");
            assert_eq!(label("cmd-,"), "⌘,");
            assert_eq!(label("ctrl-escape"), "⌃Esc");
        } else {
            assert_eq!(label("secondary-shift-r"), "Ctrl+Shift+R");
        }
        assert_eq!(normalize("secondary-r"), normalize(if cfg!(target_os = "macos") { "cmd-r" } else { "ctrl-r" }));
        assert_eq!(normalize(""), None);
    }

    /// Every key a preset (or the defaults) binds, with what it does.
    fn bound(p: &Preset) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for id in all_ids() {
            let k = p.keys_for(id);
            if is_group(id) {
                let m = modifier(k);
                if !m.is_empty() {
                    out.extend((1..=9).map(|n| (normalize(&format!("{m}-{n}")).unwrap(), format!("{id} {n}"))));
                }
            } else if !k.is_empty() {
                out.push((normalize(k).unwrap_or_else(|| panic!("{} doesn't parse: {k}", id)), id.to_string()));
            }
        }
        out
    }

    #[test]
    fn presets_bind_each_key_once() {
        for p in PRESETS {
            let keys = bound(p);
            for (i, (k, what)) in keys.iter().enumerate() {
                if let Some((_, other)) = keys[i + 1..].iter().find(|(x, _)| x == k) {
                    panic!("{}: {k} is both {what} and {other}", p.name);
                }
            }
        }
        // Both platforms' tables: only shortcuts there are, keys that parse,
        // each set once (on its own, apart from the defaults).
        for table in [HERDR_MAC, HERDR_OTHER] {
            for (i, (id, k)) in table.iter().enumerate() {
                assert!(all_ids().any(|x| x == *id), "no shortcut {id}");
                assert!(table[i + 1..].iter().all(|(x, _)| x != id), "{id} twice");
                if !is_group(id) {
                    assert!(normalize(k).is_some(), "{id}: {k} doesn't parse");
                }
            }
        }
    }

    #[test]
    fn herdrs_own_keys() {
        let herdr = PRESETS.iter().find(|p| p.name == "herdr").unwrap();
        let k = |id: &str| normalize(herdr.keys_for(id));
        if cfg!(target_os = "macos") {
            assert_eq!(k("new-session"), normalize("cmd-t"));
            assert_eq!(k("next-agent"), normalize("cmd-]"));
            assert_eq!(modifier(herdr.keys_for(SESSIONS)), "cmd");
        } else {
            assert_eq!(k("new-session"), normalize("ctrl-shift-t"));
            assert_eq!(modifier(herdr.keys_for(SESSIONS)), "ctrl");
        }
        assert_eq!(k("rename"), normalize("f2"));
    }

    #[test]
    fn a_shortcut_needs_a_modifier() {
        assert_eq!(from_press(&Keystroke::parse("a").unwrap()), None);
        assert_eq!(from_press(&Keystroke::parse("shift-a").unwrap()), None);
        assert!(from_press(&Keystroke::parse("cmd-k").unwrap()).is_some());
        assert!(from_press(&Keystroke::parse("f5").unwrap()).is_some());
    }
}
