//! A single-line text input, adapted from GPUI's input example (GPUI has no
//! built-in text field). Emits `InputEvent`s for changes, Enter and Escape.

use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, TextRun, UTF16Selection, Window, actions, anchored, deferred, div, fill, point, prelude::*, px,
    relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use super::theme::{ActiveTheme, mono_font, ui_font};

actions!(
    text_input,
    [Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, Paste, Cut, Copy, Enter, Escape]
);

const CONTEXT: &str = "TextInput";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("secondary-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("secondary-v", Paste, Some(CONTEXT)),
        KeyBinding::new("secondary-c", Copy, Some(CONTEXT)),
        KeyBinding::new("secondary-x", Cut, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        KeyBinding::new("secondary-left", Home, Some(CONTEXT)),
        KeyBinding::new("secondary-right", End, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        KeyBinding::new("escape", Escape, Some(CONTEXT)),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Changed,
    Submit,
    Cancel,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// How far the text is scrolled left, to keep the cursor in view when
    /// it's longer than the field.
    scroll_x: Pixels,
    is_selecting: bool,
    mono: bool,
    /// Shows "•" for each character and won't copy (tokens, keys).
    secret: bool,
    /// No frame of its own (it sits in one, like the palette's).
    bare: bool,
    /// Its right-click menu (Cut, Copy, Paste, Select All), where it opened.
    menu: Option<Point<Pixels>>,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        TextInput {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_x: px(0.),
            is_selecting: false,
            mono: false,
            secret: false,
            bare: false,
            menu: None,
        }
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    pub fn bare(mut self) -> Self {
        self.bare = true;
        self
    }

    /// Where a text offset is on screen: the same, unless each character is
    /// shown as one "•" (3 bytes).
    fn to_display(&self, offset: usize) -> usize {
        if self.secret { self.content[..offset.min(self.content.len())].chars().count() * '•'.len_utf8() } else { offset }
    }

    fn from_display(&self, index: usize) -> usize {
        if !self.secret {
            return index;
        }
        let n = index / '•'.len_utf8();
        self.content.char_indices().nth(n).map(|(i, _)| i).unwrap_or(self.content.len())
    }

    pub fn with_text(mut self, text: impl Into<SharedString>) -> Self {
        self.content = text.into();
        self.selected_range = self.content.len()..self.content.len();
        self
    }

    /// Starts with all of its text selected, so typing replaces it.
    pub fn selected(mut self) -> Self {
        self.selected_range = 0..self.content.len();
        self
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.selected_range = self.content.len()..self.content.len();
        self.marked_range = None;
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Submit);
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Cancel);
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    /// Right-click: the edit menu, with the cursor where it was clicked
    /// (a selection stays, to cut or copy).
    fn on_right_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        if self.selected_range.is_empty() {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }
        self.menu = Some(event.position);
        cx.notify();
    }

    /// The edit menu: each item runs its action, then it closes.
    fn edit_menu(&self, at: Point<Pixels>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let selected = !self.selected_range.is_empty() && !self.secret;
        let has_text = !self.content.is_empty();
        let key = |k: &str| if cfg!(target_os = "macos") { format!("⌘{k}") } else { format!("Ctrl+{k}") };
        let items: [(&str, String, bool, fn(&mut Self, &mut Window, &mut Context<Self>)); 4] = [
            ("Cut", key("X"), selected, |this, window, cx| this.cut(&Cut, window, cx)),
            ("Copy", key("C"), selected, |this, window, cx| this.copy(&Copy, window, cx)),
            ("Paste", key("V"), true, |this, window, cx| this.paste(&Paste, window, cx)),
            ("Select All", key("A"), has_text, |this, window, cx| this.select_all(&SelectAll, window, cx)),
        ];
        let rows = items.into_iter().enumerate().map(|(i, (label, keys, on, run))| {
            div()
                .id(("input-menu", i))
                .flex()
                .items_center()
                .h(px(28.))
                .mx(px(4.))
                .px(px(10.))
                .rounded(px(6.))
                .text_size(px(13.))
                .text_color(theme.text)
                .when(!on, |d| d.opacity(0.4))
                .when(on, |d| {
                    d.cursor_pointer().hover(|st| st.bg(theme.element_hover)).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.menu = None;
                            run(this, window, cx);
                            cx.notify();
                        }),
                    )
                })
                .child(div().flex_1().child(label))
                .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child(keys))
        });
        deferred(
            anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                div()
                    .id("input-menu")
                    .occlude()
                    .w(px(190.))
                    .py(px(4.))
                    .rounded(px(9.))
                    .bg(theme.elevated)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .font_family(ui_font())
                    .children(rows)
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    })),
            ),
        )
        .with_priority(2)
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace(['\n', '\r'], " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.secret {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.secret {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed { self.selected_range.start } else { self.selected_range.end }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        self.from_display(line.closest_index_for_x(position.x - bounds.left() + self.scroll_x))
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        utf8_to_utf16(&self.content, offset)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        clamp(&self.content, utf16_to_utf8(&self.content, range_utf16.start)..utf16_to_utf8(&self.content, range_utf16.end))
    }

    /// Runs an input method call (`EntityInputHandler`). On a Mac these come
    /// from AppKit, through Objective-C, where a panic can't unwind and aborts
    /// the app (#1). If one slips through anyway, the call is dropped and the
    /// text stays usable.
    fn guard<R>(&mut self, fallback: R, f: impl FnOnce(&mut Self) -> R) -> R {
        guard(self, fallback, f, |this| recover(&this.content, &mut this.marked_range, &mut this.selected_range))
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content.grapheme_indices(true).rev().find_map(|(idx, _)| (idx < offset).then_some(idx)).unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        self.guard(None, |this| {
            let range = this.range_from_utf16(&range_utf16);
            actual_range.replace(this.range_to_utf16(&range));
            Some(this.content[range].to_string())
        })
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to_utf16(&self.selected_range), reversed: self.selection_reversed })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, new_text: &str, _: &mut Window, cx: &mut Context<Self>) {
        self.guard((), |this| {
            let range = range_utf16
                .as_ref()
                .map(|r| this.range_from_utf16(r))
                .or(this.marked_range.clone())
                .unwrap_or(this.selected_range.clone());
            let (content, _, selected) = splice(&this.content, range, new_text, None);
            this.content = content.into();
            this.selected_range = selected;
            this.marked_range.take();
        });
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.guard((), |this| {
            let range = range_utf16
                .as_ref()
                .map(|r| this.range_from_utf16(r))
                .or(this.marked_range.clone())
                .unwrap_or(this.selected_range.clone());
            let (content, inserted, selected) = splice(&this.content, range, new_text, new_selected_range_utf16);
            this.content = content.into();
            this.marked_range = (!new_text.is_empty()).then_some(inserted);
            this.selected_range = selected;
        });
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(&mut self, range_utf16: Range<usize>, bounds: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        self.guard(None, |this| {
            let last_layout = this.last_layout.as_ref()?;
            let range = this.range_from_utf16(&range_utf16);
            Some(Bounds::from_corners(
                point(bounds.left() + last_layout.x_for_index(this.to_display(range.start)) - this.scroll_x, bounds.top()),
                point(bounds.left() + last_layout.x_for_index(this.to_display(range.end)) - this.scroll_x, bounds.bottom()),
            ))
        })
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        self.guard(None, |this| {
            let line_point = this.last_bounds?.localize(&point)?;
            let last_layout = this.last_layout.as_ref()?;
            let utf8_index = this.from_display(last_layout.index_for_x(point.x - line_point.x + this.scroll_x)?);
            Some(this.offset_to_utf16(utf8_index))
        })
    }
}

