//! The machine page's Components card (recipes): which of Muxin's components
//! a machine has, installing the missing ones picked, and updating what's
//! there. Both are `recipe_apply`, as an operation; the card checks again
//! once it's done.

use std::collections::HashMap;

use gpui::{App, AnyElement, Context, SharedString, div, prelude::*, px};

use super::github::Load;
use super::kit::{self, Button, ButtonStyle, card, icon, section_title};
use super::root::Root;
use super::theme::Theme;
use crate::fleet::model::Machine;
use crate::recipe::{self, MachineCheck};

/// The Components cards, by machine id.
#[derive(Default)]
pub struct Recipes {
    /// What the checks found.
    checks: HashMap<String, Load<MachineCheck>>,
    /// Missing components picked to install.
    picked: HashMap<String, Vec<String>>,
    /// The operation applying to a machine, until it's done.
    applying: HashMap<String, u64>,
}

impl Recipes {
    /// Forgets what was found on a machine (it went offline): it's checked again when shown.
    pub(super) fn forget(&mut self, machine_id: &str) {
        self.checks.remove(machine_id);
    }
}

impl Root {
    pub(super) fn machine_components(&mut self, theme: &Theme, m: &Machine, cx: &mut Context<Self>) -> AnyElement {
        // Applying is done: look again.
        if let Some(op) = self.recipes.applying.get(&m.id)
            && !self.snapshot.operations.iter().any(|o| o.id == *op && o.status == "running")
        {
            self.recipes.applying.remove(&m.id);
            self.recipes.checks.remove(&m.id);
        }
        if !self.recipes.checks.contains_key(&m.id) {
            self.load_components(&m.id, cx);
        }
        let mid = m.id.clone();
        let title = div()
            .flex()
            .items_center()
            .child(section_title(theme, "Components", None))
            .child(div().flex_1())
            .child(Button::icon_only("m-rc-refresh", "refresh-cw").tooltip("Check again").on_click(cx.listener(move |this, _, _, cx| this.load_components(&mid, cx))));
        let row = || div().flex().items_center().gap(px(10.)).px(px(14.)).py(px(10.)).text_size(px(12.5)).text_color(theme.text_muted);
        let body = match self.recipes.checks.get(&m.id) {
            Some(Load::Ready(found)) => self.components_found(theme, m, found, cx),
            Some(Load::Failed(e)) => row().child(icon("circle-alert", 13., theme.warning)).child(e.clone()).into_any_element(),
            _ => row().child(kit::spinner("m-rc-spin", 13., theme.text_muted)).child("Checking components…").into_any_element(),
        };
        div().flex().flex_col().gap(px(10.)).child(title).child(card(theme).child(body)).into_any_element()
    }

