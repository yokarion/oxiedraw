use std::cell::{Cell, RefCell};
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::color::{Color, ColorState};

// ---------------------------------------------------------------------------
// Crop types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CropOverlay {
    #[default]
    Thirds,
    Grid,
    Diagonal,
    Triangle,
    Golden,
    Spiral,
}

impl CropOverlay {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Thirds => "THIRDS",
            Self::Grid => "GRID",
            Self::Diagonal => "DIAGONAL",
            Self::Triangle => "TRIANGLE",
            Self::Golden => "GOLDEN",
            Self::Spiral => "SPIRAL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CropAspectRatio {
    #[default]
    Free,
    Square,
    FourThree,
    ThreeTwo,
    SixteenNine,
}

impl crate::enum_meta::EnumMeta for CropAspectRatio {
    const ALL: &'static [Self] = &[
        Self::Free,
        Self::Square,
        Self::FourThree,
        Self::ThreeTwo,
        Self::SixteenNine,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Free => "Free",
            Self::Square => "1 : 1",
            Self::FourThree => "4 : 3",
            Self::ThreeTwo => "3 : 2",
            Self::SixteenNine => "16 : 9",
        }
    }
}

impl CropAspectRatio {
    pub fn ratio(self) -> Option<f32> {
        match self {
            Self::Free => None,
            Self::Square => Some(1.0),
            Self::FourThree => Some(4.0 / 3.0),
            Self::ThreeTwo => Some(3.0 / 2.0),
            Self::SixteenNine => Some(16.0 / 9.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl std::fmt::Debug for CropRect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CropRect({},{} {}x{})", self.x, self.y, self.w, self.h)
    }
}

impl CropRect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            x: if self.w >= 0.0 {
                self.x
            } else {
                self.x + self.w
            },
            y: if self.h >= 0.0 {
                self.y
            } else {
                self.y + self.h
            },
            w: self.w.abs(),
            h: self.h.abs(),
        }
    }

    pub fn right(self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }
    pub const fn width_px(self) -> u32 {
        self.w.abs().round() as u32
    }
    pub const fn height_px(self) -> u32 {
        self.h.abs().round() as u32
    }
}

pub struct CropState {
    pub rect: Rc<Cell<Option<CropRect>>>,
    pub overlay: Rc<Cell<CropOverlay>>,
    pub snap_to_canvas: Rc<Cell<bool>>,
    pub aspect_ratio: Rc<Cell<CropAspectRatio>>,
    rect_changed: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
}

impl std::fmt::Debug for CropState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CropState")
            .field("rect", &self.rect.get())
            .field("overlay", &self.overlay.get())
            .field("snap_to_canvas", &self.snap_to_canvas.get())
            .finish_non_exhaustive()
    }
}

impl Clone for CropState {
    fn clone(&self) -> Self {
        Self {
            rect: Rc::clone(&self.rect),
            overlay: Rc::clone(&self.overlay),
            snap_to_canvas: Rc::clone(&self.snap_to_canvas),
            aspect_ratio: Rc::clone(&self.aspect_ratio),
            rect_changed: Rc::clone(&self.rect_changed),
        }
    }
}

