mod brush;
pub mod builtins;
pub mod cursor;
mod dynamics;
pub mod format;
mod input;
mod pattern;
mod preset;
pub mod preview_renderer;
mod registry;
mod stamp;

pub use brush::{BrushPresetId, Dab, PaintTarget, StrokeContext, StrokeRenderer};
pub use cursor::{BrushCursor, compute_brush_cursor};
pub use dynamics::{Curve, DynSource, Dynamics, Mapping, SpawnInput, evaluate, make_spawn_input};
pub use input::InputSample;
pub use pattern::PatternData;
pub use preset::{BrushFamily, BrushPreset, TexturingMode, TipShape};
pub use registry::BrushRegistry;
pub use stamp::start_stroke;

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::Path;
use std::rc::Rc;

use crate::color::Color;

#[derive(Clone)]
pub struct BrushEngine {
    /// Loaded brush presets. Mutable so user brushes loaded from disk
    /// can be appended at runtime.
    pub brushes: Rc<RefCell<Vec<BrushPreset>>>,
    pub active: Rc<Cell<BrushPresetId>>,
    pub size: Rc<Cell<f32>>,
    pub opacity: Rc<Cell<f32>>,
    in_flight_stroke: Rc<RefCell<Option<Box<dyn StrokeRenderer>>>>,
    /// Color + opacity of the in-flight stroke. Exposed so the canvas
    /// can composite the stroke-buffer mask at the right tint without
    /// reaching into the renderer's private state.
    stroke_ctx: Rc<Cell<Option<StrokeContext>>>,
    /// Monotonic counter for assigning fresh `BrushPresetId`s as user
    /// brushes are loaded.
    next_id: Rc<Cell<u32>>,
    /// Listeners invoked after the brush *list* changes (add/clear/
    /// reload). Stored as `(id, callback)` so individual listeners can
    /// be detached - important for transient windows (Manage Brushes)
    /// that must not leak their closures across reopens.
    brushes_listeners: Rc<RefCell<Vec<(BrushListenerId, Rc<dyn Fn()>)>>>,
    next_listener_id: Rc<Cell<u32>>,
    /// While `> 0`, `notify_brushes_changed` records the request and
    /// returns without firing. The trailing `flush_pending_notify`
    /// call fires once when the depth drops back to zero. Used by
    /// `reload_from_dir` so listeners see a single transition instead
    /// of `clear -> add -> add -> ...` flicker.
    suppress_notify_depth: Rc<Cell<u32>>,
    notify_pending: Rc<Cell<bool>>,
}

/// Opaque handle returned by `connect_brushes_changed` so the caller
/// can detach the listener via `disconnect_brushes_changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrushListenerId(u32);

impl fmt::Debug for BrushEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrushEngine")
            .field("brushes", &self.brushes.borrow().len())
            .field("active", &self.active.get())
            .field("size", &self.size.get())
            .field("opacity", &self.opacity.get())
            .field("in_stroke", &self.in_flight_stroke.borrow().is_some())
            .field("stroke_ctx", &self.stroke_ctx.get())
            .finish_non_exhaustive()
    }
}

impl BrushEngine {
    pub fn new() -> Self {
        let engine = Self {
            brushes: Rc::new(RefCell::new(Vec::new())),
            active: Rc::new(Cell::new(BrushPresetId(0))),
            size: Rc::new(Cell::new(12.0)),
            opacity: Rc::new(Cell::new(1.0)),
            in_flight_stroke: Rc::new(RefCell::new(None)),
            stroke_ctx: Rc::new(Cell::new(None)),
            next_id: Rc::new(Cell::new(0)),
            brushes_listeners: Rc::new(RefCell::new(Vec::new())),
            next_listener_id: Rc::new(Cell::new(0)),
            suppress_notify_depth: Rc::new(Cell::new(0)),
            notify_pending: Rc::new(Cell::new(false)),
        };
        // Built-in factories; `add_brush` reassigns the id from the counter
        // so the literal `BrushPresetId(0)` passed here is just a placeholder.
        let placeholder = BrushPresetId(0);
        let default_id = engine.add_brush(BrushPreset::default_round(placeholder));
        engine.add_brush(BrushPreset::ink_pen(placeholder));
        engine.add_brush(BrushPreset::pixel(placeholder));
        engine.add_brush(BrushPreset::scatter_dot(placeholder));
        engine.add_brush(BrushPreset::speed_brush(placeholder));
        engine.add_brush(BrushPreset::chalk(placeholder));
        engine.add_brush(BrushPreset::comics(placeholder));
        engine.add_brush(BrushPreset::real_brush(placeholder));
        engine.active.set(default_id);
        let default_preset_size = engine
            .brushes
            .borrow()
            .iter()
            .find(|p| p.id == default_id)
            .map_or(12.0, |p| p.default_size);
        engine.size.set(default_preset_size);
        engine
    }

