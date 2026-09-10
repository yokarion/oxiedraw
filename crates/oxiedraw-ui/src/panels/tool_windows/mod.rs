//! Tool-owned property panels - gradient, text, crop, pattern, guides - which
//! float over the canvas instead of docking, shown while their tool is active.
//! The layout anchors them to a corner or along an edge.

mod frame;

pub(crate) mod crop;
pub(crate) mod gradient;
pub(crate) mod guide;
pub(crate) mod pattern;
pub(crate) mod text;

use std::cell::RefCell;
use std::rc::Rc;

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::ColorState;
use oxiedraw_core::guides::GuideState;
use oxiedraw_core::patterns::PatternState;
use oxiedraw_core::text::fonts::TextEngine;
use oxiedraw_core::tools::{CropState, GradientState, Tool};
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::font_previews::FontPreviews;
use crate::layout::{FloatAnchor, FloatSize, Layout, ToolWindowId};

pub(crate) use frame::load_css;
use frame::FloatingFrame;

// A drawing area with NO draw function: allocated the whole overlay so it hears
// every size change, and snapshots nothing - only one that draws costs a surface.
fn install_room_watch(overlay: &gtk::Overlay, windows: &Rc<ToolWindows>) {
    let watch = gtk::DrawingArea::builder().can_target(false).build();
    let windows = Rc::downgrade(windows);
    watch.connect_resize(move |_, _, _| {
        if let Some(windows) = windows.upgrade() {
            for (_, frame) in &windows.frames {
                frame.replace();
            }
        }
    });
    overlay.add_overlay(&watch);
}

pub(crate) struct ToolWindows {
    frames: Vec<(ToolWindowId, FloatingFrame)>,
    set_gradient_active: Rc<dyn Fn(bool)>,
    pub(crate) refresh_text: Rc<dyn Fn()>,
}

impl ToolWindows {
    pub(crate) fn build(
        colors: &ColorState,
        gradient: &GradientState,
        crop: &CropState,
        pattern: &PatternState,
        pattern_regenerate: Rc<dyn Fn()>,
        guide: &GuideState,
        canvas: &Rc<RefCell<Canvas>>,
        text_edit_slot: &Rc<RefCell<Option<crate::text_edit::TextEdit>>>,
        text_engine: &Rc<RefCell<TextEngine>>,
        font_previews: &FontPreviews,
    ) -> Self {
        let (gradient_panel, set_gradient_active) = gradient::build(gradient, colors);
        let (text_panel, refresh_text) = text::build(text_edit_slot, text_engine, font_previews);

        let text_frame = FloatingFrame::new(&text_panel);
        text_frame.follow_visibility(&text_panel);

        let frames = vec![
            (
                ToolWindowId::Gradient,
                FloatingFrame::new(&gradient_panel),
            ),
            (ToolWindowId::TextProperties, text_frame),
            (ToolWindowId::Crop, FloatingFrame::new(&crop::build(crop))),
            (
                ToolWindowId::Pattern,
                FloatingFrame::new(&pattern::build(pattern, pattern_regenerate)),
            ),
            (
                ToolWindowId::Guide,
                FloatingFrame::new(&guide::build(guide, canvas, colors)),
            ),
        ];

        Self {
            frames,
            set_gradient_active,
            refresh_text,
        }
    }

    pub(crate) fn attach(self: &Rc<Self>, overlay: &gtk::Overlay) {
        install_room_watch(overlay, self);
        for (_, frame) in &self.frames {
            overlay.add_overlay(&frame.widget());
        }
    }

    pub(crate) fn natural_size(&self, window: ToolWindowId) -> Option<(f32, f32)> {
        self.frames
            .iter()
            .find(|(id, _)| *id == window)
            .map(|(_, frame)| frame.natural_size())
    }

    pub(crate) fn apply_anchors(&self, layout: &Layout) {
        for (id, frame) in &self.frames {
            frame.set_placement(layout.anchor_of(*id), layout.float_size(*id));
        }
    }

    pub(crate) fn set_float_size(&self, window: ToolWindowId, size: FloatSize) {
        if let Some((_, frame)) = self.frames.iter().find(|(id, _)| *id == window) {
            frame.set_placement(frame.anchor(), size);
        }
    }

    pub(crate) fn visible_frames(&self) -> Vec<(ToolWindowId, gtk::Widget, FloatAnchor)> {
        self.frames
            .iter()
            .map(|(id, frame)| (*id, frame.widget(), frame.anchor()))
            .filter(|(_, widget, _)| widget.is_visible())
            .collect()
    }

    pub(crate) fn set_tool(&self, tool: Tool) {
        let wanted = ToolWindowId::for_tool(tool);
        for (id, frame) in &self.frames {
            if *id == ToolWindowId::TextProperties {
                continue;
            }
            frame.set_open(wanted == Some(*id));
        }
        (self.set_gradient_active)(wanted == Some(ToolWindowId::Gradient));
    }
}
