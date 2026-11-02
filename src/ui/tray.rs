//! The icon in the menu bar or the system tray: the way back to a closed window,
//! with a dot while a session is waiting for you (never a notification from the
//! system), and those sessions in its menu.
//!
//! On a Mac and Windows it's `tray-icon`, on the main thread. On Linux it's a
//! StatusNotifierItem over D-Bus, and outlives the app's windows (see `ui::run`).
//! A desktop with nothing to show those (GNOME without its AppIndicator
//! extension) shows no icon: `available` says so, and closing the window quits.

use std::sync::Arc;

use gpui::App;

use super::{AppCommand, command};
use crate::fleet::Engine;
use crate::fleet::model::Snapshot;

/// The menu's list stops here: more than this is a reason to open the window.
const MOST_LISTED: usize = 8;
const LONGEST_TITLE: usize = 40;

/// The waiting dot: black on a Mac, where the menu bar tints the icon.
const DOT: [u8; 3] = if cfg!(target_os = "macos") { [0, 0, 0] } else { [0xF5, 0xA6, 0x23] };

#[cfg(target_os = "macos")]
const ICON: &[u8] = include_bytes!("../../assets/brand/menu-bar.png");
#[cfg(not(target_os = "macos"))]
const ICON: &[u8] = include_bytes!("../../assets/brand/icon-128.png");

/// A session waiting for you, for the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Waiting {
    id: String,
    label: String,
}

fn waiting_sessions(snapshot: &Snapshot) -> Vec<Waiting> {
    snapshot
        .sessions
        .iter()
        .filter(|s| s.attention.is_some())
        .map(|s| {
            let machine = snapshot.machines.iter().find(|m| m.id == s.machine_id).map_or(s.machine_id.as_str(), |m| m.name.as_str());
            let title = if s.title.trim().is_empty() { &s.agent_name } else { &s.title };
            Waiting { id: s.id.clone(), label: format!("{} · {machine}", short_title(title)) }
        })
        .collect()
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

/// A line of the menu, whichever system draws it.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Note(String),
    Session(Waiting),
    Separator,
    Show,
    Quit,
}

fn menu_entries(waiting: &[Waiting]) -> Vec<Entry> {
    let mut entries = Vec::new();
    if !waiting.is_empty() {
        entries.push(Entry::Note("Waiting for you".into()));
        entries.extend(waiting.iter().take(MOST_LISTED).cloned().map(Entry::Session));
        if waiting.len() > MOST_LISTED {
            entries.push(Entry::Note(format!("and {} more", waiting.len() - MOST_LISTED)));
        }
        entries.push(Entry::Separator);
    }
    entries.extend([Entry::Show, Entry::Quit]);
    entries
}

