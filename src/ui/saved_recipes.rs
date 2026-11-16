//! Recipes saved like snippets (recipe::saved). On a machine's Components
//! card: save what's picked as one, apply one, rename or delete it. In Add
//! machine: pick one to apply once the new machine answers.

use std::sync::Arc;

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use super::dialogs::{AddMachineDialog, ConfirmDialog, DialogEvent, error_line, field, footer, frame};
use super::input::{InputEvent, TextInput};
use super::kit::{self, Button, ButtonStyle};
use super::root::{Modal, Root};
use super::theme::{ActiveTheme, Theme};
use crate::fleet::Engine;
use crate::fleet::model::Machine;
use crate::recipe::saved::{self, SavedRecipe};

/// Component names for ids, as the card shows them; an id Muxin doesn't know, as it is.
fn names(ids: &[String]) -> String {
    let catalog = crate::recipe::catalog().ok();
    ids.iter().map(|id| catalog.and_then(|c| c.get(id)).map_or(id.as_str(), |c| c.name.as_str())).collect::<Vec<_>>().join(", ")
}

pub struct RecipeDialog {
    engine: Arc<Engine>,
    /// The name of the recipe being renamed; None for a new one.
    renaming: Option<String>,
    components: Vec<String>,
    name: Entity<TextInput>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for RecipeDialog {}

impl RecipeDialog {
    /// Save `components` as a new recipe, or rename the one called `renaming` (its components).
    pub fn new(engine: Arc<Engine>, renaming: Option<String>, components: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = renaming.clone().unwrap_or_default();
        let name = cx.new(|cx| TextInput::new(cx, "e.g. Agent CLIs").with_text(current).selected());
        let sub = cx.subscribe_in(&name, window, |this: &mut Self, _, e: &InputEvent, _, cx| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => cx.emit(DialogEvent::Dismissed),
            InputEvent::Changed => {
                this.error = None;
                cx.notify()
            }
        });
        RecipeDialog { engine, renaming, components, name, error: None, _subscriptions: vec![sub] }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let recipe = SavedRecipe { name: self.name.read(cx).text().to_string(), components: self.components.clone() };
        match self.engine.edit_saved_recipes(|list| saved::save(list, self.renaming.as_deref(), recipe)) {
            Ok(_) => cx.emit(DialogEvent::Dismissed),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }
}

impl Render for RecipeDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (title, intro, save) = match &self.renaming {
            Some(_) => ("Rename recipe", format!("Only its name changes. It still installs {}.", names(&self.components)), "Rename"),
            None => ("Save as recipe", format!("{}, and what they need. Apply it to another machine from its page, or as you add one.", names(&self.components)), "Save recipe"),
        };
        frame(&theme, title, Some(intro.into()))
            .child(field(&theme, "Name", self.name.clone(), None))
            .children(error_line(&theme, &self.error))
            .child(footer(vec![
                Button::new("rc-cancel", "Cancel").style(ButtonStyle::Ghost).on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Dismissed))).into_any_element(),
                Button::new("rc-save", save).style(ButtonStyle::Primary).key_hint("↵").on_click(cx.listener(|this, _, _, cx| this.submit(cx))).into_any_element(),
            ]))
    }
}

impl Root {
    /// Save `components` as a recipe, or rename the one called `renaming`.
    pub(super) fn show_recipe_dialog(&mut self, renaming: Option<String>, components: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let dialog = cx.new(|cx| RecipeDialog::new(engine, renaming, components, window, cx));
        self.show_modal(Modal::Recipe(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_recipe_dialog_event));
    }

    /// As any dialog's, and the saved recipes are read again.
    fn on_recipe_dialog_event<D>(&mut self, dialog: &Entity<D>, event: &DialogEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.recipes.saved = None;
        self.on_dialog_event(dialog, event, window, cx);
    }

