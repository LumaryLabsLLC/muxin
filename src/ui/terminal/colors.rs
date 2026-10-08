//! Terminal colors: the 16 ANSI colors, the 256-color cube, and truecolor,
//! with the semantic ones (red, green, yellow, blue) taken from the app theme.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui::{Hsla, Rgba};

use crate::ui::theme::Theme;

// One Dark-ish and GitHub-light-ish bases; 0-7 normal, 8-15 bright.
const DARK: [u32; 16] = [
    0x2c313a, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, //
    0x5c6370, 0xff7b86, 0xb5e890, 0xffd68a, 0x7cc4ff, 0xde99f0, 0x74d0dc, 0xe6e6e6,
];
const LIGHT: [u32; 16] = [
    0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781, //
    0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
];

#[derive(Clone, Copy)]
pub struct Palette {
    pub fg: Hsla,
    pub bg: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub ansi: [Hsla; 16],
}

impl Palette {
    pub fn new(theme: &Theme) -> Palette {
        let dark = theme.canvas.l < 0.5;
        let mut ansi = (if dark { DARK } else { LIGHT }).map(hex);
        // The theme's own status colors, so the terminal matches the rest of the app.
        for (i, c) in [(1, theme.error), (2, theme.success), (3, theme.warning), (4, theme.info)] {
            ansi[i] = c;
            ansi[i + 8] = shift(c, if dark { 0.08 } else { -0.06 });
        }
        Palette { fg: theme.text, bg: theme.canvas, cursor: theme.accent, selection: theme.selection, ansi }
    }

    /// Resolves a cell color, honoring colors the program changed (OSC 4/10/11).
    pub fn resolve(&self, color: Color, overrides: &Colors) -> Hsla {
        match color {
            Color::Spec(rgb) => from_rgb(rgb),
            Color::Indexed(i) => overrides[i as usize].map(from_rgb).unwrap_or_else(|| self.indexed(i)),
            Color::Named(n) => overrides[n as usize].map(from_rgb).unwrap_or_else(|| self.named(n)),
        }
    }

    pub fn named(&self, n: NamedColor) -> Hsla {
        let i = n as usize;
        match n {
            NamedColor::Foreground | NamedColor::BrightForeground => self.fg,
            NamedColor::Background => self.bg,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimForeground => dim(self.fg),
            _ if i < 16 => self.ansi[i],
            // Dim black..white follow Foreground/Background/Cursor in the enum.
            _ => dim(self.ansi[(i - NamedColor::DimBlack as usize) % 8]),
        }
    }

    fn indexed(&self, i: u8) -> Hsla {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v as u32 * 40 };
                hex(level(i / 36) << 16 | level((i / 6) % 6) << 8 | level(i % 6))
            }
            _ => {
                let v = 8 + (i as u32 - 232) * 10;
                hex(v << 16 | v << 8 | v)
            }
        }
    }

    /// An Rgb for answering color queries.
    pub fn rgb(&self, index: usize) -> Rgb {
        let c = match index {
            256 => self.fg,
            257 => self.bg,
            258 => self.cursor,
            i if i < 256 => self.indexed(i as u8),
            _ => self.fg,
        };
        let r = Rgba::from(c);
        Rgb { r: (r.r * 255.) as u8, g: (r.g * 255.) as u8, b: (r.b * 255.) as u8 }
    }
}

pub fn dim(c: Hsla) -> Hsla {
    Hsla { a: c.a * 0.66, ..c }
}

fn shift(c: Hsla, by: f32) -> Hsla {
    Hsla { l: (c.l + by).clamp(0., 1.), ..c }
}

fn hex(v: u32) -> Hsla {
    gpui::rgb(v).into()
}

fn from_rgb(c: Rgb) -> Hsla {
    hex((c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32)
}
