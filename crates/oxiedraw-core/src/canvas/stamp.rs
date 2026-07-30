use crate::brush_engine::{BrushFamily, Dab, PaintTarget};
use crate::guides::Symmetry;
use crate::renderer::{DabFamily, DabInstance, NO_SLICE, RendererError, SmudgeDab, VulkanRenderer};

/// Expand one brush dab into its instance plus every symmetry copy, appending
/// to `out`. Reflected copies reuse the dab's shape with a mirrored rotation;
/// asymmetric tips mirror approximately (like Procreate).
fn expand_dab(dab: &Dab, symmetry: Option<&Symmetry>, out: &mut Vec<DabInstance>) {
    let base = DabInstance::from_dab(dab);
    out.push(base);
    if let Some(sym) = symmetry {
        for el in &sym.elements {
            let (p, rotation, _flip) = el.apply(sym.origin, dab.center, dab.rotation);
            let mut copy = base;
            copy.center = [p.x, p.y];
            copy.rotation = rotation;
            out.push(copy);
        }
    }
}

/// `PaintTarget` adapter that funnels brush dabs into the Vulkan stroke
/// buffer via `stamp_mask`.
///
/// On `set_family`, translates `BrushFamily` into the renderer-side
/// `DabFamily`. For `Textured`, this is where we resolve the pattern
/// `Rc<PatternData>` to an atlas slice (uploading on first sight).
///
/// `paint_dabs` returns nothing per the trait, so any GPU error during
/// stamping is buffered here and surfaced when the caller calls
/// [`StrokeStamp::into_result`]. Once an error is set the adapter is
/// "poisoned" - subsequent `paint_dabs` calls become no-ops so we don't
/// pile up follow-on failures from the same bad state.
pub(super) struct StrokeStamp<'a> {
    renderer: &'a mut VulkanRenderer,
    error: Option<RendererError>,
    scratch: Vec<DabInstance>,
    family: DabFamily,
    symmetry: Option<Symmetry>,
}

impl<'a> StrokeStamp<'a> {
    pub(super) const fn new(renderer: &'a mut VulkanRenderer, symmetry: Option<Symmetry>) -> Self {
        Self {
            renderer,
            error: None,
            scratch: Vec::new(),
            family: DabFamily::SoftRound,
            symmetry,
        }
    }

    pub(super) fn into_result(self) -> Result<(), RendererError> {
        self.error.map_or(Ok(()), Err)
    }
}

impl PaintTarget for StrokeStamp<'_> {
    fn set_family(&mut self, family: &BrushFamily) {
        if self.error.is_some() {
            return;
        }
        match resolve_family(self.renderer, family) {
            Ok(f) => self.family = f,
            Err(e) => self.error = Some(e),
        }
    }

    fn paint_dabs(&mut self, dabs: &[Dab]) {
        if self.error.is_some() || dabs.is_empty() {
            return;
        }
        self.scratch.clear();
        self.scratch.reserve(dabs.len());
        for dab in dabs {
            expand_dab(dab, self.symmetry.as_ref(), &mut self.scratch);
        }
        if let Err(e) = self.renderer.stamp_mask(self.family, &self.scratch) {
            self.error = Some(e);
        }
    }
}

/// `PaintTarget` that accumulates dab instances WITHOUT submitting, so a
/// single combined submit can stamp + preview + present (the per-event
/// drag path). A family switch with pending instances is flushed via
/// `stamp_mask` (rare - a single brush keeps one family for a stroke); the
/// final accumulated batch is handed back to the caller to stamp.
pub(super) struct BatchStamp<'a> {
    renderer: &'a mut VulkanRenderer,
    error: Option<RendererError>,
    family: DabFamily,
    instances: Vec<DabInstance>,
    symmetry: Option<Symmetry>,
}

impl<'a> BatchStamp<'a> {
    pub(super) const fn new(renderer: &'a mut VulkanRenderer, symmetry: Option<Symmetry>) -> Self {
        Self {
            renderer,
            error: None,
            family: DabFamily::SoftRound,
            instances: Vec::new(),
            symmetry,
        }
    }

    /// The accumulated `(family, instances)` to stamp in the combined
    /// submit, or the first error seen.
    pub(super) fn into_result(self) -> Result<(DabFamily, Vec<DabInstance>), RendererError> {
        match self.error {
            Some(e) => Err(e),
            None => Ok((self.family, self.instances)),
        }
    }
}

impl PaintTarget for BatchStamp<'_> {
    fn set_family(&mut self, family: &BrushFamily) {
        if self.error.is_some() {
            return;
        }
        let new_family = match resolve_family(self.renderer, family) {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        // A family change with pending instances can't share one draw, so
        // flush the pending batch now (its own submit) and start fresh.
        if !self.instances.is_empty() && !same_family(self.family, new_family) {
            if let Err(e) = self.renderer.stamp_mask(self.family, &self.instances) {
                self.error = Some(e);
            }
            self.instances.clear();
        }
        self.family = new_family;
    }

    fn paint_dabs(&mut self, dabs: &[Dab]) {
        if self.error.is_some() || dabs.is_empty() {
            return;
        }
        self.instances.reserve(dabs.len());
        for dab in dabs {
            expand_dab(dab, self.symmetry.as_ref(), &mut self.instances);
        }
    }
}

