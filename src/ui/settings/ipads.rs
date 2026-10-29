//! Settings › iPads: pairing Muxin for iPad with your machines, by a QR
//! code the iPad scans (share::pair, fleet::ipads), and the iPads paired so far.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{AnyElement, ClipboardItem, Context, RenderImage, SharedString, div, prelude::*, px};

use super::SettingsPage;
use crate::fleet::ipads::Ipad;
use crate::share::pair::{self, View};
use crate::ui::kit::{Button, ButtonStyle, card, checkbox, icon, section_title};
use crate::ui::theme::{Theme, mono_font};

#[derive(Default)]
pub(super) struct IpadState {
    /// Machines left out of the next pairing (ids); the others that are online go.
    left_out: HashSet<String>,
    starting: bool,
    /// The pairing going on, or the last one's outcome.
    pairing: Option<View>,
    /// Its link as a QR code.
    qr: Option<(String, Arc<RenderImage>)>,
    error: Option<String>,
    ipads: Vec<Ipad>,
    /// The iPad being unpaired (its key), and what the last unpairing said.
    removing: Option<String>,
    note: Option<String>,
}

impl SettingsPage {
    /// Reads the paired iPads, and follows a pairing started before this page was.
    pub(super) fn load_ipads(&mut self, cx: &mut Context<Self>) {
        self.ipad.ipads = self.engine.ipads();
        if self.ipad.pairing.as_ref().is_none_or(|v| v.finished())
            && let Some(rx) = pair::watch()
        {
            self.follow_pairing(rx, cx);
        }
    }

    /// The machines an iPad can get: id, name, and whether it's online now.
    fn ipad_machines(&self) -> Vec<(String, String, bool)> {
        self.snapshot.machines.iter().filter(|m| !m.local && m.status != "disconnected").map(|m| (m.id.clone(), m.name.clone(), m.status == "online")).collect()
    }

