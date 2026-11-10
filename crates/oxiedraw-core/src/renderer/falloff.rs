//! Soft-round softness curve, baked into a uniform table for the dab shaders.

use ash::{Device, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::resources::Buffer;
use crate::brush_engine::Curve;

pub(super) const LUT_LEN: usize = 1024;
const LUT_BYTES: usize = LUT_LEN * 4;

pub(super) struct FalloffLut {
    buffer: Buffer,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    baked: Option<Curve>,
}

impl FalloffLut {
    pub(super) fn new(device: &Device, allocator: &mut Allocator) -> Result<Self, RendererError> {
        let buffer = Buffer::new(
            device,
            allocator,
            "falloff-lut",
            LUT_BYTES as vk::DeviceSize,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            MemoryLocation::CpuToGpu,
        )?;
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let set_layout = unsafe { device.create_descriptor_set_layout(&layout_info, None)? };
        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::UNIFORM_BUFFER,
            descriptor_count: 1,
        }];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&sizes)
            .max_sets(1);
        let pool = unsafe { device.create_descriptor_pool(&pool_info, None)? };
        let layouts = [set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let set = unsafe { device.allocate_descriptor_sets(&alloc_info)? }[0];
        let buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(buffer.handle)
            .offset(0)
            .range(LUT_BYTES as vk::DeviceSize)];
        let write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .buffer_info(&buffer_info);
        unsafe { device.update_descriptor_sets(&[write], &[]) };
        Ok(Self {
            buffer,
            set_layout,
            pool,
            set,
            baked: None,
        })
    }

    pub(super) const fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }

    pub(super) const fn set(&self) -> vk::DescriptorSet {
        self.set
    }

    pub(super) fn is_stale(&self, curve: &Curve) -> bool {
        self.baked.as_ref() != Some(curve)
    }

    /// No submitted work may still read the old table.
    pub(super) fn upload(&mut self, curve: &Curve) -> Result<(), RendererError> {
        let dst = self
            .buffer
            .mapped_mut()
            .ok_or(RendererError::StagingNotMapped)?;
        #[allow(clippy::cast_precision_loss)]
        let last = (LUT_LEN - 1) as f32;
        for (i, chunk) in dst[..LUT_BYTES].chunks_exact_mut(4).enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let v = curve.sample(i as f32 / last);
            chunk.copy_from_slice(&v.to_ne_bytes());
        }
        self.baked = Some(curve.clone());
        Ok(())
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_descriptor_pool(self.pool, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            self.buffer.destroy(device, allocator);
        }
    }
}
