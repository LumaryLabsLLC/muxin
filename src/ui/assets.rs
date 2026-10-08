//! Icons and fonts compiled into the binary, so the app is a single file.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[
            $(($name, include_bytes!(concat!("../../assets/icons/", $name, ".svg")))),*
        ];
    };
}

icons!(
    "activity", "box", "check", "chevron-down", "chevron-right", "circle", "circle-alert", "circle-check",
    "circle-x", "clock", "copy", "cpu", "ellipsis", "external-link", "folder-open", "git-branch",
    "hard-drive", "inbox", "info", "laptop", "layers", "layout-grid", "list", "loader-circle",
    "memory-stick", "minus", "monitor", "network", "panel-bottom", "play", "plus", "refresh-cw",
    "rotate-ccw", "scroll-text", "search", "server", "settings", "square", "terminal", "trash-2",
    "triangle-alert", "x", "zap", "arrow-left", "arrow-right", "gauge", "radio", "workflow",
);

const FONTS: &[&[u8]] = &[
    include_bytes!("../../assets/fonts/geist-400.ttf"),
    include_bytes!("../../assets/fonts/geist-500.ttf"),
    include_bytes!("../../assets/fonts/geist-600.ttf"),
    include_bytes!("../../assets/fonts/geist-mono-400.ttf"),
    include_bytes!("../../assets/fonts/geist-mono-500.ttf"),
];

/// The app icon, for the Dock when running outside an .app bundle.
pub const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/brand/icon-512.png");
/// Small copy of the icon for the sidebar.
const BRAND_PNG: &[u8] = include_bytes!("../../assets/brand/icon-128.png");

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == "brand/icon.png" {
            return Ok(Some(Cow::Borrowed(BRAND_PNG)));
        }
        let name = path.strip_prefix("icons/").and_then(|p| p.strip_suffix(".svg"));
        Ok(name.and_then(|n| ICONS.iter().find(|(icon, _)| *icon == n)).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(name, _)| SharedString::from(format!("icons/{name}.svg")))
            .filter(|p| p.starts_with(path))
            .collect())
    }
}

pub fn load_fonts(cx: &mut App) {
    let fonts = FONTS.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        tracing::warn!("couldn't load bundled fonts: {e}");
    }
}

/// Shows the OpenRunner icon in the Dock. Bundled builds get it from Info.plist;
/// this covers `cargo run` and `cargo install`.
#[cfg(target_os = "macos")]
pub fn set_dock_icon() {
    use cocoa::appkit::{NSApp, NSApplication, NSImage};
    use cocoa::base::nil;
    use cocoa::foundation::NSData;
    // SAFETY: called on the main thread after GPUI has created NSApplication;
    // the PNG bytes are 'static and copied into NSData.
    unsafe {
        let data = NSData::dataWithBytes_length_(nil, APP_ICON_PNG.as_ptr() as *const std::ffi::c_void, APP_ICON_PNG.len() as u64);
        let image = NSImage::initWithData_(NSImage::alloc(nil), data);
        if image != nil {
            NSApp().setApplicationIconImage_(image);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_icon() {}
