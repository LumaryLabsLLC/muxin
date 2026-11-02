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
mod tray;
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

pub fn run(engine: Arc<Engine>, initial_view: Option<String>, listener: Option<crate::instance::Listener>) {
    if !engine.is_demo() {
        std::thread::spawn(crate::renamed::retire_old_app);
    }
    if let Some(listener) = listener {
        listener.serve(|request| {
            command(match request {
                crate::instance::Request::Show => AppCommand::Show,
                crate::instance::Request::Join(url) => AppCommand::Join(url),
                crate::instance::Request::Quit => AppCommand::Quit,
            })
        });
    }
    // After starting over (see `start_afresh`): the page the window was on.
    let initial_view = match (initial_view, std::env::var("MUXEN_REOPEN_VIEW").ok().filter(|v| !v.is_empty())) {
        (Some(view), Some(page)) => Some(format!("{view},{page}")),
        (view, page) => view.or(page),
    };
    // What a test run asked of its windows, for when one opens again.
    let window_options = initial_view.as_deref().map(|v| v.split(',').map(str::trim).filter(|p| p.starts_with("size:") || p.starts_with("display:") || *p == "tray").collect::<Vec<_>>().join(","));
    let mut view = initial_view;
    let mut reopened = 0;
    loop {
        run_app(engine.clone(), view);
        // A Mac never gets here, and on Windows this is the end. GPUI on Linux
        // ends with its last window (and has no window to hide, on Wayland), so
        // with the tray there the engine carries on, and a new app opens the
        // window when it's wanted.
        if !cfg!(target_os = "linux") || !wait_to_reopen(&engine) {
            return;
        }
        reopened += 1;
        // Each app leaves its parked threads behind, and a little memory.
        if reopened >= REOPENS_PER_PROCESS {
            start_afresh();
        }
        view = window_options.clone();
    }
}

/// How often the window opens again before the process starts over, on Linux.
const REOPENS_PER_PROCESS: u32 = 12;

fn run_app(engine: Arc<Engine>, initial_view: Option<String>) {
    let app = Application::new().with_assets(assets::Assets);
    // Share links (muxin://join?server=...#...) opened with Muxin.
    app.on_open_urls(|urls| urls.into_iter().filter(|u| u.starts_with("muxin://join")).for_each(|url| command(AppCommand::Join(url))));
    // The Dock icon clicked, or Muxin opened again, with no window showing.
    app.on_reopen(show_main_window);
    app.run(move |cx: &mut App| {
        assets::load_fonts(cx);
        assets::set_dock_icon();
        keep_from_napping();
        // The look lives in config.toml ([ui]); demo mode keeps it in memory.
        let prefs_path = (!engine.is_demo()).then(|| engine.paths().config.clone());
        let appearance = Appearance::load(prefs_path);
        let wants_tray = appearance.tray;
        cx.set_global(appearance.resolve(cx.window_appearance()));
        cx.set_global(appearance);
        input::bind_keys(cx);
        box_dialog::bind_keys(cx);
        terminal::bind_keys(cx);
        desktop::bind_keys(cx);
        root::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| quit(cx));
        // Quitting ends this app's shares: guests hear so if there's time.
        cx.on_app_quit(|cx| {
            crate::share::host::stop_all();
            #[cfg(windows)]
            tray::stop();
            cx.background_executor().timer(std::time::Duration::from_millis(90))
        })
        .detach();
        root::bind_app_actions(cx);
        cx.set_menus(root::menus());
        updates::start(engine.clone(), cx);
        // A test window (`--view display:builtin`) doesn't add an icon, unless
        // it's there to test the icon (`--view tray`).
        let has_token = |token: &str| initial_view.as_deref().is_some_and(|v| v.split(',').any(|p| p.trim() == token));
        if wants_tray && (!has_token("display:builtin") || has_token("tray")) {
            tray::start(engine.clone(), cx);
        }
        let takes_focus = !has_token("display:builtin");
        if open_main_window(engine.clone(), initial_view.as_deref(), cx).is_none() {
            quit(cx);
            return;
        }
        // With no window left, there's nothing to come back to, unless the tray
        // is there. (On a Mac and Windows the main window is hidden then, so
        // this only happens on Linux, where the app itself ends: see `run`.)
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() && !(cfg!(target_os = "linux") && tray::available()) {
                quit(cx);
            }
        })
        .detach();
        // What the tray and later launches ask for.
        cx.spawn(async move |cx| {
            let mut changed = commands().1.subscribe();
            loop {
                while let Some(next) = next_command() {
                    if cx.update(|cx| run_command(next, cx)).is_err() {
                        return;
                    }
                }
                if changed.changed().await.is_err() {
                    return;
                }
            }
        })
        .detach();
        if takes_focus {
            cx.activate(true);
        }
    });
}

