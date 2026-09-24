//! Colour palettes: the recent list, the user's custom swatches and the named
//! preset palettes behind the palette dock.
//!
//! Everything persists as one JSON document ([`PaletteStore`]); where that file
//! lives is the UI's business. The shipped palettes are compiled in and
//! read-only, so anything in [`PaletteStore::palettes`] is the user's own and
//! is all that gets written back. [`PaletteState`] is the shared handle the
//! panels mutate, with the same callback-on-change shape as
//! [`crate::color::ColorState`].

mod builtins;
mod extract;

pub use builtins::{DEFAULT_PRESET, builtin_palettes};
pub use extract::{
    BackgroundMode, ExtractOptions, ExtractOrder, ExtractedColor, MAX_EXTRACTED_COLORS,
    MIN_EXTRACTED_COLORS, PaletteSource, extract_palette,
};

use std::cell::{Cell, Ref, RefCell};
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::color::Color;
use crate::enum_meta::EnumMeta;

/// How many colours the Recent tab remembers.
pub const RECENT_CAPACITY: usize = 18;

/// Current on-disk schema of the palette file.
pub const STORE_VERSION: u32 = 1;

/// A named list of colours: one of the shipped palettes, or one the user made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    pub name: String,
    #[serde(with = "hex_colors")]
    pub colors: Vec<Color>,
    /// Shipped palettes are read-only. Never written to the file: it is set
    /// when the built-ins are merged back in on load.
    #[serde(skip)]
    pub builtin: bool,
}

impl Palette {
    pub fn new(name: String, colors: Vec<Color>) -> Self {
        Self {
            name,
            colors,
            builtin: false,
        }
    }
}

/// Which key the Sort entries in the palette menu order by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Hue,
    Brightness,
    Saturation,
}

impl EnumMeta for SortKey {
    const ALL: &'static [Self] = &[Self::Hue, Self::Brightness, Self::Saturation];

    fn label(self) -> &'static str {
        match self {
            Self::Hue => "Hue",
            Self::Brightness => "Brightness",
            Self::Saturation => "Saturation",
        }
    }
}

/// Rec. 709 luma in 0..1 - what both sorting and the extractor mean by
/// "lighter".
pub fn lightness(color: Color) -> f32 {
    oxiedraw_utils::color::luma(color.r, color.g, color.b) / 255.0
}

/// Hue for sorting, with greys parked past every real hue: an unsaturated
/// colour has no meaningful one to sort by.
pub fn hue_rank(color: Color) -> f32 {
    let (hue, saturation, _) = color.to_hsv();
    if saturation < 0.02 { 2.0 } else { hue }
}

/// Sort in place.
pub fn sort_colors(colors: &mut [Color], key: SortKey) {
    match key {
        SortKey::Hue => colors.sort_by(|a, b| {
            hue_rank(*a)
                .total_cmp(&hue_rank(*b))
                .then_with(|| lightness(*a).total_cmp(&lightness(*b)))
        }),
        SortKey::Brightness => colors.sort_by(|a, b| lightness(*a).total_cmp(&lightness(*b))),
        SortKey::Saturation => colors.sort_by(|a, b| {
            let (_, sa, _) = a.to_hsv();
            let (_, sb, _) = b.to_hsv();
            sa.total_cmp(&sb)
        }),
    }
}

/// Everything the palette dock persists. Each field is salvaged on its own, so
/// one unreadable entry costs that entry rather than the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaletteStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default, with = "hex_colors")]
    pub recent: Vec<Color>,
    #[serde(default, with = "hex_colors")]
    pub custom: Vec<Color>,
    /// User palettes. The built-ins are merged in front of these on load and
    /// stripped again on save.
    #[serde(default, deserialize_with = "salvaged_palettes")]
    pub palettes: Vec<Palette>,
    /// Palette shown on the Presets tab.
    #[serde(default, deserialize_with = "salvaged")]
    pub active_preset: String,
    /// Starred palettes, by name. Kept apart from [`Palette`] so a star on a
    /// built-in survives without writing the built-in itself out.
    #[serde(default, deserialize_with = "salvaged")]
    pub favorites: Vec<String>,
}

