//! Machines on other machines: on a VM's page, where it runs (and starting
//! or stopping it); on its host's page, what runs there.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

use super::format;
use super::kit::{self, Button, ButtonStyle, card, section_title};
use super::root::{Route, Root};
use super::theme::{Theme, mono_font};
use crate::fleet::model::{Machine, ParentMachine, Snapshot};

impl Root {
    /// On a machine that runs on (or is reached through) another: which one,
    /// the container it's in, and starting or stopping it.
    pub(super) fn parent_card(&self, theme: &Theme, s: &Snapshot, m: &Machine, parent: &ParentMachine, cx: &mut Context<Self>) -> AnyElement {
        let host = s.machines.iter().find(|x| x.id == parent.machine_id);
        let host_down = host.is_none_or(|h| h.is_down());
        let powering = powering(s, &m.id);
        let title = match parent.relation.as_str() {
            "vm" => format!("Virtual machine on {}", parent.name),
            "container" => format!("Container on {}", parent.name),
            "through" => format!("Reached through {}", parent.name),
            _ => format!("Runs on {}", parent.name),
        };
        let installing = m.unreachable.as_ref().is_some_and(|u| u.kind == "installing");
        let (state, color) = match (powering, &parent.container) {
            _ if installing => (m.unreachable.as_ref().map(|u| u.title.clone()).unwrap_or_default(), theme.text_muted),
            (Some(true), _) => ("Starting…".to_string(), theme.text_muted),
            (Some(false), _) => ("Shutting down…".to_string(), theme.text_muted),
            _ if host_down => (format!("{} is {}", parent.name, host.map(format::unreachable_word).unwrap_or("gone").to_lowercase()), theme.warning),
            (None, Some(c)) if parent.container_state.as_deref() == Some("running") => (format!("In its container {c} · running"), theme.text_placeholder),
            (None, Some(c)) => (format!("Its container {c} is {}", container_word(parent.container_state.as_deref())), theme.warning),
            (None, None) if parent.inside() => (format!("ssh reaches it through a port {} forwards to it.", parent.name), theme.text_placeholder),
            (None, None) => (format!("ssh goes through {} to reach it.", parent.name), theme.text_placeholder),
        };
        let power = if installing { None } else { power_button(s, m, parent, host_down, "m-vm-power", cx) };
        // A VM Muxin made: its install's screen, its password, destroying it.
        let made = m.vm.as_ref();
        let watch = made.and_then(|v| v.web_port).filter(|_| installing && !host_down).map(|port| {
            let pid = parent.machine_id.clone();
            Button::new("m-vm-watch", "Watch the install")
                .icon("monitor")
                .tooltip("Its screen in your browser, while Windows installs")
                .on_click(cx.listener(move |this, _, _, cx| this.open_port(&pid, port, None, true, cx)))
                .into_any_element()
        });
        let password = made.filter(|v| v.os == "windows").map(|_| {
            let mid = m.id.clone();
            Button::new("m-vm-password", "Copy password")
                .style(ButtonStyle::Ghost)
                .icon("copy")
                .tooltip("Its Windows account's password (Muxin made it; it's kept in your Keychain)")
                .on_click(cx.listener(move |this, _, _, cx| match this.engine.vm_password(&mid) {
                    Some(p) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(p));
                        this.notify_user("Copied its password".to_string(), false, cx);
                    }
                    None => this.notify_user("Muxin doesn't have its password.".to_string(), true, cx),
                }))
                .into_any_element()
        });
        let destroy = made.map(|_| {
            let machine = m.clone();
            Button::new("m-vm-destroy", "Destroy…")
                .style(ButtonStyle::Danger)
                .on_click(cx.listener(move |this, _, window, cx| this.confirm_remove_machine(&machine, window, cx)))
                .into_any_element()
        });
        let pid = parent.machine_id.clone();
        let (from, to) = host.map(format::machine_tile).unwrap_or(kit::tiles::OVERVIEW);
        card(theme)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(12.))
                    .child(kit::tile(host.map(format::machine_icon).unwrap_or("server"), from, to, 28.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(div().truncate().text_size(px(13.)).child(title))
                            .child(div().text_size(px(11.5)).text_color(color).child(state)),
                    )
                    .children(watch)
                    .children(power)
                    .children(password)
                    .child(
                        Button::new("m-vm-host", format!("Open {}", parent.name))
                            .style(ButtonStyle::Ghost)
                            .icon("external-link")
                            .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(pid.clone()), window, cx))),
                    )
                    .children(destroy),
            )
            .into_any_element()
    }

    /// On a machine others run on (VMs, containers): those machines, to open,
    /// start or stop.
    pub(super) fn hosted_card(&self, theme: &Theme, s: &Snapshot, m: &Machine, cx: &mut Context<Self>) -> Option<AnyElement> {
        let guests: Vec<&Machine> = s.machines.iter().filter(|g| g.parent.as_ref().is_some_and(|p| p.inside() && p.machine_id == m.id)).collect();
        if guests.is_empty() {
            return None;
        }
        let host_down = m.is_down();
        let rows: Vec<AnyElement> = guests
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let parent = g.parent.as_ref().expect("filtered on parent");
                let (from, to) = format::machine_tile(g);
                let (subtitle, tone) = format::machine_subtitle_in(g, true);
                let gid = g.id.clone();
                div()
                    .id(SharedString::from(format!("m-guest-{}", g.id)))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(10.))
                    .when(i > 0, |d| d.border_t_1().border_color(theme.border_variant))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme.ghost_hover))
                    .child(
                        div().relative().flex_none().child(kit::tile(format::machine_icon(g), from, to, 28.)).child(
                            div()
                                .absolute()
                                .right(px(-4.))
                                .bottom(px(-4.))
                                .size(px(13.))
                                .rounded_full()
                                .bg(theme.surface)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(kit::dot(theme.machine_status(g), 8.)),
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(div().truncate().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child(g.name.clone()))
                            .child(div().truncate().text_size(px(11.5)).text_color(if tone == "error" { theme.error } else { theme.text_placeholder }).child(subtitle)),
                    )
                    .children(parent.container.clone().map(|c| div().flex_none().font_family(mono_font()).text_size(px(11.5)).text_color(theme.text_placeholder).child(c)))
                    .children(power_button(s, g, parent, host_down, &format!("m-guest-power-{}", g.id), cx))
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(Route::Machine(gid.clone()), window, cx)))
                    .into_any_element()
            })
            .collect();
        Some(div().flex().flex_col().gap(px(10.)).child(section_title(theme, "Runs here", Some(guests.len()))).child(card(theme).children(rows)).into_any_element())
    }

    pub(super) fn machine_power(&mut self, machine_id: &str, on: bool, cx: &mut Context<Self>) {
        match self.engine.machine_power(machine_id, on) {
            Ok(op) => self.show_operation(op, cx),
            Err(e) => self.notify_user(e, true, cx),
        }
    }
}