    /// Append a preset to the registry, assigning it a fresh id.
    pub fn add_brush(&self, mut preset: BrushPreset) -> BrushPresetId {
        let id = BrushPresetId(self.next_id.get());
        self.next_id.set(self.next_id.get().wrapping_add(1));
        preset.id = id;
        self.brushes.borrow_mut().push(preset);
        self.notify_brushes_changed();
        id
    }

    /// Drop every loaded brush. Active id is not changed here - callers
    /// reload + re-select before the engine is queried again.
    pub fn clear_brushes(&self) {
        self.brushes.borrow_mut().clear();
        self.notify_brushes_changed();
    }

    /// Register a callback invoked whenever the brush *list* changes.
    /// Returns a handle the caller passes to
    /// [`Self::disconnect_brushes_changed`] to detach the listener.
    /// Listeners run in registration order; panics in a listener are
    /// the caller's problem.
    pub fn connect_brushes_changed(&self, listener: Rc<dyn Fn()>) -> BrushListenerId {
        let id = BrushListenerId(self.next_listener_id.get());
        self.next_listener_id
            .set(self.next_listener_id.get().wrapping_add(1));
        self.brushes_listeners.borrow_mut().push((id, listener));
        id
    }

    /// Remove a previously-registered listener by its handle. No-op if
    /// the handle is unknown.
    pub fn disconnect_brushes_changed(&self, id: BrushListenerId) {
        self.brushes_listeners.borrow_mut().retain(|(i, _)| *i != id);
    }

    fn notify_brushes_changed(&self) {
        if self.suppress_notify_depth.get() > 0 {
            self.notify_pending.set(true);
            return;
        }
        // Snapshot the listener list so callbacks that mutate it (add,
        // remove, re-enter) don't deadlock or skip entries.
        let snapshot: Vec<Rc<dyn Fn()>> = self
            .brushes_listeners
            .borrow()
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for cb in snapshot {
            cb();
        }
    }

    /// Run `f` with notifications coalesced - any `notify_brushes_changed`
    /// triggered inside is held until the outer guard drops, then fires
    /// once if at least one notification was suppressed.
    fn batch_notify<R>(&self, f: impl FnOnce() -> R) -> R {
        self.suppress_notify_depth
            .set(self.suppress_notify_depth.get() + 1);
        let r = f();
        let depth_after = self.suppress_notify_depth.get() - 1;
        self.suppress_notify_depth.set(depth_after);
        if depth_after == 0 && self.notify_pending.replace(false) {
            self.notify_brushes_changed();
        }
        r
    }

    /// Load every `*.oxiebrush` archive in `dir` and append the parsed
    /// presets. Returns the number successfully loaded; errors per
    /// file are logged via `tracing::warn` so a single bad archive
    /// doesn't break the user's whole library.
    pub fn load_brushes_from_dir(&self, dir: &Path) -> usize {
        let (packages, errors) = BrushRegistry::scan_dir(dir);
        for (path, err) in errors {
            tracing::warn!(?path, %err, "failed to load brush");
        }
        let mut loaded = 0;
        for (path, pkg) in packages {
            let placeholder = BrushPresetId(0);
            match pkg.into_preset(placeholder, Some(path)) {
                Ok(preset) => {
                    self.add_brush(preset);
                    loaded += 1;
                }
                Err(e) => tracing::warn!(%e, "brush package missing pattern"),
            }
        }
        loaded
    }

