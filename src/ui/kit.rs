//! Small building blocks in Zed's visual language: flat surfaces, hairline
//! borders, ghost buttons, monospace numbers.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyView, App, ClickEvent, Div, ElementId, FontWeight, Hsla, IntoElement,
    ParentElement, RenderOnce, SharedString, Styled, Svg, Transformation, Window, div, percentage, prelude::*, px,
    svg,
};

use super::theme::{ActiveTheme, mono_font, Theme};

pub fn icon(name: &str, size: f32, color: Hsla) -> Svg {
    svg().path(SharedString::from(format!("icons/{name}.svg"))).size(px(size)).flex_none().text_color(color)
}

/// How icons look across the app (Settings › Appearance › Icons). Each style
/// draws the same icons from its own set: on a solid tile in white, on a
/// faint tile in color, or on their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IconStyle {
    /// Solid tile, white filled glyph (Phosphor).
    Tiles,
    /// Faint tile, colored two-tone glyph (Solar).
    Soft,
    /// No tile, colored two-tone glyph (Solar).
    Duotone,
    /// Solid tile, white rounded glyph (MingCute).
    Rounded,
    /// Faint tile, colored filled glyph (Fluent).
    Fluent,
    /// No tile, gray line glyph (Hugeicons); no colored icons anywhere.
    #[default]
    Minimal,
}

#[derive(Clone, Copy, PartialEq)]
enum Backdrop {
    Solid,
    Faint,
    None,
}

impl IconStyle {
    pub const ALL: [IconStyle; 6] = [IconStyle::Minimal, IconStyle::Tiles, IconStyle::Soft, IconStyle::Duotone, IconStyle::Rounded, IconStyle::Fluent];

    /// As saved in config.toml.
    pub fn id(self) -> &'static str {
        match self {
            IconStyle::Tiles => "tiles",
            IconStyle::Soft => "soft",
            IconStyle::Duotone => "duotone",
            IconStyle::Rounded => "rounded",
            IconStyle::Fluent => "fluent",
            IconStyle::Minimal => "minimal",
        }
    }

    pub fn from_id(id: &str) -> IconStyle {
        IconStyle::ALL.into_iter().find(|s| s.id() == id).unwrap_or_default()
    }

    pub fn label(self) -> &'static str {
        match self {
            IconStyle::Tiles => "Tiles",
            IconStyle::Soft => "Soft",
            IconStyle::Duotone => "Duotone",
            IconStyle::Rounded => "Rounded",
            IconStyle::Fluent => "Fluent",
            IconStyle::Minimal => "Minimal",
        }
    }

    /// The prefix of its glyph files (assets/icons/<prefix>-<name>.svg).
    fn prefix(self) -> &'static str {
        match self {
            IconStyle::Tiles => "pf",
            IconStyle::Soft | IconStyle::Duotone => "so",
            IconStyle::Rounded => "mc",
            IconStyle::Fluent => "fl",
            IconStyle::Minimal => "hg",
        }
    }

    fn backdrop(self) -> Backdrop {
        match self {
            IconStyle::Tiles | IconStyle::Rounded => Backdrop::Solid,
            IconStyle::Soft | IconStyle::Fluent => Backdrop::Faint,
            IconStyle::Duotone | IconStyle::Minimal => Backdrop::None,
        }
    }

    /// Icons have their own colors (all but Minimal).
    pub fn colored(self) -> bool {
        self != IconStyle::Minimal
    }
}

/// The style in use, with the theme colors it draws with.
#[derive(Clone, Copy)]
pub struct Look {
    pub style: IconStyle,
    text: Hsla,
    muted: Hsla,
    dark: bool,
}

impl Look {
    pub fn new(style: IconStyle, theme: &Theme) -> Look {
        Look { style, text: theme.text, muted: theme.text_muted, dark: theme.canvas.l < 0.5 }
    }
}

static LOOK: std::sync::RwLock<Option<Look>> = std::sync::RwLock::new(None);

/// Sets the style and colors icons are drawn with (when the theme or the
/// preference changes; see theme::apply).
pub fn set_look(look: Look) {
    if let Ok(mut l) = LOOK.write() {
        *l = Some(look);
    }
}

fn look() -> Look {
    LOOK.read().ok().and_then(|l| *l).unwrap_or(Look {
        style: IconStyle::default(),
        text: gpui::white(),
        muted: gpui::hsla(0., 0., 0.62, 1.),
        dark: true,
    })
}