impl CropState {
    pub fn new() -> Self {
        Self {
            rect: Rc::new(Cell::new(None)),
            overlay: Rc::new(Cell::new(CropOverlay::Thirds)),
            snap_to_canvas: Rc::new(Cell::new(true)),
            aspect_ratio: Rc::new(Cell::new(CropAspectRatio::Free)),
            rect_changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn notify_rect_changed(&self) {
        for cb in self.rect_changed.borrow().iter() {
            cb();
        }
    }

    pub fn connect_rect_changed(&self, cb: Box<dyn Fn()>) {
        self.rect_changed.borrow_mut().push(cb);
    }
}

impl Default for CropState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CropHandle {
    #[default]
    None,
    NewRect,
    Move,
    TopLeft,
    TopMid,
    TopRight,
    MidLeft,
    MidRight,
    BottomLeft,
    BottomMid,
    BottomRight,
}

// ---------------------------------------------------------------------------
// Transform types
// ---------------------------------------------------------------------------

pub use oxiedraw_utils::geometry::{TransformFilter, TransformRect};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransformHandle {
    #[default]
    None,
    Move,
    Rotate,
    TopLeft,
    TopMid,
    TopRight,
    MidLeft,
    MidRight,
    BottomLeft,
    BottomMid,
    BottomRight,
}

pub struct TransformState {
    pub rect: Rc<Cell<Option<TransformRect>>>,
    /// The bounding-box rect captured when the transform was activated.
    /// Used as the "source" rect for the pixel remap on apply.
    pub original_rect: Rc<Cell<Option<TransformRect>>>,
    pub filter: Rc<Cell<TransformFilter>>,
    /// Original pixels of the active layer captured when the tool was activated.
    /// For a normal transform this is canvas-sized; for a paste-via-transform it
    /// is the full source image (which may be larger or smaller than the canvas).
    pub original_pixels: Rc<RefCell<Option<Vec<u8>>>>,
    /// Index of the layer that was active when the transform started.
    pub original_layer_idx: Rc<Cell<Option<usize>>>,
    /// Pixel dimensions of `original_pixels`. `None` means canvas-sized (normal
    /// transform); `Some((w, h))` means a paste-via-transform with a different size.
    pub original_src_dims: Rc<Cell<Option<(u32, u32)>>>,
    /// Set to `true` before calling `set_active_tool(Transform)` from the paste
    /// path. The activation handler checks this flag and skips the normal
    /// "read layer + clear" initialisation, using the pre-loaded state instead.
    pub pre_seeded: Rc<Cell<bool>>,
    /// True when the transform was initiated by pasting an external image (a new
    /// transparent layer was added). Cancel must remove that layer instead of
    /// restoring pixels.
    pub is_paste: Rc<Cell<bool>>,
    /// When the transform was loaded from a `LayerExtension` (off-canvas data),
    /// this holds `(offset_x, offset_y)` in canvas coordinates so that cancel
    /// can reconstruct and re-store the extension.
    pub original_src_offset: Rc<Cell<Option<(i32, i32)>>>,
    /// Set when transforming a component instance: `(component_id, original
    /// placement-as-rect)`. The source is the component master, so apply
    /// re-renders crisply at the new rect and updates the layer's placement
    /// instead of baking the (downscaled) slot pixels.
    pub component: Rc<RefCell<Option<(String, TransformRect)>>>,
    /// Set when transforming a text layer: `(layer_id, original box-as-rect)`.
    /// The source is the text rendered in its local frame, so apply re-renders
    /// crisply at the new box and updates the layer's `Text` box geometry
    /// instead of baking rotated/scaled pixels.
    pub text: Rc<RefCell<Option<(String, TransformRect)>>>,
    changed: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
}

impl std::fmt::Debug for TransformState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransformState")
            .field("rect", &self.rect.get())
            .field("filter", &self.filter.get())
            .finish_non_exhaustive()
    }
}

impl Clone for TransformState {
    fn clone(&self) -> Self {
        Self {
            rect: Rc::clone(&self.rect),
            original_rect: Rc::clone(&self.original_rect),
            filter: Rc::clone(&self.filter),
            original_pixels: Rc::clone(&self.original_pixels),
            original_layer_idx: Rc::clone(&self.original_layer_idx),
            original_src_dims: Rc::clone(&self.original_src_dims),
            pre_seeded: Rc::clone(&self.pre_seeded),
            is_paste: Rc::clone(&self.is_paste),
            original_src_offset: Rc::clone(&self.original_src_offset),
            component: Rc::clone(&self.component),
            text: Rc::clone(&self.text),
            changed: Rc::clone(&self.changed),
        }
    }
}

