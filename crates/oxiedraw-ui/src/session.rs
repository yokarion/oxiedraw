//! Per-document session state and the shared global state.
//!
//! Multi-document support splits what used to be a single `EngineState` into
//! two halves: [`GlobalState`] holds state shared across every open tab
//! (brush library, colors, active tool, clipboard, toaster), while
//! [`DocumentSession`] owns everything specific to one document - its canvas,
//! layers, history, tool-interaction state, viewport, and the per-document
//! right bar / tool options widgets that get swapped into the window chrome
//! when the tab becomes active.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::brush_engine::BrushEngine;
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::ColorState;
use oxiedraw_core::components::{ComponentLayer, ComponentLibrary};
use oxiedraw_core::document::{
    ComponentInstance, Document, DocumentProperties, LayerKind, Placement,
};
use oxiedraw_core::history::{
    CropLayer, HistoryAction, HistoryConfig, HistoryStack, LayerPatch, SelectionSnapshot,
};
use oxiedraw_core::renderer::RendererError;
use oxiedraw_core::tools::{
    CropRect, CropState, FillState, GradientState, SelectionState, ShapeState, Tool, ToolState,
    TransformFilter, TransformRect, TransformState,
};
use oxiedraw_utils::geometry::Size;
use relm4::gtk;
use relm4::gtk::glib;

use crate::canvas::{self, Viewport};

/// Late-bound slot for the window-level "set the active tool" callback. It is
/// shared across all documents so per-document apply/cancel closures can switch
/// back to the Cursor tool without a forward reference.
pub(crate) type SetActiveToolSlot = Rc<RefCell<Option<Rc<dyn Fn(Tool)>>>>;

/// Off-canvas pixel data stored when a transform apply extends beyond the
/// canvas. Keyed by layer ID; lets the transform tool reload the full image on
/// re-activation.
pub(crate) struct LayerExtension {
    offset_x: i32,
    offset_y: i32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// State shared across every open document tab.
#[derive(Clone)]
pub(crate) struct GlobalState {
    pub(crate) brush_engine: BrushEngine,
    pub(crate) colors: ColorState,
    pub(crate) tools: ToolState,
    pub(crate) clipboard: Rc<RefCell<Option<crate::clipboard::LayerClipboard>>>,
    pub(crate) toaster: crate::toaster::Toaster,
    /// Shared text shaping engine + font database (system fonts plus any loaded
    /// from projects). Shared across documents since fonts are global.
    pub(crate) text_engine: Rc<RefCell<oxiedraw_core::text::fonts::TextEngine>>,
    /// Pre-rendered font-name previews for the font dropdown (shared).
    pub(crate) font_previews: crate::font_previews::FontPreviews,
    /// True while a project save is writing to disk. A single in-flight save is
    /// allowed at a time across all documents.
    pub(crate) save_in_progress: Rc<Cell<bool>>,
    /// Name of the brush that will be active on startup. Shared between
    /// the brush picker and manager so star-click in either updates both.
    pub(crate) default_brush_name: Rc<RefCell<Option<String>>>,
}

impl GlobalState {
    /// Build the global state with empty brush/font libraries. The expensive
    /// loading (brushes from disk, system fonts, font previews) is driven
    /// separately by the startup splash so it can show progress; see
    /// [`Self::load_brushes`]. The text engine starts empty and is filled
    /// font-by-font by the splash loader.
    pub(crate) fn new() -> Self {
        let settings = crate::settings::AppSettings::load();
        Self {
            brush_engine: BrushEngine::new(),
            colors: ColorState::new(),
            tools: ToolState::new(),
            clipboard: Rc::new(RefCell::new(None)),
            toaster: crate::toaster::Toaster::new(),
            text_engine: Rc::new(RefCell::new(oxiedraw_core::text::fonts::TextEngine::empty())),
            font_previews: crate::font_previews::FontPreviews::new(),
            save_in_progress: Rc::new(Cell::new(false)),
            default_brush_name: Rc::new(RefCell::new(settings.default_brush_name)),
        }
    }

    /// Seed + load the on-disk brush library (the splash's "Loading brushes"
    /// step). Brushes live under $XDG_CONFIG_HOME/oxiedraw/brushes; built-in
    /// `.oxiebrush` archives are seeded on first launch, then everything in the
    /// directory is loaded (overriding the hard-coded builtins).
    pub(crate) fn load_brushes(&self) {
        let brush_engine = &self.brush_engine;
        if let Some(dir) = oxiedraw_core::brush_engine::BrushRegistry::config_dir() {
            match oxiedraw_core::brush_engine::builtins::seed_missing(&dir) {
                Ok(0) => {}
                Ok(n) => tracing::info!(dir = ?dir, count = n, "seeded built-in brushes"),
                Err(e) => tracing::warn!(dir = ?dir, %e, "failed to seed built-in brushes"),
            }
            brush_engine.clear_brushes();
            let loaded = brush_engine.load_brushes_from_dir(&dir);
            if loaded == 0 {
                brush_engine.add_brush(oxiedraw_core::brush_engine::builtins::fallback_brush());
                tracing::warn!(dir = ?dir, "no brushes loaded - falling back to single default");
            } else {
                tracing::info!(dir = ?dir, count = loaded, "loaded brushes");
            }
            // Select the default brush by name. Falls back through
            // "Ink Pen" -> "Default Round" -> first if not found.
            {
                let settings = crate::settings::AppSettings::load();
                let brushes = brush_engine.brushes.borrow();
                let target = settings.default_brush_name
                    .as_deref()
                    .and_then(|name| brushes.iter().find(|p| p.name == name))
                    .or_else(|| brushes.iter().find(|p| p.name == "Ink Pen"))
                    .or_else(|| brushes.iter().find(|p| p.name == "Default Round"))
                    .or_else(|| brushes.first());
                if let Some(preset) = target {
                    brush_engine.active.set(preset.id);
                    brush_engine.size.set(preset.default_size);
                    brush_engine.opacity.set(preset.default_opacity);
                }
            }
            brush_engine.backfill_missing_previews();
        } else {
            tracing::warn!("no XDG config dir - keeping in-memory brushes only");
        }
    }
}

/// Everything specific to one open document tab.
///
/// Several state fields (`crop`, `transform`, `fill`, `shape`,
/// `layer_extensions`, `crop_apply`) are held to own the per-document state and
/// keep it alive; the active clones live inside the wired input/tool closures.
#[allow(dead_code)]
pub(crate) struct DocumentSession {
    pub(crate) global: GlobalState,
    pub(crate) history: Rc<RefCell<HistoryStack>>,
    pub(crate) crop: CropState,
    pub(crate) transform: TransformState,
    pub(crate) selection: SelectionState,
    pub(crate) fill: FillState,
    pub(crate) shape: ShapeState,
    pub(crate) gradient: GradientState,
    pub(crate) viewport: Viewport,
    pub(crate) doc_props: DocumentProperties,
    pub(crate) layer_extensions: Rc<RefCell<HashMap<String, LayerExtension>>>,
    /// Per-document component library (see [`crate::right_bar`] Components tab).
    pub(crate) components: Rc<RefCell<ComponentLibrary>>,
    /// Rebuild the Components-tab grid from the library (after edit/placement).
    pub(crate) refresh_components: Rc<dyn Fn()>,
    /// `Some` while a component is open in edit mode; stashes the main canvas.
    pub(crate) edit_mode: Rc<RefCell<Option<ComponentEditContext>>>,
    /// Leave component edit mode (bake the component, restore the main canvas).
    pub(crate) exit_component_edit: Rc<dyn Fn()>,

    /// On-canvas text editing controller (enter/exit edit mode, keyboard,
    /// caret/selection overlay, live re-render, history). Held here so tool
    /// switches and ESC can commit the in-flight edit.
    pub(crate) text_edit: crate::text_edit::TextEdit,

    // Per-document callbacks.
    pub(crate) apply_tool: Rc<dyn Fn(Tool)>,
    pub(crate) transform_apply: Rc<dyn Fn()>,
    pub(crate) transform_cancel: Rc<dyn Fn()>,
    pub(crate) crop_apply: Rc<dyn Fn()>,
    pub(crate) refresh_layers: Rc<dyn Fn()>,
    /// Rename the active layer/group, or the selected component when that tab is
    /// showing. Backs the `app.rename` action (F2).
    pub(crate) begin_rename: Rc<dyn Fn()>,
    pub(crate) selected_layer_ids: Rc<dyn Fn() -> Vec<String>>,
    pub(crate) set_right_panel_tool: Rc<dyn Fn(Tool)>,
    pub(crate) set_tool_options: Rc<dyn Fn(Tool)>,
    /// Re-register this document's layer gio actions (copy/cut/paste/...) so
    /// the app-global action names point at this document. Called on activation.
    pub(crate) reinstall_actions: Rc<dyn Fn()>,

    // Per-document chrome widgets (swapped into window slots on activation).
    pub(crate) right_bar: gtk::Widget,
    pub(crate) tool_options: gtk::Widget,
    pub(crate) picture: gtk::Picture,