/// A field that cannot be read falls back to its default instead of taking the
/// whole file down with it.
fn salvaged<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(raw).unwrap_or_else(|e| {
        tracing::warn!(err = %e, "unreadable palette field, using its default");
        T::default()
    }))
}

/// Same, per palette: a damaged entry is dropped and the rest are kept.
fn salvaged_palettes<'de, D>(deserializer: D) -> Result<Vec<Palette>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<serde_json::Value> = salvaged(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|entry| {
            serde_json::from_value(entry)
                .inspect_err(|e| tracing::warn!(err = %e, "dropping unreadable palette"))
                .ok()
        })
        .collect())
}

const fn default_version() -> u32 {
    STORE_VERSION
}

impl Default for PaletteStore {
    fn default() -> Self {
        let mut store = Self {
            version: STORE_VERSION,
            recent: Vec::new(),
            custom: Vec::new(),
            palettes: Vec::new(),
            active_preset: DEFAULT_PRESET.to_string(),
            favorites: Vec::new(),
        };
        store.sanitize();
        store
    }
}

impl PaletteStore {
    /// Repair rather than reject. Something that is not JSON at all is an
    /// error, so the caller keeps the file instead of saving defaults over it.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        let mut store: Self = serde_json::from_str(text)?;
        store.sanitize();
        Ok(store)
    }

    /// Serialise without the built-ins, which are compiled in and would only
    /// go stale in the file.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut out = self.clone();
        out.version = STORE_VERSION;
        out.palettes.retain(|p| !p.builtin);
        serde_json::to_string_pretty(&out)
    }

    /// Put the built-ins back in front, drop user palettes that collide with
    /// them, and make sure the active preset points at something real.
    pub fn sanitize(&mut self) {
        self.palettes.retain(|p| !p.builtin && !p.name.trim().is_empty());
        let mut taken: Vec<String> = builtin_palettes().iter().map(|p| p.name.clone()).collect();
        for palette in &mut self.palettes {
            if taken.contains(&palette.name) {
                palette.name = unique_name(&taken, &palette.name);
            }
            taken.push(palette.name.clone());
        }
        let mut merged = builtin_palettes();
        merged.append(&mut self.palettes);
        self.palettes = merged;

        self.recent.truncate(RECENT_CAPACITY);
        self.favorites.retain(|name| self.palettes.iter().any(|p| &p.name == name));
        if !self.palettes.iter().any(|p| p.name == self.active_preset) {
            self.active_preset = self
                .palettes
                .first()
                .map_or_else(|| DEFAULT_PRESET.to_string(), |p| p.name.clone());
        }
    }

    /// Move `color` to the front of the recent list, dropping the oldest once
    /// it is full. Returns whether anything moved.
    pub fn push_recent(&mut self, color: Color) -> bool {
        if self.recent.first() == Some(&color) {
            return false;
        }
        self.recent.retain(|c| *c != color);
        self.recent.insert(0, color);
        self.recent.truncate(RECENT_CAPACITY);
        true
    }

    /// Append to the custom swatches. Returns false when it is already there,
    /// which the panel reports rather than silently duplicating.
    pub fn add_custom(&mut self, color: Color) -> bool {
        if self.custom.contains(&color) {
            return false;
        }
        self.custom.push(color);
        true
    }

    pub fn palette(&self, name: &str) -> Option<&Palette> {
        self.palettes.iter().find(|p| p.name == name)
    }

    pub fn palette_mut(&mut self, name: &str) -> Option<&mut Palette> {
        self.palettes.iter_mut().find(|p| p.name == name)
    }

    pub fn active_palette(&self) -> Option<&Palette> {
        self.palette(&self.active_preset)
    }

    pub fn is_favorite(&self, name: &str) -> bool {
        self.favorites.iter().any(|n| n == name)
    }

    pub fn set_favorite(&mut self, name: &str, on: bool) {
        self.favorites.retain(|n| n != name);
        if on {
            self.favorites.push(name.to_string());
        }
    }

    /// Palette names for the presets dropdown and the manage list: starred
    /// first, then the shipped order.
    pub fn listed_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.palettes.iter().map(|p| p.name.clone()).collect();
        names.sort_by_key(|name| usize::from(!self.is_favorite(name)));
        names
    }

    /// Add a palette under a name nothing else is using, and return that name.
    pub fn add_palette(&mut self, name: &str, colors: Vec<Color>) -> String {
        let taken: Vec<String> = self.palettes.iter().map(|p| p.name.clone()).collect();
        let name = unique_name(&taken, name.trim());
        self.palettes.push(Palette::new(name.clone(), colors));
        name
    }

    /// Remove a user palette. Built-ins stay: they are the fallback the
    /// presets tab lands on when the last user palette goes.
    pub fn remove_palette(&mut self, name: &str) -> bool {
        let Some(index) = self
            .palettes
            .iter()
            .position(|p| p.name == name && !p.builtin)
        else {
            return false;
        };
        self.palettes.remove(index);
        self.favorites.retain(|n| n != name);
        if self.active_preset == name {
            self.active_preset = self
                .palettes
                .first()
                .map_or_else(|| DEFAULT_PRESET.to_string(), |p| p.name.clone());
        }
        true
    }

    /// Rename a user palette, refusing blanks and names already in use.
    pub fn rename_palette(&mut self, from: &str, to: &str) -> bool {
        let to = to.trim();
        if to.is_empty() || to == from || self.palettes.iter().any(|p| p.name == to) {
            return false;
        }
        let Some(palette) = self.palettes.iter_mut().find(|p| p.name == from && !p.builtin) else {
            return false;
        };
        palette.name = to.to_string();
        for name in &mut self.favorites {
            if name == from {
                *name = to.to_string();
            }
        }
        if self.active_preset == from {
            self.active_preset = to.to_string();
        }
        true
    }

    /// Copy a palette (built-in included) into a new user palette.
    pub fn duplicate_palette(&mut self, name: &str) -> Option<String> {
        let source = self.palette(name)?.clone();
        Some(self.add_palette(&format!("{} copy", source.name), source.colors))
    }
}