    /// For every loaded brush that lacks a cached `preview.png` and has
    /// a known `source_path`, render one via the real engine and
    /// rewrite the archive on disk. Returns the number of previews
    /// generated. Failures are logged and skipped - they're not fatal.
    ///
    /// Re-saves trigger the file watcher, which will fire one extra
    /// reload pass; the second pass sees the cached preview and is a
    /// no-op on this code path.
    pub fn backfill_missing_previews(&self) -> usize {
        // Snapshot work to do *outside* the borrow so save can run
        // without re-entering the borrow.
        let targets: Vec<(BrushPresetId, BrushPreset)> = self
            .brushes
            .borrow()
            .iter()
            .filter(|p| p.preview.is_none() && p.source_path.is_some())
            .map(|p| (p.id, p.clone()))
            .collect();
        let mut filled = 0;
        for (id, mut preset) in targets {
            let png = match preview_renderer::render_preview_png(&preset) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(brush = %preset.name, %e, "preview render failed");
                    continue;
                }
            };
            preset.preview = Some(png.clone());
            // Persist to disk first - if this fails we don't want the
            // in-memory copy out of sync with the archive.
            let Some(path) = preset.source_path.as_ref() else {
                continue;
            };
            if let Err(e) = format::save(&preset, path) {
                tracing::warn!(brush = %preset.name, %e, "preview save failed");
                continue;
            }
            // Now update the in-memory brush so the picker/editor see
            // the cached preview without waiting for the watcher.
            if let Some(b) = self.brushes.borrow_mut().iter_mut().find(|p| p.id == id) {
                b.preview = Some(png);
            }
            filled += 1;
        }
        if filled > 0 {
            tracing::debug!(filled, "backfilled missing brush previews");
        }
        filled
    }

    /// Wipe the brush list and reload from `dir`, preserving the active
    /// selection by *name* across the swap. Used by the file watcher
    /// when a `.oxiebrush` change is detected. No-op while a stroke is
    /// in flight (don't yank the active brush mid-paint) or when the
    /// scan saw a transient I/O error (file mid-write - the watcher
    /// will fire again when the write completes).
    pub fn reload_from_dir(&self, dir: &Path) {
        if self.is_drawing() {
            return;
        }
        let (packages, errors) = BrushRegistry::scan_dir(dir);
        for (path, err) in &errors {
            tracing::warn!(?path, %err, "failed to load brush");
        }
        if errors.iter().any(|(_, e)| matches!(e, format::BrushError::Io(_))) {
            tracing::debug!(
                ?dir,
                "transient I/O during reload - keeping current brushes, will retry on next event"
            );
            return;
        }
        let prev_name = self
            .brushes
            .borrow()
            .iter()
            .find(|p| p.id == self.active.get())
            .map(|p| p.name.clone());
        // Coalesce the clear + add xN notifications into a single
        // final fire so listeners see one transition with the final
        // brush list, not N+1 intermediate states.
        self.batch_notify(|| {
            self.clear_brushes();
            for (path, pkg) in packages {
                match pkg.into_preset(BrushPresetId(0), Some(path)) {
                    Ok(preset) => {
                        self.add_brush(preset);
                    }
                    Err(e) => tracing::warn!(%e, "brush package missing pattern"),
                }
            }
            if self.brushes.borrow().is_empty() {
                self.add_brush(builtins::fallback_brush());
            }
            let brushes = self.brushes.borrow();
            let target_id = prev_name
                .as_ref()
                .and_then(|name| brushes.iter().find(|p| &p.name == name))
                .or_else(|| brushes.first())
                .map(|p| p.id);
            if let Some(id) = target_id {
                self.active.set(id);
            }
        });
    }

    /// Clone of the currently selected brush. Presets are small POD  - 
    /// cloning avoids holding a `Ref` across stroke construction.
    pub fn active_brush(&self) -> BrushPreset {
        let id = self.active.get();
        self.brushes
            .borrow()
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .expect("active id must exist in brushes")
    }

    pub fn begin_stroke(&self, sample: InputSample, color: Color, target: &mut dyn PaintTarget) {
        let preset = self.active_brush();
        let ctx = StrokeContext {
            preset: preset.id,
            color,
            size: self.size.get(),
            opacity: self.opacity.get(),
        };
        let mut renderer = start_stroke(&preset, ctx);
        renderer.push(sample, target);
        *self.in_flight_stroke.borrow_mut() = Some(renderer);
        self.stroke_ctx.set(Some(ctx));
    }

    pub fn push_sample(&self, sample: InputSample, target: &mut dyn PaintTarget) {
        if let Some(renderer) = self.in_flight_stroke.borrow_mut().as_mut() {
            renderer.push(sample, target);
        }
    }

    pub fn end_stroke(&self, target: &mut dyn PaintTarget) {
        if let Some(mut renderer) = self.in_flight_stroke.borrow_mut().take() {
            renderer.end(target);
        }
        self.stroke_ctx.set(None);
    }

    pub fn current_stroke_context(&self) -> Option<StrokeContext> {
        self.stroke_ctx.get()
    }

    /// Render the unsettled tail of the in-flight stroke. Called from
    /// the canvas `draw_func` every frame so the cursor's latest position
    /// is visible without waiting for spline neighbours.
    pub fn preview(&self, target: &mut dyn PaintTarget) {
        if let Some(renderer) = self.in_flight_stroke.borrow().as_ref() {
            renderer.preview(target);
        }
    }

    pub fn is_drawing(&self) -> bool {
        self.in_flight_stroke.borrow().is_some()
    }
}

impl Default for BrushEngine {
    fn default() -> Self {
        Self::new()
    }
}