    // Metadata.
    pub(crate) file_path: RefCell<Option<PathBuf>>,
    /// `history.undo_len()` captured at the last save/load; drives the dirty
    /// (`*`) marker.
    pub(crate) saved_marker: Rc<Cell<usize>>,
    pub(crate) title: Rc<RefCell<String>>,
    pub(crate) tab_page: Rc<RefCell<Option<adw::TabPage>>>,
    /// Liveness token: the dirty-title timer holds a weak ref and stops once the
    /// session is dropped (tab closed).
    _alive: Rc<()>,
}

impl DocumentSession {
    /// Build a fresh document session of the given canvas size. Creates its own
    /// Vulkan canvas, viewport, right bar, and tool-options bar.
    pub(crate) fn new(
        global: &GlobalState,
        set_active_tool_late: &SetActiveToolSlot,
        init_size: Size,
        history_capacity: usize,
        title: impl Into<String>,
    ) -> Rc<Self> {
        let document = Document::new(init_size);
        let history = Rc::new(RefCell::new(HistoryStack::new(HistoryConfig {
            capacity: history_capacity,
        })));
        // Created up front so the transform apply/cancel closures (built below)
        // can re-render component instances from the master.
        let components: Rc<RefCell<ComponentLibrary>> =
            Rc::new(RefCell::new(ComponentLibrary::new()));
        let crop = CropState::new();
        let transform = TransformState::new();
        let selection = SelectionState::new();
        let fill = FillState::new();
        let shape = ShapeState::new();
        let gradient = GradientState::new();
        let doc_props = document.properties.clone();
        let viewport = Viewport::new(init_size, document.layers.clone());
        let layer_extensions: Rc<RefCell<HashMap<String, LayerExtension>>> =
            Rc::new(RefCell::new(HashMap::new()));

        // -- Crop apply --------------------------------------------------
        let crop_apply: Rc<dyn Fn()> = {
            let viewport = viewport.clone();
            let crop_c = crop.clone();
            let redraw = viewport.redraw_handle();
            let set_tool = Rc::clone(set_active_tool_late);
            let history_for_crop = Rc::clone(&history);
            let canvas_for_crop = viewport.canvas();
            Rc::new(move || {
                let Some(rect) = crop_c.rect.get() else {
                    return;
                };

                let (before_size, before_layers, active_layer) = {
                    let mut c = canvas_for_crop.borrow_mut();
                    let sz = c.size();
                    let snap = c.layers().snapshot();
                    let active = c.layers().active();
                    let layers: Vec<CropLayer> = snap
                        .iter()
                        .enumerate()
                        .filter_map(|(i, l)| {
                            c.read_layer(i).ok().map(|px| CropLayer {
                                id: l.id.clone(),
                                name: l.name.clone(),
                                visible: l.visible,
                                pixels: px,
                                kind: l.kind.clone(),
                                blend: l.blend,
                                opacity: l.opacity,
                            })
                        })
                        .collect();
                    ((sz.width, sz.height), layers, active)
                };

                let new_size = viewport.apply_crop(rect);
                if new_size.is_some() {
                    let (after_size, after_layers) = {
                        let mut c = canvas_for_crop.borrow_mut();
                        let sz = c.size();
                        let snap = c.layers().snapshot();
                        let layers: Vec<CropLayer> = snap
                            .iter()
                            .enumerate()
                            .filter_map(|(i, l)| {
                                c.read_layer(i).ok().map(|px| CropLayer {
                                    id: l.id.clone(),
                                    name: l.name.clone(),
                                    visible: l.visible,
                                    pixels: px,
                                    kind: l.kind.clone(),
                                    blend: l.blend,
                                    opacity: l.opacity,
                                })
                            })
                            .collect();
                        ((sz.width, sz.height), layers)
                    };
                    history_for_crop.borrow_mut().record(HistoryAction::CropCanvas {
                        before_size,
                        after_size,
                        before_layers,
                        after_layers,
                        active_layer,
                    });

                    viewport.zoom_fit();
                    redraw.request();
                }
                crop_c.rect.set(None);
                crop_c.notify_rect_changed();
                if let Some(setter) = set_tool.borrow().as_ref() {
                    setter(Tool::Cursor);
                }
            })
        };

        // -- Transform cancel --------------------------------------------
        let transform_cancel: Rc<dyn Fn()> = {
            let canvas_c = viewport.canvas();
            let transform_c = transform.clone();
            let paintable_c = viewport.paintable().clone();
            let redraw = viewport.redraw_handle();
            let set_tool = Rc::clone(set_active_tool_late);
            let extensions_c = Rc::clone(&layer_extensions);
            let components_c = Rc::clone(&components);
            let text_engine_c = Rc::clone(&global.text_engine);
            Rc::new(move || {
                // End any live GPU blend preview before restoring/recompositing.
                canvas_c.borrow_mut().clear_transform_preview();
                paintable_c.set_transform_gpu_preview(false);
                // Text layer transform: re-render the text at its (unchanged)
                // box and restore - the box geometry was never modified.
                if transform_c.text.borrow().is_some() {
                    if let Some(idx) = transform_c.original_layer_idx.get() {
                        let kind = canvas_c.borrow().layers().kind(idx);
                        if let Some(LayerKind::Text(content)) = kind {
                            let cs = canvas_c.borrow().size();
                            let pixels = {
                                let mut engine = text_engine_c.borrow_mut();
                                oxiedraw_core::text::render::render_text(
                                    &content, &mut engine, cs.width, cs.height,
                                )
                            };
                            if let Err(e) = canvas_c.borrow_mut().restore_layer(idx, &pixels) {
                                tracing::error!(error = %e, "text transform cancel: restore failed");
                            }
                        }
                    }
                    transform_c.clear();
                    transform_c.notify_changed();
                    paintable_c.set_transform_rect(None);
                    paintable_c.set_transform_source(None, 0, 0, None);
                    redraw.request();
                    if let Some(setter) = set_tool.borrow().as_ref() {
                        setter(Tool::Cursor);
                    }
                    return;
                }
                // Component instance transform: re-render at the original
                // placement and clear, no pixel restore (source was the master).
                // Bind the clone first so the borrow is released before clear().
                let component_marker = transform_c.component.borrow().clone();
                if let Some((component_id, orig_rect)) = component_marker {
                    if let Some(idx) = transform_c.original_layer_idx.get() {
                        let size = canvas_c.borrow().size();
                        let filter = transform_c.filter.get();
                        let pixels = components_c
                            .borrow()
                            .get(&component_id)
                            .map(|c| {
                                c.render_instance(
                                    size.width,
                                    size.height,
                                    Placement::from_rect(orig_rect),
                                    filter,
                                )
                            });
                        if let Some(px) = pixels
                            && let Err(e) = canvas_c.borrow_mut().restore_layer(idx, &px)
                        {
                            tracing::error!(error = %e, "component transform cancel: restore failed");
                        }
                    }
                    transform_c.clear();
                    transform_c.notify_changed();
                    paintable_c.set_transform_rect(None);
                    paintable_c.set_transform_source(None, 0, 0, None);
                    redraw.request();
                    if let Some(setter) = set_tool.borrow().as_ref() {
                        setter(Tool::Cursor);
                    }
                    return;
                }
                if let Some(idx) = transform_c.original_layer_idx.get() {
                    if transform_c.is_paste.get() {
                        if let Err(e) = canvas_c.borrow_mut().remove_layer(idx) {
                            tracing::error!(error = %e, "transform cancel: remove_layer failed");
                        }
                    } else if let Some((off_x, off_y)) = transform_c.original_src_offset.get() {
                        let pixels = transform_c.original_pixels.borrow().clone();
                        if let Some(ref pix) = pixels {
                            let (ew, eh) =
                                transform_c.original_src_dims.get().unwrap_or_else(|| {
                                    let s = canvas_c.borrow().size();
                                    (s.width, s.height)
                                });
                            let cs = canvas_c.borrow().size();
                            let canvas_pix =
                                crop_from_extension(pix, off_x, off_y, ew, eh, cs.width, cs.height);
                            let mut canvas = canvas_c.borrow_mut();
                            let layer_id =
                                canvas.layers().snapshot().get(idx).map(|l| l.id.clone());
                            if let Err(e) = canvas.restore_layer(idx, &canvas_pix) {
                                tracing::error!(error = %e, "transform cancel: restore_layer failed");
                            }
                            if let Some(id) = layer_id {
                                extensions_c.borrow_mut().insert(
                                    id,
                                    LayerExtension {
                                        offset_x: off_x,
                                        offset_y: off_y,
                                        width: ew,
                                        height: eh,
                                        pixels: pix.clone(),
                                    },
                                );
                            }
                        }
                    } else {
                        let pixels = transform_c.original_pixels.borrow().clone();
                        if let Some(pixels) = pixels
                            && let Err(e) = canvas_c.borrow_mut().restore_layer(idx, &pixels)
                        {
                            tracing::error!(error = %e, "transform cancel: restore_layer failed");
                        }
                    }
                }
                transform_c.clear();
                transform_c.notify_changed();
                paintable_c.set_transform_rect(None);
                paintable_c.set_transform_source(None, 0, 0, None);
                redraw.request();
                if let Some(setter) = set_tool.borrow().as_ref() {
                    setter(Tool::Cursor);
                }
            })
        };

        // -- Transform apply ---------------------------------------------
        let transform_apply: Rc<dyn Fn()> = {
            let canvas_c = viewport.canvas();
            let transform_c = transform.clone();
            let paintable_c = viewport.paintable().clone();
            let redraw = viewport.redraw_handle();
            let set_tool = Rc::clone(set_active_tool_late);
            let extensions_c = Rc::clone(&layer_extensions);
            let cancel = Rc::clone(&transform_cancel);
            let toaster = global.toaster.clone();
            let history_for_xform = Rc::clone(&history);
            let components_c = Rc::clone(&components);
            let text_engine_c = Rc::clone(&global.text_engine);
            Rc::new(move || {
                // End any live GPU blend preview; the commit recomposites below.
                canvas_c.borrow_mut().clear_transform_preview();
                paintable_c.set_transform_gpu_preview(false);
                // Text layer transform: the natural layout (box w/h, font) is
                // unchanged - the transform only updates the box centre/angle and
                // the anamorphic display scale, so a scale squishes the glyphs and
                // *persists* (the text stays editable at its natural size).
                let text_marker = transform_c.text.borrow().clone();
                if let Some((layer_id, _orig_rect)) = text_marker {
                    let (Some(new_rect), Some(idx)) =
                        (transform_c.rect.get(), transform_c.original_layer_idx.get())
                    else {
                        return;
                    };
                    let kind = canvas_c.borrow().layers().kind(idx);
                    if let Some(LayerKind::Text(content)) = kind {
                        let cs = canvas_c.borrow().size();
                        let natural = content.box_rect;
                        // Pre-transform state (natural box + current scale).
                        let before_content = content.clone();
                        // After: same natural layout, new centre/angle, new scale
                        // = visible (transform rect) / natural.
                        let mut after_content = content;
                        after_content.box_rect = oxiedraw_core::text::TextBox::new(
                            new_rect.cx, new_rect.cy, natural.w, natural.h, new_rect.angle,
                        );
                        after_content.scale = (
                            if natural.w.abs() > 1e-3 { new_rect.w / natural.w } else { after_content.scale.0 },
                            if natural.h.abs() > 1e-3 { new_rect.h / natural.h } else { after_content.scale.1 },
                        );

                        let (before, after) = {
                            let mut engine = text_engine_c.borrow_mut();
                            let before = oxiedraw_core::text::render::render_text(
                                &before_content, &mut engine, cs.width, cs.height,
                            );
                            let after = oxiedraw_core::text::render::render_text(
                                &after_content, &mut engine, cs.width, cs.height,
                            );
                            (before, after)
                        };
                        if let Err(e) = canvas_c.borrow_mut().restore_layer(idx, &after) {
                            tracing::error!(error = %e, "text transform apply: write failed");
                        }
                        canvas_c
                            .borrow()
                            .layers()
                            .set_kind(idx, LayerKind::Text(after_content.clone()));
                        if let Some(patch) =
                            LayerPatch::from_full_diff(&before, &after, cs.width, cs.height)
                        {
                            history_for_xform.borrow_mut().record(HistoryAction::TextEdit {
                                layer_id,
                                patch,
                                before_content: Box::new(before_content),
                                after_content: Box::new(after_content),
                            });
                        }
                    }
                    transform_c.clear();
                    transform_c.notify_changed();
                    paintable_c.set_transform_rect(None);
                    paintable_c.set_transform_source(None, 0, 0, None);
                    redraw.request();
                    if let Some(setter) = set_tool.borrow().as_ref() {
                        setter(Tool::Cursor);
                    }
                    return;
                }
                // Component instance transform: re-render the master at the new
                // placement (crisp) and update the layer's placement metadata.
                // Bind the clone first so the borrow is released before clear().
                let component_marker = transform_c.component.borrow().clone();
                if let Some((component_id, orig_rect)) = component_marker {
                    let Some(current_rect) = transform_c.rect.get() else {
                        return;
                    };
                    let Some(idx) = transform_c.original_layer_idx.get() else {
                        return;
                    };
                    let size = canvas_c.borrow().size();
                    let filter = transform_c.filter.get();
                    let new_placement = Placement::from_rect(current_rect);
                    let (before, after, layer_id) = {
                        let lib = components_c.borrow();
                        let Some(comp) = lib.get(&component_id) else {
                            return;
                        };
                        let before = comp.render_instance(
                            size.width,
                            size.height,
                            Placement::from_rect(orig_rect),
                            filter,
                        );
                        let after =
                            comp.render_instance(size.width, size.height, new_placement, filter);
                        let layer_id = canvas_c
                            .borrow()
                            .layers()
                            .snapshot()
                            .get(idx)
                            .map(|l| l.id.clone());
                        (before, after, layer_id)
                    };
                    if let Err(e) = canvas_c.borrow_mut().restore_layer(idx, &after) {
                        tracing::error!(error = %e, "component transform apply: write failed");
                    }
                    canvas_c.borrow().layers().set_kind(
                        idx,
                        LayerKind::Component(ComponentInstance {
                            component_id: component_id.clone(),
                            placement: new_placement,
                        }),
                    );
                    if let Some(layer_id) = layer_id
                        && let Some(patch) =
                            LayerPatch::from_full_diff(&before, &after, size.width, size.height)
                    {
                        history_for_xform.borrow_mut().record(
                            HistoryAction::ComponentRetransform {
                                layer_id,
                                component_id,
                                patch,
                                before_placement: Placement::from_rect(orig_rect),
                                after_placement: new_placement,
                            },
                        );
                    }
                    transform_c.clear();
                    transform_c.notify_changed();
                    paintable_c.set_transform_rect(None);
                    paintable_c.set_transform_source(None, 0, 0, None);
                    redraw.request();
                    if let Some(setter) = set_tool.borrow().as_ref() {
                        setter(Tool::Cursor);
                    }
                    return;
                }
                let Some(current_rect) = transform_c.rect.get() else {
                    return;
                };
                let Some(original_rect) = transform_c.original_rect.get() else {
                    return;
                };
                let Some(idx) = transform_c.original_layer_idx.get() else {
                    return;
                };
                let pixels = transform_c.original_pixels.borrow().clone();
                let Some(pixels) = pixels else { return };
                let is_paste = transform_c.is_paste.get();

                let canvas_size = canvas_c.borrow().size();
                let (src_w, src_h) = transform_c
                    .original_src_dims
                    .get()
                    .unwrap_or((canvas_size.width, canvas_size.height));

                let gpu_result = canvas_c.borrow_mut().apply_layer_transform_gpu(
                    idx,
                    &pixels,
                    src_w,
                    src_h,
                    original_rect,
                    current_rect,
                );
                let (full_result, ext_x, ext_y, full_w, full_h) = match gpu_result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "transform apply (GPU) failed");
                        cancel();
                        let msg = match &e {
                            RendererError::TransformTooLarge { limit, .. } => format!(
                                "Error: Can't transform the layer. Max layer texture size: {limit}"
                            ),
                            _ => format!("Error: transform failed: {e}"),
                        };
                        toaster.error(&msg);
                        return;
                    }
                };
                let layer_snapshot = canvas_c
                    .borrow()
                    .layers()
                    .snapshot()
                    .get(idx)
                    .map(|l| (l.id.clone(), l.name.clone(), l.visible));
                if let Some((id, name, visible)) = layer_snapshot {
                    let is_outside = ext_x < 0
                        || ext_y < 0
                        || ext_x.saturating_add(full_w as i32) > canvas_size.width as i32
                        || ext_y.saturating_add(full_h as i32) > canvas_size.height as i32;
                    if is_outside {
                        extensions_c.borrow_mut().insert(
                            id.clone(),
                            LayerExtension {
                                offset_x: ext_x,
                                offset_y: ext_y,
                                width: full_w,
                                height: full_h,
                                pixels: full_result,
                            },
                        );
                    } else {
                        extensions_c.borrow_mut().remove(&id);
                    }

                    let after_px = canvas_c.borrow_mut().read_layer(idx).unwrap_or_default();
                    if is_paste {
                        history_for_xform.borrow_mut().record(HistoryAction::LayerAdd {
                            idx,
                            id,
                            name,
                            visible,
                            layer_kind: LayerKind::Raster,
                            blend: oxiedraw_core::document::BlendMode::Normal,
                            opacity: 1.0,
                            pixels: after_px,
                        });
                    } else {
                        let before_canvas = if transform_c.original_src_offset.get().is_some() {
                            let (off_x, off_y) =
                                transform_c.original_src_offset.get().unwrap_or((0, 0));
                            let (ew, eh) = transform_c
                                .original_src_dims
                                .get()
                                .unwrap_or((canvas_size.width, canvas_size.height));
                            crop_from_extension(
                                &pixels,
                                off_x,
                                off_y,
                                ew,
                                eh,
                                canvas_size.width,
                                canvas_size.height,
                            )
                        } else {
                            pixels.clone()
                        };
                        if let Some(patch) = LayerPatch::from_full_diff(
                            &before_canvas,
                            &after_px,
                            canvas_size.width,
                            canvas_size.height,
                        ) {
                            history_for_xform
                                .borrow_mut()
                                .record(HistoryAction::Transform { layer_id: id, patch });
                        }
                    }
                }

                transform_c.clear();
                transform_c.notify_changed();
                paintable_c.set_transform_rect(None);
                paintable_c.set_transform_source(None, 0, 0, None);
                redraw.request();
                if let Some(setter) = set_tool.borrow().as_ref() {
                    setter(Tool::Cursor);
                }
            })
        };

        // -- Tool options bar --------------------------------------------
        // The text controller doesn't exist yet (it needs `refresh_layers`),
        // so the B/I/U buttons dispatch through this late-bound slot.
        let text_edit_slot: Rc<RefCell<Option<crate::text_edit::TextEdit>>> =
            Rc::new(RefCell::new(None));
        let (tool_options_widget, set_tool_options) = crate::tool_options_bar::build(
            &global.tools,
            &global.brush_engine,
            &crop,
            Rc::clone(&crop_apply),
            &transform,
            Rc::clone(&transform_apply),
            Rc::clone(&transform_cancel),
            &fill,
            &shape,
            &gradient,
            &text_edit_slot,
            global.default_brush_name.clone(),
            global.toaster.clone(),
        );

        // -- select_layer_content (thumbnail swatch click) ---------------
        let select_layer_content: Rc<dyn Fn(usize)> = {
            let canvas = viewport.canvas();
            let selection_state = selection.clone();
            let canvas_size = viewport.canvas_size_handle();
            let redraw = viewport.redraw_handle();
            Rc::new(move |layer_idx: usize| {
                {
                    let mut c = canvas.borrow_mut();
                    if let Err(e) = c.select_from_layer_alpha(layer_idx) {
                        tracing::error!(error = %e, "select_from_layer_alpha failed");
                        return;
                    }
                    selection_state.active.set(c.selection_active());
                }
                selection_state.source_layer.set(Some(layer_idx));
                canvas::primary_drag::refresh_selection_contours(
                    &canvas,
                    &selection_state,
                    &canvas_size,
                );
                selection_state.notify_changed();
                redraw.request();
            })
        };

        // Created early so the component edit closures can swap it out (the
        // dirty `*` marker is driven from it; the dirty timer is set up later).
        let saved_marker = Rc::new(Cell::new(0usize));

        // -- Component edit-mode plumbing --------------------------------
        let edit_mode: Rc<RefCell<Option<ComponentEditContext>>> = Rc::new(RefCell::new(None));
        // Late-bound enter/exit closures: the panel callbacks dispatch through
        // these slots, which are filled once the panel-derived handles exist.
        let enter_slot: Rc<RefCell<Option<Rc<dyn Fn(String)>>>> = Rc::new(RefCell::new(None));
        let component_exit: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

        let on_edit_component: Rc<dyn Fn(String)> = {
            let enter_slot = Rc::clone(&enter_slot);
            Rc::new(move |id: String| {
                let cb = enter_slot.borrow().clone();
                if let Some(cb) = cb {
                    cb(id);
                }
            })
        };

        // Cancel an in-progress transform before a layer is deleted, so the
        // stale layer index the transform holds can't later write onto a
        // shifted layer. Routed into the layer-delete action so every delete
        // path (Delete key, layers context menu) is covered.
        let prepare_delete: Rc<dyn Fn() -> bool> = {
            let transform = transform.clone();
            let transform_cancel = Rc::clone(&transform_cancel);
            Rc::new(move || prepare_transform_for_delete(&transform, || transform_cancel()))
        };

        // Commit an in-progress transform before the layers panel reorders the
        // stack: the transform holds a fixed layer index and has cleared the
        // target's pixels into the live overlay, so a reorder underneath it would
        // strand that content (the layer vanishes on apply) and corrupt the live
        // preview. Applying first bakes the result at the current index, then the
        // reorder proceeds on a stable stack.
        let prepare_reorder: Rc<dyn Fn()> = {
            let transform = transform.clone();
            let transform_apply = Rc::clone(&transform_apply);
            Rc::new(move || {
                let in_progress =
                    transform.original_layer_idx.get().is_some() || transform.rect.get().is_some();
                if in_progress {
                    transform_apply();
                }
            })
        };

        // Callback for layers panel: if the Cursor tool is active when the
        // user clicks a layer row, automatically switch to Transform so they
        // can immediately drag the selected layer.
        let cursor_activates_transform: Rc<dyn Fn()> = {
            let slot = Rc::clone(set_active_tool_late);
            let tools = global.tools.clone();
            Rc::new(move || {
                if tools.active.get() == Tool::Cursor {
                    if let Some(setter) = slot.borrow().as_ref() {
                        setter(Tool::Transform);
                    }
                }
            })
        };

        // -- Right bar ---------------------------------------------------
        let (
            right_bar_widget,
            set_right_panel_tool,
            refresh_layers,
            selected_layer_ids,
            reinstall_actions,
            refresh_components,
            set_component_edit,
            begin_rename,
            refresh_text_panel,
        ) = crate::right_bar::build(
            global.colors.clone(),
            &document.layers,
            &viewport.canvas(),
            &viewport.redraw_handle(),
            &crop,
            &global.tools,
            &gradient,
            &global.clipboard,
            &global.toaster,
            &select_layer_content,
            &history,
            &components,
            &on_edit_component,
            &component_exit,
            &text_edit_slot,
            &global.text_engine,
            &global.font_previews,
            &prepare_delete,
            &prepare_reorder,
        );

        // -- Wire component enter/exit now the panel handles exist --------
        {
            let enter: Rc<dyn Fn(String)> = {
                let viewport = viewport.clone();
                let history = Rc::clone(&history);
                let components = Rc::clone(&components);
                let edit_mode = Rc::clone(&edit_mode);
                let refresh_layers = Rc::clone(&refresh_layers);
                let set_component_edit = Rc::clone(&set_component_edit);
                let saved_marker = Rc::clone(&saved_marker);
                // A themed widget used to read the libadwaita accent color.
                let accent_widget = right_bar_widget.clone();
                Rc::new(move |id: String| {
                    do_enter_component_edit(
                        id,
                        &viewport,
                        &history,
                        &components,
                        &edit_mode,
                        &refresh_layers,
                        &set_component_edit,
                        &saved_marker,
                        &accent_widget,
                        history_capacity,
                    );
                })
            };
            *enter_slot.borrow_mut() = Some(enter);

            let exit: Rc<dyn Fn()> = {
                let viewport = viewport.clone();
                let history = Rc::clone(&history);
                let components = Rc::clone(&components);
                let edit_mode = Rc::clone(&edit_mode);
                let refresh_layers = Rc::clone(&refresh_layers);
                let refresh_components = Rc::clone(&refresh_components);
                let set_component_edit = Rc::clone(&set_component_edit);
                let saved_marker = Rc::clone(&saved_marker);
                Rc::new(move || {
                    do_exit_component_edit(
                        &viewport,
                        &history,
                        &components,
                        &edit_mode,
                        &refresh_layers,
                        &refresh_components,
                        &set_component_edit,
                        &saved_marker,
                    );
                })
            };
            *component_exit.borrow_mut() = Some(Rc::clone(&exit));
        }
        let exit_component_edit: Rc<dyn Fn()> = {
            let component_exit = Rc::clone(&component_exit);
            Rc::new(move || {
                let cb = component_exit.borrow().clone();
                if let Some(cb) = cb {
                    cb();
                }
            })
        };

        // -- apply_tool (per-document half of set_active_tool) -----------
        let apply_tool: Rc<dyn Fn(Tool)> = build_apply_tool(
            &viewport,
            &crop,
            &transform,
            &selection,
            &layer_extensions,
            &components,
            &global.text_engine,
            Rc::clone(&set_tool_options),
            Rc::clone(&set_right_panel_tool),
        );

        // -- Wire canvas input + present ---------------------------------
        let picture = gtk::Picture::builder()
            .hexpand(true)
            .vexpand(true)
            .content_fit(gtk::ContentFit::Fill)
            .build();

        // On-canvas text editing controller (enter/exit, keys, render, history).
        let text_edit = crate::text_edit::TextEdit::new(
            viewport.canvas(),
            Rc::clone(&global.text_engine),
            Rc::clone(&history),
            viewport.paintable().clone(),
            viewport.redraw_handle(),
            Rc::clone(&refresh_layers),
            global.colors.clone(),
            viewport.canvas_size_handle(),
            viewport.zoom_handle(),
        );
        // Recolour text live when the colour wheel changes during editing, and
        // wire the late-bound B/I/U buttons + right-bar panel to this controller.
        text_edit.connect_color();
        *text_edit_slot.borrow_mut() = Some(text_edit.clone());
        {
            let refresh = Rc::clone(&refresh_text_panel);
            text_edit.connect_changed(Box::new(move || refresh()));
        }

        canvas::wire(
            &picture,
            &viewport,
            &global.brush_engine,
            &global.colors,
            &global.tools,
            &crop,
            &transform,
            &selection,
            &fill,
            &shape,
            &gradient,
            &history,
            &global.toaster,
            &text_edit,
            Rc::clone(&cursor_activates_transform),
        );

        // Drag-and-drop a component card onto the canvas to place an instance.
        {
            let drop = gtk::DropTarget::new(glib::types::Type::STRING, gtk::gdk::DragAction::COPY);
            let viewport_c = viewport.clone();
            let components_c = Rc::clone(&components);
            let history_c = Rc::clone(&history);
            let edit_mode_c = Rc::clone(&edit_mode);
            let refresh_layers_c = Rc::clone(&refresh_layers);
            let toaster_c = global.toaster.clone();
            drop.connect_drop(move |_, value, x, y| {
                let Ok(id) = value.get::<String>() else {
                    return false;
                };
                do_place_component(
                    id,
                    x,
                    y,
                    &viewport_c,
                    &components_c,
                    &history_c,
                    &edit_mode_c,
                    &refresh_layers_c,
                    &toaster_c,
                );
                true
            });
            picture.add_controller(drop);
        }

        // Central per-document listeners (selection ants, crop overlay).
        {
            let selection_c = selection.clone();
            let paintable_c = viewport.paintable().clone();
            selection.connect_changed(Box::new(move || {
                let contours = selection_c.ants_contours.borrow().clone();
                paintable_c.set_selection_contours(contours);
            }));
        }
        {
            let selection_c = selection.clone();
            let paintable_c = viewport.paintable().clone();
            let offset = Rc::new(Cell::new(0.0_f64));
            glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
                if !selection_c.ants_contours.borrow().is_empty() {
                    let next = (offset.get() + 1.0) % 10.0;
                    offset.set(next);
                    paintable_c.set_selection_ants_offset(next);
                }
                glib::ControlFlow::Continue
            });
        }
        {
            let crop_c = crop.clone();
            let paintable_c = viewport.paintable().clone();
            crop.connect_rect_changed(Box::new(move || {
                paintable_c.set_crop(crop_c.rect.get(), crop_c.overlay.get());
            }));
        }

        // Apply pixel-view settings to this document's paintable.
        {
            let pv = crate::settings::AppSettings::load().pixel_view;
            viewport.paintable().set_pixel_view(
                pv.enabled,
                pv.nearest_threshold,
                pv.grid_enabled,
                pv.grid_threshold,
            );
        }

        let title = Rc::new(RefCell::new(title.into()));
        let tab_page: Rc<RefCell<Option<adw::TabPage>>> = Rc::new(RefCell::new(None));
        let alive = Rc::new(());

        // Drive the unsaved (`*`) marker on the tab title. Polling keeps this
        // independent of the many code paths that mutate history; the timer
        // stops when the session is dropped (tab closed) via the weak token.
        {
            let weak = Rc::downgrade(&alive);
            let history = Rc::clone(&history);
            let saved = Rc::clone(&saved_marker);
            let title = Rc::clone(&title);
            let tab_page = Rc::clone(&tab_page);
            let last_dirty = Cell::new(false);
            glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                if weak.upgrade().is_none() {
                    return glib::ControlFlow::Break;
                }
                let dirty = history.borrow().undo_len() != saved.get();
                if dirty != last_dirty.get() {
                    last_dirty.set(dirty);
                    if let Some(page) = tab_page.borrow().as_ref() {
                        let base = title.borrow().clone();
                        page.set_title(&if dirty { format!("*{base}") } else { base });
                    }
                }
                glib::ControlFlow::Continue
            });
        }

        Rc::new(Self {
            global: global.clone(),
            history,
            crop,
            transform,
            selection,
            fill,
            shape,
            gradient,
            viewport,
            doc_props,
            layer_extensions,
            components,
            refresh_components,
            edit_mode,
            exit_component_edit,
            text_edit,
            apply_tool,
            transform_apply,
            transform_cancel,
            crop_apply,
            refresh_layers,
            begin_rename,
            selected_layer_ids,
            set_right_panel_tool,
            set_tool_options,
            reinstall_actions,
            right_bar: right_bar_widget,
            tool_options: tool_options_widget.upcast::<gtk::Widget>(),
            picture,
            file_path: RefCell::new(None),
            saved_marker,
            title,
            tab_page,
            _alive: alive,
        })
    }

    // -- Dirty / title helpers -------------------------------------------

    pub(crate) fn is_dirty(&self) -> bool {
        self.history.borrow().undo_len() != self.saved_marker.get()
    }

    pub(crate) fn mark_saved(&self) {
        self.saved_marker.set(self.history.borrow().undo_len());
    }

    /// Title with a leading `*` when the document has unsaved changes.
    pub(crate) fn display_title(&self) -> String {
        let base = self.title.borrow().clone();
        if self.is_dirty() {
            format!("*{base}")
        } else {
            base
        }
    }

    /// Push the current display title onto the associated tab page.
    pub(crate) fn refresh_tab_title(&self) {
        if let Some(page) = self.tab_page.borrow().as_ref() {
            page.set_title(&self.display_title());
        }
    }

    pub(crate) fn apply_pixel_view(&self, pv: &crate::settings::PixelViewSettings) {
        self.viewport.paintable().set_pixel_view(
            pv.enabled,
            pv.nearest_threshold,
            pv.grid_enabled,
            pv.grid_threshold,
        );
    }

    /// Current document properties with the live canvas size folded in (the
    /// stored size goes stale after a crop).
    pub(crate) fn current_properties(&self) -> DocumentProperties {
        let mut props = self.doc_props.clone();
        props.canvas = self.viewport.canvas().borrow().size();
        props
    }

    // -- App-level action handlers (dispatched to the active document) ---

    pub(crate) fn undo(&self) {
        // Land any in-flight shape-correction stroke as a real history entry
        // first, so this undo pops a consistent canvas state instead of
        // reverting an older action behind an unrecorded corrected shape.
        self.viewport.flush_pending_correction();
        let canvas = self.viewport.canvas();
        let label = {
            let mut h = self.history.borrow_mut();
            let mut c = canvas.borrow_mut();
            let mut comps = self.components.borrow_mut();
            match h.undo(&mut c, &mut comps) {
                Ok(label) => label,
                Err(e) => {
                    tracing::error!(error = %e, "undo failed");
                    None
                }
            }
        };
        if let Some(l) = label {
            self.viewport.resync_canvas_size();
            (self.refresh_layers)();
            (self.refresh_components)();
            refresh_selection_after_history(
                &canvas,
                &self.selection,
                &self.viewport.canvas_size_handle(),
            );
            self.viewport.redraw_handle().request();
            self.global.toaster.info(&format!("Undo: {l}"));
            self.refresh_tab_title();
        }
    }

    pub(crate) fn redo(&self) {
        self.viewport.flush_pending_correction();
        let canvas = self.viewport.canvas();
        let label = {
            let mut h = self.history.borrow_mut();
            let mut c = canvas.borrow_mut();
            let mut comps = self.components.borrow_mut();
            match h.redo(&mut c, &mut comps) {
                Ok(label) => label,
                Err(e) => {
                    tracing::error!(error = %e, "redo failed");
                    None
                }
            }
        };
        if let Some(l) = label {
            self.viewport.resync_canvas_size();
            (self.refresh_layers)();
            (self.refresh_components)();
            refresh_selection_after_history(
                &canvas,
                &self.selection,
                &self.viewport.canvas_size_handle(),
            );
            self.viewport.redraw_handle().request();
            self.global.toaster.info(&format!("Redo: {l}"));
            self.refresh_tab_title();
        }
    }

    pub(crate) fn select_all(&self) {
        let canvas = self.viewport.canvas();
        let before = snapshot_selection(&canvas);
        {
            let mut c = canvas.borrow_mut();
            if let Err(e) = c.select_all() {
                tracing::error!(error = %e, "select_all failed");
                return;
            }
            self.selection.active.set(c.selection_active());
        }
        let after = snapshot_selection(&canvas);
        self.history
            .borrow_mut()
            .record(HistoryAction::SelectionChange { before, after });
        self.selection.source_layer.set(None);
        canvas::primary_drag::refresh_selection_contours(
            &canvas,
            &self.selection,
            &self.viewport.canvas_size_handle(),
        );
        self.selection.notify_changed();
        self.viewport.redraw_handle().request();
    }

    pub(crate) fn deselect(&self) {
        let canvas = self.viewport.canvas();
        let before = snapshot_selection(&canvas);
        {
            let mut c = canvas.borrow_mut();
            c.deselect();
            self.selection.active.set(false);
        }
        self.history.borrow_mut().record(HistoryAction::SelectionChange {
            before,
            after: SelectionSnapshot { active: false, mask: None },
        });
        self.selection.ants_contours.borrow_mut().clear();
        self.selection.source_layer.set(None);
        self.selection.notify_changed();
        self.viewport.redraw_handle().request();
    }

    pub(crate) fn select_inverse(&self) {
        let canvas = self.viewport.canvas();
        let before = snapshot_selection(&canvas);
        {
            let mut c = canvas.borrow_mut();
            if let Err(e) = c.invert_selection() {
                tracing::error!(error = %e, "invert_selection failed");
                return;
            }
            self.selection.active.set(c.selection_active());
        }
        let after = snapshot_selection(&canvas);
        self.history
            .borrow_mut()
            .record(HistoryAction::SelectionChange { before, after });
        self.selection.source_layer.set(None);
        canvas::primary_drag::refresh_selection_contours(
            &canvas,
            &self.selection,
            &self.viewport.canvas_size_handle(),
        );
        self.selection.notify_changed();
        self.viewport.redraw_handle().request();
    }

    /// Whether a component is currently open in edit mode.
    pub(crate) fn is_editing_component(&self) -> bool {
        self.edit_mode.borrow().is_some()
    }

    /// If a component is open, leave edit mode and return `true`. Used by the
    /// window-level ESC handler so it can fall through to other ESC behaviour
    /// when no component is open.
    pub(crate) fn escape_component_edit(&self) -> bool {
        if self.is_editing_component() {
            (self.exit_component_edit)();
            true
        } else {
            false
        }
    }

    /// Delete pressed with an active selection: erase the selected pixels from
    /// the active layer (and clear the selection) instead of removing the whole
    /// layer. Returns `true` if a selection was present and handled.
    pub(crate) fn delete_selection(&self) -> bool {
        let canvas = self.viewport.canvas();
        if !canvas.borrow().selection_active() {
            return false;
        }
        let Some(idx) = canvas.borrow().layers().active() else {
            return false;
        };
        let (layer_id, before) = {
            let mut c = canvas.borrow_mut();
            let id = c.layers().snapshot().get(idx).map(|l| l.id.clone()).unwrap_or_default();
            let px = c.read_layer(idx).unwrap_or_default();
            (id, px)
        };
        // Erase the masked pixels but keep the selection so the marquee stays.
        if let Err(e) = canvas.borrow_mut().erase_selection_in_layer(idx) {
            tracing::error!(error = %e, "delete_selection failed");
            return true;
        }
        let cs = canvas.borrow().size();
        let after = canvas.borrow_mut().read_layer(idx).unwrap_or_default();
        if let Some(patch) = LayerPatch::from_full_diff(&before, &after, cs.width, cs.height) {
            self.history
                .borrow_mut()
                .record(HistoryAction::Clear { layer_id, patch });
        }
        (self.refresh_layers)();
        self.viewport.redraw_handle().request();
        true
    }

    /// ESC pressed while the crop tool is active: discard the pending crop rect.
    /// The caller is expected to switch back to the cursor tool afterwards.
    pub(crate) fn cancel_crop(&self) {
        self.crop.rect.set(None);
        self.crop.notify_rect_changed();
        self.viewport.redraw_handle().request();
    }

    /// ESC pressed while a selection tool is active: clear the selection.
    pub(crate) fn escape_deselect(&self) {
        let canvas = self.viewport.canvas();
        {
            let mut c = canvas.borrow_mut();
            c.deselect();
            self.selection.active.set(false);
        }
        self.selection.ants_contours.borrow_mut().clear();
        self.selection.source_layer.set(None);
        self.selection.notify_changed();
        self.viewport.redraw_handle().request();
    }
}