    /// What's there (a check mark), what's missing (a box to pick it), and the buttons.
    fn components_found(&self, theme: &Theme, m: &Machine, found: &MachineCheck, cx: &mut Context<Self>) -> AnyElement {
        let shown: Vec<_> = found.components.iter().filter(|c| c.result != "unsupported").collect();
        if shown.is_empty() {
            let system = if found.os == "windows" { "Windows" } else { "this system" };
            return div().px(px(14.)).py(px(10.)).text_size(px(12.5)).text_color(theme.text_muted).child(format!("Muxin has no components for {system} yet.")).into_any_element();
        }
        let picked = self.recipes.picked.get(&m.id).cloned().unwrap_or_default();
        let this = cx.entity().downgrade();
        let items: Vec<AnyElement> = shown
            .iter()
            .map(|c| {
                let cell = div().w(px(150.));
                if c.result == "present" {
                    return cell
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .child(icon("circle-check", 15., theme.success))
                        .child(c.name.clone())
                        .into_any_element();
                }
                let (mid, id, this) = (m.id.clone(), c.id.clone(), this.clone());
                let on = picked.contains(&c.id);
                cell.child(kit::checkbox(theme, SharedString::from(format!("m-rc-{}", c.id)), on, c.name.clone(), move |v, _, cx: &mut App| {
                    let (v, mid, id) = (*v, mid.clone(), id.clone());
                    let _ = this.update(cx, |this, cx| {
                        let list = this.recipes.picked.entry(mid).or_default();
                        list.retain(|x| *x != id);
                        if v {
                            list.push(id);
                        }
                        cx.notify()
                    });
                }))
                .into_any_element()
            })
            .collect();
        let busy = self.snapshot.operations.iter().any(|o| o.status == "running" && o.machine_id.as_deref() == Some(&m.id) && o.kind != "logs");
        let updatable = found.updatable();
        let missing = shown.iter().any(|c| c.result == "missing");
        let install = if picked.is_empty() { "Install".to_string() } else { format!("Install {}", picked.len()) };
        let note = match (missing, !updatable.is_empty()) {
            (true, true) => "What they need is installed first. Update brings the agent CLIs here to their newest versions.",
            (true, false) => "What they need is installed first.",
            (false, true) => "Update brings the agent CLIs here to their newest versions.",
            (false, false) => "Everything Muxin has for this system is here.",
        };
        let footer = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .py(px(10.))
            .border_t_1()
            .border_color(theme.border_variant)
            .child(div().flex_1().min_w(px(0.)).text_size(px(11.5)).text_color(theme.text_placeholder).child(note))
            .when(!updatable.is_empty(), |d| {
                let mid = m.id.clone();
                d.child(Button::new("m-rc-update", "Update").style(ButtonStyle::Outlined).disabled(busy).on_click(cx.listener(move |this, _, _, cx| this.apply_components(&mid, updatable.clone(), true, cx))))
            })
            .when(missing, |d| {
                let mid = m.id.clone();
                d.child(Button::new("m-rc-install", install).style(ButtonStyle::Primary).disabled(busy || picked.is_empty()).on_click(cx.listener(move |this, _, _, cx| this.apply_components(&mid, picked.clone(), false, cx))))
            });
        div().flex().flex_col().child(div().px(px(14.)).py(px(12.)).child(kit::rows_of(items, 3, 9.))).child(footer).into_any_element()
    }

    fn load_components(&mut self, machine_id: &str, cx: &mut Context<Self>) {
        self.recipes.checks.entry(machine_id.to_string()).or_insert(Load::Loading);
        let (engine, id) = (self.engine.clone(), machine_id.to_string());
        let job = engine.runtime().spawn({
            let (engine, id) = (engine.clone(), id.clone());
            async move {
                let all = recipe::catalog()?.resolve(&[])?;
                engine.recipe_check(&id, &all).await
            }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                let found = match result {
                    Ok(found) => found,
                    Err(e) => {
                        this.recipes.checks.insert(id, Load::Failed(e));
                        return cx.notify();
                    }
                };
                // What's there now isn't picked anymore.
                if let Some(list) = this.recipes.picked.get_mut(&id) {
                    list.retain(|p| found.components.iter().any(|c| c.id == *p && c.result == "missing"));
                }
                this.recipes.checks.insert(id, Load::Ready(found));
                cx.notify()
            });
        });
        self.track(task);
    }

    /// Applies `ids` (with `update`, updates what's behind too), shown on the Activity page.
    fn apply_components(&mut self, machine_id: &str, ids: Vec<String>, update: bool, cx: &mut Context<Self>) {
        let started = recipe::catalog().and_then(|catalog| {
            let components = catalog.resolve(&ids)?;
            let others = catalog.conflicting(&components).into_iter().cloned().collect();
            self.engine.recipe_apply(machine_id, components.into_iter().cloned().collect(), others, update)
        });
        match started {
            Ok((op, _)) => {
                self.recipes.applying.insert(machine_id.to_string(), op);
                self.recipes.picked.remove(machine_id);
                self.show_operation(op, cx);
            }
            Err(e) => self.notify_user(e, true, cx),
        }
    }
}
