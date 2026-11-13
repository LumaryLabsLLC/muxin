//! The rows of a text field that holds several lines: where each one starts
//! and ends in the text, and where the cursor goes when it moves up or down.
//! Kept apart from the field (input.rs) and from how text is measured, so it
//! can be tested without a window.
//!
//! A *typed line* is what's between two new lines the user typed. A *row* is
//! a line as it's drawn: a typed line too long for the field wraps into
//! several rows.

use std::ops::Range;

/// One row as drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The typed line it's part of.
    pub line: usize,
    /// Its bytes in the whole text. A row that wrapped ends where the next
    /// one starts; a typed line's last row ends before its new line.
    pub range: Range<usize>,
}

/// Each typed line's bytes, without its new line. Never empty: no text is
/// one empty line.
pub fn typed_lines(text: &str) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (at, _) in text.match_indices('\n') {
        lines.push(start..at);
        start = at + 1;
    }
    lines.push(start..text.len());
    lines
}

/// The rows of `text`. `wraps[n]` says where typed line `n` wraps: the bytes
/// (counted from that line's start) at which a new row begins.
pub fn rows(text: &str, wraps: &[Vec<usize>]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (line, typed) in typed_lines(text).into_iter().enumerate() {
        let mut start = typed.start;
        for wrap in wraps.get(line).map(Vec::as_slice).unwrap_or_default() {
            let at = typed.start + wrap;
            // Only a wrap that's inside the line and moves forward makes a row.
            if at > start && at < typed.end {
                rows.push(Row { line, range: start..at });
                start = at;
            }
        }
        rows.push(Row { line, range: start..typed.end });
    }
    rows
}

/// The row the cursor at `offset` is on. Where a line wraps, that's the end
/// of the upper row.
pub fn row_of(rows: &[Row], offset: usize) -> usize {
    rows.iter().position(|row| offset >= row.range.start && offset <= row.range.end).unwrap_or(rows.len().saturating_sub(1))
}

/// Where the typed line around `offset` starts.
pub fn line_start(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    text[..offset].rfind('\n').map(|at| at + 1).unwrap_or(0)
}

/// Where the typed line around `offset` ends (before its new line).
pub fn line_end(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    text[offset..].find('\n').map(|at| offset + at).unwrap_or(text.len())
}