impl TransformState {
    pub fn new() -> Self {
        Self {
            rect: Rc::new(Cell::new(None)),
            original_rect: Rc::new(Cell::new(None)),
            filter: Rc::new(Cell::new(TransformFilter::Bilinear)),
            original_pixels: Rc::new(RefCell::new(None)),
            original_layer_idx: Rc::new(Cell::new(None)),
            original_src_dims: Rc::new(Cell::new(None)),
            pre_seeded: Rc::new(Cell::new(false)),
            is_paste: Rc::new(Cell::new(false)),
            original_src_offset: Rc::new(Cell::new(None)),
            component: Rc::new(RefCell::new(None)),
            text: Rc::new(RefCell::new(None)),
            changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn notify_changed(&self) {
        for cb in self.changed.borrow().iter() {
            cb();
        }
    }

    pub fn connect_changed(&self, cb: Box<dyn Fn()>) {
        self.changed.borrow_mut().push(cb);
    }

    /// Clear all transform state (pixels, layer index, rects).
    pub fn clear(&self) {
        *self.original_pixels.borrow_mut() = None;
        self.original_layer_idx.set(None);
        self.rect.set(None);
        self.original_rect.set(None);
        self.original_src_dims.set(None);
        self.original_src_offset.set(None);
        self.pre_seeded.set(false);
        self.is_paste.set(false);
        *self.component.borrow_mut() = None;
        *self.text.borrow_mut() = None;
    }
}

impl Default for TransformState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Selection / Fill / Shape tools
// ---------------------------------------------------------------------------

/// Boolean op used when committing a marquee/lasso into the selection mask.
/// Driven by modifier keys: plain = Replace, Shift = Add, Alt = Subtract,
/// Shift+Alt = Intersect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionMode {
    #[default]
    Replace,
    Add,
    Subtract,
    Intersect,
}

/// Live rubber-band shape being dragged out by the user. Stored in
/// canvas-pixel coordinates. Replaced with a fresh shape on every drag
/// update; cleared when the drag commits or cancels.
#[derive(Debug, Clone)]
pub enum PendingMarquee {
    /// Bounding rect of an in-progress Square / Circle drag.
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        circle: bool,
    },
    /// In-progress lasso polyline.
    Lasso(Vec<oxiedraw_utils::geometry::Point>),
}

pub struct SelectionState {
    /// Active = the GPU mask has meaningful contents; clearable by Deselect.
    pub active: Rc<Cell<bool>>,
    /// Current boolean op applied to the next committed shape.
    pub mode: Rc<Cell<SelectionMode>>,
    /// In-flight rubber-band shape; not yet committed to the mask.
    pub pending: Rc<RefCell<Option<PendingMarquee>>>,
    /// Contour polylines for the marching-ants overlay. In *canvas*
    /// pixel coordinates; recomputed after every mask mutation.
    pub ants_contours: Rc<RefCell<Vec<Vec<oxiedraw_utils::geometry::Point>>>>,
    /// Layer index whose alpha channel produced the current selection
    /// (set when the user clicks a layer's thumbnail in the panel).
    /// `None` for selections drawn directly with the marquee/lasso. When
    /// `Some`, Transform activation lifts pixels from this layer rather
    /// than the active one - so clicking a non-active layer's preview
    /// then pressing Ctrl+T transforms the *clicked* layer's content
    /// without changing the active layer.
    pub source_layer: Rc<Cell<Option<usize>>>,
    /// Monotonic counter bumped whenever any selection state changes
    /// (mask, pending shape, contours). The UI listens for changes and
    /// invalidates the paintable.
    pub version: Rc<Cell<u64>>,
    changed: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
}

impl std::fmt::Debug for SelectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectionState")
            .field("active", &self.active.get())
            .field("mode", &self.mode.get())
            .field("version", &self.version.get())
            .finish_non_exhaustive()
    }
}

impl Clone for SelectionState {
    fn clone(&self) -> Self {
        Self {
            active: Rc::clone(&self.active),
            mode: Rc::clone(&self.mode),
            pending: Rc::clone(&self.pending),
            ants_contours: Rc::clone(&self.ants_contours),
            source_layer: Rc::clone(&self.source_layer),
            version: Rc::clone(&self.version),
            changed: Rc::clone(&self.changed),
        }
    }
}

