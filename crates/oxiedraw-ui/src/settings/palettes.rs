//! Loading and saving `palettes.json`, the palette dock's whole persistence.
//!
//! Kept apart from `settings.json`: the palette file is written on every
//! swatch the user adds or drags, and folding that into the settings blob
//! would mean rewriting the layout tree each time. Written the way projects
//! are - temp file, fsync, rename - because it is rewritten that often, and a
//! file cut short by a crash used to be indistinguishable from an empty one.

use std::cell::Cell;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use oxiedraw_core::palettes::{PaletteState, PaletteStore};
use relm4::gtk::glib;

const FILE_NAME: &str = "palettes.json";

/// Reordering a swatch fires a change per drop; this coalesces a burst of them
/// into one write.
const SAVE_DELAY: Duration = Duration::from_millis(400);

pub(crate) fn path() -> PathBuf {
    super::app_settings::config_dir().join(FILE_NAME)
}

pub(crate) fn load() -> PaletteStore {
    let path = path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PaletteStore::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), err = %e, "failed to read palettes");
            return PaletteStore::default();
        }
    };
    match PaletteStore::from_json(&text) {
        Ok(store) => store,
        Err(e) => {
            // Starting from defaults would be fine on its own, but the next
            // swatch would save them straight over the file. Move it aside so
            // whatever is in there can still be recovered by hand.
            let kept = path.with_extension("json.bak");
            match std::fs::rename(&path, &kept) {
                Ok(()) => tracing::error!(
                    path = %path.display(),
                    kept = %kept.display(),
                    err = %e,
                    "unreadable palette file kept aside; starting from the defaults"
                ),
                Err(move_err) => tracing::error!(
                    path = %path.display(),
                    err = %e,
                    %move_err,
                    "unreadable palette file could not be moved aside"
                ),
            }
            PaletteStore::default()
        }
    }
}

pub(crate) fn save(store: &PaletteStore) {
    let dir = super::app_settings::config_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(err = %e, "failed to create config directory");
        return;
    }
    let text = match store.to_json() {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(err = %e, "failed to serialize palettes");
            return;
        }
    };
    if let Err(e) = write_atomically(&dir.join(FILE_NAME), &text) {
        tracing::warn!(err = %e, "failed to write palettes");
    }
}

/// Write through a sibling temp file so an interrupted write can never leave a
/// half-written palette file in place.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let temp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)
}

/// Write the palettes back a moment after anything changes them. Returns a
/// flush to call before the app exits, for a save still pending.
pub(crate) fn install_autosave(state: &PaletteState) -> Rc<dyn Fn()> {
    let queued = Rc::new(Cell::new(false));
    {
        let owner = state.clone();
        let queued = Rc::clone(&queued);
        state.connect_changed(Box::new(move || {
            if queued.replace(true) {
                return;
            }
            let state = owner.clone();
            let queued = Rc::clone(&queued);
            glib::timeout_add_local_once(SAVE_DELAY, move || {
                if queued.replace(false) {
                    save(&state.store());
                }
            });
        }));
    }

    let state = state.clone();
    Rc::new(move || {
        if queued.replace(false) {
            save(&state.store());
        }
    })
}
