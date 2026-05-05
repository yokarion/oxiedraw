use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_utils::color as color_math;
use serde::{Deserialize, Serialize};

/// 8-bit-per-channel sRGB color used everywhere outside the picker math.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Which of the two stored colors is currently selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSlot {
    Primary,
    Secondary,
}

/// Primary, secondary, and currently selected color.
///
/// Backed by `Rc<Cell<...>>` so the picker (which mutates from high-frequency
/// pointer input closures) can share handles without going through the relm4
/// message loop. Same trade-off as `Viewport`.
///
/// `changed` lets surfaces that don't own the picker (e.g. the canvas
/// color-picker tool) push a new color and have the picker widget redraw.
#[derive(Clone)]
pub struct ColorState {
    pub primary: Rc<Cell<Color>>,
    pub secondary: Rc<Cell<Color>>,
    pub selected: Rc<Cell<ColorSlot>>,
    changed: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
}

impl std::fmt::Debug for ColorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColorState")
            .field("primary", &self.primary.get())
            .field("secondary", &self.secondary.get())
            .field("selected", &self.selected.get())
            .finish_non_exhaustive()
    }
}

impl ColorState {
    pub fn new() -> Self {
        Self {
            primary: Rc::new(Cell::new(Color::BLACK)),
            secondary: Rc::new(Cell::new(Color::WHITE)),
            selected: Rc::new(Cell::new(ColorSlot::Primary)),
            changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Color of the currently selected slot.
    pub fn current(&self) -> Color {
        match self.selected.get() {
            ColorSlot::Primary => self.primary.get(),
            ColorSlot::Secondary => self.secondary.get(),
        }
    }

    /// Overwrite the currently selected slot.
    pub fn set_current(&self, color: Color) {
        match self.selected.get() {
            ColorSlot::Primary => self.primary.set(color),
            ColorSlot::Secondary => self.secondary.set(color),
        }
    }

    /// Run all registered change callbacks. Call after mutating a slot
    /// from outside the picker widget so the picker redraws.
    pub fn notify_changed(&self) {
        for cb in self.changed.borrow().iter() {
            cb();
        }
    }

    /// Register a callback fired by [`Self::notify_changed`].
    pub fn connect_changed(&self, cb: Box<dyn Fn()>) {
        self.changed.borrow_mut().push(cb);
    }
}

impl Default for ColorState {
    fn default() -> Self {
        Self::new()
    }
}

impl Color {
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
    };

    #[inline]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hsv(h: f32, s: f32, v: f32) -> Self {
        let [r, g, b] = color_math::hsv_to_rgb(h, s, v);
        Self { r, g, b }
    }

    pub fn to_hsv(self) -> (f32, f32, f32) {
        color_math::rgb_to_hsv(self.r, self.g, self.b)
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        let [r, g, b] = color_math::parse_hex_rgb(text)?;
        Some(Self { r, g, b })
    }

    pub fn to_hex(self) -> String {
        color_math::rgb_to_hex(self.r, self.g, self.b)
    }

    /// Convert sRGB-encoded 8-bit channels to linear floats in [0, 1].
    /// The Vulkan composite pipeline expects linear input because the
    /// canvas attachment is `R8G8B8A8_SRGB`.
    #[must_use]
    pub fn to_linear_rgb(self) -> [f32; 3] {
        [
            color_math::srgb_to_linear(self.r),
            color_math::srgb_to_linear(self.g),
            color_math::srgb_to_linear(self.b),
        ]
    }
}
