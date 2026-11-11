//! The sidebar's Agents section: every agent session on every machine, the
//! ones that need you first, to switch between them and see how each is
//! doing. It sits under the machines and "Shared with you" in a pane of its
//! own, and Settings › General › Agents turns it off. The order and the tree's
//! lines are fleet::sessions::list's.

use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*, px};

use super::kit;
use super::root::{Root, Route};
use super::sessions::mark_view;
use super::theme::{ActiveTheme, Appearance, Theme};
use crate::fleet::sessions::list::{self, Lines};
use crate::fleet::sessions::{Mark, SessionInfo};

/// A session's row is as tall as a machine's, its agent's logo as big; a
/// helper's is like the rows under a machine.
const ROW_H: f32 = 44.;
const TILE: f32 = 30.;
const HELPER_H: f32 = 26.;
const HELPER_TILE: f32 = 16.;
/// How far each level of helpers sits right of the one above.
const STEP: f32 = 20.;

/// Where the line of a row at `depth` runs: down the middle of its icon
/// (rows have a margin of 10; a session's row, a padding of 8).
fn line_x(depth: usize) -> f32 {
    10. + 8. + TILE / 2. + STEP * depth as f32
}

/// A helper row's left padding, which puts its icon on its line.
fn helper_pad(depth: usize) -> f32 {
    line_x(depth) - 10. - HELPER_TILE / 2.
}

impl Root {
    /// The Agents section, or nothing when it's turned off or no machine has
    /// an agent session (or none matches the sidebar's search).
    pub(super) fn agents_section(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !cx.global::<Appearance>().sidebar_agents {
            return None;
        }
        let theme = cx.theme();
        let s = self.snapshot.clone();
        let q = self.filter_query(cx);
        let name = |id: &str| s.machines.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.to_string());
        // Hidden ones aren't listed, unless a search finds them.
        let sessions: Vec<_> = s.sessions.iter().filter(|x| !q.is_empty() || !self.is_hidden(x, cx)).cloned().collect();
        let rows = list::agents(&sessions, &q, name, |id| s.machines.iter().any(|m| m.id == id && m.is_down()));
        if rows.is_empty() {
            return None;
        }
        let selected = match &self.route {
            Route::Session(id) => Some(id.as_str()),
            _ => None,
        };
        let lines = list::lines(&rows);
        let items: Vec<AnyElement> = rows
            .iter()
            .zip(&lines)
            .map(|((_, x), l)| {
                let row = self.agent_row(&theme, x, l, selected == Some(x.id.as_str()), cx);
                with_lines(&theme, row, l)
            })
            .collect();
        // Its own scrolling, up to a share of the window: it stays in view
        // however long the machines above it get.
        let max_h = (f32::from(window.viewport_size().height) * 0.4).max(ROW_H * 3.);
        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .child(self.section_header("Agents", Some(rows.len()), None, cx))
                .child(div().id("agent-list").max_h(px(max_h)).overflow_y_scroll().pb(px(10.)).children(items))
                .into_any_element(),
        )
    }

    /// One agent session: its logo, title, machine and state. A helper is a
    /// smaller row under the session that started it.
    fn agent_row(&self, theme: &Theme, x: &SessionInfo, l: &Lines, selected: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = x.id.clone();
        let (icon_name, (from, to)) = kit::agent_look(&x.agent);
        let machine = self.snapshot.machines.iter().find(|m| m.id == x.machine_id);
        let down = machine.is_some_and(|m| m.is_down());
        let exited = x.status == "exited";
        // What it said last is old news while its machine is down.
        let mark = if down { Mark::Idle } else { x.mark() };
        // While it's open, only working shows (it's in front of you).
        let shown = if selected && mark != Mark::Working { Mark::Idle } else { mark };
        let state = if down {
            if machine.is_some_and(|m| m.is_disconnected()) { "disconnected" } else { "offline" }.to_string()
        } else if exited && shown == Mark::Idle {
            match x.exit_code {
                Some(c) if c != 0 => format!("exited ({c})"),
                _ => "exited".to_string(),
            }
        } else {
            shown.label().to_string()
        };
        let state_color = match shown {
            _ if down || exited => theme.text_placeholder,
            Mark::Working => theme.text_accent,
            Mark::Idle => theme.text_placeholder,
            _ => theme.text,
        };
        let dim = down || exited;
        let top = l.depth == 0;
        let trailing = if shown != Mark::Idle { 18. } else { 0. };
        let row = div()
            .id(SharedString::from(format!("side-a-{}", x.id)))
            .flex()
            .items_center()
            .h(px(if top { ROW_H } else { HELPER_H }))
            .mx(px(10.))
            .pr(px(8.))
            .rounded(kit::corner(if top { 9. } else { 7. }))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.element_selected))
            .when(!selected, |d| d.hover(|st| st.bg(theme.ghost_hover)));
        let row = if top {
            // Explicit widths: GPUI 0.2 only ellipsizes text whose width it knows up front.
            let text_w = (self.row_width() - 20. - 16. - TILE - 10. - trailing).max(40.);
            let where_ = machine.map(|m| m.name.clone()).unwrap_or_else(|| x.machine_id.clone());
            row.gap(px(10.)).pl(px(8.)).child(kit::tile(icon_name, from, to, TILE)).child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .gap(px(1.))
                    .child(div().w(px(text_w)).truncate().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(if dim { theme.text_placeholder } else { theme.text }).child(x.title.clone()))
                    .child(
                        div()
                            .w(px(text_w))
                            .flex()
                            .gap(px(5.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(px(11.5))
                            .text_color(theme.text_placeholder)
                            .child(div().flex_shrink().min_w(px(0.)).overflow_hidden().child(where_))
                            .child("·")
                            .child(div().flex_none().text_color(state_color).child(state)),
                    ),
            )
        } else {
            // A helper on another machine than its session says which.
            let elsewhere = x.parent.as_deref().and_then(|p| p.split_once('/')).is_some_and(|(pm, _)| pm != x.machine_id);
            let words = if down || exited { Some(state) } else { elsewhere.then(|| machine.map(|m| m.name.clone()).unwrap_or_else(|| x.machine_id.clone())) };
            let words_w = words.as_ref().map_or(0., |w| w.chars().count() as f32 * 6.5 + 8.);
            let text_w = (self.row_width() - 20. - helper_pad(l.depth) - HELPER_TILE - 8. - 8. - trailing - words_w).max(40.);
            row.gap(px(8.))
                .pl(px(helper_pad(l.depth)))
                .child(kit::tile(icon_name, from, to, HELPER_TILE))
                .child(div().w(px(text_w)).truncate().text_size(px(12.5)).text_color(if dim { theme.text_placeholder } else if selected { theme.text } else { theme.text_muted }).child(x.title.clone()))
                .child(div().flex_1())
                .children(words.map(|w| div().flex_none().text_size(px(11.)).text_color(theme.text_placeholder).child(w)))
        };
        row
            // Working, needs you, done (until it's opened), exited with an error.
            .children(mark_view(theme, format!("side-a-spin-{}", x.id), shown, 8.))
            .on_mouse_down(gpui::MouseButton::Right, {
                let sid = x.id.clone();
                super::menu::on_right_click(move || super::menu::MenuTarget::Session(sid.clone()), cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Session(id.clone()), window, cx)))
            .into_any_element()
    }
}