/// `PaintTarget` for colour-smudge brushes. Each dab is painted straight into
/// the active layer via the GPU smudge path (no stroke buffer). `prev_center`
/// carries across `paint_dabs` calls so each dab's drag vector is relative to
/// the previous one; the caller seeds it from and reads it back into canvas
/// state so it persists across the whole stroke.
pub(super) struct SmudgeStamp<'a> {
    renderer: &'a mut VulkanRenderer,
    layer_idx: usize,
    paint_linear: [f32; 4],
    opacity: f32,
    prev_center: Option<[f32; 2]>,
    error: Option<RendererError>,
    scratch: Vec<SmudgeDab>,
}

impl<'a> SmudgeStamp<'a> {
    pub(super) const fn new(
        renderer: &'a mut VulkanRenderer,
        layer_idx: usize,
        paint_linear: [f32; 4],
        opacity: f32,
        prev_center: Option<[f32; 2]>,
    ) -> Self {
        Self {
            renderer,
            layer_idx,
            paint_linear,
            opacity,
            prev_center,
            error: None,
            scratch: Vec::new(),
        }
    }

    /// Returns the error (if any) plus the updated previous-centre so the
    /// caller can persist it for the next `paint_dabs` batch of this stroke.
    pub(super) fn into_result(self) -> (Result<(), RendererError>, Option<[f32; 2]>) {
        (self.error.map_or(Ok(()), Err), self.prev_center)
    }
}

/// Convert a brush `Dab` into a `SmudgeDab`, computing the drag vector against
/// (and advancing) the running previous centre. Shared by both smudge adapters.
fn push_smudge_dab(out: &mut Vec<SmudgeDab>, prev: &mut Option<[f32; 2]>, dab: &Dab) {
    let center = [dab.center.x, dab.center.y];
    let p = prev.unwrap_or(center);
    out.push(SmudgeDab {
        center,
        delta: [center[0] - p[0], center[1] - p[1]],
        radius: dab.radius,
        hardness: dab.hardness,
        smudge_rate: dab.smudge_rate,
        color_rate: dab.color_rate,
    });
    *prev = Some(center);
}

impl PaintTarget for SmudgeStamp<'_> {
    // The tip is always a round mask; the smudge family carries no pattern.
    fn set_family(&mut self, _family: &BrushFamily) {}

    fn paint_dabs(&mut self, dabs: &[Dab]) {
        if self.error.is_some() || dabs.is_empty() {
            return;
        }
        self.scratch.clear();
        self.scratch.reserve(dabs.len());
        for dab in dabs {
            push_smudge_dab(&mut self.scratch, &mut self.prev_center, dab);
        }
        if let Err(e) =
            self.renderer
                .smudge_dabs(self.layer_idx, self.paint_linear, self.opacity, &self.scratch)
        {
            self.error = Some(e);
        }
    }
}

/// `PaintTarget` that accumulates smudge dabs WITHOUT submitting, so the whole
/// motion event (dabs + recomposite + present) can go in one async submit -
/// matching the normal brush's hot path. Tracks `prev_center` across
/// `paint_dabs` calls for the drag vectors; the caller persists it.
pub(super) struct SmudgeBatchStamp {
    prev_center: Option<[f32; 2]>,
    dabs: Vec<SmudgeDab>,
}

impl SmudgeBatchStamp {
    pub(super) const fn new(prev_center: Option<[f32; 2]>) -> Self {
        Self {
            prev_center,
            dabs: Vec::new(),
        }
    }

    /// The accumulated dabs plus the updated previous-centre.
    pub(super) fn into_dabs(self) -> (Vec<SmudgeDab>, Option<[f32; 2]>) {
        (self.dabs, self.prev_center)
    }
}

impl PaintTarget for SmudgeBatchStamp {
    fn set_family(&mut self, _family: &BrushFamily) {}

    fn paint_dabs(&mut self, dabs: &[Dab]) {
        self.dabs.reserve(dabs.len());
        for dab in dabs {
            push_smudge_dab(&mut self.dabs, &mut self.prev_center, dab);
        }
    }
}

/// Resolve a brush-engine `BrushFamily` (which carries pattern data) into
/// the renderer `DabFamily` with atlas slices uploaded/cached. Shared by
/// both `PaintTarget` adapters so the two-slice image-tip logic lives once.
fn resolve_family(
    renderer: &mut VulkanRenderer,
    family: &BrushFamily,
) -> Result<DabFamily, RendererError> {
    match family {
        // Smudge is painted by the dedicated GPU path, not `stamp_mask`; it
        // never reaches this resolver in practice (canvas routes smudge brushes
        // to `SmudgeStamp`), but map it to a round mask to stay exhaustive.
        BrushFamily::SoftRound | BrushFamily::Smudge => Ok(DabFamily::SoftRound),
        BrushFamily::Pixel => Ok(DabFamily::Pixel),
        BrushFamily::Textured(grain) => {
            let grain_slice = renderer.upload_pattern(grain)?;
            Ok(DabFamily::Textured {
                grain_slice,
                tip_slice: NO_SLICE,
            })
        }
        BrushFamily::ImageTip { tip, grain } => {
            let tip_slice = renderer.upload_pattern(tip)?;
            let grain_slice = match grain {
                Some(g) => renderer.upload_pattern(g)?,
                None => NO_SLICE,
            };
            Ok(DabFamily::Textured {
                grain_slice,
                tip_slice,
            })
        }
    }
}

const fn same_family(a: DabFamily, b: DabFamily) -> bool {
    matches!(
        (a, b),
        (DabFamily::SoftRound, DabFamily::SoftRound) | (DabFamily::Pixel, DabFamily::Pixel)
    ) || matches!(
        (a, b),
        (
            DabFamily::Textured { grain_slice: g1, tip_slice: t1 },
            DabFamily::Textured { grain_slice: g2, tip_slice: t2 },
        ) if g1 == g2 && t1 == t2
    )
}
