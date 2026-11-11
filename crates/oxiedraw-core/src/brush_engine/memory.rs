//! Per-document brush settings: a size per brush and mode, an opacity per mode.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BrushMemory {
    #[serde(default)]
    pub draw: ModeSettings,
    #[serde(default)]
    pub erase: ModeSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModeSettings {
    /// Keyed by brush name, the only brush identity that outlives a launch.
    #[serde(default)]
    pub sizes: BTreeMap<String, f32>,
    #[serde(default)]
    pub opacity: Option<f32>,
}

/// The size slider's range; values read from a project are held to it.
pub(crate) const SIZE_RANGE: (f32, f32) = (1.0, 1000.0);

impl BrushMemory {
    const fn mode_mut(&mut self, erasing: bool) -> &mut ModeSettings {
        if erasing { &mut self.erase } else { &mut self.draw }
    }

    /// Erase mode falls back to the draw values until it has its own.
    #[must_use]
    pub fn size(&self, brush: &str, erasing: bool) -> Option<f32> {
        let read = |mode: &ModeSettings| {
            mode.sizes.get(brush).copied().and_then(|v| clamped(v, SIZE_RANGE))
        };
        if erasing {
            read(&self.erase).or_else(|| read(&self.draw))
        } else {
            read(&self.draw)
        }
    }

    #[must_use]
    pub fn opacity(&self, erasing: bool) -> Option<f32> {
        let read = |mode: &ModeSettings| mode.opacity.and_then(|v| clamped(v, (0.0, 1.0)));
        if erasing {
            read(&self.erase).or_else(|| read(&self.draw))
        } else {
            read(&self.draw)
        }
    }

    pub fn record(&mut self, brush: &str, erasing: bool, size: f32, opacity: f32) {
        let mode = self.mode_mut(erasing);
        mode.sizes.insert(brush.to_string(), size);
        mode.opacity = Some(opacity);
    }
}

fn clamped(value: f32, (min, max): (f32, f32)) -> Option<f32> {
    value.is_finite().then(|| value.clamp(min, max))
}


#[cfg(test)]
#[allow(clippy::float_cmp, clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::brush_engine::BrushEngine;

    #[test]
    fn erase_falls_back_to_draw_until_recorded() {
        let mut memory = BrushMemory::default();
        memory.record("Ink Pen", false, 8.0, 0.5);
        assert_eq!(memory.size("Ink Pen", true), Some(8.0));
        assert_eq!(memory.opacity(true), Some(0.5));

        memory.record("Ink Pen", true, 40.0, 1.0);
        assert_eq!(memory.size("Ink Pen", true), Some(40.0));
        assert_eq!(memory.size("Ink Pen", false), Some(8.0));
        assert_eq!(memory.opacity(false), Some(0.5));
        assert_eq!(memory.size("Chalk", false), None);
    }

    #[test]
    fn values_from_a_damaged_file_are_held_in_range() {
        let json = r#"{ "draw": { "sizes": { "A": 1e9, "B": -4 }, "opacity": 7.0 } }"#;
        let memory: BrushMemory = serde_json::from_str(json).unwrap();
        assert_eq!(memory.size("A", false), Some(SIZE_RANGE.1));
        assert_eq!(memory.size("B", false), Some(SIZE_RANGE.0));
        assert_eq!(memory.opacity(false), Some(1.0));

        let mut memory = BrushMemory::default();
        memory.record("A", false, f32::NAN, f32::INFINITY);
        assert_eq!((memory.size("A", false), memory.opacity(false)), (None, None));
    }

    #[test]
    fn pen_eraser_end_uses_and_keeps_the_erase_settings() {
        let engine = BrushEngine::new();
        engine.set_size(10.0);
        engine.set_erasing(true);
        engine.set_size(70.0);
        engine.set_erasing(false);

        engine.set_pen_erasing(true);
        assert_eq!(engine.size.get(), 70.0);
        engine.set_size(80.0);
        engine.set_pen_erasing(false);
        assert_eq!(engine.size.get(), 10.0);
        engine.set_erasing(true);
        assert_eq!(engine.size.get(), 80.0);
    }

    #[test]
    fn engine_keeps_a_size_per_brush_and_mode() {
        let engine = BrushEngine::new();
        let (first, second) = {
            let brushes = engine.brushes.borrow();
            (brushes[0].id, brushes[1].id)
        };
        engine.set_active(first);
        engine.set_size(31.0);
        engine.set_opacity(0.4);

        engine.set_erasing(true);
        assert_eq!(engine.size.get(), 31.0, "first erase carries the draw size");
        engine.set_size(93.0);
        engine.set_opacity(1.0);
        engine.set_erasing(false);
        assert_eq!((engine.size.get(), engine.opacity.get()), (31.0, 0.4));

        engine.set_active(second);
        assert_eq!(engine.size.get(), engine.with_active_brush(|p| p.default_size));
        engine.set_active(first);
        assert_eq!(engine.size.get(), 31.0);
        engine.set_erasing(true);
        assert_eq!((engine.size.get(), engine.opacity.get()), (93.0, 1.0));
    }

    #[test]
    fn engine_switches_memory_per_document() {
        let engine = BrushEngine::new();
        let default_size = engine.with_active_brush(|p| p.default_size);
        let doc_a = Rc::new(RefCell::new(BrushMemory::default()));
        let doc_b = Rc::new(RefCell::new(BrushMemory::default()));

        engine.set_memory(Rc::clone(&doc_a));
        engine.set_size(57.0);
        engine.set_memory(Rc::clone(&doc_b));
        assert_eq!(engine.size.get(), default_size);
        engine.set_size(5.0);

        engine.set_memory(doc_a);
        assert_eq!(engine.size.get(), 57.0);
        let name = engine.with_active_brush(|p| p.name.clone());
        assert_eq!(doc_b.borrow().size(&name, false), Some(5.0));
    }
}
