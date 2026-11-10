//! The icon in the menu bar or the system tray: the way back to a closed window,
//! with a dot while a session needs you or is done (never a notification from
//! the system). Its menu says how things stand in one line, lists those
//! sessions, opens Settings and the update check, has the versions to copy for
//! a bug report, and quits (sessions keep running on their machines).
//!
//! On a Mac and Windows it's `tray-icon`, on the main thread. On Linux it's a
//! StatusNotifierItem over D-Bus, and outlives the app's windows (see `ui::run`).
//! A desktop with nothing to show those (GNOME without its AppIndicator
//! extension) shows no icon: `available` says so, and closing the window quits.

use std::sync::{Arc, OnceLock};

use gpui::App;
use tokio::sync::watch;

use super::{AppCommand, command};
use crate::fleet::Engine;
use crate::fleet::model::Snapshot;

/// The menu's list stops here: more than this is a reason to open the window.
const MOST_LISTED: usize = 8;
const LONGEST_TITLE: usize = 40;
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The waiting dot: black on a Mac, where the menu bar tints the icon.
const DOT: [u8; 3] = if cfg!(target_os = "macos") { [0, 0, 0] } else { [0xF5, 0xA6, 0x23] };

#[cfg(target_os = "macos")]
const ICON: &[u8] = include_bytes!("../../assets/brand/menu-bar.png");
#[cfg(not(target_os = "macos"))]
const ICON: &[u8] = include_bytes!("../../assets/brand/icon-128.png");

/// A session that needs you (or ended with an error) or is done, for the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Waiting {
    id: String,
    label: String,
    /// Needs you (or exited with an error); else it's done.
    needs_you: bool,
}

/// Those that need you first, then those that are done.
fn waiting_sessions(snapshot: &Snapshot) -> Vec<Waiting> {
    use crate::fleet::sessions::Mark;
    let mut out: Vec<Waiting> = snapshot
        .sessions
        .iter()
        .filter(|s| matches!(s.mark(), Mark::NeedsYou | Mark::Failed | Mark::Done))
        .map(|s| {
            let machine = snapshot.machines.iter().find(|m| m.id == s.machine_id).map_or(s.machine_id.as_str(), |m| m.name.as_str());
            let title = if s.title.trim().is_empty() { &s.agent_name } else { &s.title };
            Waiting { id: s.id.clone(), label: format!("{} · {machine}", short_title(title)), needs_you: s.mark() != Mark::Done }
        })
        .collect();
    out.sort_by_key(|w| !w.needs_you);
    out
}

/// A session's title can be anything a program set: one short line for a menu.
fn short_title(title: &str) -> String {
    let line = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= LONGEST_TITLE {
        return line;
    }
    let cut: String = line.chars().take(LONGEST_TITLE - 1).collect();
    format!("{}…", cut.trim_end())
}

/// How things stand, in one short line: "3 machines online · 2 working · 1
/// needs you". Machines disconnected on purpose, or with nothing to check
/// them by, don't count.
fn status_line(snapshot: &Snapshot) -> String {
    use crate::fleet::sessions::Mark;
    let machines: Vec<_> = snapshot.machines.iter().filter(|m| !matches!(m.status.as_str(), "disconnected" | "manual")).collect();
    let online = machines.iter().filter(|m| m.status == "online").count();
    let mut parts = vec![match machines.len() {
        0 if snapshot.machines.is_empty() => "No machines yet".to_string(),
        0 => "No machines connected".to_string(),
        _ if online == 0 && machines.iter().any(|m| m.status == "unchecked") => "Checking machines…".to_string(),
        1 if online == 1 => "1 machine online".to_string(),
        1 => "1 machine offline".to_string(),
        n if online == n => format!("{n} machines online"),
        _ if online == 0 => "No machines online".to_string(),
        n => format!("{online} of {n} machines online"),
    }];
    let marks: Vec<Mark> = snapshot.sessions.iter().map(|s| s.mark()).collect();
    let working = marks.iter().filter(|m| **m == Mark::Working).count();
    // As in the list below: an exit with an error waits for you too.
    let needs_you = marks.iter().filter(|m| matches!(m, Mark::NeedsYou | Mark::Failed)).count();
    if working > 0 {
        parts.push(format!("{working} working"));
    }
    if needs_you > 0 {
        parts.push(format!("{needs_you} need{} you", if needs_you == 1 { "s" } else { "" }));
    }
    parts.join(" · ")
}

/// This computer's agent version, as its last check found it. None when this
/// computer isn't one of the machines, or before its first check.
fn agent_here(snapshot: &Snapshot) -> Option<String> {
    snapshot.machines.iter().filter(|m| m.local).find_map(|m| m.agent_version.clone())
}