pub fn icon_style() -> IconStyle {
    look().style
}

/// The icons each style has its own glyph for (assets/icons/<prefix>-<name>.svg).
const STYLED: &[&str] = &[
    "gauge", "workflow", "terminal", "activity", "server", "laptop", "monitor", "hard-drive", "folder", "folder-open", "file", "copy",
    "refresh-cw", "rotate-ccw", "play", "square", "trash-2", "power", "unplug", "plug", "zap", "external-link", "git-branch", "arrow-up",
    "box", "layers", "lock", "info", "inbox", "clock", "bot", "scroll-text", "circle-check", "circle-x", "triangle-alert", "circle-alert",
    "star", "message-square", "settings", "search", "cpu", "memory-stick", "book-open", "network", "plus", "x", "check", "agent-github",
];

/// The glyph a style draws for an icon; logos and others are used as they are.
fn glyph(style: IconStyle, name: &str) -> String {
    let name = match name {
        "square-terminal" => "terminal",
        "star-filled" => "star",
        other => other,
    };
    if STYLED.contains(&name) { format!("{}-{name}", style.prefix()) } else { name.to_string() }
}

/// An icon on its tile, in the style in use. `from`/`to` are its color pair
/// (0xRRGGBB, see `tiles` and `icon_colors`); `size` is the tile's.
pub fn tile(icon_name: &str, from: u32, to: u32, size: f32) -> Div {
    tile_in(look(), icon_name, from, to, size)
}

/// `tile`, in a given look (Settings shows each style this way).
pub fn tile_in(look: Look, icon_name: &str, from: u32, to: u32, size: f32) -> Div {
    let glyph = glyph(look.style, icon_name);
    // In color on the page, a pair's lighter color reads on dark themes and
    // its deeper one on light themes; near-black brand colors use the text color.
    let tone = if look.dark { from } else { to };
    let tone: Hsla = if luminance(tone) < 0.22 { look.text } else { gpui::rgb(tone).into() };
    let base: Hsla = gpui::rgb(mix(from, to)).into();
    let el = div().flex_none().size(px(size)).flex().items_center().justify_center();
    match look.style.backdrop() {
        Backdrop::Solid => el.rounded(px(size * 0.27)).bg(base).child(icon(&glyph, size * 0.58, gpui::white())),
        Backdrop::Faint => el.rounded(px(size * 0.27)).bg(base.opacity(0.2)).child(icon(&glyph, size * 0.62, tone)),
        // A provider's logo keeps its color even in Minimal, so a droplet reads as one at a glance.
        Backdrop::None => el.child(icon(&glyph, size * 0.86, if look.style.colored() || icon_name.starts_with("brand-") { tone } else { look.muted })),
    }
}

/// The color halfway between two 0xRRGGBB colors.
fn mix(a: u32, b: u32) -> u32 {
    let ch = |shift: u32| (((a >> shift) & 0xff) + ((b >> shift) & 0xff)) / 2 << shift;
    ch(16) | ch(8) | ch(0)
}

/// Relative brightness of a 0xRRGGBB color, 0 to 1.
fn luminance(c: u32) -> f32 {
    let ch = |shift: u32| ((c >> shift) & 0xff) as f32 / 255.;
    0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
}

/// Tile colors per navigation destination.
pub mod tiles {
    pub const OVERVIEW: (u32, u32) = (0x9d8cff, 0x5b47e0);
    pub const JOBS: (u32, u32) = (0xffbf55, 0xff7a3d);
    pub const WORKSPACES: (u32, u32) = (0x4be3b9, 0x14a083);
    pub const ACTIVITY: (u32, u32) = (0xff7aa8, 0xe0457b);
    pub const USAGE: (u32, u32) = (0x6ee7a0, 0x1c9e55);
    pub const RUN: (u32, u32) = (0xffd84d, 0xe8a10c);
    pub const MACOS: (u32, u32) = (0x6cb6ff, 0x2f6fe4);
    pub const LINUX: (u32, u32) = (0xffb35c, 0xf0703f);
    pub const WINDOWS: (u32, u32) = (0x5ad2f4, 0x1f8fd6);
    pub const OTHER: (u32, u32) = (0xa7aebb, 0x5f6673);
    /// DigitalOcean's blue, for droplets.
    pub const DIGITALOCEAN: (u32, u32) = (0x4d9bff, 0x0069ff);
}

