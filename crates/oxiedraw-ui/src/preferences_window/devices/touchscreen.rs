use std::rc::Rc;

use adw::prelude::*;

use super::binding_rows::{BindingRows, plain_label};
use crate::input::{self, TouchAction};

pub(super) fn build_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Gestures")
        .description(
            "A drag or rotate also works with more fingers than it is bound to. \
             Pinch to zoom with the Pan fingers once two are down.",
        )
        .build();
    let rows = BindingRows::<TouchAction>::new(&group, Rc::new(plain_label));
    rows.sync(&input::read(|s| s.touchscreen.bindings.clone()), None);
    rows.connect(|action, choice| {
        input::update(|s| {
            let displaced = s.touchscreen.bindings.assign(action, choice);
            (s.touchscreen.bindings.clone(), displaced)
        })
    });
    page.add(&group);
    page
}
