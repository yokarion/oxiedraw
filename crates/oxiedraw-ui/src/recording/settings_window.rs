//! Recording Settings: per-project capture options, saved with the project,
//! plus the size of what has been recorded and a way to delete it.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use oxiedraw_core::enum_meta::EnumMeta;
use oxiedraw_core::recording::{CanvasScale, Frequency, RecordingSettings};
use relm4::gtk::{gio, glib};

use super::{Recorder, format_bytes, hint_button};
use crate::session::DocumentSession;

pub(super) fn show(parent: &adw::ApplicationWindow, session: &Rc<DocumentSession>) {
    let recorder = Rc::clone(&session.recording);
    let settings = recorder.settings();

    let win = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .default_height(620)
        .title("Recording Settings")
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&adw::HeaderBar::new());
    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    content.append(&page);
    win.set_content(Some(&content));

    let recording_group = adw::PreferencesGroup::builder().title("Recording").build();
    let frequency = combo_row("Frequency", &Frequency::labels(), settings.frequency.to_index());
    let (skip_row, skip) = switch_row(
        "Skip if no change",
        settings.skip_unchanged,
        Some(
            "Frames where nothing changed are not recorded. If you leave the program open, \
             no extra frames pile up, and the paused time won't be visible in the final output.",
        ),
    );
    let scaling = combo_row("Canvas scaling", &CanvasScale::labels(), settings.scale.to_index());
    scaling.set_subtitle("Size of the recorded frames. Smaller frames keep the project file smaller");
    recording_group.add(&frequency);
    recording_group.add(&skip_row);
    recording_group.add(&scaling);
    page.add(&recording_group);

    let behaviour_group = adw::PreferencesGroup::builder().title("Behaviour").build();
    let (mid_row, mid_stroke) = switch_row(
        "Record frame mid-stroke",
        settings.mid_stroke,
        Some("Also records frames while a stroke is still being drawn. Enabling this option may cause lag when you are drawing."),
    );
    let (auto_row, auto_start) = switch_row("AutoStart on project open", settings.auto_start, None);
    behaviour_group.add(&mid_row);
    behaviour_group.add(&auto_row);
    page.add(&behaviour_group);

    let data_group = adw::PreferencesGroup::builder().title("Recorded Data").build();
    let data_row = adw::ActionRow::builder().title("Recording").build();
    let delete = gtk::Button::builder()
        .label("Delete")
        .valign(gtk::Align::Center)
        .build();
    delete.add_css_class("destructive-action");
    data_row.add_suffix(&delete);
    data_group.add(&data_row);
    page.add(&data_group);

    let refresh_data: Rc<dyn Fn()> = {
        let recorder = Rc::clone(&recorder);
        let data_row = data_row.clone();
        let delete = delete.clone();
        Rc::new(move || {
            let stats = recorder.stats();
            data_row.set_subtitle(&if stats.frames == 0 {
                "Nothing recorded yet".to_string()
            } else {
                format!("{} frames - {}", super::group_thousands(stats.frames), format_bytes(stats.bytes))
            });
            delete.set_sensitive(stats.frames > 0);
        })
    };
    refresh_data();

    // Frame-per-stroke recording only happens on a change and never mid-stroke.
    let sync_sensitivity = {
        let skip_row = skip_row.clone();
        let mid_row = mid_row.clone();
        move |f: Frequency| {
            let timed = f.interval().is_some();
            skip_row.set_sensitive(timed);
            mid_row.set_sensitive(timed);
        }
    };
    sync_sensitivity(settings.frequency);

    let apply: Rc<dyn Fn()> = {
        let recorder = Rc::clone(&recorder);
        let (frequency, scaling) = (frequency.clone(), scaling.clone());
        let (skip, mid_stroke, auto_start) = (skip.clone(), mid_stroke.clone(), auto_start.clone());
        Rc::new(move || {
            let settings = RecordingSettings {
                frequency: Frequency::from_index(frequency.selected()),
                skip_unchanged: skip.is_active(),
                scale: CanvasScale::from_index(scaling.selected()),
                mid_stroke: mid_stroke.is_active(),
                auto_start: auto_start.is_active(),
            };
            sync_sensitivity(settings.frequency);
            recorder.set_settings(settings);
        })
    };
    for row in [&frequency, &scaling] {
        let apply = Rc::clone(&apply);
        row.connect_selected_notify(move |_| apply());
    }
    for switch in [&skip, &mid_stroke, &auto_start] {
        let apply = Rc::clone(&apply);
        switch.connect_active_notify(move |_| apply());
    }

    {
        let recorder = Rc::clone(&recorder);
        let win_c = win.clone();
        let refresh_data = Rc::clone(&refresh_data);
        delete.connect_clicked(move |_| confirm_delete(&win_c, &recorder, Rc::clone(&refresh_data)));
    }

    // The frame count keeps growing while recording.
    let ticker: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    {
        let refresh_data = Rc::clone(&refresh_data);
        let id = glib::timeout_add_local(Duration::from_secs(1), move || {
            refresh_data();
            glib::ControlFlow::Continue
        });
        *ticker.borrow_mut() = Some(id);
    }
    win.connect_close_request(move |_| {
        if let Some(id) = ticker.borrow_mut().take() {
            id.remove();
        }
        glib::Propagation::Proceed
    });

    win.present();
}

fn confirm_delete(win: &adw::Window, recorder: &Rc<Recorder>, refresh: Rc<dyn Fn()>) {
    let frames = recorder.stats().frames;
    let dialog = gtk::AlertDialog::builder()
        .message("Delete the recording?")
        .detail(format!(
            "All {} recorded frames will be removed from this project. It takes effect in the \
             file when you next save, and it can't be undone.",
            super::group_thousands(frames)
        ))
        .modal(true)
        .build();
    dialog.set_buttons(&["Cancel", "Delete"]);
    dialog.set_cancel_button(0);
    dialog.set_default_button(0);
    let recorder = Rc::clone(recorder);
    let win = win.clone();
    dialog.choose(Some(&win.clone()), None::<&gio::Cancellable>, move |result| {
        if result != Ok(1) {
            return;
        }
        if recorder.delete_all() {
            refresh();
        } else {
            let busy = gtk::AlertDialog::builder()
                .message("Could not delete the recording")
                .detail("The encoder is still busy with the last frames. Try again in a moment.")
                .modal(true)
                .build();
            busy.set_buttons(&["OK"]);
            busy.choose(Some(&win), None::<&gio::Cancellable>, |_| {});
        }
    });
}

pub(super) fn combo_row(title: &str, labels: &[&str], selected: u32) -> adw::ComboRow {
    let row = adw::ComboRow::builder().title(title).build();
    row.set_model(Some(&gtk::StringList::new(labels)));
    row.set_selected(selected);
    row
}

/// A row with a switch at the end, and an optional hint button just before it.
pub(super) fn switch_row(title: &str, active: bool, hint: Option<&str>) -> (adw::ActionRow, gtk::Switch) {
    let row = adw::ActionRow::builder().title(title).build();
    let switch = gtk::Switch::builder()
        .active(active)
        .valign(gtk::Align::Center)
        .build();
    if let Some(text) = hint {
        row.add_suffix(&hint_button(text));
    }
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));
    (row, switch)
}
