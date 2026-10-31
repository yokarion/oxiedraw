use std::rc::Rc;

use adw::prelude::*;

use super::binding_rows::{BindingRows, pan_mode_switch, plain_label};
use crate::input::{self, MouseAction};

pub(super) fn build_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Actions")
        .description("Wayland reports every connected mouse as one pointer, so these apply to all of them.")
        .build();
    let rows = BindingRows::<MouseAction>::new(&group, Rc::new(plain_label));
    rows.sync(&input::read(|s| s.mouse.bindings.clone()), None);
    rows.connect(|action, choice| {
        input::update(|s| {
            let displaced = s.mouse.bindings.assign(action, choice);
            (s.mouse.bindings.clone(), displaced)
        })
    });
    let current = input::read(|s| s.mouse.pan_mode);
    rows.add_selector(
        MouseAction::Pan,
        &pan_mode_switch(current, |mode| input::update(|s| s.mouse.pan_mode = mode)),
    );
    page.add(&group);
    page
}