/// An icon's colors, by what it stands for, so a thing looks the same in
/// menus, buttons and rows. `None` for small controls (chevrons, close) that
/// stay quiet.
pub fn icon_colors(name: &str) -> Option<(u32, u32)> {
    Some(match name {
        "terminal" | "square-terminal" => (0x8f95ff, 0x5057e0),
        "monitor" | "laptop" | "server" | "maximize" | "minimize" | "panel-left" | "panel-right" | "panel-bottom" | "layout-grid" => {
            (0x6cc8ff, 0x2f7ae4)
        }
        "folder" | "folder-open" | "hard-drive" | "box" => (0xffd36b, 0xf09a0b),
        "file" | "settings" | "gauge" | "cpu" | "memory-stick" | "search" => (0xa3b4d9, 0x5a6d99),
        "copy" => (0xf0a6fc, 0xb83ad0),
        "pencil" => (0xffc861, 0xe5900f),
        "refresh-cw" | "rotate-ccw" | "play" | "check" | "circle-check" => (0x6ee7a0, 0x1c9e55),
        "plus" => (0xb69cff, 0x7a4ce8),
        "trash-2" | "circle-x" | "square" => (0xff8080, 0xdc3b33),
        "power" | "unplug" => (0xff8fb1, 0xd63c6e),
        "external-link" | "info" | "inbox" | "message-square" | "arrow-up" | "arrow-down" | "cloud" => (0x6cb6ff, 0x2f6fe4),
        "git-branch" | "git-merge" | "git-pull-request" | "git-pull-request-draft" | "workflow" | "list" | "scroll-text" => {
            (0xffa95c, 0xea6a2a)
        }
        "layers" => tiles::WORKSPACES,
        "activity" | "clock" => tiles::ACTIVITY,
        "zap" | "star" | "star-filled" | "circle-alert" | "triangle-alert" => (0xffcc4d, 0xf08c00),
        "network" | "plug" | "radio" => (0x5ee7f0, 0x0d9cbd),
        "lock" | "book-open" => tiles::OVERVIEW,
        "bot" => (0xf472d0, 0xb33fd0),
        "agent-github" => (0x4a4a50, 0x18181b),
        _ => return None,
    })
}

/// `icon` on its colored tile (see `icon_colors`).
pub fn color_tile(name: &str, size: f32) -> Div {
    let (from, to) = icon_colors(name).unwrap_or(tiles::OTHER);
    tile(name, from, to, size)
}

/// An icon's own color, readable on this theme: the lighter stop on dark
/// themes, the deeper one on light themes. Logos keep the text color, and
/// the Minimal style has none.
pub fn tint(theme: &Theme, name: &str) -> Option<Hsla> {
    // Minimal: no colored icons, only status colors.
    if name.starts_with("agent-") || !icon_style().colored() {
        return None;
    }
    let (from, to) = icon_colors(name)?;
    Some(gpui::rgb(if theme.canvas.l < 0.5 { from } else { to }).into())
}

/// `icon` in its own color, or in `fallback` when it has none.
pub fn color_icon(theme: &Theme, name: &str, size: f32, fallback: Hsla) -> Svg {
    icon(name, size, tint(theme, name).unwrap_or(fallback))
}

/// Logo and tile colors for an agent CLI (see fleet::sessions::AGENTS): the
/// product's own mark, white on its brand color.
pub fn agent_look(agent: &str) -> (&'static str, (u32, u32)) {
    match agent {
        "claude" => ("agent-claude", (0xe58b6d, 0xc4603c)),
        "codex" => ("agent-codex", (0x3d3d42, 0x111114)),
        "opencode" => ("agent-opencode", (0x34363c, 0x16171a)),
        "gemini" => ("agent-gemini", (0x4796e3, 0x9a6fd0)),
        "amp" => ("agent-amp", (0xf2704f, 0xc9352b)),
        "pi" => ("agent-pi", (0x5aa6cf, 0x3a7fa6)),
        "shell" => ("terminal", (0x8f95ff, 0x5057e0)),
        _ => ("bot", (0xa7aebb, 0x5f6673)),
    }
}

pub fn spinner(id: impl Into<ElementId>, size: f32, color: Hsla) -> impl IntoElement {
    icon("loader-circle", size, color).with_animation(
        id,
        Animation::new(Duration::from_millis(900)).repeat(),
        |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
    )
}

