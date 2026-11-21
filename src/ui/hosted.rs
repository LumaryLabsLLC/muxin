//! Machines on other machines: on a VM's page, where it runs (and starting
//! or stopping it); on its host's page, what runs there.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

use super::format;
use super::kit::{self, Button, ButtonStyle, card, section_title};
use crate::ui::i18n::{tr, trf};
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
            "vm" => trf("Virtual machine on {name}", &[("name", &parent.name)]),
            "container" => trf("Container on {name}", &[("name", &parent.name)]),
            "through" => trf("Reached through {name}", &[("name", &parent.name)]),
            _ => trf("Runs on {name}", &[("name", &parent.name)]),
        };
        let installing = m.unreachable.as_ref().is_some_and(|u| u.kind == "installing");
        let (state, color) = match (powering, &parent.container) {
            _ if installing => (m.unreachable.as_ref().map(|u| u.title.clone()).unwrap_or_default(), theme.text_muted),
            (Some(true), _) => (tr("Starting…").to_string(), theme.text_muted),
            (Some(false), _) => (tr("Shutting down…").to_string(), theme.text_muted),
            _ if host_down => (trf("{name} is {state}", &[("name", &parent.name), ("state", &host.map(format::unreachable_word).unwrap_or("gone").to_lowercase())]), theme.warning),
            (None, Some(c)) if parent.container_state.as_deref() == Some("running") => (trf("In its container {container} · running", &[("container", c)]), theme.text_placeholder),
            (None, Some(c)) => (trf("Its container {container} is {state}", &[("container", c), ("state", &container_word(parent.container_state.as_deref()))]), theme.warning),
            (None, None) if parent.inside() => (trf("ssh reaches it through a port {name} forwards to it.", &[("name", &parent.name)]), theme.text_placeholder),
            (None, None) => (trf("ssh goes through {name} to reach it.", &[("name", &parent.name)]), theme.text_placeholder),
        };
        let power = if installing { None } else { power_button(s, m, parent, host_down, "m-vm-power", cx) };
        // A VM Muxin made: its install's screen, its password, destroying it.
        let made = m.vm.as_ref();
        let watch = made.and_then(|v| v.web_port).filter(|_| installing && !host_down).map(|port| {
            let pid = parent.machine_id.clone();
            Button::new("m-vm-watch", tr("Watch the install"))
                .icon("monitor")
                .tooltip(tr("Its screen in your browser, while Windows installs"))
                .on_click(cx.listener(move |this, _, _, cx| this.open_port(&pid, port, None, true, cx)))
                .into_any_element()
        });
        // A narrow page: the quieter buttons show only their icons, so the title keeps room.
        let narrow = self.content_width < 760.;
        let password = made.filter(|v| v.os == "windows").map(|_| {
            let mid = m.id.clone();
            if narrow { Button::icon_only("m-vm-password", "lock") } else { Button::new("m-vm-password", tr("Copy password")).icon("copy") }
                .style(ButtonStyle::Ghost)
                .tooltip(tr("Its Windows account's password (Muxin made it; it's kept in your Keychain)"))
                .on_click(cx.listener(move |this, _, _, cx| match this.engine.vm_password(&mid) {
                    Some(p) => {
                        // It stays on this computer: the shared clipboard doesn't pass it on.
                        crate::fleet::clipboard::keep_here(&p);
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(p));
                        this.notify_user(tr("Copied its password").to_string(), false, cx);
                    }
                    None => this.notify_user(tr("Muxin doesn't have its password.").to_string(), true, cx),
                }))
                .into_any_element()
        });
        let destroy = made.map(|_| {
            let machine = m.clone();
            Button::new("m-vm-destroy", tr("Destroy…"))
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
                            .overflow_hidden()
                            .gap(px(2.))
                            // Wrapped when it's long: cut, it lost the host's name.
                            .child(div().min_w(px(0.)).text_size(px(13.)).child(title))
                            .child(div().min_w(px(0.)).text_size(px(11.5)).text_color(color).child(state)),
                    )
                    .children(watch)
                    .children(power)
                    .children(password)
                    .child(
                        if narrow { Button::icon_only("m-vm-host", "external-link").tooltip(trf("Open {name}", &[("name", &parent.name)])) } else { Button::new("m-vm-host", trf("Open {name}", &[("name", &parent.name)])).icon("external-link") }
                            .style(ButtonStyle::Ghost)
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
        Some(div().flex().flex_col().gap(px(10.)).child(section_title(theme, tr("Runs here"), Some(guests.len()))).child(card(theme).children(rows)).into_any_element())
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
        Some(true) => tr("Starting…"),
        Some(false) => tr("Stopping…"),
        None if running => tr("Stop"),
        None => tr("Start"),
    };
    let mid = m.id.clone();
    Some(
        Button::new(SharedString::from(id.to_string()), label)
            .icon("power")
            .tooltip(if running { tr("Shut it down: stops its container (it gets two minutes to shut down cleanly)") } else { tr("Start its container") })
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
        Some("paused") => tr("paused"),
        Some("restarting") => tr("restarting"),
        Some("removing") => tr("being removed"),
        _ => tr("stopped"),

    }
}