/// A row with the tree's lines: from its session's icon into it (and on to
/// the next helper), from its own icon down to its helpers, and the lines of
/// sessions further up passing by on its left.
fn with_lines(theme: &Theme, row: AnyElement, l: &Lines) -> AnyElement {
    if l.depth == 0 && !l.kids {
        return row;
    }
    let color = theme.text_placeholder.opacity(0.45);
    let (h, tile) = if l.depth == 0 { (ROW_H, TILE) } else { (HELPER_H, HELPER_TILE) };
    let mid = h / 2.;
    let line = |x: f32, top: f32, len: f32| div().absolute().left(px(x)).top(px(top)).w(px(1.)).h(px(len)).bg(color);
    div()
        .relative()
        .child(row)
        .children(l.rails.iter().map(|level| line(line_x(*level), 0., h).into_any_element()))
        .when(l.depth > 0, |d| {
            let x = line_x(l.depth - 1);
            d.child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(x))
                    .w(px(line_x(l.depth) - HELPER_TILE / 2. - 2. - x))
                    .h(px(mid))
                    .border_l_1()
                    .border_b_1()
                    .rounded_bl(kit::corner(5.))
                    .border_color(color),
            )
            // More helpers of its session below: the line goes on past it.
            .when(!l.last, |d| d.child(line(x, mid, h - mid)))
        })
        .when(l.kids, |d| {
            let from = (h + tile) / 2. + 2.;
            d.child(line(line_x(l.depth), from, h - from))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_section_is_on_unless_turned_off() {
        assert!(Appearance::default().sidebar_agents);
        // A config from before it: on.
        let old: Appearance = toml::from_str("mode = \"dark\"\nsidebar_width = 300.0\n").unwrap();
        assert!(old.sidebar_agents);
        let off: Appearance = toml::from_str("sidebar_agents = false\n").unwrap();
        assert!(!off.sidebar_agents);
        // It's saved with the rest of [ui].
        let a = Appearance { sidebar_agents: false, ..Default::default() };
        let back: Appearance = toml::from_str(&toml::to_string(&a).unwrap()).unwrap();
        assert!(!back.sidebar_agents);
    }

    #[test]
    fn helpers_hang_off_the_middle_of_the_icon_above() {
        // A session's logo is centred on its line; each helper's on the next one.
        assert_eq!(line_x(0), 10. + 8. + TILE / 2.);
        for depth in 1..4 {
            assert_eq!(10. + helper_pad(depth) + HELPER_TILE / 2., line_x(depth));
            // The elbow reaches from the line above to just before the helper's icon.
            assert!(10. + helper_pad(depth) - line_x(depth - 1) >= 8.);
        }
    }
}
