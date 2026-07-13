//! On-canvas text editing controller.
//!
//! Owns the live [`TextEditor`] for the text layer currently being edited and
//! drives the whole loop: enter/exit edit mode, route keyboard and pointer
//! input, re-render the layer slot per keystroke, keep the caret/selection
//! overlay in sync, blink the caret, and record undo history on commit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::ColorState;
use oxiedraw_core::document::LayerKind;
use oxiedraw_core::history::{HistoryAction, HistoryStack, LayerPatch};
use oxiedraw_core::text::editor::TextEditor;
use oxiedraw_core::text::fonts::TextEngine;
use oxiedraw_core::text::{FontId, HAlign, ResizeMode, TextBox, TextContent, TextStyle, VAlign};
use oxiedraw_utils::geometry::{Point, Size};
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::canvas::RedrawHandle;
use crate::canvas_paintable::CanvasPaintable;

/// Caret blink half-period.
const BLINK_MS: u64 = 530;

/// Hit tolerance for a resize handle, in widget pixels.
const HANDLE_HIT_PX: f32 = 9.0;

/// A box resize handle. Which ones are shown depends on the resize mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextHandle {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// In-flight handle drag.
#[derive(Clone, Copy)]
struct ResizeDrag {
    handle: TextHandle,
    start_box: TextBox,
    start_pt: Point,
}

struct Active {
    editor: TextEditor,
    layer_id: String,
    /// `true` when this session created a brand-new layer, so committing an
    /// empty box drops it and a non-empty commit records `LayerAdd`.
    created: bool,
    /// Layer pixels before this edit (baseline for the history patch).
    before_pixels: Vec<u8>,
    /// Content before this edit (for `TextEdit` undo).
    before_content: TextContent,
    /// `true` while a pointer drag is extending the selection.
    selecting: bool,
    /// `Some` while a resize handle is being dragged.
    resize: Option<ResizeDrag>,
    /// Canvas-pixel AABB `(x, y, w, h)` of the box at the last slot upload.
    /// The next keystroke uploads the union of this and the new AABB so glyphs
    /// left behind by a shrink/move are cleared. See [`TextEdit::rerender`].
    last_region: Option<(i32, i32, u32, u32)>,
}

#[derive(Clone)]
pub(crate) struct TextEdit {
    active: Rc<RefCell<Option<Active>>>,
    canvas: Rc<RefCell<Canvas>>,
    engine: Rc<RefCell<TextEngine>>,
    history: Rc<RefCell<HistoryStack>>,
    paintable: CanvasPaintable,
    redraw: RedrawHandle,
    refresh_layers: Rc<dyn Fn()>,
    colors: ColorState,
    canvas_size: Rc<Cell<Size>>,
    zoom: Rc<Cell<f32>>,
    blink_timer: Rc<RefCell<Option<glib::SourceId>>>,
    /// Listeners notified whenever the editing state changes (enter/exit/caret/
    /// edit), so the right-bar properties panel can refresh.
    changed: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
}

/// Snapshot of the editing state for the right-bar "Editing text" panel.
pub(crate) struct TextProps {
    pub font: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub h_align: HAlign,
    pub v_align: VAlign,
    pub resize: ResizeMode,
}