/// Opens the main window: at launch, and again on Linux, where closing it
/// really closes it. None if it can't.
fn open_main_window(engine: Arc<Engine>, view: Option<&str>, cx: &mut App) -> Option<()> {
    let has_token = |token: &str| view.is_some_and(|v| v.split(',').any(|p| p.trim() == token));
    // `--view size:960x640` (screenshots and layout checks) overrides the default size.
    let (w, h) = view
        .and_then(|v| v.split(',').find_map(|p| p.trim().strip_prefix("size:")))
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?)))
        .unwrap_or((1380., 880.));
    // `--view display:builtin` puts the window on the laptop's own screen
    // (test runs stay off the main monitor); otherwise the primary display.
    let wants_builtin = has_token("display:builtin");
    let display = wants_builtin.then(|| builtin_display(cx)).flatten();
    // No bigger than the screen it opens on (a laptop's, a VM's 1280×800).
    let screen = display.and_then(|id| cx.find_display(id)).or_else(|| cx.primary_display()).map(|d| d.bounds().size);
    let (w, h) = match screen {
        // (Room for Windows' taskbar, the Dock, a panel.)
        Some(s) => (w.min(f32::from(s.width) * 0.94).max(640.), h.min(f32::from(s.height) * 0.86).max(480.)),
        None => (w, h),
    };
    let left = left_as_it_was().lock().unwrap().take();
    let bounds = left.as_ref().and_then(|l| l.bounds).unwrap_or_else(|| WindowBounds::Windowed(Bounds::centered(display, size(px(w), px(h)), cx)));
    let titlebar = if cfg!(target_os = "macos") {
        TitlebarOptions { title: Some("Muxin".into()), appears_transparent: true, traffic_light_position: Some(point(px(16.), px(19.))) }
    } else {
        TitlebarOptions { title: Some("Muxin".into()), appears_transparent: false, traffic_light_position: None }
    };
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
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
            let view_entity = cx.new(|cx| Root::new(engine.clone(), window, cx));
            window.focus(&view_entity.focus_handle(cx));
            if let Some(v) = view {
                // Comma-separated, e.g. "theme:gruvbox-light,machine:mac-studio".
                view_entity.update(cx, |ws, cx| v.split(',').for_each(|v| ws.open_view(v.trim(), window, cx)));
            }
            if let Some(left) = left {
                view_entity.update(cx, |root, cx| root.go_back_to(left, window, cx));
            }
            // The Dock or the tray brings the window back, so closing only hides
            // it. Without the tray on Windows, closing it quits. On Linux it
            // really closes, either way (see `run`).
            if !cfg!(target_os = "linux") {
                window.on_window_should_close(cx, |window, cx| {
                    let hide = cfg!(target_os = "macos") || tray::available();
                    if hide {
                        hide_main_window(window, cx);
                    }
                    !hide
                });
            }
            view_entity
        },
    );
    let window = match window {
        Ok(window) => window,
        Err(e) => {
            tracing::error!("couldn't open a window: {e:#}");
            return None;
        }
    };
    cx.set_global(MainWindow { handle: window, engine, takes_focus: !wants_builtin, options: view.map(str::to_string) });
    Some(())
}

/// The app's main window, for bringing it back.
struct MainWindow {
    handle: gpui::WindowHandle<Root>,
    engine: Arc<Engine>,
    /// Not a test window (see `--view display:builtin`).
    takes_focus: bool,
    /// What it was opened with, for opening it again.
    options: Option<String>,
}

impl gpui::Global for MainWindow {}

/// Brings the main window back as it was left, in front of the others.
fn show_main_window(cx: &mut App) {
    let Some(main) = cx.try_global::<MainWindow>() else { return };
    let (handle, takes_focus) = (main.handle, main.takes_focus);
    let shown = handle.update(cx, |root, window, cx| {
        order_in(window);
        root.window_shown(window, cx);
        window.activate_window();
    });
    // It really closed, with another window of ours keeping the app going (on Linux).
    if shown.is_err() {
        let (engine, options) = (main_engine(cx), cx.global::<MainWindow>().options.clone());
        // Only its window options: `go_back_to` finds the page.
        let options = options.map(|o| o.split(',').map(str::trim).filter(|p| p.starts_with("display:")).collect::<Vec<_>>().join(","));
        let _ = open_main_window(engine, options.as_deref(), cx);
    }
    if takes_focus {
        cx.activate(true);
    }
}

fn main_engine(cx: &App) -> Arc<Engine> {
    cx.global::<MainWindow>().engine.clone()
}

/// What the tray and later launches ask of the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppCommand {
    Show,
    OpenSession(String),
    /// A share link opened with Muxin: the window offers to join it.
    Join(String),
    Quit,
}

/// They wait in a queue, because they can come with no app running to take them
/// (on Linux, with the window closed), and a number changes for each.
fn commands() -> &'static (std::sync::Mutex<std::collections::VecDeque<AppCommand>>, tokio::sync::watch::Sender<u64>) {
    static COMMANDS: std::sync::OnceLock<(std::sync::Mutex<std::collections::VecDeque<AppCommand>>, tokio::sync::watch::Sender<u64>)> = std::sync::OnceLock::new();
    COMMANDS.get_or_init(|| (Default::default(), tokio::sync::watch::Sender::new(0)))
}

pub(crate) fn command(next: AppCommand) {
    let (queue, changed) = commands();
    queue.lock().unwrap().push_back(next);
    changed.send_modify(|n| *n = n.wrapping_add(1));
}

