//! The active theme, resolved from the user's appearance preference (system,
//! light or dark, plus a chosen light and dark theme) and kept as a GPUI global.

use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use gpui::{App, Global, Hsla, Window, WindowAppearance};
use serde::{Deserialize, Serialize};

use super::themes::{self, DEFAULT_DARK, DEFAULT_LIGHT};

pub const UI_FONT: &str = "Geist";
pub const MONO_FONT: &str = "Geist Mono";

/// The fonts of the active theme. Most themes use the bundled Geist; the macOS
/// themes use the system font. Read at render time by `ui_font()` / `mono_font()`.
static FONTS: RwLock<(&str, &str)> = RwLock::new((UI_FONT, MONO_FONT));
/// Picked once: SF Mono when installed, else Menlo, else Geist Mono.
static SYSTEM_MONO: OnceLock<&'static str> = OnceLock::new();

pub fn ui_font() -> &'static str {
    FONTS.read().map(|f| f.0).unwrap_or(UI_FONT)
}

pub fn mono_font() -> &'static str {
    FONTS.read().map(|f| f.1).unwrap_or(MONO_FONT)
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub id: &'static str,
    /// Use the platform's own UI and monospace fonts (the macOS themes).
    pub system_fonts: bool,
    /// The sidebar, which runs the full height of the window.
    pub sidebar: Hsla,
    /// The main content background.
    pub canvas: Hsla,
    /// Cards.
    pub surface: Hsla,
    /// Dialogs, drawers, tooltips.
    pub elevated: Hsla,
    pub border: Hsla,
    pub border_variant: Hsla,
    pub border_focused: Hsla,
    pub element: Hsla,
    pub element_hover: Hsla,
    pub element_selected: Hsla,
    pub ghost_hover: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_placeholder: Hsla,
    pub text_accent: Hsla,
    pub accent: Hsla,
    pub accent_text: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub error: Hsla,
    pub info: Hsla,
    pub selection: Hsla,
    pub scrim: Hsla,
    pub track: Hsla,
}

impl Global for Theme {}

impl Theme {
    /// Color for a runner state: working, idle, offline, unknown.
    pub fn state(&self, state: &str) -> Hsla {
        match state {
            "working" => self.accent,
            "idle" => self.success,
            _ => self.text_placeholder,
        }
    }

    /// Color for a diagnostic / attention level.
    pub fn level(&self, level: &str) -> Hsla {
        match level {
            "error" => self.error,
            "warning" => self.warning,
            "info" => self.info,
            _ => self.text_placeholder,
        }
    }

    /// A machine's status dot: green online; gray when it's just offline (shut
    /// down, asleep) or unchecked; amber or red when something needs fixing.
    pub fn machine_status(&self, m: &crate::fleet::model::Machine) -> Hsla {
        match (m.status.as_str(), m.unreachable.as_ref().map(|u| u.kind.as_str())) {
            ("online", _) => self.success,
            ("offline", Some("offline" | "powered-off")) => self.text_placeholder,
            ("offline", Some("refused" | "tailscale-off")) => self.warning,
            ("offline", _) => self.error,
            _ => self.text_placeholder,
        }
    }
}

pub trait ActiveTheme {
    fn theme(&self) -> Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> Theme {
        *self.global::<Theme>()
    }
}

