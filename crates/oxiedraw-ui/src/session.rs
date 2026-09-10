//! Per-document session state ([`DocumentSession`]) plus the state shared by
//! every open tab ([`GlobalState`]).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::brush_engine::BrushEngine;
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::ColorState;
use oxiedraw_core::components::{ComponentLayer, ComponentLibrary};
use oxiedraw_core::liquify::LiquifyState;
use oxiedraw_core::document::{
    ComponentInstance, Document, DocumentProperties, LayerKind, Placement,
};
use oxiedraw_core::history::{
    CropLayer, HistoryAction, HistoryConfig, HistoryStack, LayerPatch, PatchBounds,
    SelectionSnapshot,
};
use oxiedraw_core::guides::{GuideConfig, GuideState, Symmetry};
use oxiedraw_core::renderer::RendererError;
use oxiedraw_core::tools::{
    CropRect, CropState, FillState, GradientState, SelectionState, ShapeState, TargetKind, Tool,
    ToolState, TransformFilter, TransformRect, TransformState, TransformTarget,
};
use oxiedraw_utils::frame_profile;
use oxiedraw_utils::geometry::Size;
use relm4::gtk;
use relm4::gtk::glib;

use crate::canvas::{self, Viewport};

pub(crate) type SetActiveToolSlot = Rc<RefCell<Option<Rc<dyn Fn(Tool)>>>>;

fn call_slot(slot: &Rc<RefCell<Option<Rc<dyn Fn()>>>>) {
    let callback = slot.borrow().clone();
    if let Some(callback) = callback {
        callback();
    }
}

pub(crate) use oxiedraw_core::history::LayerExtension;

#[derive(Clone)]
pub(crate) struct GlobalState {
    pub(crate) brush_engine: BrushEngine,
    pub(crate) colors: ColorState,
    pub(crate) tools: ToolState,
    pub(crate) clipboard: Rc<RefCell<Option<crate::clipboard::LayerClipboard>>>,
    pub(crate) toaster: crate::toaster::Toaster,
    pub(crate) text_engine: Rc<RefCell<oxiedraw_core::text::fonts::TextEngine>>,
    pub(crate) font_previews: crate::font_previews::FontPreviews,
    pub(crate) save_in_progress: Rc<Cell<bool>>,
    pub(crate) default_brush_name: Rc<RefCell<Option<String>>>,
    pub(crate) autosave: AutosaveConfig,
}

#[derive(Clone)]
pub(crate) struct AutosaveConfig {
    pub(crate) enabled: Rc<Cell<bool>>,
    pub(crate) interval_secs: Rc<Cell<u32>>,
}

impl GlobalState {
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
            autosave: AutosaveConfig {
                enabled: Rc::new(Cell::new(settings.save.autosave_enabled)),
                interval_secs: Rc::new(Cell::new(settings.save.autosave_interval_secs)),
            },
        }
    }

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
                    brush_engine.set_active(preset.id);
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
    pub(crate) pattern: oxiedraw_core::patterns::PatternState,
    pub(crate) liquify: LiquifyState,
    pub(crate) guide: GuideState,
    pub(crate) viewport: Viewport,
    pub(crate) doc_props: DocumentProperties,
    pub(crate) layer_extensions: Rc<RefCell<HashMap<String, LayerExtension>>>,
    pub(crate) components: Rc<RefCell<ComponentLibrary>>,
    pub(crate) refresh_components: Rc<dyn Fn()>,
    pub(crate) edit_mode: Rc<RefCell<Option<ComponentEditContext>>>,
    pub(crate) exit_component_edit: Rc<dyn Fn()>,

    pub(crate) text_edit: crate::text_edit::TextEdit,

    pub(crate) pattern_edit: crate::pattern_edit::PatternEdit,

    pub(crate) apply_tool: Rc<dyn Fn(Tool)>,
    pub(crate) set_active_tool: SetActiveToolSlot,
    pub(crate) transform_apply: Rc<dyn Fn()>,
    pub(crate) transform_cancel: Rc<dyn Fn()>,
    pub(crate) pattern_apply: Rc<dyn Fn()>,
    pub(crate) pattern_cancel: Rc<dyn Fn()>,
    pub(crate) liquify_cancel: Rc<dyn Fn()>,
    pub(crate) liquify_restore: Rc<dyn Fn()>,
    pub(crate) liquify_flush: Rc<dyn Fn()>,
    pub(crate) crop_apply: Rc<dyn Fn()>,
    pub(crate) refresh_layers: Rc<dyn Fn()>,
    pub(crate) create_adjustment_layer: Rc<dyn Fn() -> Option<usize>>,
    pub(crate) begin_rename: Rc<dyn Fn()>,
    pub(crate) selected_layer_ids: Rc<dyn Fn() -> Vec<String>>,
    pub(crate) set_tool_window: Rc<dyn Fn(Tool)>,
    pub(crate) set_tool_options: Rc<dyn Fn(Tool)>,
    pub(crate) reinstall_actions: Rc<dyn Fn()>,

    pub(crate) color_picker: gtk::Widget,
    pub(crate) layers: gtk::Widget,
    pub(crate) tool_options: gtk::Widget,
    pub(crate) canvas_info: gtk::Widget,
    pub(crate) tool_windows: Rc<crate::panels::tool_windows::ToolWindows>,
    pub(crate) picture: gtk::Picture,
    pub(crate) canvas_root: gtk::Widget,

    pub(crate) file_path: RefCell<Option<PathBuf>>,
    pub(crate) saved_marker: Rc<Cell<usize>>,
    pub(crate) title: Rc<RefCell<String>>,
    pub(crate) tab_page: Rc<RefCell<Option<adw::TabPage>>>,
    pub(crate) recovery_file: RefCell<Option<PathBuf>>,
    pub(crate) last_autosave_len: Cell<Option<usize>>,
    _alive: Rc<()>,
}

fn snapshot_crop_layers(canvas: &Rc<RefCell<Canvas>>) -> ((u32, u32), Vec<CropLayer>) {
    let mut c = canvas.borrow_mut();
    let size = c.size();
    let snap = c.layers().snapshot();
    let layers = snap
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
                clipped: l.clipped,
                alpha_locked: l.alpha_locked,
            })
        })
        .collect();
    ((size.width, size.height), layers)
}

fn build_crop_apply(ctx: &SessionCtx) -> Rc<dyn Fn()> {
    let ctx = ctx.clone();
    Rc::new(move || {
        let Some(rect) = ctx.crop.rect.get() else {
            return;
        };
        let canvas = ctx.canvas();
        let active_layer = canvas.borrow().layers().active();
        let (before_size, before_layers) = snapshot_crop_layers(&canvas);

        if ctx.viewport.apply_crop(rect).is_some() {
            let (after_size, after_layers) = snapshot_crop_layers(&canvas);
            tracing::info!(
                target: "oxiedraw::canvas",
                from = ?before_size,
                to = ?after_size,
                layers = after_layers.len(),
                "canvas resize applied"
            );
            ctx.history.borrow_mut().record(HistoryAction::CropCanvas {
                before_size,
                after_size,
                before_layers,
                after_layers,
                active_layer,
            });
            ctx.viewport.zoom_fit();
            ctx.viewport.redraw_handle().request();
        }
        ctx.crop.rect.set(None);
        ctx.crop.notify_rect_changed();
        ctx.set_tool(Tool::Cursor);
    })
}

fn notify_selection_changed(ctx: &SessionCtx) {
    canvas::primary_drag::refresh_selection_contours(
        &ctx.canvas(),
        &ctx.selection,
        &ctx.viewport.canvas_size_handle(),
    );
    ctx.selection.notify_changed();
    ctx.viewport.redraw_handle().request();
}

fn build_select_layer_content(ctx: &SessionCtx) -> Rc<dyn Fn(usize)> {
    let ctx = ctx.clone();
    Rc::new(move |layer_idx: usize| {
        {
            let canvas = ctx.canvas();
            let mut c = canvas.borrow_mut();
            if let Err(e) = c.select_from_layer_alpha(layer_idx) {
                tracing::error!(error = %e, "select_from_layer_alpha failed");
                return;
            }
            ctx.selection.active.set(c.selection_active());
        }
        ctx.selection.source_layer.set(Some(layer_idx));
        notify_selection_changed(&ctx);
    })
}

fn build_select_folder_content(ctx: &SessionCtx) -> Rc<dyn Fn(Vec<usize>)> {
    let ctx = ctx.clone();
    Rc::new(move |layer_indices: Vec<usize>| {
        {
            let canvas = ctx.canvas();
            let mut c = canvas.borrow_mut();
            if let Err(e) = c.select_from_layers_alpha(&layer_indices) {
                tracing::error!(error = %e, "select_from_layers_alpha failed");
                return;
            }
            ctx.selection.active.set(c.selection_active());
        }
        ctx.selection.source_layer.set(None);
        notify_selection_changed(&ctx);
    })
}

struct LiquifyPending {
    id: String,
    pristine: Vec<u8>,
}

fn liquify_target(ctx: &SessionCtx) -> Option<(usize, String)> {
    let idx = ctx.canvas().borrow().liquify_target()?;
    let id = ctx.liquify_pending.borrow().as_ref()?.id.clone();
    Some((idx, id))
}