    fn start_ipad_pairing(&mut self, cx: &mut Context<Self>) {
        if self.ipad.starting {
            return;
        }
        let ids: Vec<String> = self.ipad_machines().into_iter().filter(|(id, _, online)| *online && !self.ipad.left_out.contains(id)).map(|(id, _, _)| id).collect();
        self.ipad.error = None;
        self.ipad.note = None;
        if ids.is_empty() {
            self.ipad.error = Some("Pick a machine that's online.".into());
            cx.notify();
            return;
        }
        self.ipad.starting = true;
        let engine = self.engine.clone();
        let job = self.engine.runtime().spawn(async move { engine.pair_ipad(&ids).await });
        let task = cx.spawn(async move |this, cx| {
            let started = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.ipad.starting = false;
                match started {
                    Ok(rx) => this.follow_pairing(rx, cx),
                    Err(e) => this.ipad.error = Some(e),
                }
                cx.notify();
            });
        });
        self._tasks.push(task);
        cx.notify();
    }

    /// Shows how a pairing goes, until it's over.
    fn follow_pairing(&mut self, mut rx: tokio::sync::watch::Receiver<View>, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                let v = rx.borrow_and_update().clone();
                let finished = v.finished();
                let shown = this.update(cx, |this, cx| {
                    this.show_pairing(v, cx);
                    cx.notify();
                });
                if shown.is_err() || finished || rx.changed().await.is_err() {
                    break;
                }
            }
        });
        self._tasks.push(task);
    }

    fn show_pairing(&mut self, v: View, cx: &mut Context<Self>) {
        if v.finished() {
            self.ipad.ipads = self.engine.ipads();
        }
        if self.ipad.qr.as_ref().is_none_or(|(link, _)| *link != v.link)
            && let Some(image) = qr_image(&v.link)
            && let Some((_, old)) = self.ipad.qr.replace((v.link.clone(), image))
        {
            cx.drop_image(old, None);
        }
        self.ipad.pairing = Some(v);
    }

    fn remove_ipad(&mut self, key: String, cx: &mut Context<Self>) {
        if self.ipad.removing.is_some() {
            return;
        }
        self.ipad.removing = Some(key.clone());
        self.ipad.note = None;
        let engine = self.engine.clone();
        let job = self.engine.runtime().spawn(async move { engine.remove_ipad(&key).await });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.ipad.removing = None;
                this.ipad.note = Some(match result {
                    Ok((ipad, kept)) if kept.is_empty() => format!("Unpaired {}.", ipad.name),
                    Ok((ipad, kept)) => format!("Unpaired {}. {} couldn't be reached and still trust it.", ipad.name, kept.join(", ")),
                    Err(e) => e,
                });
                this.ipad.ipads = this.engine.ipads();
                cx.notify();
            });
        });
        self._tasks.push(task);
        cx.notify();
    }

    pub(super) fn ipads_section(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let going = self.ipad.pairing.as_ref().filter(|v| !v.finished());
        let pairing = card(theme)
            .p(px(14.))
            .gap(px(12.))
            .child(div().w_full().min_w(px(0.)).text_size(px(12.5)).text_color(theme.text_muted).child(
                "Muxin for iPad reaches your machines directly, with a key of its own. On the iPad, tap Pair with your Mac and scan the code that shows here.",
            ))
            // Until it is: then its App Store link.
            .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child("Muxin for iPad isn't on the App Store yet."))
            .child(match going {
                Some(v) => self.pairing_now(v, theme),
                None => self.pair_form(theme, cx),
            });
        let names = |ids: &[String]| -> String {
            ids.iter().map(|id| self.snapshot.machines.iter().find(|m| &m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.clone())).collect::<Vec<_>>().join(", ")
        };
        let rows: Vec<AnyElement> = self
            .ipad
            .ipads
            .iter()
            .map(|i| {
                let key = i.fingerprint.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .py(px(8.))
                    .child(icon("tablet", 16., theme.text_muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().text_size(px(13.)).child(i.name.clone()))
                            .child(div().truncate().text_size(px(12.)).text_color(theme.text_placeholder).child(format!("Paired {} · {}", crate::ui::format::ago(Some(i.paired_at)), names(&i.machines)))),
                    )
                    .child(
                        Button::new(SharedString::from(format!("ipad-remove-{key}")), "Unpair")
                            .loading(self.ipad.removing.as_deref() == Some(key.as_str()))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_ipad(key.clone(), cx))),
                    )
                    .into_any_element()
            })
            .collect();
        let list = card(theme)
            .px(px(14.))
            .py(px(6.))
            .when(rows.is_empty(), |c| c.child(div().py(px(8.)).text_size(px(12.5)).text_color(theme.text_muted).child("No iPads yet.")))
            .children(rows)
            .children(self.ipad.note.clone().map(|n| div().pb(px(8.)).text_size(px(12.)).text_color(theme.text_muted).child(n)));
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title(theme, "Pair an iPad", None))
            .child(pairing)
            .child(div().h(px(10.)))
            .child(section_title(theme, "Paired iPads", Some(self.ipad.ipads.len())))
            .child(list)
            .into_any_element()
    }

    /// The machines to pair it with, the button, and how the last pairing went.
    fn pair_form(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let machines = self.ipad_machines();
        let picked = machines.iter().filter(|(id, _, online)| *online && !self.ipad.left_out.contains(id)).count();
        let boxes: Vec<AnyElement> = machines
            .into_iter()
            .map(|(id, name, online)| {
                if !online {
                    return div().flex().items_center().gap(px(8.)).text_size(px(12.5)).text_color(theme.text_placeholder).child(div().size(px(15.))).child(format!("{name} · offline")).into_any_element();
                }
                let on = !self.ipad.left_out.contains(&id);
                checkbox(theme, SharedString::from(format!("ipad-machine-{id}")), on, name, cx.listener(move |this, value: &bool, _, cx| {
                    if *value {
                        this.ipad.left_out.remove(&id);
                    } else {
                        this.ipad.left_out.insert(id.clone());
                    }
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();
        let outcome = self.ipad.pairing.as_ref().map(|v| {
            let (glyph, color, line) = match v.state.as_str() {
                "done" => ("circle-check", theme.success, format!("Paired {} with {}.", v.device.as_deref().unwrap_or("the iPad"), v.paired.join(", "))),
                "denied" => ("circle-x", theme.text_muted, "Not paired: you didn't allow it.".to_string()),
                _ => ("triangle-alert", theme.warning, v.error.clone().unwrap_or_else(|| "The pairing ended.".into())),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().flex().items_center().gap(px(8.)).text_size(px(12.5)).child(icon(glyph, 14., color)).child(line))
                .children(v.failed.iter().map(|f| div().pl(px(22.)).text_size(px(12.)).text_color(theme.text_muted).child(format!("{}: {}", f.name, f.error))))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .when(boxes.is_empty(), |d| d.child(div().text_size(px(12.5)).text_color(theme.text_muted).child("Add a machine first: the iPad reaches your other machines, not this computer.")))
            .children(boxes)
            .child(
                div().flex().child(
                    Button::new("ipad-pair", "Pair an iPad")
                        .icon("tablet")
                        .style(ButtonStyle::Primary)
                        .loading(self.ipad.starting)
                        .disabled(picked == 0)
                        .on_click(cx.listener(|this, _, _, cx| this.start_ipad_pairing(cx))),
                ),
            )
            .children(self.ipad.error.clone().map(|e| div().text_size(px(12.)).text_color(theme.error).child(e)))
            .children(outcome)
            .into_any_element()
    }

    /// The QR code to scan, then the iPad asking, then the machines taking its key.
    fn pairing_now(&self, v: &View, theme: &Theme) -> AnyElement {
        let cancel = Button::new("ipad-cancel", "Cancel").on_click(|_, _, _| pair::stop());
        match v.state.as_str() {
            "asking" => {
                let code = v.code.clone().unwrap_or_default();
                let spaced = if code.len() == 6 { format!("{} {}", &code[..3], &code[3..]) } else { code };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(div().flex().items_center().gap(px(8.)).text_size(px(14.)).child(icon("tablet", 16., theme.accent)).child(format!("{} wants to pair.", v.device.as_deref().unwrap_or("An iPad"))))
                    .child(div().text_size(px(12.5)).text_color(theme.text_muted).child("Allow it only if the iPad shows this code:"))
                    .child(div().font_family(mono_font()).text_size(px(28.)).child(spaced))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(Button::new("ipad-allow", "Allow").icon("check").style(ButtonStyle::Primary).on_click(|_, _, _| pair::allow(true)))
                            .child(Button::new("ipad-deny", "Don't allow").on_click(|_, _, _| pair::allow(false))),
                    )
                    .into_any_element()
            }
            "pairing" => div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.5))
                .child(icon("loader-circle", 14., theme.text_muted))
                .child(format!("Pairing {} with {}…", v.device.as_deref().unwrap_or("the iPad"), v.machines.join(", ")))
                .into_any_element(),
            _ => {
                let link = v.link.clone();
                div()
                    .flex()
                    .gap(px(18.))
                    .children(self.ipad.qr.as_ref().map(|(_, image)| div().flex_none().p(px(10.)).rounded(crate::ui::kit::corner(8.)).bg(gpui::white()).child(gpui::img(image.clone()).size(px(200.)))))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child(div().text_size(px(13.)).child("Scan this with Muxin on your iPad."))
                            .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(format!("It gets {}. You'll see the same code on both screens before anything is paired.", v.machines.join(", "))))
                            .child(div().text_size(px(12.)).text_color(theme.text_placeholder).child("No camera? Copy the link and paste it on the iPad. It works for 10 minutes."))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .child(Button::new("ipad-copy", "Copy link").icon("copy").on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(link.clone()))))
                                    .child(cancel),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}

/// The link as a QR code: black on white, with the quiet zone a scanner
/// needs, about twice the size it's shown at (for a Retina screen).
fn qr_image(link: &str) -> Option<Arc<RenderImage>> {
    let code = qrcode::QrCode::new(link.as_bytes()).ok()?;
    let (width, colors) = (code.width(), code.to_colors());
    let quiet = 4usize;
    let modules = width + quiet * 2;
    let scale = (440 / modules).max(4);
    let side = (modules * scale) as u32;
    let picture = image::RgbaImage::from_fn(side, side, |x, y| {
        let (mx, my) = (x as usize / scale, y as usize / scale);
        let inside = (quiet..quiet + width).contains(&mx) && (quiet..quiet + width).contains(&my);
        if inside && colors[(my - quiet) * width + (mx - quiet)] == qrcode::Color::Dark { image::Rgba([0, 0, 0, 255]) } else { image::Rgba([255, 255, 255, 255]) }
    });
    Some(Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(picture)])))
}
