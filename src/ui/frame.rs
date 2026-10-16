//! A window's frame when the app draws it: Linux on Wayland, where we ask
//! for it (GNOME draws no title bars for apps). Then our own top bar is the
//! title bar, with minimize, maximize and close at its end and the system's
//! window menu on right-click, and the window resizes from thin edges along
//! its sides. Where the system draws the frame (macOS, Windows, X11) none of
//! this shows.

use gpui::{AnyElement, App, CursorStyle, Decorations, ElementId, MouseButton, MouseDownEvent, ResizeEdge, Tiling, Window, WindowDecorations, div, prelude::*, px};

use super::kit::Button;

/// How far in from a side the window resizes, and how big a corner is.
const EDGE: f32 = 5.;
const CORNER: f32 = 12.;

/// What to ask for when opening a window: on Wayland, our own frame (GNOME
/// has no other; GPUI can't tell when the compositor gives none), the way Zed
/// does; elsewhere the system's.
pub fn requested() -> Option<WindowDecorations> {
    let wayland = cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
    wayland.then_some(WindowDecorations::Client)
}

/// The app draws the frame.
pub fn ours(window: &Window) -> bool {
    cfg!(target_os = "linux") && matches!(window.window_decorations(), Decorations::Client { .. })
}

/// Draw a border round the window: our frame, not filling the screen.
pub fn bordered(window: &Window) -> bool {
    ours(window) && !window.is_maximized() && !window.is_fullscreen()
}

/// Minimize, maximize (or restore) and close, for the end of the top bar.
pub fn window_buttons(window: &Window, _cx: &App) -> Option<AnyElement> {
    if !ours(window) {
        return None;
    }
    let controls = window.window_controls();
    let maximized = window.is_maximized();
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .ml(px(4.))
            .when(controls.minimize, |d| {
                d.child(Button::icon_only("window-minimize", "minus").tooltip("Minimize").on_click(|_, window, _| window.minimize_window()))
            })
            .when(controls.maximize, |d| {
                d.child(
                    Button::icon_only("window-maximize", if maximized { "copy" } else { "square" })
                        .tooltip(if maximized { "Restore" } else { "Maximize" })
                        .on_click(|_, window, _| window.zoom_window()),
                )
            })
            .child(Button::icon_only("window-close", "x").tooltip("Close").on_click(|_, window, _| window.remove_window()))
            .into_any_element(),
    )
}

/// A right-click on the top bar: the system's window menu, when it has one.
pub fn title_bar_menu(e: &MouseDownEvent, window: &mut Window) {
    if ours(window) && window.window_controls().window_menu {
        window.show_window_menu(e.position);
    }
}

/// The edges and corners to resize from, laid over the window (its root must
/// be `relative`): none when it fills the screen, or along a side it's tiled
/// against.
pub fn resize_edges(window: &Window) -> Vec<AnyElement> {
    let Decorations::Client { tiling } = window.window_decorations() else { return Vec::new() };
    if !cfg!(target_os = "linux") || window.is_maximized() || window.is_fullscreen() {
        return Vec::new();
    }
    let Tiling { top, left, right, bottom } = tiling;
    let grip = |id: &'static str, edge: ResizeEdge, cursor: CursorStyle| {
        div().id(ElementId::Name(id.into())).absolute().occlude().cursor(cursor).on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            window.start_window_resize(edge);
        })
    };
    let mut out = Vec::new();
    if !top {
        out.push(grip("resize-top", ResizeEdge::Top, CursorStyle::ResizeUpDown).top_0().left(px(CORNER)).right(px(CORNER)).h(px(EDGE)).into_any_element());
    }
    if !bottom {
        out.push(grip("resize-bottom", ResizeEdge::Bottom, CursorStyle::ResizeUpDown).bottom_0().left(px(CORNER)).right(px(CORNER)).h(px(EDGE)).into_any_element());
    }
    if !left {
        out.push(grip("resize-left", ResizeEdge::Left, CursorStyle::ResizeLeftRight).left_0().top(px(CORNER)).bottom(px(CORNER)).w(px(EDGE)).into_any_element());
    }
    if !right {
        out.push(grip("resize-right", ResizeEdge::Right, CursorStyle::ResizeLeftRight).right_0().top(px(CORNER)).bottom(px(CORNER)).w(px(EDGE)).into_any_element());
    }
    let corner = |id, edge, cursor| grip(id, edge, cursor).size(px(CORNER));
    if !top && !left {
        out.push(corner("resize-top-left", ResizeEdge::TopLeft, CursorStyle::ResizeUpLeftDownRight).top_0().left_0().into_any_element());
    }
    if !top && !right {
        out.push(corner("resize-top-right", ResizeEdge::TopRight, CursorStyle::ResizeUpRightDownLeft).top_0().right_0().into_any_element());
    }
    if !bottom && !left {
        out.push(corner("resize-bottom-left", ResizeEdge::BottomLeft, CursorStyle::ResizeUpRightDownLeft).bottom_0().left_0().into_any_element());
    }
    if !bottom && !right {
        out.push(corner("resize-bottom-right", ResizeEdge::BottomRight, CursorStyle::ResizeUpLeftDownRight).bottom_0().right_0().into_any_element());
    }
    out
}
