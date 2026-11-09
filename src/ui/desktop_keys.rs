//! Which keys a machine is told for the keys pressed here, when the two don't
//! have the same keyboard: a Mac's ⌘ on Windows or Linux, Alt+Tab on a Mac.
//! And the key that gives the keyboard back, and the combinations the Keys
//! menu sends. No window in any of it (ui/desktop.rs does the sending).

use gpui::Modifiers;

/// A keyboard's kind: a Mac's (⌘, Option), or a PC's (Windows and Linux:
/// Ctrl, Alt, the Windows key).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Pc,
}

impl Os {
    /// This computer's.
    pub fn here() -> Os {
        if cfg!(target_os = "macos") { Os::Mac } else { Os::Pc }
    }
}

// X keysyms, as the agents take them.
pub const SHIFT: u32 = 0xffe1;
pub const CTRL: u32 = 0xffe3;
pub const ALT: u32 = 0xffe9;
/// ⌘ on a Mac, the Windows key, Super.
pub const SUPER: u32 = 0xffeb;

/// A key (keysym) to press or let go on the machine.
pub type Event = (u32, bool);

/// The modifier keys the machine has been told are down.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Told {
    shift: bool,
    ctrl: bool,
    alt: bool,
    sup: bool,
}

/// A modifier held here that stands for another on the machine, until it's
/// let go.
#[derive(Clone, Copy, Debug, PartialEq)]
enum StandIn {
    /// ⌘ is Alt there: ⌘Tab is Alt+Tab, ⌘Q is Alt+F4.
    CommandIsAlt,
    /// ⌘ is nothing there: ⌘Space pressed the Windows key by itself.
    CommandIsNothing,
    /// Alt is ⌘ there: Alt+Tab is ⌘Tab, Alt+F4 is ⌘Q.
    AltIsCommand,
}

/// What a key going down here is on the machine.
#[derive(Debug, PartialEq)]
pub struct Pressed {
    /// Modifiers to change there first.
    pub before: Vec<Event>,
    /// The key to press (named as `fleet::desktop::keysym` takes it), or none
    /// when the key did its work in `before`.
    pub key: Option<String>,
}

/// The modifier keys held here, and what the machine was told about them.
///
/// Keys keep their place: Ctrl is Control, Alt is Option, the Windows key is
/// ⌘. One exception, as before: from a Mac, ⌘ is Ctrl on Windows and Linux
/// (⌘C copies there). And a few shortcuts go as what they mean rather than
/// as their keys: the app switcher (⌘Tab, Alt+Tab), quitting (⌘Q, Alt+F4),
/// and ⌘Space, which is the Windows key from a Mac.
#[derive(Debug)]
pub struct Mods {
    here: Os,
    there: Os,
    /// Shift, Control, ⌘ and Alt as they are here.
    held: Modifiers,
    /// Whether Alt should be down there for the Alt held here (a Mac's
    /// Option isn't always: see `OptionKey` in ui/desktop.rs).
    alt: bool,
    stand_in: Option<StandIn>,
    /// This computer's own shortcuts are going to the machine (see
    /// `crate::capture`): the Windows key is told at once, as its tap opens
    /// Start there. Otherwise it's this computer's, and a tap of it opens
    /// Start here: the machine only hears of it once a key or a click comes
    /// with it (`super_used`). A Mac's ⌘ is always told at once.
    taking: bool,
    super_used: bool,
    told: Told,
    /// Keys down here that went as another key, or as none (None).
    renamed: Vec<(String, Option<String>)>,
}

impl Mods {
    pub fn new(here: Os, there: Os) -> Mods {
        Mods { here, there, held: Modifiers::default(), alt: false, stand_in: None, taking: false, super_used: false, told: Told::default(), renamed: Vec::new() }
    }

    /// The machine's keyboard turned out to be another kind (a shared desktop
    /// says so once it's joined). Nothing is held yet then.
    pub fn set_there(&mut self, there: Os) {
        self.there = there;
    }

    /// Whether this computer's own shortcuts are going to the machine.
    pub fn set_taking(&mut self, taking: bool) {
        self.taking = taking;
    }

