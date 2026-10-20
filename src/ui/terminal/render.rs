//! Draws the terminal grid on a canvas: cell backgrounds, then text shaped in
//! runs pinned to the cell grid (`force_width`), then the cursor.

use std::sync::{Arc, OnceLock};
use std::ops::Range;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use gpui::{
    App, Bounds, ContentMask, ElementInputHandler, Entity, Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Point, ShapedLine, SharedString, StrikethroughStyle,
    TextRun, UnderlineStyle, Window, canvas, fill, point, prelude::*, px, size,
};

use super::boxdraw::{self, Shape};
use super::colors::{Palette, dim};
use super::ime::{Composition, TerminalInputHandler};
use super::{Geometry, Metrics, PAD_X, PAD_Y, Shared, TerminalView};
use crate::ui::theme::{ActiveTheme, mono_font};

/// The terminal font when none is chosen, and what's used when the chosen
/// one isn't installed: SF Mono, then Menlo, then the app's monospace font.
pub const DEFAULT_FONT: &str = "SF Mono";
const FALLBACKS: &[&str] = &["SF Mono", "Menlo"];

/// The font the user picked (Settings › Terminal; see `set_font`).
static CHOSEN: std::sync::RwLock<Option<SharedString>> = std::sync::RwLock::new(None);

/// Sets the terminal font (from the user's preferences).
pub fn set_font(family: &str) {
    if let Ok(mut c) = CHOSEN.write() {
        *c = Some(SharedString::from(family.to_string()));
    }
}

fn installed(window: &Window) -> &'static [String] {
    static INSTALLED: OnceLock<Vec<String>> = OnceLock::new();
    INSTALLED.get_or_init(|| window.text_system().all_font_names())
}

/// Installed families whose letters are all one width, for the font picker.
pub fn monospace_families(window: &Window) -> Vec<SharedString> {
    static MONO: OnceLock<Vec<SharedString>> = OnceLock::new();
    MONO.get_or_init(|| {
        let ts = window.text_system();
        let mut out: Vec<SharedString> = installed(window)
            .iter()
            // Hidden system faces (".SF NS Mono") aren't meant to be picked, and
            // even-width fonts for other scripts or symbols aren't for code.
            .filter(|name| !name.starts_with('.'))
            .filter(|name| {
                let n = name.to_lowercase();
                ["mono", "code", "courier", "menlo", "monaco", "consol", "hack", "iosevka", "nerd", "term", "fixed", "input", "fira"].iter().any(|w| n.contains(w))
            })
            .filter(|name| {
                let id = ts.resolve_font(&gpui::font(SharedString::from(name.to_string())));
                let w = |c| ts.advance(id, px(12.), c).map(|s| f32::from(s.width)).ok();
                matches!((w('i'), w('M'), w('.')), (Some(a), Some(b), Some(c)) if a > 0. && (a - b).abs() < 0.01 && (a - c).abs() < 0.01)
            })
            .map(|n| SharedString::from(n.to_string()))
            .collect();
        out.sort_by_key(|n| n.to_lowercase());
        out.dedup();
        out
    })
    .clone()
}

/// One drawable cell, copied out of the grid so the lock is held briefly.
struct CellOut {
    row: usize,
    col: usize,
    c: char,
    fg: Hsla,
    bg: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: Option<bool>,
    strike: bool,
    wide: bool,
}

pub(super) struct Frame {
    line_h: Pixels,
    rects: Vec<(Bounds<Pixels>, Hsla)>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    shapes: Vec<Shape>,
    cursor: Vec<(Bounds<Pixels>, Hsla)>,
    bounds: Bounds<Pixels>,
    preedit: Option<InputLayout>,
}

/// `cursor`: draw the cursor (false in a blink's off phase).
pub(super) fn grid(view: Entity<TerminalView>, palette: Palette, focused: bool, cursor: bool, metrics: Metrics) -> impl IntoElement {
    let entity = view.clone();
    canvas(
        move |bounds, window, cx| {
            let view = entity.read(cx);
            let mut frame = prepaint(&view.shared, &palette, focused, cursor && view.ime.text.is_empty(), metrics, bounds, window);
            if focused && !view.ime.text.is_empty() {
                frame.preedit = Some(input_layout(&view.shared, &view.ime, metrics, window, cx));
            }
            frame
        },
        move |bounds, frame, window, cx| {
            let focus = view.read(cx).focus.clone();
            // Registration must happen during paint, and only the focused
            // terminal receives the platform's text-input callbacks.
            window.handle_input(&focus, TerminalInputHandler(ElementInputHandler::new(bounds, view)), cx);
            paint(frame, window, cx);
        },
    ).size_full()
}

