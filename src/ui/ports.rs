//! A machine's servers on this computer (see fleet/ports.rs): the machine
//! page's Servers section (what's listening there, Open for a web server),
//! a sidebar item under the machine for each port open here, and bringing
//! them back after a restart (Appearance::forwards).

use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};

use super::github::Load;
use super::kit::{self, Button, ButtonStyle, icon, spinner};
use super::root::Root;
use super::theme::Theme;
use crate::fleet::model::Machine;
use crate::fleet::ports::{Forward, ListeningPort};

/// Ports shown before "Show N more" (sites always show).
const OTHERS_SHOWN: usize = 0;

impl Root {
    /// The machine page's Servers: sites first (Open), then its other ports
    /// (Forward), and a field to open any port by number.
    pub(super) fn machine_servers(&mut self, theme: &Theme, m: &Machine, cx: &mut Context<Self>) -> AnyElement {
        if !self.machine_ports.contains_key(&m.id) {
            self.load_machine_ports(&m.id, cx);
        }
        let open: Vec<Forward> = self.engine.forwards().into_iter().filter(|f| f.machine_id == m.id).collect();
        let mid = m.id.clone();
        let (sites, others): (Vec<ListeningPort>, Vec<ListeningPort>) = match self.machine_ports.get(&m.id) {
            Some(Load::Ready(list)) => list.iter().cloned().partition(|p| p.is_site()),
            _ => (Vec::new(), Vec::new()),
        };
        let more = self.ports_more.contains(&m.id);
        let count = match self.machine_ports.get(&m.id) {
            Some(Load::Ready(list)) => Some(list.len()),
            _ => None,
        };
        let header = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(kit::section_title(theme, "Servers", count))
            .child(div().flex_1())
            .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child("Open a port"))
            .child(div().w(px(110.)).child(self.port_input.clone()))
            .child(Button::icon_only("m-ports-refresh", "refresh-cw").tooltip("Look again").on_click(cx.listener({
                let mid = mid.clone();
                move |this, _, _, cx| this.load_machine_ports(&mid, cx)
            })));
        let body: AnyElement = match self.machine_ports.get(&m.id) {
            None | Some(Load::Loading) => kit::card(theme)
                .p(px(16.))
                .flex_row()
                .items_center()
                .gap(px(10.))
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .child(spinner("m-ports-loading", 13., theme.text_muted))
                .child(format!("Looking at what's listening on {}…", m.name))
                .into_any_element(),
            Some(Load::Failed(e)) => kit::card(theme).p(px(16.)).text_size(px(12.5)).text_color(theme.error).child(e.clone()).into_any_element(),
            Some(Load::Ready(_)) => {
                let mut rows: Vec<AnyElement> = Vec::new();
                for (i, p) in sites.iter().enumerate() {
                    rows.push(self.port_row(theme, m, p, open.iter().find(|f| f.port == p.port), i, cx));
                }
                let hidden = if more { 0 } else { others.len().saturating_sub(OTHERS_SHOWN) };
                for (i, p) in others.iter().take(others.len() - hidden).enumerate() {
                    rows.push(self.port_row(theme, m, p, open.iter().find(|f| f.port == p.port), 100 + i, cx));
                }
                // Ports open here that aren't listed (opened by number, or gone quiet).
                for (i, f) in open.iter().filter(|f| !sites.iter().chain(others.iter()).any(|p| p.port == f.port)).enumerate() {
                    let p = ListeningPort { port: f.port, host: f.host.clone(), ..Default::default() };
                    rows.push(self.port_row(theme, m, &p, Some(f), 500 + i, cx));
                }
                if rows.is_empty() && hidden == 0 {
                    rows.push(div().p(px(16.)).text_size(px(12.5)).text_color(theme.text_muted).child(format!("Nothing is listening on {} right now. Start a server there and look again.", m.name)).into_any_element());
                } else if sites.is_empty() && others.len() == hidden && open.is_empty() {
                    rows.insert(0, div().px(px(16.)).py(px(12.)).text_size(px(12.5)).text_color(theme.text_muted).child("No web servers running there.").into_any_element());
                }
                let toggle = (!others.is_empty()).then(|| {
                    let mid = mid.clone();
                    div()
                        .id("m-ports-more")
                        .px(px(16.))
                        .py(px(9.))
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme.text_placeholder)
                        .hover(|st| st.text_color(theme.text_muted))
                        .child(icon(if more { "chevron-down" } else { "chevron-right" }, 11., theme.text_placeholder))
                        .child(if more { "Fewer ports".to_string() } else { format!("{} other port{}: databases, system services, apps' helpers", others.len(), if others.len() == 1 { "" } else { "s" }) })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.ports_more.remove(&mid) {
                                this.ports_more.insert(mid.clone());
                            }
                            cx.notify();
                        }))
                });
                kit::card(theme).children(rows).children(toggle).into_any_element()
            }
        };
        div().flex().flex_col().gap(px(10.)).child(header).child(body).into_any_element()
    }

    /// One port: what it is, where it listens, and Open (a site), Forward,
    /// or its address here and Close.
    fn port_row(&self, theme: &Theme, m: &Machine, p: &ListeningPort, open: Option<&Forward>, n: usize, cx: &mut Context<Self>) -> AnyElement {
        let site = p.is_site();
        // Open here, but nothing listens there (yet): it waits for a server.
        let waiting = p.process.is_empty() && p.title.is_none() && p.container.is_none() && open.is_some() && p.address.is_empty();
        let what = if waiting { "Nothing's listening there yet".to_string() } else { p.name() };
        let mut detail: Vec<String> = Vec::new();
        if let Some(c) = &p.container
            && p.title.is_some()
        {
            detail.push(format!("container {c}"));
        }
        if !p.process.is_empty() && (p.title.is_some() || p.container.is_some()) {
            detail.push(p.process.clone());
        }
        if let Some(s) = p.status.filter(|_| p.web) {
            detail.push(format!("HTTP {s}"));
        }
        if p.local_only() {
            detail.push(format!("only on {} itself", m.name));
        }
        let (mid, port, host) = (m.id.clone(), p.port, p.host.clone());
        let action: AnyElement = match open {
            Some(f) => {
                let url = f.url();
                let (mid2, url2) = (mid.clone(), url.clone());
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .id(SharedString::from(format!("m-port-url-{n}")))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .px(px(8.))
                            .h(px(24.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(theme.text_accent)
                            .hover(|st| st.bg(theme.ghost_hover))
                            .tooltip(move |window, cx| kit::Tooltip::view(format!("Open {url2} in your browser").into(), window, cx))
                            .child(format!("localhost:{}", f.local_port))
                            .child(icon("external-link", 11., theme.text_accent))
                            .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                    )
                    .child(Button::new(SharedString::from(format!("m-port-close-{n}")), "Close").style(ButtonStyle::Ghost).tooltip("Stop opening it here").on_click(cx.listener(move |this, _, _, cx| this.close_port(&mid2, port, cx))))
                    .into_any_element()
            }
            None if site => Button::new(SharedString::from(format!("m-port-open-{n}")), "Open")
                .icon("external-link")
                .tooltip("Open it in your browser here, at localhost")
                .on_click(cx.listener(move |this, _, _, cx| this.open_port(&mid, port, Some(host.clone()), true, cx)))
                .into_any_element(),
            None => Button::new(SharedString::from(format!("m-port-fwd-{n}")), "Forward")
                .style(ButtonStyle::Ghost)
                .tooltip("Reach it here, at localhost")
                .on_click(cx.listener(move |this, _, _, cx| this.open_port(&mid, port, Some(host.clone()), false, cx)))
                .into_any_element(),
        };
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(16.))
            .py(px(10.))
            .border_b_1()
            .border_color(theme.border_variant)
            .child(kit::dot(if site { theme.success } else { theme.text_placeholder.opacity(0.6) }, 7.))
            .child(div().flex_none().w(px(64.)).font_family(super::theme::mono_font()).text_size(px(13.)).text_color(if site { theme.text } else { theme.text_muted }).child(format!(":{}", p.port)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(13.)).text_color(if site { theme.text } else { theme.text_muted }).overflow_hidden().whitespace_nowrap().child(what))
                    .when(!detail.is_empty(), |d| d.child(div().text_size(px(11.5)).text_color(theme.text_placeholder).overflow_hidden().whitespace_nowrap().child(detail.join(" · ")))),
            )
            .child(action)
            .into_any_element()
    }

    pub(super) fn load_machine_ports(&mut self, machine_id: &str, cx: &mut Context<Self>) {
        // A look in flight keeps what was shown until it's back.
        self.machine_ports.entry(machine_id.to_string()).or_insert(Load::Loading);
        let (engine, id) = (self.engine.clone(), machine_id.to_string());
        let job = engine.runtime().spawn({
            let (engine, id) = (engine.clone(), id.clone());
            async move { engine.ports(&id).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.machine_ports.insert(id, match result {
                    Ok(list) => Load::Ready(list),
                    Err(e) => Load::Failed(e),
                });
                cx.notify()
            });
        });
        self.track(task);
    }

    /// Opens a machine's port here (localhost), remembered for next time;
    /// `browse` opens it in the browser too.
    pub(super) fn open_port(&mut self, machine_id: &str, port: u16, host: Option<String>, browse: bool, cx: &mut Context<Self>) {
        let (engine, id) = (self.engine.clone(), machine_id.to_string());
        let job = engine.runtime().spawn({
            let (engine, id) = (engine.clone(), id.clone());
            async move { engine.forward(&id, port, host.as_deref(), None).await }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(f) => {
                        let entry = format!("{}:{}:{}", f.machine_id, f.port, f.local_port);
                        super::theme::remember(cx, |a| {
                            a.forwards.retain(|x| !x.starts_with(&format!("{}:{}:", f.machine_id, f.port)));
                            a.forwards.push(entry);
                        });
                        if browse {
                            cx.open_url(&f.url());
                        }
                    }
                    Err(e) => this.notify_user(e, true, cx),
                }
                cx.notify()
            });
        });
        self.track(task);
    }

    /// Stops opening a machine's port here (and forgets it).
    pub(super) fn close_port(&mut self, machine_id: &str, port: u16, cx: &mut Context<Self>) {
        self.engine.stop_forward(machine_id, port);
        let prefix = format!("{machine_id}:{port}:");
        super::theme::remember(cx, |a| a.forwards.retain(|x| !x.starts_with(&prefix)));
        cx.notify();
    }

    /// A quick look at what listens on each machine with ports open here (and
    /// the one whose page is shown): a forward whose server was running and
    /// stopped closes, and the page's Servers is looked at again when
    /// something started or stopped.
    pub(super) fn watch_servers(&mut self, cx: &mut Context<Self>) {
        let forwards = self.engine.forwards();
        let mut machines: Vec<String> = forwards.iter().map(|f| f.machine_id.clone()).collect();
        if let super::root::Route::Machine(id) = &self.route {
            machines.push(id.clone());
        }
        machines.sort();
        machines.dedup();
        for id in machines {
            // Only machines we can reach (an offline one says nothing about its servers).
            let Some(m) = self.snapshot.machines.iter().find(|m| m.id == id) else { continue };
            if m.is_down() || m.local || m.status != "online" {
                continue;
            }
            let name = m.name.clone();
            let (engine, mid) = (self.engine.clone(), id.clone());
            let job = engine.runtime().spawn({
                let (engine, mid) = (engine.clone(), mid.clone());
                async move { engine.listening(&mid).await }
            });
            let task = cx.spawn(async move |this, cx| {
                let Ok(Ok(listening)) = job.await else { return };
                let _ = this.update(cx, |this, cx| {
                    for f in this.engine.forwards().into_iter().filter(|f| f.machine_id == mid) {
                        let key = (mid.clone(), f.port);
                        if listening.contains(&f.port) {
                            this.forward_seen.insert(key);
                        } else if this.forward_seen.remove(&key) {
                            this.close_port(&mid, f.port, cx);
                            this.notify_user(format!("The server on {name}'s port {} stopped, so localhost:{} is closed.", f.port, f.local_port), false, cx);
                        }
                    }
                    // The page shown: look again when its servers changed.
                    let shown = matches!(&this.route, super::root::Route::Machine(r) if *r == mid);
                    if shown && let Some(Load::Ready(list)) = this.machine_ports.get(&mid) {
                        let mut had: Vec<u16> = list.iter().map(|p| p.port).collect();
                        had.sort_unstable();
                        let mut now = listening.clone();
                        now.sort_unstable();
                        if had != now {
                            this.load_machine_ports(&mid, cx);
                        }
                    }
                    cx.notify();
                });
            });
            self.track(task);
        }
    }

    /// The ports open here last time, open again (at the same local ports when free).
    pub(super) fn restore_forwards(&mut self, cx: &mut Context<Self>) {
        let saved = cx.global::<super::theme::Appearance>().forwards.clone();
        for entry in saved {
            let mut parts = entry.split(':');
            let (Some(machine), Some(port), Some(local)) = (parts.next(), parts.next().and_then(|p| p.parse::<u16>().ok()), parts.next().and_then(|p| p.parse::<u16>().ok())) else { continue };
            // It was open before: if its server's gone now, it closes.
            self.forward_seen.insert((machine.to_string(), port));
            let (engine, machine) = (self.engine.clone(), machine.to_string());
            let job = engine.runtime().spawn({
                let engine = engine.clone();
                async move { engine.forward(&machine, port, None, Some(local)).await }
            });
            let task = cx.spawn(async move |this, cx| {
                let _ = job.await;
                let _ = this.update(cx, |_, cx| cx.notify());
            });
            self.track(task);
        }
    }

    /// A port open here, under its machine in the sidebar: click opens it in
    /// the browser, × closes it.
    pub(super) fn forward_row(&self, theme: &Theme, m: &Machine, f: &Forward, cx: &mut Context<Self>) -> AnyElement {
        let (from, to) = super::format::machine_tile(m);
        let group = SharedString::from(format!("fwd-row-{}-{}", m.id, f.port));
        let name = match self.machine_ports.get(&m.id) {
            Some(Load::Ready(list)) => list.iter().find(|p| p.port == f.port).filter(|p| p.title.is_some() || p.container.is_some()).map(|p| p.name()),
            _ => None,
        };
        let (url, mid, port) = (f.url(), m.id.clone(), f.port);
        let tip = format!("{}'s port {} at localhost:{}: click to open it in your browser", m.name, f.port, f.local_port);
        let w = (self.sidebar_width - 20. - 30. - 16. - 16. - 110. - 16.).max(30.);
        div()
            .id(SharedString::from(format!("side-fwd-{}-{}", m.id, f.port)))
            .group(group.clone())
            .relative()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .mx(px(10.))
            .pl(px(22.))
            .pr(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|st| st.bg(theme.ghost_hover))
            .tooltip(move |window, cx| kit::Tooltip::view(tip.clone().into(), window, cx))
            .child(kit::tile("external-link", from, to, 16.))
            .child(div().flex_none().text_size(px(12.5)).text_color(theme.text_muted).child(format!("localhost:{}", f.local_port)))
            .child(div().flex_1())
            .children(name.map(|n| {
                let hover = group.clone();
                div().w(px(w)).flex().justify_end().group_hover(hover, |st| st.invisible()).child(div().max_w(px(w)).truncate().text_size(px(11.)).text_color(theme.text_placeholder).child(n))
            }))
            .child(
                div()
                    .id(SharedString::from(format!("side-fwd-x-{}-{}", m.id, f.port)))
                    .absolute()
                    .right(px(8.))
                    .invisible()
                    .group_hover(group, |st| st.visible())
                    .tooltip(move |window, cx| kit::Tooltip::view("Close it here".into(), window, cx))
                    .child(icon("x", 12., theme.text_placeholder))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_port(&mid, port, cx)
                    })),
            )
            .on_click(move |_, _, cx| cx.open_url(&url))
            .into_any_element()
    }
}