pub fn mono(text: impl Into<SharedString>) -> Div {
    div().font_family(mono_font()).child(text.into())
}

pub fn dot(color: Hsla, size: f32) -> Div {
    div().flex_none().size(px(size)).rounded_full().bg(color)
}

/// A runner as a small square: filled when connected, dashed outline when not.
pub fn runner_glyph(theme: &Theme, state: &str, size: f32) -> Div {
    let base = div().flex_none().size(px(size)).rounded(px(2.5));
    match state {
        "working" | "idle" => base.bg(theme.state(state)),
        _ => base.border_1().border_dashed().border_color(theme.text_placeholder),
    }
}

pub fn pill(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(6.))
        .h(px(18.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .bg(color.opacity(0.14))
        .text_color(color)
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .child(text.into())
}

pub fn chip(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .px(px(5.))
        .h(px(18.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(theme.border_variant)
        .bg(theme.element)
        .text_color(theme.text_muted)
        .text_size(px(11.))
        .font_family(mono_font())
        .child(text.into())
}

/// Shortcut labels are written Mac-style ("⇧⌘R"); elsewhere they read "Ctrl+Shift+R".
pub fn keys(text: &str) -> String {
    if cfg!(target_os = "macos") {
        text.to_string()
    } else {
        text.replace("⇧⌘", "Ctrl+Shift+").replace('⌘', "Ctrl+")
    }
}

pub fn kbd(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .px(px(4.))
        .h(px(16.))
        .flex()
        .items_center()
        .rounded(px(3.))
        .border_1()
        .border_color(theme.border_variant)
        .text_color(theme.text_placeholder)
        .text_size(px(10.5))
        .font_family(mono_font())
        .child(keys(&text.into()))
}

/// A thin horizontal meter. `fraction` is clamped to 0..=1.
pub fn meter(theme: &Theme, fraction: f32, color: Hsla) -> Div {
    let f = fraction.clamp(0.0, 1.0);
    div()
        .w_full()
        .h(px(3.))
        .rounded_full()
        .bg(theme.track)
        .child(div().h_full().rounded_full().bg(color).w(gpui::relative(f.max(if f > 0.0 { 0.02 } else { 0.0 }))))
}

/// A bordered surface, the basic container for page sections.
pub fn card(theme: &Theme) -> Div {
    div().flex().flex_col().rounded(px(8.)).border_1().border_color(theme.border_variant).bg(theme.surface)
}

pub fn section_title(theme: &Theme, title: impl Into<SharedString>, count: Option<usize>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child(title.into())
        .children(count.map(|n| div().font_family(mono_font()).text_color(theme.text_placeholder).font_weight(FontWeight::NORMAL).child(n.to_string())))
}

pub fn label(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().text_size(px(11.)).text_color(theme.text_placeholder).child(text.into())
}

pub fn empty_state(theme: &Theme, icon_name: &str, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(8.))
        .px(px(12.))
        .py(px(14.))
        .text_size(px(12.5))
        .text_color(theme.text_placeholder)
        // Icon box matches the first line's height so long text wraps under itself.
        .child(div().flex_none().h(px(20.)).flex().items_center().child(color_icon(theme, icon_name, 14., theme.text_placeholder)))
        .child(div().flex_1().min_w(px(0.)).child(text.into()))
}

/// A two-column key/value line for detail panes.
pub fn kv(theme: &Theme, key: impl Into<SharedString>, value: impl IntoElement) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(12.))
        .py(px(5.))
        .text_size(px(12.5))
        .child(div().w(px(112.)).flex_none().text_color(theme.text_placeholder).child(key.into()))
        .child(div().flex_1().min_w(px(0.)).text_color(theme.text).child(value))
}