// ---------------------------------------------------------------------------
// Appearance preference ([ui] in config.toml)

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub mode: Mode,
    pub light: String,
    pub dark: String,
    /// Sidebar width in pixels, as last dragged, and whether it's hidden.
    pub sidebar_width: f32,
    pub sidebar_hidden: bool,
    /// The terminal's font family (SF Mono unless picked in Settings).
    pub terminal_font: String,
    /// Terminal text size in points, and line height as a multiple of it.
    pub terminal_font_size: f32,
    pub terminal_line_height: f32,
    /// The terminal cursor blinks (while the terminal has focus).
    pub terminal_cursor_blink: bool,
    /// The GitHub page's repository list: grouped by owner, and the groups
    /// toggled from how they start (owners start open, "shared with you" folded).
    pub github_group_by_owner: bool,
    pub github_folded_owners: Vec<String>,
    /// A session page split with a desktop: the terminal's share of the width.
    pub split_ratio: f32,
    /// Machines whose desktop is an item in the sidebar.
    pub desktops: Vec<String>,
    /// Terminals split into panes, each layout as text (see panes.rs).
    pub panes: Vec<String>,
    /// Favorite machines: first in the sidebar (so ⌘1, ⌘2, …), with a star.
    pub favorite_machines: Vec<String>,
    /// Machines' ports open on this computer, as "machine:port:local port"
    /// (opened again at launch).
    pub forwards: Vec<String>,
    /// How icons look (see kit::IconStyle): "minimal", "tiles", "soft", ...
    pub icon_style: String,
    /// Shortcuts the user changed: id → keys ("" for none). See shortcuts.rs.
    pub shortcuts: std::collections::BTreeMap<String, String>,
    /// Where to save; None in demo mode.
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            mode: Mode::System,
            light: DEFAULT_LIGHT.into(),
            dark: DEFAULT_DARK.into(),
            sidebar_width: super::root::SIDEBAR_DEFAULT,
            sidebar_hidden: false,
            terminal_font: super::terminal::DEFAULT_FONT.into(),
            terminal_font_size: TERMINAL_FONT_SIZE,
            terminal_line_height: TERMINAL_LINE_HEIGHT,
            terminal_cursor_blink: true,
            github_group_by_owner: false,
            github_folded_owners: Vec::new(),
            split_ratio: 0.5,
            desktops: Vec::new(),
            panes: Vec::new(),
            favorite_machines: Vec::new(),
            forwards: Vec::new(),
            icon_style: super::kit::IconStyle::default().id().into(),
            shortcuts: Default::default(),
            path: None,
        }
    }
}

impl Global for Appearance {}

impl Appearance {
    /// `[ui]` in config.toml.
    pub fn load(path: Option<PathBuf>) -> Appearance {
        let mut a: Appearance = path.as_deref().map(|p| crate::config::read_section(p, "ui")).unwrap_or_default();
        a.path = path;
        a
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Err(e) = crate::config::write_section(path, "ui", self) {
            tracing::warn!("saving appearance: {e}");
        }
    }

    /// Whether the dark theme shows, given the system's light/dark appearance.
    pub fn is_dark(&self, system: WindowAppearance) -> bool {
        match self.mode {
            Mode::Dark => true,
            Mode::Light => false,
            Mode::System => matches!(system, WindowAppearance::Dark | WindowAppearance::VibrantDark),
        }
    }

    /// The theme to show given the system's light/dark appearance.
    pub fn resolve(&self, system: WindowAppearance) -> Theme {
        let dark = self.is_dark(system);
        let (id, fallback) = if dark { (&self.dark, DEFAULT_DARK) } else { (&self.light, DEFAULT_LIGHT) };
        themes::find(id).filter(|p| p.dark == dark).or_else(|| themes::find(fallback)).expect("default themes exist").theme()
    }
}

/// Re-resolves the theme for this window (after a preference or system change).
pub fn apply(window: &Window, cx: &mut App) {
    let theme = cx.global::<Appearance>().resolve(window.appearance());
    let fonts = if theme.system_fonts {
        let mono = *SYSTEM_MONO.get_or_init(|| {
            let names = cx.text_system().all_font_names();
            ["SF Mono", "Menlo"].into_iter().find(|f| names.iter().any(|n| n == f)).unwrap_or(MONO_FONT)
        });
        (".SystemUIFont", mono)
    } else {
        (UI_FONT, MONO_FONT)
    };
    if let Ok(mut f) = FONTS.write() {
        *f = fonts;
    }
    super::terminal::set_font(&cx.global::<Appearance>().terminal_font);
    let style = super::kit::IconStyle::from_id(&cx.global::<Appearance>().icon_style);
    super::kit::set_look(super::kit::Look::new(style, &theme));
    #[cfg(windows)]
    title_bar(window, cx.global::<Appearance>().is_dark(window.appearance()), &theme);
    cx.set_global(theme);
}