/// Start or Stop for a machine that runs in a container on another one
/// (None: no container to start or stop, or its host can't be reached).
fn power_button(s: &Snapshot, m: &Machine, parent: &ParentMachine, host_down: bool, id: &str, cx: &mut Context<Root>) -> Option<AnyElement> {
    parent.container.as_ref()?;
    if host_down {
        return None;
    }
    let busy = s.operations.iter().any(|o| o.status == "running" && o.machine_id.as_deref() == Some(&m.id) && o.kind != "logs");
    let running = parent.container_state.as_deref() == Some("running");
    let label = match powering(s, &m.id) {
        Some(true) => "Starting…",
        Some(false) => "Stopping…",
        None if running => "Stop",
        None => "Start",
    };
    let mid = m.id.clone();
    Some(
        Button::new(SharedString::from(id.to_string()), label)
            .icon("power")
            .tooltip(if running { "Shut it down: stops its container (it gets two minutes to shut down cleanly)" } else { "Start its container" })
            .disabled(busy)
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.machine_power(&mid, !running, cx)
            }))
            .into_any_element(),
    )
}

/// Whether it's being started (true) or stopped (false) right now.
fn powering(s: &Snapshot, machine_id: &str) -> Option<bool> {
    s.operations.iter().find(|o| o.status == "running" && o.machine_id.as_deref() == Some(machine_id)).and_then(|o| match o.kind.as_str() {
        "vm-start" => Some(true),
        "vm-stop" => Some(false),
        _ => None,
    })
}

/// Docker's word for a container that isn't running, in everyday words.
fn container_word(state: Option<&str>) -> &'static str {
    match state {
        Some("paused") => "paused",
        Some("restarting") => "restarting",
        Some("removing") => "being removed",
        _ => "stopped",
    }
}
