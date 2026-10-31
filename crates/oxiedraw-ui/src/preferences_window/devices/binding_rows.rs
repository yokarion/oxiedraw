//! One dropdown row per action, for any device's bindings table.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::enum_meta::EnumMeta;

use crate::input::{Bindings, Displaced, InputAction, PanMode, Trigger};

pub(super) type LabelFn<T> = Rc<dyn Fn(Option<T>) -> String>;

pub(super) fn plain_label<T: Trigger>(choice: Option<T>) -> String {
    choice.map_or_else(|| "Unbind".to_owned(), |t| t.label().to_owned())
}

struct Entry<A> {
    action: A,
    row: adw::ActionRow,
    suffix: gtk::Box,
    dropdown: gtk::DropDown,
    labels: gtk::StringList,
}

pub(super) struct BindingRows<A: InputAction> {
    rows: Vec<Entry<A>>,
    label: LabelFn<A::Trigger>,
    syncing: Cell<bool>,
}

impl<A: InputAction> BindingRows<A> {
    pub(super) fn new(group: &adw::PreferencesGroup, label: LabelFn<A::Trigger>) -> Rc<Self> {
        let rows = A::ALL
            .iter()
            .map(|&action| {
                let labels = gtk::StringList::new(&[]);
                for &choice in action.choices() {
                    labels.append(&label(choice));
                }
                let dropdown = gtk::DropDown::builder()
                    .model(&labels)
                    .valign(gtk::Align::Center)
                    .build();
                let suffix = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).build();
                suffix.append(&dropdown);
                let row = adw::ActionRow::builder().title(action.label()).build();
                row.add_suffix(&suffix);
                row.set_activatable_widget(Some(&dropdown));
                group.add(&row);
                Entry { action, row, suffix, dropdown, labels }
            })
            .collect();
        Rc::new(Self { rows, label, syncing: Cell::new(false) })
    }

    pub(super) fn connect(
        self: &Rc<Self>,
        pick: impl Fn(A, Option<A::Trigger>) -> (Bindings<A>, Vec<Displaced<A>>) + 'static,
    ) {
        let pick = Rc::new(pick);
        for entry in &self.rows {
            let rows = Rc::clone(self);
            let pick = Rc::clone(&pick);
            let action = entry.action;
            entry.dropdown.connect_selected_notify(move |dropdown| {
                if rows.syncing.get() {
                    return;
                }
                let index = usize::try_from(dropdown.selected()).unwrap_or(usize::MAX);
                let Some(&choice) = action.choices().get(index) else {
                    return;
                };
                let (bindings, displaced) = pick(action, choice);
                rows.sync(&bindings, Some((action, &displaced)));
            });
        }
    }

    pub(super) fn sync(&self, bindings: &Bindings<A>, change: Option<(A, &[Displaced<A>])>) {
        self.syncing.set(true);
        for entry in &self.rows {
            let bound = bindings.get(entry.action);
            let index = entry.action.choices().iter().position(|c| *c == bound).unwrap_or(0);
            entry.dropdown.set_selected(u32::try_from(index).unwrap_or(0));
            let note = change.and_then(|(taker, displaced)| {
                displaced.iter().find(|d| d.action == entry.action).map(|d| displaced_note(taker, d))
            });
            entry.row.set_subtitle(note.as_deref().unwrap_or_default());
        }
        self.syncing.set(false);
    }

    pub(super) fn relabel(&self) {
        self.syncing.set(true);
        for entry in &self.rows {
            let labels: Vec<String> = entry.action.choices().iter().map(|&c| (self.label)(c)).collect();
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            entry.labels.splice(0, entry.labels.n_items(), &labels);
        }
        self.syncing.set(false);
    }

    pub(super) fn add_selector(&self, action: A, selector: &impl IsA<gtk::Widget>) {
        if let Some(entry) = self.rows.iter().find(|r| r.action == action) {
            entry.suffix.prepend(selector);
        }
    }
}

fn displaced_note<A: InputAction>(taker: A, displaced: &Displaced<A>) -> String {
    let took = format!("{} took {}", taker.label(), displaced.lost.label());
    displaced.now.map_or_else(
        || format!("Unbound - {took}"),
        |now| format!("Moved to {} - {took}", now.label()),
    )
}

pub(super) fn pan_mode_switch(current: PanMode, on_change: impl Fn(PanMode) + 'static) -> gtk::Box {
    let linked = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    linked.add_css_class("linked");
    linked.set_valign(gtk::Align::Center);
    let on_change = Rc::new(on_change);
    let mut first: Option<gtk::ToggleButton> = None;
    for &mode in PanMode::ALL {
        let button = gtk::ToggleButton::with_label(mode.label());
        button.set_group(first.as_ref());
        first.get_or_insert_with(|| button.clone());
        button.set_active(mode == current);
        let on_change = Rc::clone(&on_change);
        button.connect_toggled(move |b| {
            if b.is_active() {
                on_change(mode);
            }
        });
        linked.append(&button);
    }
    linked
}