fn build_liquify_ensure(ctx: &SessionCtx, flush: &Rc<dyn Fn()>) -> Rc<dyn Fn() -> bool> {
    let ctx = ctx.clone();
    let flush = Rc::clone(flush);
    Rc::new(move || {
        let canvas = ctx.canvas();
        let Some(idx) = canvas.borrow().layers().active() else {
            return false;
        };
        // Match by layer id, not index: duplicate/merge/group/paste renumber the
        // stack, and an index match would bake one layer's warp onto another.
        let same_layer = {
            let c = canvas.borrow();
            c.liquify_target() == Some(idx)
                && ctx.liquify_pending.borrow().as_ref().is_some_and(|p| {
                    c.layers().snapshot().get(idx).is_some_and(|l| l.id == p.id)
                })
        };
        if same_layer {
            return true;
        }
        let kind = canvas.borrow().layers().kind(idx);
        if !matches!(kind, Some(LayerKind::Raster)) {
            ctx.global
                .toaster
                .info("Liquify only works on raster layers. Rasterize this one first.");
            return false;
        }
        flush();

        let (id, pristine) = {
            let mut c = canvas.borrow_mut();
            let Some(id) = c.layers().snapshot().get(idx).map(|l| l.id.clone()) else {
                return false;
            };
            match c.read_layer(idx) {
                Ok(pristine) => (id, pristine),
                Err(e) => {
                    tracing::error!(error = %e, "liquify: read_layer failed");
                    return false;
                }
            }
        };
        if let Err(e) = canvas.borrow_mut().begin_liquify(idx) {
            tracing::error!(error = %e, "liquify: begin_liquify failed");
            return false;
        }
        tracing::info!(target: "oxiedraw::tool", layer = %id, idx, "liquify session opened");
        *ctx.liquify_pending.borrow_mut() = Some(LiquifyPending { id, pristine });
        ctx.viewport.redraw_handle().request();
        true
    })
}

fn build_liquify_bake_stroke(ctx: &SessionCtx) -> Rc<dyn Fn() -> bool> {
    let ctx = ctx.clone();
    Rc::new(move || {
        let canvas = ctx.canvas();
        let Some((idx, id)) = liquify_target(&ctx) else {
            return true;
        };
        let Some((x, y, w, h)) = canvas.borrow().liquify_dirty_bounds() else {
            return true; // nothing painted since the last bake
        };

        let mut before = Vec::new();
        let mut after = Vec::new();
        if let Err(e) = canvas
            .borrow_mut()
            .read_layer_region_into(idx, x, y, w, h, &mut before)
        {
            tracing::error!(error = %e, "liquify: before-region read failed");
            return false;
        }
        let size = {
            let mut c = canvas.borrow_mut();
            if let Err(e) = c.liquify_bake() {
                tracing::error!(error = %e, "liquify: bake failed");
                return false;
            }
            if let Err(e) = c.read_layer_region_into(idx, x, y, w, h, &mut after) {
                tracing::error!(error = %e, "liquify: after-region read failed");
                return false;
            }
            c.size()
        };
        if before.len() != after.len() {
            tracing::error!("liquify: region size mismatch - stroke left unrecorded");
            return false;
        }
        let region = PatchBounds { x, y, w, h };
        if let Some(patch) =
            LayerPatch::from_region_diff(&before, &after, region, size.width, size.height)
        {
            ctx.history
                .borrow_mut()
                .record(HistoryAction::Liquify { layer_id: id, patch });
        }
        ctx.viewport.redraw_handle().request();
        true
    })
}

fn build_liquify_flush(ctx: &SessionCtx, bake_stroke: &Rc<dyn Fn() -> bool>) -> Rc<dyn Fn()> {
    let ctx = ctx.clone();
    let bake_stroke = Rc::clone(bake_stroke);
    Rc::new(move || {
        let active = ctx.canvas().borrow().liquify_active();
        if !active {
            ctx.liquify_pending.borrow_mut().take();
            return;
        }
        if !bake_stroke() {
            ctx.global
                .toaster
                .info("Liquify: the last warp could not be saved to the layer.");
        }
        ctx.liquify_pending.borrow_mut().take();
        if let Err(e) = ctx.canvas().borrow_mut().end_liquify() {
            tracing::error!(error = %e, "liquify: end session failed");
        }
        ctx.viewport.redraw_handle().request();
    })
}

fn build_liquify_restore(ctx: &SessionCtx) -> Rc<dyn Fn()> {
    let ctx = ctx.clone();
    Rc::new(move || {
        let canvas = ctx.canvas();
        let Some((idx, id)) = liquify_target(&ctx) else {
            return;
        };
        let pristine = {
            let pending = ctx.liquify_pending.borrow();
            let Some(p) = pending.as_ref() else {
                return;
            };
            p.pristine.clone()
        };
        let (size, before) = {
            let mut c = canvas.borrow_mut();
            let before = match c.read_layer(idx) {
                Ok(px) => px,
                Err(e) => {
                    tracing::error!(error = %e, "liquify: restore read failed");
                    return;
                }
            };
            if let Err(e) = c.liquify_restore_all() {
                tracing::error!(error = %e, "liquify: restore all failed");
                return;
            }
            if let Err(e) = c.liquify_bake() {
                tracing::error!(error = %e, "liquify: restore bake failed");
                return;
            }
            (c.size(), before)
        };
        if before.len() != pristine.len() {
            tracing::error!(
                before = before.len(),
                pristine = pristine.len(),
                "liquify: canvas resized during the session - skipping restore",
            );
            return;
        }
        if let Some(patch) =
            LayerPatch::from_full_diff(&before, &pristine, size.width, size.height)
        {
            ctx.history
                .borrow_mut()
                .record(HistoryAction::Liquify { layer_id: id, patch });
        }
        ctx.viewport.redraw_handle().request();
    })
}

fn build_liquify_cancel(restore: &Rc<dyn Fn()>, flush: &Rc<dyn Fn()>) -> Rc<dyn Fn()> {
    let restore = Rc::clone(restore);
    let flush = Rc::clone(flush);
    Rc::new(move || {
        restore();
        flush();
    })
}

fn build_transform_cancel(ctx: &SessionCtx) -> Rc<dyn Fn()> {
    let ctx = ctx.clone();
    Rc::new(move || {
        let canvas = ctx.canvas();
        let paintable = ctx.viewport.paintable();
        canvas.borrow_mut().clear_transform_preview();
        paintable.set_transform_gpu_preview(false);

        let filter = ctx.transform.filter.get();
        let targets = ctx.transform.targets.borrow().clone();
        for target in targets.iter().rev() {
            restore_target(
                &canvas,
                target,
                &ctx.components,
                &ctx.global.text_engine,
                &ctx.layer_extensions,
                filter,
            );
        }
        ctx.finish_transform();
    })
}

fn build_transform_apply(ctx: &SessionCtx, cancel: &Rc<dyn Fn()>) -> Rc<dyn Fn()> {
    let ctx = ctx.clone();
    let cancel = Rc::clone(cancel);
    Rc::new(move || {
        let canvas = ctx.canvas();
        let paintable = ctx.viewport.paintable();
        canvas.borrow_mut().clear_transform_preview();
        paintable.set_transform_gpu_preview(false);

        let (Some(rect), Some(original_rect)) =
            (ctx.transform.rect.get(), ctx.transform.original_rect.get())
        else {
            ctx.finish_transform();
            return;
        };
        let targets = ctx.transform.targets.borrow().clone();
        if targets.is_empty() {
            ctx.finish_transform();
            return;
        }
        let single = targets.len() == 1;
        let filter = ctx.transform.filter.get();

        canvas.borrow_mut().defer_recomposite(true);
        let mut actions: Vec<HistoryAction> = Vec::with_capacity(targets.len());
        for target in &targets {
            match commit_target(
                &canvas,
                target,
                original_rect,
                rect,
                single,
                filter,
                &ctx.components,
                &ctx.global.text_engine,
                &ctx.layer_extensions,
            ) {
                Ok(Some(action)) => actions.push(action),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e, "transform apply (GPU) failed");
                    canvas.borrow_mut().defer_recomposite(false);
                    cancel();
                    let msg = match &e {
                        RendererError::TransformTooLarge { limit, .. } => format!(
                            "Error: Can't transform the layer. Max layer texture size: {limit}"
                        ),
                        _ => format!("Error: transform failed: {e}"),
                    };
                    ctx.global.toaster.error(&msg);
                    return;
                }
            }
        }
        {
            let mut c = canvas.borrow_mut();
            c.defer_recomposite(false);
            if let Err(e) = c.recomposite() {
                tracing::error!(error = %e, "transform apply: final recomposite failed");
            }
        }

        tracing::info!(
            target: "oxiedraw::tool",
            targets = targets.len(),
            filter = ?filter,
            "transform applied"
        );

        if actions.len() > 1 {
            ctx.history
                .borrow_mut()
                .record(HistoryAction::Batch { label: "Transform".into(), actions });
        } else if let Some(action) = actions.pop() {
            ctx.history.borrow_mut().record(action);
        }
        ctx.finish_transform();
    })
}

#[derive(Clone)]
struct SessionCtx {
    global: GlobalState,
    set_active_tool: SetActiveToolSlot,
    history: Rc<RefCell<HistoryStack>>,
    components: Rc<RefCell<ComponentLibrary>>,
    layer_extensions: Rc<RefCell<HashMap<String, LayerExtension>>>,
    viewport: Viewport,
    crop: CropState,
    transform: TransformState,
    selection: SelectionState,
    liquify_pending: Rc<RefCell<Option<LiquifyPending>>>,
}