/// A local preedit overlay, also used to position the native candidate window.
pub(super) struct InputLayout {
    pub line: ShapedLine,
    pub origin: Point<Pixels>,
    line_h: Pixels,
    selected: Range<usize>,
    bg: Hsla,
    fg: Hsla,
    selection: Hsla,
}

impl InputLayout {
    pub fn bounds_for_range(&self, range: Range<usize>) -> Bounds<Pixels> {
        let start = self.line.x_for_index(range.start);
        let end = self.line.x_for_index(range.end);
        Bounds::new(self.origin + point(start, px(0.)), size((end - start).max(px(1.)), self.line_h))
    }
}

pub(super) fn input_layout(shared: &Shared, ime: &Composition, metrics: Metrics, window: &Window, cx: &App) -> InputLayout {
    let palette = Palette::new(&cx.theme());
    let text = SharedString::from(ime.text.clone());
    let run = TextRun {
        len: text.len(), font: base_font(window), color: palette.fg, background_color: None,
        underline: Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false }), strikethrough: None,
    };
    let line = window.text_system().shape_line(text, px(metrics.size), &[run], None);
    let g = *shared.geometry.lock().unwrap();
    let term = shared.term.lock();
    let cursor = term.renderable_content().cursor.point;
    let row = (cursor.line.0 + term.grid().display_offset() as i32).clamp(0, g.rows.saturating_sub(1) as i32);
    let col = cursor.column.0.min(g.cols.saturating_sub(1));
    let left = f32::from(g.origin.x);
    let right = left + g.cols as f32 * g.cell_w;
    // Shift a long preedit left at the right edge rather than hiding the
    // candidate anchor outside the terminal. The normal content mask clips it.
    let x = (left + col as f32 * g.cell_w).min((right - f32::from(line.width)).max(left));
    let origin = point(px(x), g.origin.y + px(row as f32 * g.line_h));
    InputLayout { line, origin, line_h: px(g.line_h.max(1.)), selected: ime.selected.clone(), bg: palette.bg, fg: palette.fg, selection: palette.selection }
}

fn base_font(window: &Window) -> Font {
    let installed = installed(window);
    let has = |f: &str| installed.iter().any(|n| n == f);
    let chosen = CHOSEN.read().ok().and_then(|c| c.clone()).unwrap_or_else(|| DEFAULT_FONT.into());
    let family = if has(&chosen) {
        chosen
    } else {
        FALLBACKS.iter().find(|f| has(f)).map(|f| SharedString::from(*f)).unwrap_or_else(|| mono_font().into())
    };
    let mut font = gpui::font(family);
    // Icons the font lacks (Nerd Font glyphs): from the bundled symbols font.
    // On Windows, then its own monochrome symbols, before the system's
    // fallback reaches Segoe UI Emoji (Claude Code's ⏺ as a blue square).
    let mut fallbacks: Vec<String> = crate::ui::assets::NERD_SYMBOLS_NAMES.iter().map(|n| n.to_string()).collect();
    if cfg!(windows) {
        fallbacks.extend(["Segoe UI Symbol", "Cascadia Mono", "Consolas"].map(String::from));
    }
    font.fallbacks = Some(gpui::FontFallbacks::from_fonts(fallbacks));
    // Ligatures would merge cells and break the grid.
    font.features = FontFeatures(Arc::new(vec![("calt".into(), 0), ("liga".into(), 0)]));
    font
}

/// Cell width and line height for the terminal font at these metrics.
pub(super) fn cell_size(window: &Window, m: Metrics) -> (f32, f32) {
    let ts = window.text_system();
    let id = ts.resolve_font(&base_font(window));
    let w = ts.advance(id, px(m.size), 'm').map(|s| f32::from(s.width)).unwrap_or(m.size * 0.6);
    (w, (m.size * m.line_height).round())
}

/// The terminal font's family name (Settings shows it).
pub(super) fn family(window: &Window) -> SharedString {
    base_font(window).family
}

