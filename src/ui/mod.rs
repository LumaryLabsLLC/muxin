//! The native window, built with GPUI (Zed's UI framework).

mod activity;
mod assets;
mod detail;
mod dialogs;
mod format;
mod input;
mod kit;
mod overview;
mod settings;
mod theme;
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
        // UI preferences live next to the inventory; demo mode keeps them in memory.
        let prefs_path = (!engine.is_demo()).then(|| engine.paths().dir.join("ui.json"));
        let appearance = Appearance::load(prefs_path);
        cx.set_global(appearance.resolve(cx.window_appearance()));
        cx.set_global(appearance);
        input::bind_keys(cx);
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
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let titlebar = if cfg!(target_os = "macos") {
            TitlebarOptions { title: Some("OpenRunner".into()), appears_transparent: true, traffic_light_position: Some(point(px(13.), px(12.))) }
        } else {
            TitlebarOptions { title: Some("OpenRunner".into()), appears_transparent: false, traffic_light_position: None }
        };
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(titlebar),
                window_min_size: Some(size(px(960.), px(600.))),
                app_id: Some("openrunner".into()),
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
        cx.activate(true);
    });
}