impl SelectionState {
    pub fn new() -> Self {
        Self {
            active: Rc::new(Cell::new(false)),
            mode: Rc::new(Cell::new(SelectionMode::Replace)),
            pending: Rc::new(RefCell::new(None)),
            ants_contours: Rc::new(RefCell::new(Vec::new())),
            source_layer: Rc::new(Cell::new(None)),
            version: Rc::new(Cell::new(0)),
            changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn notify_changed(&self) {
        self.version.set(self.version.get().wrapping_add(1));
        for cb in self.changed.borrow().iter() {
            cb();
        }
    }

    pub fn connect_changed(&self, cb: Box<dyn Fn()>) {
        self.changed.borrow_mut().push(cb);
    }
}

impl Default for SelectionState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionTool {
    Square,
    Circle,
    Free,
}

impl SelectionTool {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Square => "Square Selection",
            Self::Circle => "Circle Selection",
            Self::Free => "Free Selection",
        }
    }

    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Square => "oxiedraw-selection-symbolic",
            Self::Circle => "oxiedraw-selection-circle-symbolic",
            Self::Free => "oxiedraw-selection-free-symbolic",
        }
    }
}

/// State for the Fill tool (currently only the bucket variant has settings).
///
/// `tolerance` is the maximum per-channel-style difference (squared-sum
/// across BGRA) between a candidate pixel and the seed pixel for the
/// bucket flood-fill to include it. Stored as 0..=255 where 0 = exact
/// match only and 255 = match anything within the connected region.
///
/// `sample_all_layers` makes the flood-fill seed/match against the
/// composited image of every visible layer instead of just the active
/// one; the fill itself is still painted into the active layer.
#[derive(Debug, Clone)]
pub struct FillState {
    pub tolerance: Rc<Cell<u8>>,
    pub sample_all_layers: Rc<Cell<bool>>,
}

impl FillState {
    pub fn new() -> Self {
        Self {
            tolerance: Rc::new(Cell::new(16)),
            sample_all_layers: Rc::new(Cell::new(false)),
        }
    }
}

impl Default for FillState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FillTool {
    Bucket,
    Gradient,
}

impl FillTool {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Bucket => "Bucket Fill",
            Self::Gradient => "Gradient",
        }
    }

    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Bucket => "oxiedraw-fill-symbolic",
            Self::Gradient => "oxiedraw-gradient-symbolic",
        }
    }
}

/// Settings for the shape tools. `filter` selects how shape edges are
/// rasterised: `Bilinear` produces anti-aliased edges, `NearestNeighbor`
/// produces hard 1-bit edges.
#[derive(Debug, Clone)]
pub struct ShapeState {
    pub filter: Rc<Cell<TransformFilter>>,
}

impl ShapeState {
    pub fn new() -> Self {
        Self {
            filter: Rc::new(Cell::new(TransformFilter::default())),
        }
    }
}

impl Default for ShapeState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShapeTool {
    Rectangle,
    Line,
    Circle,
    Triangle,
}

impl ShapeTool {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Rectangle => "Rectangle",
            Self::Line => "Line",
            Self::Circle => "Circle",
            Self::Triangle => "Triangle",
        }
    }

    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Rectangle => "oxiedraw-rectangle-symbolic",
            Self::Line => "oxiedraw-line-symbolic",
            Self::Circle => "oxiedraw-circle-symbolic",
            Self::Triangle => "oxiedraw-triangle-symbolic",
        }
    }

    /// Map to the renderer's primitive enum.
    #[must_use]
    pub const fn to_renderer_kind(self) -> crate::renderer::ShapeKind {
        use crate::renderer::ShapeKind;
        match self {
            Self::Rectangle => ShapeKind::Rectangle,
            Self::Circle => ShapeKind::Circle,
            Self::Triangle => ShapeKind::Triangle,
            Self::Line => ShapeKind::Line,
        }
    }
}

// ---------------------------------------------------------------------------
// Gradient tool
// ---------------------------------------------------------------------------

/// Number of texels in the baked gradient LUT. 256 gives one entry per
/// 8-bit level; the GPU sampler interpolates linearly between them.
pub const GRADIENT_LUT_SIZE: usize = 256;

