//! A Compose project as a picture (the Compose tab, a project opened): its
//! services as boxes, laid out by what waits for what. What's reached first
//! (the web front) is at the left, what it waits for to its right (the API,
//! then the database), an arrow for each wait. The named volumes they keep
//! data in sit at the far right; networks are colored tags. Each box has the
//! image, state and health, what it uses, the ports it opens on the machine
//! and the machine's files it reads.

use std::collections::HashMap;
use std::cell::Cell;
use std::rc::Rc;

use gpui::{AnyElement, App, FontWeight, Hsla, PathBuilder, SharedString, Window, canvas, div, hsla, point, prelude::*, px};

use super::kit::{self, Button, icon};
use super::services::{cpu, memory};
use super::theme::Theme;
use crate::fleet::services::{ComposeGraph, ComposeMount, ComposeService};

/// What a box's buttons ask for: a service's logs (by its name), a shell in
/// a container (by the container's).
pub enum NodeAction {
    Logs(String),
    Shell(String),
    /// A port it publishes, opened in the browser here (localhost).
    OpenPort(u16),
}

pub type OnNode = Rc<dyn Fn(NodeAction, &mut Window, &mut App)>;

/// CPU and memory by container name (from the Containers list).
pub type Usage = HashMap<String, (Option<f64>, Option<u64>)>;

