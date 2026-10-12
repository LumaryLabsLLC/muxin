//! The native window, built with GPUI (Zed's UI framework).

mod about;
mod activity;
mod cloud_dialog;
mod assets;
mod detail;
mod dialogs;
mod files;
mod format;
mod github;
mod input;
mod kit;
mod logs;
mod menu;
mod overview;
mod palette;
mod panes;
mod ports;
mod desktop;
mod sessions;
mod compose;
mod services;
mod settings;
mod shortcuts;
mod terminal;
mod theme;
mod run;
mod usage;
mod themes;
mod workspaces;
mod root;

use std::sync::Arc;

use gpui::{App, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*, px, size};

use crate::fleet::Engine;
use theme::Appearance;
use root::{Quit, Root};

pub fn run(engine: Arc<Engine>, initial_view: Option<String>) {
    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        assets::load_fonts(cx);
        assets::set_dock_icon();
        // The look lives in config.toml ([ui]); demo mode keeps it in memory.
        let prefs_path = (!engine.is_demo()).then(|| engine.paths().config.clone());
        let appearance = Appearance::load(prefs_path);
        cx.set_global(appearance.resolve(cx.window_appearance()));
        cx.set_global(appearance);
        input::bind_keys(cx);
        terminal::bind_keys(cx);
        desktop::bind_keys(cx);
        root::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(root::menus());

        // `--view size:960x640` (screenshots and layout checks) overrides the default size.
        let (w, h) = initial_view
            .as_deref()
            .and_then(|v| v.split(',').find_map(|p| p.trim().strip_prefix("size:")))
            .and_then(|s| s.split_once('x'))
            .and_then(|(w, h)| Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?)))
            .unwrap_or((1380., 880.));
        // `--view display:builtin` puts the window on the laptop's own screen
        // (test runs stay off the main monitor); otherwise the primary display.
        let wants_builtin = initial_view.as_deref().is_some_and(|v| v.split(',').any(|p| p.trim() == "display:builtin"));
        let display = wants_builtin.then(|| builtin_display(cx)).flatten();
        // No bigger than the screen it opens on (a laptop's, a VM's 1280×800).
        let screen = display.and_then(|id| cx.find_display(id)).or_else(|| cx.primary_display()).map(|d| d.bounds().size);
        let (w, h) = match screen {
            // (Room for Windows' taskbar, the Dock, a panel.)
            Some(s) => (w.min(f32::from(s.width) * 0.94).max(640.), h.min(f32::from(s.height) * 0.86).max(480.)),
            None => (w, h),
        };
        let bounds = Bounds::centered(display, size(px(w), px(h)), cx);
        let titlebar = if cfg!(target_os = "macos") {
            TitlebarOptions { title: Some("Muxen".into()), appears_transparent: true, traffic_light_position: Some(point(px(16.), px(19.))) }
        } else {
            TitlebarOptions { title: Some("Muxen".into()), appears_transparent: false, traffic_light_position: None }
        };
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id: display,
                titlebar: Some(titlebar),
                window_min_size: Some(size(px(960.), px(600.))),
                app_id: Some("muxen".into()),
                // A test window (on the laptop's screen) doesn't take the keyboard
                // from whatever the user is typing in.
                focus: !wants_builtin,
                ..Default::default()
            },
            |window, cx| {
                theme::apply(window, cx);
                let view = cx.new(|cx| Root::new(engine.clone(), window, cx));
                window.focus(&view.focus_handle(cx));
                if let Some(v) = &initial_view {
                    // Comma-separated, e.g. "theme:gruvbox-light,machine:mac-studio".
                    view.update(cx, |ws, cx| v.split(',').for_each(|v| ws.open_view(v.trim(), window, cx)));
                }
                view
            },
        );
        if let Err(e) = window {
            tracing::error!("couldn't open a window: {e:#}");
            cx.quit();
            return;
        }
        // Closing the window quits, like a single-window utility should.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        if !wants_builtin {
            cx.activate(true);
        }
    });
}

/// This computer's clipboard right now: a number that changes with every
/// copy, and whether what's there is private (a password manager marks its
/// copies, per nspasteboard.org). None where that can't be told (not macOS).
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare
pub(crate) fn pasteboard_state() -> Option<(i64, bool)> {
    unsafe {
        use objc::runtime::Object;
        use objc::{class, msg_send, sel, sel_impl};
        let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
        if pb.is_null() {
            return None;
        }
        let count: i64 = msg_send![pb, changeCount];
        let types: *mut Object = msg_send![pb, types];
        let mut private = false;
        if !types.is_null() {
            let n: usize = msg_send![types, count];
            for i in 0..n {
                let t: *mut Object = msg_send![types, objectAtIndex: i];
                let utf8: *const std::os::raw::c_char = msg_send![t, UTF8String];
                if utf8.is_null() {
                    continue;
                }
                let name = std::ffi::CStr::from_ptr(utf8).to_string_lossy();
                if ["org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType", "org.nspasteboard.AutoGeneratedType", "com.agilebits.onepassword"].contains(&name.as_ref()) {
                    private = true;
                }
            }
        }
        Some((count, private))
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn pasteboard_state() -> Option<(i64, bool)> {
    None
}

/// Brings up the Tailscale app (to turn it on or sign in). Muxen itself never
/// changes Tailscale's settings.
pub(crate) fn open_tailscale() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").args(["-a", "Tailscale"]).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new(r"C:\Program Files\Tailscale\tailscale-ipn.exe").spawn();
}

/// Whether `open_tailscale` can do anything here (Linux has no Tailscale app).
pub(crate) fn can_open_tailscale() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// The built-in display (a MacBook's own screen), when there is one.
#[cfg(target_os = "macos")]
fn builtin_display(cx: &App) -> Option<gpui::DisplayId> {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGDisplayIsBuiltin(display: u32) -> i32;
    }
    cx.displays().into_iter().map(|d| d.id()).find(|id| unsafe { CGDisplayIsBuiltin(u32::from(*id)) != 0 })
}

#[cfg(not(target_os = "macos"))]
fn builtin_display(_: &App) -> Option<gpui::DisplayId> {
    None
}

/// A press on our own title bar: a double click zooms (or whatever the system
/// setting says); otherwise the window follows the mouse. On macOS GPUI only
/// does this in the top 28 points, so we ask AppKit to drag from the event.
#[allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare
pub(crate) fn title_bar_press(e: &gpui::MouseDownEvent, window: &mut gpui::Window) {
    if e.click_count == 2 {
        window.titlebar_double_click();
        return;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        use objc::runtime::Object;
        use objc::{class, msg_send, sel, sel_impl};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = HasWindowHandle::window_handle(&*window) else { return };
        let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
        let view = h.ns_view.as_ptr() as *mut Object;
        let ns_window: *mut Object = msg_send![view, window];
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let event: *mut Object = msg_send![app, currentEvent];
        if !ns_window.is_null() && !event.is_null() {
            let _: () = msg_send![ns_window, performWindowDragWithEvent: event];
        }
    }
    #[cfg(not(target_os = "macos"))]
    window.start_window_move();
}
