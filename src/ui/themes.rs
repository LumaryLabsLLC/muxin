//! Built-in color themes. Each is a small palette of base colors; hover,
//! selection and track colors are derived so every theme behaves the same.

use gpui::{Hsla, Rgba, black, rgba};

use super::theme::Theme;

pub struct Palette {
    pub id: &'static str,
    pub name: &'static str,
    pub dark: bool,
    /// Use the platform's UI font instead of the bundled Geist.
    pub system_fonts: bool,
    pub sidebar: u32,
    pub canvas: u32,
    pub surface: u32,
    pub elevated: u32,
    pub border: u32,
    pub border_variant: u32,
    pub text: u32,
    pub text_muted: u32,
    pub text_placeholder: u32,
    /// Brand color; also the "working" state.
    pub accent: u32,
    /// A second brand color (the icon's orange): links and accent text, and
    /// the far end of primary buttons' gradient. The accent itself elsewhere.
    pub accent2: u32,
    /// Also the "idle" state.
    pub success: u32,
    pub warning: u32,
    pub error: u32,
    pub info: u32,
    /// Looks like a TUI in a terminal: one monospace font, square corners, no
    /// shadows or gradients, `[ Buttons ]` and `── Headings ──` (see kit::square).
    pub tui: bool,
    /// A selected row's color, a solid bar (0: the accent, faintly, as usual).
    pub bar: u32,
}

macro_rules! palette {
    ($id:literal, $name:literal, $dark:literal,
     sidebar $sidebar:literal, canvas $canvas:literal, surface $surface:literal, elevated $elevated:literal,
     border $border:literal, $border_variant:literal,
     text $text:literal, $muted:literal, $placeholder:literal,
     accent $accent:literal, success $success:literal, warning $warning:literal, error $error:literal, info $info:literal) => {
        Palette {
            id: $id,
            name: $name,
            dark: $dark,
            system_fonts: false,
            sidebar: $sidebar,
            canvas: $canvas,
            surface: $surface,
            elevated: $elevated,
            border: $border,
            border_variant: $border_variant,
            text: $text,
            text_muted: $muted,
            text_placeholder: $placeholder,
            accent: $accent,
            accent2: $accent,
            success: $success,
            warning: $warning,
            error: $error,
            info: $info,
            tui: false,
            bar: 0,
        }
    };
}

/// A TUI look's palette: its selection bar a solid color, one accent, no gradient.
macro_rules! tui {
    ($id:literal, $name:literal, $dark:literal, bar $bar:literal,
     sidebar $sidebar:literal, canvas $canvas:literal, surface $surface:literal, elevated $elevated:literal,
     border $border:literal, $border_variant:literal,
     text $text:literal, $muted:literal, $placeholder:literal,
     accent $accent:literal, success $success:literal, warning $warning:literal, error $error:literal, info $info:literal) => {
        Palette {
            tui: true,
            bar: $bar,
            ..palette!($id, $name, $dark,
                sidebar $sidebar, canvas $canvas, surface $surface, elevated $elevated,
                border $border, $border_variant,
                text $text, $muted, $placeholder,
                accent $accent, success $success, warning $warning, error $error, info $info)
        }
    };
}

pub const DEFAULT_DARK: &str = "dusk";
pub const DEFAULT_LIGHT: &str = "dawn";

