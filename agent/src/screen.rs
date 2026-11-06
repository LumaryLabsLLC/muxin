//! A session's screen, as escape sequences that redraw it in a fresh terminal
//! (sent to a client when it attaches), or as plain text (`read`).

use std::fmt::Write as _;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};

/// Scrollback sent ahead of the screen on attach (main screen only).
const REDRAW_HISTORY: usize = 2000;

const STYLE: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

/// Escape sequences that reproduce the screen, recent scrollback, cursor and
/// the modes programs set (application cursor keys, bracketed paste, mouse).
pub fn redraw<T: alacritty_terminal::event::EventListener>(term: &Term<T>) -> Vec<u8> {
    let mode = *term.mode();
    let alt = mode.contains(TermMode::ALT_SCREEN);
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines() as i32;
    let mut out = String::from("\x1b[0m\x1b[?25l");
    if alt {
        out.push_str("\x1b[?1049h");
    }
    out.push_str("\x1b[H\x1b[2J");
    if !alt {
        out.push_str("\x1b[3J");
    }
    let history = if alt { 0 } else { grid.history_size().min(REDRAW_HISTORY) as i32 };
    let content = term.renderable_content();
    let cursor = content.cursor.point;
    // Down to the last row with something on it (or the cursor), no further:
    // blank rows below would scroll a smaller terminal, pushing the screen
    // off its top. The clear above already blanked them.
    let mut lines: Vec<String> = (-history..rows)
        .map(|line| {
            let mut s = String::new();
            row(&grid[Line(line)], cols, &mut s);
            s
        })
        .collect();
    let keep = (lines.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1)).max((history + cursor.line.0 + 1).max(0) as usize);
    lines.truncate(keep);
    out.push_str(&lines.join("\r\n"));

    let _ = write!(out, "\x1b[{};{}H", cursor.line.0 + content.display_offset as i32 + 1, cursor.column.0 + 1);
    for (flag, on) in [
        (TermMode::APP_CURSOR, "\x1b[?1h"),
        (TermMode::APP_KEYPAD, "\x1b="),
        (TermMode::BRACKETED_PASTE, "\x1b[?2004h"),
        (TermMode::FOCUS_IN_OUT, "\x1b[?1004h"),
        (TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h"),
        (TermMode::MOUSE_DRAG, "\x1b[?1002h"),
        (TermMode::MOUSE_MOTION, "\x1b[?1003h"),
        (TermMode::SGR_MOUSE, "\x1b[?1006h"),
        (TermMode::INSERT, "\x1b[4h"),
    ] {
        if mode.contains(flag) {
            out.push_str(on);
        }
    }
    if !mode.contains(TermMode::LINE_WRAP) {
        out.push_str("\x1b[?7l");
    }
    let style = term.cursor_style();
    let shape = match style.shape {
        CursorShape::Underline => 3,
        CursorShape::Beam => 5,
        _ => 1,
    } + if style.blinking { 0 } else { 1 };
    let _ = write!(out, "\x1b[{shape} q");
    if mode.contains(TermMode::SHOW_CURSOR) {
        out.push_str("\x1b[?25h");
    }
    out.into_bytes()
}

/// Up to `history` lines of scrollback and the screen (to its last line with
/// something on it), as plain lines with their colors: what a session saves
/// (saved.rs) and prints again into a new terminal when it comes back.
pub fn saved<T: alacritty_terminal::event::EventListener>(term: &Term<T>, history: usize) -> Vec<u8> {
    let grid = term.grid();
    let cols = grid.columns();
    let first = -(history.min(grid.history_size()) as i32);
    let mut lines: Vec<String> = (first..grid.screen_lines() as i32)
        .map(|line| {
            let mut s = String::new();
            row(&grid[Line(line)], cols, &mut s);
            s
        })
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut out = lines.join("\r\n");
    out.push_str("\x1b[0m");
    out.into_bytes()
}