/// A newer version, once a look at GitHub found one (see `ui::updates`). It
/// outlives the app on Linux, where the tray does too.
fn newer_version() -> &'static watch::Sender<Option<String>> {
    static NEWER: OnceLock<watch::Sender<Option<String>>> = OnceLock::new();
    NEWER.get_or_init(|| watch::Sender::new(None))
}

/// What the last look at GitHub found: the menu then says how to update.
pub(super) fn set_newer_version(version: Option<String>) {
    newer_version().send_if_modified(|v| {
        let changed = *v != version;
        *v = version;
        changed
    });
}

/// Everything the menu shows: when it changes, so does the menu.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct MenuState {
    status: String,
    waiting: Vec<Waiting>,
    agent: Option<String>,
    newer: Option<String>,
}

fn menu_state(snapshot: &Snapshot, newer: Option<String>) -> MenuState {
    MenuState { status: status_line(snapshot), waiting: waiting_sessions(snapshot), agent: agent_here(snapshot), newer }
}

/// A line of the menu, whichever system draws it.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    /// How things stand (see `status_line`): first, and not a button.
    Status(String),
    Note(String),
    Session(Waiting),
    Separator,
    Show,
    Settings,
    /// Check for Updates…, or the newer version a look found.
    Updates(Option<String>),
    /// The app's version and this computer's agent's, not a button.
    Versions(String),
    CopyVersions,
    Quit,
}

fn menu_entries(state: &MenuState) -> Vec<Entry> {
    let mut entries = vec![Entry::Status(state.status.clone()), Entry::Separator];
    let waiting = &state.waiting;
    if !waiting.is_empty() {
        let mut heading = None;
        for w in waiting.iter().take(MOST_LISTED) {
            if heading != Some(w.needs_you) {
                heading = Some(w.needs_you);
                entries.push(Entry::Note(if w.needs_you { "Needs you" } else { "Done" }.into()));
            }
            entries.push(Entry::Session(w.clone()));
        }
        if waiting.len() > MOST_LISTED {
            entries.push(Entry::Note(format!("and {} more", waiting.len() - MOST_LISTED)));
        }
        entries.push(Entry::Separator);
    }
    entries.extend([
        Entry::Show,
        Entry::Settings,
        Entry::Updates(state.newer.clone()),
        Entry::Separator,
        Entry::Versions(versions_line(state.agent.as_deref())),
        Entry::CopyVersions,
        Entry::Separator,
        Entry::Quit,
    ]);
    entries
}

const SHOW_LABEL: &str = "Show Muxin";
const SETTINGS_LABEL: &str = "Settings…";
const COPY_LABEL: &str = "Copy Versions";
/// Agent sessions live on their machines, held by Muxin's agent there.
const QUIT_LABEL: &str = "Quit Muxin (sessions keep running)";

fn updates_label(newer: Option<&str>) -> String {
    match newer {
        Some(version) => format!("Update to Muxin {version}…"),
        None => "Check for Updates…".into(),
    }
}

fn versions_line(agent: Option<&str>) -> String {
    match agent {
        Some(agent) => format!("Muxin {APP_VERSION} · Agent {agent}"),
        None => format!("Muxin {APP_VERSION}"),
    }
}

/// What Copy Versions puts on the clipboard, for a bug report: the app (and
/// its build, for a release), this computer's agent, and the system.
fn versions_report(agent: Option<&str>) -> String {
    let build = option_env!("MUXEN_BUILD").filter(|b| *b != APP_VERSION).map(|b| format!(" (build {b})")).unwrap_or_default();
    let agent = agent.map_or_else(|| "Agent: unknown".to_string(), |a| format!("Agent {a}"));
    format!("Muxin {APP_VERSION}{build}\n{agent}\n{}", this_system())
}

/// "macOS 27.2, aarch64", "Windows 11 (build 26100), x86_64", "Ubuntu 26.04
/// LTS, X-Cinnamon, x11, x86_64".
fn this_system() -> String {
    static SYSTEM: OnceLock<String> = OnceLock::new();
    SYSTEM.get_or_init(|| format!("{}, {}", system_name(), std::env::consts::ARCH)).clone()
}

