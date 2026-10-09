//! Settings › General › Command line: the `muxen` command in the PATH (see
//! crate::shell_command), so terminals and other agents can run Muxen.

use gpui::{AnyElement, Context, div, prelude::*, px};

use super::SettingsPage;
use crate::shell_command::{self, Status};
use crate::ui::kit::{Button, ButtonStyle, card, icon, section_title};
use crate::ui::theme::{Theme, mono_font};

#[derive(Default)]
pub(super) struct ShellState {
    status: Option<Status>,
    busy: bool,
    error: Option<String>,
}

impl SettingsPage {
    /// Looks at the PATH (in the background: on Windows it runs PowerShell).
    pub(super) fn load_shell(&mut self, cx: &mut Context<Self>) {
        self.run_shell(None, cx);
    }

    /// Installs (`Some(true)`), removes (`Some(false)`) or just looks.
    fn run_shell(&mut self, install: Option<bool>, cx: &mut Context<Self>) {
        if self.shell.busy {
            return;
        }
        self.shell.busy = true;
        self.shell.error = None;
        let job = self.engine.runtime().spawn_blocking(move || match install {
            Some(true) => shell_command::install(),
            Some(false) => shell_command::remove(),
            None => Ok(shell_command::status()),
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.shell.busy = false;
                match result {
                    Ok(status) => this.shell.status = Some(status),
                    Err(e) => this.shell.error = Some(e),
                }
                cx.notify();
            });
        });
        self._tasks.push(task);
        cx.notify();
    }

    pub(super) fn shell_section(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let st = &self.shell;
        let (glyph, color, line): (&str, _, String) = match &st.status {
            None => ("loader-circle", theme.text_muted, "Looking…".into()),
            Some(s) => match s.state {
                "installed" => ("circle-check", theme.success, format!("Installed: {}", s.path)),
                "missing" if cfg!(windows) => ("circle", theme.text_muted, "Not installed. Muxen's folder gets added to your PATH.".into()),
                "missing" => ("circle", theme.text_muted, format!("Not installed. It goes in {}.", s.path)),
                "other" => ("triangle-alert", theme.warning, format!("{} is already there.", s.path)),
                _ => ("circle-x", theme.error, "Not available on this computer.".into()),
            },
        };
        let note = st.status.as_ref().and_then(|s| s.note.clone()).or_else(|| {
            // Before it asks: macOS shows its own password prompt for /usr/local/bin.
            st.status.as_ref().filter(|s| s.state == "missing" && cfg!(target_os = "macos")).map(|_| "macOS may ask for your password to add it there.".to_string())
        });
        let state = st.status.as_ref().map(|s| (s.state, s.replaceable));
        let button = match state {
            Some(("missing", _)) => Some(Button::new("shell-install", "Install").icon("plus").style(ButtonStyle::Primary).loading(st.busy).on_click(cx.listener(|this, _, _, cx| this.run_shell(Some(true), cx)))),
            Some(("other", true)) => Some(Button::new("shell-replace", "Use this copy").icon("refresh-cw").loading(st.busy).on_click(cx.listener(|this, _, _, cx| this.run_shell(Some(true), cx)))),
            Some(("installed", _)) => Some(Button::new("shell-remove", "Remove").loading(st.busy).on_click(cx.listener(|this, _, _, cx| this.run_shell(Some(false), cx)))),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Command line", None))
            .child(
                card(theme)
                    .p(px(14.))
                    .gap(px(10.))
                    .child(div().w_full().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text_muted).child(
                        "Use Muxen from a terminal, and connect other agents to it with muxen mcp.",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(icon(glyph, 14., color))
                            .child(div().flex_1().min_w(px(0.)).truncate().font_family(mono_font()).text_size(px(12.5)).child(line))
                            .children(button),
                    )
                    .children(note.map(|n| div().text_size(px(12.)).text_color(theme.text_placeholder).child(n)))
                    .children(st.error.clone().map(|e| div().text_size(px(12.)).text_color(theme.error).child(e))),
            )
            .into_any_element()
    }
}