/// One grid row with its colors, trailing blanks dropped.
fn row(cells: &alacritty_terminal::grid::Row<Cell>, cols: usize, out: &mut String) {
    let blank = |c: &Cell| c.c == ' ' && c.bg == Color::Named(NamedColor::Background) && !c.flags.intersects(STYLE);
    let end = (0..cols).rev().find(|&i| !blank(&cells[Column(i)])).map(|i| i + 1).unwrap_or(0);
    let mut current: Option<(Color, Color, Flags)> = None;
    for i in 0..end {
        let cell = &cells[Column(i)];
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let style = (cell.fg, cell.bg, cell.flags & STYLE);
        if current != Some(style) {
            sgr(style, out);
            current = Some(style);
        }
        out.push(cell.c);
        if let Some(extra) = cell.zerowidth() {
            out.extend(extra.iter());
        }
    }
    if current.is_some_and(|s| s != (Color::Named(NamedColor::Foreground), Color::Named(NamedColor::Background), Flags::empty())) {
        out.push_str("\x1b[0m");
    }
}

fn sgr((fg, bg, flags): (Color, Color, Flags), out: &mut String) {
    out.push_str("\x1b[0");
    for (flag, code) in [
        (Flags::BOLD, ";1"),
        (Flags::DIM, ";2"),
        (Flags::ITALIC, ";3"),
        (Flags::UNDERLINE, ";4"),
        (Flags::DOUBLE_UNDERLINE, ";21"),
        (Flags::UNDERCURL, ";4:3"),
        (Flags::DOTTED_UNDERLINE, ";4:4"),
        (Flags::DASHED_UNDERLINE, ";4:5"),
        (Flags::INVERSE, ";7"),
        (Flags::HIDDEN, ";8"),
        (Flags::STRIKEOUT, ";9"),
    ] {
        if flags.contains(flag) {
            out.push_str(code);
        }
    }
    color(fg, true, out);
    color(bg, false, out);
    out.push('m');
}

