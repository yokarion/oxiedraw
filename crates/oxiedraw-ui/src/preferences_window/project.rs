//! Project page: document-level settings. Currently rolling numbered backups
//! and background autosave; a home for future document management (templates,
//! recent files, etc.).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::session::AutosaveConfig;
use crate::settings::AppSettings;

/// Autosave interval choices, as `(label, seconds)`. The combo index maps
/// straight into this table.
const INTERVALS: &[(&str, u32)] = &[
    ("10 seconds", 10),
    ("30 seconds", 30),
    ("1 minute", 60),
    ("2 minutes", 120),
    ("3 minutes", 180),
    ("5 minutes", 300),
    ("10 minutes", 600),
    ("20 minutes", 1200),
    ("30 minutes", 1800),
    ("60 minutes", 3600),
];

fn interval_index(secs: u32) -> u32 {
    INTERVALS
        .iter()
        .position(|(_, s)| *s == secs)
        // Default to "5 minutes" when the stored value isn't one of the presets.
        .unwrap_or(5) as u32
}

pub(super) fn build_project_page(
    settings: Rc<RefCell<AppSettings>>,
    autosave: AutosaveConfig,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("Project");
    page.set_icon_name(Some("document-save-symbolic"));

    let save = settings.borrow().save.clone();

    // -- Backups group ---------------------------------------------------------
    let backup_group = adw::PreferencesGroup::new();
    backup_group.set_title("Backups");
    backup_group.set_description(Some(
        "Keep previous versions next to your project as name.oxiedrawproj-1 ... -N \
         (-N is the newest), rotated on every manual save",
    ));

    let backup_expander = adw::ExpanderRow::new();
    backup_expander.set_title("Keep numbered backups");
    backup_expander.set_show_enable_switch(true);
    backup_expander.set_enable_expansion(save.backups_enabled);

    let count_row = adw::SpinRow::with_range(1.0, 20.0, 1.0);
    count_row.set_title("Versions to keep");
    count_row.set_subtitle("How many previous saves to keep around");
    count_row.set_value(save.backup_count as f64);
    backup_expander.add_row(&count_row);

    {
        let settings = Rc::clone(&settings);
        backup_expander.connect_enable_expansion_notify(move |e| {
            settings.borrow_mut().save.backups_enabled = e.enables_expansion();
            settings.borrow().save();
        });
    }
    {
        let settings = Rc::clone(&settings);
        count_row.connect_value_notify(move |r| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = r.value() as usize;
            settings.borrow_mut().save.backup_count = v;
            settings.borrow().save();
        });
    }

    backup_group.add(&backup_expander);
    page.add(&backup_group);

    // -- Autosave group --------------------------------------------------------
    let autosave_group = adw::PreferencesGroup::new();
    autosave_group.set_title("Autosave");

    let autosave_expander = adw::ExpanderRow::new();
    autosave_expander.set_title("Autosave in the background");
    autosave_expander.set_subtitle(
        "Silently keep your open documents saved. Unsaved documents are written to a recovery copy",
    );
    autosave_expander.set_show_enable_switch(true);
    autosave_expander.set_enable_expansion(save.autosave_enabled);

    let interval_model = gtk::StringList::new(
        &INTERVALS.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
    );
    let interval_row = adw::ComboRow::new();
    interval_row.set_title("Autosave every");
    interval_row.set_model(Some(&interval_model));
    interval_row.set_selected(interval_index(save.autosave_interval_secs));
    autosave_expander.add_row(&interval_row);

    {
        let settings = Rc::clone(&settings);
        let autosave = autosave.clone();
        autosave_expander.connect_enable_expansion_notify(move |e| {
            let enabled = e.enables_expansion();
            settings.borrow_mut().save.autosave_enabled = enabled;
            settings.borrow().save();
            autosave.enabled.set(enabled);
        });
    }
    {
        let settings = Rc::clone(&settings);
        interval_row.connect_selected_notify(move |r| {
            let (_, secs) = INTERVALS
                .get(r.selected() as usize)
                .copied()
                .unwrap_or(("5 minutes", 300));
            settings.borrow_mut().save.autosave_interval_secs = secs;
            settings.borrow().save();
            autosave.interval_secs.set(secs);
        });
    }

    autosave_group.add(&autosave_expander);
    page.add(&autosave_group);

    page
}

#[cfg(test)]
mod tests {
    use super::{INTERVALS, interval_index};

    #[test]
    fn interval_index_maps_known_values() {
        assert_eq!(interval_index(10), 0);
        assert_eq!(interval_index(300), 5);
        assert_eq!(interval_index(3600), 9);
    }

    #[test]
    fn interval_index_falls_back_to_five_minutes() {
        // 45s is not one of the presets - pick the 5-minute default (index 5).
        assert_eq!(interval_index(45), 5);
        assert_eq!(INTERVALS[interval_index(45) as usize].1, 300);
    }

    #[test]
    fn every_interval_index_round_trips() {
        for (i, (_, secs)) in INTERVALS.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let expected = i as u32;
            assert_eq!(interval_index(*secs), expected, "preset {secs}s must map to its own index");
        }
    }
}
