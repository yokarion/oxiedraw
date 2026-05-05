//! Pattern atlas for the `Textured` brush family.
//!
//! Backing image is a `R8G8B8A8_UNORM` 2D array with full mip chains.
//! Patterns are uploaded into one slice each; the slice index is pushed
//! as a 4-byte constant per draw so the textured fragment shader can
//! sample the right layer with `texture(sampler2DArray, vec3(uv, slice))`.

use ash::{Device, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator};

use super::RendererError;
use super::resources::Buffer;

/// Dimension of each atlas slice, in pixels. Patterns are resized to
/// this on upload (nearest-neighbour for now; bilinear later if needed).
pub(super) const ATLAS_SLICE_DIM: u32 = 512;

/// Number of slices reserved. Stage 4 only uses one (the debug chalk
/// pattern); the registry will fill more in later stages.
pub(super) const ATLAS_SLICE_COUNT: u32 = 16;

const ATLAS_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

pub(super) struct PatternAtlas {
    handle: vk::Image,
    view: vk::ImageView,
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    allocation: Option<Allocation>,
    mip_levels: u32,
    /// Next free slice index. Hand-managed counter, no fragmentation  - 
    /// patterns never get deleted in stage 4.
    next_slot: u32,
}

impl PatternAtlas {
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
    ) -> Result<Self, RendererError> {
        let mip_levels = mip_levels_for(ATLAS_SLICE_DIM);
        let extent = vk::Extent3D {
            width: ATLAS_SLICE_DIM,
            height: ATLAS_SLICE_DIM,
            depth: 1,
        };
        let create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(ATLAS_FORMAT)
            .extent(extent)
            .mip_levels(mip_levels)
            .array_layers(ATLAS_SLICE_COUNT)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let handle = unsafe { device.create_image(&create_info, None)? };
        let requirements = unsafe { device.get_image_memory_requirements(handle) };
        let allocation = allocator.allocate(&AllocationCreateDesc {
            name: "pattern-atlas",
            requirements,
            location: MemoryLocation::GpuOnly,
            linear: false,
            allocation_scheme: AllocationScheme::GpuAllocatorManaged,
        })?;
        unsafe { device.bind_image_memory(handle, allocation.memory(), allocation.offset())? };

        let view_info = vk::ImageViewCreateInfo::default()
            .image(handle)
            .view_type(vk::ImageViewType::TYPE_2D_ARRAY)
            .format(ATLAS_FORMAT)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: mip_levels,
                base_array_layer: 0,
                layer_count: ATLAS_SLICE_COUNT,
            });
        let view = unsafe { device.create_image_view(&view_info, None)? };

        let sampler = create_sampler(device, mip_levels)?;
        let descriptor_set_layout = create_descriptor_set_layout(device)?;
        let descriptor_pool = create_descriptor_pool(device)?;
        let descriptor_set = allocate_and_update_descriptor_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            view,
            sampler,
        )?;

        Ok(Self {
            handle,
            view,
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            descriptor_set,
            allocation: Some(allocation),
            mip_levels,
            next_slot: 0,
        })
    }

    pub(super) const fn descriptor_set_layout(&self) -> vk::DescriptorSetLayout {
        self.descriptor_set_layout
    }

    pub(super) const fn descriptor_set(&self) -> vk::DescriptorSet {
        self.descriptor_set
    }

    /// Allocate the next slice. Returns `None` when the atlas is full.
    pub(super) const fn allocate_slot(&mut self) -> Option<u32> {
        if self.next_slot >= ATLAS_SLICE_COUNT {
            return None;
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        Some(slot)
    }

    /// Reset the bump allocator so all slices become available again.
    /// Safe only when no in-flight stroke still references a slice  - 
    /// the caller is responsible for that ordering. Used by the
    /// preview renderer's dedicated canvas, which sees one stroke at
    /// a time and is the natural place to recycle.
    pub(super) const fn reset_slots(&mut self) {
        self.next_slot = 0;
    }

    /// One-shot helper: transition the whole image (all mips + slices)
    /// to `SHADER_READ_ONLY_OPTIMAL`. Called at startup so sampling the
    /// uninitialised atlas is defined behaviour.
    pub(super) fn cmd_prime_layout(&self, device: &Device, cmd: vk::CommandBuffer) {
        let barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.handle)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: self.mip_levels,
                base_array_layer: 0,
                layer_count: ATLAS_SLICE_COUNT,
            });
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
    }

    /// Records the copy + barriers for uploading `staging` (containing
    /// the full mip pyramid laid out contiguously) into `slot`.
    /// `mip_offsets` holds `(byte_offset, mip_width, mip_height)` for
    /// each mip level, level 0 first.
    pub(super) fn cmd_upload_slot(
        &self,
        device: &Device,
        cmd: vk::CommandBuffer,
        staging_buffer: vk::Buffer,
        slot: u32,
        mip_offsets: &[(u64, u32, u32)],
    ) {
        // Transition the slot's mips to TRANSFER_DST_OPTIMAL.
        let to_transfer = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.handle)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: self.mip_levels,
                base_array_layer: slot,
                layer_count: 1,
            });
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer],
            );
        }

        for (mip_level, &(offset, w, h)) in mip_offsets.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let mip_level_u32 = mip_level as u32;
            let region = vk::BufferImageCopy::default()
                .buffer_offset(offset)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: mip_level_u32,
                    base_array_layer: slot,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                });
            unsafe {
                device.cmd_copy_buffer_to_image(
                    cmd,
                    staging_buffer,
                    self.handle,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
            }
        }

        // Transition back to SHADER_READ_ONLY_OPTIMAL.
        let to_shader = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.handle)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: self.mip_levels,
                base_array_layer: slot,
                layer_count: 1,
            });
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_shader],
            );
        }
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing this atlas is in flight.
    pub(super) unsafe fn destroy(mut self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.handle, None);
        }
        if let Some(a) = self.allocation.take() {
            let _ = allocator.free(a);
        }
    }
}

