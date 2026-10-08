//! The About page: version, where to find us, licenses, and who built it.

use gpui::{AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Window, div, prelude::*, px, svg};

use super::dialogs::DialogEvent;
use super::kit::{Button, ButtonStyle, card};
use super::root::{Modal, Root};
use super::theme::{ActiveTheme, Theme};

const WEBSITE: &str = "https://lumarylabs.com/openrunner";
const COMPANY: &str = "https://lumarylabs.com";
const EMAIL: &str = "contact@lumarylabs.com";

/// The version, and the build (set `MUXEN_BUILD` when building releases).
fn version() -> String {
    let v = env!("CARGO_PKG_VERSION");
    format!("Version {v} ({})", option_env!("MUXEN_BUILD").unwrap_or(v))
}

/// What Muxen includes from others, and under which license.
const COMPONENTS: &[(&str, &str, &str)] = &[
    ("GPUI", "Zed Industries", "Apache-2.0"),
    ("alacritty_terminal", "Alacritty", "Apache-2.0"),
    ("portable-pty", "WezTerm", "MIT"),
    ("Geist and Geist Mono", "Vercel", "SIL Open Font License 1.1"),
    ("Lucide icons", "Lucide", "ISC"),
    ("Agent logos", "LobeHub Icons; marks belong to their owners", "MIT"),
    ("Rust crates", "See THIRD_PARTY_NOTICES.md", "MIT / Apache-2.0"),
];

impl Root {
    pub(super) fn render_about(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let header = card(&theme)
            .flex_row()
            .items_center()
            .gap(px(20.))
            .p(px(24.))
            .child(gpui::img("brand/icon-512.png").size(px(72.)).flex_none())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .flex_1()
                    .min_w(px(0.))
                    .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Muxen"))
                    .child(div().text_size(px(14.)).text_color(theme.text_muted).child(version())),
            )
            .child(Button::new("about-updates", "Check for Updates…").on_click(|_, _, cx| cx.open_url(WEBSITE)));

        let row = |i: usize, title: &'static str, detail: &'static str, action: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .px(px(20.))
                .py(px(14.))
                .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .flex_1()
                        .min_w(px(0.))
                        .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title))
                        .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(detail)),
                )
                .child(action)
        };
        let links = card(&theme)
            .child(row(0, "Website", "lumarylabs.com/openrunner", Button::new("about-web", "Open").on_click(|_, _, cx| cx.open_url(WEBSITE)).into_any_element()))
            .child(row(1, "Lumary Labs", "lumarylabs.com", Button::new("about-company", "Open").on_click(|_, _, cx| cx.open_url(COMPANY)).into_any_element()))
            .child(row(
                2,
                "Feedback",
                EMAIL,
                Button::new("about-mail", "Send Email").on_click(|_, _, cx| cx.open_url(&format!("mailto:{EMAIL}?subject=Muxen%20feedback"))).into_any_element(),
            ))
            .child(row(
                3,
                "Licenses",
                "Open-source software Muxen includes",
                Button::new("about-licenses", "View")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let dialog = cx.new(|cx| LicensesDialog { focus: cx.focus_handle() });
                        this.show_modal(Modal::Licenses(dialog.clone()), window, cx);
                        this._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_dialog_event));
                    }))
                    .into_any_element(),
            ));

        let built_by = card(&theme)
            .items_center()
            .gap(px(26.))
            .py(px(52.))
            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_placeholder).child("B U I L T    B Y"))
            .child(lumary_logo(&theme));

        div()
            .id("about-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.page_scroll)
            .flex()
            .flex_col()
            .items_center()
            .px(px(28.))
            .py(px(32.))
            .child(div().w_full().max_w(px(620.)).flex().flex_col().gap(px(16.)).child(header).child(links).child(built_by))
            .into_any_element()
    }
}

/// The Lumary Labs logo, laid out as in branding/logos/logo-large.svg: the
/// wordmark, then "LABS LLC" in light gray caps with 0.6em letter spacing
/// (drawn here, since SVG text doesn't render), sized to the wordmark.
fn lumary_logo(theme: &Theme) -> impl IntoElement {
    const WIDTH: f32 = 180.;
    const ASPECT: f32 = 3.909; // the wordmark's width / height
    let size = (WIDTH * 0.0719).round(); // 18px text on the logo's 250px wordmark
    let spacing = size * 0.6;
    let letters = "LABS LLC".chars().map(|c| {
        if c == ' ' {
            div().w(px(size * 0.28)).into_any_element()
        } else {
            div().child(c.to_string()).into_any_element()
        }
    });
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(4.))
        .child(svg().path("brand/lumary-wordmark.svg").w(px(WIDTH)).h(px(WIDTH / ASPECT)).text_color(theme.text))
        .child(
            div()
                .flex()
                .gap(px(spacing))
                // Letter spacing trails the last letter too; keep the word centered.
                .ml(px(spacing))
                .text_size(px(size))
                .line_height(px(18.))
                .font_weight(FontWeight::LIGHT)
                .text_color(theme.text_muted)
                .children(letters),
        )
}

/// The components list, in a dialog.
pub struct LicensesDialog {
    focus: FocusHandle,
}

impl EventEmitter<DialogEvent> for LicensesDialog {}

impl Focusable for LicensesDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LicensesDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = COMPONENTS.iter().enumerate().map(|(i, (name, by, license))| component_row(&theme, i, name, by, license));
        div()
            .track_focus(&self.focus)
            .w(px(500.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(18.))
            .rounded(px(10.))
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .text_color(theme.text)
            .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Licenses"))
            .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(
                "Muxen is proprietary software by Lumary Labs LLC. It includes these components under their own licenses; the full texts ship with the app.",
            ))
            .child(card(&theme).children(rows))
            .child(
                div().flex().justify_end().child(
                    Button::new("licenses-close", "Close").style(ButtonStyle::Primary).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))),
                ),
            )
    }
}

fn component_row(theme: &Theme, i: usize, name: &str, by: &str, license: &str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(14.))
        .py(px(9.))
        .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child(name.to_string()))
                .child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(by.to_string())),
        )
        .child(div().text_size(px(12.)).text_color(theme.text_muted).child(license.to_string()))
}
