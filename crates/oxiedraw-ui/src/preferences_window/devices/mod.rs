//! The input device list atop the Controls page.

mod binding_rows;
mod mouse;
mod pen;
mod test_pad;
mod touchscreen;

use std::collections::BTreeSet;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::input::{self, DeviceId};

pub(super) fn build_devices_group(window: &adw::PreferencesWindow) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Input Devices")
        .description("Mouse, drawing tablet and touchscreen work side by side, each with its own bindings.")
        .build();
    let rows: Rc<std::cell::RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let refresh: Rc<dyn Fn()> = {
        let group = group.downgrade();
        let window = window.downgrade();
        let rows = Rc::clone(&rows);
        Rc::new(move || {
            let (Some(group), Some(window)) = (group.upgrade(), window.upgrade()) else {
                return;
            };
            for row in rows.borrow_mut().drain(..) {
                group.remove(&row);
            }
            let connected = connected_devices();
            let mut ids: Vec<DeviceId> = listed_devices(&connected).into_iter().collect();
            ids.sort_by_key(|id| !connected.contains(id));
            for id in ids {
                let row = device_row(&window, &id, connected.contains(&id));
                group.add(&row);
                rows.borrow_mut().push(row);
            }
        })
    };
    refresh();
    watch_seat(window, &refresh);
    group
}

fn watch_seat(window: &adw::PreferencesWindow, refresh: &Rc<dyn Fn()>) {
    let Some(seat) = gdk::Display::default().and_then(|d| d.default_seat()) else {
        return;
    };
    let added = {
        let refresh = Rc::clone(refresh);
        seat.connect_device_added(move |_, _| refresh())
    };
    let removed = {
        let refresh = Rc::clone(refresh);
        seat.connect_device_removed(move |_, _| refresh())
    };
    let handlers = std::cell::Cell::new(Some((added, removed)));
    window.connect_destroy(move |_| {
        if let Some((added, removed)) = handlers.take() {
            seat.disconnect(added);
            seat.disconnect(removed);
        }
    });
}

fn connected_devices() -> BTreeSet<DeviceId> {
    let Some(seat) = gdk::Display::default().and_then(|d| d.default_seat()) else {
        return BTreeSet::new();
    };
    let caps = seat.capabilities();
    let mut ids = BTreeSet::new();
    if caps.contains(gdk::SeatCapabilities::POINTER) {
        ids.insert(DeviceId::Mouse);
    }
    if caps.contains(gdk::SeatCapabilities::TOUCH) {
        ids.insert(DeviceId::Touchscreen);
    }
    ids.extend(
        seat.devices(gdk::SeatCapabilities::TABLET_STYLUS)
            .iter()
            .filter_map(input::device_id)
            .filter(|id| matches!(id, DeviceId::Tablet(_))),
    );
    ids
}

fn listed_devices(connected: &BTreeSet<DeviceId>) -> BTreeSet<DeviceId> {
    let mut ids: BTreeSet<DeviceId> = [DeviceId::Mouse, DeviceId::Touchscreen].into();
    ids.extend(connected.iter().cloned());
    ids.extend(input::read(|s| s.tablets.keys().cloned().map(DeviceId::Tablet).collect::<Vec<_>>()));
    ids
}

fn device_row(window: &adw::PreferencesWindow, id: &DeviceId, connected: bool) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&id.title()))
        .subtitle(if connected { "Connected" } else { "Not detected" })
        .activatable(true)
        .sensitive(connected)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(id.kind().icon_name()));
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    let id = id.clone();
    let window = window.downgrade();
    row.connect_activated(move |_| {
        if let Some(window) = window.upgrade() {
            window.push_subpage(&device_subpage(&id));
        }
    });
    row
}

fn device_subpage(id: &DeviceId) -> adw::NavigationPage {
    let content = match id {
        DeviceId::Mouse => mouse::build_page(),
        DeviceId::Touchscreen => touchscreen::build_page(),
        DeviceId::Tablet(name) => pen::build_page(name),
    };
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&content));
    adw::NavigationPage::new(&view, &id.title())
}