fn next_command() -> Option<AppCommand> {
    commands().0.lock().unwrap().pop_front()
}

fn run_command(next: AppCommand, cx: &mut App) {
    match next {
        AppCommand::Show => show_main_window(cx),
        AppCommand::OpenSession(id) => {
            show_main_window(cx);
            with_root(cx, |root, window, cx| root.navigate(root::Route::Session(id), window, cx));
        }
        AppCommand::Join(url) => {
            show_main_window(cx);
            with_root(cx, |root, _, cx| {
                root.pending_join = Some(url);
                cx.notify();
            });
        }
        AppCommand::Quit => quit(cx),
    }
}

fn with_root(cx: &mut App, f: impl FnOnce(&mut Root, &mut gpui::Window, &mut gpui::Context<Root>)) {
    if let Some(handle) = cx.try_global::<MainWindow>().map(|main| main.handle) {
        let _ = handle.update(cx, f);
    }
}

/// The user asked to quit, as opposed to the last window closing.
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn quit(cx: &mut App) {
    QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
    cx.quit();
}

/// What a window that really closed (on Linux) comes back to.
fn left_as_it_was() -> &'static std::sync::Mutex<Option<root::Left>> {
    static LEFT: std::sync::Mutex<Option<root::Left>> = std::sync::Mutex::new(None);
    &LEFT
}

/// How long the tray may be gone before the window comes back: a desktop shell
/// restarting brings it back itself.
const TRAY_GONE_FOR: std::time::Duration = std::time::Duration::from_secs(5);

/// With no app running (Linux, the window closed), waits until the window is
/// wanted again. False when the app should end instead. A window closed with no
/// tray to bring it back ends the app; if the tray goes away later, the window
/// comes back, so it's never out of reach.
fn wait_to_reopen(engine: &Engine) -> bool {
    use std::sync::atomic::Ordering;
    if QUITTING.load(Ordering::SeqCst) || !tray::available() {
        return false;
    }
    engine.runtime().block_on(async {
        let mut changed = commands().1.subscribe();
        let mut gone_since = None;
        loop {
            {
                let queue = commands().0.lock().unwrap();
                if queue.contains(&AppCommand::Quit) {
                    return false;
                }
                if !queue.is_empty() {
                    return true;
                }
            }
            gone_since = if tray::available() { None } else { gone_since.or(Some(std::time::Instant::now())) };
            if gone_since.is_some_and(|at| at.elapsed() >= TRAY_GONE_FOR) {
                return true;
            }
            tokio::select! {
                _ = changed.changed() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
        }
    })
}

/// Replaces this process with a new one of the same (see `REOPENS_PER_PROCESS`),
/// on the page the window was on. Links reconnect; forwards open again.
#[cfg(target_os = "linux")]
fn start_afresh() {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return };
    let mut view = left_as_it_was().lock().unwrap().as_ref().map(root::Left::as_view).unwrap_or_default();
    view.retain(|v| !v.is_empty());
    // (Without a share link it was opened with: that one's been dealt with.)
    let args: Vec<_> = std::env::args_os().skip(1).filter(|a| !a.to_string_lossy().starts_with("muxin://")).collect();
    let error = std::process::Command::new(exe).args(args).env("MUXEN_REOPEN_VIEW", view.join(",")).exec();
    tracing::warn!("starting over: {error}");
}

#[cfg(not(target_os = "linux"))]
fn start_afresh() {}

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

/// Closes the main window on a Mac, or on Windows with the tray: it goes out of
/// sight with everything in it (`show_main_window` brings it back), and the app
/// keeps running until it's told to quit.
fn hide_main_window(window: &mut gpui::Window, cx: &mut App) {
    if let Some(Some(root)) = window.root::<Root>() {
        root.update(cx, |root, cx| root.window_hidden(cx));
    }
    if !cfg!(target_os = "macos") || !window.is_fullscreen() {
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

#[cfg(windows)]
fn window_hwnd(window: &gpui::Window) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return None };
    Some(h.hwnd.get() as *mut std::ffi::c_void)
}

#[cfg(windows)]
fn order_out(window: &gpui::Window, cx: &App) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
    let Some(hwnd) = window_hwnd(window) else { return };
    // (As on a Mac: not from in here. It would change which window is active.)
    let hwnd = hwnd as usize;
    cx.foreground_executor()
        .spawn(async move {
            unsafe { ShowWindow(hwnd as *mut std::ffi::c_void, SW_HIDE) };
        })
        .detach();
}

#[cfg(not(any(target_os = "macos", windows)))]
fn order_out(_: &gpui::Window, _: &App) {}

/// A hidden window, shown again. (`activate_window` does this itself on a Mac.)
#[cfg(windows)]
fn order_in(window: &gpui::Window) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_SHOW, ShowWindow};
    if let Some(hwnd) = window_hwnd(window) {
        unsafe { ShowWindow(hwnd, SW_SHOW) };
    }
}

#[cfg(not(windows))]
fn order_in(_: &gpui::Window) {}

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