impl TextEdit {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        canvas: Rc<RefCell<Canvas>>,
        engine: Rc<RefCell<TextEngine>>,
        history: Rc<RefCell<HistoryStack>>,
        paintable: CanvasPaintable,
        redraw: RedrawHandle,
        refresh_layers: Rc<dyn Fn()>,
        colors: ColorState,
        canvas_size: Rc<Cell<Size>>,
        zoom: Rc<Cell<f32>>,
    ) -> Self {
        Self {
            active: Rc::new(RefCell::new(None)),
            canvas,
            engine,
            history,
            paintable,
            redraw,
            refresh_layers,
            colors,
            canvas_size,
            zoom,
            blink_timer: Rc::new(RefCell::new(None)),
            changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Register a listener fired whenever the editing state changes.
    pub(crate) fn connect_changed(&self, cb: Box<dyn Fn()>) {
        self.changed.borrow_mut().push(cb);
    }

    fn notify_changed(&self) {
        for cb in self.changed.borrow().iter() {
            cb();
        }
    }

    /// Render the live Transform source for `content` at total scale `(sx, sy)`
    /// over its natural box (see
    /// [`oxiedraw_core::text::render::render_visible_local`]). Keeps a scaling
    /// drag sharp instead of magnifying a fixed-res raster.
    pub(crate) fn render_scaled_source(
        &self,
        content: &TextContent,
        sx: f32,
        sy: f32,
    ) -> (Vec<u8>, u32, u32) {
        let mut engine = self.engine.borrow_mut();
        oxiedraw_core::text::render::render_visible_local(content, sx, sy, &mut engine)
    }

    /// Current editing properties for the panel, or `None` when not editing.
    #[must_use]
    pub(crate) fn props(&self) -> Option<TextProps> {
        let act = self.active.borrow();
        let a = act.as_ref()?;
        let style = a.editor.current_style();
        Some(TextProps {
            font: style.font.0,
            size: style.size,
            bold: style.bold,
            italic: style.italic,
            h_align: a.editor.h_align(),
            v_align: a.editor.v_align(),
            resize: a.editor.resize_mode(),
        })
    }

    pub(crate) fn set_font(&self, family: String) {
        self.edit(move |ed, eng| ed.set_font(eng, FontId::new(family)));
    }

    pub(crate) fn set_size(&self, size: f32) {
        self.edit(move |ed, eng| ed.set_size(eng, size));
    }

    pub(crate) fn set_face(&self, bold: bool, italic: bool) {
        self.edit(move |ed, eng| ed.set_face(eng, bold, italic));
    }

    pub(crate) fn set_h_align(&self, h: HAlign) {
        self.edit(move |ed, eng| ed.set_h_align(h, eng));
    }

    pub(crate) fn set_v_align(&self, v: VAlign) {
        self.edit(move |ed, _eng| ed.set_v_align(v));
    }

    pub(crate) fn set_resize_mode(&self, m: ResizeMode) {
        self.edit(move |ed, eng| ed.set_resize_mode(m, eng));
    }

    /// Register a listener so that changing the colour while editing recolours
    /// the selection (or the whole box). Call once after construction.
    pub(crate) fn connect_color(&self) {
        let this = self.clone();
        self.colors.connect_changed(Box::new(move || {
            if this.is_active() {
                let color = this.colors.current();
                this.edit(move |ed, eng| ed.set_color(eng, color));
            }
        }));
    }

    #[must_use]
    pub(crate) fn is_active(&self) -> bool {
        self.active.borrow().is_some()
    }

    pub(crate) fn toggle_bold(&self) {
        self.edit(TextEditor::toggle_bold);
    }

    pub(crate) fn toggle_italic(&self) {
        self.edit(TextEditor::toggle_italic);
    }

    pub(crate) fn toggle_underline(&self) {
        self.edit(TextEditor::toggle_underline);
    }

    // -- entry points ------------------------------------------------------

    /// Handle a pointer press at a canvas point. Returns `true` if the click
    /// was consumed (resize handle grabbed, caret placed in the active box, or
    /// an existing text layer entered); `false` if it missed all text (the
    /// caller should create one).
    pub(crate) fn pointer_press(&self, pt: Point) -> bool {
        // Grabbing a resize handle takes priority over caret placement.
        if let Some(handle) = self.hit_test_handle(pt) {
            self.begin_resize(handle, pt);
            return true;
        }
        if self.box_contains(pt) {
            self.place_caret(pt, false);
            self.set_selecting(true);
            return true;
        }
        // Clicked outside the current box: commit it, then try another layer.
        self.commit();
        if let Some((idx, id)) = self.hit_test(pt) {
            self.begin_existing(idx, id);
            self.place_caret(pt, false);
            self.set_selecting(true);
            return true;
        }
        false
    }

    pub(crate) fn pointer_motion(&self, pt: Point) {
        let resizing = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|a| a.resize.is_some());
        if resizing {
            self.resize_to(pt);
        } else if self.active.borrow().as_ref().is_some_and(|a| a.selecting) {
            self.place_caret(pt, true);
        }
    }

    pub(crate) fn pointer_release(&self) {
        if let Some(a) = self.active.borrow_mut().as_mut() {
            a.selecting = false;
            a.resize = None;
        }
    }

    /// Cursor name to show while the Text tool hovers at `pt`: a resize cursor
    /// over a handle, otherwise `None` (the caller defaults to a text cursor).
    #[must_use]
    pub(crate) fn cursor_for(&self, pt: Point) -> Option<&'static str> {
        self.hit_test_handle(pt).map(handle_cursor)
    }

    /// The resize handle near `pt`, if any (only active-box handles for the
    /// current resize mode are considered).
    fn hit_test_handle(&self, pt: Point) -> Option<TextHandle> {
        let act = self.active.borrow();
        let a = act.as_ref()?;
        let b = a.editor.box_rect();
        let scale = a.editor.scale();
        let mode = a.editor.resize_mode();
        let tol = HANDLE_HIT_PX / self.zoom.get().max(0.01);
        handles_for(mode).iter().copied().find(|&h| {
            let (hx, hy) = handle_canvas(b, scale, h);
            (pt.x - hx).hypot(pt.y - hy) <= tol
        })
    }

    fn begin_resize(&self, handle: TextHandle, pt: Point) {
        if let Some(a) = self.active.borrow_mut().as_mut() {
            a.resize = Some(ResizeDrag {
                handle,
                start_box: a.editor.box_rect(),
                start_pt: pt,
            });
        }
    }

    fn resize_to(&self, pt: Point) {
        let new_box = {
            let act = self.active.borrow();
            let Some(a) = act.as_ref() else { return };
            let Some(rd) = a.resize else {
                return;
            };
            let (sx, sy) = a.editor.scale();
            // Project the canvas drag delta onto the box's local axes so a
            // rotated box resizes along its own edges, then un-scale to the
            // natural box so the visible edge tracks the cursor and the text
            // re-wraps at the corresponding natural width.
            let (sa, ca) = rd.start_box.angle.sin_cos();
            let ddx = pt.x - rd.start_pt.x;
            let ddy = pt.y - rd.start_pt.y;
            let local_dx = ddx.mul_add(ca, ddy * sa) / sx;
            let local_dy = ddy.mul_add(ca, -ddx * sa) / sy;
            compute_resized_box(rd.start_box, rd.handle, local_dx, local_dy)
        };
        {
            let mut act = self.active.borrow_mut();
            let Some(a) = act.as_mut() else { return };
            let mut engine = self.engine.borrow_mut();
            a.editor.set_box(new_box, &mut engine);
        }
        self.rerender();
    }

    /// Create a new text layer and begin editing it. For `AutoWidth`, `anchor`
    /// is the top-left corner; for `Fixed`/`AutoHeight`, it is the box rect.
    pub(crate) fn create_and_edit(&self, anchor: TextBox, mode: ResizeMode) {
        self.commit();

        let color = self.colors.current();
        let style = {
            let engine = self.engine.borrow();
            TextStyle::new(FontId::new(engine.default_family()), color)
        };
        // Empty box: AutoWidth starts as a thin caret-height box at the anchor.
        let line_h = style.size * 1.2;
        let box_rect = match mode {
            ResizeMode::AutoWidth => {
                TextBox::new(anchor.cx + 0.5, anchor.cy + line_h / 2.0, 1.0, line_h, 0.0)
            }
            ResizeMode::AutoHeight | ResizeMode::Fixed => anchor,
        };
        let content = TextContent::empty(box_rect, mode, style);

        let cs = self.canvas_size.get();
        let pixels = {
            let mut engine = self.engine.borrow_mut();
            oxiedraw_core::text::render::render_text(&content, &mut engine, cs.width, cs.height)
        };
        let idx = match self.canvas.borrow_mut().add_layer_with_pixels("Text", &pixels) {
            Ok(i) => i,
            Err(e) => {
                tracing::error!(error = %e, "text edit: add_layer_with_pixels failed");
                return;
            }
        };
        self.canvas
            .borrow()
            .layers()
            .set_kind(idx, LayerKind::Text(content.clone()));
        let id = self
            .canvas
            .borrow()
            .layers()
            .snapshot()
            .get(idx)
            .map_or_default_id();

        let editor = {
            let mut engine = self.engine.borrow_mut();
            TextEditor::from_content(&content, &mut engine)
        };
        let init_region = visible_aabb(box_rect, content.scale);
        *self.active.borrow_mut() = Some(Active {
            editor,
            layer_id: id,
            created: true,
            before_pixels: pixels,
            before_content: content,
            selecting: false,
            resize: None,
            last_region: Some(init_region),
        });
        (self.refresh_layers)();
        self.redraw.request();
        self.update_overlay();
        self.start_blink();
    }

    /// Enter edit mode on an existing text layer.
    fn begin_existing(&self, idx: usize, id: String) {
        let Some(LayerKind::Text(content)) = self.canvas.borrow().layers().kind(idx) else {
            return;
        };
        let before_pixels = match self.canvas.borrow_mut().read_layer(idx) {
            Ok(px) => px,
            Err(e) => {
                tracing::error!(error = %e, "text edit: read_layer failed");
                return;
            }
        };
        let editor = {
            let mut engine = self.engine.borrow_mut();
            TextEditor::from_content(&content, &mut engine)
        };
        let init_region = visible_aabb(content.box_rect, content.scale);
        *self.active.borrow_mut() = Some(Active {
            editor,
            layer_id: id,
            created: false,
            before_pixels,
            before_content: content,
            selecting: false,
            resize: None,
            last_region: Some(init_region),
        });
        self.update_overlay();
        self.start_blink();
    }

    /// Commit the current edit: write the final content + pixels to the layer
    /// and record history. Empty brand-new boxes are dropped.
    pub(crate) fn commit(&self) {
        let Some(mut a) = self.active.borrow_mut().take() else {
            return;
        };
        self.stop_blink();
        self.paintable
            .set_text_edit(false, None, None, Vec::new(), Vec::new(), (1.0, 1.0));
        self.notify_changed();

        let content = a.editor.to_content();
        let empty = content.is_empty();
        let Some(idx) = self.find_idx(&a.layer_id) else {
            self.redraw.request();
            return;
        };
        let cs = self.canvas.borrow().size();

        if empty && a.created {
            let _ = self.canvas.borrow_mut().remove_layer(idx);
            (self.refresh_layers)();
            self.redraw.request();
            return;
        }

        let pixels = {
            let mut engine = self.engine.borrow_mut();
            a.editor.render_into_slot(&mut engine, cs.width, cs.height)
        };
        self.canvas
            .borrow()
            .layers()
            .set_kind(idx, LayerKind::Text(content.clone()));
        if let Err(e) = self.canvas.borrow_mut().restore_layer(idx, &pixels) {
            tracing::error!(error = %e, "text edit: restore_layer failed");
        }

        if a.created {
            if let Some((lid, name, visible, kind, blend, opacity, px)) =
                oxiedraw_core::history::capture_layer(&mut self.canvas.borrow_mut(), idx)
            {
                self.history.borrow_mut().record(HistoryAction::LayerAdd {
                    idx,
                    id: lid,
                    name,
                    visible,
                    layer_kind: kind,
                    blend,
                    opacity,
                    pixels: px,
                });
            }
        } else if let Some(patch) =
            LayerPatch::from_full_diff(&a.before_pixels, &pixels, cs.width, cs.height)
        {
            self.history.borrow_mut().record(HistoryAction::TextEdit {
                layer_id: a.layer_id.clone(),
                patch,
                before_content: Box::new(a.before_content.clone()),
                after_content: Box::new(content),
            });
        }

        (self.refresh_layers)();
        self.redraw.request();
    }

    // -- keyboard ----------------------------------------------------------

    /// Route a key press to the editor. Returns whether the event was handled.
    pub(crate) fn handle_key(&self, key: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        if !self.is_active() {
            return glib::Propagation::Proceed;
        }
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);

        match key {
            gdk::Key::Escape => {
                self.commit();
                return glib::Propagation::Stop;
            }
            gdk::Key::BackSpace => self.edit(TextEditor::backspace),
            gdk::Key::Delete => self.edit(TextEditor::delete),
            gdk::Key::Return | gdk::Key::KP_Enter => self.edit(TextEditor::enter),
            gdk::Key::Left if ctrl => self.edit(|ed, eng| ed.move_word_left(eng, shift)),
            gdk::Key::Right if ctrl => self.edit(|ed, eng| ed.move_word_right(eng, shift)),
            gdk::Key::Left => self.edit(|ed, eng| ed.move_left(eng, shift)),
            gdk::Key::Right => self.edit(|ed, eng| ed.move_right(eng, shift)),
            gdk::Key::Up => self.edit(|ed, eng| ed.move_up(eng, shift)),
            gdk::Key::Down => self.edit(|ed, eng| ed.move_down(eng, shift)),
            gdk::Key::Home => self.edit(|ed, eng| ed.move_home(eng, shift)),
            gdk::Key::End => self.edit(|ed, eng| ed.move_end(eng, shift)),
            gdk::Key::a if ctrl => self.edit(TextEditor::select_all),
            gdk::Key::c if ctrl => self.clipboard_copy(false),
            gdk::Key::x if ctrl => self.clipboard_copy(true),
            gdk::Key::v if ctrl => self.clipboard_paste(),
            _ => {
                // Printable character (no Ctrl/Alt).
                if ctrl || state.contains(gdk::ModifierType::ALT_MASK) {
                    return glib::Propagation::Proceed;
                }
                if let Some(c) = key.to_unicode()
                    && !c.is_control()
                {
                    self.edit(|ed, eng| ed.insert_char(eng, c));
                } else {
                    return glib::Propagation::Proceed;
                }
            }
        }
        glib::Propagation::Stop
    }

    // -- internals ---------------------------------------------------------

    /// Run an editor operation, then re-render + refresh the overlay.
    fn edit(&self, f: impl FnOnce(&mut TextEditor, &mut TextEngine)) {
        {
            let mut act = self.active.borrow_mut();
            let Some(a) = act.as_mut() else { return };
            let mut engine = self.engine.borrow_mut();
            f(&mut a.editor, &mut engine);
        }
        self.rerender();
    }

    /// Re-render the editor's text into its layer slot and refresh the overlay.
    /// Only the dirty rectangle - the union of the previous and current box
    /// AABBs - is rendered and uploaded, so a keystroke costs a small region
    /// upload instead of a full canvas re-render + transfer.
    fn rerender(&self) {
        let cs = self.canvas.borrow().size();
        let rendered = {
            let mut act = self.active.borrow_mut();
            act.as_mut().map(|a| {
                let cur = visible_aabb(a.editor.box_rect(), a.editor.scale());
                let region = a.last_region.map_or(cur, |prev| union_region(prev, cur));
                let region = clamp_region(region, cs.width, cs.height);
                a.last_region = Some(cur);
                let mut engine = self.engine.borrow_mut();
                let pixels =
                    a.editor.render_region(&mut engine, region.0, region.1, region.2, region.3);
                (a.layer_id.clone(), region, pixels)
            })
        };
        if let Some((id, region, pixels)) = rendered
            && let Some(idx) = self.find_idx(&id)
        {
            let (x, y, w, h) = region;
            if let Err(e) = self
                .canvas
                .borrow_mut()
                .restore_layer_region(idx, x, y, w, h, &pixels)
            {
                tracing::error!(error = %e, "text edit: restore_layer_region failed");
            }
            self.redraw.request();
        }
        self.update_overlay();
        self.reset_blink();
    }

    fn place_caret(&self, pt: Point, dragging: bool) {
        {
            let mut act = self.active.borrow_mut();
            let Some(a) = act.as_mut() else { return };
            let b = a.editor.box_rect();
            let (sx, sy) = a.editor.scale();
            // Canvas -> visible box-local offset, then un-scale to the editor's
            // natural coordinates (top-left origin).
            let (vlx, vly) = b.to_rect().canvas_to_local(pt.x, pt.y);
            let (lx, ly) = (vlx / sx + b.w / 2.0, vly / sy + b.h / 2.0);
            let mut engine = self.engine.borrow_mut();
            if dragging {
                a.editor.drag(&mut engine, lx, ly);
            } else {
                a.editor.click(&mut engine, lx, ly);
            }
        }
        self.update_overlay();
        self.reset_blink();
    }

    fn update_overlay(&self) {
        let mut act = self.active.borrow_mut();
        let Some(a) = act.as_mut() else {
            self.paintable
                .set_text_edit(false, None, None, Vec::new(), Vec::new(), (1.0, 1.0));
            return;
        };
        let mut engine = self.engine.borrow_mut();
        // Caret/selection/handles are all in natural box-local coords (top-left
        // origin); the paintable applies the box transform AND the anamorphic
        // scale when drawing.
        let caret = a.editor.caret_rect(&mut engine);
        let sel = a.editor.selection_rects(&mut engine);
        let b = a.editor.box_rect();
        let scale = a.editor.scale();
        let mode = a.editor.resize_mode();
        let handles: Vec<(f32, f32)> = handles_for(mode)
            .iter()
            .map(|&h| handle_point(b, h))
            .collect();
        let box_rect = b.to_rect();
        drop(engine);
        drop(act);
        self.paintable
            .set_text_edit(true, Some(box_rect), caret, sel, handles, scale);
        self.notify_changed();
    }

    fn box_contains(&self, pt: Point) -> bool {
        let act = self.active.borrow();
        act.as_ref()
            .is_some_and(|a| box_hit(a.editor.box_rect(), a.editor.scale(), pt))
    }

    fn set_selecting(&self, selecting: bool) {
        if let Some(a) = self.active.borrow_mut().as_mut() {
            a.selecting = selecting;
        }
    }

    /// Topmost visible text layer whose box contains `pt`.
    fn hit_test(&self, pt: Point) -> Option<(usize, String)> {
        let canvas = self.canvas.borrow();
        let layers = canvas.layers().snapshot();
        for idx in (0..layers.len()).rev() {
            if !layers[idx].visible {
                continue;
            }
            if let Some(LayerKind::Text(content)) = canvas.layers().kind(idx)
                && box_hit(content.box_rect, content.scale, pt)
            {
                return Some((idx, layers[idx].id.clone()));
            }
        }
        None
    }

    fn find_idx(&self, id: &str) -> Option<usize> {
        self.canvas
            .borrow()
            .layers()
            .snapshot()
            .iter()
            .position(|l| l.id == id)
    }

    // -- clipboard ---------------------------------------------------------

    fn clipboard_copy(&self, cut: bool) {
        let text = {
            let mut act = self.active.borrow_mut();
            let Some(a) = act.as_mut() else { return };
            if cut {
                let mut engine = self.engine.borrow_mut();
                a.editor.cut(&mut engine)
            } else {
                a.editor.copy()
            }
        };
        if let Some(text) = text {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
            if cut {
                self.rerender();
            }
        }
    }

    fn clipboard_paste(&self) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let this = self.clone();
        display.clipboard().read_text_async(
            gtk::gio::Cancellable::NONE,
            move |res| {
                if let Ok(Some(text)) = res {
                    let s = text.to_string();
                    if !s.is_empty() {
                        this.edit(|ed, eng| ed.insert_str(eng, &s));
                    }
                }
            },
        );
    }

    // -- caret blink -------------------------------------------------------

    fn start_blink(&self) {
        self.stop_blink();
        self.paintable.set_text_caret_visible(true);
        let paintable = self.paintable.clone();
        let active = Rc::clone(&self.active);
        let visible = Cell::new(true);
        let src = glib::timeout_add_local(std::time::Duration::from_millis(BLINK_MS), move || {
            if active.borrow().is_none() {
                return glib::ControlFlow::Break;
            }
            let v = !visible.get();
            visible.set(v);
            paintable.set_text_caret_visible(v);
            glib::ControlFlow::Continue
        });
        *self.blink_timer.borrow_mut() = Some(src);
    }

    /// Caret solid + restart the blink cycle (called after any edit so the
    /// caret doesn't blink away mid-typing).
    fn reset_blink(&self) {
        if self.is_active() {
            self.start_blink();
        }
    }

    fn stop_blink(&self) {
        if let Some(src) = self.blink_timer.borrow_mut().take() {
            src.remove();
        }
        self.paintable.set_text_caret_visible(true);
    }
}