pub static PALETTES: &[Palette] = &[
    // --- Dark ---------------------------------------------------------------
    palette!("graphite", "Muxin Graphite", true,
        sidebar 0x0f1013, canvas 0x15161a, surface 0x1b1c21, elevated 0x202127, border 0x2b2d35, 0x24262d,
        text 0xececf1, 0xa2a4ae, 0x6c6f7b,
        accent 0x8b7cff, success 0x4ad6a7, warning 0xf2b347, error 0xff6b6b, info 0x6cb8ff),
    // The app icon's colors: its navy for the window, its violet for
    // selection and focus, its orange for links and accent text, and the two
    // as a gradient on primary buttons (as on the icon).
    Palette {
        accent2: 0xe8734a,
        ..palette!("dusk", "Muxin Dusk", true,
            sidebar 0x151834, canvas 0x1a1e3c, surface 0x20254a, elevated 0x272d56, border 0x363d6e, 0x2b3160,
            text 0xeeecfb, 0xaeb0d8, 0x7a7fae,
            accent 0xa66bf2, success 0x52d6ad, warning 0xf2c94c, error 0xff6e86, info 0x7fa4ff)
    },
    // Apple's dark Aqua: system blue accent, graphite surfaces, SF fonts.
    Palette {
        id: "macos-dark",
        name: "macOS Dark",
        dark: true,
        system_fonts: true,
        sidebar: 0x2a2a2c,
        canvas: 0x1e1e1e,
        surface: 0x282828,
        elevated: 0x2c2c2e,
        border: 0x3d3d3f,
        border_variant: 0x333335,
        text: 0xf5f5f7,
        text_muted: 0xa1a1a6,
        text_placeholder: 0x6e6e73,
        accent: 0x0a84ff,
        accent2: 0x0a84ff,
        success: 0x30d158,
        warning: 0xff9f0a,
        error: 0xff453a,
        info: 0x40c8e0,
        tui: false,
        bar: 0,
    },
    palette!("gruvbox-dark", "Gruvbox Dark", true,
        sidebar 0x1d2021, canvas 0x282828, surface 0x32302f, elevated 0x3c3836, border 0x504945, 0x3c3836,
        text 0xebdbb2, 0xbdae93, 0x928374,
        accent 0xfe8019, success 0xb8bb26, warning 0xfabd2f, error 0xfb4934, info 0x83a598),
    palette!("catppuccin-mocha", "Catppuccin Mocha", true,
        sidebar 0x181825, canvas 0x1e1e2e, surface 0x262637, elevated 0x313244, border 0x45475a, 0x313244,
        text 0xcdd6f4, 0xa6adc8, 0x7f849c,
        accent 0xcba6f7, success 0xa6e3a1, warning 0xf9e2af, error 0xf38ba8, info 0x89b4fa),
    palette!("catppuccin-macchiato", "Catppuccin Macchiato", true,
        sidebar 0x1e2030, canvas 0x24273a, surface 0x2b2f44, elevated 0x363a4f, border 0x494d64, 0x363a4f,
        text 0xcad3f5, 0xa5adcb, 0x8087a2,
        accent 0xc6a0f6, success 0xa6da95, warning 0xeed49f, error 0xed8796, info 0x8aadf4),
    palette!("catppuccin-frappe", "Catppuccin Frappé", true,
        sidebar 0x292c3c, canvas 0x303446, surface 0x383c50, elevated 0x414559, border 0x51576d, 0x414559,
        text 0xc6d0f5, 0xa5adce, 0x838ba7,
        accent 0xca9ee6, success 0xa6d189, warning 0xe5c890, error 0xe78284, info 0x8caaee),
    palette!("tokyo-night", "Tokyo Night", true,
        sidebar 0x16161e, canvas 0x1a1b26, surface 0x1f2335, elevated 0x24283b, border 0x3b4261, 0x292e42,
        text 0xc0caf5, 0xa9b1d6, 0x565f89,
        accent 0x7aa2f7, success 0x9ece6a, warning 0xe0af68, error 0xf7768e, info 0x7dcfff),
    palette!("nord", "Nord", true,
        sidebar 0x2a2f3a, canvas 0x2e3440, surface 0x343a48, elevated 0x3b4252, border 0x4c566a, 0x3b4252,
        text 0xeceff4, 0xd8dee9, 0x7b88a1,
        accent 0x88c0d0, success 0xa3be8c, warning 0xebcb8b, error 0xbf616a, info 0x81a1c1),
    palette!("rose-pine", "Rosé Pine", true,
        sidebar 0x1f1d2e, canvas 0x191724, surface 0x1f1d2e, elevated 0x26233a, border 0x403d52, 0x26233a,
        text 0xe0def4, 0x908caa, 0x6e6a86,
        accent 0xc4a7e7, success 0x9ccfd8, warning 0xf6c177, error 0xeb6f92, info 0xebbcba),
    palette!("solarized-dark", "Solarized Dark", true,
        sidebar 0x00252f, canvas 0x002b36, surface 0x073642, elevated 0x0a3d4a, border 0x2a525c, 0x0d3f4b,
        text 0xe2ded0, 0x93a1a1, 0x657b83,
        accent 0x268bd2, success 0x859900, warning 0xb58900, error 0xdc322f, info 0x2aa198),
    palette!("dracula", "Dracula", true,
        sidebar 0x21222c, canvas 0x282a36, surface 0x2e303e, elevated 0x343746, border 0x44475a, 0x343746,
        text 0xf8f8f2, 0xbfc0cc, 0x6272a4,
        accent 0xbd93f9, success 0x50fa7b, warning 0xffb86c, error 0xff5555, info 0x8be9fd),
    palette!("everforest-dark", "Everforest Dark", true,
        sidebar 0x232a2e, canvas 0x2d353b, surface 0x343f44, elevated 0x3d484d, border 0x475258, 0x3d484d,
        text 0xd3c6aa, 0x9da9a0, 0x7a8478,
        accent 0x7fbbb3, success 0xa7c080, warning 0xdbbc7f, error 0xe67e80, info 0x83c092),
    palette!("kanagawa-wave", "Kanagawa Wave", true,
        sidebar 0x16161d, canvas 0x1f1f28, surface 0x2a2a37, elevated 0x363646, border 0x54546d, 0x363646,
        text 0xdcd7ba, 0xc8c093, 0x727169,
        accent 0x7e9cd8, success 0x98bb6c, warning 0xe6c384, error 0xe46876, info 0x7fb4ca),
    // Muxin as a TUI: a black terminal with a green accent and a deep blue
    // selection bar, and Midnight Commander's navy with cyan lines. A bar is
    // a deep tone, so a selected row's icons and grey text still read on it.
    tui!("terminal", "Terminal", true, bar 0x1e3a52,
        sidebar 0x111111, canvas 0x0c0c0c, surface 0x141414, elevated 0x1a1a1a, border 0x3a3a3a, 0x262626,
        text 0xd0d0d0, 0x9a9a9a, 0x707070,
        accent 0x5fd75f, success 0x00af87, warning 0xd7af5f, error 0xff5f5f, info 0x5fafd7),
    tui!("terminal-midnight", "Terminal Midnight", true, bar 0x123f5e,
        sidebar 0x0a1630, canvas 0x0b1a36, surface 0x0e2042, elevated 0x12274e, border 0x2f6fa8, 0x1d3f6e,
        text 0xdfe7f5, 0x9fb3d4, 0x647aa0,
        accent 0xffd75f, success 0x5fd787, warning 0xffaf5f, error 0xff6f6f, info 0x5fd7ff),
    palette!("github-dark", "GitHub Dark", true,
        sidebar 0x010409, canvas 0x0d1117, surface 0x161b22, elevated 0x1c2128, border 0x30363d, 0x21262d,
        text 0xe6edf3, 0x9198a1, 0x6e7681,
        accent 0x4493f8, success 0x3fb950, warning 0xd29922, error 0xf85149, info 0x58a6ff),
    // --- Light --------------------------------------------------------------
    palette!("paper", "Muxin Paper", false,
        sidebar 0xf0f0f3, canvas 0xfafafb, surface 0xffffff, elevated 0xffffff, border 0xe2e2e8, 0xebebf0,
        text 0x16171c, 0x5c5f6a, 0x8c8f9a,
        accent 0x6d5cf6, success 0x12a27a, warning 0xc8870e, error 0xe5484d, info 0x2f80ed),
    // The app icon's colors in daylight: lavender white, its navy for text,
    // deeper violet and orange so they read on white.
    Palette {
        accent2: 0xc0501a,
        ..palette!("dawn", "Muxin Dawn", false,
            sidebar 0xf2eff9, canvas 0xfaf8fe, surface 0xffffff, elevated 0xffffff, border 0xe1dcf0, 0xebe7f6,
            text 0x1e2148, 0x585c88, 0x8b8eb2,
            accent 0x7d45e4, success 0x0c8f6b, warning 0x9a6a00, error 0xd8435a, info 0x3b6fe0)
    },
    // Apple's light Aqua, like System Settings: white grouped boxes on a light grey window.
    Palette {
        id: "macos-light",
        name: "macOS Light",
        dark: false,
        system_fonts: true,
        sidebar: 0xe8e8ea,
        canvas: 0xf5f5f7,
        surface: 0xffffff,
        elevated: 0xffffff,
        border: 0xd2d2d7,
        border_variant: 0xe3e3e6,
        text: 0x1d1d1f,
        text_muted: 0x6e6e73,
        text_placeholder: 0xa1a1a6,
        accent: 0x007aff,
        accent2: 0x007aff,
        success: 0x28c840,
        warning: 0xff9500,
        error: 0xff3b30,
        info: 0x30b0c7,
        tui: false,
        bar: 0,
    },
    palette!("gruvbox-light", "Gruvbox Light", false,
        sidebar 0xf2e5bc, canvas 0xfbf1c7, surface 0xf9f5d7, elevated 0xf9f5d7, border 0xd5c4a1, 0xebdbb2,
        text 0x3c3836, 0x665c54, 0x928374,
        accent 0xaf3a03, success 0x79740e, warning 0xb57614, error 0x9d0006, info 0x076678),
    palette!("catppuccin-latte", "Catppuccin Latte", false,
        sidebar 0xe6e9ef, canvas 0xeff1f5, surface 0xf7f8fb, elevated 0xffffff, border 0xccd0da, 0xdce0e8,
        text 0x4c4f69, 0x6c6f85, 0x8c8fa1,
        accent 0x8839ef, success 0x40a02b, warning 0xdf8e1d, error 0xd20f39, info 0x1e66f5),
    palette!("tokyo-night-day", "Tokyo Night Day", false,
        sidebar 0xd6d8df, canvas 0xe1e2e7, surface 0xe9e9ed, elevated 0xf2f2f5, border 0xc4c8da, 0xd0d3de,
        text 0x3760bf, 0x6172b0, 0x848cb5,
        accent 0x2e7de9, success 0x587539, warning 0x8c6c3e, error 0xf52a65, info 0x007197),
    palette!("nord-light", "Nord Light", false,
        sidebar 0xe5e9f0, canvas 0xeceff4, surface 0xf5f7fa, elevated 0xffffff, border 0xd8dee9, 0xe5e9f0,
        text 0x2e3440, 0x4c566a, 0x7b88a1,
        accent 0x5e81ac, success 0x6a8e4f, warning 0xc49b3a, error 0xbf616a, info 0x81a1c1),
    palette!("rose-pine-dawn", "Rosé Pine Dawn", false,
        sidebar 0xf2e9e1, canvas 0xfaf4ed, surface 0xfffaf3, elevated 0xfffaf3, border 0xdfdad9, 0xf2e9e1,
        text 0x575279, 0x797593, 0x9893a5,
        accent 0x907aa9, success 0x56949f, warning 0xea9d34, error 0xb4637a, info 0x286983),
    palette!("solarized-light", "Solarized Light", false,
        sidebar 0xeee8d5, canvas 0xfdf6e3, surface 0xf8f1dd, elevated 0xfdf6e3, border 0xe0d9c4, 0xeee8d5,
        text 0x073642, 0x586e75, 0x93a1a1,
        accent 0x268bd2, success 0x859900, warning 0xb58900, error 0xdc322f, info 0x2aa198),
    palette!("everforest-light", "Everforest Light", false,
        sidebar 0xf4f0d9, canvas 0xfdf6e3, surface 0xfffbef, elevated 0xfffbef, border 0xe0dcc7, 0xefebd4,
        text 0x5c6a72, 0x829181, 0xa6b0a0,
        accent 0x3a94c5, success 0x8da101, warning 0xdfa000, error 0xf85552, info 0x35a77c),
    palette!("kanagawa-lotus", "Kanagawa Lotus", false,
        sidebar 0xe5ddb0, canvas 0xf2ecbc, surface 0xf7f2cf, elevated 0xf9f5d8, border 0xd5cea3, 0xe4dcb2,
        text 0x545464, 0x716e61, 0x8a8980,
        accent 0x4d699b, success 0x6f894e, warning 0xcc6d00, error 0xc84053, info 0x597b75),
    palette!("github-light", "GitHub Light", false,
        sidebar 0xf6f8fa, canvas 0xffffff, surface 0xffffff, elevated 0xffffff, border 0xd0d7de, 0xd8dee4,
        text 0x1f2328, 0x59636e, 0x818b98,
        accent 0x0969da, success 0x1a7f37, warning 0x9a6700, error 0xd1242f, info 0x0969da),
    // Muxin as a TUI in a light terminal: black on off-white, a yellow selection bar.
    tui!("terminal-paper", "Terminal Paper", false, bar 0xffd75f,
        sidebar 0xefeee5, canvas 0xf7f6ef, surface 0xfbfaf4, elevated 0xffffff, border 0xb9b8ad, 0xd6d5ca,
        text 0x1c1c1c, 0x55554d, 0x8e8e84,
        accent 0x005fd7, success 0x2e7d32, warning 0x9a6700, error 0xc62828, info 0x005f87),
];