const NODE_W: f32 = 272.;
/// How narrow boxes and the gaps between columns get, to fit the page.
const NODE_MIN_W: f32 = 218.;
const GAP_MIN: f32 = 72.;
const VOL_W: f32 = 196.;
const VOL_MIN_W: f32 = 168.;
const VOL_H: f32 = 50.;
const COL_GAP: f32 = 104.;
const ROW_GAP: f32 = 30.;
const PAD: f32 = 28.;
/// Folders and files listed in a box before "+N more".
const MOUNTS_SHOWN: usize = 3;

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Spot {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Spot {
    fn cy(&self) -> f32 {
        self.y + self.h / 2.
    }

    fn right(&self) -> f32 {
        self.x + self.w
    }

    fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// What a box lists: the machine's folders and files it reads, and its named
/// volumes too when they're not boxes of their own (`inline`: no room).
fn mounts(s: &ComposeService, inline: bool) -> Vec<&ComposeMount> {
    let named = s.volumes.iter().filter(|m| inline && m.kind == "volume" && !m.source.is_empty());
    named.chain(s.volumes.iter().filter(|m| m.kind == "bind")).collect()
}

/// Its networks, when they say something (not just Docker's default one).
fn shows_networks(s: &ComposeService) -> bool {
    !s.network_mode.is_empty() || !(s.networks.is_empty() || s.networks == ["default"])
}

/// A box's height: what it has to show.
fn height(s: &ComposeService, inline: bool) -> f32 {
    let mut h = 12. + 22. + 6. + 16. + 6. + 16. + 12.;
    if !s.ports.is_empty() {
        h += 6. + 20.;
    }
    if shows_networks(s) {
        h += 6. + 20.;
    }
    let b = mounts(s, inline).len();
    if b > 0 {
        h += 6. + 16. * if b > MOUNTS_SHOWN { MOUNTS_SHOWN + 1 } else { b } as f32;
    }
    h
}

struct Layout {
    services: Vec<Spot>,
    /// Named volumes: the name and its spot.
    volumes: Vec<(String, Spot)>,
    w: f32,
    h: f32,
    /// The gap between columns.
    gap: f32,
    /// Named volumes listed in their services' boxes (no room for a column of them).
    inline: bool,
}

/// Puts items in a column, each as near as it can to where it wants to be
/// (the middle of what it's joined to), top to bottom without overlapping.
fn stack(mut items: Vec<(usize, Option<f32>, f32)>) -> Vec<(usize, f32)> {
    items.sort_by(|a, b| a.1.unwrap_or(f32::MIN).total_cmp(&b.1.unwrap_or(f32::MIN)));
    let mut next = PAD;
    items
        .into_iter()
        .map(|(i, want, h)| {
            let top = want.map(|c| c - h / 2.).unwrap_or(next).max(next);
            next = top + h + ROW_GAP;
            (i, top)
        })
        .collect()
}

/// Lays the picture out in `avail` pixels of width (0: as wide as it likes):
/// narrower gaps first, then narrower boxes; centered when there's room.
fn layout(g: &ComposeGraph, avail: f32) -> Layout {
    let n = g.services.len();
    let index: HashMap<&str, usize> = g.services.iter().enumerate().map(|(i, s)| (s.name.as_str(), i)).collect();
    // Who waits for each service.
    let mut dependents = vec![Vec::new(); n];
    for (i, s) in g.services.iter().enumerate() {
        for d in &s.depends_on {
            if let Some(&j) = index.get(d.name.as_str())
                && j != i
            {
                dependents[j].push(i);
            }
        }
    }
    // A service's column: 0 when nothing waits for it, else one past the
    // furthest one that does.
    fn column(i: usize, dependents: &[Vec<usize>], memo: &mut [Option<usize>], depth: usize) -> usize {
        if let Some(c) = memo[i] {
            return c;
        }
        if depth > dependents.len() {
            return 0; // a loop (Compose refuses those anyway)
        }
        let c = dependents[i].iter().map(|&d| column(d, dependents, memo, depth + 1) + 1).max().unwrap_or(0);
        memo[i] = Some(c);
        c
    }
    let mut memo = vec![None; n];
    let cols: Vec<usize> = (0..n).map(|i| column(i, &dependents, &mut memo, 0)).collect();
    let ncols = cols.iter().copied().max().map(|c| c + 1).unwrap_or(0);
    let named = g.services.iter().any(|s| s.volumes.iter().any(|m| m.kind == "volume" && !m.source.is_empty()));
    // No room for a column of volumes even at its narrowest: they go in the boxes.
    let narrowest = 2. * PAD + ncols as f32 * NODE_MIN_W + ncols as f32 * GAP_MIN + VOL_MIN_W;
    let inline = named && avail > 0. && narrowest > avail;
    let has_volumes = named && !inline;
    let (mut node_w, mut gap, mut vol_w) = (NODE_W, COL_GAP, if has_volumes { VOL_W } else { 0. });
    let gaps = (ncols + usize::from(has_volumes)).saturating_sub(1) as f32;
    let wide = |node_w: f32, gap: f32, vol_w: f32| 2. * PAD + ncols as f32 * node_w + gaps * gap + vol_w;
    if avail > 0. && wide(node_w, gap, vol_w) > avail {
        if gaps > 0. {
            gap = ((avail - wide(node_w, 0., vol_w)) / gaps).clamp(GAP_MIN, COL_GAP);
        }
        if has_volumes {
            vol_w = (vol_w - (wide(node_w, gap, vol_w) - avail)).clamp(VOL_MIN_W, VOL_W);
        }
        if ncols > 0 {
            node_w = (node_w - (wide(node_w, gap, vol_w) - avail).max(0.) / ncols as f32).clamp(NODE_MIN_W, NODE_W);
        }
    }
    let left = if avail > 0. { ((avail - wide(node_w, gap, vol_w)) / 2.).max(0.) } else { 0. };
    let col_x = |c: usize| left + PAD + c as f32 * (node_w + gap);
    let mut spots = vec![Spot::default(); n];
    for c in 0..ncols {
        let items = (0..n)
            .filter(|&i| cols[i] == c)
            .map(|i| {
                let ys: Vec<f32> = dependents[i].iter().map(|&d| spots[d].cy()).collect();
                let want = (!ys.is_empty()).then(|| ys.iter().sum::<f32>() / ys.len() as f32);
                (i, want, height(&g.services[i], inline))
            })
            .collect();
        for (i, top) in stack(items) {
            spots[i] = Spot { x: col_x(c), y: top, w: node_w, h: height(&g.services[i], inline) };
        }
    }
    // Named volumes, at the right, beside the services that use them.
    let mut users: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, s) in g.services.iter().enumerate() {
        for m in s.volumes.iter().filter(|m| has_volumes && m.kind == "volume" && !m.source.is_empty()) {
            match users.iter_mut().find(|(name, _)| *name == m.source) {
                Some((_, list)) => list.push(i),
                None => users.push((m.source.clone(), vec![i])),
            }
        }
    }
    let vol_x = col_x(ncols);
    let items = users
        .iter()
        .enumerate()
        .map(|(k, (_, list))| {
            let ys: Vec<f32> = list.iter().map(|&i| spots[i].cy()).collect();
            (k, Some(ys.iter().sum::<f32>() / ys.len() as f32), VOL_H)
        })
        .collect();
    let mut volumes: Vec<(String, Spot)> = stack(items).into_iter().map(|(k, top)| (users[k].0.clone(), Spot { x: vol_x, y: top, w: vol_w, h: VOL_H })).collect();
    volumes.sort_by(|a, b| a.1.y.total_cmp(&b.1.y));
    let w = left + wide(node_w, gap, vol_w);
    let h = spots.iter().map(Spot::bottom).chain(volumes.iter().map(|(_, s)| s.bottom())).fold(0., f32::max) + PAD;
    Layout { services: spots, volumes, w, h, gap, inline }
}

/// A color for each network, the same each time.
pub fn network_colors(g: &ComposeGraph) -> Vec<(String, Hsla)> {
    const HUES: [f32; 6] = [0.58, 0.76, 0.47, 0.11, 0.92, 0.33];
    let mut names: Vec<String> = g.networks.iter().map(|n| n.name.clone()).collect();
    for s in &g.services {
        for n in &s.networks {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
    }
    names.into_iter().enumerate().map(|(i, name)| (name, hsla(HUES[i % HUES.len()], 0.62, 0.64, 1.))).collect()
}

/// The picture: a scrolling canvas of boxes and the lines between them.
/// `width` holds the space the picture had when last drawn (it's measured
/// here, and the page drawn again when it changes).
pub fn diagram(g: &ComposeGraph, usage: &Usage, theme: &Theme, width: Rc<Cell<f32>>, on: OnNode) -> AnyElement {
    let l = layout(g, width.get());
    let nets: HashMap<String, Hsla> = network_colors(g).into_iter().collect();
    let index: HashMap<&str, usize> = g.services.iter().enumerate().map(|(i, s)| (s.name.as_str(), i)).collect();

    // The lines: what each service waits for, and the volumes it keeps data in.
    struct Line {
        from: (f32, f32),
        to: (f32, f32),
        color: Hsla,
        dashed: bool,
        arrow: bool,
    }
    let mut lines = Vec::new();
    let mut labels = Vec::new();
    for (i, s) in g.services.iter().enumerate() {
        let a = l.services[i];
        for d in &s.depends_on {
            let Some(&j) = index.get(d.name.as_str()) else { continue };
            let b = l.services[j];
            let color = match d.condition.as_str() {
                "service_healthy" => theme.success.opacity(0.75),
                "service_completed_successfully" => theme.info.opacity(0.75),
                _ => theme.text_placeholder.opacity(0.8),
            };
            lines.push(Line { from: (a.right(), a.cy()), to: (b.x, b.cy()), color, dashed: false, arrow: true });
            let label = match d.condition.as_str() {
                "service_healthy" => Some(("once healthy", theme.success)),
                "service_completed_successfully" => Some(("once done", theme.info)),
                _ => None,
            };
            if let Some((text, color)) = label {
                // Near where the line meets the box it waits for.
                // In the gap before the box it waits for, just above the line.
                labels.push((text, color, b.x - (l.gap + 70.) / 2., b.cy() + (a.cy() - b.cy()) * 0.12 - 22.));
            }
        }
        for m in s.volumes.iter().filter(|m| m.kind == "volume") {
            if let Some((_, v)) = l.volumes.iter().find(|(name, _)| *name == m.source) {
                lines.push(Line { from: (a.right(), a.cy()), to: (v.x, v.cy()), color: theme.info.opacity(0.55), dashed: true, arrow: false });
            }
        }
    }
    let edges = canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let o = bounds.origin;
            let at = |x: f32, y: f32| point(o.x + px(x), o.y + px(y));
            for line in &lines {
                let ((x1, y1), (x2, y2)) = (line.from, line.to);
                // The arrow's tip touches the box; the line stops at its base.
                let end = if line.arrow { x2 - 7. } else { x2 };
                let bend = ((end - x1) / 2.).max(30.);
                let mut path = PathBuilder::stroke(px(1.5));
                if line.dashed {
                    path = path.dash_array(&[px(4.), px(3.)]);
                }
                path.move_to(at(x1, y1));
                path.cubic_bezier_to(at(end, y2), at(x1 + bend, y1), at(end - bend, y2));
                if let Ok(p) = path.build() {
                    window.paint_path(p, line.color);
                }
                if line.arrow {
                    let mut head = PathBuilder::fill();
                    head.move_to(at(x2, y2));
                    head.line_to(at(x2 - 8., y2 - 4.5));
                    head.line_to(at(x2 - 8., y2 + 4.5));
                    head.close();
                    if let Ok(p) = head.build() {
                        window.paint_path(p, line.color);
                    }
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .w(px(l.w))
    .h(px(l.h));

    let boxes = g.services.iter().enumerate().map(|(i, s)| service_box(i, s, l.services[i], l.inline, usage, &nets, theme, &on));
    let vols = l.volumes.iter().map(|(name, spot)| {
        let external = g.volumes.iter().any(|v| v.name == *name && v.external);
        let used_by: Vec<&str> = g.services.iter().filter(|s| s.volumes.iter().any(|m| m.kind == "volume" && m.source == *name)).map(|s| s.name.as_str()).collect();
        let tip = format!("A Docker volume{}: data kept apart from the containers, so it stays when they're recreated. Used by {}.", if external { " made outside this project" } else { "" }, used_by.join(", "));
        div()
            .id(SharedString::from(format!("cmp-vol-{name}")))
            .absolute()
            .left(px(spot.x))
            .top(px(spot.y))
            .w(px(spot.w))
            .h(px(spot.h))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(9.))
            .rounded(crate::ui::kit::corner(9.))
            .border_1()
            .border_dashed()
            .border_color(theme.info.opacity(0.55))
            .bg(theme.surface)
            .tooltip(move |window, cx| kit::Tooltip::view(tip.clone().into(), window, cx))
            .child(icon("hard-drive", 15., theme.info))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().w(px(spot.w - 60.)).truncate().text_size(px(12.5)).text_color(theme.text).child(name.clone()))
                    .child(div().text_size(px(11.)).text_color(theme.text_placeholder).child(if external { "volume · external" } else { "volume" })),
            )
    });
    let tags = labels.into_iter().map(|(text, color, x, y)| {
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .px(px(5.))
            .h(px(16.))
            .flex()
            .items_center()
            .rounded(crate::ui::kit::corner(4.))
            .bg(theme.canvas)
            .text_size(px(10.5))
            .text_color(color)
            .child(text)
    });
    div()
        .flex_1()
        .min_h(px(0.))
        .relative()
        .child(measure(width))
        .child(
            div()
                .id("compose-diagram")
                .absolute()
                .size_full()
                .overflow_scroll()
                .child(div().relative().w(px(l.w)).h(px(l.h)).child(edges).children(tags).children(boxes).children(vols)),
        )
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn service_box(n: usize, s: &ComposeService, spot: Spot, inline: bool, usage: &Usage, nets: &HashMap<String, Hsla>, theme: &Theme, on: &OnNode) -> AnyElement {
    let running = s.running();
    let health = s.health();
    let failed = health == "unhealthy" || s.containers.iter().any(|c| c.state == "restarting" || (c.state == "exited" && !c.status.starts_with("Exited (0)")));
    let dot = if failed {
        theme.error
    } else if running {
        theme.success
    } else {
        theme.text_placeholder
    };
    let inner = spot.w - 24.;
    // What its containers use, added up.
    let (mut used_cpu, mut used_mem) = (None::<f64>, None::<u64>);
    for c in &s.containers {
        if let Some((c_cpu, c_mem)) = usage.get(&c.name) {
            if let Some(v) = c_cpu {
                used_cpu = Some(used_cpu.unwrap_or(0.) + v);
            }
            if let Some(v) = c_mem {
                used_mem = Some(used_mem.unwrap_or(0) + v);
            }
        }
    }
    let state = if running {
        let mut parts = vec![s.state()];
        if used_cpu.is_some() {
            parts.push(format!("{} CPU", cpu(used_cpu)));
        }
        if used_mem.is_some_and(|m| m > 0) {
            parts.push(memory(used_mem));
        }
        parts.join(" · ")
    } else {
        s.containers.first().map(|c| c.status.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| s.state())
    };
    let first = s.containers.first().map(|c| c.name.clone());
    let buttons = first.map(|container| {
        let (logs, shell) = (on.clone(), on.clone());
        let (c1, c2) = (s.name.clone(), container);
        div()
            .flex()
            .items_center()
            .child(Button::icon_only(SharedString::from(format!("cmp-node-logs-{n}")), "scroll-text").tooltip("Its logs").on_click(move |_, window, cx| logs(NodeAction::Logs(c1.clone()), window, cx)))
            .when(running, |d| {
                d.child(Button::icon_only(SharedString::from(format!("cmp-node-shell-{n}")), "square-terminal").tooltip("A shell inside it").on_click(move |_, window, cx| shell(NodeAction::Shell(c2.clone()), window, cx)))
            })
    });
    let health_color = match health {
        "healthy" => theme.success,
        "unhealthy" => theme.error,
        _ => theme.warning,
    };
    let what = if s.build.is_empty() { s.image.clone() } else { format!("Built from {}", s.build) };
    let what_tip = {
        let mut t = what.clone();
        if !s.command.is_empty() {
            t.push_str(&format!("\nRuns: {}", s.command));
        }
        if s.env > 0 {
            t.push_str(&format!("\n{} environment variable{}", s.env, if s.env == 1 { "" } else { "s" }));
        }
        if !s.restart.is_empty() && s.restart != "no" {
            t.push_str(&format!("\nRestarts: {}", s.restart));
        }
        t
    };
    let ports = (!s.ports.is_empty()).then(|| {
        div().flex().items_center().gap(px(5.)).h(px(20.)).children(s.ports.iter().enumerate().map(|(k, p)| {
            let udp = if p.protocol == "udp" { "/udp" } else { "" };
            let label = if p.published.is_empty() { format!("{}{udp}", p.target) } else { format!(":{} → {}{udp}", p.published, p.target) };
            let tip = if p.published.is_empty() {
                format!("Port {} inside, open on a port Docker picks", p.target)
            } else {
                let on_what = match p.host_ip.as_str() {
                    "" | "0.0.0.0" | "::" => "every address of this machine".to_string(),
                    "127.0.0.1" | "::1" => "this machine only (localhost)".to_string(),
                    ip => ip.to_string(),
                };
                format!("Open on {on_what} at port {}, into the container's port {}. Click to open it in your browser here.", p.published, p.target)
            };
            let open = (p.protocol != "udp").then(|| p.published.parse::<u16>().ok()).flatten().map(|port| {
                let on = on.clone();
                move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| on(NodeAction::OpenPort(port), window, cx)
            });
            div()
                .id(SharedString::from(format!("cmp-port-{n}-{k}")))
                .flex_none()
                .px(px(6.))
                .h(px(20.))
                .flex()
                .items_center()
                .gap(px(4.))
                .rounded(crate::ui::kit::corner(5.))
                .bg(theme.accent.opacity(0.13))
                .text_size(px(11.5))
                .text_color(theme.text_accent)
                .tooltip(move |window, cx| kit::Tooltip::view(tip.clone().into(), window, cx))
                .when_some(open, |d, open| d.cursor_pointer().hover(|st| st.bg(theme.accent.opacity(0.24))).on_click(open))
                .child(icon("plug", 11., theme.text_accent))
                .child(label)
        }))
    });
    let networks = shows_networks(s).then(|| {
        if !s.network_mode.is_empty() {
            return div().flex().items_center().h(px(20.)).child(kit::pill(format!("network: {}", s.network_mode), theme.text_muted)).into_any_element();
        }
        div()
            .flex()
            .items_center()
            .gap(px(5.))
            .h(px(20.))
            .children(s.networks.iter().map(|name| {
                let color = nets.get(name).copied().unwrap_or(theme.text_muted);
                div().flex_none().flex().items_center().gap(px(5.)).px(px(6.)).h(px(20.)).rounded(crate::ui::kit::corner(5.)).bg(color.opacity(0.13)).text_size(px(11.)).text_color(color).child(kit::dot(color, 6.)).child(name.clone())
            }))
            .into_any_element()
    });
    let all_binds = mounts(s, inline);
    let more = all_binds.len().saturating_sub(MOUNTS_SHOWN);
    let shown = if more > 0 { &all_binds[..MOUNTS_SHOWN] } else { &all_binds[..] };
    let mounts = (!all_binds.is_empty()).then(|| {
        div()
            .flex()
            .flex_col()
            .children(shown.iter().enumerate().map(|(k, m)| {
                let volume = m.kind == "volume";
                let file = !volume && m.source.rsplit('/').next().is_some_and(|last| last.contains('.') && !last.starts_with('.'));
                let text = format!("{} → {}{}", m.source, m.target, if m.read_only { " (read-only)" } else { "" });
                let tip = if volume {
                    format!("The Docker volume {}, seen inside as {}: its data stays when the container's recreated", m.source, m.target)
                } else {
                    format!("This machine's {} {}, seen inside as {}{}", if file { "file" } else { "folder" }, m.source, m.target, if m.read_only { ", read-only" } else { "" })
                };
                div()
                    .id(SharedString::from(format!("cmp-bind-{n}-{k}")))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(16.))
                    .tooltip(move |window, cx| kit::Tooltip::view(tip.clone().into(), window, cx))
                    .child(icon(if volume { "hard-drive" } else if file { "file" } else { "folder" }, 11., if volume { theme.info } else { theme.text_placeholder }))
                    .child(div().w(px(inner - 16.)).truncate().text_size(px(11.)).text_color(theme.text_muted).child(text))
            }))
            .when(more > 0, |d| d.child(div().h(px(16.)).text_size(px(11.)).text_color(theme.text_placeholder).child(format!("+{more} more"))))
    });
    div()
        .absolute()
        .left(px(spot.x))
        .top(px(spot.y))
        .w(px(spot.w))
        .h(px(spot.h))
        .px(px(12.))
        .py(px(12.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .overflow_hidden()
        .rounded(crate::ui::kit::corner(10.))
        .border_1()
        .border_color(if failed { theme.error.opacity(0.6) } else { theme.border })
        .bg(theme.surface)
        .when(!crate::ui::kit::square(), |d| d.shadow_sm())
        .child(
            div()
                .h(px(22.))
                .flex()
                .items_center()
                .gap(px(7.))
                .child(kit::dot(dot, 8.))
                .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(s.name.clone()))
                .when(s.replicas.is_some_and(|r| r > 1), |d| d.child(div().text_size(px(11.)).text_color(theme.text_placeholder).child(format!("×{}", s.replicas.unwrap_or(1)))))
                .when(!health.is_empty(), |d| d.child(kit::pill(health.to_string(), health_color)))
                .child(div().flex_1())
                .children(buttons),
        )
        .child(
            div()
                .id(SharedString::from(format!("cmp-what-{n}")))
                .h(px(16.))
                .flex()
                .items_center()
                .gap(px(6.))
                .tooltip(move |window, cx| kit::Tooltip::view(what_tip.clone().into(), window, cx))
                .child(icon("box", 12., theme.text_placeholder))
                .child(div().w(px(inner - 18.)).truncate().text_size(px(12.)).text_color(theme.text_muted).child(what)),
        )
        .child(div().h(px(16.)).w(px(inner)).truncate().text_size(px(12.)).text_color(if failed { theme.error } else { theme.text_placeholder }).child(state))
        .children(ports)
        .children(networks)
        .children(mounts)
        .into_any_element()
}

/// Measures the space it's put in into `width`, drawing the page again when
/// that changes (GPUI only cuts text off, with …, at a width it's told).
pub fn measure(width: Rc<Cell<f32>>) -> impl IntoElement {
    canvas(
        move |b, window, _| {
            let w = f32::from(b.size.width);
            if (width.get() - w).abs() > 0.5 {
                width.set(w);
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

/// The key under the picture's toolbar: what the lines and colors mean.
pub fn legend(g: &ComposeGraph, theme: &Theme) -> AnyElement {
    let item = |glyph: AnyElement, text: &'static str| div().flex().items_center().gap(px(6.)).child(glyph).child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child(text));
    let line = |color: Hsla| div().flex().items_center().child(div().w(px(16.)).h(px(1.5)).bg(color)).child(div().text_size(px(9.)).text_color(color).child("▶")).into_any_element();
    let nets = network_colors(g);
    let shows = g.services.iter().any(shows_networks);
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(16.))
        .h(px(30.))
        .px(px(14.))
        .border_b_1()
        .border_color(theme.border_variant)
        .child(item(line(theme.text_placeholder), "waits for"))
        .child(item(line(theme.success), "waits until healthy"))
        .child(item(div().w(px(16.)).h(px(10.)).rounded(crate::ui::kit::corner(3.)).border_1().border_dashed().border_color(theme.info).into_any_element(), "volume"))
        .child(item(icon("plug", 11., theme.text_accent).into_any_element(), "port on the machine"))
        .when(shows && !nets.is_empty(), |d| {
            d.child(div().text_size(px(11.5)).text_color(theme.text_placeholder).child("Networks"))
                .children(nets.into_iter().map(|(name, color)| div().flex().items_center().gap(px(5.)).child(kit::dot(color, 7.)).child(div().text_size(px(11.5)).text_color(theme.text_muted).child(name))))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::services::{ComposeDependency, ComposeMount};

    fn svc(name: &str, deps: &[&str], volume: Option<&str>) -> ComposeService {
        ComposeService {
            name: name.into(),
            depends_on: deps.iter().map(|d| ComposeDependency { name: d.to_string(), condition: "service_started".into() }).collect(),
            volumes: volume.map(|v| ComposeMount { kind: "volume".into(), source: v.into(), target: "/data".into(), read_only: false }).into_iter().collect(),
            ..Default::default()
        }
    }

    #[test]
    fn lays_out_by_what_waits() {
        // web → api → db, cache; worker → cache.
        let g = ComposeGraph {
            services: vec![svc("api", &["db", "cache"], None), svc("cache", &[], Some("cache-data")), svc("db", &[], Some("db-data")), svc("web", &["api"], None), svc("worker", &["cache"], None)],
            ..Default::default()
        };
        let l = layout(&g, 0.);
        let x = |i: usize| l.services[i].x;
        assert_eq!(x(3), x(4), "web and worker: nothing waits for them");
        assert!(x(0) > x(3), "the API right of web");
        assert!(x(2) > x(0) && x(1) == x(2), "db and cache right of the API");
        assert_eq!(l.services[0].cy(), l.services[3].cy(), "the API level with web");
        // No two boxes in a column overlap.
        for a in 0..5 {
            for b in 0..5 {
                let (p, q) = (l.services[a], l.services[b]);
                if a != b && p.x == q.x {
                    assert!(p.bottom() <= q.y || q.bottom() <= p.y);
                }
            }
        }
        assert_eq!(l.volumes.len(), 2);
        assert!(l.volumes.iter().all(|(_, v)| v.x > x(2)));
        assert!(l.w > l.volumes[0].1.right() && l.h > 0.);
    }

    #[test]
    fn fits_the_page() {
        let g = ComposeGraph { services: vec![svc("web", &["api"], None), svc("api", &["db"], None), svc("db", &[], Some("db-data"))], ..Default::default() };
        let wide = layout(&g, 0.);
        let fitted = layout(&g, 1100.);
        assert!(wide.w > 1100.);
        assert!(fitted.w <= 1100.5, "{}", fitted.w);
        assert!(fitted.services[0].w >= NODE_MIN_W);
        let roomy = layout(&g, 2400.);
        assert!(roomy.services[0].x > PAD, "centered when there's room");
        assert!(roomy.w <= 2400.5);
    }

    #[test]
    fn survives_a_loop() {
        let g = ComposeGraph { services: vec![svc("a", &["b"], None), svc("b", &["a"], None)], ..Default::default() };
        assert_eq!(layout(&g, 0.).services.len(), 2);
    }
}