    pub fn held(&self) -> Modifiers {
        self.held
    }

    fn want(&self) -> Told {
        let mac_to_pc = self.here == Os::Mac && self.there == Os::Pc;
        let command = self.held.platform;
        let told_now = self.here == Os::Mac || self.taking || self.super_used;
        let mut want = Told { shift: self.held.shift, ctrl: self.held.control || (mac_to_pc && command), alt: self.alt, sup: command && !mac_to_pc && told_now };
        match self.stand_in {
            Some(StandIn::CommandIsAlt) => {
                want.ctrl = self.held.control;
                want.alt = true;
            }
            Some(StandIn::CommandIsNothing) => want.ctrl = self.held.control,
            Some(StandIn::AltIsCommand) => {
                want.alt = false;
                want.sup = true;
            }
            None => {}
        }
        want
    }

    /// Tells the machine what changed: keys let go first, then keys pressed.
    fn sync(&mut self) -> Vec<Event> {
        let want = self.want();
        let keys = [(SHIFT, self.told.shift, want.shift), (CTRL, self.told.ctrl, want.ctrl), (ALT, self.told.alt, want.alt), (SUPER, self.told.sup, want.sup)];
        let mut events: Vec<Event> = keys.iter().filter(|(_, told, want)| *told && !*want).map(|(sym, ..)| (*sym, false)).collect();
        events.extend(keys.iter().filter(|(_, told, want)| !*told && *want).map(|(sym, ..)| (*sym, true)));
        self.told = want;
        events
    }

    /// Whether Alt should be down there, for the Alt held here.
    pub fn alt(&mut self, down: bool) -> Vec<Event> {
        self.alt = down;
        self.sync()
    }

    /// The modifier keys held here now.
    pub fn set(&mut self, held: Modifiers) -> Vec<Event> {
        self.held = held;
        // A stand-in lasts as long as its key is held.
        let over = match self.stand_in {
            Some(StandIn::CommandIsAlt | StandIn::CommandIsNothing) => !held.platform,
            Some(StandIn::AltIsCommand) => !held.alt,
            None => false,
        };
        if over {
            self.stand_in = None;
        }
        if !held.platform {
            self.super_used = false;
        }
        self.sync()
    }

    /// A key or a mouse button is going down with what's held: the machine
    /// hears of a Windows key it wasn't told about yet.
    pub fn needed(&mut self) -> Vec<Event> {
        if self.held.platform && !self.super_used {
            self.super_used = true;
            return self.sync();
        }
        Vec::new()
    }

    /// A key going down here, after `set` with what's held with it.
    pub fn key_down(&mut self, key: &str) -> Pressed {
        // Held down, it repeats: the same again, without pressing anything twice.
        if let Some((_, as_key)) = self.renamed.iter().find(|(k, _)| k == key) {
            return Pressed { before: Vec::new(), key: as_key.clone() };
        }
        let mut before = self.needed();
        let m = self.held;
        let only_command = m.platform && !m.control && !m.alt;
        let only_alt = m.alt && !m.control && !m.platform;
        let (stand_in, as_key, tap): (StandIn, Option<&str>, Option<u32>) = match (self.here, self.there, key) {
            (Os::Mac, Os::Pc, "tab") if only_command => (StandIn::CommandIsAlt, Some("tab"), None),
            (Os::Mac, Os::Pc, "q") if only_command && !m.shift => (StandIn::CommandIsAlt, Some("f4"), None),
            (Os::Mac, Os::Pc, "space") if only_command && !m.shift => (StandIn::CommandIsNothing, None, Some(SUPER)),
            (Os::Pc, Os::Mac, "tab") if only_alt => (StandIn::AltIsCommand, Some("tab"), None),
            (Os::Pc, Os::Mac, "f4") if only_alt && !m.shift => (StandIn::AltIsCommand, Some("q"), None),
            _ => return Pressed { before, key: Some(key.to_string()) },
        };
        self.stand_in = Some(stand_in);
        before.extend(self.sync());
        if let Some(sym) = tap {
            before.extend([(sym, true), (sym, false)]);
        }
        let as_key = as_key.map(str::to_string);
        if as_key.as_deref() != Some(key) {
            self.renamed.push((key.to_string(), as_key.clone()));
        }
        Pressed { before, key: as_key }
    }

