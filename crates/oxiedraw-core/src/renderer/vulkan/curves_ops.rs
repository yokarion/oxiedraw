//! Uploads to the curves LUT atlas and the row lookup the Curves pass needs.

use ash::vk;

use super::super::RendererError;
use super::super::curves_lut::ROW_BYTES;
use super::gradient_ops::f32_to_f16_bits;
use super::VulkanRenderer;
use crate::curves::{CurveSet, LUT_SIZE};
use crate::effects::EffectKind;

impl VulkanRenderer {
    /// Must run before recording any pass that reads these sets.
    pub(super) fn prepare_curve_rows(&mut self, sets: &[CurveSet]) -> Result<(), RendererError> {
        if sets.is_empty() {
            return Ok(());
        }
        let rows = &mut self.curve_atlas.rows;
        rows.begin_round();
        let mut uploads: Vec<(u32, &CurveSet)> = Vec::new();
        for curves in sets {
            match rows.claim(curves) {
                Some((row, true)) => uploads.push((row, curves)),
                Some((_, false)) => {}
                None => tracing::warn!("curves LUT atlas is full, a curve is skipped this frame"),
            }
        }
        if uploads.is_empty() {
            return Ok(());
        }
        let result = self.upload_curve_rows(&uploads);
        if result.is_err() {
            for &(row, _) in &uploads {
                self.curve_atlas.rows.release(row);
            }
        }
        result
    }

    pub(super) fn curves_push(&self, curves: &CurveSet) -> [f32; 4] {
        self.curve_atlas
            .rows
            .find(curves)
            .map_or([0.0; 4], |row| [row as f32, 1.0, 0.0, 0.0])
    }

    pub(super) fn curve_lut(&self) -> (vk::ImageView, vk::Image) {
        (self.curve_atlas.image.view, self.curve_atlas.image.handle)
    }

    pub(super) fn adjustment_curve_sets(
        &self,
        indices: impl IntoIterator<Item = usize>,
    ) -> Vec<CurveSet> {
        indices
            .into_iter()
            .filter_map(|idx| self.layer_stack.slots.get(idx)?.adjustment.as_ref())
            .flat_map(|data| data.effects.iter())
            .filter(|effect| effect.is_active())
            .filter_map(|effect| match effect.kind {
                EffectKind::Curves { curves } => Some(curves),
                _ => None,
            })
            .collect()
    }

    fn upload_curve_rows(&mut self, uploads: &[(u32, &CurveSet)]) -> Result<(), RendererError> {
        let staging = self
            .curve_atlas
            .staging
            .mapped_mut()
            .ok_or(RendererError::StagingNotMapped)?;
        for (slot, (_, curves)) in uploads.iter().enumerate() {
            let bytes = &mut staging[slot * ROW_BYTES..(slot + 1) * ROW_BYTES];
            for (texel, value) in bytes.chunks_exact_mut(2).zip(curves.bake_lut()) {
                texel.copy_from_slice(&f32_to_f16_bits(value).to_le_bytes());
            }
        }
        let regions: Vec<vk::BufferImageCopy> = uploads
            .iter()
            .enumerate()
            .map(|(slot, &(row, _))| {
                vk::BufferImageCopy::default()
                    .buffer_offset((slot * ROW_BYTES) as vk::DeviceSize)
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_offset(vk::Offset3D {
                        x: 0,
                        y: row as i32,
                        z: 0,
                    })
                    .image_extent(vk::Extent3D {
                        width: LUT_SIZE as u32,
                        height: 1,
                        depth: 1,
                    })
            })
            .collect();
        let buffer = self.curve_atlas.staging.handle;
        let image = self.curve_atlas.image.handle;
        self.record_and_submit(|this| {
            this.barrier(
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            unsafe {
                this.device.cmd_copy_buffer_to_image(
                    this.command_buffer,
                    buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &regions,
                );
            }
            this.barrier(
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })
    }
}