// ---------------------------------------------------------------------------
// Buttons

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// Filled with the accent color.
    Primary,
    /// Bordered element background.
    Outlined,
    /// No background until hovered.
    Ghost,
    /// Ghost, tinted with the error color.
    Danger,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<&'static str>,
    style: ButtonStyle,
    disabled: bool,
    loading: bool,
    key_hint: Option<SharedString>,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Button {
            id: id.into(),
            label: Some(label.into()),
            icon: None,
            style: ButtonStyle::Outlined,
            disabled: false,
            loading: false,
            key_hint: None,
            tooltip: None,
            on_click: None,
        }
    }

    pub fn icon_only(id: impl Into<ElementId>, icon: &'static str) -> Self {
        Button { label: None, icon: Some(icon), style: ButtonStyle::Ghost, ..Button::new(id, "") }
    }

    pub fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    pub fn key_hint(mut self, hint: impl Into<SharedString>) -> Self {
        self.key_hint = Some(keys(&hint.into()).into());
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    pub fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let icon_only = self.label.is_none();
        let (bg, hover, fg, border) = match self.style {
            ButtonStyle::Primary => (theme.accent, theme.accent.opacity(0.85), theme.accent_text, None),
            ButtonStyle::Outlined => (theme.element, theme.element_hover, theme.text, Some(theme.border_variant)),
            ButtonStyle::Ghost => (gpui::transparent_black(), theme.ghost_hover, theme.text_muted, None),
            ButtonStyle::Danger => (gpui::transparent_black(), theme.error.opacity(0.14), theme.error, None),
        };
        let disabled = self.disabled || self.loading;
        let mut el = div()
            .id(self.id.clone())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap(px(5.))
            .h(px(26.))
            .when(icon_only, |d| d.w(px(26.)))
            .when(!icon_only, |d| d.px(px(9.)))
            .rounded(px(6.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .when_some(border, |d, b| d.border_1().border_color(b))
            .when(disabled, |d| d.opacity(0.5))
            .when(!disabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover)));
        if self.loading {
            el = el.child(spinner(ElementId::Name(format!("{}-spin", self.id).into()), 13., fg));
        } else if let Some(i) = self.icon {
            // Icons carry their own color, except on filled and danger buttons.
            let color = match self.style {
                ButtonStyle::Outlined | ButtonStyle::Ghost => tint(&theme, i).unwrap_or(fg),
                ButtonStyle::Primary | ButtonStyle::Danger => fg,
            };
            el = el.child(icon(i, if icon_only { 15. } else { 13. }, color));
        }
        if let Some(label) = self.label {
            el = el.child(label);
        }
        if let Some(hint) = self.key_hint {
            el = el.child(
                div()
                    .text_size(px(10.5))
                    .font_family(mono_font())
                    .text_color(if self.style == ButtonStyle::Primary { fg.opacity(0.75) } else { theme.text_placeholder })
                    .child(hint),
            );
        }
        if let Some(tip) = self.tooltip {
            el = el.tooltip(move |window, cx| Tooltip::view(tip.clone(), window, cx));
        }
        // A press is the button's alone: it doesn't drag the window or select a row behind it.
        el = el.on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if let (Some(handler), false) = (self.on_click, disabled) {
            el = el.on_click(move |e, w, cx| {
                cx.stop_propagation();
                handler(e, w, cx)
            });
        }
        el
    }
}

pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    pub fn view(text: SharedString, _: &mut Window, cx: &mut App) -> AnyView {
        let text = keys(&text).into();
        cx.new(|_| Tooltip { text }).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .pl(px(8.))
            .pt(px(10.))
            .child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .bg(theme.elevated)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_md()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(self.text.clone()),
            )
    }
}

/// A checkbox row; `on_toggle` receives the new value.
pub fn checkbox(
    theme: &Theme,
    id: impl Into<ElementId>,
    checked: bool,
    text: impl Into<SharedString>,
    on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .text_size(px(12.5))
        .text_color(theme.text)
        .child(
            div()
                .size(px(15.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .border_1()
                .border_color(if checked { theme.accent } else { theme.border })
                .bg(if checked { theme.accent } else { theme.canvas })
                .when(checked, |d| d.child(icon("check", 11., theme.accent_text))),
        )
        .child(text.into())
        .on_click(move |_, w, cx| on_toggle(&!checked, w, cx))
}

/// Items in explicit rows of `per_row`. GPUI measures a wrapping flex row
/// inside an auto-width column as one long line, so what follows overlaps it.
pub fn rows_of(items: Vec<gpui::AnyElement>, per_row: usize, gap: f32) -> Div {
    let mut rows = Vec::new();
    let mut iter = items.into_iter().peekable();
    while iter.peek().is_some() {
        let row: Vec<gpui::AnyElement> = iter.by_ref().take(per_row.max(1)).collect();
        rows.push(div().flex().gap(px(gap)).children(row));
    }
    div().flex().flex_col().gap(px(gap)).children(rows)
}
