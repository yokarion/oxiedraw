//! Pixel patches and capture helpers for history entries.
//!
//! A [`LayerPatch`] is a tight AABB of changed pixels with before/after
//! BGRA8 buffers. Helpers in this module read a layer's pixels from the
//! GPU and crop them to the patch bounds.

use serde::{Deserialize, Serialize};

use crate::canvas::Canvas;
use crate::renderer::RendererError;

/// Axis-aligned bounding box in canvas pixels (integer coords).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PatchBounds {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl PatchBounds {
    pub const fn area(&self) -> usize {
        (self.w as usize) * (self.h as usize)
    }

    /// Full canvas bounds.
    pub const fn full(canvas_w: u32, canvas_h: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            w: canvas_w,
            h: canvas_h,
        }
    }
}

/// Before+after pixel snapshot of a rectangular region of a layer.
///
/// `before` and `after` are BGRA8 row-major, each `bounds.w * bounds.h * 4`
/// bytes. Undo writes `before` back into `bounds`; redo writes `after`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerPatch {
    pub bounds: PatchBounds,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

impl LayerPatch {
    /// Build a patch by diffing `before_full` against `after_full` (both
    /// canvas-sized BGRA8 buffers). Returns `None` if the buffers are
    /// identical (no patch needed).
    ///
    /// Bounds are the tight AABB of differing pixels.
    pub fn from_full_diff(
        before_full: &[u8],
        after_full: &[u8],
        canvas_w: u32,
        canvas_h: u32,
    ) -> Option<Self> {
        debug_assert_eq!(before_full.len(), after_full.len());
        debug_assert_eq!(
            before_full.len(),
            (canvas_w as usize) * (canvas_h as usize) * 4
        );

        let bounds = diff_bounds(before_full, after_full, canvas_w, canvas_h)?;
        let before = crop_region(before_full, canvas_w, &bounds);
        let after = crop_region(after_full, canvas_w, &bounds);
        Some(Self {
            bounds,
            canvas_w,
            canvas_h,
            before,
            after,
        })
    }

    /// Build a patch from explicit bounds and the canvas-sized before/after
    /// buffers. Used when the caller already knows the dirty rect (e.g.
    /// transform AABB).
    pub fn from_bounded(
        before_full: &[u8],
        after_full: &[u8],
        canvas_w: u32,
        canvas_h: u32,
        bounds: PatchBounds,
    ) -> Self {
        let before = crop_region(before_full, canvas_w, &bounds);
        let after = crop_region(after_full, canvas_w, &bounds);
        Self {
            bounds,
            canvas_w,
            canvas_h,
            before,
            after,
        }
    }

    /// Build a patch from before/after buffers that both cover `region`
    /// (each `region.w * region.h * 4` BGRA8, row-major, tightly packed).
    /// Diffs *within* the region to find the tight AABB; returns `None` if
    /// the region is unchanged. Used when the caller already read back only
    /// the stroke's dirty region instead of the whole canvas.
    pub fn from_region_diff(
        before_region: &[u8],
        after_region: &[u8],
        region: PatchBounds,
        canvas_w: u32,
        canvas_h: u32,
    ) -> Option<Self> {
        debug_assert_eq!(before_region.len(), after_region.len());
        debug_assert_eq!(before_region.len(), region.area() * 4);

        let sub = diff_bounds(before_region, after_region, region.w, region.h)?;
        let bounds = PatchBounds {
            x: region.x + sub.x,
            y: region.y + sub.y,
            w: sub.w,
            h: sub.h,
        };
        let before = crop_region(before_region, region.w, &sub);
        let after = crop_region(after_region, region.w, &sub);
        Some(Self {
            bounds,
            canvas_w,
            canvas_h,
            before,
            after,
        })
    }

    /// Crop a canvas-sized buffer down to `bounds`. Build-up strokes keep
    /// a full pristine snapshot from the stroke's start; this extracts the
    /// dirty region from it so [`Self::from_region_diff`] can be used.
    #[must_use]
    pub fn crop_canvas_region(full: &[u8], canvas_w: u32, bounds: PatchBounds) -> Vec<u8> {
        crop_region(full, canvas_w, &bounds)
    }

    /// Apply this patch to a layer in the given direction.
    pub fn apply(
        &self,
        canvas: &mut Canvas,
        layer_idx: usize,
        direction: super::action::Direction,
    ) -> Result<(), RendererError> {
        use super::action::Direction;
        let src = match direction {
            Direction::Forward => &self.after,
            Direction::Backward => &self.before,
        };
        // Read the current full layer, splat the patch into the bounds region,
        // write back. We can't write a sub-region directly through the public
        // Canvas API, so we round-trip through CPU.
        let mut full = canvas.read_layer(layer_idx)?;
        if full.len() == (self.canvas_w as usize) * (self.canvas_h as usize) * 4 {
            splat_region(&mut full, self.canvas_w, &self.bounds, src);
            canvas.restore_layer(layer_idx, &full)?;
        } else {
            tracing::warn!(
                expected = (self.canvas_w as usize) * (self.canvas_h as usize) * 4,
                actual = full.len(),
                "patch canvas size mismatch - skipping apply"
            );
        }
        Ok(())
    }
}

/// Compute the tight AABB of pixels that differ between two BGRA8 buffers.
/// Returns `None` if they are identical.
fn diff_bounds(a: &[u8], b: &[u8], w: u32, h: u32) -> Option<PatchBounds> {
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if a[i..i + 4] != b[i..i + 4] {
                if found {
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
                } else {
                    min_x = x;
                    max_x = x;
                    min_y = y;
                    max_y = y;
                    found = true;
                }
            }
        }
    }
    if !found {
        return None;
    }
    Some(PatchBounds {
        x: min_x,
        y: min_y,
        w: max_x - min_x + 1,
        h: max_y - min_y + 1,
    })
}

/// Copy a rectangular region out of a canvas-sized buffer.
fn crop_region(full: &[u8], canvas_w: u32, bounds: &PatchBounds) -> Vec<u8> {
    let mut out = vec![0u8; (bounds.w as usize) * (bounds.h as usize) * 4];
    for row in 0..bounds.h {
        let src_off = (((bounds.y + row) * canvas_w + bounds.x) * 4) as usize;
        let dst_off = (row * bounds.w * 4) as usize;
        let len = (bounds.w * 4) as usize;
        out[dst_off..dst_off + len].copy_from_slice(&full[src_off..src_off + len]);
    }
    out
}

/// Write a small region buffer back into the matching slice of a
/// canvas-sized buffer.
fn splat_region(full: &mut [u8], canvas_w: u32, bounds: &PatchBounds, region: &[u8]) {
    for row in 0..bounds.h {
        let dst_off = (((bounds.y + row) * canvas_w + bounds.x) * 4) as usize;
        let src_off = (row * bounds.w * 4) as usize;
        let len = (bounds.w * 4) as usize;
        full[dst_off..dst_off + len].copy_from_slice(&region[src_off..src_off + len]);
    }
}