/// Fallback accent (libadwaita default blue) when the themed color can't be
/// resolved. The live border color is looked up from the widget theme.
const COMPONENT_ACCENT: (f32, f32, f32) = (0.21, 0.52, 0.89);

/// Resolve the libadwaita accent color (`accent_bg_color`) from a themed
/// widget, as straight RGB in `0.0..=1.0`. Falls back to [`COMPONENT_ACCENT`].
#[allow(deprecated)]
fn accent_rgb(widget: &gtk::Widget) -> (f32, f32, f32) {
    widget
        .style_context()
        .lookup_color("accent_bg_color")
        .or_else(|| widget.style_context().lookup_color("accent_color"))
        .map_or(COMPONENT_ACCENT, |c| (c.red(), c.green(), c.blue()))
}

/// Stashed main-canvas state held while a component is open in edit mode, so it
/// can be restored byte-for-byte (including layer kinds + undo history) on exit.
pub(crate) struct ComponentEditContext {
    component_id: String,
    main_size: Size,
    main_layers: Vec<StashedLayer>,
    main_active: Option<usize>,
    main_history: HistoryStack,
    /// Main doc's saved-marker, swapped out so the dirty `*` reflects the
    /// component's own edits while open and the main doc's state on exit.
    main_saved_marker: usize,
}