    /// A key let go here: the key to let go there, if one went down for it.
    pub fn key_up(&mut self, key: &str) -> Option<String> {
        match self.renamed.iter().position(|(k, _)| k == key) {
            Some(i) => self.renamed.remove(i).1,
            None => Some(key.to_string()),
        }
    }

    /// The keyboard is going elsewhere: nothing stays held there.
    pub fn let_go(&mut self) -> Vec<Event> {
        self.held = Modifiers::default();
        self.alt = false;
        self.stand_in = None;
        self.super_used = false;
        self.renamed.clear();
        self.sync()
    }
}

/// Ctrl+Alt (⌃⌥ on a Mac) pressed and let go with nothing else: the key that
/// gives the keyboard back to this computer, and takes it again.
#[derive(Debug, Default)]
pub struct ReleaseKey {
    /// A modifier is down.
    any: bool,
    /// Ctrl and Alt were both down since none was.
    both: bool,
    /// Something else was pressed since none was: it's a shortcut, not this.
    other: bool,
}

impl ReleaseKey {
    /// The modifier keys held now. True when that was Ctrl+Alt let go by themselves.
    pub fn modifiers(&mut self, m: &Modifiers) -> bool {
        self.any = m.control || m.alt || m.shift || m.platform || m.function;
        if !self.any {
            let fire = self.both && !self.other;
            (self.both, self.other) = (false, false);
            return fire;
        }
        if m.shift || m.platform || m.function {
            self.other = true;
        }
        if m.control && m.alt {
            self.both = true;
        }
        false
    }

    /// A key or a mouse button went down.
    pub fn other(&mut self) {
        if self.any {
            self.other = true;
        }
    }
}

/// How the release key reads here.
pub fn release_key_label() -> &'static str {
    if cfg!(target_os = "macos") { "⌃⌥" } else { "Ctrl+Alt" }
}

/// Something the Keys menu sends: a combination no keyboard here can send,
/// or that this computer keeps for itself. `keys` is as `muxin desktop input`
/// takes it ("ctrl+right").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Combo {
    pub label: &'static str,
    pub keys: &'static str,
}

/// The Keys menu for a machine, by its platform ("darwin-arm64",
/// "windows-x64", "linux-x64"; empty for a shared desktop, which only says
/// whether it's a Mac).
pub fn menu(platform: &str, mac: bool) -> Vec<Combo> {
    let combo = |label, keys| Combo { label, keys };
    if mac || platform.starts_with("darwin") {
        vec![
            combo("Previous Space", "ctrl+left"),
            combo("Next Space", "ctrl+right"),
            combo("Mission Control", "ctrl+up"),
            combo("App windows", "ctrl+down"),
            combo("Switch app", "cmd+tab"),
            combo("Spotlight", "cmd+space"),
            combo("Force Quit", "cmd+alt+escape"),
        ]
    } else if platform.starts_with("windows") {
        // Not Ctrl+Alt+Del or Win+L: Windows only takes those from its own keyboard.
        vec![
            combo("Start", "super"),
            combo("Switch window", "alt+tab"),
            combo("Task view", "super+tab"),
            combo("Show desktop", "super+d"),
            combo("Task Manager", "ctrl+shift+escape"),
            combo("Close window", "alt+f4"),
        ]
    } else {
        vec![
            combo("Super", "super"),
            combo("Switch window", "alt+tab"),
            combo("Close window", "alt+f4"),
            combo("Ctrl+Alt+Del", "ctrl+alt+delete"),
        ]
    }
}