/// `floor(log2(dim)) + 1`.
const fn mip_levels_for(dim: u32) -> u32 {
    let mut levels = 1;
    let mut d = dim;
    while d > 1 {
        d /= 2;
        levels += 1;
    }
    levels
}

fn create_sampler(device: &Device, mip_levels: u32) -> Result<vk::Sampler, RendererError> {
    #[allow(clippy::cast_precision_loss)]
    let max_lod = (mip_levels - 1) as f32;
    let info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::LINEAR)
        .min_filter(vk::Filter::LINEAR)
        .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
        .address_mode_u(vk::SamplerAddressMode::REPEAT)
        .address_mode_v(vk::SamplerAddressMode::REPEAT)
        .address_mode_w(vk::SamplerAddressMode::REPEAT)
        .min_lod(0.0)
        .max_lod(max_lod);
    Ok(unsafe { device.create_sampler(&info, None)? })
}

fn create_descriptor_set_layout(device: &Device) -> Result<vk::DescriptorSetLayout, RendererError> {
    let bindings = [vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    Ok(unsafe { device.create_descriptor_set_layout(&info, None)? })
}

fn create_descriptor_pool(device: &Device) -> Result<vk::DescriptorPool, RendererError> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: 1,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(1);
    Ok(unsafe { device.create_descriptor_pool(&info, None)? })
}

fn allocate_and_update_descriptor_set(
    device: &Device,
    pool: vk::DescriptorPool,
    layout: vk::DescriptorSetLayout,
    view: vk::ImageView,
    sampler: vk::Sampler,
) -> Result<vk::DescriptorSet, RendererError> {
    let layouts = [layout];
    let info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&layouts);
    let set = unsafe { device.allocate_descriptor_sets(&info)? }[0];

    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .sampler(sampler)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(&image_info)];
    unsafe { device.update_descriptor_sets(&writes, &[]) };

    Ok(set)
}