struct StashedLayer {
    id: String,
    name: String,
    visible: bool,
    kind: LayerKind,
    blend: oxiedraw_core::document::BlendMode,
    opacity: f32,
    pixels: Vec<u8>,
}

/// Read every layer's pixels + metadata from the canvas (used to stash the main
/// canvas on enter and to read the component back on exit).
fn capture_canvas(viewport: &Viewport) -> (Size, Vec<StashedLayer>, Option<usize>) {
    let canvas = viewport.canvas();
    let mut c = canvas.borrow_mut();
    let size = c.size();
    let snap = c.layers().snapshot();
    let active = c.layers().active();
    let mut layers = Vec::with_capacity(snap.len());
    for (i, l) in snap.iter().enumerate() {
        let pixels = c.read_layer(i).unwrap_or_default();
        layers.push(StashedLayer {
            id: l.id.clone(),
            name: l.name.clone(),
            visible: l.visible,
            kind: l.kind.clone(),
            blend: l.blend,
            opacity: l.opacity,
            pixels,
        });
    }
    (size, layers, active)
}

/// Enter component edit mode: stash the main canvas (+ history), swap the canvas
/// to the component's layers, and show the edit-mode chrome. No-op if a
/// component is already open (no nesting).
#[allow(clippy::too_many_arguments)]
fn do_enter_component_edit(
    component_id: String,
    viewport: &Viewport,
    history: &Rc<RefCell<HistoryStack>>,
    components: &Rc<RefCell<oxiedraw_core::components::ComponentLibrary>>,
    edit_mode: &Rc<RefCell<Option<ComponentEditContext>>>,
    refresh_layers: &Rc<dyn Fn()>,
    set_component_edit: &Rc<dyn Fn(Option<String>)>,
    saved_marker: &Rc<Cell<usize>>,
    accent_widget: &gtk::Widget,
    history_capacity: usize,
) {
    if edit_mode.borrow().is_some() {
        return;
    }
    let Some((name, size, tuples, active)) = ({
        let lib = components.borrow();
        lib.get(&component_id)
            .map(|c| (c.name.clone(), c.size, c.layer_tuples(), c.active_layer))
    }) else {
        return;
    };

    let (main_size, main_layers, main_active) = capture_canvas(viewport);
    let fresh = HistoryStack::new(HistoryConfig {
        capacity: history_capacity,
    });
    let main_history = std::mem::replace(&mut *history.borrow_mut(), fresh);
    let main_saved_marker = saved_marker.replace(0);
    *edit_mode.borrow_mut() = Some(ComponentEditContext {
        component_id,
        main_size,
        main_layers,
        main_active,
        main_history,
        main_saved_marker,
    });

    viewport.load_layers_resized(size, &tuples, active);
    viewport
        .paintable()
        .set_edit_mode(&format!("Component - {name}"), true, accent_rgb(accent_widget));
    refresh_layers();
    viewport.redraw_handle().request();
    set_component_edit(Some(name));
}