fn unique_name(taken: &[String], base: &str) -> String {
    let base = if base.is_empty() { "Palette" } else { base };
    if !taken.iter().any(|n| n == base) {
        return base.to_string();
    }
    (2..=taken.len() + 2)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| !taken.iter().any(|n| n == candidate))
        .unwrap_or_else(|| base.to_string())
}

/// Move `from` to sit at `to`, shifting the rest along. Out-of-range indices
/// and a no-op move both return false.
pub fn reorder<T>(items: &mut Vec<T>, from: usize, to: usize) -> bool {
    if from == to || from >= items.len() || to >= items.len() {
        return false;
    }
    let item = items.remove(from);
    items.insert(to, item);
    true
}

/// Shared, mutable palette state, cloned into every widget that reads or
/// writes it. `changed` is what keeps the dock and the windows in step.
#[derive(Clone)]
pub struct PaletteState {
    store: Rc<RefCell<PaletteStore>>,
    changed: Rc<RefCell<Vec<(ObserverId, Rc<dyn Fn()>)>>>,
    next_observer: Rc<Cell<ObserverId>>,
}

/// Handle returned by [`PaletteState::connect_changed`], so a window that
/// closes can take its observer back out.
pub type ObserverId = u64;

impl std::fmt::Debug for PaletteState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaletteState")
            .field("palettes", &self.store.borrow().palettes.len())
            .field("recent", &self.store.borrow().recent.len())
            .field("custom", &self.store.borrow().custom.len())
            .finish_non_exhaustive()
    }
}

impl Default for PaletteState {
    fn default() -> Self {
        Self::new(PaletteStore::default())
    }
}