/// The resize handles shown for a given resize mode. Auto Width hugs the
/// content so it has none; Auto Height exposes only the width (left/right)
/// handles; Fixed exposes all eight.
fn handles_for(mode: ResizeMode) -> &'static [TextHandle] {
    use TextHandle::{Bottom, BottomLeft, BottomRight, Left, Right, Top, TopLeft, TopRight};
    match mode {
        ResizeMode::AutoWidth => &[],
        ResizeMode::AutoHeight => &[Left, Right],
        ResizeMode::Fixed => &[
            Left, Right, Top, Bottom, TopLeft, TopRight, BottomLeft, BottomRight,
        ],
    }
}

/// The resize cursor name for a handle.
fn handle_cursor(h: TextHandle) -> &'static str {
    use TextHandle::{Bottom, BottomLeft, BottomRight, Left, Right, Top, TopLeft, TopRight};
    match h {
        Left | Right => "ew-resize",
        Top | Bottom => "ns-resize",
        TopLeft | BottomRight => "nwse-resize",
        TopRight | BottomLeft => "nesw-resize",
    }
}

/// Canvas-pixel AABB `(x, y, w, h)` of the visible (scaled, rotated) text box,
/// padded for anti-aliased glyph edges + underlines. Drives the per-keystroke
/// region upload (the box never paints outside this rect).
fn visible_aabb(b: TextBox, scale: (f32, f32)) -> (i32, i32, u32, u32) {
    let vr = TextBox::new(b.cx, b.cy, b.w * scale.0, b.h * scale.1, b.angle).to_rect();
    let (hw, hh) = (vr.w / 2.0, vr.h / 2.0);
    let corners = [
        vr.local_to_canvas(-hw, -hh),
        vr.local_to_canvas(hw, -hh),
        vr.local_to_canvas(hw, hh),
        vr.local_to_canvas(-hw, hh),
    ];
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (x, y) in corners {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    const PAD: f32 = 6.0;
    #[allow(clippy::cast_possible_truncation)]
    let x0 = (min_x - PAD).floor() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let y0 = (min_y - PAD).floor() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let x1 = (max_x + PAD).ceil() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let y1 = (max_y + PAD).ceil() as i32;
    #[allow(clippy::cast_sign_loss)]
    ((x0), (y0), (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
}

/// Bounding union of two canvas-pixel rects.
fn union_region(a: (i32, i32, u32, u32), b: (i32, i32, u32, u32)) -> (i32, i32, u32, u32) {
    let x0 = a.0.min(b.0);
    let y0 = a.1.min(b.1);
    let x1 = (a.0 + a.2 as i32).max(b.0 + b.2 as i32);
    let y1 = (a.1 + a.3 as i32).max(b.1 + b.3 as i32);
    #[allow(clippy::cast_sign_loss)]
    (x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
}

/// Clamp a region to the canvas so we never render/upload off-canvas area.
fn clamp_region(r: (i32, i32, u32, u32), cw: u32, ch: u32) -> (i32, i32, u32, u32) {
    let x0 = r.0.clamp(0, cw as i32);
    let y0 = r.1.clamp(0, ch as i32);
    let x1 = (r.0 + r.2 as i32).clamp(0, cw as i32);
    let y1 = (r.1 + r.3 as i32).clamp(0, ch as i32);
    #[allow(clippy::cast_sign_loss)]
    (x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
}

/// `true` if a canvas point is inside the (possibly rotated/scaled) text box.
/// `scale` is the box's anamorphic display scale; `b` is the natural box, so
/// the hit area is the natural box scaled by `scale`.
fn box_hit(b: TextBox, scale: (f32, f32), pt: Point) -> bool {
    // canvas_to_local uses only centre+angle, so this is the visible offset.
    let (vlx, vly) = b.to_rect().canvas_to_local(pt.x, pt.y);
    vlx.abs() <= b.w * scale.0 / 2.0 && vly.abs() <= b.h * scale.1 / 2.0
}

/// Box-local (top-left origin, 0..w / 0..h) position of a handle.
fn handle_point(b: TextBox, h: TextHandle) -> (f32, f32) {
    use TextHandle::{Bottom, BottomLeft, BottomRight, Left, Right, Top, TopLeft, TopRight};
    let (w, hh) = (b.w, b.h);
    match h {
        Left => (0.0, hh / 2.0),
        Right => (w, hh / 2.0),
        Top => (w / 2.0, 0.0),
        Bottom => (w / 2.0, hh),
        TopLeft => (0.0, 0.0),
        TopRight => (w, 0.0),
        BottomLeft => (0.0, hh),
        BottomRight => (w, hh),
    }
}

/// Canvas-space centre of a handle: the natural box-local point scaled to the
/// visible box, then mapped through the box transform.
fn handle_canvas(b: TextBox, scale: (f32, f32), h: TextHandle) -> (f32, f32) {
    let (lx, ly) = handle_point(b, h);
    let ox = (lx - b.w / 2.0) * scale.0;
    let oy = (ly - b.h / 2.0) * scale.1;
    b.to_rect().local_to_canvas(ox, oy)
}

/// New box after dragging `handle` by `(dx, dy)` **in the box's local axes**,
/// keeping the opposite edge fixed and enforcing a minimum size. Works for
/// rotated boxes: edges move along local axes and the centre is mapped back
/// through the box transform.
fn compute_resized_box(start: TextBox, handle: TextHandle, dx: f32, dy: f32) -> TextBox {
    use TextHandle::{Bottom, BottomLeft, BottomRight, Left, Right, Top, TopLeft, TopRight};
    const MIN: f32 = 8.0;

    // Centre-relative local edges.
    let mut l = -start.w / 2.0;
    let mut r = start.w / 2.0;
    let mut t = -start.h / 2.0;
    let mut b = start.h / 2.0;

    let moves_left = matches!(handle, Left | TopLeft | BottomLeft);
    let moves_right = matches!(handle, Right | TopRight | BottomRight);
    let moves_top = matches!(handle, Top | TopLeft | TopRight);
    let moves_bottom = matches!(handle, Bottom | BottomLeft | BottomRight);

    if moves_left {
        l = (l + dx).min(r - MIN);
    } else if moves_right {
        r = (r + dx).max(l + MIN);
    }
    if moves_top {
        t = (t + dy).min(b - MIN);
    } else if moves_bottom {
        b = (b + dy).max(t + MIN);
    }

    let (ncx, ncy) = start
        .to_rect()
        .local_to_canvas(f32::midpoint(l, r), f32::midpoint(t, b));
    TextBox::new(ncx, ncy, r - l, b - t, start.angle)
}

/// Small helper: an Option<&Layer> -> its id or empty string.
trait LayerIdExt {
    fn map_or_default_id(self) -> String;
}

impl LayerIdExt for Option<&oxiedraw_core::document::Layer> {
    fn map_or_default_id(self) -> String {
        self.map(|l| l.id.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_region, union_region, visible_aabb};
    use oxiedraw_core::text::TextBox;

    #[test]
    fn union_region_covers_both_rects() {
        assert_eq!(union_region((0, 0, 10, 10), (5, 5, 10, 10)), (0, 0, 15, 15));
        // Disjoint rects: the union spans the gap between them.
        assert_eq!(union_region((0, 0, 2, 2), (20, 0, 2, 2)), (0, 0, 22, 2));
    }

    #[test]
    fn clamp_region_clips_to_canvas() {
        assert_eq!(clamp_region((-5, -5, 20, 20), 10, 10), (0, 0, 10, 10));
        assert_eq!(clamp_region((2, 3, 4, 5), 100, 100), (2, 3, 4, 5));
        // Fully off-canvas collapses to zero area.
        assert_eq!(clamp_region((50, 50, 4, 4), 10, 10), (10, 10, 0, 0));
    }

    #[test]
    fn visible_aabb_axis_aligned_pads_box() {
        // Box spans x:[60,140], y:[30,70]; PAD = 6 on each side.
        let b = TextBox::new(100.0, 50.0, 80.0, 40.0, 0.0);
        assert_eq!(visible_aabb(b, (1.0, 1.0)), (54, 24, 92, 52));
    }

    #[test]
    fn visible_aabb_scale_widens_horizontally() {
        let b = TextBox::new(100.0, 50.0, 80.0, 40.0, 0.0);
        let (_, _, w1, _) = visible_aabb(b, (1.0, 1.0));
        let (_, _, w2, _) = visible_aabb(b, (2.0, 1.0));
        assert!(w2 > w1, "x-scale should widen the AABB ({w2} > {w1})");
    }

    #[test]
    fn visible_aabb_rotation_grows_aabb() {
        let b = TextBox::new(100.0, 100.0, 80.0, 40.0, 0.0);
        let (_, _, w0, h0) = visible_aabb(b, (1.0, 1.0));
        // A 45deg rotation must enlarge the axis-aligned bounds on both axes.
        let r = TextBox::new(100.0, 100.0, 80.0, 40.0, std::f32::consts::FRAC_PI_4);
        let (_, _, wr, hr) = visible_aabb(r, (1.0, 1.0));
        assert!(wr > w0 && hr > h0, "rotated AABB should be larger");
    }
}