#[allow(clippy::too_many_arguments)]
fn prepaint(shared: &Shared, palette: &Palette, focused: bool, show_cursor: bool, m: Metrics, bounds: Bounds<Pixels>, window: &mut Window) -> Frame {
    let (cell_w, line_h) = cell_size(window, m);
    let fixed = *shared.fixed.lock().unwrap();
    let (cols, rows) = match fixed {
        Some(size) => size,
        None => {
            let cols = ((f32::from(bounds.size.width) - 2. * PAD_X) / cell_w).floor().max(2.) as usize;
            let rows = ((f32::from(bounds.size.height) - 2. * PAD_Y) / line_h).floor().max(1.) as usize;
            shared.resize(cols, rows, cell_w, line_h);
            (cols, rows)
        }
    };
    let origin = bounds.origin + point(px(PAD_X), px(PAD_Y));
    *shared.geometry.lock().unwrap() = Geometry { origin, cell_w, line_h, cols, rows };

    // Copy what's visible out of the grid.
    let (cells, cursor) = {
        let term = shared.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let overrides = content.colors;
        let selection = content.selection;
        let cur = content.cursor;
        let screen_rows = term.screen_lines() as i32;
        let mut cells = Vec::with_capacity(cols * rows);
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row >= screen_rows {
                continue;
            }
            let cell = indexed.cell;
            let flags = cell.flags;
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = palette.resolve(cell.fg, overrides);
            let bg_color = cell.bg;
            let mut bg = (bg_color != Color::Named(NamedColor::Background)).then(|| palette.resolve(bg_color, overrides));
            if flags.contains(Flags::INVERSE) {
                let back = bg.unwrap_or(palette.bg);
                bg = Some(fg);
                fg = back;
            }
            if flags.contains(Flags::DIM) {
                fg = dim(fg);
            }
            if flags.contains(Flags::HIDDEN) {
                fg = bg.unwrap_or(palette.bg);
            }
            if selection.is_some_and(|s| s.contains(indexed.point)) {
                bg = Some(palette.selection);
            }
            let is_cursor = indexed.point == cur.point;
            if is_cursor && focused && show_cursor && cur.shape == CursorShape::Block {
                bg = Some(palette.cursor);
                fg = palette.bg;
            }
            let underline = if flags.contains(Flags::UNDERCURL) {
                Some(true)
            } else if flags.intersects(Flags::ALL_UNDERLINES) {
                Some(false)
            } else {
                None
            };
            cells.push(CellOut {
                row: row as usize,
                col: indexed.point.column.0,
                c: cell.c,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline,
                strike: flags.contains(Flags::STRIKEOUT),
                wide: flags.contains(Flags::WIDE_CHAR),
            });
        }
        let cursor_row = cur.point.line.0 + offset;
        let cursor = (show_cursor && cur.shape != CursorShape::Hidden && cursor_row >= 0 && cursor_row < screen_rows)
            .then(|| (cursor_row as usize, cur.point.column.0, cur.shape, term.grid()[cur.point].flags.contains(Flags::WIDE_CHAR)));
        (cells, cursor)
    };

    let at = |row: usize, col: usize| origin + point(px(col as f32 * cell_w), px(row as f32 * line_h));
    let mut frame = Frame { line_h: px(line_h), rects: Vec::new(), lines: Vec::new(), shapes: Vec::new(), cursor: Vec::new(), bounds, preedit: None };

    // Backgrounds, merged along each row.
    let mut run: Option<(usize, usize, usize, Hsla)> = None; // row, start col, end col (exclusive), color
    let flush_rect = |run: &mut Option<(usize, usize, usize, Hsla)>, frame: &mut Frame| {
        if let Some((row, start, end, color)) = run.take() {
            let b = Bounds::new(at(row, start), size(px((end - start) as f32 * cell_w), px(line_h)));
            frame.rects.push((b, color));
        }
    };
    for c in &cells {
        let width = if c.wide { 2 } else { 1 };
        match (c.bg, &mut run) {
            (Some(bg), Some((row, _, end, color))) if *row == c.row && *end == c.col && *color == bg => *end += width,
            (Some(bg), _) => {
                flush_rect(&mut run, &mut frame);
                run = Some((c.row, c.col, c.col + width, bg));
            }
            (None, _) => flush_rect(&mut run, &mut frame),
        }
    }
    flush_rect(&mut run, &mut frame);

    // Text: segments of adjacent non-blank cells, each pinned to its first column.
    let font_size = px(m.size);
    let font = base_font(window);
    let mut seg = Segment::default();
    for c in &cells {
        if boxdraw::handles(c.c) {
            seg.flush(&mut frame, window, font_size, cell_w, &at);
            boxdraw::draw(c.c, at(c.row, c.col), cell_w, line_h, c.fg, &mut frame.shapes);
            continue;
        }
        let blank = c.c == ' ' && c.underline.is_none() && !c.strike;
        let breaks = seg.row != c.row || seg.next_col != c.col || blank || c.wide || seg.wide;
        if breaks {
            seg.flush(&mut frame, window, font_size, cell_w, &at);
        }
        if blank {
            continue;
        }
        if seg.text.is_empty() {
            seg.row = c.row;
            seg.col = c.col;
            seg.wide = c.wide;
        }
        seg.push(c, &font);
        seg.next_col = c.col + if c.wide { 2 } else { 1 };
    }
    seg.flush(&mut frame, window, font_size, cell_w, &at);

    // Cursor shapes other than a focused block (which is drawn as an inverted cell).
    if let Some((row, col, shape, wide)) = cursor {
        let o = at(row, col);
        let w = cell_w * if wide { 2. } else { 1. };
        let color = palette.cursor;
        let thin = 2.;
        match (shape, focused) {
            (CursorShape::Block, true) => {}
            (CursorShape::Beam, _) => frame.cursor.push((Bounds::new(o, size(px(thin), px(line_h))), color)),
            (CursorShape::Underline, _) => frame.cursor.push((Bounds::new(o + point(px(0.), px(line_h - thin)), size(px(w), px(thin))), color)),
            _ => {
                // Hollow block: four 1px edges.
                let edge = 1.;
                frame.cursor.push((Bounds::new(o, size(px(w), px(edge))), color));
                frame.cursor.push((Bounds::new(o + point(px(0.), px(line_h - edge)), size(px(w), px(edge))), color));
                frame.cursor.push((Bounds::new(o, size(px(edge), px(line_h))), color));
                frame.cursor.push((Bounds::new(o + point(px(w - edge), px(0.)), size(px(edge), px(line_h))), color));
            }
        }
    }
    frame
}

