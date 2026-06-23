//! `CanvasPaintable` - a `gdk::Paintable` that draws the Vulkan
//! dmabuf texture with the viewport's pan + zoom transform applied,
//! over a transparent-checker background, plus an optional crop overlay.
//!
//! GTK4 has no built-in "transformed paintable" - the standard
//! `gtk::Picture` paints at the widget's full bounds with one of a
//! few `set_content_fit` modes. We need an arbitrary pan/zoom around
//! a cursor anchor, so we implement `gdk::Paintable` directly and
//! drive the transform from the existing `Viewport` cells.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;

use oxiedraw_core::brush_engine::BrushCursor;
use oxiedraw_core::color::Color;
use oxiedraw_core::tools::{CropOverlay, CropRect, PendingMarquee, TransformRect};
use oxiedraw_utils::geometry::Point;

use crate::perf_graph::PerfGraph;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::graphene;
use relm4::gtk::gsk;
use relm4::gtk::subclass::prelude::ObjectSubclassIsExt;

/// One checker tile side, in canvas pixels. Matches the cairo path.
const CHECKER_TILE: u32 = 128;
/// One cell inside a tile, in canvas pixels. `TILE = 2 * CELL`.
const CHECKER_CELL: u32 = 64;

/// Color-picker magnifier circle radius, in widget pixels.
const PICKER_MAG_RADIUS: f32 = 58.0;
/// Magnifier zoom: widget pixels per canvas pixel inside the loupe. Large
/// enough that individual pixels read clearly under nearest-neighbour.
const PICKER_MAG_SCALE: f32 = 12.0;

/// Color-picker loupe + eyedropper overlay state. `cursor` is the
/// widget-space pointer position; `color` is the currently sampled
/// color (shown in the swatch / used to tint the eyedropper).
#[derive(Clone, Copy)]
pub(crate) struct ColorPickerOverlay {
    pub(crate) cursor: Point,
    pub(crate) color: Option<Color>,
}

glib::wrapper! {
    pub(crate) struct CanvasPaintable(ObjectSubclass<imp::CanvasPaintable>)
        @implements gdk::Paintable;
}

impl CanvasPaintable {
    pub(crate) fn new(canvas_w: u32, canvas_h: u32) -> Self {
        let obj: Self = glib::Object::builder().build();
        let imp = obj.imp();
        imp.canvas_w.set(canvas_w);
        imp.canvas_h.set(canvas_h);
        *imp.checker.borrow_mut() = Some(build_checker_texture());
        obj
    }

    /// Snapshot of the currently-set texture, if any. Used by the
    /// per-frame rebuild path to pass `set_update_texture` to the
    /// next `gdk::DmabufTextureBuilder` so GTK recognises the new
    /// texture as a successor.
    pub(crate) fn texture(&self) -> Option<gdk::Texture> {
        self.imp().texture.borrow().clone()
    }

