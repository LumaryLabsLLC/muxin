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

    pub fn machine_status(&self, status: &str) -> Hsla {
        match status {
            "online" => self.success,
            "offline" => self.error,
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
// Appearance preference (ui.json)

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
    /// Where to save; None in demo mode.
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance { mode: Mode::System, light: DEFAULT_LIGHT.into(), dark: DEFAULT_DARK.into(), path: None }
    }
}

impl Global for Appearance {}

impl Appearance {
    pub fn load(path: Option<PathBuf>) -> Appearance {
        let mut a: Appearance = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        a.path = path;
        a
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(e) = crate::config::write_atomic(path, &bytes) {
                    tracing::warn!("saving appearance: {e:#}");
                }
            }
            Err(e) => tracing::warn!("saving appearance: {e}"),
        }
    }

    /// The theme to show given the system's light/dark appearance.
    pub fn resolve(&self, system: WindowAppearance) -> Theme {
        let dark = match self.mode {
            Mode::Dark => true,
            Mode::Light => false,
            Mode::System => matches!(system, WindowAppearance::Dark | WindowAppearance::VibrantDark),
        };
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
    cx.set_global(theme);
}

/// Changes the appearance preference, saves it, and re-themes the window.
pub fn update_appearance(window: &mut Window, cx: &mut App, f: impl FnOnce(&mut Appearance, WindowAppearance)) {
    let system = window.appearance();
    let mut appearance = cx.global::<Appearance>().clone();
    f(&mut appearance, system);
    appearance.save();
    cx.set_global(appearance);
    apply(window, cx);
    window.refresh();
}