/// Geometry of the gradient ramp along the drag axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum GradientType {
    #[default]
    Linear,
    Radial,
    Square,
}

impl GradientType {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Radial => "Radial",
            Self::Square => "Square",
        }
    }

    /// Map to the renderer's primitive enum.
    #[must_use]
    pub const fn to_renderer_kind(self) -> crate::renderer::GradientKind {
        use crate::renderer::GradientKind;
        match self {
            Self::Linear => GradientKind::Linear,
            Self::Radial => GradientKind::Radial,
            Self::Square => GradientKind::Square,
        }
    }
}

/// One gradient stop: a colour + opacity anchored at `position` (0..=1)
/// along the ramp. Serialised into the project as the document default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GradientStop {
    /// Position along the ramp in `0.0..=1.0`.
    pub position: f32,
    /// Stop opacity in `0.0..=1.0`.
    pub opacity: f32,
    pub color: Color,
}

/// An ordered set of gradient stops (at least two). Interpolation between
/// stops happens in sRGB channel space so the on-canvas result matches the
/// preview bar in the panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GradientSettings {
    pub stops: Vec<GradientStop>,
}

impl GradientSettings {
    /// Sort stops ascending by position. Called after any edit that could
    /// reorder them (drag past a neighbour, insert).
    fn sort(&mut self) {
        self.stops
            .sort_by(|a, b| a.position.partial_cmp(&b.position).unwrap_or(std::cmp::Ordering::Equal));
    }

    /// Interpolated `(color, opacity)` at `t` in sRGB channel space.
    /// Positions outside the stop range clamp to the nearest end stop.
    #[must_use]
    pub fn sample_srgb(&self, t: f32) -> (Color, f32) {
        let stops = &self.stops;
        if stops.is_empty() {
            return (Color::BLACK, 1.0);
        }
        let first = stops[0];
        if t <= first.position {
            return (first.color, first.opacity);
        }
        let last = stops[stops.len() - 1];
        if t >= last.position {
            return (last.color, last.opacity);
        }
        for pair in stops.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if t >= a.position && t <= b.position {
                let span = (b.position - a.position).max(f32::EPSILON);
                let f = ((t - a.position) / span).clamp(0.0, 1.0);
                let lerp_u8 = |x: u8, y: u8| {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let v = (f32::from(x) + (f32::from(y) - f32::from(x)) * f).round() as u8;
                    v
                };
                let color = Color::new(
                    lerp_u8(a.color.r, b.color.r),
                    lerp_u8(a.color.g, b.color.g),
                    lerp_u8(a.color.b, b.color.b),
                );
                let opacity = a.opacity + (b.opacity - a.opacity) * f;
                return (color, opacity);
            }
        }
        (last.color, last.opacity)
    }

    /// Bake the ramp into a premultiplied **linear** RGBA LUT for the GPU
    /// (`GRADIENT_LUT_SIZE` texels, 4 floats each). Colours are sRGB-lerped
    /// then converted to linear so the canvas result matches the UI preview.
    #[must_use]
    pub fn bake_lut(&self) -> Vec<f32> {
        let mut out = vec![0.0_f32; GRADIENT_LUT_SIZE * 4];
        for i in 0..GRADIENT_LUT_SIZE {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 / (GRADIENT_LUT_SIZE - 1) as f32;
            let (color, opacity) = self.sample_srgb(t);
            let lin = color.to_linear_rgb();
            let a = opacity.clamp(0.0, 1.0);
            out[i * 4] = lin[0] * a;
            out[i * 4 + 1] = lin[1] * a;
            out[i * 4 + 2] = lin[2] * a;
            out[i * 4 + 3] = a;
        }
        out
    }

    /// Insert a stop at `t`, interpolating its colour + opacity from the
    /// current ramp. Returns the index of the new stop after re-sorting.
    pub fn insert_stop(&mut self, t: f32) -> usize {
        let t = t.clamp(0.0, 1.0);
        let (color, opacity) = self.sample_srgb(t);
        self.stops.push(GradientStop { position: t, opacity, color });
        self.sort();
        self.stops
            .iter()
            .position(|s| (s.position - t).abs() < f32::EPSILON && s.color == color)
            .unwrap_or(0)
    }

    /// Move the stop at `idx` to `position`, re-sorting. Returns the stop's
    /// new index so a drag can keep tracking it after a reorder.
    pub fn move_stop(&mut self, idx: usize, position: f32) -> usize {
        if idx >= self.stops.len() {
            return idx;
        }
        let mut stop = self.stops.remove(idx);
        stop.position = position.clamp(0.0, 1.0);
        let pos = self.stops.partition_point(|s| s.position <= stop.position);
        self.stops.insert(pos, stop);
        pos
    }

    /// Remove the stop at `idx`. No-op if it would drop below two stops.
    pub fn remove_stop(&mut self, idx: usize) -> bool {
        if self.stops.len() <= 2 || idx >= self.stops.len() {
            return false;
        }
        self.stops.remove(idx);
        true
    }
}