impl PaletteState {
    pub fn new(store: PaletteStore) -> Self {
        Self {
            store: Rc::new(RefCell::new(store)),
            changed: Rc::new(RefCell::new(Vec::new())),
            next_observer: Rc::new(Cell::new(0)),
        }
    }

    pub fn store(&self) -> Ref<'_, PaletteStore> {
        self.store.borrow()
    }

    /// Run `change` against the store, notifying observers when it reports one.
    /// The borrow is dropped first, so an observer may read the store back.
    pub fn edit<R>(&self, change: impl FnOnce(&mut PaletteStore) -> R) -> R
    where
        R: Changed,
    {
        let result = change(&mut self.store.borrow_mut());
        if result.changed() {
            self.notify_changed();
        }
        result
    }

    pub fn push_recent(&self, color: Color) {
        self.edit(|store| store.push_recent(color));
    }

    /// Observers are cloned out first, so one may connect or disconnect - as
    /// the manage window does on close - without tripping over the borrow.
    pub fn notify_changed(&self) {
        let observers: Vec<Rc<dyn Fn()>> = self
            .changed
            .borrow()
            .iter()
            .map(|(_, cb)| Rc::clone(cb))
            .collect();
        for cb in observers {
            cb();
        }
    }

    pub fn connect_changed(&self, cb: Box<dyn Fn()>) -> ObserverId {
        let id = self.next_observer.get();
        self.next_observer.set(id + 1);
        self.changed.borrow_mut().push((id, Rc::from(cb)));
        id
    }

    pub fn disconnect_changed(&self, id: ObserverId) {
        self.changed.borrow_mut().retain(|(existing, _)| *existing != id);
    }
}

/// Lets [`PaletteState::edit`] take closures that report whether they changed
/// anything, without forcing every caller to return a bool.
pub trait Changed {
    fn changed(&self) -> bool;
}

impl Changed for bool {
    fn changed(&self) -> bool {
        *self
    }
}

/// A count of what an edit touched; zero means it changed nothing.
impl Changed for usize {
    fn changed(&self) -> bool {
        *self > 0
    }
}

/// The name an edit settled on - returned by the calls that add a palette,
/// which always change something.
impl Changed for String {
    fn changed(&self) -> bool {
        true
    }
}

impl Changed for () {
    fn changed(&self) -> bool {
        true
    }
}

impl<T> Changed for Option<T> {
    fn changed(&self) -> bool {
        self.is_some()
    }
}

/// Colours are stored as `#rrggbb` strings: the file is meant to be readable
/// and hand-editable, which `{"r":..,"g":..,"b":..}` is not.
mod hex_colors {
    use serde::{Deserialize, Deserializer, Serializer, ser::SerializeSeq};

    use crate::color::Color;

