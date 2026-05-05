//! Pre-rendered font-name previews for the font dropdown.
//!
//! Rendering each family in its own face live during scroll loads fonts on the
//! fly and stutters on systems with many fonts. Instead each name is
//! rasterized once into a `gdk::MemoryTexture` (tinted to the libadwaita text
//! colour) and the dropdown blits the cached image. Rasterizing everything
//! takes a couple of seconds with hundreds of fonts, so the startup splash
//! does it incrementally via [`FontPreviews::render_one`]; until a preview
//! exists the dropdown falls back to plain text.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use oxiedraw_core::color::Color;
use oxiedraw_core::text::fonts::TextEngine;
use oxiedraw_core::text::render;
use relm4::gtk::gdk;
use relm4::gtk::glib;

/// Pixel height the previews are rendered at.
const PREVIEW_PX: f32 = 15.0;

#[derive(Clone, Default)]
pub(crate) struct FontPreviews {
    map: Rc<RefCell<HashMap<String, gdk::MemoryTexture>>>,
}

impl FontPreviews {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Render and cache the preview image for one family name. Called per family
    /// by the splash loader so the whole set is ready before the window shows.
    pub(crate) fn render_one(&self, engine: &mut TextEngine, family: &str, color: Color) {
        let (pixels, w, h) = render::render_label(engine, family, family, PREVIEW_PX, color);
        if let Some(tex) = texture_from_bgra(&pixels, w, h) {
            self.map.borrow_mut().insert(family.to_string(), tex);
        }
    }

    #[must_use]
    pub(crate) fn get(&self, family: &str) -> Option<gdk::MemoryTexture> {
        self.map.borrow().get(family).cloned()
    }
}

/// The libadwaita window text colour for the current scheme, as an opaque
/// approximation (alpha is folded into the rendered coverage anyway).
pub(crate) fn theme_text_color() -> Color {
    if adw::StyleManager::default().is_dark() {
        Color::new(255, 255, 255)
    } else {
        Color::new(40, 40, 40)
    }
}

fn texture_from_bgra(pixels: &[u8], w: u32, h: u32) -> Option<gdk::MemoryTexture> {
    if w == 0 || h == 0 || pixels.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let bytes = glib::Bytes::from(pixels);
    Some(gdk::MemoryTexture::new(
        w as i32,
        h as i32,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &bytes,
        (w as usize) * 4,
    ))
}
