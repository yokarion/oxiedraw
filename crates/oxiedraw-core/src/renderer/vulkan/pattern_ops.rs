//! Pattern atlas upload: resize -> mip pyramid -> GPU copy.

use std::rc::Rc;

use crate::brush_engine::PatternData;
use crate::renderer::pattern_atlas::{build_mip_chain, create_pattern_staging};

use super::super::RendererError;
use super::super::resources::Buffer;
use super::VulkanRenderer;

impl VulkanRenderer {
    /// Forget every previously-uploaded pattern slot. Subsequent
    /// `upload_pattern` calls re-upload from scratch. Intended for the
    /// dedicated preview renderer canvas - calling this on the main
    /// canvas mid-document would orphan whatever atlas indices already
    /// landed on disk dabs.
    pub fn clear_pattern_atlas(&mut self) {
        self.pattern_cache.clear();
        self.pattern_atlas.reset_slots();
    }

    /// Upload a pattern into the atlas and return its slice index.
    ///
    /// Cached by `Rc::as_ptr` identity - uploading the same
    /// `Rc<PatternData>` twice is a no-op that returns the original
    /// slice. Returns `RendererError::PatternAtlasFull` when no slots
    /// remain.
    pub fn upload_pattern(&mut self, data: &Rc<PatternData>) -> Result<u32, RendererError> {
        let key = Rc::as_ptr(data) as usize;
        if let Some(&slot) = self.pattern_cache.get(&key) {
            return Ok(slot);
        }
        let slot = self
            .pattern_atlas
            .allocate_slot()
            .ok_or(RendererError::PatternAtlasFull)?;

        let (packed, mip_offsets) = build_mip_chain(&data.rgba, data.width, data.height);
        let mut staging = create_pattern_staging(&self.device, &mut self.allocator, packed.len() as u64)?;
        let dst = staging.mapped_mut().ok_or(RendererError::StagingNotMapped)?;
        dst[..packed.len()].copy_from_slice(&packed);

        let staging_handle = staging.handle;
        self.record_and_submit(|this| {
            this.pattern_atlas.cmd_upload_slot(
                &this.device,
                this.command_buffer,
                staging_handle,
                slot,
                &mip_offsets,
            );
            Ok(())
        })?;

        // Submit completed (fence-waited); safe to free staging.
        unsafe {
            destroy_buffer(staging, &self.device, &mut self.allocator);
        }
        self.pattern_cache.insert(key, slot);
        Ok(slot)
    }
}

/// Helper so callers don't have to import the `Buffer::destroy` unsafe
/// fn directly (it's `pub(super)` on `resources`).
unsafe fn destroy_buffer(
    buffer: Buffer,
    device: &ash::Device,
    allocator: &mut gpu_allocator::vulkan::Allocator,
) {
    unsafe {
        buffer.destroy(device, allocator);
    }
}