/// Runs `f` on `target`, catching a panic: then `recover` puts it right and
/// `fallback` is returned.
fn guard<T, R>(target: &mut T, fallback: R, f: impl FnOnce(&mut T) -> R, recover: impl FnOnce(&mut T)) -> R {
    match catch_unwind(AssertUnwindSafe(|| f(&mut *target))) {
        Ok(result) => result,
        Err(_) => {
            recover(target);
            fallback
        }
    }
}

/// After a caught panic: nothing marked, and the cursor at the end.
fn recover(content: &str, marked: &mut Option<Range<usize>>, selected: &mut Range<usize>) {
    *marked = None;
    *selected = content.len()..content.len();
}

/// A UTF-16 offset (what input methods count in) as a byte offset into
/// `text`, never past its end or inside a character.
fn utf16_to_utf8(text: &str, offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in text.chars() {
        if utf16_count >= offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

fn utf8_to_utf16(text: &str, offset: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;
    for ch in text.chars() {
        if utf8_count >= offset {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }
    utf16_offset
}

/// A byte range that's safe to slice `text` with: in order, inside it, and
/// on character boundaries.
fn clamp(text: &str, range: Range<usize>) -> Range<usize> {
    let floor = |i: usize| (0..=i.min(text.len())).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let (start, end) = (floor(range.start), floor(range.end));
    start.min(end)..start.max(end)
}

/// Replaces `range` of `content` with `new_text`. Returns the new content,
/// where `new_text` went, and what to select: `selected_utf16` counted from
/// the start of `new_text` (how an input method places the cursor in text it's
/// composing), else the end of `new_text`.
fn splice(content: &str, range: Range<usize>, new_text: &str, selected_utf16: Option<Range<usize>>) -> (String, Range<usize>, Range<usize>) {
    let range = clamp(content, range);
    let inserted = range.start..range.start + new_text.len();
    let selected = match selected_utf16 {
        Some(r) => {
            let r = clamp(new_text, utf16_to_utf8(new_text, r.start)..utf16_to_utf8(new_text, r.end));
            inserted.start + r.start..inserted.start + r.end
        }
        None => inserted.end..inserted.end,
    };
    (format!("{}{new_text}{}", &content[..range.start], &content[range.end..]), inserted, selected)
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    scroll: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = cx.theme();
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();

        let (display_text, text_color) = match (content.is_empty(), input.secret) {
            (true, _) => (input.placeholder.clone(), theme.text_placeholder),
            (false, true) => (SharedString::from("•".repeat(content.chars().count())), style.color),
            (false, false) => (content, style.color),
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match input.marked_range.as_ref() {
            Some(marked) if !input.content.is_empty() && !input.secret => vec![
                TextRun { len: marked.start, ..run.clone() },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(gpui::UnderlineStyle { color: Some(run.color), thickness: px(1.0), wavy: false }),
                    ..run.clone()
                },
                TextRun { len: display_text.len() - marked.end, ..run },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect(),
            _ => vec![run],
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window.text_system().shape_line(display_text, font_size, &runs, None);

        let cursor_pos = if input.content.is_empty() { px(0.) } else { line.x_for_index(input.to_display(cursor)) };
        // Longer than the field: scrolled just enough to keep the cursor in view.
        let room = (bounds.size.width - px(2.)).max(px(0.));
        let mut scroll = input.scroll_x;
        if input.content.is_empty() {
            scroll = px(0.);
        } else {
            if cursor_pos - scroll > room {
                scroll = cursor_pos - room;
            }
            if cursor_pos < scroll {
                scroll = cursor_pos;
            }
            scroll = scroll.min((line.width - room).max(px(0.)));
        }
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(point(bounds.left() + cursor_pos - scroll, bounds.top() + px(2.)), size(px(1.5), bounds.size.height - px(4.))),
                    theme.accent,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(input.to_display(selected_range.start)) - scroll, bounds.top()),
                        point(bounds.left() + line.x_for_index(input.to_display(selected_range.end)) - scroll, bounds.bottom()),
                    ),
                    theme.selection,
                )),
                None,
            )
        };
        PrepaintState { line: Some(line), cursor, selection, scroll }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);
        let scroll = prepaint.scroll;
        let Some(line) = prepaint.line.take() else { return };
        // Text scrolled out of the field isn't drawn over its frame.
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection)
            }
            let _ = line.paint(point(bounds.origin.x - scroll, bounds.origin.y), window.line_height(), window, cx);
        });
        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll_x = scroll;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let focused = self.focus_handle.is_focused(window);
        div()
            .flex()
            .items_center()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::escape))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .min_w(px(0.))
            .h(px(28.))
            .px(px(8.))
            .rounded(px(6.))
            .when(!self.bare, |d| d.border_1().border_color(if focused { theme.border_focused } else { theme.border_variant }).bg(theme.canvas))
            .when(self.bare, |d| d.px(px(0.)))
            .text_color(theme.text)
            .font_family(if self.mono { mono_font() } else { ui_font() })
            .text_size(px(if self.bare { 14. } else { 13. }))
            .line_height(px(18.))
            .overflow_hidden()
            .child(TextElement { input: cx.entity() })
            .children(self.menu.map(|at| self.edit_menu(at, cx)))
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The field's text and ranges, changed the way the input method calls
    /// above change them.
    struct Field {
        content: String,
        selected: Range<usize>,
        marked: Option<Range<usize>>,
    }

    impl Field {
        fn new(content: &str) -> Field {
            Field { content: content.into(), selected: content.len()..content.len(), marked: None }
        }

        fn target(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
            let range = range_utf16.map(|r| clamp(&self.content, utf16_to_utf8(&self.content, r.start)..utf16_to_utf8(&self.content, r.end)));
            range.or(self.marked.clone()).unwrap_or(self.selected.clone())
        }

        /// AppKit's setMarkedText.
        fn mark(&mut self, text: &str, selected_utf16: Option<Range<usize>>, range_utf16: Option<Range<usize>>) {
            let (content, inserted, selected) = splice(&self.content, self.target(range_utf16), text, selected_utf16);
            self.content = content;
            self.marked = (!text.is_empty()).then_some(inserted);
            self.selected = selected;
            self.check();
        }

        /// AppKit's insertText.
        fn insert(&mut self, text: &str, range_utf16: Option<Range<usize>>) {
            let (content, _, selected) = splice(&self.content, self.target(range_utf16), text, None);
            self.content = content;
            self.marked = None;
            self.selected = selected;
            self.check();
        }

        /// Every range can be used to slice the text.
        fn check(&self) {
            for r in [Some(self.selected.clone()), self.marked.clone()].into_iter().flatten() {
                assert!(self.content.get(r.clone()).is_some(), "{r:?} doesn't fit {:?}", self.content);
            }
        }
    }

    #[test]
    fn composing_chinese_then_switching_input_mode() {
        // Typing pinyin, then switching the input method's mode, which clears
        // the marked text and then types into the selection (#1).
        let mut f = Field::new("");
        f.mark("n", Some(1..1), None);
        f.mark("ni", Some(2..2), None);
        f.mark("", Some(0..0), None);
        assert_eq!((f.content.as_str(), f.selected.clone(), f.marked.clone()), ("", 0..0, None));
        f.insert("ni", None);
        assert_eq!((f.content.as_str(), f.selected.clone()), ("ni", 2..2));
    }

    #[test]
    fn composing_after_chinese_text() {
        let mut f = Field::new("你好");
        f.mark("shi", Some(3..3), None);
        assert_eq!((f.content.as_str(), f.marked.clone(), f.selected.clone()), ("你好shi", Some(6..9), 9..9));
        f.mark("是", Some(1..1), None);
        assert_eq!((f.content.as_str(), f.marked.clone(), f.selected.clone()), ("你好是", Some(6..9), 9..9));
        f.mark("是的", Some(1..1), None);
        assert_eq!(f.selected, 9..9);
        f.insert("是的", None);
        assert_eq!((f.content.as_str(), f.marked.clone(), f.selected.clone()), ("你好是的", None, 12..12));
    }

    #[test]
    fn a_panic_in_an_input_method_call_is_caught_and_editing_goes_on() {
        let mut f = Field::new("你好");
        f.mark("ni", Some(2..2), None);
        assert_eq!(f.marked, Some(6..8));
        let got = guard(
            &mut f,
            "fallback",
            |f| {
                f.selected = 4..40;
                panic!("an input method edge case");
            },
            |f| recover(&f.content, &mut f.marked, &mut f.selected),
        );
        assert_eq!(got, "fallback");
        assert_eq!((f.content.as_str(), f.marked.clone(), f.selected.clone()), ("你好ni", None, 8..8));
        f.check();
        f.insert("是", None);
        assert_eq!((f.content.as_str(), f.selected.clone()), ("你好ni是", 11..11));
        f.mark("a", Some(1..1), None);
        assert_eq!((f.content.as_str(), f.marked.clone()), ("你好ni是a", Some(11..12)));
    }

    #[test]
    fn emoji_count_as_two_utf16_units() {
        assert_eq!(utf16_to_utf8("a😀b", 1), 1);
        assert_eq!(utf16_to_utf8("a😀b", 3), 5);
        // Halfway through the emoji's pair: not inside its bytes.
        assert_eq!(utf16_to_utf8("a😀b", 2), 5);
        assert_eq!(utf8_to_utf16("a😀b", 5), 3);
        let mut f = Field::new("a😀b");
        f.insert("x", Some(1..3));
        assert_eq!(f.content, "axb");
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn ranges_past_the_end_or_inside_a_character_are_clamped() {
        assert_eq!(clamp("你好", 4..100), 3..6);
        assert_eq!(clamp("abc", 2..1), 1..2);
        assert_eq!(clamp("", 3..5), 0..0);
        assert_eq!(splice("ab", 5..9, "你", Some(5..9)), ("ab你".to_string(), 2..5, 5..5));
        let mut f = Field::new("你好");
        f.selected = 4..40;
        f.insert("x", None);
        assert_eq!(f.content, "你x");
        let mut f = Field::new("你好");
        f.mark("ni", Some(9..1), Some(50..60));
        assert_eq!((f.content.as_str(), f.marked.clone(), f.selected.clone()), ("你好ni", Some(6..8), 7..8));
    }
}