/// Live state for the Gradient tool.
///
/// `settings` is `None` until the user edits a stop; while `None` the ramp is
/// derived from the primary/secondary colours (see [`Self::resolve`]). Once
/// concrete it is what gets persisted to the project as the document default.
pub struct GradientState {
    pub settings: Rc<RefCell<Option<GradientSettings>>>,
    pub gradient_type: Rc<Cell<GradientType>>,
    /// Index of the stop currently bound to the colour picker + panel fields.
    pub selected_stop: Rc<Cell<usize>>,
    changed: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
}

impl std::fmt::Debug for GradientState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GradientState")
            .field("gradient_type", &self.gradient_type.get())
            .field("selected_stop", &self.selected_stop.get())
            .finish_non_exhaustive()
    }
}

impl Clone for GradientState {
    fn clone(&self) -> Self {
        Self {
            settings: Rc::clone(&self.settings),
            gradient_type: Rc::clone(&self.gradient_type),
            selected_stop: Rc::clone(&self.selected_stop),
            changed: Rc::clone(&self.changed),
        }
    }
}

impl GradientState {
    pub fn new() -> Self {
        Self {
            settings: Rc::new(RefCell::new(None)),
            gradient_type: Rc::new(Cell::new(GradientType::default())),
            selected_stop: Rc::new(Cell::new(0)),
            changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Effective ramp: stored settings if present, else a two-stop ramp
    /// seeded from the primary (0%) and secondary (100%) colours.
    #[must_use]
    pub fn resolve(&self, colors: &ColorState) -> GradientSettings {
        if let Some(s) = self.settings.borrow().as_ref() {
            return s.clone();
        }
        GradientSettings {
            stops: vec![
                GradientStop { position: 0.0, opacity: 1.0, color: colors.primary.get() },
                GradientStop { position: 1.0, opacity: 1.0, color: colors.secondary.get() },
            ],
        }
    }

    /// Promote `None` settings to a concrete ramp (seeded from the colours)
    /// so subsequent edits persist. Returns nothing; edit via `settings`.
    pub fn ensure_owned(&self, colors: &ColorState) {
        if self.settings.borrow().is_none() {
            *self.settings.borrow_mut() = Some(self.resolve(colors));
        }
    }

    pub fn notify_changed(&self) {
        for cb in self.changed.borrow().iter() {
            cb();
        }
    }

    pub fn connect_changed(&self, cb: Box<dyn Fn()>) {
        self.changed.borrow_mut().push(cb);
    }
}

impl Default for GradientState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    Cursor,
    Selection(SelectionTool),
    Transform,
    Brush,
    ColorPicker,
    Fill(FillTool),
    Shapes(ShapeTool),
    Text,
    Crop,
}

impl Tool {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Cursor => "Cursor",
            Self::Selection(s) => s.display_name(),
            Self::Transform => "Transform",
            Self::Brush => "Brush",
            Self::ColorPicker => "Color Picker",
            Self::Fill(f) => f.display_name(),
            Self::Shapes(s) => s.display_name(),
            Self::Text => "Text",
            Self::Crop => "Crop",
        }
    }

    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Cursor => "oxiedraw-cursor-symbolic",
            Self::Selection(s) => s.icon_name(),
            Self::Transform => "oxiedraw-transform-symbolic",
            Self::Brush => "oxiedraw-brush-symbolic",
            Self::ColorPicker => "oxiedraw-colorpicker-symbolic",
            Self::Fill(f) => f.icon_name(),
            Self::Shapes(s) => s.icon_name(),
            Self::Text => "oxiedraw-text-symbolic",
            Self::Crop => "oxiedraw-crop-symbolic",
        }
    }
}