/// Leave component edit mode: bake the component's layers back into the library
/// (rebuilding its master), restore the main canvas + history, and clear the
/// edit-mode chrome. No-op if no component is open.
fn do_exit_component_edit(
    viewport: &Viewport,
    history: &Rc<RefCell<HistoryStack>>,
    components: &Rc<RefCell<oxiedraw_core::components::ComponentLibrary>>,
    edit_mode: &Rc<RefCell<Option<ComponentEditContext>>>,
    refresh_layers: &Rc<dyn Fn()>,
    refresh_components: &Rc<dyn Fn()>,
    set_component_edit: &Rc<dyn Fn(Option<String>)>,
    saved_marker: &Rc<Cell<usize>>,
) {
    let Some(ctx) = edit_mode.borrow_mut().take() else {
        return;
    };

    // Read the edited component layers back and store them (rebuilds master).
    let (_size, comp_layers, comp_active) = capture_canvas(viewport);
    {
        let mut lib = components.borrow_mut();
        if let Some(component) = lib.get_mut(&ctx.component_id) {
            let layers: Vec<ComponentLayer> = comp_layers
                .into_iter()
                .map(|l| ComponentLayer {
                    id: l.id,
                    name: l.name,
                    visible: l.visible,
                    blend: l.blend,
                    opacity: l.opacity,
                    pixels: l.pixels,
                })
                .collect();
            component.set_layers(layers, comp_active);
        }
    }

    // Restore the main canvas (with each layer's blend), then re-apply the
    // layer kinds that replace_all_layers reset to Raster.
    let main_tuples: Vec<(String, String, bool, oxiedraw_core::document::BlendMode, f32, Vec<u8>)> =
        ctx.main_layers
            .iter()
            .map(|l| {
                (
                    l.id.clone(),
                    l.name.clone(),
                    l.visible,
                    l.blend,
                    l.opacity,
                    l.pixels.clone(),
                )
            })
            .collect();
    viewport.load_layers_resized(ctx.main_size, &main_tuples, ctx.main_active);
    {
        let canvas = viewport.canvas();
        let c = canvas.borrow();
        for (i, l) in ctx.main_layers.iter().enumerate() {
            c.layers().set_kind(i, l.kind.clone());
        }
    }

    *history.borrow_mut() = ctx.main_history;
    saved_marker.set(ctx.main_saved_marker);

    // Live instances: re-render every placed instance of this component from
    // its (now updated) master.
    rerender_component_instances(viewport, components, &ctx.component_id);

    viewport
        .paintable()
        .set_edit_mode("Main canvas", false, COMPONENT_ACCENT);
    refresh_layers();
    refresh_components();
    viewport.redraw_handle().request();
    set_component_edit(None);
}