impl SessionCtx {
    fn canvas(&self) -> Rc<RefCell<Canvas>> {
        self.viewport.canvas()
    }

    fn set_tool(&self, tool: Tool) {
        if let Some(setter) = self.set_active_tool.borrow().as_ref() {
            setter(tool);
        }
    }

    fn finish_transform(&self) {
        self.transform.clear();
        self.transform.notify_changed();
        let paintable = self.viewport.paintable();
        paintable.set_transform_rect(None);
        paintable.set_transform_source(None, 0, 0, None);
        self.viewport.redraw_handle().request();
        self.set_tool(Tool::Cursor);
    }
}

impl DocumentSession {
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
        let components: Rc<RefCell<ComponentLibrary>> =
            Rc::new(RefCell::new(ComponentLibrary::new()));
        let crop = CropState::new();
        let transform = TransformState::new();
        let selection = SelectionState::new();
        let fill = FillState::new();
        let shape = ShapeState::new();
        let gradient = GradientState::new();
        let pattern = oxiedraw_core::patterns::PatternState::new();
        let pattern_regenerate: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let pattern_flush: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let pattern_discard: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let liquify = LiquifyState::new();
        let guide = GuideState::new();
        let doc_props = document.properties.clone();
        let viewport = Viewport::new(init_size, document.layers.clone());
        let layer_extensions: Rc<RefCell<HashMap<String, LayerExtension>>> =
            Rc::new(RefCell::new(HashMap::new()));

        {
            let canvas = viewport.canvas();
            let paintable = viewport.paintable().clone();
            let redraw = viewport.redraw_handle();
            let guide_c = guide.clone();
            guide.connect_changed(Box::new(move || {
                let cfg = guide_c.config.borrow().clone();
                let sym = cfg.as_ref().and_then(Symmetry::from_config);
                canvas.borrow_mut().set_symmetry(sym);
                paintable.set_guide(cfg);
                redraw.request();
            }));
        }

        let ctx = SessionCtx {
            global: global.clone(),
            set_active_tool: Rc::clone(set_active_tool_late),
            history: Rc::clone(&history),
            components: Rc::clone(&components),
            layer_extensions: Rc::clone(&layer_extensions),
            viewport: viewport.clone(),
            crop: crop.clone(),
            transform: transform.clone(),
            selection: selection.clone(),
            liquify_pending: Rc::new(RefCell::new(None)),
        };

        let crop_apply = build_crop_apply(&ctx);

        let liquify_bake_stroke = build_liquify_bake_stroke(&ctx);
        let liquify_flush = build_liquify_flush(&ctx, &liquify_bake_stroke);
        let liquify_restore = build_liquify_restore(&ctx);
        let liquify_cancel = build_liquify_cancel(&liquify_restore, &liquify_flush);
        let liquify_ensure = build_liquify_ensure(&ctx, &liquify_flush);

        let transform_cancel = build_transform_cancel(&ctx);

        let transform_apply = build_transform_apply(&ctx, &transform_cancel);

        let text_edit_slot: Rc<RefCell<Option<crate::text_edit::TextEdit>>> =
            Rc::new(RefCell::new(None));
        let on_selection_heatmap: Rc<dyn Fn(bool)> = {
            let canvas = viewport.canvas();
            let redraw = viewport.redraw_handle();
            Rc::new(move |on: bool| {
                canvas.borrow_mut().set_selection_heatmap(on);
                redraw.request();
            })
        };
        let pattern_apply: Rc<dyn Fn()> = {
            let pattern_flush = Rc::clone(&pattern_flush);
            Rc::new(move || call_slot(&pattern_flush))
        };
        let pattern_cancel: Rc<dyn Fn()> = {
            let pattern_discard = Rc::clone(&pattern_discard);
            Rc::new(move || call_slot(&pattern_discard))
        };
        let (tool_options_widget, set_tool_options) = crate::panels::tool_options::build(
            &global.tools,
            &global.brush_engine,
            &crop,
            Rc::clone(&crop_apply),
            &transform,
            Rc::clone(&transform_apply),
            Rc::clone(&transform_cancel),
            &selection,
            on_selection_heatmap,
            &fill,
            &shape,
            &gradient,
            &liquify,
            Rc::clone(&pattern_apply),
            Rc::clone(&pattern_cancel),
            &text_edit_slot,
            global.default_brush_name.clone(),
            global.toaster.clone(),
        );

        let select_layer_content = build_select_layer_content(&ctx);
        let select_folder_content = build_select_folder_content(&ctx);

        let saved_marker = Rc::new(Cell::new(0usize));

        let edit_mode: Rc<RefCell<Option<ComponentEditContext>>> = Rc::new(RefCell::new(None));
        let enter_slot: Rc<RefCell<Option<Rc<dyn Fn(String)>>>> = Rc::new(RefCell::new(None));
        let component_exit: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let alpha_lock_observer: Rc<RefCell<Option<Rc<dyn Fn(bool)>>>> =
            Rc::new(RefCell::new(None));

        let on_edit_component: Rc<dyn Fn(String)> = {
            let enter_slot = Rc::clone(&enter_slot);
            Rc::new(move |id: String| {
                let cb = enter_slot.borrow().clone();
                if let Some(cb) = cb {
                    cb(id);
                }
            })
        };

        let prepare_delete: Rc<dyn Fn() -> bool> = {
            let transform = transform.clone();
            let transform_cancel = Rc::clone(&transform_cancel);
            let liquify_flush = Rc::clone(&liquify_flush);
            let pattern_flush = Rc::clone(&pattern_flush);
            Rc::new(move || {
                liquify_flush();
                call_slot(&pattern_flush);
                prepare_transform_for_delete(&transform, || transform_cancel())
            })
        };

        let prepare_reorder: Rc<dyn Fn()> = {
            let transform = transform.clone();
            let transform_apply = Rc::clone(&transform_apply);
            let liquify_flush = Rc::clone(&liquify_flush);
            let pattern_flush = Rc::clone(&pattern_flush);
            Rc::new(move || {
                liquify_flush();
                call_slot(&pattern_flush);
                let in_progress = transform.has_targets() || transform.rect.get().is_some();
                if in_progress {
                    transform_apply();
                }
            })
        };

        let cursor_activates_transform: Rc<dyn Fn()> = {
            let slot = Rc::clone(set_active_tool_late);
            let tools = global.tools.clone();
            Rc::new(move || {
                if tools.active.get() == Tool::Cursor
                    && let Some(setter) = slot.borrow().as_ref()
                {
                    setter(Tool::Transform);
                }
            })
        };

        let color_picker_widget = crate::panels::color_picker::build(global.colors.clone());
        let (
            layers_widget,
            refresh_layers,
            selected_layer_ids,
            reinstall_actions,
            refresh_components,
            set_component_edit,
            begin_rename,
            create_adjustment_layer,
        ) = crate::panels::layers::build(
            &document.layers,
            &viewport.canvas(),
            &viewport.redraw_handle(),
            &global.clipboard,
            &global.toaster,
            &select_layer_content,
            &select_folder_content,
            &history,
            &layer_extensions,
            &components,
            &on_edit_component,
            &component_exit,
            &prepare_delete,
            &prepare_reorder,
            &alpha_lock_observer,
        );

        let tool_windows = Rc::new(crate::panels::tool_windows::ToolWindows::build(
            &global.colors,
            &gradient,
            &crop,
            &pattern,
            {
                let slot = Rc::clone(&pattern_regenerate);
                Rc::new(move || {
                    let cb = slot.borrow().clone();
                    if let Some(cb) = cb {
                        cb();
                    }
                })
            },
            &guide,
            &viewport.canvas(),
            &text_edit_slot,
            &global.text_engine,
            &global.font_previews,
        ));
        let refresh_text_panel = Rc::clone(&tool_windows.refresh_text);
        let set_tool_window: Rc<dyn Fn(Tool)> = {
            let tool_windows = Rc::clone(&tool_windows);
            Rc::new(move |t| tool_windows.set_tool(t))
        };
        set_tool_window(global.tools.active.get());

