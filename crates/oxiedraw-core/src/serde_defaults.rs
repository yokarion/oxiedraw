//! Shared `#[serde(default = "...")]` helpers.
//!
//! serde resolves a missing field by calling a function named by path, so a
//! field that should default to a non-`Default` value needs a named function.
//! These are the ones common enough to be worth sharing instead of copying.

pub const fn default_true() -> bool {
    true
}

pub const fn default_opacity() -> f32 {
    1.0
}