pub fn find(id: &str) -> Option<&'static Palette> {
    PALETTES.iter().find(|p| p.id == id)
}

fn color(hex: u32) -> Hsla {
    let rgba: Rgba = rgba((hex << 8) | 0xff);
    rgba.into()
}

/// Perceived brightness, 0..1, for picking readable text on a colored fill.
fn luminance(c: Hsla) -> f32 {
    let rgba: Rgba = c.into();
    0.2126 * rgba.r + 0.7152 * rgba.g + 0.0722 * rgba.b
}

impl Palette {
    pub fn theme(&self) -> Theme {
        let text = color(self.text);
        let accent = color(self.accent);
        let accent2 = color(self.accent2);
        let dark = self.dark;
        Theme {
            id: self.id,
            system_fonts: self.system_fonts,
            tui: self.tui,
            sidebar: color(self.sidebar),
            canvas: color(self.canvas),
            surface: color(self.surface),
            elevated: color(self.elevated),
            border: color(self.border),
            border_variant: color(self.border_variant),
            border_focused: accent,
            element: text.opacity(if dark { 0.05 } else { 0.035 }),
            element_hover: text.opacity(if dark { 0.09 } else { 0.065 }),
            element_selected: if self.bar != 0 { color(self.bar) } else { accent.opacity(if dark { 0.18 } else { 0.13 }) },
            ghost_hover: text.opacity(if dark { 0.06 } else { 0.05 }),
            text,
            text_muted: color(self.text_muted),
            text_placeholder: color(self.text_placeholder),
            text_accent: accent2,
            accent,
            accent2,
            accent_text: if luminance(accent) > 0.55 { color(self.canvas).blend(black().opacity(0.4)) } else { gpui::white() },
            success: color(self.success),
            warning: color(self.warning),
            error: color(self.error),
            info: color(self.info),
            selection: accent.opacity(0.3),
            scrim: black().opacity(if dark { 0.55 } else { 0.22 }),
            track: text.opacity(if dark { 0.1 } else { 0.08 }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG's contrast ratio between two colors.
    fn contrast(a: u32, b: u32) -> f32 {
        let (la, lb) = (luminance_wcag(a), luminance_wcag(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    fn luminance_wcag(hex: u32) -> f32 {
        let f = |c: u32| {
            let c = c as f32 / 255.;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(hex >> 16 & 0xff) + 0.7152 * f(hex >> 8 & 0xff) + 0.0722 * f(hex & 0xff)
    }

    #[test]
    fn a_selection_bar_leaves_a_rows_text_and_icons_readable() {
        for p in PALETTES.iter().filter(|p| p.bar != 0) {
            assert!(contrast(p.text, p.bar) >= 4.5, "{}: text on its bar", p.id);
            // Icons and secondary text are drawn muted; the faintest grey stays visible.
            assert!(contrast(p.text_muted, p.bar) >= 4.0, "{}: muted on its bar", p.id);
            assert!(contrast(p.text_placeholder, p.bar) >= 2.2, "{}: faint text on its bar", p.id);
        }
    }

    #[test]
    fn ids_are_unique_and_defaults_exist() {
        let mut ids: Vec<_> = PALETTES.iter().map(|p| p.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), PALETTES.len());
        assert!(find(DEFAULT_DARK).is_some_and(|p| p.dark));
        assert!(find(DEFAULT_LIGHT).is_some_and(|p| !p.dark));
    }
}