/// Windows draws the title bar: dark when the theme is, in the theme's own
/// background and text colors (Windows 11; older versions keep theirs).
#[cfg(windows)]
fn title_bar(window: &Window, dark: bool, theme: &Theme) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
    let hwnd = h.hwnd.get() as *mut std::ffi::c_void;
    // COLORREF: 0x00BBGGRR.
    let colorref = |c: gpui::Hsla| {
        let c = gpui::Rgba::from(c);
        ((c.b * 255.) as u32) << 16 | ((c.g * 255.) as u32) << 8 | (c.r * 255.) as u32
    };
    let set = |attribute: i32, value: u32| unsafe {
        DwmSetWindowAttribute(hwnd, attribute as u32, &value as *const u32 as *const std::ffi::c_void, 4);
    };
    set(DWMWA_USE_IMMERSIVE_DARK_MODE as i32, dark as u32);
    set(DWMWA_CAPTION_COLOR as i32, colorref(theme.sidebar));
    set(DWMWA_TEXT_COLOR as i32, colorref(theme.text));
}

pub const TERMINAL_FONT_SIZE: f32 = 13.;
pub const TERMINAL_LINE_HEIGHT: f32 = 1.3;

/// Changes the terminal's font size or line height (kept in sensible
/// ranges), saves it, and redraws.
pub fn update_terminal(cx: &mut App, f: impl FnOnce(&mut Appearance)) {
    let mut appearance = cx.global::<Appearance>().clone();
    f(&mut appearance);
    appearance.terminal_font_size = appearance.terminal_font_size.round().clamp(8., 32.);
    appearance.terminal_line_height = ((appearance.terminal_line_height * 20.).round() / 20.).clamp(1., 2.);
    super::terminal::set_font(&appearance.terminal_font);
    appearance.save();
    cx.set_global(appearance);
    cx.refresh_windows();
}

/// Changes and saves a view preference; nothing needs re-theming.
pub fn remember(cx: &mut App, f: impl FnOnce(&mut Appearance)) {
    let mut appearance = cx.global::<Appearance>().clone();
    f(&mut appearance);
    appearance.save();
    cx.set_global(appearance);
}

/// Remembers the sidebar width; nothing needs re-theming.
pub fn save_sidebar_width(cx: &mut App, width: f32) {
    let mut appearance = cx.global::<Appearance>().clone();
    if appearance.sidebar_width != width {
        appearance.sidebar_width = width;
        appearance.save();
        cx.set_global(appearance);
    }
}

/// Changes the appearance preference, saves it, and re-themes the window.
/// Makes `id` the theme for its kind (light or dark), switching to that kind
/// when it isn't the one showing.
pub fn choose_theme(a: &mut Appearance, id: &str, system: WindowAppearance) {
    let Some(dark) = super::themes::find(id).map(|p| p.dark) else { return };
    if dark {
        a.dark = id.into();
    } else {
        a.light = id.into();
    }
    let showing_dark = match a.mode {
        Mode::Dark => true,
        Mode::Light => false,
        Mode::System => matches!(system, WindowAppearance::Dark | WindowAppearance::VibrantDark),
    };
    if showing_dark != dark {
        a.mode = if dark { Mode::Dark } else { Mode::Light };
    }
}

/// Shows these preferences without saving them (a preview).
pub fn preview_appearance(window: &mut Window, cx: &mut App, appearance: Appearance) {
    cx.set_global(appearance);
    apply(window, cx);
    window.refresh();
}

pub fn update_appearance(window: &mut Window, cx: &mut App, f: impl FnOnce(&mut Appearance, WindowAppearance)) {
    let system = window.appearance();
    let mut appearance = cx.global::<Appearance>().clone();
    f(&mut appearance, system);
    appearance.save();
    cx.set_global(appearance);
    apply(window, cx);
    window.refresh();
}