#[cfg(target_os = "macos")]
fn system_name() -> String {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    let read = unsafe { libc::sysctlbyname(c"kern.osproductversion".as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
    let version = String::from_utf8_lossy(&buf[..if read { len.min(buf.len()) } else { 0 }]).trim_end_matches('\0').trim().to_string();
    if version.is_empty() { "macOS".into() } else { format!("macOS {version}") }
}

#[cfg(windows)]
fn system_name() -> String {
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    // (GetVersionEx says 6.2 to an app without a manifest that names Windows 10.)
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }
    let mut info = OsVersionInfo { size: std::mem::size_of::<OsVersionInfo>() as u32, major: 0, minor: 0, build: 0, platform: 0, service_pack: [0; 128] };
    if unsafe { RtlGetVersion(&mut info) } != 0 {
        return "Windows".into();
    }
    // Windows 11 still says 10.0: its builds start at 22000.
    match (info.major, info.minor) {
        (10, 0) if info.build >= 22000 => format!("Windows 11 (build {})", info.build),
        (10, 0) => format!("Windows 10 (build {})", info.build),
        (major, minor) => format!("Windows {major}.{minor} (build {})", info.build),
    }
}

/// The distribution, and the desktop and session type: what a tray's trouble
/// on Linux usually comes down to.
#[cfg(target_os = "linux")]
fn system_name() -> String {
    let release = std::fs::read_to_string("/etc/os-release").or_else(|_| std::fs::read_to_string("/usr/lib/os-release")).unwrap_or_default();
    let name = release.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty());
    let mut parts = vec![name.unwrap_or_else(|| "Linux".into())];
    parts.extend(["XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"].iter().filter_map(|v| std::env::var(v).ok()).filter(|v| !v.trim().is_empty()));
    parts.join(", ")
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn system_name() -> String {
    std::env::consts::OS.into()
}

/// The tooltip: the icon's name and how things stand (Windows shows it on a
/// hover, where the menu takes a right click).
fn tooltip(state: &MenuState) -> String {
    format!("Muxin · {}", state.status)
}

/// The icon as RGBA at this size, with the dot when something's waiting.
fn icon_pixels(waiting: bool, size: u32) -> Option<Vec<u8>> {
    let image = image::load_from_memory_with_format(ICON, image::ImageFormat::Png).ok()?.into_rgba8();
    let image = if image.width() == size { image } else { image::imageops::resize(&image, size, size, image::imageops::FilterType::Lanczos3) };
    let mut rgba = image.into_raw();
    if waiting {
        add_dot(&mut rgba, size);
    }
    Some(rgba)
}

/// Draws the dot in the top right corner, with a clear ring to set it apart.
fn add_dot(rgba: &mut [u8], size: u32) {
    let s = size as f32;
    let (radius, ring) = (s * 0.23, s * 0.09);
    let (center_x, center_y) = (s - radius, radius);
    for (i, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = ((i as u32 % size) as f32 + 0.5, (i as u32 / size) as f32 + 0.5);
        let distance = ((x - center_x).powi(2) + (y - center_y).powi(2)).sqrt();
        let cleared = (radius + ring - distance + 0.5).clamp(0., 1.);
        let dot = (radius - distance + 0.5).clamp(0., 1.);
        pixel[3] = (pixel[3] as f32 * (1. - cleared) + 255. * dot) as u8;
        if dot > 0. {
            pixel[..3].copy_from_slice(&DOT);
        }
    }
}

pub(super) use imp::{available, start, stop};

#[cfg(any(target_os = "macos", windows))]
mod imp {
    use std::cell::RefCell;
    use std::sync::Mutex;

    use tray_icon::menu::accelerator::{Accelerator, Code, Modifiers};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::*;

    const ICON_SIZE: u32 = if cfg!(target_os = "macos") { 44 } else { 32 };

    /// The icon, its menu as it shows, and the task keeping them up to date.
    struct Tray {
        icon: TrayIcon,
        menu: Shown,
        _follow: gpui::Task<()>,
    }

    /// A menu as built, with the lines whose words change in place: an open
    /// menu shows the new words at once.
    struct Shown {
        state: MenuState,
        status: MenuItem,
        updates: MenuItem,
        versions: MenuItem,
    }

    thread_local! {
        /// Main thread only.
        static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    }

    /// This computer's agent version as the menu shows it, for Copy Versions
    /// (the menu's handler may be called on any thread).
    static AGENT: Mutex<Option<String>> = Mutex::new(None);

    pub fn available() -> bool {
        TRAY.with(|t| t.borrow().is_some())
    }

    pub fn start(engine: Arc<Engine>, cx: &mut App) {
        if available() {
            return;
        }
        MenuEvent::set_event_handler(Some(|event: MenuEvent| match event.id().as_ref() {
            "show" => command(AppCommand::Show),
            "settings" => command(AppCommand::Settings),
            "updates" => command(AppCommand::CheckForUpdates),
            "copy" => command(AppCommand::Copy(versions_report(AGENT.lock().unwrap().as_deref()))),
            "quit" => command(AppCommand::Quit),
            id => {
                if let Some(session) = id.strip_prefix("session:") {
                    command(AppCommand::OpenSession(session.to_string()));
                }
            }
        }));
        // Windows' way: left click opens, right click has the menu. A Mac's menu
        // bar has the menu on either.
        TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event
                && cfg!(windows)
            {
                command(AppCommand::Show);
            }
        }));
        let mut snapshots = engine.subscribe();
        let mut newer = newer_version().subscribe();
        let state = menu_state(&snapshots.borrow_and_update(), newer.borrow_and_update().clone());
        let Some(first_icon) = icon(!state.waiting.is_empty()) else { return };
        let (menu, shown) = menu(&state);
        *AGENT.lock().unwrap() = state.agent.clone();
        let builder = TrayIconBuilder::new().with_tooltip(tooltip(&state)).with_menu_on_left_click(cfg!(target_os = "macos")).with_menu(Box::new(menu));
        // Tinted by the menu bar, and kept where it was dragged.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(first_icon).with_autosave_name("muxin");
        #[cfg(windows)]
        let builder = builder.with_icon(first_icon);
        let icon = match builder.build() {
            Ok(tray) => tray,
            Err(e) => {
                tracing::warn!("couldn't add the tray icon: {e}");
                return;
            }
        };
        let follow = cx.spawn(async move |_| {
            loop {
                // (Biased: no runtime's randomness here, on GPUI's thread.)
                tokio::select! {
                    biased;
                    changed = snapshots.changed() => if changed.is_err() { break },
                    changed = newer.changed() => if changed.is_err() { break },
                }
                show(menu_state(&snapshots.borrow_and_update(), newer.borrow_and_update().clone()));
            }
        });
        TRAY.with(|t| *t.borrow_mut() = Some(Tray { icon, menu: shown, _follow: follow }));
    }

    /// Takes the icon away. (On Windows one left at exit stays until it's hovered.)
    pub fn stop() {
        TRAY.with(|t| drop(t.borrow_mut().take()));
    }

    /// The menu, tooltip and icon for what's new, changing only what did.
    fn show(state: MenuState) {
        TRAY.with(|t| {
            let mut t = t.borrow_mut();
            let Some(tray) = t.as_mut() else { return };
            let before = &tray.menu.state;
            if *before == state {
                return;
            }
            if before.status != state.status {
                let _ = tray.icon.set_tooltip(Some(tooltip(&state)));
            }
            if before.waiting.is_empty() != state.waiting.is_empty() {
                set_icon(&tray.icon, icon(!state.waiting.is_empty()));
            }
            *AGENT.lock().unwrap() = state.agent.clone();
            if before.waiting == state.waiting {
                let shown = &mut tray.menu;
                if shown.state.status != state.status {
                    shown.status.set_text(plain(&state.status));
                }
                if shown.state.newer != state.newer {
                    shown.updates.set_text(plain(&updates_label(state.newer.as_deref())));
                }
                if shown.state.agent != state.agent {
                    shown.versions.set_text(plain(&versions_line(state.agent.as_deref())));
                }
                shown.state = state;
                return;
            }
            let (menu, shown) = menu(&state);
            tray.icon.set_menu(Some(Box::new(menu)));
            tray.menu = shown;
        });
    }

    #[cfg(target_os = "macos")]
    fn set_icon(tray: &TrayIcon, icon: Option<Icon>) {
        let _ = tray.set_icon_templated(icon);
    }

    #[cfg(windows)]
    fn set_icon(tray: &TrayIcon, icon: Option<Icon>) {
        let _ = tray.set_icon(icon);
    }

    fn icon(waiting: bool) -> Option<Icon> {
        Icon::from_rgba(icon_pixels(waiting, ICON_SIZE)?, ICON_SIZE, ICON_SIZE).ok()
    }

    /// The app's own shortcut for Settings, shown beside it: ⌘, on a Mac,
    /// Ctrl+, elsewhere. (It works in the window; in the menu only while the
    /// menu is open, on a Mac.)
    fn settings_shortcut() -> Accelerator {
        Accelerator::new(if cfg!(target_os = "macos") { Modifiers::META } else { Modifiers::CONTROL }, Code::Comma)
    }

    fn menu(state: &MenuState) -> (Menu, Shown) {
        let menu = Menu::new();
        let note = |text: &str| MenuItem::new(plain(text), false, None);
        let (mut status, mut updates, mut versions) = (None, None, None);
        for entry in menu_entries(state) {
            let _ = match entry {
                Entry::Status(text) => menu.append(status.insert(note(&text))),
                Entry::Note(text) => menu.append(&note(&text)),
                Entry::Session(w) => menu.append(&MenuItem::with_id(format!("session:{}", w.id), plain(&w.label), true, None)),
                Entry::Separator => menu.append(&PredefinedMenuItem::separator()),
                Entry::Show => menu.append(&MenuItem::with_id("show", SHOW_LABEL, true, None)),
                Entry::Settings => menu.append(&MenuItem::with_id("settings", SETTINGS_LABEL, true, Some(settings_shortcut()))),
                Entry::Updates(newer) => menu.append(updates.insert(MenuItem::with_id("updates", plain(&updates_label(newer.as_deref())), true, None))),
                Entry::Versions(text) => menu.append(versions.insert(note(&text))),
                Entry::CopyVersions => menu.append(&MenuItem::with_id("copy", COPY_LABEL, true, None)),
                Entry::Quit => menu.append(&MenuItem::with_id("quit", QUIT_LABEL, true, None)),
            };
        }
        // (Each is in every menu: see `menu_entries`.)
        let item = |i: Option<MenuItem>| i.unwrap_or_else(|| note(""));
        (menu, Shown { state: state.clone(), status: item(status), updates: item(updates), versions: item(versions) })
    }

    /// A "&" in a menu's text would underline the next letter.
    fn plain(text: &str) -> String {
        text.replace('&', "&&")
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use ksni::TrayMethods;

    use super::*;

    const ICON_SIZE: u32 = 48;

    /// Something on this desktop shows the icon.
    static AVAILABLE: AtomicBool = AtomicBool::new(false);
    /// Told (while starting up, too) that nothing does.
    static OFFLINE: AtomicBool = AtomicBool::new(false);
    static HANDLE: Mutex<Option<ksni::Handle<LinuxTray>>> = Mutex::new(None);
    static STARTED: AtomicBool = AtomicBool::new(false);

    pub fn available() -> bool {
        AVAILABLE.load(Ordering::SeqCst)
    }

    /// Once, whatever becomes of the windows: called with each new app (see
    /// `ui::run`). Runs on the engine's runtime, so it lasts with none open.
    pub fn start(engine: Arc<Engine>, _: &mut App) {
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let runtime = engine.runtime().clone();
        runtime.spawn(async move {
            OFFLINE.store(false, Ordering::SeqCst);
            let mut snapshots = engine.subscribe();
            let mut newer = newer_version().subscribe();
            let mut shown = menu_state(&snapshots.borrow_and_update(), newer.borrow_and_update().clone());
            let tray = LinuxTray { state: shown.clone(), plain: pixmap(false), dotted: pixmap(true) };
            // Assume it can show: on a desktop that can't yet, the icon appears
            // if that changes (the extension turned on), through `watcher_online`.
            let handle = match tray.assume_sni_available(true).spawn().await {
                Ok(handle) => handle,
                Err(e) => {
                    tracing::info!("no tray icon: {e}");
                    STARTED.store(false, Ordering::SeqCst);
                    return;
                }
            };
            if !OFFLINE.load(Ordering::SeqCst) {
                AVAILABLE.store(true, Ordering::SeqCst);
            }
            *HANDLE.lock().unwrap() = Some(handle.clone());
            while !handle.is_closed() {
                tokio::select! {
                    changed = snapshots.changed() => if changed.is_err() { break },
                    changed = newer.changed() => if changed.is_err() { break },
                }
                let state = menu_state(&snapshots.borrow_and_update(), newer.borrow_and_update().clone());
                if state != shown {
                    let next = state.clone();
                    handle.update(move |tray| tray.state = next).await;
                    shown = state;
                }
            }
        });
    }

    pub fn stop() {
        AVAILABLE.store(false, Ordering::SeqCst);
        STARTED.store(false, Ordering::SeqCst);
        if let Some(handle) = HANDLE.lock().unwrap().take() {
            drop(handle.shutdown());
        }
    }

    fn pixmap(waiting: bool) -> Vec<ksni::Icon> {
        let Some(mut data) = icon_pixels(waiting, ICON_SIZE) else { return Vec::new() };
        // ARGB, this format wants.
        data.as_chunks_mut::<4>().0.iter_mut().for_each(|pixel| pixel.rotate_right(1));
        vec![ksni::Icon { width: ICON_SIZE as i32, height: ICON_SIZE as i32, data }]
    }

    /// Puts text on the clipboard with the desktop's own tool when it has one:
    /// the app may have no window then, and Wayland only takes a copy from the
    /// window in front. Else the app does it (opening its window, if closed).
    fn copy(text: String) {
        std::thread::spawn(move || {
            if !copied_with_a_tool(&text) {
                command(AppCommand::Copy(text));
            }
        });
    }

    fn copied_with_a_tool(text: &str) -> bool {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let x11 = std::env::var_os("DISPLAY").is_some();
        let tools: [(&str, &[&str], bool); 3] = [("wl-copy", &[], wayland), ("xclip", &["-selection", "clipboard"], x11), ("xsel", &["--clipboard", "--input"], x11)];
        tools.iter().filter(|(_, _, usable)| *usable).any(|(tool, args, _)| run_with_input(tool, args, text))
    }

    /// Each of those stays on in the background, holding the copy, and the
    /// one started here ends once it has read it.
    fn run_with_input(tool: &str, args: &[&str], text: &str) -> bool {
        let Ok(mut child) = Command::new(tool).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return false };
        let wrote = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        child.wait().is_ok_and(|status| status.success()) && wrote
    }

    struct LinuxTray {
        state: MenuState,
        plain: Vec<ksni::Icon>,
        dotted: Vec<ksni::Icon>,
    }

    impl ksni::Tray for LinuxTray {
        fn id(&self) -> String {
            "muxin".into()
        }

        fn title(&self) -> String {
            "Muxin".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            if self.state.waiting.is_empty() { self.plain.clone() } else { self.dotted.clone() }
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "Muxin".into(), description: self.state.status.clone(), ..Default::default() }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            command(AppCommand::Show);
        }

        fn watcher_online(&self) {
            OFFLINE.store(false, Ordering::SeqCst);
            AVAILABLE.store(true, Ordering::SeqCst);
        }

        /// Keeps waiting for one to appear, unless D-Bus itself is the trouble.
        fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
            OFFLINE.store(true, Ordering::SeqCst);
            AVAILABLE.store(false, Ordering::SeqCst);
            !matches!(reason, ksni::OfflineReason::Error(ksni::Error::Dbus(_)))
        }

        /// (Settings shows no shortcut here: desktops spell a "," in one
        /// differently, and most tray menus don't show them.)
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::{MenuItem, StandardItem};
            let item = |label: &str, run: Box<dyn Fn(&mut Self) + Send>| -> MenuItem<Self> { StandardItem { label: plain(label), activate: run, ..Default::default() }.into() };
            let note = |text: &str| -> MenuItem<Self> { StandardItem { label: plain(text), enabled: false, ..Default::default() }.into() };
            menu_entries(&self.state)
                .into_iter()
                .map(|entry| match entry {
                    Entry::Status(text) | Entry::Note(text) | Entry::Versions(text) => note(&text),
                    Entry::Session(w) => {
                        let id = w.id.clone();
                        item(&w.label, Box::new(move |_| command(AppCommand::OpenSession(id.clone()))))
                    }
                    Entry::Separator => MenuItem::Separator,
                    Entry::Show => item(SHOW_LABEL, Box::new(|_| command(AppCommand::Show))),
                    Entry::Settings => item(SETTINGS_LABEL, Box::new(|_| command(AppCommand::Settings))),
                    Entry::Updates(newer) => item(&updates_label(newer.as_deref()), Box::new(|_| command(AppCommand::CheckForUpdates))),
                    Entry::CopyVersions => item(COPY_LABEL, Box::new(|tray: &mut Self| copy(versions_report(tray.state.agent.as_deref())))),
                    Entry::Quit => item(QUIT_LABEL, Box::new(|_| command(AppCommand::Quit))),
                })
                .collect()
        }
    }

    /// A "_" in a menu's text would underline the next letter.
    fn plain(text: &str) -> String {
        text.replace('_', "__")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(n: usize) -> Vec<Waiting> {
        (0..n).map(|i| Waiting { id: format!("m/s{i}"), label: format!("Session {i} · m"), needs_you: true }).collect()
    }

    fn state(waiting: Vec<Waiting>) -> MenuState {
        MenuState { status: "1 machine online".into(), waiting, agent: Some("0.6.0".into()), newer: None }
    }

    /// What every menu ends with, after the sessions.
    fn the_rest(newer: Option<&str>, agent: Option<&str>) -> Vec<Entry> {
        vec![
            Entry::Show,
            Entry::Settings,
            Entry::Updates(newer.map(str::to_string)),
            Entry::Separator,
            Entry::Versions(versions_line(agent)),
            Entry::CopyVersions,
            Entry::Separator,
            Entry::Quit,
        ]
    }

    #[test]
    fn nothing_waiting_is_the_status_then_the_rest() {
        let mut want = vec![Entry::Status("1 machine online".into()), Entry::Separator];
        want.extend(the_rest(None, Some("0.6.0")));
        assert_eq!(menu_entries(&state(Vec::new())), want);
    }

    #[test]
    fn waiting_sessions_come_after_the_status() {
        let entries = menu_entries(&state(waiting(2)));
        assert_eq!(entries[..2], [Entry::Status("1 machine online".into()), Entry::Separator]);
        assert_eq!(entries[2], Entry::Note("Needs you".into()));
        assert!(matches!(&entries[3], Entry::Session(w) if w.id == "m/s0"));
        assert!(matches!(&entries[4], Entry::Session(w) if w.id == "m/s1"));
        assert_eq!(entries[5], Entry::Separator);
        assert_eq!(entries[6..], the_rest(None, Some("0.6.0")));
        // Then the ones that are done, under their own heading.
        let mut both = waiting(1);
        both.push(Waiting { id: "m/d".into(), label: "Done one · m".into(), needs_you: false });
        let entries = menu_entries(&state(both));
        assert_eq!(entries[4], Entry::Note("Done".into()));
        assert!(matches!(&entries[5], Entry::Session(w) if w.id == "m/d"));
    }

    #[test]
    fn a_long_list_stops_and_says_how_many_more() {
        let entries = menu_entries(&state(waiting(MOST_LISTED + 3)));
        assert_eq!(entries.iter().filter(|e| matches!(e, Entry::Session(_))).count(), MOST_LISTED);
        assert!(entries.contains(&Entry::Note("and 3 more".into())));
    }

    #[test]
    fn the_labels_say_what_they_do() {
        assert_eq!(SETTINGS_LABEL, "Settings…");
        assert_eq!(updates_label(None), "Check for Updates…");
        assert_eq!(updates_label(Some("0.9.1")), "Update to Muxin 0.9.1…", "once a look found a newer one");
        assert!(QUIT_LABEL.starts_with("Quit Muxin") && QUIT_LABEL.contains("sessions keep running"));
        let entries = menu_entries(&MenuState { newer: Some("0.9.1".into()), ..state(Vec::new()) });
        assert!(entries.contains(&Entry::Updates(Some("0.9.1".into()))));
    }

    #[test]
    fn a_newer_version_reaches_the_menu() {
        let mut seen = newer_version().subscribe();
        set_newer_version(Some("9.9.9".into()));
        assert!(seen.has_changed().unwrap());
        assert_eq!(seen.borrow_and_update().as_deref(), Some("9.9.9"));
        // The same again isn't news.
        set_newer_version(Some("9.9.9".into()));
        assert!(!seen.has_changed().unwrap());
        set_newer_version(None);
        assert_eq!(seen.borrow_and_update().as_deref(), None);
    }

    fn machine(id: &str, status: &str, local: bool, agent: Option<&str>) -> crate::fleet::model::Machine {
        crate::fleet::model::Machine {
            id: id.into(),
            name: id.into(),
            host: if local { "local".into() } else { id.into() },
            local,
            platform: "linux-x64".into(),
            kind: "other".into(),
            status: status.into(),
            checking: false,
            notes: None,
            runner_ids: Vec::new(),
            summary: Default::default(),
            probe: None,
            diagnostics: Vec::new(),
            workspaces_allowed: true,
            agents: Vec::new(),
            agent_version: agent.map(str::to_string),
            sessions_error: None,
            gh: None,
            agent_hooks: None,
            unreachable: None,
            tailnet: None,
            cloud: None,
            displays: Vec::new(),
            parent: None,
            vm: None,
            direct_address: None,
            ignored_warnings: Vec::new(),
        }
    }

    fn session(id: &str, state: &str, attention: Option<&str>) -> crate::fleet::sessions::SessionInfo {
        crate::fleet::sessions::SessionInfo {
            id: id.into(),
            machine_id: "mac-studio".into(),
            name: id.into(),
            agent: "codex".into(),
            agent_name: "Codex".into(),
            title: id.into(),
            status: "running".into(),
            state: state.into(),
            attention: attention.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn the_status_line_counts_machines_and_agents() {
        let status = |machines: Vec<crate::fleet::model::Machine>, sessions: Vec<crate::fleet::sessions::SessionInfo>| status_line(&Snapshot { machines, sessions, ..Default::default() });
        assert_eq!(status(vec![], vec![]), "No machines yet");
        assert_eq!(status(vec![machine("a", "online", true, None)], vec![]), "1 machine online");
        assert_eq!(status(vec![machine("a", "offline", false, None)], vec![]), "1 machine offline");
        let three = || vec![machine("a", "online", false, None), machine("b", "online", false, None), machine("c", "online", false, None)];
        assert_eq!(
            status(three(), vec![session("w1", "working", None), session("w2", "working", None), session("n", "needs_you", None), session("d", "done", Some("waiting"))]),
            "3 machines online · 2 working · 1 needs you"
        );
        // Asked, rang, or ended with an error: all of them wait for you.
        let mut failed = session("f", "idle", Some("exited"));
        (failed.status, failed.exit_code) = ("exited".into(), Some(1));
        assert_eq!(status(three(), vec![session("n", "needs_you", None), session("b", "idle", Some("bell")), failed]), "3 machines online · 3 need you");
        // One down shows; one left alone on purpose doesn't count.
        let mut some = three();
        some[1].status = "offline".into();
        some.push(machine("d", "disconnected", false, None));
        assert_eq!(status(some, vec![]), "2 of 3 machines online");
        assert_eq!(status(vec![machine("a", "offline", false, None), machine("b", "offline", false, None)], vec![]), "No machines online");
        assert_eq!(status(vec![machine("a", "disconnected", false, None)], vec![]), "No machines connected");
        // Before the first check.
        assert_eq!(status(vec![machine("a", "unchecked", false, None), machine("b", "offline", false, None)], vec![]), "Checking machines…");
    }

    #[test]
    fn the_versions_are_the_apps_and_this_computers_agents() {
        let snapshot = Snapshot { machines: vec![machine("beelink", "online", false, Some("0.5.0")), machine("here", "online", true, Some("0.6.0"))], ..Default::default() };
        let state = menu_state(&snapshot, None);
        assert_eq!(state.agent.as_deref(), Some("0.6.0"), "this computer's, not another machine's");
        assert_eq!(versions_line(state.agent.as_deref()), format!("Muxin {APP_VERSION} · Agent 0.6.0"));
        assert_eq!(versions_line(None), format!("Muxin {APP_VERSION}"), "this computer isn't one of the machines");
        let report = versions_report(Some("0.6.0"));
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 3, "{report}");
        assert!(lines[0].starts_with(&format!("Muxin {APP_VERSION}")));
        assert_eq!(lines[1], "Agent 0.6.0");
        assert!(lines[2].ends_with(std::env::consts::ARCH), "{report}");
        #[cfg(target_os = "macos")]
        assert!(lines[2].starts_with("macOS 1") || lines[2].starts_with("macOS 2"), "the version too: {report}");
        assert_eq!(versions_report(None).lines().nth(1), Some("Agent: unknown"));
    }

    #[test]
    fn the_tooltip_says_how_things_stand() {
        assert_eq!(tooltip(&state(Vec::new())), "Muxin · 1 machine online");
    }

    #[test]
    fn only_sessions_that_want_you_are_listed() {
        let session = |id: &str, title: &str, state: &str, attention: Option<&str>| crate::fleet::sessions::SessionInfo {
            id: id.into(),
            machine_id: "mac-studio".into(),
            name: id.into(),
            agent: "codex".into(),
            agent_name: "Codex".into(),
            title: title.into(),
            status: "running".into(),
            state: state.into(),
            attention: attention.map(str::to_string),
            ..Default::default()
        };
        let snapshot = Snapshot {
            sessions: vec![
                session("a", "fix login", "done", Some("waiting")),
                session("b", "busy", "working", None),
                session("c", " ", "idle", Some("bell")),
                session("d", "seen", "done", None),
                session("e", "asks", "needs_you", None),
            ],
            ..Default::default()
        };
        let listed = waiting_sessions(&snapshot);
        // Needs you first (asked, or rang); done until it's opened.
        assert_eq!(listed.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["c", "e", "a"]);
        assert_eq!(listed[2].label, "fix login · mac-studio", "a machine not in the list goes by its id");
        assert_eq!(listed[0].label, "Codex · mac-studio", "no title: the agent's name");
    }

    #[test]
    fn titles_are_one_short_line() {
        assert_eq!(short_title("  fix\nthe   login "), "fix the login");
        let long = short_title(&"x".repeat(200));
        assert_eq!(long.chars().count(), LONGEST_TITLE);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn the_dot_is_opaque_with_a_clear_ring() {
        let size = 44u32;
        // A fully opaque square, to see what the dot does to it.
        let mut rgba = vec![9u8; (size * size * 4) as usize];
        rgba.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
        add_dot(&mut rgba, size);
        let at = |x: u32, y: u32| &rgba[((y * size + x) * 4) as usize..][..4];
        let radius = (size as f32 * 0.23) as u32;
        assert_eq!(at(size - radius - 1, radius), [DOT[0], DOT[1], DOT[2], 255], "its center");
        assert_eq!(at(size - radius * 2 - 2, radius)[3], 0, "the ring around it");
        assert_eq!(at(2, size - 3), [9, 9, 9, 255], "the rest of the icon");
    }

    #[test]
    fn the_icon_decodes_at_each_size() {
        for size in [32, 44, 48] {
            let pixels = icon_pixels(true, size).unwrap();
            assert_eq!(pixels.len(), (size * size * 4) as usize);
        }
    }
}
