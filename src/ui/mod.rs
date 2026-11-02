//! The native window, built with GPUI (Zed's UI framework).

mod about;
mod box_dialog;
mod boxes;
mod activity;
mod cloud_dialog;
mod assets;
mod detail;
mod dialogs;
mod files;
mod format;
mod frame;
mod github;
mod hosted;
mod input;
mod kit;
mod logs;
mod menu;
mod move_dialog;
mod overview;
mod palette;
mod panes;
mod ports;
mod desktop;
mod sessions;
mod compose;
mod services;
mod share;
mod settings;
mod shortcuts;
mod terminal;
mod theme;
mod run;
mod schedule_dialog;
mod schedules;
mod updates;
mod usage;
mod vm_dialog;
mod themes;
mod workspaces;
mod root;

use std::sync::Arc;

use gpui::{App, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*, px, size};

use crate::fleet::Engine;
use theme::Appearance;
use root::{Quit, Root};

pub fn run(engine: Arc<Engine>, initial_view: Option<String>) {
    if !engine.is_demo() {
        std::thread::spawn(crate::renamed::retire_old_app);
    }
    let app = Application::new().with_assets(assets::Assets);
    // Share links (muxin://join?server=...#...) opened with Muxin.
    app.on_open_urls(|urls| urls.into_iter().filter(|u| u.starts_with("muxin://join")).for_each(open_link));
    // The Dock icon clicked, or Muxin opened again, with no window showing.
    app.on_reopen(show_main_window);
    app.run(move |cx: &mut App| {
        assets::load_fonts(cx);
        assets::set_dock_icon();
        keep_from_napping();
        // The look lives in config.toml ([ui]); demo mode keeps it in memory.
        let prefs_path = (!engine.is_demo()).then(|| engine.paths().config.clone());
        let appearance = Appearance::load(prefs_path);
        cx.set_global(appearance.resolve(cx.window_appearance()));
        cx.set_global(appearance);
        input::bind_keys(cx);
        box_dialog::bind_keys(cx);
        terminal::bind_keys(cx);
        desktop::bind_keys(cx);
        root::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        // Quitting ends this app's shares: guests hear so if there's time.
        cx.on_app_quit(|cx| {
            crate::share::host::stop_all();
            cx.background_executor().timer(std::time::Duration::from_millis(90))
        })
        .detach();
        root::bind_app_actions(cx);
        cx.set_menus(root::menus());
        updates::start(engine.clone(), cx);

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
            TitlebarOptions { title: Some("Muxin".into()), appears_transparent: true, traffic_light_position: Some(point(px(16.), px(19.))) }
        } else {
            TitlebarOptions { title: Some("Muxin".into()), appears_transparent: false, traffic_light_position: None }
        };
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id: display,
                titlebar: Some(titlebar),
                window_min_size: Some(size(px(960.), px(600.))),
                app_id: Some("muxin".into()),
                window_decorations: frame::requested(),
                // A test window (on the laptop's screen) doesn't take the keyboard
                // from whatever the user is typing in.
                focus: !wants_builtin,
                ..Default::default()
            },
            |window, cx| {
                // GPUI gives Wayland no title from the options: without one, a
                // dock or app switcher shows "unknown".
                window.set_window_title("Muxin");
                theme::apply(window, cx);
                let view = cx.new(|cx| Root::new(engine.clone(), window, cx));
                window.focus(&view.focus_handle(cx));
                if let Some(v) = &initial_view {
                    // Comma-separated, e.g. "theme:gruvbox-light,machine:mac-studio".
                    view.update(cx, |ws, cx| v.split(',').for_each(|v| ws.open_view(v.trim(), window, cx)));
                }
                // A Mac's Dock brings the window back, so closing only hides it.
                // Linux and Windows have nothing to bring it back with: closing quits.
                if cfg!(target_os = "macos") {
                    window.on_window_should_close(cx, |window, cx| {
                        hide_main_window(window, cx);
                        false
                    });
                }
                view
            },
        );
        let window = match window {
            Ok(window) => window,
            Err(e) => {
                tracing::error!("couldn't open a window: {e:#}");
                cx.quit();
                return;
            }
        };
        cx.set_global(MainWindow { handle: window, takes_focus: !wants_builtin });
        // With no window left, there's nothing to come back to. (On a Mac the
        // main window is only ever hidden, so this doesn't happen there.)
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

/// The app's main window, for bringing it back.
struct MainWindow {
    handle: gpui::WindowHandle<Root>,
    /// Not a test window (see `--view display:builtin`).
    takes_focus: bool,
}

impl gpui::Global for MainWindow {}

/// Brings the main window back as it was left, in front of the others.
fn show_main_window(cx: &mut App) {
    let Some(&MainWindow { handle, takes_focus }) = cx.try_global::<MainWindow>() else { return };
    let _ = handle.update(cx, |root, window, cx| {
        root.window_shown(window, cx);
        window.activate_window();
    });
    if takes_focus {
        cx.activate(true);
    }
}