/// How a combination reads on the machine's keyboard: "⌃→" on a Mac,
/// "Ctrl+Alt+Del" elsewhere (the Windows key is Super on Linux).
pub fn combo_label(keys: &str, platform: &str, mac: bool) -> String {
    let mac = mac || platform.starts_with("darwin");
    let name = |key: &str| -> String {
        let named = match (key, mac) {
            ("ctrl", true) => "⌃",
            ("alt", true) => "⌥",
            ("shift", true) => "⇧",
            ("cmd" | "super", true) => "⌘",
            ("left", true) => "←",
            ("right", true) => "→",
            ("up", true) => "↑",
            ("down", true) => "↓",
            ("tab", true) => "⇥",
            ("ctrl", false) => "Ctrl",
            ("alt", false) => "Alt",
            ("shift", false) => "Shift",
            ("cmd" | "super", false) if platform.starts_with("windows") => "Win",
            ("cmd" | "super", false) => "Super",
            ("tab", false) => "Tab",
            ("space", _) => "Space",
            ("escape", _) => "Esc",
            ("delete", _) => "Del",
            _ => "",
        };
        if named.is_empty() { key.to_uppercase() } else { named.to_string() }
    };
    let keys: Vec<String> = keys.split('+').map(|k| name(k.trim())).collect();
    keys.join(if mac { "" } else { "+" })
}

/// The shortcuts this computer keeps for itself, for telling the user what
/// goes where.
pub fn examples() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘Tab, Mission Control and the like"
    } else if cfg!(windows) {
        "Alt+Tab, the Windows key and the like"
    } else {
        "Alt+Tab, Super and the like"
    }
}

/// The shortcut that stands for them all on the toolbar: this computer's app
/// switcher.
pub fn example_key() -> &'static str {
    if cfg!(target_os = "macos") { "⌘Tab" } else { "Alt+Tab" }
}

/// The toolbar's words for where this computer's own shortcuts go, by the
/// machine's name (empty when it has none here: a shared desktop). An offer
/// while they're this computer's ("Send ⌘Tab to Beacon"); where they go and
/// the way back while they go to the machine.
pub fn switch_label(there: bool, machine: &str) -> String {
    let machine = match machine.trim() {
        "" => "the machine".to_string(),
        name if name.chars().count() > 22 => format!("{}…", name.chars().take(21).collect::<String>().trim_end()),
        name => name.to_string(),
    };
    let key = example_key();
    if there { format!("{key} goes to {machine} · {} to stop", release_key_label()) } else { format!("Send {key} to {machine}") }
}