        {
            let enter: Rc<dyn Fn(String)> = {
                let viewport = viewport.clone();
                let history = Rc::clone(&history);
                let components = Rc::clone(&components);
                let edit_mode = Rc::clone(&edit_mode);
                let refresh_layers = Rc::clone(&refresh_layers);
                let set_component_edit = Rc::clone(&set_component_edit);
                let saved_marker = Rc::clone(&saved_marker);
                let accent_widget = layers_widget.clone().upcast::<gtk::Widget>();
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

        let apply_tool: Rc<dyn Fn(Tool)> = build_apply_tool(
            &viewport,
            &crop,
            &transform,
            &selection,
            &guide,
            &layer_extensions,
            &components,
            &global.text_engine,
            Rc::clone(&liquify_flush),
            Rc::clone(&set_tool_options),
            Rc::clone(&set_tool_window),
        );

        let picture = gtk::Picture::builder()
            .hexpand(true)
            .vexpand(true)
            .content_fit(gtk::ContentFit::Fill)
            .build();

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
        text_edit.connect_color();
        *text_edit_slot.borrow_mut() = Some(text_edit.clone());
        {
            let refresh = Rc::clone(&refresh_text_panel);
            text_edit.connect_changed(Box::new(move || refresh()));
        }

        let pattern_edit = crate::pattern_edit::PatternEdit::new(
            &viewport.canvas(),
            &pattern,
            &global.colors,
            &guide,
            &history,
            viewport.paintable(),
            &viewport.redraw_handle(),
            &viewport.canvas_size_handle(),
            &viewport.zoom_handle(),
        );
        *pattern_regenerate.borrow_mut() = Some({
            let pattern_edit = pattern_edit.clone();
            Rc::new(move || pattern_edit.settings_changed())
        });
        *pattern_flush.borrow_mut() = Some({
            let pattern_edit = pattern_edit.clone();
            Rc::new(move || {
                pattern_edit.commit();
            })
        });
        *pattern_discard.borrow_mut() = Some({
            let pattern_edit = pattern_edit.clone();
            Rc::new(move || {
                pattern_edit.cancel();
            })
        });

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
            &pattern_edit,
            &liquify,
            Rc::clone(&liquify_ensure),
            Rc::clone(&liquify_bake_stroke),
            &guide,
            &history,
            &global.toaster,
            &text_edit,
            Rc::clone(&cursor_activates_transform),
        );

        let info_bar = {
            let viewport_c = viewport.clone();
            let redraw = viewport.redraw_handle();
            let on_rotate: Rc<dyn Fn(f32)> = Rc::new(move |theta| {
                let step = canvas::rotation_snap_rad();
                viewport_c.rotate_to((theta / step).round() * step);
                redraw.request();
            });
            crate::panels::canvas_info::CanvasInfoBar::new(on_rotate)
        };
        viewport.set_info_observer(info_bar.observer());
        {
            let bar = info_bar.clone();
            *alpha_lock_observer.borrow_mut() =
                Some(Rc::new(move |locked: bool| bar.set_alpha_locked(locked)));
        }
        let canvas_root = {
            let overlay = gtk::Overlay::builder().child(&picture).build();
            tool_windows.attach(&overlay);
            overlay.upcast::<gtk::Widget>()
        };

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
                let _span = frame_profile::span(frame_profile::Stage::Timers);
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

        {
            let weak = Rc::downgrade(&alive);
            let history = Rc::clone(&history);
            let saved = Rc::clone(&saved_marker);
            let title = Rc::clone(&title);
            let tab_page = Rc::clone(&tab_page);
            let last_dirty = Cell::new(false);
            glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                let _span = frame_profile::span(frame_profile::Stage::Timers);
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
            pattern,
            liquify,
            guide,
            viewport,
            doc_props,
            layer_extensions,
            components,
            refresh_components,
            edit_mode,
            exit_component_edit,
            text_edit,
            pattern_edit,
            apply_tool,
            set_active_tool: Rc::clone(set_active_tool_late),
            transform_apply,
            transform_cancel,
            pattern_apply,
            pattern_cancel,
            liquify_cancel,
            liquify_restore,
            liquify_flush,
            crop_apply,
            refresh_layers,
            create_adjustment_layer,
            begin_rename,
            selected_layer_ids,
            set_tool_window,
            set_tool_options,
            reinstall_actions,
            color_picker: color_picker_widget.upcast::<gtk::Widget>(),
            layers: layers_widget.upcast::<gtk::Widget>(),
            tool_options: tool_options_widget.upcast::<gtk::Widget>(),
            canvas_info: info_bar.widget(),
            tool_windows,
            picture,
            canvas_root,
            file_path: RefCell::new(None),
            saved_marker,
            title,
            tab_page,
            recovery_file: RefCell::new(None),
            last_autosave_len: Cell::new(None),
            _alive: alive,
        })
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.history.borrow().undo_len() != self.saved_marker.get()
    }

    pub(crate) fn mark_saved(&self) {
        self.saved_marker.set(self.history.borrow().undo_len());
    }

    pub(crate) fn change_counter(&self) -> usize {
        self.history.borrow().undo_len()
    }

    pub(crate) fn ensure_recovery_path(&self) -> Option<PathBuf> {
        if let Some(p) = self.recovery_file.borrow().as_ref() {
            return Some(p.clone());
        }
        let dir = crate::settings::recovery_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(err = %e, "failed to create recovery directory");
            return None;
        }
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!(
            "recovery-{}-{n}.oxiedrawproj",
            std::process::id()
        ));
        *self.recovery_file.borrow_mut() = Some(path.clone());
        Some(path)
    }

    pub(crate) fn clear_recovery(&self) {
        if let Some(path) = self.recovery_file.borrow_mut().take() {
            let _ = std::fs::remove_file(path);
        }
        self.last_autosave_len.set(None);
    }

    pub(crate) fn display_title(&self) -> String {
        let base = self.title.borrow().clone();
        if self.is_dirty() {
            format!("*{base}")
        } else {
            base
        }
    }

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

    pub(crate) fn current_properties(&self) -> DocumentProperties {
        let mut props = self.doc_props.clone();
        props.canvas = self.viewport.canvas().borrow().size();
        props
    }

    fn editing_pattern(&self) -> bool {
        self.global.tools.active.get() == Tool::Pattern
    }

    fn select_tool(&self, tool: Tool) {
        if self.global.tools.active.get() == tool {
            return;
        }
        if let Some(setter) = self.set_active_tool.borrow().as_ref() {
            setter(tool);
        }
    }

    pub(crate) fn undo(&self) {
        if self.editing_pattern()
            && let Some(label) = self.pattern_edit.undo()
        {
            self.global.toaster.info(&format!("Undo: {label}"));
            return;
        }
        self.viewport.flush_pending_correction();
        (self.liquify_flush)();
        let canvas = self.viewport.canvas();
        let ext_reconcile = self.history.borrow().undo_ext_reconcile();
        let restores_curve = self.history.borrow().undo_takes_back_a_pattern();
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
            reconcile_extensions(&self.layer_extensions, ext_reconcile);
            let handed_back = restores_curve && self.pattern_edit.restore_last_applied();
            self.viewport.resync_canvas_size();
            (self.refresh_layers)();
            (self.refresh_components)();
            refresh_selection_after_history(
                &canvas,
                &self.selection,
                &self.viewport.canvas_size_handle(),
            );
            self.viewport.redraw_handle().request();
            if handed_back {
                self.select_tool(Tool::Pattern);
            }
            self.global.toaster.info(&format!("Undo: {l}"));
            self.refresh_tab_title();
        }
    }

    pub(crate) fn redo(&self) {
        if self.editing_pattern()
            && let Some(label) = self.pattern_edit.redo()
        {
            self.global.toaster.info(&format!("Redo: {label}"));
            return;
        }
        self.viewport.flush_pending_correction();
        (self.liquify_flush)();
        let canvas = self.viewport.canvas();
        let ext_reconcile = self.history.borrow().redo_ext_reconcile();
        let takes_curve = self.history.borrow().redo_puts_back_a_pattern();
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
            reconcile_extensions(&self.layer_extensions, ext_reconcile);
            let handed_back = takes_curve && self.pattern_edit.stash_live();
            self.viewport.resync_canvas_size();
            (self.refresh_layers)();
            (self.refresh_components)();
            refresh_selection_after_history(
                &canvas,
                &self.selection,
                &self.viewport.canvas_size_handle(),
            );
            self.viewport.redraw_handle().request();
            if handed_back {
                self.select_tool(Tool::Pattern);
            }
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

    pub(crate) fn is_editing_component(&self) -> bool {
        self.edit_mode.borrow().is_some()
    }

    pub(crate) fn escape_component_edit(&self) -> bool {
        if self.is_editing_component() {
            (self.exit_component_edit)();
            true
        } else {
            false
        }
    }

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

    pub(crate) fn cancel_crop(&self) {
        self.crop.rect.set(None);
        self.crop.notify_rect_changed();
        self.viewport.redraw_handle().request();
    }

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

const COMPONENT_ACCENT: (f32, f32, f32) = (0.21, 0.52, 0.89);

#[allow(deprecated)]
pub(crate) fn accent_rgb(widget: &gtk::Widget) -> (f32, f32, f32) {
    widget
        .style_context()
        .lookup_color("accent_bg_color")
        .or_else(|| widget.style_context().lookup_color("accent_color"))
        .map_or(COMPONENT_ACCENT, |c| (c.red(), c.green(), c.blue()))
}

pub(crate) struct ComponentEditContext {
    component_id: String,
    main_size: Size,
    main_layers: Vec<StashedLayer>,
    main_active: Option<usize>,
    main_history: HistoryStack,
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

    // replace_all_layers resets every kind to Raster, so re-apply them after.
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
        let kinds: Vec<_> = ctx.main_layers.iter().map(|l| l.kind.clone()).collect();
        if let Err(e) = canvas.borrow_mut().restore_layer_kinds(&kinds) {
            tracing::error!(error = %e, "restoring layer kinds after component edit failed");
        }
    }

    *history.borrow_mut() = ctx.main_history;
    saved_marker.set(ctx.main_saved_marker);

    rerender_component_instances(viewport, components, &ctx.component_id);

    viewport
        .paintable()
        .set_edit_mode("Main canvas", false, COMPONENT_ACCENT);
    refresh_layers();
    refresh_components();
    viewport.redraw_handle().request();
    set_component_edit(None);
}

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

fn prepare_transform_for_delete(transform: &TransformState, cancel: impl FnOnce()) -> bool {
    let in_progress = transform.has_targets() || transform.rect.get().is_some();
    if !in_progress {
        return true;
    }
    let was_paste = transform.targets.borrow().iter().any(|t| t.is_paste);
    cancel();
    !was_paste
}

