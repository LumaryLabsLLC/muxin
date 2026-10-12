//! The app's keyboard shortcuts: what each one does, its keys by default, and
//! the user's own keys from Settings (saved in config.toml). ⌘1…⌘9 go to the
//! machines in the sidebar; holding ⌘ shows every shortcut where it applies.

use gpui::{App, KeyBinding, Keystroke};

use super::root::{
    AddMachine, CheckNow, CommandPalette, FocusFilter, GoBack, GoForward, GoToMachine1, GoToMachine2, GoToMachine3, GoToMachine4, GoToMachine5, GoToMachine6,
    GoToMachine7, GoToMachine8, GoToMachine9, NewSession, NextPane, OpenSettings, PreviousPane, ShowGithub, ShowJobs, ShowOverview, ShowWorkspaces,
    SplitDown, SplitRight, SyncGitHub, QuickOpen, ToggleActivity, ToggleSidebar,
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
    shortcut!("overview", "Overview", page("ctrl-1", "alt-1"), ShowOverview),
    shortcut!("jobs", "Jobs", page("ctrl-2", "alt-2"), ShowJobs),
    shortcut!("workspaces", "Workspaces", page("ctrl-3", "alt-3"), ShowWorkspaces),
    shortcut!("github", "GitHub", page("ctrl-4", "alt-4"), ShowGithub),
    shortcut!("activity", "Activity", "secondary-j", ToggleActivity),
    shortcut!("settings", "Settings", "secondary-,", OpenSettings),
    shortcut!("check", "Check machines", "secondary-r", CheckNow),
    shortcut!("sync", "Sync GitHub", "secondary-shift-r", SyncGitHub),
    shortcut!("add-machine", "Add machine", "secondary-n", AddMachine),
    shortcut!("new-session", "New session", "secondary-t", NewSession),
    shortcut!("split-right", "Split the terminal right", "secondary-d", SplitRight),
    shortcut!("split-down", "Split the terminal down", "secondary-shift-d", SplitDown),
    shortcut!("next-pane", "Next pane", "secondary-alt-right", NextPane),
    shortcut!("previous-pane", "Previous pane", "secondary-alt-left", PreviousPane),
    shortcut!("filter", "Search machines and sessions", "secondary-f", FocusFilter),
    shortcut!("sidebar", "Hide or show the sidebar", "secondary-b", ToggleSidebar),
    shortcut!("back", "Back", "secondary-[", GoBack),
    shortcut!("forward", "Forward", "secondary-]", GoForward),
];

/// The machines' shortcuts: this modifier with 1…9 (in Appearance under this id).
pub const MACHINES: &str = "machines";
const MACHINES_DEFAULT: &str = "secondary";

/// Choices for the machines' modifier: (keys, how Settings shows them).
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
        None if id == MACHINES => MACHINES_DEFAULT.into(),
        None => ALL.iter().find(|s| s.id == id).map(|s| s.default.to_string()).unwrap_or_default(),
    }
}

/// The modifier for ⌘1…⌘9, normalized ("secondary" is ⌘ or Ctrl).
pub fn machine_modifier(cx: &App) -> String {
    modifier(&keys(cx, MACHINES))
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
    let m = machine_modifier(cx);
    (!m.is_empty() && (1..=9).contains(&n)).then(|| format!("{m}-{n}"))
}

/// What already uses `keys`, other than `except`: a shortcut's id and label,
/// or (MACHINES, "Machine 3") for a machine's.
pub fn used_by(cx: &App, keys_: &str, except: &str) -> Option<(&'static str, String)> {
    let k = normalize(keys_)?;
    if except != MACHINES {
        if let Some(n) = (1..=9).find(|n| machine_keys(cx, *n).and_then(|m| normalize(&m)).as_deref() == Some(k.as_str())) {
            return Some((MACHINES, format!("Machine {n}")));
        }
    }
    ALL.iter().filter(|s| s.id != except).find(|s| normalize(&keys(cx, s.id)).as_deref() == Some(k.as_str())).map(|s| (s.id, s.label.to_string()))
}

/// Sets a shortcut's keys ("" for none), keeping only what differs from the default.
pub fn set(cx: &mut App, id: &str, keys_: &str) {
    let same = if id == MACHINES {
        modifier(keys_) == modifier(MACHINES_DEFAULT)
    } else {
        normalize(keys_) == ALL.iter().find(|s| s.id == id).and_then(|s| normalize(s.default))
    };
    super::theme::remember(cx, |a| {
        if same {
            a.shortcuts.remove(id);
        } else {
            a.shortcuts.insert(id.to_string(), keys_.to_string());
        }
    });
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
    out
}

/// Every shortcut that uses Ctrl (Linux and Windows hand these to terminals
/// and remote desktops, where Ctrl keys mean something).
pub fn ctrl_keys(cx: &App) -> Vec<String> {
    let mut keys: Vec<String> = ALL.iter().filter_map(|s| normalize(&keys(cx, s.id))).collect();
    keys.extend((1..=9).filter_map(|n| machine_keys(cx, n).and_then(|k| normalize(&k))));
    keys.retain(|k| k.split('-').any(|p| p == "ctrl"));
    keys
}

/// Binds every key in the app again, after the user changed a shortcut, and
/// refreshes the menu bar (which shows them).
pub fn apply(cx: &mut App) {
    cx.clear_key_bindings();
    super::input::bind_keys(cx);
    super::terminal::bind_keys(cx);
    super::desktop::bind_keys(cx);
    super::root::bind_keys(cx);
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

    #[test]
    fn a_shortcut_needs_a_modifier() {
        assert_eq!(from_press(&Keystroke::parse("a").unwrap()), None);
        assert_eq!(from_press(&Keystroke::parse("shift-a").unwrap()), None);
        assert!(from_press(&Keystroke::parse("cmd-k").unwrap()).is_some());
        assert!(from_press(&Keystroke::parse("f5").unwrap()).is_some());
    }
}