#[derive(Default)]
struct Segment {
    row: usize,
    col: usize,
    next_col: usize,
    wide: bool,
    text: String,
    runs: Vec<TextRun>,
}

impl Segment {
    fn push(&mut self, c: &CellOut, base: &Font) {
        let mut font = base.clone();
        if c.bold {
            font.weight = FontWeight::BOLD;
        }
        if c.italic {
            font.style = FontStyle::Italic;
        }
        let underline = c.underline.map(|wavy| UnderlineStyle { thickness: px(1.), color: Some(c.fg), wavy });
        let strikethrough = c.strike.then(|| StrikethroughStyle { thickness: px(1.), color: Some(c.fg) });
        let len = c.c.len_utf8();
        self.text.push(c.c);
        if let Some(last) = self.runs.last_mut() {
            if last.font == font && last.color == c.fg && last.underline == underline && last.strikethrough == strikethrough {
                last.len += len;
                return;
            }
        }
        self.runs.push(TextRun { len, font, color: c.fg, background_color: None, underline, strikethrough });
    }

    fn flush(&mut self, frame: &mut Frame, window: &mut Window, font_size: Pixels, cell_w: f32, at: &impl Fn(usize, usize) -> Point<Pixels>) {
        if self.text.is_empty() {
            return;
        }
        let text = SharedString::from(std::mem::take(&mut self.text));
        let runs = std::mem::take(&mut self.runs);
        // Wide characters (CJK, emoji) get their own segment, so the grid stays aligned.
        let geometry_w = if self.wide { None } else { Some(px(cell_w)) };
        let shaped = window.text_system().shape_line(text, font_size, &runs, geometry_w);
        frame.lines.push((at(self.row, self.col), shaped));
        self.wide = false;
    }
}

fn paint(frame: Frame, window: &mut Window, cx: &mut App) {
    window.with_content_mask(Some(ContentMask { bounds: frame.bounds }), |window| {
        for (b, color) in frame.rects {
            window.paint_quad(fill(b, color));
        }
        for shape in frame.shapes {
            match shape {
                Shape::Rect(b, color) => window.paint_quad(fill(b, color)),
                Shape::Path(path, color) => window.paint_path(path, color),
            }
        }
        for (origin, line) in frame.lines {
            let _ = line.paint(origin, frame.line_h, window, cx);
        }
        for (b, color) in frame.cursor {
            window.paint_quad(fill(b, color));
        }
        if let Some(preedit) = frame.preedit {
            // Paint after the grid so its contents cannot show through the
            // marked text. Nothing here is written into alacritty's buffer.
            window.paint_quad(fill(Bounds::new(preedit.origin, size(preedit.line.width + px(2.), preedit.line_h)), preedit.bg));
            if !preedit.selected.is_empty() {
                window.paint_quad(fill(preedit.bounds_for_range(preedit.selected.clone()), preedit.selection));
            }
            let _ = preedit.line.paint(preedit.origin, preedit.line_h, window, cx);
            window.paint_quad(fill(preedit.bounds_for_range(preedit.selected.end..preedit.selected.end), preedit.fg));
        }
    });
}

/// The grid cell under a window position, and which half of it.
pub(super) fn cell_at(g: &Geometry, pos: Point<Pixels>) -> Option<(usize, usize, bool)> {
    if g.cols == 0 || g.rows == 0 {
        return None;
    }
    let x = f32::from(pos.x - g.origin.x);
    let y = f32::from(pos.y - g.origin.y);
    let col = (x / g.cell_w).floor().clamp(0., (g.cols - 1) as f32) as usize;
    let row = (y / g.line_h).floor().clamp(0., (g.rows - 1) as f32) as usize;
    let right_half = x - col as f32 * g.cell_w > g.cell_w / 2.;
    Some((col, row, right_half))
}