fn snapshot_selection(canvas: &Rc<RefCell<Canvas>>) -> SelectionSnapshot {
    crate::canvas::primary_drag::read_selection_snapshot(&mut canvas.borrow_mut())
}

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

fn start_transform_gpu_preview(
    canvas: &Rc<RefCell<Canvas>>,
    paintable: &crate::canvas_paintable::CanvasPaintable,
    transform: &TransformState,
) {
    let (Some(orig), Some(rect)) = (transform.original_rect.get(), transform.rect.get()) else {
        return;
    };
    let targets = transform.targets.borrow();
    if targets.is_empty() {
        return;
    }
    let mut sources: Vec<(usize, &[u8], u32, u32)> = Vec::with_capacity(targets.len());
    for t in targets.iter() {
        let (w, h) = t.src_dims;
        if t.pixels.len() != (w as usize) * (h as usize) * 4 {
            return;
        }
        sources.push((t.layer_idx, &t.pixels, w, h));
    }
    let (sw, sh) = targets[0].src_dims;
    let mut c = canvas.borrow_mut();
    match c.begin_transform_preview_gpu(&sources) {
        Ok(()) => {
            c.set_transform_preview(orig, rect, sw, sh);
            drop(c);
            paintable.set_transform_gpu_preview(true);
        }
        Err(e) => tracing::error!(error = %e, "transform: begin_transform_preview_gpu failed"),
    }
}

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

