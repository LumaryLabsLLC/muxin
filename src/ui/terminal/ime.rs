//! The terminal's input-method buffer. Preedit stays local; only committed
//! text goes to the program, which owns editing the terminal's actual contents.

use std::ops::Range;

use gpui::{App, Bounds, Context, ElementInputHandler, EntityInputHandler, InputHandler, Pixels, Point, UTF16Selection, Window};

use super::{Metrics, TerminalView, render};

#[derive(Clone, Default)]
pub(super) struct Composition {
    pub text: String,
    /// UTF-8 byte offsets, always on character boundaries.
    pub selected: Range<usize>,
}

impl Composition {
    pub fn clear(&mut self) {
        self.text.clear();
        self.selected = 0..0;
    }

    pub fn range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        byte_range(&self.text, range)
    }

    pub fn range_to_utf16(&self, range: Range<usize>) -> Range<usize> {
        self.text[..range.start].encode_utf16().count()..self.text[..range.end].encode_utf16().count()
    }

    pub fn update(&mut self, range: Option<Range<usize>>, text: &str, selected: Option<Range<usize>>) {
        let range = range.map(|r| self.range_from_utf16(r)).unwrap_or(0..self.text.len());
        // The selected range is relative to the newly inserted text, not the
        // old buffer (whose UTF-16/UTF-8 lengths can be entirely different).
        let selected = selected.map(|r| byte_range(text, r)).unwrap_or(text.len()..text.len());
        self.text.replace_range(range.clone(), text);
        self.selected = range.start + selected.start..range.start + selected.end;
    }
}

/// Clamp platform ranges to valid UTF-8 boundaries, including a range inside
/// a surrogate pair or a stale/out-of-bounds range from an input method.
fn byte_range(text: &str, range: Range<usize>) -> Range<usize> {
    let offset = |target| {
        let mut utf16 = 0;
        for (byte, ch) in text.char_indices() {
            utf16 += ch.len_utf16();
            if utf16 > target {
                return byte;
            }
        }
        text.len()
    };
    let start = offset(range.start);
    start..offset(range.end).max(start)
}

/// Keep terminal key repeat instead of macOS's press-and-hold accent picker.
pub(super) struct TerminalInputHandler(pub ElementInputHandler<TerminalView>);

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(&mut self, ignore_disabled_input: bool, window: &mut Window, cx: &mut App) -> Option<UTF16Selection> {
        self.0.selected_text_range(ignore_disabled_input, window, cx)
    }

    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.0.marked_text_range(window, cx)
    }

    fn text_for_range(&mut self, range: Range<usize>, adjusted_range: &mut Option<Range<usize>>, window: &mut Window, cx: &mut App) -> Option<String> {
        self.0.text_for_range(range, adjusted_range, window, cx)
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, window: &mut Window, cx: &mut App) {
        self.0.replace_text_in_range(range, text, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0.replace_and_mark_text_in_range(range, text, selected, window, cx);
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.0.unmark_text(window, cx);
    }

    fn bounds_for_range(&mut self, range: Range<usize>, window: &mut Window, cx: &mut App) -> Option<Bounds<Pixels>> {
        self.0.bounds_for_range(range, window, cx)
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, window: &mut Window, cx: &mut App) -> Option<usize> {
        self.0.character_index_for_point(point, window, cx)
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.ime.range_from_utf16(range);
        *adjusted_range = Some(self.ime.range_to_utf16(range.clone()));
        Some(self.ime.text[range].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        (!self.exited).then(|| UTF16Selection { range: self.ime.range_to_utf16(self.ime.selected.clone()), reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.ime.text.is_empty()).then(|| 0..self.ime.text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.ime.clear();
        cx.notify();
    }

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        // The range belongs to our local preedit, never to the PTY's screen.
        // Inserting text is ordinary typing, not a bracketed clipboard paste.
        self.ime.clear();
        self.write_input(text.as_bytes().to_vec(), cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.exited {
            self.ime.update(range, text, selected);
            self.prepare_input(cx);
        }
    }

    fn bounds_for_range(&mut self, range: Range<usize>, _: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let layout = render::input_layout(&self.shared, &self.ime, Metrics::current(cx), window, cx);
        Some(layout.bounds_for_range(self.ime.range_from_utf16(range)))
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        let layout = render::input_layout(&self.shared, &self.ime, Metrics::current(cx), window, cx);
        let byte = layout.line.index_for_x(point.x - layout.origin.x)?;
        Some(self.ime.range_to_utf16(byte..byte).start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinyin_preedit_is_replaced_until_commit() {
        let mut ime = Composition::default();
        ime.update(None, "n", Some(1..1));
        ime.update(None, "ni", Some(2..2));
        ime.update(None, "你", Some(1..1));
        assert_eq!(ime.text, "你");
        assert_eq!(ime.selected, 3..3);
        assert_eq!(ime.range_to_utf16(ime.selected.clone()), 1..1);
        ime.clear();
        assert!(ime.text.is_empty());
        assert_eq!(ime.selected, 0..0);
    }

    #[test]
    fn selection_is_relative_to_replacement_even_after_multibyte_text() {
        let mut ime = Composition::default();
        ime.update(None, "你hao", None);
        ime.update(Some(1..4), "好😀", Some(1..3));
        assert_eq!(ime.text, "你好😀");
        assert_eq!(&ime.text[ime.selected.clone()], "😀");
        assert_eq!(ime.range_to_utf16(ime.selected.clone()), 2..4);
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)] // Deliberately exercise an invalid IME range.
    fn input_method_ranges_never_split_utf8_or_panic() {
        let ime = Composition { text: "a😀中".into(), selected: 0..0 };
        assert_eq!(&ime.text[ime.range_from_utf16(1..3)], "😀");
        assert_eq!(&ime.text[ime.range_from_utf16(3..4)], "中");
        assert_eq!(ime.range_from_utf16(2..2), 1..1);
        assert_eq!(ime.range_from_utf16(usize::MAX..usize::MAX), 8..8);
        assert_eq!(ime.range_from_utf16(4..0), 8..8);
    }

    #[test]
    fn deleting_or_cancelling_preedit_leaves_no_stale_selection() {
        let mut ime = Composition::default();
        ime.update(None, "中文", Some(0..2));
        ime.update(None, "", Some(usize::MAX..usize::MAX));
        assert!(ime.text.is_empty());
        assert_eq!(ime.selected, 0..0);
        ime.update(None, "again", None);
        ime.clear();
        assert_eq!(ime.range_to_utf16(ime.selected.clone()), 0..0);
    }
}