    /// Asks, then deletes a saved recipe.
    fn confirm_delete_recipe(&mut self, recipe: SavedRecipe, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                format!("Delete {}?", recipe.name),
                format!("It's taken out of your saved recipes: {}. Nothing changes on any machine.", names(&recipe.components)),
                "Delete recipe",
                None,
                move |_, engine| engine.edit_saved_recipes(|list| saved::remove(list, &recipe.name)).map(|_| None),
                self.engine.clone(),
                window,
                cx,
            )
        });
        self.show_modal(Modal::Confirm(dialog.clone()), window, cx);
        self._subscriptions.push(cx.subscribe_in(&dialog, window, Self::on_recipe_dialog_event));
    }

    /// The Components card's saved recipes: apply one here, rename it, delete it. None when there are none.
    pub(super) fn saved_recipe_rows(&mut self, theme: &Theme, m: &Machine, busy: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let engine = self.engine.clone();
        let list = self.recipes.saved.get_or_insert_with(|| engine.saved_recipes()).clone();
        if list.is_empty() {
            return None;
        }
        let catalog = crate::recipe::catalog().ok()?;
        let rows: Vec<AnyElement> = list
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                let (known, unknown) = r.split(catalog);
                let mut what = names(&known);
                if !unknown.is_empty() {
                    what.push_str(&format!(" (and {}, which this Muxin doesn't have)", unknown.join(", ")));
                }
                let (mid, apply, rename, delete) = (m.id.clone(), r.clone(), r.clone(), r.clone());
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(14.))
                    .pr(px(8.))
                    .py(px(3.))
                    .child(div().flex_none().max_w(px(180.)).truncate().text_size(px(12.5)).child(r.name.clone()))
                    .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(11.5)).text_color(theme.text_muted).child(what))
                    .child(Button::new(("m-rs-apply", i), "Apply").disabled(busy || known.is_empty()).on_click(cx.listener(move |this, _, _, cx| this.apply_saved_recipe(&mid, &apply, cx))))
                    .child(Button::icon_only(("m-rs-rename", i), "pencil").tooltip("Rename").on_click(cx.listener(move |this, _, window, cx| this.show_recipe_dialog(Some(rename.name.clone()), rename.components.clone(), window, cx))))
                    .child(Button::icon_only(("m-rs-delete", i), "trash-2").tooltip("Delete").on_click(cx.listener(move |this, _, window, cx| this.confirm_delete_recipe(delete.clone(), window, cx))))
                    .into_any_element()
            })
            .collect();
        let title = div().px(px(14.)).pt(px(8.)).pb(px(2.)).text_size(px(11.5)).font_weight(FontWeight::MEDIUM).text_color(theme.text_muted).child("Saved recipes");
        Some(div().flex().flex_col().pb(px(6.)).border_t_1().border_color(theme.border_variant).child(title).children(rows).into_any_element())
    }

    /// Applies a saved recipe here, shown on the Activity page as Install is.
    fn apply_saved_recipe(&mut self, machine_id: &str, recipe: &SavedRecipe, cx: &mut Context<Self>) {
        match self.engine.apply_saved_recipe(machine_id, recipe) {
            Ok(op) => {
                self.recipes.watch(machine_id, op);
                self.show_operation(op, cx);
            }
            Err(e) => self.notify_user(e, true, cx),
        }
    }
}

/// Add machine's saved recipe: none, or one to apply once the machine answers.
pub struct RecipePick {
    saved: Vec<SavedRecipe>,
    picked: Option<usize>,
}

impl RecipePick {
    pub fn new(engine: &Engine) -> RecipePick {
        RecipePick { saved: engine.saved_recipes(), picked: None }
    }

    /// Once Add machine added `machine_id`: the recipe picked, applied when it answers.
    pub fn apply_once_up(&self, engine: &Arc<Engine>, machine_id: &str) {
        if let Some(recipe) = self.picked.and_then(|i| self.saved.get(i)) {
            engine.apply_recipe_once_up(machine_id, recipe.clone());
        }
    }

    /// "Recipe: None / the saved ones". Nothing when none are saved.
    pub fn menu(&self, theme: &Theme, cx: &mut Context<AddMachineDialog>) -> Option<AnyElement> {
        if self.saved.is_empty() {
            return None;
        }
        let choice = |id: (&'static str, usize), text: String, on: bool| {
            div()
                .id(id)
                .px(px(10.))
                .h(px(24.))
                .flex()
                .items_center()
                .rounded(kit::corner(6.))
                .border_1()
                .cursor_pointer()
                .text_size(px(12.))
                .border_color(if on { theme.accent } else { theme.border_variant })
                .bg(if on { theme.accent.opacity(0.12) } else { theme.element })
                .hover(|st| st.bg(theme.element_hover))
                .child(text)
        };
        let mut choices: Vec<AnyElement> = vec![choice(("am-recipe", 0), "None".into(), self.picked.is_none()).on_click(cx.listener(|this, _, _, cx| {
            this.recipe.picked = None;
            cx.notify()
        }))
        .into_any_element()];
        for (i, r) in self.saved.iter().enumerate() {
            choices.push(
                choice(("am-recipe", i + 1), r.name.clone(), self.picked == Some(i))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.recipe.picked = Some(i);
                        cx.notify()
                    }))
                    .into_any_element(),
            );
        }
        let help = match self.picked.and_then(|i| self.saved.get(i)) {
            Some(r) => format!("Once it answers: {}, and what they need.", names(&r.components)),
            None => "A recipe you saved, installed once it answers.".into(),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(kit::label(theme, "Recipe").flex_none().text_size(px(12.)).text_color(theme.text_muted))
                        .child(div().flex_1().min_w(px(0.)).flex().flex_wrap().gap(px(6.)).children(choices)),
                )
                .child(div().truncate().text_size(px(11.5)).text_color(theme.text_placeholder).child(SharedString::from(help)))
                .into_any_element(),
        )
    }
}