/// A combination's keysyms, in the order to press them.
pub fn combo_keys(keys: &str) -> Option<Vec<u32>> {
    keys.split('+').map(|k| crate::fleet::desktop::keysym(k.trim())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(control: bool, alt: bool, shift: bool, platform: bool) -> Modifiers {
        Modifiers { control, alt, shift, platform, function: false }
    }

    const NONE: Modifiers = Modifiers { control: false, alt: false, shift: false, platform: false, function: false };

    fn key(name: &str) -> Pressed {
        Pressed { before: Vec::new(), key: Some(name.to_string()) }
    }

    #[test]
    fn keys_keep_their_place_between_two_macs_and_between_two_pcs() {
        let mut mac = Mods::new(Os::Mac, Os::Mac);
        assert_eq!(mac.set(held(false, false, false, true)), [(SUPER, true)]);
        assert_eq!(mac.key_down("tab"), key("tab"));
        assert_eq!(mac.key_down("q"), key("q"));
        assert_eq!(mac.set(held(true, false, false, false)), [(SUPER, false), (CTRL, true)]);
        assert_eq!(mac.set(NONE), [(CTRL, false)]);
        // The Windows key is the Windows key (or Super), not Ctrl.
        let mut pc = Mods::new(Os::Pc, Os::Pc);
        pc.set_taking(true);
        assert_eq!(pc.set(held(false, false, false, true)), [(SUPER, true)]);
        assert_eq!(pc.key_down("e"), key("e"));
        assert_eq!(pc.set(NONE), [(SUPER, false)]);
        assert_eq!(pc.alt(true), [(ALT, true)]);
        assert_eq!(pc.set(held(false, true, false, false)), []);
        assert_eq!(pc.key_down("tab"), key("tab"));
        assert_eq!(pc.key_down("f4"), key("f4"));
    }

    #[test]
    fn the_windows_key_is_this_computers_until_its_shortcuts_go_to_the_machine() {
        // Not going there: a tap opens Start here, and the machine hears nothing.
        let mut m = Mods::new(Os::Pc, Os::Pc);
        assert_eq!(m.set(held(false, false, false, true)), []);
        assert_eq!(m.set(NONE), []);
        // With a key (one the window manager here left alone) or a click, it's told first.
        assert_eq!(m.set(held(false, false, false, true)), []);
        assert_eq!(m.key_down("e"), Pressed { before: vec![(SUPER, true)], key: Some("e".into()) });
        assert_eq!(m.key_down("r"), key("r"));
        assert_eq!(m.set(NONE), [(SUPER, false)]);
        assert_eq!(m.set(held(false, false, false, true)), []);
        assert_eq!(m.needed(), [(SUPER, true)]);
        assert_eq!(m.needed(), []);
        assert_eq!(m.let_go(), [(SUPER, false)]);
        // Going there: told at once, so a tap opens Start on the machine.
        m.set_taking(true);
        assert_eq!(m.set(held(false, false, false, true)), [(SUPER, true)]);
        assert_eq!(m.set(NONE), [(SUPER, false)]);
        // A Mac's ⌘ is told at once either way: macOS does nothing with it alone.
        let mut mac = Mods::new(Os::Mac, Os::Mac);
        assert_eq!(mac.set(held(false, false, false, true)), [(SUPER, true)]);
        assert_eq!(mac.needed(), []);
    }

    #[test]
    fn a_macs_command_is_ctrl_on_a_pc() {
        let mut m = Mods::new(Os::Mac, Os::Pc);
        assert_eq!(m.set(held(false, false, false, true)), [(CTRL, true)]);
        assert_eq!(m.key_down("c"), key("c"));
        assert_eq!(m.key_up("c"), Some("c".into()));
        // ⌃ too, and both at once are still one Ctrl.
        assert_eq!(m.set(held(true, false, false, true)), []);
        assert_eq!(m.set(held(true, false, false, false)), []);
        assert_eq!(m.set(NONE), [(CTRL, false)]);
    }

    #[test]
    fn command_tab_from_a_mac_is_alt_tab_on_a_pc_while_command_is_held() {
        let mut m = Mods::new(Os::Mac, Os::Pc);
        assert_eq!(m.set(held(false, false, false, true)), [(CTRL, true)]);
        assert_eq!(m.key_down("tab"), Pressed { before: vec![(CTRL, false), (ALT, true)], key: Some("tab".into()) });
        assert_eq!(m.key_up("tab"), Some("tab".into()));
        // Tab again, with Shift to go back: Alt stays down, so the switcher stays open.
        assert_eq!(m.set(held(false, false, true, true)), [(SHIFT, true)]);
        assert_eq!(m.key_down("tab"), key("tab"));
        assert_eq!(m.set(held(false, false, false, true)), [(SHIFT, false)]);
        // ⌘ let go: Alt let go, which picks the window.
        assert_eq!(m.set(NONE), [(ALT, false)]);
        // And ⌘ is Ctrl again.
        assert_eq!(m.set(held(false, false, false, true)), [(CTRL, true)]);
    }

    #[test]
    fn command_q_from_a_mac_is_alt_f4_on_a_pc() {
        let mut m = Mods::new(Os::Mac, Os::Pc);
        let _ = m.set(held(false, false, false, true));
        assert_eq!(m.key_down("q"), Pressed { before: vec![(CTRL, false), (ALT, true)], key: Some("f4".into()) });
        // Held, it repeats as the same key, and it's that key that's let go.
        assert_eq!(m.key_down("q"), key("f4"));
        assert_eq!(m.key_up("q"), Some("f4".into()));
        assert_eq!(m.set(NONE), [(ALT, false)]);
        // ⌘⇧Q isn't that.
        let _ = m.set(held(false, false, true, true));
        assert_eq!(m.key_down("q"), key("q"));
    }

    #[test]
    fn command_space_from_a_mac_is_the_windows_key() {
        let mut m = Mods::new(Os::Mac, Os::Pc);
        let _ = m.set(held(false, false, false, true));
        assert_eq!(m.key_down("space"), Pressed { before: vec![(CTRL, false), (SUPER, true), (SUPER, false)], key: None });
        // Held, Space repeats: the key isn't pressed again, and has nothing to let go.
        assert_eq!(m.key_down("space"), Pressed { before: Vec::new(), key: None });
        assert_eq!(m.key_up("space"), None);
        // Nothing was left held for ⌘.
        assert_eq!(m.set(NONE), []);
        assert_eq!(m.key_down("space"), key("space"));
    }

    #[test]
    fn alt_tab_from_a_pc_is_command_tab_on_a_mac_while_alt_is_held() {
        let mut m = Mods::new(Os::Pc, Os::Mac);
        // As the view tells it: Alt for the machine first, then what's held.
        assert_eq!(m.alt(true), [(ALT, true)]);
        assert_eq!(m.set(held(false, true, false, false)), []);
        assert_eq!(m.key_down("tab"), Pressed { before: vec![(ALT, false), (SUPER, true)], key: Some("tab".into()) });
        assert_eq!(m.key_up("tab"), Some("tab".into()));
        assert_eq!(m.key_down("tab"), key("tab"));
        // Alt let go: ⌘ let go, and Option never comes back down.
        assert_eq!(m.alt(false), []);
        assert_eq!(m.set(NONE), [(SUPER, false)]);
    }

    #[test]
    fn alt_f4_from_a_pc_is_command_q_on_a_mac() {
        let mut m = Mods::new(Os::Pc, Os::Mac);
        let _ = m.alt(true);
        let _ = m.set(held(false, true, false, false));
        assert_eq!(m.key_down("f4"), Pressed { before: vec![(ALT, false), (SUPER, true)], key: Some("q".into()) });
        assert_eq!(m.key_up("f4"), Some("q".into()));
        // The Windows key is ⌘, and Ctrl stays Control (⌃C in a terminal).
        let mut m = Mods::new(Os::Pc, Os::Mac);
        m.set_taking(true);
        assert_eq!(m.set(held(false, false, false, true)), [(SUPER, true)]);
        assert_eq!(m.set(held(true, false, false, false)), [(SUPER, false), (CTRL, true)]);
    }

    #[test]
    fn other_shortcuts_are_left_alone() {
        // Ctrl+Alt+Tab, ⌃⌘Q (lock a Mac's screen) and ⌥⌘Q aren't the app switcher or quitting.
        let mut m = Mods::new(Os::Pc, Os::Mac);
        let _ = m.alt(true);
        let _ = m.set(held(true, true, false, false));
        assert_eq!(m.key_down("tab"), key("tab"));
        let mut m = Mods::new(Os::Mac, Os::Pc);
        let _ = m.set(held(true, false, false, true));
        assert_eq!(m.key_down("q"), key("q"));
        assert_eq!(m.key_down("tab"), key("tab"));
        // Tab and Q by themselves.
        let mut m = Mods::new(Os::Mac, Os::Pc);
        assert_eq!(m.key_down("tab"), key("tab"));
        assert_eq!(m.key_down("q"), key("q"));
    }

    #[test]
    fn letting_go_leaves_nothing_held_there() {
        let mut m = Mods::new(Os::Mac, Os::Pc);
        let _ = m.set(held(false, false, true, true));
        let _ = m.key_down("tab");
        assert_eq!(m.let_go(), [(SHIFT, false), (ALT, false)]);
        assert_eq!(m.let_go(), []);
        assert_eq!(m.key_up("tab"), Some("tab".into()));
        // The keys still held here are told again by the next `set`.
        assert_eq!(m.set(held(false, false, false, true)), [(CTRL, true)]);
    }

    #[test]
    fn ctrl_alt_by_themselves_give_the_keyboard_back() {
        let mut r = ReleaseKey::default();
        assert!(!r.modifiers(&held(true, false, false, false)));
        assert!(!r.modifiers(&held(true, true, false, false)));
        assert!(!r.modifiers(&held(false, true, false, false)));
        assert!(r.modifiers(&NONE));
        // Once: nothing held again doesn't fire again.
        assert!(!r.modifiers(&NONE));
        // Alt first, then Ctrl, is the same.
        assert!(!r.modifiers(&held(false, true, false, false)));
        assert!(!r.modifiers(&held(true, true, false, false)));
        assert!(r.modifiers(&NONE));
    }

    #[test]
    fn ctrl_alt_with_anything_else_is_a_shortcut() {
        // Ctrl+Alt+Del, Ctrl+Alt+→, a click with both held.
        let mut r = ReleaseKey::default();
        let _ = r.modifiers(&held(true, true, false, false));
        r.other();
        assert!(!r.modifiers(&NONE));
        // Ctrl+Alt+Shift, ⌃⌥⌘.
        for with in [held(true, true, true, false), held(true, true, false, true)] {
            let _ = r.modifiers(&held(true, true, false, false));
            let _ = r.modifiers(&with);
            let _ = r.modifiers(&held(true, true, false, false));
            assert!(!r.modifiers(&NONE));
        }
        // Ctrl by itself, Alt by itself.
        let _ = r.modifiers(&held(true, false, false, false));
        assert!(!r.modifiers(&NONE));
        // Typing before doesn't spoil the next one.
        r.other();
        let _ = r.modifiers(&held(true, true, false, false));
        assert!(r.modifiers(&NONE));
    }

    #[test]
    fn every_menu_entry_is_keys_the_agents_know() {
        for (platform, mac) in [("darwin-arm64", true), ("windows-x64", false), ("linux-x64", false), ("", false), ("", true)] {
            let menu = menu(platform, mac);
            assert!(!menu.is_empty());
            for combo in menu {
                assert!(combo_keys(combo.keys).is_some_and(|k| !k.is_empty()), "{platform}: {}", combo.keys);
            }
        }
        assert_eq!(combo_label("ctrl+right", "darwin-arm64", true), "⌃→");
        assert_eq!(combo_label("cmd+alt+escape", "", true), "⌘⌥Esc");
        assert_eq!(combo_label("cmd+tab", "darwin-arm64", false), "⌘⇥");
        assert_eq!(combo_label("ctrl+alt+delete", "linux-x64", false), "Ctrl+Alt+Del");
        assert_eq!(combo_label("super+d", "windows-x64", false), "Win+D");
        assert_eq!(combo_label("super", "linux-x64", false), "Super");
        assert_eq!(combo_label("alt+f4", "windows-x64", false), "Alt+F4");
        assert_eq!(combo_keys("ctrl+right"), Some(vec![CTRL, 0xff53]));
        assert_eq!(combo_keys("cmd+alt+escape"), Some(vec![SUPER, ALT, 0xff1b]));
        assert_eq!(combo_keys("ctrl+nope"), None);
        // A Mac's Spaces, and nothing Windows refuses from anything but its own keyboard.
        assert_eq!(menu("darwin-arm64", true)[0], Combo { label: "Previous Space", keys: "ctrl+left" });
        assert!(menu("windows-x64", false).iter().all(|c| c.keys != "ctrl+alt+delete" && c.keys != "super+l"));
        assert!(menu("linux-x64", false).iter().any(|c| c.keys == "ctrl+alt+delete"));
    }

    #[test]
    fn the_toolbar_names_the_key_and_the_machine() {
        let key = example_key();
        // An offer while the shortcuts are this computer's; where they go, and the way back, after.
        assert_eq!(switch_label(false, "Beacon"), format!("Send {key} to Beacon"));
        assert_eq!(switch_label(true, "Beacon"), format!("{key} goes to Beacon · {} to stop", release_key_label()));
        // A shared desktop has no name here.
        assert_eq!(switch_label(false, ""), format!("Send {key} to the machine"));
        // A long name doesn't push the rest of the toolbar away.
        let long = switch_label(false, "Adib's MacBook Pro in the living room");
        assert_eq!(long, format!("Send {key} to Adib's MacBook Pro in…"));
    }
}