fn build_apply_tool(
    viewport: &Viewport,
    crop: &CropState,
    transform: &TransformState,
    selection: &SelectionState,
    guide: &GuideState,
    layer_extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
    components: &Rc<RefCell<ComponentLibrary>>,
    text_engine: &Rc<RefCell<oxiedraw_core::text::fonts::TextEngine>>,
    liquify_flush: Rc<dyn Fn()>,
    set_tool_options: Rc<dyn Fn(Tool)>,
    set_tool_window: Rc<dyn Fn(Tool)>,
) -> Rc<dyn Fn(Tool)> {
    let paintable = viewport.paintable().clone();
    let crop_for_tool = crop.clone();
    let transform_for_tool = transform.clone();
    let guide_for_tool = guide.clone();
    let viewport_for_guide = viewport.clone();
    let canvas_for_tool = viewport.canvas();
    let redraw_for_tool = viewport.redraw_handle();
    let extensions_for_sat = Rc::clone(layer_extensions);
    let selection_for_sat = selection.clone();
    let components_for_tool = Rc::clone(components);
    let text_engine_for_tool = Rc::clone(text_engine);
    Rc::new(move |t: Tool| {
        set_tool_options(t);
        set_tool_window(t);

        if t != Tool::Liquify {
            liquify_flush();
        }

        paintable.set_crop_active(t == Tool::Crop);
        if t != Tool::ColorPicker {
            paintable.set_color_picker(None);
        }

        paintable.set_guide_editing(t == Tool::DrawingGuide);
        if t == Tool::DrawingGuide {
            let accent_rgb = viewport_for_guide
                .picture_widget()
                .map_or(COMPONENT_ACCENT, |w| accent_rgb(w.upcast_ref::<gtk::Widget>()));
            paintable.set_guide_accent(accent_rgb);
            if guide_for_tool.config.borrow().is_none() {
                let cs = canvas_for_tool.borrow().size();
                let mut cfg = GuideConfig::centered(cs.width, cs.height);
                cfg.color =
                    oxiedraw_core::guides::guide_pos_from_rgb(accent_rgb.0, accent_rgb.1, accent_rgb.2);
                *guide_for_tool.config.borrow_mut() = Some(cfg);
            }
            guide_for_tool
                .entry_snapshot
                .borrow_mut()
                .clone_from(&guide_for_tool.config.borrow());
            guide_for_tool.notify_changed();
        } else {
            redraw_for_tool.request();
        }
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
                let (src, dims, max_idx) = {
                    let targets = transform_for_tool.targets.borrow();
                    let src = targets.first().map(|t| (t.pixels.clone(), t.src_dims));
                    let max_idx = targets.iter().map(|t| t.layer_idx).max();
                    (src.as_ref().map(|s| s.0.clone()), src.map(|s| s.1), max_idx)
                };
                if let (Some(px), Some((sw, sh))) = (src.as_ref(), dims) {
                    paintable.set_transform_source(
                        Some(px),
                        sw,
                        sh,
                        transform_for_tool.original_rect.get(),
                    );
                }
                paintable.set_transform_rect(transform_for_tool.rect.get());
                if let Some(idx) = max_idx {
                    capture_transform_above(&canvas_for_tool, &paintable, idx);
                }
                start_transform_gpu_preview(&canvas_for_tool, &paintable, &transform_for_tool);
                redraw_for_tool.request();
                transform_for_tool.notify_changed();
            } else {
                let indices: Vec<usize> = {
                    let c = canvas_for_tool.borrow();
                    let snap = c.layers().snapshot();
                    let ids = c.layers().selected_leaves();
                    let mut v: Vec<usize> = ids
                        .iter()
                        .filter_map(|id| snap.iter().position(|l| &l.id == id))
                        .collect();
                    if v.is_empty()
                        && let Some(a) = c.layers().active()
                    {
                        v.push(a);
                    }
                    v
                };
                let has_pixel_selection = canvas_for_tool.borrow().selection_active();
                let single_idx = if has_pixel_selection {
                    canvas_for_tool
                        .borrow()
                        .layers()
                        .active()
                        .or_else(|| indices.first().copied())
                } else if indices.len() <= 1 {
                    indices.first().copied()
                } else {
                    None
                };
                if let Some(idx) = single_idx {
                    seed_single_transform(
                        &canvas_for_tool,
                        &paintable,
                        &transform_for_tool,
                        &selection_for_sat,
                        &components_for_tool,
                        &text_engine_for_tool,
                        &extensions_for_sat,
                        idx,
                    );
                    redraw_for_tool.request();
                } else if indices.len() > 1 {
                    seed_multi_transform(
                        &canvas_for_tool,
                        &paintable,
                        &transform_for_tool,
                        &selection_for_sat,
                        &components_for_tool,
                        &text_engine_for_tool,
                        &extensions_for_sat,
                        &indices,
                    );
                    redraw_for_tool.request();
                }
            }
        } else {
            canvas_for_tool.borrow_mut().clear_transform_preview();
            paintable.set_transform_gpu_preview(false);
            let filter = transform_for_tool.filter.get();
            let targets = transform_for_tool.targets.borrow().clone();
            if !targets.is_empty() {
                for target in targets.iter().rev() {
                    restore_target(
                        &canvas_for_tool,
                        target,
                        &components_for_tool,
                        &text_engine_for_tool,
                        &extensions_for_sat,
                        filter,
                    );
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

#[allow(clippy::too_many_arguments)]
fn seed_multi_transform(
    canvas: &Rc<RefCell<Canvas>>,
    paintable: &crate::canvas_paintable::CanvasPaintable,
    transform: &TransformState,
    selection: &SelectionState,
    components: &Rc<RefCell<ComponentLibrary>>,
    text_engine: &Rc<RefCell<oxiedraw_core::text::fonts::TextEngine>>,
    extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
    indices: &[usize],
) {
    let cs = canvas.borrow().size();
    struct Raw {
        idx: usize,
        layer_id: String,
        kind: LayerKind,
        canvas_pixels: Vec<u8>,
        ext_rect: Option<(i32, i32, u32, u32)>,
    }
    let mut raws: Vec<Raw> = Vec::new();
    {
        let mut c = canvas.borrow_mut();
        for &idx in indices {
            let Some(kind) = c.layers().kind(idx) else { continue };
            if matches!(kind, LayerKind::Adjustment(_)) {
                continue;
            }
            let Some(layer_id) = c.layers().snapshot().get(idx).map(|l| l.id.clone()) else {
                continue;
            };
            let Ok(canvas_pixels) = c.read_layer(idx) else { continue };
            let ext_rect = extensions
                .borrow()
                .get(&layer_id)
                .map(|e| (e.offset_x, e.offset_y, e.width, e.height));
            if ext_rect.is_none()
                && non_empty_bounds(&canvas_pixels, cs.width, cs.height).is_none()
            {
                continue;
            }
            raws.push(Raw { idx, layer_id, kind, canvas_pixels, ext_rect });
        }
    }
    if raws.len() < 2 {
        if let Some(r) = raws.first() {
            seed_single_transform(
                canvas, paintable, transform, selection, components, text_engine, extensions, r.idx,
            );
        }
        return;
    }
    let ext_rects: Vec<(i32, i32, u32, u32)> = raws.iter().filter_map(|r| r.ext_rect).collect();
    let (ox, oy, cw, ch) = frame_union(cs, &ext_rects);
    let expanded = (ox, oy, cw, ch) != (0, 0, cs.width, cs.height);
    let src_offset = if expanded { Some((ox, oy)) } else { None };
    #[allow(clippy::cast_precision_loss)]
    let (oxf, oyf) = (ox as f32, oy as f32);

    let mut targets: Vec<TransformTarget> = Vec::with_capacity(raws.len());
    for r in raws {
        let ext = if r.ext_rect.is_some() {
            extensions.borrow_mut().remove(&r.layer_id)
        } else {
            None
        };
        let (pixels, dims) = if expanded {
            (place_into_frame(cs, &r.canvas_pixels, ext.as_ref(), ox, oy, cw, ch), (cw, ch))
        } else {
            (r.canvas_pixels, (cs.width, cs.height))
        };
        let Some(orig_bounds) = non_empty_bounds(&pixels, dims.0, dims.1) else { continue };
        let kind = match r.kind {
            LayerKind::Text(content) => {
                let mut g = content.visible_rect();
                g.cx -= oxf;
                g.cy -= oyf;
                TargetKind::Text { layer_id: r.layer_id, orig_geom: g }
            }
            LayerKind::Component(inst) => {
                let mut g = inst.placement.to_rect();
                g.cx -= oxf;
                g.cy -= oyf;
                TargetKind::Component { component_id: inst.component_id, orig_geom: g }
            }
            _ => TargetKind::Raster,
        };
        targets.push(TransformTarget {
            layer_idx: r.idx,
            pixels,
            src_dims: dims,
            src_offset,
            orig_bounds,
            is_paste: false,
            history_before: None,
            orig_extension: ext,
            kind,
        });
    }
    if targets.len() < 2 {
        return;
    }
    let Some(union) = union_bounds(&targets) else {
        return;
    };
    {
        let mut c = canvas.borrow_mut();
        for t in &targets {
            if let Err(e) = c.clear_layer_at(t.layer_idx, [0.0, 0.0, 0.0, 0.0]) {
                tracing::error!(error = %e, "multi transform: clear failed");
            }
        }
    }
    let rest = TransformRect::new(union.cx + oxf, union.cy + oyf, union.w, union.h, 0.0);
    let max_idx = targets.iter().map(|t| t.layer_idx).max().unwrap_or(0);
    transform.original_rect.set(Some(union));
    transform.rect.set(Some(rest));
    *transform.targets.borrow_mut() = targets;
    paintable.set_transform_source(None, 0, 0, None);
    capture_transform_above(canvas, paintable, max_idx);
    paintable.set_transform_rect(Some(rest));
    transform.notify_changed();
    start_transform_gpu_preview(canvas, paintable, transform);
}

#[allow(clippy::too_many_arguments)]
fn seed_single_transform(
    canvas: &Rc<RefCell<Canvas>>,
    paintable: &crate::canvas_paintable::CanvasPaintable,
    transform: &TransformState,
    selection: &SelectionState,
    components: &Rc<RefCell<ComponentLibrary>>,
    text_engine: &Rc<RefCell<oxiedraw_core::text::fonts::TextEngine>>,
    extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
    idx: usize,
) {
    let cs = canvas.borrow().size();
    let kind = canvas.borrow().layers().kind(idx);

    let captured: Option<(TransformTarget, TransformRect, TransformRect)> = match kind {
        Some(LayerKind::Component(inst)) => {
            let master = components
                .borrow()
                .get(&inst.component_id)
                .map(|c| (c.master.clone(), c.size.width, c.size.height));
            master.map(|(master, mw, mh)| {
                let placement_rect = inst.placement.to_rect();
                #[allow(clippy::cast_precision_loss)]
                let orig_full =
                    TransformRect::new(mw as f32 / 2.0, mh as f32 / 2.0, mw as f32, mh as f32, 0.0);
                if let Err(e) = canvas.borrow_mut().clear_layer_at(idx, [0.0, 0.0, 0.0, 0.0]) {
                    tracing::error!(error = %e, "component transform: clear failed");
                }
                let target = TransformTarget {
                    layer_idx: idx,
                    pixels: master,
                    src_dims: (mw, mh),
                    src_offset: None,
                    orig_bounds: orig_full,
                    is_paste: false,
                    history_before: None,
                    orig_extension: None,
                    kind: TargetKind::Component {
                        component_id: inst.component_id,
                        orig_geom: placement_rect,
                    },
                };
                (target, orig_full, placement_rect)
            })
        }
        Some(LayerKind::Text(content)) => {
            let layer_id = canvas.borrow().layers().snapshot().get(idx).map(|l| l.id.clone());
            let (local, lw, lh) = {
                let mut engine = text_engine.borrow_mut();
                oxiedraw_core::text::render::render_text_local(&content, &mut engine)
            };
            #[allow(clippy::cast_precision_loss)]
            let orig_full =
                TransformRect::new(lw as f32 / 2.0, lh as f32 / 2.0, lw as f32, lh as f32, 0.0);
            let current_rect = content.visible_rect();
            if let Err(e) = canvas.borrow_mut().clear_layer_at(idx, [0.0, 0.0, 0.0, 0.0]) {
                tracing::error!(error = %e, "text transform: clear failed");
            }
            layer_id.map(|id| {
                let target = TransformTarget {
                    layer_idx: idx,
                    pixels: local,
                    src_dims: (lw, lh),
                    src_offset: None,
                    orig_bounds: orig_full,
                    is_paste: false,
                    history_before: None,
                    orig_extension: None,
                    kind: TargetKind::Text { layer_id: id, orig_geom: current_rect },
                };
                (target, orig_full, current_rect)
            })
        }
        _ => {
            let layer_id = canvas.borrow().layers().snapshot().get(idx).map(|l| l.id.clone());
            let extension = if canvas.borrow().selection_active() {
                None
            } else {
                layer_id
                    .as_ref()
                    .and_then(|id| extensions.borrow_mut().remove(id))
            };
            if let Some(ext) = extension {
                let gpu_pixels = canvas.borrow_mut().read_layer(idx).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "transform: read_layer for merge failed; using extension only");
                    vec![0u8; (cs.width * cs.height * 4) as usize]
                });
                let (mx, my, mw, mh) =
                    frame_union(cs, &[(ext.offset_x, ext.offset_y, ext.width, ext.height)]);
                let merged = place_into_frame(cs, &gpu_pixels, Some(&ext), mx, my, mw, mh);
                #[allow(clippy::cast_precision_loss)]
                let tight = non_empty_bounds(&merged, mw, mh).unwrap_or_else(|| {
                    TransformRect::new(mw as f32 / 2.0, mh as f32 / 2.0, mw as f32, mh as f32, 0.0)
                });
                #[allow(clippy::cast_precision_loss)]
                let current_rect = TransformRect::new(
                    mx as f32 + tight.cx, my as f32 + tight.cy, tight.w, tight.h, 0.0,
                );
                if let Err(e) = canvas.borrow_mut().clear_layer_at(idx, [0.0, 0.0, 0.0, 0.0]) {
                    tracing::error!(error = %e, "transform: clear_layer_at (ext) failed");
                }
                let target = TransformTarget {
                    layer_idx: idx,
                    pixels: merged,
                    src_dims: (mw, mh),
                    src_offset: Some((mx, my)),
                    orig_bounds: tight,
                    is_paste: false,
                    history_before: None,
                    orig_extension: Some(ext),
                    kind: TargetKind::Raster,
                };
                Some((target, tight, current_rect))
            } else {
                let lift_idx = selection
                    .source_layer
                    .get()
                    .filter(|&i| i < canvas.borrow().layers().len())
                    .unwrap_or(idx);
                let original_full = if canvas.borrow().selection_active() {
                    canvas.borrow_mut().read_layer(lift_idx).ok()
                } else {
                    None
                };
                let selection_lift = if canvas.borrow().selection_active() {
                    match canvas.borrow_mut().extract_selection_pixels(lift_idx) {
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
                    match canvas.borrow_mut().read_layer(idx) {
                        Ok(px) => Some((px, cs.width, cs.height, false, idx)),
                        Err(e) => {
                            tracing::error!(error = %e, "transform: read_layer failed");
                            None
                        }
                    }
                };
                lifted.map(|(pixels, src_w, src_h, from_selection, target_idx)| {
                    #[allow(clippy::cast_precision_loss)]
                    let orig_rect = non_empty_bounds(&pixels, src_w, src_h).unwrap_or_else(|| {
                        TransformRect::new(
                            src_w as f32 / 2.0, src_h as f32 / 2.0, src_w as f32, src_h as f32, 0.0,
                        )
                    });
                    if !from_selection
                        && let Err(e) =
                            canvas.borrow_mut().clear_layer_at(target_idx, [0.0, 0.0, 0.0, 0.0])
                    {
                        tracing::error!(error = %e, "transform: clear_layer_at failed");
                    }
                    if from_selection {
                        selection.active.set(false);
                        selection.ants_contours.borrow_mut().clear();
                        selection.source_layer.set(None);
                        selection.notify_changed();
                    }
                    let target = TransformTarget {
                        layer_idx: target_idx,
                        pixels,
                        src_dims: (src_w, src_h),
                        src_offset: None,
                        orig_bounds: orig_rect,
                        is_paste: false,
                        history_before: if from_selection { original_full } else { None },
                        orig_extension: None,
                        kind: TargetKind::Raster,
                    };
                    (target, orig_rect, orig_rect)
                })
            }
        }
    };

    let Some((target, orig_rect, start_rect)) = captured else {
        return;
    };
    let target_idx = target.layer_idx;
    paintable.set_transform_source(
        Some(&target.pixels),
        target.src_dims.0,
        target.src_dims.1,
        Some(orig_rect),
    );
    transform.original_rect.set(Some(orig_rect));
    transform.rect.set(Some(start_rect));
    *transform.targets.borrow_mut() = vec![target];
    capture_transform_above(canvas, paintable, target_idx);
    paintable.set_transform_rect(Some(start_rect));
    transform.notify_changed();
    start_transform_gpu_preview(canvas, paintable, transform);
}

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

#[allow(clippy::option_option)]
fn reconcile_extensions(
    extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
    reconcile: Vec<(String, Option<Option<LayerExtension>>)>,
) {
    let mut map = extensions.borrow_mut();
    for (id, change) in reconcile {
        match change {
            None => {}
            Some(None) => {
                map.remove(&id);
            }
            Some(Some(ext)) => {
                map.insert(id, ext);
            }
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn source_extends_off_canvas(src: &[u8], sw: u32, sh: u32, ox: i32, oy: i32, cs: Size) -> bool {
    let Some(b) = non_empty_bounds(src, sw, sh) else {
        return false;
    };
    let left = b.cx - b.w / 2.0 + ox as f32;
    let top = b.cy - b.h / 2.0 + oy as f32;
    let right = b.cx + b.w / 2.0 + ox as f32;
    let bottom = b.cy + b.h / 2.0 + oy as f32;
    left < 0.0 || top < 0.0 || right > cs.width as f32 || bottom > cs.height as f32
}

fn union_bounds(targets: &[TransformTarget]) -> Option<TransformRect> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for t in targets {
        let b = t.orig_bounds;
        min_x = min_x.min(b.cx - b.w / 2.0);
        min_y = min_y.min(b.cy - b.h / 2.0);
        max_x = max_x.max(b.cx + b.w / 2.0);
        max_y = max_y.max(b.cy + b.h / 2.0);
    }
    if !min_x.is_finite() {
        return None;
    }
    let w = (max_x - min_x).max(1.0);
    let h = (max_y - min_y).max(1.0);
    Some(TransformRect::new(min_x + w / 2.0, min_y + h / 2.0, w, h, 0.0))
}

#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn blit_over(src: &[u8], sw: u32, sh: u32, dst: &mut [u8], dw: u32, dh: u32, dx: i32, dy: i32) {
    for row in 0..sh {
        let ty = dy + row as i32;
        if ty < 0 || ty >= dh as i32 {
            continue;
        }
        let x0 = dx.max(0);
        let x1 = (dx + sw as i32).min(dw as i32);
        if x0 >= x1 {
            continue;
        }
        let sx0 = (x0 - dx) as u32;
        let si = ((row * sw + sx0) as usize) * 4;
        let di = ((ty as u32 * dw + x0 as u32) as usize) * 4;
        let len = ((x1 - x0) as usize) * 4;
        if si + len <= src.len() && di + len <= dst.len() {
            dst[di..di + len].copy_from_slice(&src[si..si + len]);
        }
    }
}

const MAX_FRAME_MARGIN: i32 = 4096;

const MAX_FRAME_DIM: i32 = 8192;

fn trim_margins(lo: &mut i32, hi: &mut i32, canvas_hi: i32, over: i32) {
    if over <= 0 {
        return;
    }
    let trim_left = (-*lo).max(0).min(over - over / 2);
    *lo += trim_left;
    let trim_right = (*hi - canvas_hi).max(0).min(over - trim_left);
    *hi -= trim_right;
}

fn frame_union(cs: Size, ext_rects: &[(i32, i32, u32, u32)]) -> (i32, i32, u32, u32) {
    let (mut min_x, mut min_y) = (0i32, 0i32);
    #[allow(clippy::cast_possible_wrap)]
    let (cw, ch) = (cs.width as i32, cs.height as i32);
    let (mut max_x, mut max_y) = (cw, ch);
    for &(ox, oy, w, h) in ext_rects {
        min_x = min_x.min(ox);
        min_y = min_y.min(oy);
        max_x = max_x.max(ox.saturating_add(w as i32));
        max_y = max_y.max(oy.saturating_add(h as i32));
    }
    min_x = min_x.max(-MAX_FRAME_MARGIN);
    min_y = min_y.max(-MAX_FRAME_MARGIN);
    max_x = max_x.min(cw + MAX_FRAME_MARGIN);
    max_y = max_y.min(ch + MAX_FRAME_MARGIN);
    let over_x = (max_x - min_x) - MAX_FRAME_DIM;
    let over_y = (max_y - min_y) - MAX_FRAME_DIM;
    trim_margins(&mut min_x, &mut max_x, cw, over_x);
    trim_margins(&mut min_y, &mut max_y, ch, over_y);
    #[allow(clippy::cast_sign_loss)]
    (min_x, min_y, (max_x - min_x) as u32, (max_y - min_y) as u32)
}

fn place_into_frame(
    cs: Size,
    canvas_pixels: &[u8],
    ext: Option<&LayerExtension>,
    ox: i32,
    oy: i32,
    cw: u32,
    ch: u32,
) -> Vec<u8> {
    let mut buf = vec![0u8; (cw as usize) * (ch as usize) * 4];
    if let Some(e) = ext {
        blit_over(&e.pixels, e.width, e.height, &mut buf, cw, ch, e.offset_x - ox, e.offset_y - oy);
    }
    blit_over(canvas_pixels, cs.width, cs.height, &mut buf, cw, ch, -ox, -oy);
    buf
}

fn restore_target(
    canvas: &Rc<RefCell<Canvas>>,
    target: &TransformTarget,
    components: &Rc<RefCell<ComponentLibrary>>,
    text_engine: &Rc<RefCell<oxiedraw_core::text::fonts::TextEngine>>,
    extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
    filter: TransformFilter,
) {
    let idx = target.layer_idx;
    match &target.kind {
        TargetKind::Text { .. } => {
            let kind = canvas.borrow().layers().kind(idx);
            if let Some(LayerKind::Text(content)) = kind {
                let cs = canvas.borrow().size();
                let pixels = {
                    let mut engine = text_engine.borrow_mut();
                    oxiedraw_core::text::render::render_text(&content, &mut engine, cs.width, cs.height)
                };
                if let Err(e) = canvas.borrow_mut().restore_layer(idx, &pixels) {
                    tracing::error!(error = %e, "transform cancel: text restore failed");
                }
            }
        }
        TargetKind::Component { component_id, orig_geom } => {
            let size = canvas.borrow().size();
            let pixels = components.borrow().get(component_id).map(|c| {
                c.render_instance(size.width, size.height, Placement::from_rect(*orig_geom), filter)
            });
            if let Some(px) = pixels
                && let Err(e) = canvas.borrow_mut().restore_layer(idx, &px)
            {
                tracing::error!(error = %e, "transform cancel: component restore failed");
            }
        }
        TargetKind::Raster => {
            if target.is_paste {
                if let Err(e) = canvas.borrow_mut().remove_layer(idx) {
                    tracing::error!(error = %e, "transform cancel: remove paste layer failed");
                }
            } else if let Some((off_x, off_y)) = target.src_offset {
                let (ew, eh) = target.src_dims;
                let cs = canvas.borrow().size();
                let canvas_pix =
                    crop_from_extension(&target.pixels, off_x, off_y, ew, eh, cs.width, cs.height);
                let mut c = canvas.borrow_mut();
                let layer_id = c.layers().snapshot().get(idx).map(|l| l.id.clone());
                if let Err(e) = c.restore_layer(idx, &canvas_pix) {
                    tracing::error!(error = %e, "transform cancel: restore_layer (ext) failed");
                }
                drop(c);
                if let Some(id) = layer_id {
                    if source_extends_off_canvas(&target.pixels, ew, eh, off_x, off_y, cs) {
                        extensions.borrow_mut().insert(
                            id,
                            LayerExtension {
                                offset_x: off_x,
                                offset_y: off_y,
                                width: ew,
                                height: eh,
                                pixels: Rc::new(target.pixels.clone()),
                            },
                        );
                    } else {
                        extensions.borrow_mut().remove(&id);
                    }
                }
            } else {
                let restore = target.history_before.as_ref().unwrap_or(&target.pixels);
                if let Err(e) = canvas.borrow_mut().restore_layer(idx, restore) {
                    tracing::error!(error = %e, "transform cancel: restore_layer failed");
                }
                if target.history_before.is_none()
                    && let Some(id) = canvas.borrow().layers().snapshot().get(idx).map(|l| l.id.clone())
                {
                    extensions.borrow_mut().remove(&id);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn commit_target(
    canvas: &Rc<RefCell<Canvas>>,
    target: &TransformTarget,
    original_rect: TransformRect,
    rect: TransformRect,
    single: bool,
    filter: TransformFilter,
    components: &Rc<RefCell<ComponentLibrary>>,
    text_engine: &Rc<RefCell<oxiedraw_core::text::fonts::TextEngine>>,
    extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
) -> Result<Option<HistoryAction>, RendererError> {
    let idx = target.layer_idx;
    let cs = canvas.borrow().size();
    match &target.kind {
        TargetKind::Text { layer_id, orig_geom } => {
            let new_rect = if single { rect } else { orig_geom.remap(original_rect, rect) };
            let Some(LayerKind::Text(content)) = canvas.borrow().layers().kind(idx) else {
                return Ok(None);
            };
            let natural = content.box_rect;
            let before_content = content.clone();
            let mut after_content = content;
            let sx = if natural.w.abs() > 1e-3 { new_rect.w / natural.w } else { after_content.scale.0 };
            let sy = if natural.h.abs() > 1e-3 { new_rect.h / natural.h } else { after_content.scale.1 };
            after_content.bake_transform(new_rect.cx, new_rect.cy, new_rect.angle, sx, sy);
            let (before, after) = {
                let mut engine = text_engine.borrow_mut();
                let before = oxiedraw_core::text::render::render_text(
                    &before_content, &mut engine, cs.width, cs.height,
                );
                let after = oxiedraw_core::text::render::render_text(
                    &after_content, &mut engine, cs.width, cs.height,
                );
                (before, after)
            };
            if let Err(e) = canvas.borrow_mut().restore_layer(idx, &after) {
                tracing::error!(error = %e, "text transform apply: write failed");
            }
            canvas.borrow().layers().set_kind(idx, LayerKind::Text(after_content.clone()));
            Ok(LayerPatch::from_full_diff(&before, &after, cs.width, cs.height).map(|patch| {
                HistoryAction::TextEdit {
                    layer_id: layer_id.clone(),
                    patch,
                    before_content: Box::new(before_content),
                    after_content: Box::new(after_content),
                }
            }))
        }
        TargetKind::Component { component_id, orig_geom } => {
            let new_rect = if single { rect } else { orig_geom.remap(original_rect, rect) };
            let new_placement = Placement::from_rect(new_rect);
            let rendered = {
                let lib = components.borrow();
                lib.get(component_id).map(|comp| {
                    let before = comp.render_instance(
                        cs.width, cs.height, Placement::from_rect(*orig_geom), filter,
                    );
                    let after = comp.render_instance(cs.width, cs.height, new_placement, filter);
                    (before, after)
                })
            };
            let Some((before, after)) = rendered else {
                return Ok(None);
            };
            if let Err(e) = canvas.borrow_mut().restore_layer(idx, &after) {
                tracing::error!(error = %e, "component transform apply: write failed");
            }
            canvas.borrow().layers().set_kind(
                idx,
                LayerKind::Component(ComponentInstance {
                    component_id: component_id.clone(),
                    placement: new_placement,
                }),
            );
            let Some(layer_id) = canvas.borrow().layers().snapshot().get(idx).map(|l| l.id.clone())
            else {
                return Ok(None);
            };
            Ok(LayerPatch::from_full_diff(&before, &after, cs.width, cs.height).map(|patch| {
                HistoryAction::ComponentRetransform {
                    layer_id,
                    component_id: component_id.clone(),
                    patch,
                    before_placement: Placement::from_rect(*orig_geom),
                    after_placement: new_placement,
                }
            }))
        }
        TargetKind::Raster => {
            let (full_result, ext_x, ext_y, full_w, full_h) = canvas
                .borrow_mut()
                .apply_layer_transform_gpu(
                    idx, &target.pixels, target.src_dims.0, target.src_dims.1, original_rect, rect,
                )?;
            let Some((id, name, visible)) =
                canvas.borrow().layers().snapshot().get(idx).map(|l| {
                    (l.id.clone(), l.name.clone(), l.visible)
                })
            else {
                return Ok(None);
            };
            let (ext_before, ext_after): (
                Option<Option<LayerExtension>>,
                Option<Option<LayerExtension>>,
            ) = if target.history_before.is_none() {
                let is_outside = ext_x < 0
                    || ext_y < 0
                    || ext_x.saturating_add(full_w as i32) > cs.width as i32
                    || ext_y.saturating_add(full_h as i32) > cs.height as i32;
                let new_ext = is_outside.then(|| LayerExtension {
                    offset_x: ext_x,
                    offset_y: ext_y,
                    width: full_w,
                    height: full_h,
                    pixels: Rc::new(full_result),
                });
                match &new_ext {
                    Some(e) => {
                        extensions.borrow_mut().insert(id.clone(), e.clone());
                    }
                    None => {
                        extensions.borrow_mut().remove(&id);
                    }
                }
                (Some(target.orig_extension.clone()), Some(new_ext))
            } else {
                (None, None)
            };
            let after_px = canvas.borrow_mut().read_layer(idx).unwrap_or_default();
            if target.is_paste {
                Ok(Some(HistoryAction::LayerAdd {
                    idx,
                    id,
                    name,
                    visible,
                    layer_kind: LayerKind::Raster,
                    blend: oxiedraw_core::document::BlendMode::Normal,
                    opacity: 1.0,
                    pixels: after_px,
                }))
            } else {
                let before_canvas = match (&target.history_before, target.src_offset) {
                    (Some(before), _) => before.clone(),
                    (None, Some((off_x, off_y))) => {
                        let (ew, eh) = target.src_dims;
                        crop_from_extension(
                            &target.pixels,
                            off_x,
                            off_y,
                            ew,
                            eh,
                            cs.width,
                            cs.height,
                        )
                    }
                    (None, None) => target.pixels.clone(),
                };
                Ok(LayerPatch::from_full_diff(&before_canvas, &after_px, cs.width, cs.height)
                    .map(|patch| HistoryAction::Transform {
                        layer_id: id,
                        patch,
                        ext_before,
                        ext_after,
                    }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn raster_target(idx: usize, is_paste: bool) -> TransformTarget {
        TransformTarget {
            layer_idx: idx,
            pixels: Vec::new(),
            src_dims: (1, 1),
            src_offset: None,
            orig_bounds: TransformRect::new(0.0, 0.0, 1.0, 1.0, 0.0),
            is_paste,
            history_before: None,
            orig_extension: None,
            kind: TargetKind::Raster,
        }
    }

    #[test]
    fn delete_without_transform_proceeds_without_cancel() {
        let transform = TransformState::new();
        let cancelled = Cell::new(false);

        let proceed = prepare_transform_for_delete(&transform, || cancelled.set(true));

        assert!(proceed, "delete should proceed");
        assert!(!cancelled.get(), "no transform: cancel must not run");
    }

    #[test]
    fn delete_during_transform_cancels_then_deletes() {
        let transform = TransformState::new();
        *transform.targets.borrow_mut() = vec![raster_target(2, false)];
        transform.rect.set(Some(TransformRect::new(8.0, 8.0, 16.0, 16.0, 0.0)));
        let cancelled = Cell::new(false);

        let proceed = prepare_transform_for_delete(&transform, || cancelled.set(true));

        assert!(cancelled.get(), "transform in progress: cancel must run");
        assert!(proceed, "the restored layer still gets deleted");
    }

    #[test]
    fn delete_during_paste_transform_skips_extra_delete() {
        let transform = TransformState::new();
        *transform.targets.borrow_mut() = vec![raster_target(0, true)];
        let cancelled = Cell::new(false);

        let proceed = prepare_transform_for_delete(&transform, || cancelled.set(true));

        assert!(cancelled.get(), "paste transform: cancel must run");
        assert!(!proceed, "paste cancel already removed the layer; don't delete again");
    }

    fn ext(offset_x: i32, offset_y: i32, w: u32, h: u32) -> LayerExtension {
        LayerExtension {
            offset_x,
            offset_y,
            width: w,
            height: h,
            pixels: Rc::new(vec![0u8; (w * h * 4) as usize]),
        }
    }

    fn one_pixel(w: u32, h: u32, px: u32, py: u32) -> Vec<u8> {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        buf[((py * w + px) * 4 + 3) as usize] = 255;
        buf
    }

    #[test]
    fn frame_union_spans_canvas_and_extensions() {
        let cs = Size::new(10, 10);
        assert_eq!(frame_union(cs, &[]), (0, 0, 10, 10), "no extension = canvas");
        assert_eq!(frame_union(cs, &[(-4, 8, 6, 6)]), (-4, 0, 14, 14));
    }

    #[test]
    fn frame_union_clamps_runaway_expansion() {
        let cs = Size::new(10, 10);
        let (ox, _oy, cw, _ch) = frame_union(cs, &[(-1_000_000, 0, 4, 4)]);
        assert_eq!(ox, -MAX_FRAME_MARGIN);
        assert_eq!(cw, (10 + MAX_FRAME_MARGIN) as u32);
    }

    #[test]
    fn frame_union_caps_total_dimension() {
        let cs = Size::new(10, 10);
        let (ox, _oy, cw, _ch) =
            frame_union(cs, &[(-1_000_000, 0, 4, 4), (1_000_000, 0, 4, 4)]);
        assert_eq!(cw, MAX_FRAME_DIM as u32, "total axis clamped to the cap");
        assert!(ox <= 0 && ox + cw as i32 >= 10);
    }

    #[test]
    fn place_into_frame_positions_canvas_and_extension() {
        let cs = Size::new(2, 2);
        let canvas_px = one_pixel(2, 2, 0, 0);
        let e = LayerExtension { pixels: Rc::new(one_pixel(2, 2, 0, 0)), ..ext(-2, 0, 2, 2) };
        let (ox, oy, cw, ch) = frame_union(cs, &[(e.offset_x, e.offset_y, e.width, e.height)]);
        assert_eq!((ox, oy, cw, ch), (-2, 0, 4, 2));
        let buf = place_into_frame(cs, &canvas_px, Some(&e), ox, oy, cw, ch);
        assert_eq!(buf[3], 255, "extension pixel at frame (0,0)");
        assert_eq!(buf[(2 * 4 + 3) as usize], 255, "canvas pixel at frame (2,0)");
    }

    #[test]
    fn source_extends_off_canvas_detects_overflow() {
        let cs = Size::new(4, 4);
        let inside = one_pixel(4, 4, 1, 1);
        assert!(!source_extends_off_canvas(&inside, 4, 4, 0, 0, cs));
        assert!(source_extends_off_canvas(&inside, 4, 4, -3, 0, cs));
    }
}
