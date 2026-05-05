use crate::brush_engine::{BrushFamily, Dab, PaintTarget};
use crate::renderer::{DabFamily, DabInstance, RendererError, VulkanRenderer};

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
}

impl<'a> StrokeStamp<'a> {
    pub(super) const fn new(renderer: &'a mut VulkanRenderer) -> Self {
        Self {
            renderer,
            error: None,
            scratch: Vec::new(),
            family: DabFamily::SoftRound,
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
        match family {
            BrushFamily::SoftRound => self.family = DabFamily::SoftRound,
            BrushFamily::Pixel => self.family = DabFamily::Pixel,
            BrushFamily::Textured(data) => match self.renderer.upload_pattern(data) {
                Ok(slice) => self.family = DabFamily::Textured { slice },
                Err(e) => self.error = Some(e),
            },
        }
    }

    fn paint_dabs(&mut self, dabs: &[Dab]) {
        if self.error.is_some() || dabs.is_empty() {
            return;
        }
        self.scratch.clear();
        self.scratch.reserve(dabs.len());
        for dab in dabs {
            self.scratch.push(DabInstance::from_dab(dab));
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
}

impl<'a> BatchStamp<'a> {
    pub(super) const fn new(renderer: &'a mut VulkanRenderer) -> Self {
        Self {
            renderer,
            error: None,
            family: DabFamily::SoftRound,
            instances: Vec::new(),
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
        let new_family = match family {
            BrushFamily::SoftRound => DabFamily::SoftRound,
            BrushFamily::Pixel => DabFamily::Pixel,
            BrushFamily::Textured(data) => match self.renderer.upload_pattern(data) {
                Ok(slice) => DabFamily::Textured { slice },
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            },
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
            self.instances.push(DabInstance::from_dab(dab));
        }
    }
}

const fn same_family(a: DabFamily, b: DabFamily) -> bool {
    matches!(
        (a, b),
        (DabFamily::SoftRound, DabFamily::SoftRound) | (DabFamily::Pixel, DabFamily::Pixel)
    ) || matches!(
        (a, b),
        (DabFamily::Textured { slice: s1 }, DabFamily::Textured { slice: s2 }) if s1 == s2
    )
}