/// Currently selected tool.
///
/// Backed by `Rc<Cell<...>>` so toolbar click handlers can mutate without going
/// through the relm4 message loop, mirroring `ColorState`.
#[derive(Debug, Clone)]
pub struct ToolState {
    pub active: Rc<Cell<Tool>>,
    /// Brush eraser mode. When set, brush strokes remove coverage from the
    /// active layer instead of painting. The brush bar's toggle button and the
    /// eraser keybinding both drive this through the stateful `eraser-toggle`
    /// gio action, which keeps the button and this cell in sync.
    pub eraser: Rc<Cell<bool>>,
}

impl ToolState {
    pub fn new() -> Self {
        Self {
            active: Rc::new(Cell::new(Tool::Brush)),
            eraser: Rc::new(Cell::new(false)),
        }
    }
}

impl Default for ToolState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod gradient_tests {
    use super::*;

    fn two_stop() -> GradientSettings {
        GradientSettings {
            stops: vec![
                GradientStop { position: 0.0, opacity: 1.0, color: Color::BLACK },
                GradientStop { position: 1.0, opacity: 1.0, color: Color::WHITE },
            ],
        }
    }

    #[test]
    fn sample_clamps_and_interpolates() {
        let g = two_stop();
        assert_eq!(g.sample_srgb(-0.5).0, Color::BLACK);
        assert_eq!(g.sample_srgb(1.5).0, Color::WHITE);
        let (mid, _) = g.sample_srgb(0.5);
        assert_eq!(mid, Color::new(128, 128, 128));
    }

    #[test]
    fn lut_endpoints_are_premultiplied() {
        let mut g = two_stop();
        g.stops[1].opacity = 0.0; // white, fully transparent
        let lut = g.bake_lut();
        assert_eq!(lut.len(), GRADIENT_LUT_SIZE * 4);
        // First texel: opaque black -> all zero rgb, alpha 1.
        assert!((lut[3] - 1.0).abs() < 1e-4);
        // Last texel: transparent white -> premultiplied rgb collapses to 0.
        let last = (GRADIENT_LUT_SIZE - 1) * 4;
        assert!(lut[last] < 1e-4 && lut[last + 3] < 1e-4);
    }

    #[test]
    fn insert_keeps_sorted_and_returns_index() {
        let mut g = two_stop();
        let idx = g.insert_stop(0.5);
        assert_eq!(g.stops.len(), 3);
        assert_eq!(idx, 1);
        assert!(g.stops[0].position <= g.stops[1].position);
        assert!(g.stops[1].position <= g.stops[2].position);
    }

    #[test]
    fn remove_respects_two_stop_minimum() {
        let mut g = two_stop();
        assert!(!g.remove_stop(0));
        g.insert_stop(0.5);
        assert!(g.remove_stop(1));
        assert_eq!(g.stops.len(), 2);
    }

    #[test]
    fn resolve_defaults_to_primary_secondary() {
        let colors = ColorState::new();
        colors.primary.set(Color::new(10, 20, 30));
        colors.secondary.set(Color::new(40, 50, 60));
        let g = GradientState::new();
        let r = g.resolve(&colors);
        assert_eq!(r.stops.len(), 2);
        assert_eq!(r.stops[0].color, Color::new(10, 20, 30));
        assert_eq!(r.stops[1].color, Color::new(40, 50, 60));
    }
}