/// Keeps App Nap off for as long as the app runs. Minutes after its windows go
/// out of sight, it would hold back the app's timers by a minute or more, and
/// machine checks, connections and forwards with them. The Mac can still sleep.
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare
fn keep_from_napping() {
    use cocoa::base::{id, nil};
    use cocoa::foundation::NSString;
    use objc::{class, msg_send, sel, sel_impl};
    // NSActivityUserInitiatedAllowingIdleSystemSleep.
    const OPTIONS: u64 = 0x00EF_FFFF;
    unsafe {
        let info: id = msg_send![class!(NSProcessInfo), processInfo];
        let reason = NSString::alloc(nil).init_str("Keeping up with your machines");
        let activity: id = msg_send![info, beginActivityWithOptions: OPTIONS reason: reason];
        // Never ended: it lasts until the app quits.
        let _: id = msg_send![activity, retain];
    }
}

#[cfg(not(target_os = "macos"))]
fn keep_from_napping() {}

/// Closes the main window on a Mac: it goes out of sight with everything in it
/// (`show_main_window` brings it back), and the app keeps running until ⌘Q.
fn hide_main_window(window: &mut gpui::Window, cx: &mut App) {
    if let Some(Some(root)) = window.root::<Root>() {
        root.update(cx, |root, cx| root.window_hidden(cx));
    }
    if !window.is_fullscreen() {
        order_out(window, cx);
        return;
    }
    // Hidden while it fills the screen, it would leave an empty Space behind.
    window.toggle_fullscreen();
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        for _ in 0..50 {
            cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
            if !handle.update(cx, |_, window, _| window.is_fullscreen()).unwrap_or(false) {
                break;
            }
        }
        // (The animation may still be finishing.)
        cx.background_executor().timer(std::time::Duration::from_millis(700)).await;
        let _ = handle.update(cx, |_, window, cx| order_out(window, cx));
    })
    .detach();
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare
fn order_out(window: &gpui::Window, cx: &App) {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    let view = h.ns_view.as_ptr() as *mut Object;
    let ns_window: *mut Object = unsafe { msg_send![view, window] };
    if ns_window.is_null() {
        return;
    }
    // Not from in here: another of the app's windows gets the keyboard at once,
    // and draws a frame then, which GPUI can't do while it's already busy.
    cx.foreground_executor()
        .spawn(async move {
            let _: () = unsafe { msg_send![ns_window, orderOut: std::ptr::null_mut::<Object>()] };
        })
        .detach();
}

#[cfg(not(target_os = "macos"))]
fn order_out(_: &gpui::Window, _: &App) {}

fn opened_links() -> &'static (futures::channel::mpsc::UnboundedSender<String>, std::sync::Mutex<Option<futures::channel::mpsc::UnboundedReceiver<String>>>) {
    static LINKS: std::sync::OnceLock<(futures::channel::mpsc::UnboundedSender<String>, std::sync::Mutex<Option<futures::channel::mpsc::UnboundedReceiver<String>>>)> = std::sync::OnceLock::new();
    LINKS.get_or_init(|| {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        (tx, std::sync::Mutex::new(Some(rx)))
    })
}

/// A share link opened with Muxin: the window offers to join it.
pub fn open_link(url: String) {
    let _ = opened_links().0.unbounded_send(url);
}

/// The links opened with Muxin, for the window (once).
fn take_opened_links() -> Option<futures::channel::mpsc::UnboundedReceiver<String>> {
    opened_links().1.lock().unwrap().take()
}

/// This computer's clipboard right now: a number that changes with every
/// copy, and whether what's there is private (a password manager marks its
/// copies: per nspasteboard.org on a Mac, with Windows' clipboard-history
/// formats on Windows). None where that can't be told (Linux).
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

/// Windows numbers clipboard changes. A copy that carries one of the formats
/// that keep it out of clipboard history and other devices counts as private.
/// (The last two hold a DWORD, 0 to keep it out; reading it would mean opening
/// the clipboard every few hundred milliseconds, in other programs' way.)
#[cfg(windows)]
pub(crate) fn pasteboard_state() -> Option<(i64, bool)> {
    use windows_sys::Win32::System::DataExchange::{GetClipboardSequenceNumber, IsClipboardFormatAvailable, RegisterClipboardFormatW};
    let private = ["ExcludeClipboardContentFromMonitorProcessing", "CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"].iter().any(|name| {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let format = unsafe { RegisterClipboardFormatW(wide.as_ptr()) };
        format != 0 && unsafe { IsClipboardFormatAvailable(format) } != 0
    });
    Some((unsafe { GetClipboardSequenceNumber() } as i64, private))
}

/// Linux: GPUI says neither what kinds of data a copy holds nor when it
/// changed, so a password manager's copies (marked `x-kde-passwordManagerHint`)
/// can't be told from others here; they're shared like any text.
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) fn pasteboard_state() -> Option<(i64, bool)> {
    None
}

/// Brings up the Tailscale app (to turn it on or sign in). Muxin itself never
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

/// A press on our own title bar: a double click zooms (on macOS, whatever the
/// system setting says); otherwise the window follows the mouse. On macOS GPUI
/// only does this in the top 28 points, so we ask AppKit to drag from the event.
#[allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare
pub(crate) fn title_bar_press(e: &gpui::MouseDownEvent, window: &mut gpui::Window) {
    if e.click_count == 2 {
        if cfg!(target_os = "macos") {
            window.titlebar_double_click();
        } else {
            window.zoom_window();
        }
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