    pub(super) fn serialize<S: Serializer>(
        colors: &[Color],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(colors.len()))?;
        for color in colors {
            seq.serialize_element(&color.to_hex())?;
        }
        seq.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Color>, D::Error> {
        let raw = Vec::<String>::deserialize(deserializer)?;
        Ok(raw.iter().filter_map(|s| Color::from_hex(s)).collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn color(r: u8, g: u8, b: u8) -> Color {
        Color::new(r, g, b)
    }

    #[test]
    fn recent_moves_a_repeat_pick_to_the_front_without_duplicating_it() {
        let mut store = PaletteStore::default();
        assert!(store.push_recent(color(1, 0, 0)));
        assert!(store.push_recent(color(2, 0, 0)));
        assert!(store.push_recent(color(1, 0, 0)));
        assert_eq!(store.recent, vec![color(1, 0, 0), color(2, 0, 0)]);
        assert!(
            !store.push_recent(color(1, 0, 0)),
            "re-picking the newest colour changes nothing"
        );
    }

    #[test]
    fn recent_drops_the_oldest_once_it_is_full() {
        let mut store = PaletteStore::default();
        for i in 0..(RECENT_CAPACITY + 4) {
            store.push_recent(color(i as u8, 0, 0));
        }
        assert_eq!(store.recent.len(), RECENT_CAPACITY);
        assert_eq!(store.recent[0], color((RECENT_CAPACITY + 3) as u8, 0, 0));
        assert!(!store.recent.contains(&color(0, 0, 0)));
    }

    #[test]
    fn custom_swatches_are_added_once_and_reorder_in_place() {
        let mut store = PaletteStore::default();
        assert!(store.add_custom(color(10, 20, 30)));
        assert!(!store.add_custom(color(10, 20, 30)), "already saved");
        store.add_custom(color(40, 50, 60));
        store.add_custom(color(70, 80, 90));

        assert!(reorder(&mut store.custom, 2, 0));
        assert_eq!(store.custom[0], color(70, 80, 90));
        assert!(!reorder(&mut store.custom, 1, 1));
        assert!(!reorder(&mut store.custom, 0, 9));
    }

    #[test]
    fn the_builtins_are_always_present_and_never_written_out() {
        let store = PaletteStore::default();
        assert_eq!(store.palettes.len(), builtin_palettes().len());
        assert!(store.palettes.iter().all(|p| p.builtin));

        let json = store.to_json().unwrap();
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(raw["palettes"].as_array().unwrap().len(), 0);

        let back = PaletteStore::from_json(&json).unwrap();
        assert_eq!(back.palettes.len(), builtin_palettes().len());
        assert_eq!(back.active_preset, DEFAULT_PRESET);
    }

    #[test]
    fn a_user_palette_round_trips_as_hex_and_keeps_its_star() {
        let mut store = PaletteStore::default();
        let name = store.add_palette("Mine", vec![color(0x11, 0x22, 0x33)]);
        store.set_favorite(&name, true);
        store.active_preset = name.clone();
        store.push_recent(color(0xff, 0x00, 0x00));

        let json = store.to_json().unwrap();
        assert!(json.contains("\"#112233\""), "colours store as hex: {json}");
        assert!(json.contains("\"#ff0000\""));

        let back = PaletteStore::from_json(&json).unwrap();
        assert_eq!(back.palette(&name).unwrap().colors, vec![color(0x11, 0x22, 0x33)]);
        assert!(back.is_favorite(&name));
        assert_eq!(back.active_preset, name);
        assert_eq!(back.recent, vec![color(0xff, 0, 0)]);
    }

    #[test]
    fn a_user_palette_cannot_take_a_builtin_name() {
        let mut store = PaletteStore::default();
        let name = store.add_palette(DEFAULT_PRESET, vec![color(1, 2, 3)]);
        assert_ne!(name, DEFAULT_PRESET);
        assert!(store.palette(DEFAULT_PRESET).unwrap().builtin);
        assert!(!store.rename_palette(&name, DEFAULT_PRESET));
        assert!(!store.rename_palette(&name, "   "));
        assert!(store.rename_palette(&name, "Renamed"));
        assert!(store.palette("Renamed").is_some());
    }

    #[test]
    fn built_in_palettes_resist_renaming_and_deletion() {
        let mut store = PaletteStore::default();
        assert!(!store.remove_palette(DEFAULT_PRESET));
        assert!(!store.rename_palette(DEFAULT_PRESET, "Nope"));

        let copy = store.duplicate_palette(DEFAULT_PRESET).unwrap();
        assert_eq!(copy, format!("{DEFAULT_PRESET} copy"));
        assert!(!store.palette(&copy).unwrap().builtin);
        assert_eq!(
            store.palette(&copy).unwrap().colors,
            store.palette(DEFAULT_PRESET).unwrap().colors
        );
    }

    #[test]
    fn deleting_the_shown_preset_falls_back_to_another() {
        let mut store = PaletteStore::default();
        let name = store.add_palette("Temp", vec![color(1, 1, 1)]);
        store.active_preset = name.clone();
        assert!(store.remove_palette(&name));
        assert_ne!(store.active_preset, name);
        assert!(store.active_palette().is_some());
    }

    #[test]
    fn starred_palettes_are_listed_first() {
        let mut store = PaletteStore::default();
        store.set_favorite("Pastel", true);
        assert_eq!(store.listed_names().first().map(String::as_str), Some("Pastel"));
        store.set_favorite("Pastel", false);
        assert_eq!(store.listed_names().first().map(String::as_str), Some(DEFAULT_PRESET));
    }

    #[test]
    fn an_unknown_active_preset_is_repaired_on_load() {
        let json =
            r##"{ "version": 1, "active_preset": "Deleted", "custom": ["#ffffff", "nonsense"] }"##;
        let store = PaletteStore::from_json(json).unwrap();
        assert_eq!(store.active_preset, DEFAULT_PRESET);
        assert_eq!(store.custom, vec![Color::WHITE], "unparseable entries drop");
    }

    #[test]
    fn a_bad_field_costs_that_field_and_a_bad_palette_only_itself() {
        let json = r##"{
            "version": 1,
            "favorites": null,
            "custom": ["#010203"],
            "palettes": [
                { "name": "Good", "colors": ["#040506"] },
                { "name": "Broken" },
                { "colors": ["#070809"] }
            ]
        }"##;
        let store = PaletteStore::from_json(json).expect("still parses");
        assert_eq!(store.custom, vec![color(1, 2, 3)], "kept despite the bad field");
        assert!(store.favorites.is_empty(), "unreadable field falls back");
        assert!(store.palette("Good").is_some(), "the sound palette survives");
        assert!(store.palette("Broken").is_none());
    }

    /// A file that is not JSON at all is an error, never silently replaced by
    /// defaults: the caller keeps it instead of overwriting someone's palettes.
    #[test]
    fn a_truncated_file_is_an_error_rather_than_a_reset() {
        let mut json = PaletteStore::default().to_json().unwrap();
        json.truncate(json.len() / 2);
        assert!(PaletteStore::from_json(&json).is_err());
        assert!(PaletteStore::from_json("").is_err());
    }

    /// `parse_hex_rgb` slices by byte, so a hand-edited multi-byte value used
    /// to panic on the way in.
    #[test]
    fn a_non_ascii_hex_value_is_dropped_not_fatal() {
        let json = r##"{ "custom": ["#€abc", "#0a0b0c"] }"##;
        let store = PaletteStore::from_json(json).expect("parses");
        assert_eq!(store.custom, vec![color(0x0a, 0x0b, 0x0c)]);
    }

    #[test]
    fn sorting_orders_by_the_requested_key() {
        let mut colors = vec![color(255, 255, 255), color(0, 0, 255), color(0, 0, 0)];
        sort_colors(&mut colors, SortKey::Brightness);
        assert_eq!(colors[0], color(0, 0, 0));
        assert_eq!(colors[2], color(255, 255, 255));

        let mut colors = vec![color(128, 128, 128), color(255, 0, 0)];
        sort_colors(&mut colors, SortKey::Saturation);
        assert_eq!(colors[0], color(128, 128, 128));

        // Greys have no hue, so they sort behind anything coloured.
        let mut colors = vec![color(200, 200, 200), color(0, 0, 255), color(255, 0, 0)];
        sort_colors(&mut colors, SortKey::Hue);
        assert_eq!(colors[0], color(255, 0, 0));
        assert_eq!(colors[2], color(200, 200, 200));
    }

    #[test]
    fn state_notifies_only_when_the_edit_changed_something() {
        let state = PaletteState::default();
        let hits = Rc::new(std::cell::Cell::new(0));
        {
            let hits = Rc::clone(&hits);
            state.connect_changed(Box::new(move || hits.set(hits.get() + 1)));
        }
        state.push_recent(Color::WHITE);
        assert_eq!(hits.get(), 1);
        state.push_recent(Color::WHITE);
        assert_eq!(hits.get(), 1, "a repeat of the newest colour is not a change");
        state.edit(|store| store.add_custom(Color::BLACK));
        assert_eq!(hits.get(), 2);
    }
}
