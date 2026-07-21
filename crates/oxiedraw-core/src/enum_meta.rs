//! Shared metadata for simple, index-backed enums.
//!
//! Menus and `gtk::DropDown`s all need the same three things from a plain
//! enum: the ordered list of variants, a label per variant, and conversion
//! to/from the selected row index. [`EnumMeta`] derives the index conversions
//! from a single `ALL` slice plus `label`, so that boilerplate stops being
//! hand-rolled (and subtly diverging) on every such enum.
//!
//! Only for C-like enums whose variants carry no data; anything with a payload
//! (e.g. `Tool::Selection(..)`) is not a fit.

/// A plain enum whose variants map one-to-one onto dropdown rows.
///
/// Implementors supply `ALL` (variants in menu order) and `label`; the index
/// conversions come for free. Row `i` in the dropdown is `Self::ALL[i]`.
pub trait EnumMeta: Copy + PartialEq + 'static {
    /// Every variant, in the order they appear in menus.
    const ALL: &'static [Self];

    /// Human-readable name shown in menus, dropdowns, and toasts.
    fn label(self) -> &'static str;

    /// This variant's row in [`Self::ALL`] - the value a dropdown selects.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    fn to_index(self) -> u32 {
        Self::ALL.iter().position(|v| *v == self).unwrap_or(0) as u32
    }

    /// The variant at row `index`, falling back to the first when out of range.
    #[must_use]
    fn from_index(index: u32) -> Self {
        Self::ALL
            .get(index as usize)
            .copied()
            .unwrap_or(Self::ALL[0])
    }

    /// The labels of every variant, in menu order - ready to hand to
    /// `gtk::DropDown::from_strings`.
    #[must_use]
    fn labels() -> Vec<&'static str> {
        Self::ALL.iter().map(|v| v.label()).collect()
    }
}
