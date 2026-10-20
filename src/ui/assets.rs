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
    "bot", "square-terminal", "power", "plug", "unplug",
    "eye", "link", "users", "user-x", "user-check",
    "agent-claude", "agent-codex", "agent-gemini", "agent-opencode", "agent-amp", "agent-pi", "agent-github",
    "circle-dot", "git-pull-request", "git-pull-request-draft", "git-merge", "message-square", "lock", "star",
    "arrow-down", "arrow-up", "book-open", "star-filled", "maximize", "minimize", "panel-right", "panel-left", "columns-2", "rows-2", "pause", "circle-stop",
    "folder", "file", "sliders-horizontal", "palette", "keyboard", "cloud", "brand-digitalocean", "pencil", "code", "corner-down-right",
    // The glyphs of each icon style (see kit::IconStyle): pf- Phosphor (MIT),
    // so- Solar (CC BY 4.0), mc- MingCute (Apache-2.0), fl- Fluent (MIT),
    // hg- Hugeicons (MIT). Generated from the Iconify sets; one file per icon.
    "fl-activity", "fl-agent-github", "fl-arrow-up", "fl-book-open", "fl-bot", "fl-box",
    "fl-check", "fl-circle-alert", "fl-circle-check", "fl-circle-x", "fl-clock", "fl-copy",
    "fl-cpu", "fl-external-link", "fl-file", "fl-folder", "fl-folder-open", "fl-gauge",
    "fl-git-branch", "fl-hard-drive", "fl-inbox", "fl-info", "fl-laptop", "fl-layers",
    "fl-lock", "fl-memory-stick", "fl-message-square", "fl-monitor", "fl-network", "fl-play",
    "fl-plug", "fl-plus", "fl-power", "fl-refresh-cw", "fl-rotate-ccw", "fl-scroll-text",
    "fl-search", "fl-server", "fl-settings", "fl-square", "fl-star", "fl-terminal",
    "fl-trash-2", "fl-triangle-alert", "fl-unplug", "fl-workflow", "fl-x", "fl-zap",
    "hg-activity", "hg-agent-github", "hg-arrow-up", "hg-book-open", "hg-bot", "hg-box",
    "hg-check", "hg-circle-alert", "hg-circle-check", "hg-circle-x", "hg-clock", "hg-copy",
    "hg-cpu", "hg-external-link", "hg-file", "hg-folder", "hg-folder-open", "hg-gauge",
    "hg-git-branch", "hg-hard-drive", "hg-inbox", "hg-info", "hg-laptop", "hg-layers",
    "hg-lock", "hg-memory-stick", "hg-message-square", "hg-monitor", "hg-network", "hg-play",
    "hg-plug", "hg-plus", "hg-power", "hg-refresh-cw", "hg-rotate-ccw", "hg-scroll-text",
    "hg-search", "hg-server", "hg-settings", "hg-square", "hg-star", "hg-terminal",
    "hg-trash-2", "hg-triangle-alert", "hg-unplug", "hg-workflow", "hg-x", "hg-zap",
    "mc-activity", "mc-agent-github", "mc-arrow-up", "mc-book-open", "mc-bot", "mc-box",
    "mc-check", "mc-circle-alert", "mc-circle-check", "mc-circle-x", "mc-clock", "mc-copy",
    "mc-cpu", "mc-external-link", "mc-file", "mc-folder", "mc-folder-open", "mc-gauge",
    "mc-git-branch", "mc-hard-drive", "mc-inbox", "mc-info", "mc-laptop", "mc-layers",
    "mc-lock", "mc-memory-stick", "mc-message-square", "mc-monitor", "mc-network", "mc-play",
    "mc-plug", "mc-plus", "mc-power", "mc-refresh-cw", "mc-rotate-ccw", "mc-scroll-text",
    "mc-search", "mc-server", "mc-settings", "mc-square", "mc-star", "mc-terminal",
    "mc-trash-2", "mc-triangle-alert", "mc-unplug", "mc-workflow", "mc-x", "mc-zap",
    "pf-activity", "pf-agent-github", "pf-arrow-up", "pf-book-open", "pf-bot", "pf-box",
    "pf-check", "pf-circle-alert", "pf-circle-check", "pf-circle-x", "pf-clock", "pf-copy",
    "pf-cpu", "pf-external-link", "pf-file", "pf-folder", "pf-folder-open", "pf-gauge",
    "pf-git-branch", "pf-hard-drive", "pf-inbox", "pf-info", "pf-laptop", "pf-layers",
    "pf-lock", "pf-memory-stick", "pf-message-square", "pf-monitor", "pf-network", "pf-play",
    "pf-plug", "pf-plus", "pf-power", "pf-refresh-cw", "pf-rotate-ccw", "pf-scroll-text",
    "pf-search", "pf-server", "pf-settings", "pf-square", "pf-star", "pf-terminal",
    "pf-trash-2", "pf-triangle-alert", "pf-unplug", "pf-workflow", "pf-x", "pf-zap",
    "so-activity", "so-agent-github", "so-arrow-up", "so-book-open", "so-bot", "so-box",
    "so-check", "so-circle-alert", "so-circle-check", "so-circle-x", "so-clock", "so-copy",
    "so-cpu", "so-external-link", "so-file", "so-folder", "so-folder-open", "so-gauge",
    "so-git-branch", "so-hard-drive", "so-inbox", "so-info", "so-laptop", "so-layers",
    "so-lock", "so-memory-stick", "so-message-square", "so-monitor", "so-network", "so-play",
    "so-plug", "so-plus", "so-power", "so-refresh-cw", "so-rotate-ccw", "so-scroll-text",
    "so-search", "so-server", "so-settings", "so-square", "so-star", "so-terminal",
    "so-trash-2", "so-triangle-alert", "so-unplug", "so-workflow", "so-x", "so-zap",
);