    /// Replace the displayed texture. Pass `None` to draw just the
    /// background until the first frame is ready. Triggers a redraw.
    pub(crate) fn set_texture(&self, texture: Option<gdk::Texture>) {
        *self.imp().texture.borrow_mut() = texture;
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Update the viewport transform. Cheap - no GPU work, just queues
    /// a redraw that will paint the existing texture at the new
    /// translation/scale.
    pub(crate) fn set_transform(&self, pan_x: f32, pan_y: f32, zoom: f32) {
        let imp = self.imp();
        imp.pan_x.set(pan_x);
        imp.pan_y.set(pan_y);
        imp.zoom.set(zoom);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Update the crop rectangle and overlay style. Pass `None` to clear.
    /// Triggers a redraw.
    pub(crate) fn set_crop(&self, rect: Option<CropRect>, overlay: CropOverlay) {
        let imp = self.imp();
        imp.crop_rect.set(rect);
        imp.crop_overlay.set(overlay);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Show or hide the crop overlay. When `false` the crop rect is not
    /// rendered regardless of whether one is stored.
    pub(crate) fn set_crop_active(&self, active: bool) {
        self.imp().crop_active.set(active);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Update the transform overlay rect. Pass `None` to hide.
    pub(crate) fn set_transform_rect(&self, rect: Option<TransformRect>) {
        self.imp().transform_rect.set(rect);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Show or hide the transform overlay. Leaving transform mode also drops
    /// the captured above-layers texture so it can't leak into later frames.
    pub(crate) fn set_transform_active(&self, active: bool) {
        let imp = self.imp();
        imp.transform_active.set(active);
        if !active {
            *imp.transform_above_texture.borrow_mut() = None;
            imp.transform_gpu_preview.set(false);
        }
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// When the live GPU transform preview is driving the canvas (so the warped
    /// layer is already composited - with its blend mode - into the presented
    /// dmabuf), the GSK overlay must not also draw the source texture, or it
    /// would double the layer. The handle box still draws.
    pub(crate) fn set_transform_gpu_preview(&self, active: bool) {
        self.imp().transform_gpu_preview.set(active);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Set the composite of the layers above the one being transformed, drawn
    /// on top of the live preview to preserve z-order. `pixels` is BGRA8
    /// premultiplied, `canvas_w x canvas_h`. Pass `None` to clear.
    pub(crate) fn set_transform_above(&self, pixels: Option<&[u8]>, canvas_w: u32, canvas_h: u32) {
        let imp = self.imp();
        if let Some(pixels) = pixels {
            let bytes = glib::Bytes::from(pixels);
            let stride = (canvas_w * 4) as usize;
            #[allow(clippy::cast_possible_wrap)]
            let texture = gdk::MemoryTexture::new(
                canvas_w as i32,
                canvas_h as i32,
                gdk::MemoryFormat::B8g8r8a8Premultiplied,
                &bytes,
                stride,
            );
            use gtk::prelude::Cast;
            *imp.transform_above_texture.borrow_mut() = Some(texture.upcast());
        } else {
            *imp.transform_above_texture.borrow_mut() = None;
        }
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Set the source image used for the live transform preview. Call with `None`
    /// to clear (on cancel or after apply). `pixels` must be BGRA8 premultiplied,
    /// `src_w x src_h` in row-major order. The pixels are uploaded as a
    /// `gdk::MemoryTexture` once; per-frame transform sampling is done by the
    /// GSK renderer on the GPU (Vulkan/OpenGL), so re-draws cost next to nothing.
    pub(crate) fn set_transform_source(
        &self,
        pixels: Option<&[u8]>,
        src_w: u32,
        src_h: u32,
        original_rect: Option<TransformRect>,
    ) {
        let imp = self.imp();
        if let Some(pixels) = pixels {
            let bytes = glib::Bytes::from(pixels);
            let stride = (src_w * 4) as usize;
            #[allow(clippy::cast_possible_wrap)]
            let texture = gdk::MemoryTexture::new(
                src_w as i32,
                src_h as i32,
                gdk::MemoryFormat::B8g8r8a8Premultiplied,
                &bytes,
                stride,
            );
            use gtk::prelude::Cast;
            *imp.transform_source_texture.borrow_mut() = Some(texture.upcast());
        } else {
            *imp.transform_source_texture.borrow_mut() = None;
        }
        imp.transform_original_rect.set(original_rect);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Update the stored canvas dimensions after a crop/resize. Triggers a redraw.
    pub(crate) fn set_canvas_size(&self, w: u32, h: u32) {
        let imp = self.imp();
        imp.canvas_w.set(w);
        imp.canvas_h.set(h);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Replace the marching-ants contours. Coordinates in canvas pixels.
    /// Pass an empty Vec to clear.
    pub(crate) fn set_selection_contours(&self, contours: Vec<Vec<Point>>) {
        *self.imp().selection_contours.borrow_mut() = contours;
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Update the in-flight (uncommitted) selection shape - rubber-band
    /// rect/ellipse or lasso polyline. Pass `None` to hide.
    pub(crate) fn set_selection_pending(&self, pending: Option<PendingMarquee>) {
        *self.imp().selection_pending.borrow_mut() = pending;
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Set the marching-ants dash phase (canvas-pixel offset). The ants
    /// timer in `app.rs` ticks this forward so the dashes appear to march.
    pub(crate) fn set_selection_ants_offset(&self, offset: f64) {
        self.imp().selection_ants_offset.set(offset);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Update the brush-footprint cursor outline. `anchor_canvas` is the
    /// brush dab origin in canvas-pixel coordinates; the cursor's
    /// per-stroke offsets (scatter, etc.) are already baked into the
    /// outline. Pass `None` to hide.
    pub(crate) fn set_brush_cursor(&self, cursor: Option<BrushCursor>, anchor_canvas: Point) {
        let imp = self.imp();
        *imp.brush_cursor.borrow_mut() = cursor;
        imp.brush_cursor_anchor.set(anchor_canvas);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Update the color-picker loupe + eyedropper overlay. Pass `None` to
    /// hide (e.g. when switching away from the picker or leaving the canvas).
    pub(crate) fn set_color_picker(&self, overlay: Option<ColorPickerOverlay>) {
        self.imp().color_picker.set(overlay);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// The color the loupe last sampled under the pointer, if the picker
    /// overlay is active. The primary-drag handler commits this instead of
    /// re-reading the pixel from the GPU, so a pick costs one readback per
    /// pointer move rather than two.
    pub(crate) fn picker_color(&self) -> Option<Color> {
        self.imp().color_picker.get().and_then(|o| o.color)
    }

    /// Configure pixel-editing visual aids. Above `nearest_threshold` the
    /// canvas texture is sampled with nearest-neighbour; above
    /// `grid_threshold` (and when `grid_enabled`) a 1-px white grid is drawn
    /// over each canvas pixel. `enabled` is the master switch for both.
    pub(crate) fn set_pixel_view(
        &self,
        enabled: bool,
        nearest_threshold: f32,
        grid_enabled: bool,
        grid_threshold: f32,
    ) {
        let imp = self.imp();
        imp.pixel_view_enabled.set(enabled);
        imp.nearest_threshold.set(nearest_threshold);
        imp.grid_enabled.set(grid_enabled);
        imp.grid_threshold.set(grid_threshold);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Set the edit-mode chrome: a dim top-left label (always shown) and an
    /// optional accent border around the canvas. The main canvas uses
    /// `("Main canvas", false, _)`; a component uses `("Component - Name",
    /// true, accent)`. `accent` is straight RGB in `0.0..=1.0`.
    pub(crate) fn set_edit_mode(&self, label: &str, bordered: bool, accent: (f32, f32, f32)) {
        let imp = self.imp();
        *imp.edit_label.borrow_mut() = label.to_string();
        // Drop the cached pill so the next snapshot re-renders it for the new
        // label; the texture stays valid across pan/zoom (fixed screen size).
        *imp.edit_label_cache.borrow_mut() = None;
        imp.edit_bordered.set(bordered);
        imp.edit_accent.set(accent);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Update the text-editing overlay. `box_rect`, `caret` and `selection`
    /// rects are all in canvas coordinates. Pass `active = false` to clear.
    pub(crate) fn set_text_edit(
        &self,
        active: bool,
        box_rect: Option<TransformRect>,
        caret: Option<(f32, f32, f32, f32)>,
        selection: Vec<(f32, f32, f32, f32)>,
        handles: Vec<(f32, f32)>,
        scale: (f32, f32),
    ) {
        let imp = self.imp();
        imp.text_edit_active.set(active);
        imp.text_edit_box.set(box_rect);
        imp.text_caret.set(caret);
        *imp.text_selection.borrow_mut() = selection;
        *imp.text_handles.borrow_mut() = handles;
        imp.text_scale.set(scale);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Rubber-band outline shown while dragging out a new text box. Canvas
    /// coordinates; `None` clears it.
    pub(crate) fn set_text_pending_box(&self, box_rect: Option<TransformRect>) {
        self.imp().text_pending_box.set(box_rect);
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Show or hide the frame-time performance overlay (F3). Triggers a redraw
    /// so the panel appears/disappears immediately.
    pub(crate) fn toggle_perf_graph(&self) {
        self.imp().perf.borrow_mut().toggle();
        gdk::prelude::PaintableExt::invalidate_contents(self);
    }

    /// Toggle caret visibility (blink) without disturbing the rest of the
    /// overlay.
    pub(crate) fn set_text_caret_visible(&self, visible: bool) {
        let imp = self.imp();
        if imp.text_caret_visible.get() != visible {
            imp.text_caret_visible.set(visible);
            gdk::prelude::PaintableExt::invalidate_contents(self);
        }
    }
}

/// Build a 128x128 BGRA `MemoryTexture` whose pixels form a 2x2
/// checker of 64x64 cells. Stored on the paintable and tiled with a
/// `gsk::RepeatNode`. Sharp cells (no filtering blur) because each
/// cell is a solid block of identical pixels at native size.
fn build_checker_texture() -> gdk::MemoryTexture {
    let side = CHECKER_TILE as usize;
    let cell = CHECKER_CELL as usize;
    let stride = side * 4;
    let mut bytes = Vec::with_capacity(stride * side);
    for y in 0..side {
        for x in 0..side {
            let dark = ((x / cell) ^ (y / cell)) & 1 == 0;
            // BGRA premultiplied opaque. The two greys mirror the
            // cairo Rgb24 values (0.92 / 0.78) - bright enough to
            // read against a transparent canvas, muted enough to not
            // fight the painted content.
            let bgra: [u8; 4] = if dark {
                [0xC7, 0xC7, 0xC7, 0xFF]
            } else {
                [0xEB, 0xEB, 0xEB, 0xFF]
            };
            bytes.extend_from_slice(&bgra);
        }
    }
    let bytes_obj = glib::Bytes::from_owned(bytes);
    let side_i = i32::try_from(side).expect("checker side fits");
    gdk::MemoryTexture::new(
        side_i,
        side_i,
        gdk::MemoryFormat::B8g8r8a8,
        &bytes_obj,
        stride,
    )
}

// ---------------------------------------------------------------------------
// Crop overlay drawing (widget-space cairo)
// ---------------------------------------------------------------------------

fn draw_crop_overlay_cairo(
    cr: &gtk::cairo::Context,
    w: i32,
    h: i32,
    rect: CropRect,
    overlay: CropOverlay,
    pan_x: f32,
    pan_y: f32,
    zoom: f32,
) {
    let n = rect.normalized();
    let wx1 = f64::from(pan_x + n.x * zoom);
    let wy1 = f64::from(pan_y + n.y * zoom);
    let wx2 = f64::from(pan_x + n.right() * zoom);
    let wy2 = f64::from(pan_y + n.bottom() * zoom);
    let wf = f64::from(w);
    let hf = f64::from(h);

    // Dark vignette outside the crop region - four rectangles.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.50);
    cr.rectangle(0.0, 0.0, wf, wy1.max(0.0));
    cr.fill().ok();
    cr.rectangle(0.0, wy2.min(hf), wf, (hf - wy2).max(0.0));
    cr.fill().ok();
    cr.rectangle(0.0, wy1, wx1.max(0.0), wy2 - wy1);
    cr.fill().ok();
    cr.rectangle(wx2.min(wf), wy1, (wf - wx2).max(0.0), wy2 - wy1);
    cr.fill().ok();

    // Grid overlay inside the crop rect.
    draw_crop_grid(cr, wx1, wy1, wx2, wy2, overlay);

    // Thin crop border.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.8);
    cr.set_line_width(1.0);
    cr.rectangle(wx1, wy1, wx2 - wx1, wy2 - wy1);
    cr.stroke().ok();

    // L-shaped corner handles.
    const CORNER_LEN: f64 = 18.0;
    const CORNER_W: f64 = 3.5;
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.set_line_width(CORNER_W);
    cr.set_line_cap(gtk::cairo::LineCap::Square);

    // Top-left
    cr.move_to(wx1, wy1 + CORNER_LEN);
    cr.line_to(wx1, wy1);
    cr.line_to(wx1 + CORNER_LEN, wy1);
    // Top-right
    cr.move_to(wx2 - CORNER_LEN, wy1);
    cr.line_to(wx2, wy1);
    cr.line_to(wx2, wy1 + CORNER_LEN);
    // Bottom-left
    cr.move_to(wx1, wy2 - CORNER_LEN);
    cr.line_to(wx1, wy2);
    cr.line_to(wx1 + CORNER_LEN, wy2);
    // Bottom-right
    cr.move_to(wx2 - CORNER_LEN, wy2);
    cr.line_to(wx2, wy2);
    cr.line_to(wx2, wy2 - CORNER_LEN);
    cr.stroke().ok();

    // Short bar handles at edge midpoints.
    const EDGE_BAR: f64 = 16.0;
    let mx = f64::midpoint(wx1, wx2);
    let my = f64::midpoint(wy1, wy2);

    cr.move_to(mx - EDGE_BAR / 2.0, wy1);
    cr.line_to(mx + EDGE_BAR / 2.0, wy1);
    cr.move_to(mx - EDGE_BAR / 2.0, wy2);
    cr.line_to(mx + EDGE_BAR / 2.0, wy2);
    cr.move_to(wx1, my - EDGE_BAR / 2.0);
    cr.line_to(wx1, my + EDGE_BAR / 2.0);
    cr.move_to(wx2, my - EDGE_BAR / 2.0);
    cr.line_to(wx2, my + EDGE_BAR / 2.0);
    cr.stroke().ok();

    // "CROP W x H px" label in a dark rounded pill, above the top-right corner.
    let text = format!("CROP  {} x {}  px", n.width_px(), n.height_px());
    cr.set_font_size(12.0);
    if let Ok(ext) = cr.text_extents(&text) {
        let tw = ext.width();
        let th = ext.height();
        let pad_h = 8.0;
        let pad_v = 5.0;
        let pill_w = tw + pad_h * 2.0;
        let pill_h = th + pad_v * 2.0;
        let pill_x = (wx2 - pill_w).max(wx1);
        let pill_y = (wy1 - pill_h - 6.0).max(2.0);
        let r = pill_h / 2.0;

        cr.new_sub_path();
        cr.arc(pill_x + r, pill_y + r, r, PI, PI * 1.5);
        cr.arc(pill_x + pill_w - r, pill_y + r, r, -PI / 2.0, 0.0);
        cr.arc(pill_x + pill_w - r, pill_y + pill_h - r, r, 0.0, PI / 2.0);
        cr.arc(pill_x + r, pill_y + pill_h - r, r, PI / 2.0, PI);
        cr.close_path();
        cr.set_source_rgba(0.08, 0.08, 0.10, 0.85);
        cr.fill().ok();

        cr.set_source_rgba(1.0, 1.0, 1.0, 0.92);
        cr.move_to(pill_x + pad_h, pill_y + pad_v + th);
        cr.show_text(&text).ok();
    }
}

fn draw_crop_grid(
    cr: &gtk::cairo::Context,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    overlay: CropOverlay,
) {
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.45);
    cr.set_line_width(1.0);
    let w = x2 - x1;
    let h = y2 - y1;

    match overlay {
        CropOverlay::Thirds => {
            for i in 1..3 {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f64 / 3.0;
                cr.move_to(x1 + w * t, y1);
                cr.line_to(x1 + w * t, y2);
                cr.move_to(x1, y1 + h * t);
                cr.line_to(x2, y1 + h * t);
            }
            cr.stroke().ok();
        }
        CropOverlay::Grid => {
            for i in 1..4 {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f64 / 4.0;
                cr.move_to(x1 + w * t, y1);
                cr.line_to(x1 + w * t, y2);
                cr.move_to(x1, y1 + h * t);
                cr.line_to(x2, y1 + h * t);
            }
            cr.stroke().ok();
        }
        CropOverlay::Diagonal => {
            cr.move_to(x1, y1);
            cr.line_to(x2, y2);
            cr.move_to(x2, y1);
            cr.line_to(x1, y2);
            cr.stroke().ok();
        }
        CropOverlay::Triangle => {
            cr.move_to(x1, y2);
            cr.line_to(x2, y1);
            let mid_x = f64::midpoint(x1, x2);
            cr.move_to(x1, y1);
            cr.line_to(mid_x, y2);
            cr.move_to(x2, y2);
            cr.line_to(mid_x, y2);
            cr.stroke().ok();
        }
        CropOverlay::Golden => {
            let g = 0.382;
            cr.move_to(x1 + w * g, y1);
            cr.line_to(x1 + w * g, y2);
            cr.move_to(x1 + w * (1.0 - g), y1);
            cr.line_to(x1 + w * (1.0 - g), y2);
            cr.move_to(x1, y1 + h * g);
            cr.line_to(x2, y1 + h * g);
            cr.move_to(x1, y1 + h * (1.0 - g));
            cr.line_to(x2, y1 + h * (1.0 - g));
            cr.stroke().ok();
        }
        CropOverlay::Spiral => {
            cr.arc(x2, y1, w * 0.618, PI, PI * 1.5);
            cr.stroke().ok();
        }
    }
}

// ---------------------------------------------------------------------------
// Transform overlay drawing (widget-space cairo)
// ---------------------------------------------------------------------------

fn draw_transform_overlay_cairo(
    cr: &gtk::cairo::Context,
    rect: TransformRect,
    pan_x: f32,
    pan_y: f32,
    zoom: f32,
) {
    // The pixel preview is rendered by the GSK pass (GPU). Cairo only draws
    // the dashed border, scale handles, and rotation handle.

    let sa = rect.angle.sin();
    let ca = rect.angle.cos();

    // Centre of the rect in widget space.
    let cx_w = pan_x + rect.cx * zoom;
    let cy_w = pan_y + rect.cy * zoom;
    // Half-extents in widget pixels.
    let hw_w = rect.half_w() * zoom;
    let hh_w = rect.half_h() * zoom;

    // -- Dashed border (drawn in local, rotated space) ----------------------
    cr.save().ok();
    cr.translate(f64::from(cx_w), f64::from(cy_w));
    cr.rotate(f64::from(rect.angle));

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
    cr.set_line_width(1.5);
    cr.set_dash(&[8.0, 4.0], 0.0);
    cr.rectangle(
        f64::from(-hw_w),
        f64::from(-hh_w),
        f64::from(hw_w) * 2.0,
        f64::from(hh_w) * 2.0,
    );
    cr.stroke().ok();
    cr.set_dash(&[], 0.0);

    // -- Scale handles (squares) --------------------------------------------
    const HS: f64 = 8.0; // handle half-size
    let handles: [(f64, f64); 8] = [
        (f64::from(-hw_w), f64::from(-hh_w)), // TL
        (f64::from(hw_w), f64::from(-hh_w)),  // TR
        (f64::from(-hw_w), f64::from(hh_w)),  // BL
        (f64::from(hw_w), f64::from(hh_w)),   // BR
        (0.0, f64::from(-hh_w)),              // TopMid
        (0.0, f64::from(hh_w)),               // BottomMid
        (f64::from(-hw_w), 0.0),              // MidLeft
        (f64::from(hw_w), 0.0),               // MidRight
    ];
    for (hx, hy) in handles {
        cr.rectangle(hx - HS, hy - HS, HS * 2.0, HS * 2.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.fill_preserve().ok();
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
        cr.set_line_width(1.0);
        cr.stroke().ok();
    }

    cr.restore().ok();

    // -- Rotation handle (widget-space, above top-mid) ---------------------
    // Top-mid in widget space: local (0, -hh) -> canvas (cx + hh*sa, cy - hh*ca)
    let top_mid_wx = cx_w + hh_w * sa;
    let top_mid_wy = cy_w - hh_w * ca;

    // The outward normal of the top edge in widget space is (sa, -ca).
    const ROT_DIST: f32 = 28.0; // widget pixels
    let rot_wx = top_mid_wx + sa * ROT_DIST;
    let rot_wy = top_mid_wy - ca * ROT_DIST;

    // Connector line.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.8);
    cr.set_line_width(1.5);
    cr.move_to(f64::from(top_mid_wx), f64::from(top_mid_wy));
    cr.line_to(f64::from(rot_wx), f64::from(rot_wy));
    cr.stroke().ok();

    // Rotation circle.
    const ROT_R: f64 = 7.0;
    cr.arc(
        f64::from(rot_wx),
        f64::from(rot_wy),
        ROT_R,
        0.0,
        2.0 * std::f64::consts::PI,
    );
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.fill_preserve().ok();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
    cr.set_line_width(1.0);
    cr.stroke().ok();
}

// ---------------------------------------------------------------------------
// Text editing overlay drawing (widget-space cairo)
// ---------------------------------------------------------------------------

/// Draw the text box outline, selection highlight and (when visible) the caret.
/// `box_rect` is the box transform (canvas coords, with rotation); `caret`,
/// `selection` and `handles` are in box-local coordinates (top-left origin), so
/// the box transform is applied here. Handles stay a fixed screen size.
#[allow(clippy::too_many_arguments)]
fn draw_text_edit_overlay_cairo(
    cr: &gtk::cairo::Context,
    box_rect: TransformRect,
    caret: Option<(f32, f32, f32, f32)>,
    caret_visible: bool,
    selection: &[(f32, f32, f32, f32)],
    handles: &[(f32, f32)],
    scale: (f32, f32),
    pan_x: f32,
    pan_y: f32,
    zoom: f32,
) {
    // `box_rect` is the natural (unscaled) box; caret/selection/handles are in
    // natural-local coords. Set up a CTM that maps natural-local (top-left
    // origin) to widget space: translate to the box centre, rotate, scale by
    // zoom, apply the anamorphic squish, then shift to the box top-left. After
    // this, drawing (0,0)..(w,h) produces the rotated, squished box on screen.
    let (sx, sy) = scale;
    cr.save().ok();
    let cwx = f64::from(pan_x + box_rect.cx * zoom);
    let cwy = f64::from(pan_y + box_rect.cy * zoom);
    cr.translate(cwx, cwy);
    cr.rotate(f64::from(box_rect.angle));
    cr.scale(f64::from(zoom), f64::from(zoom));
    cr.scale(f64::from(sx), f64::from(sy));
    cr.translate(f64::from(-box_rect.half_w()), f64::from(-box_rect.half_h()));
    let px = f64::from(1.0 / zoom); // one screen pixel in local units

    // Selection highlight (under the text, translucent accent).
    cr.set_source_rgba(0.21, 0.52, 0.89, 0.35);
    for &(x, y, w, h) in selection {
        cr.rectangle(f64::from(x), f64::from(y), f64::from(w), f64::from(h));
        cr.fill().ok();
    }

    // Box outline.
    cr.set_source_rgba(0.21, 0.52, 0.89, 0.9);
    cr.set_line_width(px);
    cr.rectangle(0.0, 0.0, f64::from(box_rect.w), f64::from(box_rect.h));
    cr.stroke().ok();

    // Caret.
    if caret_visible
        && let Some((x, y, w, h)) = caret
    {
        cr.set_source_rgba(0.1, 0.1, 0.12, 1.0);
        cr.rectangle(f64::from(x), f64::from(y), f64::from(w).max(px), f64::from(h));
        cr.fill().ok();
    }
    cr.restore().ok();

    // Resize handles: fixed-size screen squares at the (transformed) local
    // handle points, drawn axis-aligned in widget space.
    const HS: f64 = 4.0;
    for &(hx, hy) in handles {
        // Natural-local handle point -> scaled offset from centre -> canvas.
        let ox = (hx - box_rect.half_w()) * sx;
        let oy = (hy - box_rect.half_h()) * sy;
        let (cx, cy) = box_rect.local_to_canvas(ox, oy);
        let wx = f64::from(pan_x + cx * zoom);
        let wy = f64::from(pan_y + cy * zoom);
        cr.rectangle(wx - HS, wy - HS, HS * 2.0, HS * 2.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.fill_preserve().ok();
        cr.set_source_rgba(0.21, 0.52, 0.89, 1.0);
        cr.set_line_width(1.0);
        cr.stroke().ok();
    }
}

/// Render the edit-mode label into a tight rounded-pill texture (dark fill,
/// dim white text). Cached on the paintable and positioned per frame, so the
/// hot snapshot path never re-shapes text or allocates a full-widget surface.
/// Returns the texture plus its pixel `(width, height)`.
fn render_edit_label_texture(label: &str) -> Option<(gdk::MemoryTexture, f32, f32)> {
    use gtk::cairo::{Context, Format, ImageSurface};

    let pad_h = 8.0;
    let pad_v = 4.0;

    // Measure on a throwaway 1x1 surface so we can size the real one tightly.
    let (pill_w, pill_h, text_h) = {
        let tmp = ImageSurface::create(Format::ARgb32, 1, 1).ok()?;
        let cr = Context::new(&tmp).ok()?;
        cr.set_font_size(13.0);
        let ext = cr.text_extents(label).ok()?;
        (
            ext.width() + pad_h * 2.0,
            ext.height() + pad_v * 2.0,
            ext.height(),
        )
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let w = pill_w.ceil() as i32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let h = pill_h.ceil() as i32;
    if w <= 0 || h <= 0 {
        return None;
    }

    let mut surface = ImageSurface::create(Format::ARgb32, w, h).ok()?;
    {
        let cr = Context::new(&surface).ok()?;
        cr.set_font_size(13.0);
        let r = pill_h / 2.0;
        cr.new_sub_path();
        cr.arc(r, r, r, PI, PI * 1.5);
        cr.arc(pill_w - r, r, r, -PI / 2.0, 0.0);
        cr.arc(pill_w - r, pill_h - r, r, 0.0, PI / 2.0);
        cr.arc(r, pill_h - r, r, PI / 2.0, PI);
        cr.close_path();
        cr.set_source_rgba(0.08, 0.08, 0.10, 0.7);
        cr.fill().ok();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.55);
        cr.move_to(pad_h, pad_v + text_h);
        cr.show_text(label).ok();
    }
    surface.flush();

    let stride = surface.stride() as usize;
    let data = surface.data().ok()?;
    let bytes = glib::Bytes::from(&data[..]);
    drop(data);
    let texture = gdk::MemoryTexture::new(
        w,
        h,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &bytes,
        stride,
    );
    #[allow(clippy::cast_possible_truncation)]
    Some((texture, pill_w as f32, pill_h as f32))
}

// ---------------------------------------------------------------------------
// Selection overlay drawing (widget-space cairo)
// ---------------------------------------------------------------------------

/// Draw marching ants (committed contours) + the in-flight rubber-band
/// shape (pending). All inputs are in canvas-pixel coordinates and are
/// transformed to widget pixels via `pan + p * zoom`.
fn draw_selection_overlay_cairo(
    cr: &gtk::cairo::Context,
    contours: &[Vec<Point>],
    pending: Option<&PendingMarquee>,
    pan_x: f32,
    pan_y: f32,
    zoom: f32,
    ants_offset: f64,
) {
    let to_widget =
        |p: Point| -> (f64, f64) { (f64::from(pan_x + p.x * zoom), f64::from(pan_y + p.y * zoom)) };

    // Committed ants. Two-pass dashed stroke (black underlay + white) so
    // the marching pattern reads on any background.
    if !contours.is_empty() {
        cr.set_line_width(1.0);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.9);
        cr.set_dash(&[], 0.0);
        for chain in contours {
            if chain.len() < 2 {
                continue;
            }
            let (wx, wy) = to_widget(chain[0]);
            cr.move_to(wx, wy);
            for p in &chain[1..] {
                let (x, y) = to_widget(*p);
                cr.line_to(x, y);
            }
        }
        cr.stroke().ok();

        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.set_dash(&[5.0, 5.0], ants_offset);
        for chain in contours {
            if chain.len() < 2 {
                continue;
            }
            let (wx, wy) = to_widget(chain[0]);
            cr.move_to(wx, wy);
            for p in &chain[1..] {
                let (x, y) = to_widget(*p);
                cr.line_to(x, y);
            }
        }
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);
    }

    // In-flight rubber-band shape. Drawn with a static dashed stroke
    // (no marching) until commit.
    if let Some(p) = pending {
        cr.set_line_width(1.0);
        cr.set_dash(&[4.0, 4.0], 0.0);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.9);
        draw_pending_path(cr, p, pan_x, pan_y, zoom);
        cr.stroke().ok();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.set_dash(&[4.0, 4.0], 4.0);
        draw_pending_path(cr, p, pan_x, pan_y, zoom);
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);
    }
}

fn draw_pending_path(
    cr: &gtk::cairo::Context,
    pending: &PendingMarquee,
    pan_x: f32,
    pan_y: f32,
    zoom: f32,
) {
    match pending {
        PendingMarquee::Rect { x, y, w, h, circle } => {
            let x0 = pan_x + *x * zoom;
            let y0 = pan_y + *y * zoom;
            let w0 = *w * zoom;
            let h0 = *h * zoom;
            if *circle {
                if w0.abs() < 1.0 || h0.abs() < 1.0 {
                    return;
                }
                // Approximate the ellipse with cairo's scale+arc trick.
                cr.save().ok();
                cr.translate(f64::from(x0 + w0 * 0.5), f64::from(y0 + h0 * 0.5));
                cr.scale(f64::from(w0.abs() * 0.5), f64::from(h0.abs() * 0.5));
                cr.arc(0.0, 0.0, 1.0, 0.0, 2.0 * std::f64::consts::PI);
                cr.restore().ok();
            } else {
                cr.rectangle(f64::from(x0), f64::from(y0), f64::from(w0), f64::from(h0));
            }
        }
        PendingMarquee::Lasso(pts) => {
            if pts.len() < 2 {
                return;
            }
            let (x0, y0) = (
                f64::from(pan_x + pts[0].x * zoom),
                f64::from(pan_y + pts[0].y * zoom),
            );
            cr.move_to(x0, y0);
            for p in &pts[1..] {
                cr.line_to(f64::from(pan_x + p.x * zoom), f64::from(pan_y + p.y * zoom));
            }
            cr.close_path();
        }
    }
}

// ---------------------------------------------------------------------------
// Pixel grid drawing (widget-space cairo)
// ---------------------------------------------------------------------------

/// Draw a 1-px white grid between every canvas pixel that's currently
/// visible in the widget. Called only when zoom is high enough that the
/// grid adds clarity rather than clutter.
fn draw_pixel_grid_cairo(
    cr: &gtk::cairo::Context,
    widget_w: f32,
    widget_h: f32,
    canvas_w: u32,
    canvas_h: u32,
    pan_x: f32,
    pan_y: f32,
    zoom: f32,
) {
    if zoom <= 0.0 || canvas_w == 0 || canvas_h == 0 {
        return;
    }

    // Clip to the on-screen canvas rectangle so the grid lines stop at
    // the document edge regardless of how far the user has panned.
    #[allow(clippy::cast_precision_loss)]
    let canvas_wf = canvas_w as f32;
    #[allow(clippy::cast_precision_loss)]
    let canvas_hf = canvas_h as f32;
    let canvas_right = pan_x + canvas_wf * zoom;
    let canvas_bottom = pan_y + canvas_hf * zoom;

    let vis_x1 = pan_x.max(0.0);
    let vis_y1 = pan_y.max(0.0);
    let vis_x2 = canvas_right.min(widget_w);
    let vis_y2 = canvas_bottom.min(widget_h);
    if vis_x2 <= vis_x1 || vis_y2 <= vis_y1 {
        return;
    }

    // Convert the visible widget rect back to canvas-pixel index range.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cx_min = (((vis_x1 - pan_x) / zoom).floor().max(0.0) as u32).min(canvas_w);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cx_max = (((vis_x2 - pan_x) / zoom).ceil().max(0.0) as u32).min(canvas_w);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cy_min = (((vis_y1 - pan_y) / zoom).floor().max(0.0) as u32).min(canvas_h);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cy_max = (((vis_y2 - pan_y) / zoom).ceil().max(0.0) as u32).min(canvas_h);

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
    cr.set_line_width(1.0);
    cr.set_antialias(gtk::cairo::Antialias::None);

    // Vertical lines at every internal pixel boundary.
    for i in (cx_min + 1)..cx_max {
        #[allow(clippy::cast_precision_loss)]
        let x = f64::from(pan_x + i as f32 * zoom);
        cr.move_to(x, f64::from(vis_y1));
        cr.line_to(x, f64::from(vis_y2));
    }
    // Horizontal lines.
    for j in (cy_min + 1)..cy_max {
        #[allow(clippy::cast_precision_loss)]
        let y = f64::from(pan_y + j as f32 * zoom);
        cr.move_to(f64::from(vis_x1), y);
        cr.line_to(f64::from(vis_x2), y);
    }
    cr.stroke().ok();

    cr.set_antialias(gtk::cairo::Antialias::Default);
}

/// Stroke the brush footprint outline as a two-tone haloed line - a
/// thicker dark underlay with a thin white line on top - so it stays
/// readable over any canvas background. True compositor inversion
/// isn't reachable here: `append_cairo` gives us a fresh transparent
/// surface, so cairo's `Operator::Difference` only mixes with what we
/// draw inside that surface, not with the canvas underneath. The
/// dual-tone halo is what Photoshop / Krita / GIMP do for the same
/// reason.
fn draw_brush_cursor_cairo(
    cr: &gtk::cairo::Context,
    cursor: &BrushCursor,
    anchor_canvas: Point,
    pan_x: f32,
    pan_y: f32,
    zoom: f32,
) {
    if cursor.is_empty() || zoom <= 0.0 {
        return;
    }

    let to_widget = |p: Point| -> (f64, f64) {
        let wx = pan_x + (anchor_canvas.x + p.x) * zoom;
        let wy = pan_y + (anchor_canvas.y + p.y) * zoom;
        (f64::from(wx), f64::from(wy))
    };

    let append_paths = |cr: &gtk::cairo::Context| {
        for stroke in &cursor.strokes {
            if stroke.len() < 2 {
                continue;
            }
            let (x0, y0) = to_widget(stroke[0]);
            cr.move_to(x0, y0);
            for p in &stroke[1..] {
                let (x, y) = to_widget(*p);
                cr.line_to(x, y);
            }
        }
    };

    cr.save().ok();
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    cr.set_line_cap(gtk::cairo::LineCap::Round);

    // Dark halo - slightly wider, mostly opaque. Stays visible on
    // bright backgrounds; provides a shadow on dark ones.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.85);
    cr.set_line_width(2.5);
    append_paths(cr);
    cr.stroke().ok();

    // Light core line - 1px white, fully opaque. Pops on dark
    // backgrounds and sits cleanly inside the dark halo on light ones.
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.set_line_width(1.0);
    append_paths(cr);
    cr.stroke().ok();

    cr.restore().ok();
}

// ---------------------------------------------------------------------------
// Color-picker overlay drawing (widget-space cairo)
// ---------------------------------------------------------------------------

/// Draw the eyedropper cursor (tip at the pointer) plus the decorations
/// around the magnifier loupe: a two-tone ring, the sampled-pixel box at
/// the loupe centre, and a swatch + hex readout. The magnified canvas
/// texture itself is drawn by the GSK pass before this runs.
fn draw_color_picker_cairo(
    cr: &gtk::cairo::Context,
    cursor: Point,
    loupe_center: (f64, f64),
    loupe_radius: f64,
    pixel_side: f64,
    color: Option<Color>,
) {
    let (cx, cy) = loupe_center;

    // Two-tone ring around the loupe.
    cr.set_line_width(4.0);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
    cr.arc(cx, cy, loupe_radius, 0.0, 2.0 * PI);
    cr.stroke().ok();
    cr.set_line_width(2.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
    cr.arc(cx, cy, loupe_radius, 0.0, 2.0 * PI);
    cr.stroke().ok();

    // Sampled-pixel box at the loupe centre (the pixel that will be picked).
    let half = pixel_side / 2.0;
    cr.set_line_width(3.0);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.75);
    cr.rectangle(cx - half, cy - half, pixel_side, pixel_side);
    cr.stroke().ok();
    cr.set_line_width(1.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.rectangle(cx - half, cy - half, pixel_side, pixel_side);
    cr.stroke().ok();

    // Swatch + hex pill below the loupe.
    if let Some(c) = color {
        let pill_w = 84.0;
        let pill_h = 24.0;
        let pill_x = cx - pill_w / 2.0;
        let pill_y = cy + loupe_radius + 6.0;
        let r = 6.0;
        cr.new_sub_path();
        cr.arc(pill_x + pill_w - r, pill_y + r, r, -PI / 2.0, 0.0);
        cr.arc(pill_x + pill_w - r, pill_y + pill_h - r, r, 0.0, PI / 2.0);
        cr.arc(pill_x + r, pill_y + pill_h - r, r, PI / 2.0, PI);
        cr.arc(pill_x + r, pill_y + r, r, PI, PI * 1.5);
        cr.close_path();
        cr.set_source_rgba(0.08, 0.08, 0.10, 0.9);
        cr.fill().ok();

        // Color chip.
        let chip = pill_h - 10.0;
        cr.rectangle(pill_x + 5.0, pill_y + 5.0, chip, chip);
        cr.set_source_rgb(
            f64::from(c.r) / 255.0,
            f64::from(c.g) / 255.0,
            f64::from(c.b) / 255.0,
        );
        cr.fill_preserve().ok();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.6);
        cr.set_line_width(1.0);
        cr.stroke().ok();

        // Hex text.
        cr.set_font_size(12.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.move_to(pill_x + chip + 12.0, pill_y + pill_h - 7.0);
        cr.show_text(&c.to_hex()).ok();
    }

    // Eyedropper, tip anchored at the pointer, body angled up-right.
    draw_eyedropper_cairo(cr, cursor, color);
}

/// Draw a stylized eyedropper with its tip at `tip` (widget pixels),
/// pointing up and to the right. Drawn in a rotated local frame so the
/// geometry stays simple. The bulb is filled with `color` so the picker
/// visibly "holds" the sampled color.
fn draw_eyedropper_cairo(cr: &gtk::cairo::Context, tip: Point, color: Option<Color>) {
    cr.save().ok();
    cr.translate(f64::from(tip.x), f64::from(tip.y));
    // Local +y points "down" the dropper; rotate so local-up maps up-right.
    cr.rotate(PI / 4.0);

    // Outline pass (dark halo) then fill pass, so it reads on any backdrop.
    let trace = |cr: &gtk::cairo::Context| {
        // Tip triangle.
        cr.move_to(0.0, 0.0);
        cr.line_to(-2.5, -6.0);
        cr.line_to(2.5, -6.0);
        cr.close_path();
        // Shaft.
        cr.rectangle(-3.0, -26.0, 6.0, 20.0);
        // Bulb.
        cr.rectangle(-7.0, -42.0, 14.0, 16.0);
    };

    cr.set_line_join(gtk::cairo::LineJoin::Round);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.85);
    cr.set_line_width(3.5);
    trace(cr);
    cr.stroke().ok();

    // White body fill.
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.move_to(0.0, 0.0);
    cr.line_to(-2.5, -6.0);
    cr.line_to(2.5, -6.0);
    cr.close_path();
    cr.fill().ok();
    cr.rectangle(-3.0, -26.0, 6.0, 20.0);
    cr.fill().ok();

    // Bulb filled with the sampled color (white when none yet).
    if let Some(c) = color {
        cr.set_source_rgb(
            f64::from(c.r) / 255.0,
            f64::from(c.g) / 255.0,
            f64::from(c.b) / 255.0,
        );
    } else {
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    }
    cr.rectangle(-7.0, -42.0, 14.0, 16.0);
    cr.fill_preserve().ok();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
    cr.set_line_width(1.0);
    cr.stroke().ok();

    cr.restore().ok();
}

mod imp {
    use super::{
        BrushCursor, CHECKER_TILE, Cell, ColorPickerOverlay, CropOverlay, CropRect, PendingMarquee,
        Point, RefCell, TransformRect, draw_brush_cursor_cairo, draw_color_picker_cairo,
        draw_crop_overlay_cairo, draw_pixel_grid_cairo, draw_selection_overlay_cairo,
        draw_text_edit_overlay_cairo, draw_transform_overlay_cairo, gdk, glib, graphene, gsk, gtk,
    };
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    pub(crate) struct CanvasPaintable {
        pub(super) texture: RefCell<Option<gdk::Texture>>,
        pub(super) checker: RefCell<Option<gdk::MemoryTexture>>,
        pub(super) canvas_w: Cell<u32>,
        pub(super) canvas_h: Cell<u32>,
        pub(super) pan_x: Cell<f32>,
        pub(super) pan_y: Cell<f32>,
        pub(super) zoom: Cell<f32>,
        // crop overlay
        pub(super) crop_rect: Cell<Option<CropRect>>,
        pub(super) crop_overlay: Cell<CropOverlay>,
        pub(super) crop_active: Cell<bool>,
        // transform overlay
        pub(super) transform_rect: Cell<Option<TransformRect>>,
        pub(super) transform_active: Cell<bool>,
        /// True while the renderer composites the warped layer itself (live GPU
        /// blend preview); the GSK source/above overlay is then suppressed.
        pub(super) transform_gpu_preview: Cell<bool>,
        /// GPU texture holding the captured source pixels for live preview.
        /// Sampled with an affine transform by GSK at draw time.
        pub(super) transform_source_texture: RefCell<Option<gdk::Texture>>,
        /// Original bounding rect at the time the transform was activated.
        pub(super) transform_original_rect: Cell<Option<TransformRect>>,
        /// Composite of the layers above the transformed one, captured at
        /// activation. Drawn on top of the live preview so the transformed
        /// layer stays in its z-order instead of floating above everything.
        pub(super) transform_above_texture: RefCell<Option<gdk::Texture>>,
        // selection overlay
        pub(super) selection_contours: RefCell<Vec<Vec<Point>>>,
        pub(super) selection_pending: RefCell<Option<PendingMarquee>>,
        pub(super) selection_ants_offset: Cell<f64>,
        // pixel view (nearest-neighbour + grid at high zoom)
        pub(super) pixel_view_enabled: Cell<bool>,
        pub(super) nearest_threshold: Cell<f32>,
        pub(super) grid_enabled: Cell<bool>,
        pub(super) grid_threshold: Cell<f32>,
        // brush footprint cursor
        pub(super) brush_cursor: RefCell<Option<BrushCursor>>,
        pub(super) brush_cursor_anchor: Cell<Point>,
        // color-picker loupe + eyedropper
        pub(super) color_picker: Cell<Option<ColorPickerOverlay>>,
        // edit-mode chrome (main canvas vs component): dim label + accent border
        pub(super) edit_label: RefCell<String>,
        pub(super) edit_bordered: Cell<bool>,
        pub(super) edit_accent: Cell<(f32, f32, f32)>,
        /// Cached pill texture for `edit_label` + its (width, height) in pixels.
        /// Rebuilt only when the label changes, so the per-frame snapshot path
        /// never re-shapes the text or allocates a full-widget cairo surface.
        pub(super) edit_label_cache: RefCell<Option<(gdk::MemoryTexture, f32, f32)>>,
        // text editing overlay (box outline + caret + selection, canvas coords)
        pub(super) text_edit_active: Cell<bool>,
        pub(super) text_edit_box: Cell<Option<TransformRect>>,
        pub(super) text_caret: Cell<Option<(f32, f32, f32, f32)>>,
        pub(super) text_caret_visible: Cell<bool>,
        pub(super) text_selection: RefCell<Vec<(f32, f32, f32, f32)>>,
        /// Resize-handle centres in canvas coordinates.
        pub(super) text_handles: RefCell<Vec<(f32, f32)>>,
        /// Rubber-band box drawn while dragging out a new text box (canvas coords).
        pub(super) text_pending_box: Cell<Option<TransformRect>>,
        /// Anamorphic display scale `(sx, sy)` of the text box being edited.
        /// `text_edit_box` is the natural box; caret/selection/handles are in
        /// natural-local coords, so the overlay applies this scale when drawing.
        pub(super) text_scale: Cell<(f32, f32)>,
        /// Frame-time/FPS performance overlay (toggle with F3). Records one
        /// sample per snapshot and paints itself in the top-left corner.
        pub(super) perf: RefCell<super::PerfGraph>,
    }

    impl Default for CanvasPaintable {
        fn default() -> Self {
            Self {
                texture: RefCell::new(None),
                checker: RefCell::new(None),
                canvas_w: Cell::new(0),
                canvas_h: Cell::new(0),
                pan_x: Cell::new(0.0),
                pan_y: Cell::new(0.0),
                zoom: Cell::new(1.0),
                crop_rect: Cell::new(None),
                crop_overlay: Cell::new(CropOverlay::default()),
                crop_active: Cell::new(false),
                transform_rect: Cell::new(None),
                transform_active: Cell::new(false),
                transform_gpu_preview: Cell::new(false),
                transform_source_texture: RefCell::new(None),
                transform_original_rect: Cell::new(None),
                transform_above_texture: RefCell::new(None),
                selection_contours: RefCell::new(Vec::new()),
                selection_pending: RefCell::new(None),
                selection_ants_offset: Cell::new(0.0),
                pixel_view_enabled: Cell::new(true),
                nearest_threshold: Cell::new(4.0),
                grid_enabled: Cell::new(true),
                grid_threshold: Cell::new(8.0),
                brush_cursor: RefCell::new(None),
                brush_cursor_anchor: Cell::new(Point::ZERO),
                color_picker: Cell::new(None),
                edit_label: RefCell::new("Main canvas".to_string()),
                edit_bordered: Cell::new(false),
                edit_accent: Cell::new((0.21, 0.52, 0.89)),
                edit_label_cache: RefCell::new(None),
                text_edit_active: Cell::new(false),
                text_edit_box: Cell::new(None),
                text_caret: Cell::new(None),
                text_caret_visible: Cell::new(true),
                text_selection: RefCell::new(Vec::new()),
                text_handles: RefCell::new(Vec::new()),
                text_pending_box: Cell::new(None),
                text_scale: Cell::new((1.0, 1.0)),
                perf: RefCell::new(super::PerfGraph::default()),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CanvasPaintable {
        const NAME: &'static str = "OxieDrawCanvasPaintable";
        type Type = super::CanvasPaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for CanvasPaintable {}

    impl CanvasPaintable {
        /// Draw the color-picker loupe (magnified nearest-neighbour canvas
        /// clipped to a circle) and the eyedropper cursor. Split out of
        /// `snapshot` to keep that method readable.
        #[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
        fn draw_color_picker(
            &self,
            snapshot: &gdk::Snapshot,
            widget_rect: &graphene::Rect,
            picker: ColorPickerOverlay,
            pan_x: f32,
            pan_y: f32,
            zoom: f32,
            canvas_w: f32,
            canvas_h: f32,
        ) {
            let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
            let r = super::PICKER_MAG_RADIUS;
            // Always magnify past the current canvas zoom so the loupe shows
            // more detail than what's already on screen.
            let s = super::PICKER_MAG_SCALE.max(zoom * 2.0);
            let width = widget_rect.width();
            let height = widget_rect.height();

            // Canvas pixel under the cursor; map its centre to the loupe centre.
            let cxp = (picker.cursor.x - pan_x) / zoom;
            let cyp = (picker.cursor.y - pan_y) / zoom;
            let sample_cx = cxp.floor() + 0.5;
            let sample_cy = cyp.floor() + 0.5;

            // Loupe centre: up-and-left of the cursor (the eyedropper points
            // up-right, so this keeps them from overlapping), clamped on-screen.
            let cc_x = (picker.cursor.x - (r + 16.0)).clamp(r + 2.0, (width - r - 2.0).max(r + 2.0));
            let cc_y =
                (picker.cursor.y - (r + 16.0)).clamp(r + 2.0, (height - r - 2.0).max(r + 2.0));

            // canvas point P -> widget: cc + (P - sample_centre) * s.
            let origin_wx = cc_x - sample_cx * s;
            let origin_wy = cc_y - sample_cy * s;

            let bounds = graphene::Rect::new(cc_x - r, cc_y - r, 2.0 * r, 2.0 * r);
            gtk_snap.push_rounded_clip(&gsk::RoundedRect::from_rect(bounds, r));

            // Checker behind transparency, magnified + nearest.
            if let Some(checker) = self.checker.borrow().clone() {
                #[allow(clippy::cast_precision_loss)]
                let tile_w = CHECKER_TILE as f32 * s;
                let tile_rect = graphene::Rect::new(origin_wx, origin_wy, tile_w, tile_w);
                let tile_node =
                    gsk::TextureScaleNode::new(&checker, &tile_rect, gsk::ScalingFilter::Nearest);
                let repeat = gsk::RepeatNode::new(&bounds, &tile_node, Some(&tile_rect));
                gtk_snap.append_node(&repeat);
            }

            // Magnified canvas, nearest-neighbour.
            if let Some(texture) = self.texture.borrow().clone() {
                let node_rect =
                    graphene::Rect::new(origin_wx, origin_wy, canvas_w * s, canvas_h * s);
                let node =
                    gsk::TextureScaleNode::new(&texture, &node_rect, gsk::ScalingFilter::Nearest);
                gtk_snap.append_node(&node);
            }

            gtk_snap.pop();

            // Ring, sampled-pixel box, swatch + eyedropper (cairo on top).
            let cr = gtk_snap.append_cairo(widget_rect);
            draw_color_picker_cairo(
                &cr,
                picker.cursor,
                (f64::from(cc_x), f64::from(cc_y)),
                f64::from(r),
                f64::from(s),
                picker.color,
            );
        }
    }

    impl PaintableImpl for CanvasPaintable {
        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            // 1. Solid dark backdrop behind everything (off-canvas).
            #[allow(clippy::cast_possible_truncation)]
            let widget_rect = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
            snapshot.append_color(&gdk::RGBA::new(0.12, 0.12, 0.14, 1.0), &widget_rect);

            #[allow(clippy::cast_precision_loss)]
            let canvas_w = self.canvas_w.get() as f32;
            #[allow(clippy::cast_precision_loss)]
            let canvas_h = self.canvas_h.get() as f32;
            let pan_x = self.pan_x.get();
            let pan_y = self.pan_y.get();
            let zoom = self.zoom.get().max(f32::EPSILON);

            // Pixel-perfect mode: above the configured threshold the checker
            // *and* the canvas image are sampled with nearest-neighbour so
            // cell boundaries and individual pixels stay crisp. GSK's filter
            // hint only applies to scale-to-fit inside a node's bounds, so
            // both layers are emitted in widget space at this zoom.
            let use_nearest = self.pixel_view_enabled.get() && zoom >= self.nearest_threshold.get();

            let canvas_rect = graphene::Rect::new(0.0, 0.0, canvas_w, canvas_h);

            if use_nearest {
                let widget_canvas_rect =
                    graphene::Rect::new(pan_x, pan_y, canvas_w * zoom, canvas_h * zoom);

                // 2. Checker tile - widget-space scale node with NEAREST filter.
                if let Some(checker) = self.checker.borrow().clone() {
                    #[allow(clippy::cast_precision_loss)]
                    let tile_widget = CHECKER_TILE as f32 * zoom;
                    let tile_rect = graphene::Rect::new(pan_x, pan_y, tile_widget, tile_widget);
                    let tile_node = gsk::TextureScaleNode::new(
                        &checker,
                        &tile_rect,
                        gsk::ScalingFilter::Nearest,
                    );
                    let repeat =
                        gsk::RepeatNode::new(&widget_canvas_rect, &tile_node, Some(&tile_rect));
                    snapshot.append_node(&repeat);
                }

                // 3. Canvas texture - append_scaled_texture honours the filter.
                if let Some(texture) = self.texture.borrow().clone() {
                    let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                    gtk_snap.append_scaled_texture(
                        &texture,
                        gsk::ScalingFilter::Nearest,
                        &widget_canvas_rect,
                    );
                }
            } else {
                // Standard path: draw in canvas space; GSK default (linear)
                // sampling renders the outer zoom transform smoothly.
                snapshot.save();
                snapshot.translate(&graphene::Point::new(pan_x, pan_y));
                snapshot.scale(zoom, zoom);

                if let Some(checker) = self.checker.borrow().clone() {
                    #[allow(clippy::cast_precision_loss)]
                    let tile = CHECKER_TILE as f32;
                    let tile_rect = graphene::Rect::new(0.0, 0.0, tile, tile);
                    let tile_node = gsk::TextureNode::new(&checker, &tile_rect);
                    let repeat = gsk::RepeatNode::new(&canvas_rect, &tile_node, Some(&tile_rect));
                    snapshot.append_node(&repeat);
                }

                if let Some(texture) = self.texture.borrow().clone() {
                    snapshot.append_texture(&texture, &canvas_rect);
                }
                snapshot.restore();
            }

            // 3b. Live transform preview via GSK. Emitted in widget space so
            //     GSK's TextureScaleNode filter honours the configured
            //     nearest-neighbour threshold (the filter only applies to
            //     scale-to-fit inside the node's bounds, so the bounds have
            //     to absorb the outer canvas zoom).
            // When the GPU preview is live, the warped layer (with its blend
            // mode) is already in the presented dmabuf; skip the GSK overlay so
            // it isn't drawn twice. The handle box is drawn separately below.
            if self.transform_active.get()
                && !self.transform_gpu_preview.get()
                && let (Some(texture), Some(rect), Some(orig)) = (
                    self.transform_source_texture.borrow().clone(),
                    self.transform_rect.get(),
                    self.transform_original_rect.get(),
                )
            {
                let filter = if use_nearest {
                    gsk::ScalingFilter::Nearest
                } else {
                    gsk::ScalingFilter::Linear
                };
                let sx = if orig.w > 0.0 { rect.w / orig.w } else { 1.0 };
                let sy = if orig.h > 0.0 { rect.h / orig.h } else { 1.0 };
                let final_sx = sx * zoom;
                let final_sy = sy * zoom;
                let center_wx = rect.cx.mul_add(zoom, pan_x);
                let center_wy = rect.cy.mul_add(zoom, pan_y);

                snapshot.save();
                snapshot.translate(&graphene::Point::new(center_wx, center_wy));
                snapshot.rotate(rect.angle.to_degrees());
                #[allow(clippy::cast_precision_loss)]
                let local_rect = graphene::Rect::new(
                    -orig.cx * final_sx,
                    -orig.cy * final_sy,
                    texture.width() as f32 * final_sx,
                    texture.height() as f32 * final_sy,
                );
                let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                gtk_snap.append_scaled_texture(&texture, filter, &local_rect);
                snapshot.restore();

                // Layers above the transformed one, redrawn on top of the
                // preview so the transformed layer stays in its z-order
                // instead of floating above everything. Matches the base
                // canvas's filtering (nearest in pixel-view, else linear).
                if let Some(above) = self.transform_above_texture.borrow().clone() {
                    if use_nearest {
                        let widget_canvas_rect =
                            graphene::Rect::new(pan_x, pan_y, canvas_w * zoom, canvas_h * zoom);
                        let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                        gtk_snap.append_scaled_texture(
                            &above,
                            gsk::ScalingFilter::Nearest,
                            &widget_canvas_rect,
                        );
                    } else {
                        snapshot.save();
                        snapshot.translate(&graphene::Point::new(pan_x, pan_y));
                        snapshot.scale(zoom, zoom);
                        snapshot.append_texture(&above, &canvas_rect);
                        snapshot.restore();
                    }
                }
            }

            // Re-enter canvas space for the document border.
            snapshot.save();
            snapshot.translate(&graphene::Point::new(pan_x, pan_y));
            snapshot.scale(zoom, zoom);

            // 4. Document-edge border.
            let border_thickness = 1.0_f32.max(1.0 / zoom);
            let border_color = gdk::RGBA::new(0.0, 0.0, 0.0, 0.45);
            snapshot.append_color(
                &border_color,
                &graphene::Rect::new(0.0, 0.0, canvas_w, border_thickness),
            );
            snapshot.append_color(
                &border_color,
                &graphene::Rect::new(0.0, canvas_h - border_thickness, canvas_w, border_thickness),
            );
            snapshot.append_color(
                &border_color,
                &graphene::Rect::new(0.0, 0.0, border_thickness, canvas_h),
            );
            snapshot.append_color(
                &border_color,
                &graphene::Rect::new(canvas_w - border_thickness, 0.0, border_thickness, canvas_h),
            );

            // 4a. Component edit-mode accent border, drawn just *outside* the
            //     canvas so it never covers any drawing-zone pixels.
            if self.edit_bordered.get() {
                let (ar, ag, ab) = self.edit_accent.get();
                let accent = gdk::RGBA::new(ar, ag, ab, 1.0);
                let t = 2.0_f32.max(2.0 / zoom);
                // Top + bottom span the full outer width; left + right fill the
                // gaps along the canvas height.
                snapshot.append_color(&accent, &graphene::Rect::new(-t, -t, canvas_w + 2.0 * t, t));
                snapshot.append_color(
                    &accent,
                    &graphene::Rect::new(-t, canvas_h, canvas_w + 2.0 * t, t),
                );
                snapshot.append_color(&accent, &graphene::Rect::new(-t, 0.0, t, canvas_h));
                snapshot.append_color(&accent, &graphene::Rect::new(canvas_w, 0.0, t, canvas_h));
            }

            snapshot.restore();

            // 4a-2. Edit-mode dim label, top-left in widget space. The pill is
            //       pre-rendered into a cached texture (rebuilt only when the
            //       label changes), so the hot path just positions it - no
            //       per-frame text shaping or full-widget cairo surface.
            {
                if self.edit_label_cache.borrow().is_none() {
                    let label = self.edit_label.borrow().clone();
                    if !label.is_empty() {
                        *self.edit_label_cache.borrow_mut() =
                            super::render_edit_label_texture(&label);
                    }
                }
                if let Some((texture, pill_w, pill_h)) = self.edit_label_cache.borrow().as_ref() {
                    // Float just above the canvas top edge, anchored to the
                    // canvas (widget space), not the viewport. Snap to whole
                    // pixels so the cached glyphs stay crisp at fractional pan.
                    let gap = 12.0;
                    let x = (pan_x).round();
                    let y = (pan_y - pill_h - gap).round();
                    snapshot.append_texture(texture, &graphene::Rect::new(x, y, *pill_w, *pill_h));
                }
            }

            // 4b. Pixel grid - drawn in widget space, just above the canvas
            //     border. Only visible above the configured zoom threshold.
            if self.pixel_view_enabled.get()
                && self.grid_enabled.get()
                && zoom >= self.grid_threshold.get()
            {
                let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                #[allow(clippy::cast_possible_truncation)]
                let cr = gtk_snap.append_cairo(&widget_rect);
                #[allow(clippy::cast_possible_truncation)]
                draw_pixel_grid_cairo(
                    &cr,
                    width as f32,
                    height as f32,
                    self.canvas_w.get(),
                    self.canvas_h.get(),
                    pan_x,
                    pan_y,
                    zoom,
                );
            }

            // 5. Crop overlay - drawn in widget space after the canvas
            //    transform is popped, so coordinates are widget pixels.
            if self.crop_active.get()
                && let Some(rect) = self.crop_rect.get()
            {
                // GTK4 always calls snapshot() with a gtk::Snapshot; the
                // gdk::Snapshot parameter is just the base-class type.
                let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                let cr = gtk_snap.append_cairo(&widget_rect);
                #[allow(clippy::cast_possible_truncation)]
                draw_crop_overlay_cairo(
                    &cr,
                    width as i32,
                    height as i32,
                    rect,
                    self.crop_overlay.get(),
                    pan_x,
                    pan_y,
                    zoom,
                );
            }

            // 6. Transform handles + dashed border in widget space (cairo).
            //    The live pixel preview was already drawn via GSK in step 3b.
            if self.transform_active.get()
                && let Some(rect) = self.transform_rect.get()
            {
                let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                let cr = gtk_snap.append_cairo(&widget_rect);
                draw_transform_overlay_cairo(&cr, rect, pan_x, pan_y, zoom);
            }

            // 6a. Text editing overlay: box outline + selection + caret.
            if self.text_edit_active.get()
                && let Some(box_rect) = self.text_edit_box.get()
            {
                let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                let cr = gtk_snap.append_cairo(&widget_rect);
                draw_text_edit_overlay_cairo(
                    &cr,
                    box_rect,
                    self.text_caret.get(),
                    self.text_caret_visible.get(),
                    &self.text_selection.borrow(),
                    &self.text_handles.borrow(),
                    self.text_scale.get(),
                    pan_x,
                    pan_y,
                    zoom,
                );
            }

            // 6a-bis. Rubber-band outline while dragging out a new text box.
            if let Some(rect) = self.text_pending_box.get() {
                let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                let cr = gtk_snap.append_cairo(&widget_rect);
                let lx = f64::from(pan_x + (rect.cx - rect.half_w()) * zoom);
                let ly = f64::from(pan_y + (rect.cy - rect.half_h()) * zoom);
                cr.set_source_rgba(0.21, 0.52, 0.89, 0.9);
                cr.set_line_width(1.0);
                cr.set_dash(&[5.0, 3.0], 0.0);
                cr.rectangle(lx, ly, f64::from(rect.w * zoom), f64::from(rect.h * zoom));
                cr.stroke().ok();
                cr.set_dash(&[], 0.0);
            }

            // 6b. Brush footprint cursor - inverse-blended outline of
            //     what the active brush would paint at the current
            //     pointer position. Drawn before selection ants so the
            //     marching pattern stays on top when both are visible.
            // Drawn into a full-widget cairo node on purpose: any partial
            // (sub-region) repaint of the canvas re-rasterises the checker
            // `RepeatNode` into an offscreen with a shifted sub-pixel origin,
            // which leaves dark tile-boundary seams (and trails as the cursor
            // moves). A full-widget node forces a full repaint, matching every
            // other overlay here.
            let brush_cursor = self.brush_cursor.borrow();
            if let Some(cursor) = brush_cursor.as_ref() {
                let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                let cr = gtk_snap.append_cairo(&widget_rect);
                draw_brush_cursor_cairo(
                    &cr,
                    cursor,
                    self.brush_cursor_anchor.get(),
                    pan_x,
                    pan_y,
                    zoom,
                );
            }
            drop(brush_cursor);

            // 7. Selection overlay: marching ants + in-flight rubber-band.
            //    Drawn unconditionally - the selection tool isn't required to
            //    be active; ants remain visible across tool switches.
            let contours = self.selection_contours.borrow();
            let pending = self.selection_pending.borrow();
            if !contours.is_empty() || pending.is_some() {
                let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                let cr = gtk_snap.append_cairo(&widget_rect);
                draw_selection_overlay_cairo(
                    &cr,
                    &contours,
                    pending.as_ref(),
                    pan_x,
                    pan_y,
                    zoom,
                    self.selection_ants_offset.get(),
                );
            }

            // 8. Color-picker loupe + eyedropper. The magnified canvas is
            //    drawn via GSK (nearest-neighbour) clipped to a circle so
            //    individual pixels stay crisp; the ring, sampled-pixel box,
            //    swatch and eyedropper are cairo on top.
            if let Some(picker) = self.color_picker.get() {
                self.draw_color_picker(
                    snapshot,
                    &widget_rect,
                    picker,
                    pan_x,
                    pan_y,
                    zoom,
                    canvas_w,
                    canvas_h,
                );
            }

            // 9. Performance overlay (F3): records this frame's interval and
            //    paints the graphs + readouts on top of everything else.
            {
                let mut perf = self.perf.borrow_mut();
                if perf.enabled() {
                    let gtk_snap = unsafe { snapshot.unsafe_cast_ref::<gtk::Snapshot>() };
                    let cr = gtk_snap.append_cairo(&widget_rect);
                    perf.render(&cr);
                }
            }
        }

        fn intrinsic_width(&self) -> i32 {
            0 // expand to widget bounds
        }

        fn intrinsic_height(&self) -> i32 {
            0
        }

        fn flags(&self) -> gdk::PaintableFlags {
            gdk::PaintableFlags::empty()
        }
    }
}