/// Re-render every `Component` layer instancing `component_id` from the
/// component's current master, in place (used after editing the component).
fn rerender_component_instances(
    viewport: &Viewport,
    components: &Rc<RefCell<ComponentLibrary>>,
    component_id: &str,
) {
    let canvas = viewport.canvas();
    let size = canvas.borrow().size();
    let targets: Vec<(usize, Placement)> = {
        let c = canvas.borrow();
        c.layers()
            .snapshot()
            .iter()
            .enumerate()
            .filter_map(|(i, l)| match &l.kind {
                LayerKind::Component(inst) if inst.component_id == component_id => {
                    Some((i, inst.placement))
                }
                _ => None,
            })
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    let lib = components.borrow();
    let Some(comp) = lib.get(component_id) else {
        return;
    };
    for (idx, placement) in targets {
        let pixels = comp.render_instance(size.width, size.height, placement, TransformFilter::Bilinear);
        if let Err(e) = canvas.borrow_mut().restore_layer(idx, &pixels) {
            tracing::error!(error = %e, idx, "re-render component instance failed");
        }
    }
}

/// Place a component instance on the main canvas at the drop point. Adds a
/// `Component`-kind layer whose slot is rendered from the component master at a
/// placement centred on `(drop_x, drop_y)`, and records it for undo.
#[allow(clippy::too_many_arguments)]
fn do_place_component(
    component_id: String,
    drop_x: f64,
    drop_y: f64,
    viewport: &Viewport,
    components: &Rc<RefCell<ComponentLibrary>>,
    history: &Rc<RefCell<HistoryStack>>,
    edit_mode: &Rc<RefCell<Option<ComponentEditContext>>>,
    refresh_layers: &Rc<dyn Fn()>,
    toaster: &crate::toaster::Toaster,
) {
    if edit_mode.borrow().is_some() {
        toaster.info("Finish editing the component before placing one.");
        return;
    }

    let canvas_pos = viewport.widget_to_canvas_point(drop_x, drop_y);
    let canvas_size = viewport.canvas().borrow().size();

    let Some((name, placement, pixels)) = ({
        let lib = components.borrow();
        lib.get(&component_id).map(|c| {
            #[allow(clippy::cast_precision_loss)]
            let placement = Placement::new(
                canvas_pos.x,
                canvas_pos.y,
                c.size.width as f32,
                c.size.height as f32,
                0.0,
            );
            let pixels = c.render_instance(
                canvas_size.width,
                canvas_size.height,
                placement,
                TransformFilter::Bilinear,
            );
            (c.name.clone(), placement, pixels)
        })
    }) else {
        return;
    };

    let canvas = viewport.canvas();
    let idx = match canvas.borrow_mut().add_layer_with_pixels(&name, &pixels) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!(error = %e, "place component: add_layer_with_pixels failed");
            return;
        }
    };
    canvas.borrow().layers().set_kind(
        idx,
        LayerKind::Component(ComponentInstance {
            component_id,
            placement,
        }),
    );

    if let Some((id, lname, visible, kind, blend, opacity, px)) =
        oxiedraw_core::history::capture_layer(&mut canvas.borrow_mut(), idx)
    {
        history.borrow_mut().record(HistoryAction::LayerAdd {
            idx,
            id,
            name: lname,
            visible,
            layer_kind: kind,
            blend,
            opacity,
            pixels: px,
        });
    }

    refresh_layers();
    viewport.redraw_handle().request();
}

/// Decide whether a layer-delete should proceed, cancelling any in-progress
/// transform first. `cancel` runs only when a transform is in progress. Returns
/// `false` when that cancel already removed the layer itself (paste-via-
/// transform), so the caller must not delete a second layer.
fn prepare_transform_for_delete(transform: &TransformState, cancel: impl FnOnce()) -> bool {
    let in_progress =
        transform.original_layer_idx.get().is_some() || transform.rect.get().is_some();
    if !in_progress {
        return true;
    }
    let was_paste = transform.is_paste.get();
    cancel();
    !was_paste
}

/// Read the current selection mask into a [`SelectionSnapshot`] for history.
fn snapshot_selection(canvas: &Rc<RefCell<Canvas>>) -> SelectionSnapshot {
    let mut c = canvas.borrow_mut();
    if c.selection_active() {
        c.read_selection_mask().map_or(
            SelectionSnapshot { active: true, mask: None },
            |m| SelectionSnapshot { active: true, mask: Some(m) },
        )
    } else {
        SelectionSnapshot { active: false, mask: None }
    }
}

/// Capture the layers above `target_idx` and hand them to the paintable so the
/// live transform preview draws inside its z-order instead of on top of every
/// layer. The transformed layer has already been cleared at this point.
fn capture_transform_above(
    canvas: &Rc<RefCell<Canvas>>,
    paintable: &crate::canvas_paintable::CanvasPaintable,
    target_idx: usize,
) {
    let mut c = canvas.borrow_mut();
    let cs = c.size();
    let mut above = Vec::new();
    match c.begin_transform_preview(target_idx, &mut above) {
        Ok(()) => {
            drop(c);
            paintable.set_transform_above(Some(&above), cs.width, cs.height);
        }
        Err(e) => tracing::error!(error = %e, "transform: begin_transform_preview failed"),
    }
}

/// Eagerly start the live GPU blend preview from the now-populated transform
/// state, so a non-Normal layer shows its real blend at rest - before any drag.
/// Reads the captured source + geometry; falls back to the GSK overlay if it
/// can't start (e.g. a dims/pixels mismatch). One submit at tool-enter; the
/// per-drag path is unchanged, so dragging stays smooth.
fn start_transform_gpu_preview(
    canvas: &Rc<RefCell<Canvas>>,
    paintable: &crate::canvas_paintable::CanvasPaintable,
    transform: &TransformState,
) {
    let (Some(idx), Some((w, h)), Some(orig), Some(rect)) = (
        transform.original_layer_idx.get(),
        transform.original_src_dims.get(),
        transform.original_rect.get(),
        transform.rect.get(),
    ) else {
        return;
    };
    let Some(pixels) = transform.original_pixels.borrow().clone() else {
        return;
    };
    if pixels.len() != (w as usize) * (h as usize) * 4 {
        return;
    }
    let mut c = canvas.borrow_mut();
    match c.begin_transform_preview_gpu(idx, &pixels, w, h) {
        Ok(()) => {
            c.set_transform_preview(orig, rect, w, h);
            drop(c);
            paintable.set_transform_gpu_preview(true);
        }
        Err(e) => tracing::error!(error = %e, "transform: begin_transform_preview_gpu failed"),
    }
}

/// Re-sync the UI selection state to the canvas after an undo/redo applied a
/// `SelectionChange`.
fn refresh_selection_after_history(
    canvas: &Rc<RefCell<Canvas>>,
    selection: &SelectionState,
    canvas_size: &Rc<Cell<Size>>,
) {
    selection.active.set(canvas.borrow().selection_active());
    selection.source_layer.set(None);
    canvas::primary_drag::refresh_selection_contours(canvas, selection, canvas_size);
    selection.notify_changed();
}