const FONTS: &[&[u8]] = &[
    include_bytes!("../../assets/fonts/geist-400.ttf"),
    include_bytes!("../../assets/fonts/geist-500.ttf"),
    include_bytes!("../../assets/fonts/geist-600.ttf"),
    include_bytes!("../../assets/fonts/geist-mono-400.ttf"),
    include_bytes!("../../assets/fonts/geist-mono-500.ttf"),
    NERD_SYMBOLS,
];

/// Nerd Font icons (Neovim's file icons, prompt glyphs), drawn when the
/// terminal font doesn't have them: Symbols Nerd Font Mono (MIT, see
/// NERD-FONTS-LICENSE.txt), one cell wide each.
const NERD_SYMBOLS: &[u8] = include_bytes!("../../assets/fonts/symbols-nerd-font-mono.ttf");
/// Its names, for the terminal font's fallbacks: PostScript (what macOS
/// looks fallbacks up by), then family.
pub const NERD_SYMBOLS_NAMES: [&str; 2] = ["SymbolsNFM", "Symbols Nerd Font Mono"];

/// The app icon, for the Dock when running outside an .app bundle.
pub const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/brand/icon-512.png");
/// Small copy of the icon for the sidebar.
const BRAND_PNG: &[u8] = include_bytes!("../../assets/brand/icon-128.png");
/// The Lumary Labs wordmark, for the About page.
const LUMARY_SVG: &[u8] = include_bytes!("../../assets/brand/lumary-wordmark.svg");

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "brand/icon.png" => return Ok(Some(Cow::Borrowed(BRAND_PNG))),
            "brand/icon-512.png" => return Ok(Some(Cow::Borrowed(APP_ICON_PNG))),
            "brand/lumary-wordmark.svg" => return Ok(Some(Cow::Borrowed(LUMARY_SVG))),
            _ => {}
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
    #[cfg(target_os = "macos")]
    register_with_core_text(NERD_SYMBOLS);
}

/// Makes a bundled font known to macOS by name, for this app only: font
/// fallbacks are looked up that way, and GPUI's own fonts aren't.
#[cfg(target_os = "macos")]
fn register_with_core_text(bytes: &'static [u8]) {
    use std::ffi::c_void;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGDataProviderCreateWithData(info: *mut c_void, data: *const c_void, size: usize, release: *const c_void) -> *const c_void;
        fn CGDataProviderRelease(provider: *const c_void);
        fn CGFontCreateWithDataProvider(provider: *const c_void) -> *const c_void;
        fn CGFontRelease(font: *const c_void);
    }
    #[link(name = "CoreText", kind = "framework")]
    unsafe extern "C" {
        fn CTFontManagerRegisterGraphicsFont(font: *const c_void, error: *mut *const c_void) -> bool;
    }
    // SAFETY: `bytes` lives for the whole program; each object made here is released here.
    unsafe {
        let provider = CGDataProviderCreateWithData(std::ptr::null_mut(), bytes.as_ptr().cast(), bytes.len(), std::ptr::null());
        if provider.is_null() {
            return;
        }
        let font = CGFontCreateWithDataProvider(provider);
        CGDataProviderRelease(provider);
        if font.is_null() {
            return;
        }
        let mut error = std::ptr::null();
        if !CTFontManagerRegisterGraphicsFont(font, &mut error) {
            tracing::warn!("couldn't register the Nerd Font symbols");
        }
        CGFontRelease(font);
    }
}

/// Shows the Muxin icon in the Dock. Bundled builds get it from Info.plist;
/// this covers `cargo run` and `cargo install`.
#[cfg(target_os = "macos")]
pub fn set_dock_icon() {
    use cocoa::appkit::{NSApp, NSApplication, NSImage};
    use cocoa::base::nil;
    use cocoa::foundation::NSData;
    // A bundled app keeps the icon macOS draws from its .icns (on macOS 26 a
    // set image would stand apart from the Dock's and Spotlight's look); only
    // a bare binary (cargo run) needs one.
    let bundled = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).is_some_and(|e| e.to_string_lossy().contains(".app/Contents/MacOS/"));
    if bundled {
        return;
    }
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