/// Where the cursor goes from `offset` when it moves `by` rows (-1: up,
/// 1: down): the place on that row nearest to where it is across. With no
/// such row, the start or the end of the text, as text fields do.
///
/// `across(offset)` is how far across its row a place in the text is drawn;
/// `nearest(row, across)` is the place in a row nearest to that.
pub fn vertical(rows: &[Row], offset: usize, by: isize, len: usize, across: impl Fn(usize) -> f32, nearest: impl Fn(usize, f32) -> usize) -> usize {
    if rows.is_empty() {
        return offset.min(len);
    }
    let from = row_of(rows, offset) as isize;
    let to = from + by;
    if to < 0 {
        return 0;
    }
    if to >= rows.len() as isize {
        return len;
    }
    let row = &rows[to as usize];
    nearest(to as usize, across(offset)).clamp(row.range.start, row.range.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A field ten characters wide, in a font where every character is as
    /// wide as the next: the wraps, and how to measure in it.
    fn wrapped(text: &str, width: usize) -> Vec<Row> {
        let wraps: Vec<Vec<usize>> = typed_lines(text).into_iter().map(|line| (1..).map(|n| n * width).take_while(|at| *at < line.len()).collect()).collect();
        rows(text, &wraps)
    }

    fn up_down(text: &str, width: usize, offset: usize, by: isize) -> usize {
        let rows = wrapped(text, width);
        let across = |offset: usize| (offset - rows[row_of(&rows, offset)].range.start) as f32;
        let nearest = |row: usize, x: f32| rows[row].range.start + x.round() as usize;
        vertical(&rows, offset, by, text.len(), across, nearest)
    }

    #[test]
    fn typed_lines_are_whats_between_new_lines() {
        assert_eq!(typed_lines(""), [0..0]);
        assert_eq!(typed_lines("one"), [0..3]);
        assert_eq!(typed_lines("one\ntwo"), [0..3, 4..7]);
        // A new line at the end starts an empty line: that's where the cursor is.
        assert_eq!(typed_lines("one\n"), [0..3, 4..4]);
        assert_eq!(typed_lines("\n\n"), [0..0, 1..1, 2..2]);
    }

    #[test]
    fn rows_are_typed_lines_cut_where_they_wrap() {
        let text = "a line that is long\nshort";
        let rows = wrapped(text, 10);
        let ranges: Vec<Range<usize>> = rows.iter().map(|r| r.range.clone()).collect();
        assert_eq!(ranges, [0..10, 10..19, 20..25]);
        assert_eq!(rows.iter().map(|r| r.line).collect::<Vec<_>>(), [0, 0, 1]);
        // A wrap outside its line, or one that goes nowhere, makes no row.
        assert_eq!(super::rows("abc", &[vec![0, 3, 9]]), [Row { line: 0, range: 0..3 }]);
        assert_eq!(super::rows("", &[]), [Row { line: 0, range: 0..0 }]);
    }

    #[test]
    fn the_cursors_row() {
        let rows = wrapped("a line that is long\nshort", 10);
        assert_eq!(row_of(&rows, 0), 0);
        assert_eq!(row_of(&rows, 9), 0);
        // Where it wraps: the end of the upper row.
        assert_eq!(row_of(&rows, 10), 0);
        assert_eq!(row_of(&rows, 11), 1);
        // Before the new line is the first line's last row; after it, the next line.
        assert_eq!(row_of(&rows, 19), 1);
        assert_eq!(row_of(&rows, 20), 2);
        assert_eq!(row_of(&rows, 25), 2);
        // Past the end (the text changed since it was drawn): the last row.
        assert_eq!(row_of(&rows, 99), 2);
    }

    #[test]
    fn a_typed_lines_ends() {
        let text = "one\ntwo three\n";
        assert_eq!((line_start(text, 0), line_end(text, 0)), (0, 3));
        assert_eq!((line_start(text, 3), line_end(text, 3)), (0, 3));
        assert_eq!((line_start(text, 8), line_end(text, 8)), (4, 13));
        assert_eq!((line_start(text, 14), line_end(text, 14)), (14, 14));
        assert_eq!((line_start(text, 99), line_end(text, 99)), (14, 14));
        assert_eq!((line_start("", 0), line_end("", 0)), (0, 0));
    }

    #[test]
    fn up_and_down_keep_the_place_across() {
        let text = "a line that is long\nshort\n\nthe last line";
        // From "short" (row 2), three across: up to the wrapped row above, down to the empty line.
        assert_eq!(up_down(text, 10, 23, -1), 13);
        assert_eq!(up_down(text, 10, 23, 1), 26);
        // A shorter row takes the cursor to its end, not past it.
        assert_eq!(up_down(text, 10, 35, -1), 26);
        assert_eq!(up_down(text, 10, 8, 2), 25);
        // Within one typed line that wrapped.
        assert_eq!(up_down(text, 10, 4, 1), 14);
        assert_eq!(up_down(text, 10, 14, -1), 4);
    }

    #[test]
    fn past_the_first_or_last_row_goes_to_the_texts_start_or_end() {
        let text = "one\ntwo";
        assert_eq!(up_down(text, 10, 2, -1), 0);
        assert_eq!(up_down(text, 10, 5, 1), 7);
        assert_eq!(up_down("", 10, 0, -1), 0);
        assert_eq!(up_down("", 10, 0, 1), 0);
        assert_eq!(vertical(&[], 3, 1, 2, |_| 0., |_, _| 0), 2);
    }
}