fn color(c: Color, fg: bool, out: &mut String) {
    let (base, bright, ext) = if fg { (30, 90, 38) } else { (40, 100, 48) };
    match c {
        Color::Named(n) => {
            let i = n as usize;
            if i < 8 {
                let _ = write!(out, ";{}", base + i);
            } else if i < 16 {
                let _ = write!(out, ";{}", bright + i - 8);
            } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) {
                let _ = write!(out, ";{}", base + i - NamedColor::DimBlack as usize);
            }
            // Foreground/Background/Cursor: the terminal's defaults.
        }
        Color::Indexed(i) => {
            let _ = write!(out, ";{ext};5;{i}");
        }
        Color::Spec(rgb) => {
            let _ = write!(out, ";{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}

/// The screen and up to `history` lines of scrollback as plain text.
pub fn text<T>(term: &Term<T>, history: usize) -> String {
    let grid = term.grid();
    let cols = grid.columns();
    let first = -(history.min(grid.history_size()) as i32);
    let mut lines: Vec<String> = (first..grid.screen_lines() as i32)
        .map(|l| {
            let row = &grid[Line(l)];
            let mut s = String::with_capacity(cols);
            for i in 0..cols {
                let cell = &row[Column(i)];
                if !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    s.push(cell.c);
                }
            }
            s.trim_end().to_string()
        })
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    lines.join("\n")
}

/// What the screen says, as a number: equal when only animation changed.
/// Braille dots count as blanks (spinners, and the starry backdrop some
/// Codex models draw on empty cells while they wait) and each run of blanks
/// as one, so dots moving between words change nothing. Lines scrolling into
/// the history change it, so a program that keeps printing the same lines
/// still counts as printing.
pub fn signature<T>(term: &Term<T>) -> u64 {
    use std::hash::{Hash, Hasher};
    let grid = term.grid();
    let cols = grid.columns();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    grid.history_size().hash(&mut h);
    for l in 0..grid.screen_lines() as i32 {
        let row = &grid[Line(l)];
        let mut gap = false;
        for i in 0..cols {
            let cell = &row[Column(i)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let c = cell.c;
            if c.is_whitespace() || ('\u{2800}'..='\u{28ff}').contains(&c) {
                gap = true;
                continue;
            }
            if gap {
                ' '.hash(&mut h);
                gap = false;
            }
            c.hash(&mut h);
        }
        '\n'.hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    struct Size(usize, usize);
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    fn term(bytes: &[u8]) -> Term<VoidListener> {
        let mut t = Term::new(Config::default(), &Size(40, 6), VoidListener);
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut t, bytes);
        t
    }

    #[test]
    fn a_redraw_reproduces_the_screen() {
        let original = term(b"plain line\r\n\x1b[1;31mbold red\x1b[0m and \x1b[38;2;10;20;30mrgb\x1b[0m\r\n\x1b[?2004h> prompt");
        let copy = term(&redraw(&original));
        assert_eq!(text(&copy, 100), text(&original, 100));
        assert!(copy.mode().contains(TermMode::BRACKETED_PASTE));
        let (a, b) = (original.grid().cursor.point, copy.grid().cursor.point);
        assert_eq!((a.line, a.column), (b.line, b.column));
        let red = &copy.grid()[Line(1)][Column(0)];
        assert!(red.flags.contains(Flags::BOLD));
        assert_eq!(red.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn a_smaller_terminal_keeps_the_screen_in_view() {
        // A session's screen with 36 rows, mostly empty, shown in a terminal
        // with fewer (the window is smaller than the session was).
        let feed = |t: &mut Term<VoidListener>, b: &[u8]| Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(t, b);
        let mut session = Term::new(Config::default(), &Size(120, 36), VoidListener);
        feed(&mut session, b"~\r\n> echo hi\r\nhi\r\n~\r\n> ");
        let mut client = Term::new(Config::default(), &Size(118, 31), VoidListener);
        feed(&mut client, &redraw(&session));
        let row = |t: &Term<VoidListener>, l: i32| (0..10).map(|c| t.grid()[Line(l)][Column(c)].c).collect::<String>().trim_end().to_string();
        assert_eq!((row(&client, 0), row(&client, 2), row(&client, 4)), ("~".into(), "hi".into(), ">".into()));
        let (a, b) = (session.grid().cursor.point, client.grid().cursor.point);
        assert_eq!((a.line, a.column), (b.line, b.column));
    }

    #[test]
    fn a_saved_screen_prints_back_with_its_scrollback_and_colors() {
        let lines: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
        let original = term(format!("{lines}\x1b[1;32mgreen\x1b[0m end\r\n\r\n").as_bytes());
        let saved = saved(&original, 1000);
        let copy = term(&saved);
        assert_eq!(text(&copy, 1000), text(&original, 1000));
        let green = &copy.grid()[Line(copy.grid().cursor.point.line.0)][Column(0)];
        assert!(green.flags.contains(Flags::BOLD) && green.fg == Color::Named(NamedColor::Green));
        // Only as much scrollback as asked for, and no blank rows at the end.
        assert!(!text(&term(&self::saved(&original, 5)), 1000).contains("line 10"));
        assert!(saved.ends_with(b"end\x1b[0m"));
    }

    #[test]
    fn scrollback_comes_along() {
        let lines: String = (0..20).map(|i| format!("line {i}\r\n")).collect();
        let original = term(lines.as_bytes());
        let copy = term(&redraw(&original));
        assert!(text(&copy, 100).contains("line 3"));
    }

    #[test]
    fn moving_dots_dont_change_the_signature() {
        let mut term = Term::new(Config::default(), &Size(40, 5), VoidListener);
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        let mut feed = |term: &mut Term<VoidListener>, s: &str| parser.advance(term, s.as_bytes());
        feed(&mut term, "\u{2801}   \u{2802}\r\n> Ask Codex\u{2804} to do anything");
        let idle = signature(&term);
        // The backdrop's dots move (cursor home, new dots, the prompt redrawn).
        feed(&mut term, "\x1b[H\u{2808}  \u{2810} \u{2820}\r\n> Ask Codex  \u{2840}to do anything");
        assert_eq!(signature(&term), idle);
        // New words do change it.
        feed(&mut term, "\x1b[H\r\n> Ask Codex to do anything\r\n\u{2022} Working (3s)");
        assert_ne!(signature(&term), idle);
        let working = signature(&term);
        // So do the same lines scrolling into the history.
        for _ in 0..6 {
            feed(&mut term, "\r\ny");
        }
        let scrolled = signature(&term);
        feed(&mut term, "\r\ny");
        assert_ne!(signature(&term), scrolled);
        assert_ne!(scrolled, working);
    }
}