// ---------------------------------------------------------------------------
// CPU pattern preprocessing
// ---------------------------------------------------------------------------

/// Resize an arbitrary RGBA8 pattern to `ATLAS_SLICE_DIM` square via
/// nearest-neighbour sampling, then build a mip pyramid by box-filter
/// downsampling. Returns one contiguous buffer with mip levels packed
/// large->small, plus per-mip `(offset, width, height)` triples.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn build_mip_chain(src: &[u8], src_w: u32, src_h: u32) -> (Vec<u8>, Vec<(u64, u32, u32)>) {
    let level0 = resize_to_atlas(src, src_w, src_h);
    let mut mips: Vec<Vec<u8>> = vec![level0];
    let mut w = ATLAS_SLICE_DIM;
    let mut h = ATLAS_SLICE_DIM;
    while w > 1 || h > 1 {
        let new_w = (w / 2).max(1);
        let new_h = (h / 2).max(1);
        let prev = &mips[mips.len() - 1];
        let mut next = vec![0u8; (new_w * new_h * 4) as usize];
        for y in 0..new_h {
            for x in 0..new_w {
                let mut acc = [0u32; 4];
                let mut count = 0u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let sx = (x * 2 + dx).min(w.saturating_sub(1));
                        let sy = (y * 2 + dy).min(h.saturating_sub(1));
                        let i = ((sy * w + sx) * 4) as usize;
                        for c in 0..4 {
                            acc[c] += u32::from(prev[i + c]);
                        }
                        count += 1;
                    }
                }
                let dst = ((y * new_w + x) * 4) as usize;
                for c in 0..4 {
                    next[dst + c] = (acc[c] / count) as u8;
                }
            }
        }
        mips.push(next);
        w = new_w;
        h = new_h;
    }

    // Pack into one buffer, recording per-mip offsets.
    let total: usize = mips.iter().map(Vec::len).sum();
    let mut packed = Vec::with_capacity(total);
    let mut offsets = Vec::with_capacity(mips.len());
    let mut mw = ATLAS_SLICE_DIM;
    let mut mh = ATLAS_SLICE_DIM;
    for mip in &mips {
        offsets.push((packed.len() as u64, mw, mh));
        packed.extend_from_slice(mip);
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
    }
    (packed, offsets)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
fn resize_to_atlas(src: &[u8], src_w: u32, src_h: u32) -> Vec<u8> {
    let dim = ATLAS_SLICE_DIM;
    let mut out = vec![0u8; (dim * dim * 4) as usize];
    if src_w == 0 || src_h == 0 {
        return out;
    }
    let sx_step = src_w as f32 / dim as f32;
    let sy_step = src_h as f32 / dim as f32;
    for y in 0..dim {
        for x in 0..dim {
            let sx = ((x as f32) * sx_step) as u32;
            let sy = ((y as f32) * sy_step) as u32;
            let sx = sx.min(src_w - 1);
            let sy = sy.min(src_h - 1);
            let i_src = ((sy * src_w + sx) * 4) as usize;
            let i_dst = ((y * dim + x) * 4) as usize;
            out[i_dst..i_dst + 4].copy_from_slice(&src[i_src..i_src + 4]);
        }
    }
    out
}

/// Helper: a host-visible staging buffer big enough for the packed mip
/// pyramid. Lifetime is per-upload; the renderer destroys it after the
/// submit fences.
pub(super) fn create_pattern_staging(
    device: &Device,
    allocator: &mut Allocator,
    size: u64,
) -> Result<Buffer, RendererError> {
    Buffer::new(
        device,
        allocator,
        "pattern-staging",
        size,
        vk::BufferUsageFlags::TRANSFER_SRC,
        MemoryLocation::CpuToGpu,
    )
}