const SHOW_LABEL: &str = "Show Muxin";
const QUIT_LABEL: &str = "Quit Muxin";

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

    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::*;

    const ICON_SIZE: u32 = if cfg!(target_os = "macos") { 44 } else { 32 };

    thread_local! {
        /// The icon, and the task keeping it up to date. Main thread only.
        static TRAY: RefCell<Option<(TrayIcon, gpui::Task<()>)>> = const { RefCell::new(None) };
    }

    pub fn available() -> bool {
        TRAY.with(|t| t.borrow().is_some())
    }

    pub fn start(engine: Arc<Engine>, cx: &mut App) {
        if available() {
            return;
        }
        MenuEvent::set_event_handler(Some(|event: MenuEvent| match event.id().as_ref() {
            "show" => command(AppCommand::Show),
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
        let Some(plain_icon) = icon(false) else { return };
        let builder = TrayIconBuilder::new().with_tooltip("Muxin").with_menu_on_left_click(cfg!(target_os = "macos")).with_menu(Box::new(menu(&[])));
        // Tinted by the menu bar, and kept where it was dragged.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(plain_icon).with_autosave_name("muxin");
        #[cfg(windows)]
        let builder = builder.with_icon(plain_icon);
        let tray = match builder.build() {
            Ok(tray) => tray,
            Err(e) => {
                tracing::warn!("couldn't add the tray icon: {e}");
                return;
            }
        };
        let follow = cx.spawn(async move |_| {
            let mut snapshots = engine.subscribe();
            let mut shown = Vec::new();
            loop {
                let waiting = waiting_sessions(&snapshots.borrow_and_update());
                if waiting != shown {
                    show_waiting(&waiting, !shown.is_empty());
                    shown = waiting;
                }
                if snapshots.changed().await.is_err() {
                    break;
                }
            }
        });
        TRAY.with(|t| *t.borrow_mut() = Some((tray, follow)));
    }

    /// Takes the icon away. (On Windows one left at exit stays until it's hovered.)
    pub fn stop() {
        TRAY.with(|t| drop(t.borrow_mut().take()));
    }

    fn show_waiting(waiting: &[Waiting], had_dot: bool) {
        TRAY.with(|t| {
            let t = t.borrow();
            let Some((tray, _)) = t.as_ref() else { return };
            tray.set_menu(Some(Box::new(menu(waiting))));
            if had_dot != !waiting.is_empty() {
                set_icon(tray, icon(!waiting.is_empty()));
            }
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

    fn menu(waiting: &[Waiting]) -> Menu {
        let menu = Menu::new();
        for entry in menu_entries(waiting) {
            let _ = match entry {
                Entry::Note(text) => menu.append(&MenuItem::new(plain(&text), false, None)),
                Entry::Session(w) => menu.append(&MenuItem::with_id(format!("session:{}", w.id), plain(&w.label), true, None)),
                Entry::Separator => menu.append(&PredefinedMenuItem::separator()),
                Entry::Show => menu.append(&MenuItem::with_id("show", SHOW_LABEL, true, None)),
                Entry::Quit => menu.append(&MenuItem::with_id("quit", QUIT_LABEL, true, None)),
            };
        }
        menu
    }

    /// A "&" in a menu's text would underline the next letter.
    fn plain(text: &str) -> String {
        text.replace('&', "&&")
    }
}

#[cfg(target_os = "linux")]
mod imp {
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
            let tray = LinuxTray { waiting: Vec::new(), plain: pixmap(false), dotted: pixmap(true) };
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
            let mut snapshots = engine.subscribe();
            let mut shown = Vec::new();
            while !handle.is_closed() {
                let waiting = waiting_sessions(&snapshots.borrow_and_update());
                if waiting != shown {
                    let next = waiting.clone();
                    handle.update(move |tray| tray.waiting = next).await;
                    shown = waiting;
                }
                if snapshots.changed().await.is_err() {
                    break;
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

    struct LinuxTray {
        waiting: Vec<Waiting>,
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
            if self.waiting.is_empty() { self.plain.clone() } else { self.dotted.clone() }
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

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::{MenuItem, StandardItem};
            let item = |label: &str, run: Box<dyn Fn(&mut Self) + Send>| StandardItem { label: plain(label), activate: run, ..Default::default() }.into();
            menu_entries(&self.waiting)
                .into_iter()
                .map(|entry| match entry {
                    Entry::Note(text) => StandardItem { label: plain(&text), enabled: false, ..Default::default() }.into(),
                    Entry::Session(w) => {
                        let id = w.id.clone();
                        item(&w.label, Box::new(move |_| command(AppCommand::OpenSession(id.clone()))))
                    }
                    Entry::Separator => MenuItem::Separator,
                    Entry::Show => item(SHOW_LABEL, Box::new(|_| command(AppCommand::Show))),
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
        (0..n).map(|i| Waiting { id: format!("m/s{i}"), label: format!("Session {i} · m") }).collect()
    }

    #[test]
    fn nothing_waiting_is_just_show_and_quit() {
        assert_eq!(menu_entries(&[]), vec![Entry::Show, Entry::Quit]);
    }

    #[test]
    fn waiting_sessions_come_first() {
        let entries = menu_entries(&waiting(2));
        assert_eq!(entries[0], Entry::Note("Waiting for you".into()));
        assert!(matches!(&entries[1], Entry::Session(w) if w.id == "m/s0"));
        assert!(matches!(&entries[2], Entry::Session(w) if w.id == "m/s1"));
        assert_eq!(entries[3..], [Entry::Separator, Entry::Show, Entry::Quit]);
    }

    #[test]
    fn a_long_list_stops_and_says_how_many_more() {
        let entries = menu_entries(&waiting(MOST_LISTED + 3));
        assert_eq!(entries.iter().filter(|e| matches!(e, Entry::Session(_))).count(), MOST_LISTED);
        assert!(entries.contains(&Entry::Note("and 3 more".into())));
    }

    #[test]
    fn only_sessions_that_want_you_are_listed() {
        let session = |id: &str, title: &str, attention: Option<&str>| {
            crate::fleet::sessions::SessionInfo {
                id: id.into(),
                machine_id: "mac-studio".into(),
                name: id.into(),
                agent: "codex".into(),
                agent_name: "Codex".into(),
                title: title.into(),
                cwd: "/".into(),
                status: "running".into(),
                exit_code: None,
                busy: false,
                attention: attention.map(str::to_string),
                clients: 0,
                created_at: None,
                last_output_at: None,
                attach_command: String::new(),
                persistent: true,
                git: None,
                box_id: None,
            }
        };
        let snapshot = Snapshot { sessions: vec![session("a", "fix login", Some("waiting")), session("b", "busy", None), session("c", " ", Some("bell"))], ..Default::default() };
        let listed = waiting_sessions(&snapshot);
        assert_eq!(listed.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(listed[0].label, "fix login · mac-studio", "a machine not in the list goes by its id");
        assert_eq!(listed[1].label, "Codex · mac-studio", "no title: the agent's name");
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