/// Build the per-document "apply this tool" closure. This is the body of the
/// old `set_active_tool` minus the global tool-state and left-bar updates (those
/// happen in the window-level wrapper, since the active tool is shared).
fn build_apply_tool(
    viewport: &Viewport,
    crop: &CropState,
    transform: &TransformState,
    selection: &SelectionState,
    layer_extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
    components: &Rc<RefCell<ComponentLibrary>>,
    text_engine: &Rc<RefCell<oxiedraw_core::text::fonts::TextEngine>>,
    set_tool_options: Rc<dyn Fn(Tool)>,
    set_right_panel_tool: Rc<dyn Fn(Tool)>,
) -> Rc<dyn Fn(Tool)> {
    let paintable = viewport.paintable().clone();
    let crop_for_tool = crop.clone();
    let transform_for_tool = transform.clone();
    let canvas_for_tool = viewport.canvas();
    let redraw_for_tool = viewport.redraw_handle();
    let extensions_for_sat = Rc::clone(layer_extensions);
    let selection_for_sat = selection.clone();
    let components_for_tool = Rc::clone(components);
    let text_engine_for_tool = Rc::clone(text_engine);
    Rc::new(move |t: Tool| {
        set_tool_options(t);
        set_right_panel_tool(t);

        paintable.set_crop_active(t == Tool::Crop);
        if t != Tool::ColorPicker {
            paintable.set_color_picker(None);
        }
        // The gradient ramp cursor is transient; drop it on any tool switch
        // (the next pointer motion re-arms it when the Gradient tool is active).
        paintable.set_gradient_cursor(None);
        if t == Tool::Crop && crop_for_tool.rect.get().is_none() {
            let cs = canvas_for_tool.borrow().size();
            #[allow(clippy::cast_precision_loss)]
            let default_rect = CropRect::new(0.0, 0.0, cs.width as f32, cs.height as f32);
            crop_for_tool.rect.set(Some(default_rect));
            crop_for_tool.notify_rect_changed();
        }

        paintable.set_transform_active(t == Tool::Transform);
        if t == Tool::Transform {
            if transform_for_tool.pre_seeded.get() {
                transform_for_tool.pre_seeded.set(false);
                let (src_w, src_h) = transform_for_tool.original_src_dims.get().unwrap_or_else(|| {
                    let cs = canvas_for_tool.borrow().size();
                    (cs.width, cs.height)
                });
                let orig_rect = transform_for_tool.original_rect.get();
                let pixels = transform_for_tool.original_pixels.borrow().clone();
                paintable.set_transform_source(pixels.as_deref(), src_w, src_h, orig_rect);
                paintable.set_transform_rect(transform_for_tool.rect.get());
                if let Some(idx) = transform_for_tool.original_layer_idx.get() {
                    capture_transform_above(&canvas_for_tool, &paintable, idx);
                }
                start_transform_gpu_preview(&canvas_for_tool, &paintable, &transform_for_tool);
                redraw_for_tool.request();
                transform_for_tool.notify_changed();
            } else {
                let mut canvas = canvas_for_tool.borrow_mut();
                let cs = canvas.size();
                if let Some(idx) = canvas.layers().active() {
                    // Component instance: transform from the master (crisp),
                    // starting at the instance's current placement.
                    if let Some(LayerKind::Component(inst)) = canvas.layers().kind(idx) {
                        let master = components_for_tool
                            .borrow()
                            .get(&inst.component_id)
                            .map(|c| (c.master.clone(), c.size.width, c.size.height));
                        if let Some((master, mw, mh)) = master {
                            let placement_rect = inst.placement.to_rect();
                            #[allow(clippy::cast_precision_loss)]
                            let orig_full =
                                TransformRect::new(mw as f32 / 2.0, mh as f32 / 2.0, mw as f32, mh as f32, 0.0);
                            if let Err(e) = canvas.clear_layer_at(idx, [0.0, 0.0, 0.0, 0.0]) {
                                tracing::error!(error = %e, "component transform: clear failed");
                            }
                            drop(canvas);
                            paintable.set_transform_source(Some(&master), mw, mh, Some(orig_full));
                            capture_transform_above(&canvas_for_tool, &paintable, idx);
                            *transform_for_tool.original_pixels.borrow_mut() = Some(master);
                            transform_for_tool.original_layer_idx.set(Some(idx));
                            transform_for_tool.original_rect.set(Some(orig_full));
                            transform_for_tool.original_src_dims.set(Some((mw, mh)));
                            transform_for_tool.rect.set(Some(placement_rect));
                            transform_for_tool.is_paste.set(false);
                            *transform_for_tool.component.borrow_mut() =
                                Some((inst.component_id, placement_rect));
                            transform_for_tool.notify_changed();
                            paintable.set_transform_rect(Some(placement_rect));
                            start_transform_gpu_preview(
                                &canvas_for_tool,
                                &paintable,
                                &transform_for_tool,
                            );
                            redraw_for_tool.request();
                        }
                        return;
                    }
                    // Text layer: transform from the text rendered in its local
                    // frame, starting at the box's current geometry. Apply
                    // updates the box (rotation/scale/position), not pixels.
                    if let Some(LayerKind::Text(content)) = canvas.layers().kind(idx) {
                        let layer_id = canvas.layers().snapshot().get(idx).map(|l| l.id.clone());
                        let (local, lw, lh) = {
                            let mut engine = text_engine_for_tool.borrow_mut();
                            oxiedraw_core::text::render::render_text_local(&content, &mut engine)
                        };
                        #[allow(clippy::cast_precision_loss)]
                        let orig_full = TransformRect::new(
                            lw as f32 / 2.0,
                            lh as f32 / 2.0,
                            lw as f32,
                            lh as f32,
                            0.0,
                        );
                        // Start the transform box at the *visible* (already
                        // scaled) rect; the source texture is the natural layout,
                        // so the preview shows the current squish at rest.
                        let current_rect = content.visible_rect();
                        if let Err(e) = canvas.clear_layer_at(idx, [0.0, 0.0, 0.0, 0.0]) {
                            tracing::error!(error = %e, "text transform: clear failed");
                        }
                        drop(canvas);
                        paintable.set_transform_source(Some(&local), lw, lh, Some(orig_full));
                        capture_transform_above(&canvas_for_tool, &paintable, idx);
                        *transform_for_tool.original_pixels.borrow_mut() = Some(local);
                        transform_for_tool.original_layer_idx.set(Some(idx));
                        transform_for_tool.original_rect.set(Some(orig_full));
                        transform_for_tool.original_src_dims.set(Some((lw, lh)));
                        transform_for_tool.rect.set(Some(current_rect));
                        transform_for_tool.is_paste.set(false);
                        if let Some(id) = layer_id {
                            *transform_for_tool.text.borrow_mut() = Some((id, current_rect));
                        }
                        transform_for_tool.notify_changed();
                        paintable.set_transform_rect(Some(current_rect));
                        start_transform_gpu_preview(
                            &canvas_for_tool,
                            &paintable,
                            &transform_for_tool,
                        );
                        redraw_for_tool.request();
                        return;
                    }
                    let layer_id = canvas.layers().snapshot().get(idx).map(|l| l.id.clone());
                    let extension = layer_id
                        .as_ref()
                        .and_then(|id| extensions_for_sat.borrow_mut().remove(id));

                    if let Some(ext) = extension {
                        let gpu_pixels = canvas.read_layer(idx).unwrap_or_else(|e| {
                            tracing::warn!(error = %e, "transform: read_layer for merge failed; using extension only");
                            vec![0u8; (cs.width * cs.height * 4) as usize]
                        });
                        let (merged, mx, my, mw, mh) = merge_extension_with_gpu(
                            &ext.pixels,
                            ext.offset_x,
                            ext.offset_y,
                            ext.width,
                            ext.height,
                            &gpu_pixels,
                            cs.width,
                            cs.height,
                        );
                        #[allow(clippy::cast_precision_loss)]
                        let tight = non_empty_bounds(&merged, mw, mh).unwrap_or_else(|| {
                            TransformRect::new(
                                mw as f32 / 2.0,
                                mh as f32 / 2.0,
                                mw as f32,
                                mh as f32,
                                0.0,
                            )
                        });
                        let orig_rect = tight;
                        #[allow(clippy::cast_precision_loss)]
                        let current_rect = TransformRect::new(
                            mx as f32 + tight.cx,
                            my as f32 + tight.cy,
                            tight.w,
                            tight.h,
                            0.0,
                        );
                        if let Err(e) = canvas.clear_layer_at(idx, [0.0, 0.0, 0.0, 0.0]) {
                            tracing::error!(error = %e, "transform: clear_layer_at (ext) failed");
                        }
                        drop(canvas);
                        capture_transform_above(&canvas_for_tool, &paintable, idx);
                        redraw_for_tool.request();
                        paintable.set_transform_source(Some(&merged), mw, mh, Some(orig_rect));
                        *transform_for_tool.original_pixels.borrow_mut() = Some(merged);
                        transform_for_tool.original_layer_idx.set(Some(idx));
                        transform_for_tool.original_rect.set(Some(orig_rect));
                        transform_for_tool.original_src_dims.set(Some((mw, mh)));
                        transform_for_tool.original_src_offset.set(Some((mx, my)));
                        transform_for_tool.rect.set(Some(current_rect));
                        transform_for_tool.is_paste.set(false);
                        transform_for_tool.notify_changed();
                        paintable.set_transform_rect(Some(current_rect));
                        start_transform_gpu_preview(
                            &canvas_for_tool,
                            &paintable,
                            &transform_for_tool,
                        );
                        redraw_for_tool.request();
                    } else {
                        let lift_idx = selection_for_sat
                            .source_layer
                            .get()
                            .filter(|&i| i < canvas.layers().len())
                            .unwrap_or(idx);
                        let selection_lift = if canvas.selection_active() {
                            match canvas.extract_selection_pixels(lift_idx) {
                                Ok(opt) => opt,
                                Err(e) => {
                                    tracing::error!(error = %e, "transform: extract_selection_pixels failed");
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        let lifted = if let Some((px, w, h)) = selection_lift {
                            Some((px, w, h, true, lift_idx))
                        } else {
                            match canvas.read_layer(idx) {
                                Ok(px) => Some((px, cs.width, cs.height, false, idx)),
                                Err(e) => {
                                    tracing::error!(error = %e, "transform: read_layer failed");
                                    None
                                }
                            }
                        };
                        if let Some((pixels, src_w, src_h, from_selection, target_idx)) = lifted {
                            #[allow(clippy::cast_precision_loss)]
                            let orig_rect = non_empty_bounds(&pixels, src_w, src_h).unwrap_or_else(
                                || {
                                    TransformRect::new(
                                        src_w as f32 / 2.0,
                                        src_h as f32 / 2.0,
                                        src_w as f32,
                                        src_h as f32,
                                        0.0,
                                    )
                                },
                            );
                            transform_for_tool.original_rect.set(Some(orig_rect));
                            if !from_selection
                                && let Err(e) =
                                    canvas.clear_layer_at(target_idx, [0.0, 0.0, 0.0, 0.0])
                            {
                                tracing::error!(error = %e, "transform: clear_layer_at failed");
                            }
                            drop(canvas);
                            if from_selection {
                                selection_for_sat.active.set(false);
                                selection_for_sat.ants_contours.borrow_mut().clear();
                                selection_for_sat.source_layer.set(None);
                                selection_for_sat.notify_changed();
                            }
                            capture_transform_above(&canvas_for_tool, &paintable, target_idx);
                            redraw_for_tool.request();
                            paintable.set_transform_source(
                                Some(&pixels),
                                src_w,
                                src_h,
                                Some(orig_rect),
                            );
                            *transform_for_tool.original_pixels.borrow_mut() = Some(pixels);
                            transform_for_tool.original_layer_idx.set(Some(target_idx));
                            transform_for_tool.original_src_dims.set(Some((src_w, src_h)));
                            transform_for_tool.is_paste.set(false);
                            transform_for_tool.rect.set(Some(orig_rect));
                            transform_for_tool.notify_changed();
                            paintable.set_transform_rect(Some(orig_rect));
                            start_transform_gpu_preview(
                                &canvas_for_tool,
                                &paintable,
                                &transform_for_tool,
                            );
                            redraw_for_tool.request();
                        }
                    }
                }
            }
        } else {
            // Switching away from Transform without apply/cancel - silently cancel.
            // End any live GPU blend preview first.
            canvas_for_tool.borrow_mut().clear_transform_preview();
            paintable.set_transform_gpu_preview(false);
            // Text layer: re-render at its (unchanged) box and restore.
            if transform_for_tool.text.borrow().is_some() {
                if let Some(idx) = transform_for_tool.original_layer_idx.get() {
                    let kind = canvas_for_tool.borrow().layers().kind(idx);
                    if let Some(LayerKind::Text(content)) = kind {
                        let cs = canvas_for_tool.borrow().size();
                        let pixels = {
                            let mut engine = text_engine_for_tool.borrow_mut();
                            oxiedraw_core::text::render::render_text(
                                &content, &mut engine, cs.width, cs.height,
                            )
                        };
                        if let Err(e) = canvas_for_tool.borrow_mut().restore_layer(idx, &pixels) {
                            tracing::error!(error = %e, "text transform silent cancel: restore failed");
                        }
                    }
                }
                transform_for_tool.clear();
                transform_for_tool.notify_changed();
                paintable.set_transform_rect(None);
                paintable.set_transform_source(None, 0, 0, None);
                redraw_for_tool.request();
                return;
            }
            // Component instance: re-render at the original placement.
            // Bind the clone first so the borrow is released before clear().
            let component_marker = transform_for_tool.component.borrow().clone();
            if let Some((component_id, orig_rect)) = component_marker {
                if let Some(idx) = transform_for_tool.original_layer_idx.get() {
                    let size = canvas_for_tool.borrow().size();
                    let filter = transform_for_tool.filter.get();
                    let pixels = components_for_tool.borrow().get(&component_id).map(|c| {
                        c.render_instance(size.width, size.height, Placement::from_rect(orig_rect), filter)
                    });
                    if let Some(px) = pixels
                        && let Err(e) = canvas_for_tool.borrow_mut().restore_layer(idx, &px)
                    {
                        tracing::error!(error = %e, "component transform silent cancel: restore failed");
                    }
                }
                transform_for_tool.clear();
                transform_for_tool.notify_changed();
                paintable.set_transform_rect(None);
                paintable.set_transform_source(None, 0, 0, None);
                redraw_for_tool.request();
                return;
            }
            if transform_for_tool.rect.get().is_some() {
                if let Some(idx) = transform_for_tool.original_layer_idx.get() {
                    if transform_for_tool.is_paste.get() {
                        if let Err(e) = canvas_for_tool.borrow_mut().remove_layer(idx) {
                            tracing::error!(error = %e, "transform silent cancel: remove_layer failed");
                        }
                    } else if let Some((off_x, off_y)) = transform_for_tool.original_src_offset.get()
                    {
                        let pixels = transform_for_tool.original_pixels.borrow().clone();
                        if let Some(ref pix) = pixels {
                            let (ew, eh) =
                                transform_for_tool.original_src_dims.get().unwrap_or_else(|| {
                                    let s = canvas_for_tool.borrow().size();
                                    (s.width, s.height)
                                });
                            let cs = canvas_for_tool.borrow().size();
                            let canvas_pix =
                                crop_from_extension(pix, off_x, off_y, ew, eh, cs.width, cs.height);
                            let mut canvas = canvas_for_tool.borrow_mut();
                            let layer_id =
                                canvas.layers().snapshot().get(idx).map(|l| l.id.clone());
                            if let Err(e) = canvas.restore_layer(idx, &canvas_pix) {
                                tracing::error!(error = %e, "transform silent cancel: restore_layer failed");
                            }
                            if let Some(id) = layer_id {
                                extensions_for_sat.borrow_mut().insert(
                                    id,
                                    LayerExtension {
                                        offset_x: off_x,
                                        offset_y: off_y,
                                        width: ew,
                                        height: eh,
                                        pixels: pix.clone(),
                                    },
                                );
                            }
                        }
                    } else {
                        let pixels = transform_for_tool.original_pixels.borrow().clone();
                        if let Some(pixels) = pixels
                            && let Err(e) = canvas_for_tool.borrow_mut().restore_layer(idx, &pixels)
                        {
                            tracing::error!(error = %e, "transform silent cancel: restore_layer failed");
                        }
                    }
                }
                transform_for_tool.clear();
                transform_for_tool.notify_changed();
                paintable.set_transform_rect(None);
                paintable.set_transform_source(None, 0, 0, None);
                redraw_for_tool.request();
            }
        }
    })
}

// -- Off-canvas extension helpers (moved from app.rs) --------------------

/// Merge a `LayerExtension` with the canvas-sized GPU layer pixels into a single
/// buffer covering the union of both regions.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn merge_extension_with_gpu(
    ext_pixels: &[u8],
    ext_x: i32,
    ext_y: i32,
    ext_w: u32,
    ext_h: u32,
    gpu_pixels: &[u8],
    canvas_w: u32,
    canvas_h: u32,
) -> (Vec<u8>, i32, i32, u32, u32) {
    let mx = ext_x.min(0);
    let my = ext_y.min(0);
    let mx_end = ext_x.saturating_add(ext_w as i32).max(canvas_w as i32);
    let my_end = ext_y.saturating_add(ext_h as i32).max(canvas_h as i32);
    let mw = (mx_end - mx) as u32;
    let mh = (my_end - my) as u32;
    let mut merged = vec![0u8; (mw * mh * 4) as usize];

    let ext_dx = (ext_x - mx) as u32;
    let ext_dy = (ext_y - my) as u32;
    for row in 0..ext_h {
        let si = (row * ext_w) as usize * 4;
        let di = ((ext_dy + row) * mw + ext_dx) as usize * 4;
        let len = ext_w as usize * 4;
        if si + len <= ext_pixels.len() && di + len <= merged.len() {
            merged[di..di + len].copy_from_slice(&ext_pixels[si..si + len]);
        }
    }

    let gpu_dx = (-mx) as u32;
    let gpu_dy = (-my) as u32;
    for row in 0..canvas_h {
        let si = (row * canvas_w) as usize * 4;
        let di = ((gpu_dy + row) * mw + gpu_dx) as usize * 4;
        let len = canvas_w as usize * 4;
        if si + len <= gpu_pixels.len() && di + len <= merged.len() {
            merged[di..di + len].copy_from_slice(&gpu_pixels[si..si + len]);
        }
    }

    (merged, mx, my, mw, mh)
}

/// Extract the canvas-visible portion of a `LayerExtension` pixel buffer into a
/// canvas-sized BGRA8 buffer.
fn crop_from_extension(
    ext: &[u8],
    ext_x: i32,
    ext_y: i32,
    ext_w: u32,
    ext_h: u32,
    canvas_w: u32,
    canvas_h: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (canvas_w * canvas_h * 4) as usize];
    let end_x = ext_x.saturating_add(ext_w as i32);
    let end_y = ext_y.saturating_add(ext_h as i32);
    if end_x <= 0 || end_y <= 0 {
        return out;
    }
    let sx0 = ext_x.max(0) as u32;
    let sy0 = ext_y.max(0) as u32;
    let sx1 = (end_x as u32).min(canvas_w);
    let sy1 = (end_y as u32).min(canvas_h);
    if sx0 >= sx1 || sy0 >= sy1 {
        return out;
    }
    let ex0 = (sx0 as i32 - ext_x) as u32;
    let ey0 = (sy0 as i32 - ext_y) as u32;
    let copy_w = sx1 - sx0;
    let copy_h = sy1 - sy0;
    for row in 0..copy_h {
        let si = ((ey0 + row) * ext_w + ex0) as usize * 4;
        let di = ((sy0 + row) * canvas_w + sx0) as usize * 4;
        let len = copy_w as usize * 4;
        if si + len <= ext.len() && di + len <= out.len() {
            out[di..di + len].copy_from_slice(&ext[si..si + len]);
        }
    }
    out
}

/// Scan BGRA8 pixels for the tight bounding box of non-transparent pixels.
fn non_empty_bounds(pixels: &[u8], w: u32, h: u32) -> Option<TransformRect> {
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;

    for y in 0..h {
        for x in 0..w {
            let a = pixels[((y * w + x) * 4 + 3) as usize];
            if a > 0 {
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
                found = true;
            }
        }
    }

    if !found {
        return None;
    }
    let bx = min_x as f32;
    let by = min_y as f32;
    let bw = (max_x - min_x + 1) as f32;
    let bh = (max_y - min_y + 1) as f32;
    Some(TransformRect::new(bx + bw / 2.0, by + bh / 2.0, bw, bh, 0.0))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    // Bug 2: deleting a layer that isn't being transformed must not touch the
    // transform, and `cancel` must not run.
    #[test]
    fn delete_without_transform_proceeds_without_cancel() {
        let transform = TransformState::new();
        let cancelled = Cell::new(false);

        let proceed = prepare_transform_for_delete(&transform, || cancelled.set(true));

        assert!(proceed, "delete should proceed");
        assert!(!cancelled.get(), "no transform: cancel must not run");
    }

    // Bug 2: deleting the layer being transformed must cancel the transform
    // first (so its stale layer index can't write onto a shifted layer), then
    // still delete the restored layer.
    #[test]
    fn delete_during_transform_cancels_then_deletes() {
        let transform = TransformState::new();
        transform.original_layer_idx.set(Some(2));
        transform.rect.set(Some(TransformRect::new(8.0, 8.0, 16.0, 16.0, 0.0)));
        let cancelled = Cell::new(false);

        let proceed = prepare_transform_for_delete(&transform, || cancelled.set(true));

        assert!(cancelled.get(), "transform in progress: cancel must run");
        assert!(proceed, "the restored layer still gets deleted");
    }

    // Bug 2: a paste-via-transform cancel removes its own freshly-added layer,
    // so the caller must not delete a second layer.
    #[test]
    fn delete_during_paste_transform_skips_extra_delete() {
        let transform = TransformState::new();
        transform.original_layer_idx.set(Some(0));
        transform.is_paste.set(true);
        let cancelled = Cell::new(false);

        let proceed = prepare_transform_for_delete(&transform, || cancelled.set(true));

        assert!(cancelled.get(), "paste transform: cancel must run");
        assert!(!proceed, "paste cancel already removed the layer; don't delete again");
    }
}
