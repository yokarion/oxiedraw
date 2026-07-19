//! Present-time colour-space conversion.
//!
//! Samples the canvas (premultiplied linear) and writes premultiplied-gamma
//! pixels into the display dmabuf, which is the form GSK composites correctly.
//! `present_convert.frag` has the reasoning.

use ash::{Device, vk};

use super::RendererError;
use super::pass::{
    FullscreenPass, nearest_clamp_sampler, pipeline_layout, replace_blend, sampler_set_layout,
};
use super::vulkan::RING_FRAMES;

const VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/present_convert.frag.spv"));

/// Fullscreen conversion pipeline, with one source descriptor set per ring
/// frame so a present can rebind its own slot without racing frames still in
/// flight (`submit_to_ring` fences the slot before we get here).
pub(super) struct PresentConvertPipeline {
    pub render_pass: vk::RenderPass,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub descriptor_pool: vk::DescriptorPool,
    pub src_sets: Vec<vk::DescriptorSet>,
    pub sampler: vk::Sampler,
}

impl PresentConvertPipeline {
    pub(super) fn new(device: &Device, display_format: vk::Format) -> Result<Self, RendererError> {
        let render_pass = create_render_pass(device, display_format)?;
        let descriptor_set_layout = sampler_set_layout(device, 1)?;
        let layout = pipeline_layout(device, descriptor_set_layout, 0)?;
        // Replace, not blend: the shader writes the final display pixel.
        let pipeline = FullscreenPass {
            vert_spv: VERT_SPV,
            frag_spv: FRAG_SPV,
            render_pass,
            layout,
            blend: replace_blend(),
        }
        .build(device)?;
        // 1:1 same-size pass, so no filtering happens; nearest + clamp is fine.
        let sampler = nearest_clamp_sampler(device)?;
        let (descriptor_pool, src_sets) =
            create_descriptor_sets(device, descriptor_set_layout, RING_FRAMES)?;
        Ok(Self {
            render_pass,
            layout,
            pipeline,
            descriptor_set_layout,
            descriptor_pool,
            src_sets,
            sampler,
        })
    }

    /// Point ring slot `slot`'s source set at `view` (in GENERAL layout).
    pub(super) fn bind_source(&self, device: &Device, slot: usize, view: vk::ImageView) {
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::GENERAL)
            .sampler(self.sampler)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(self.src_sets[slot])
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing this pipeline is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_render_pass(self.render_pass, None);
        }
    }
}

/// Single-attachment pass that discards the previous contents (the whole
/// buffer is redrawn each present) and leaves the image in GENERAL for the
/// dma-buf importer. A subpass dependency flushes the colour writes out to
/// `MEMORY_READ` so implicit dma-buf sync sees a coherent frame.
fn create_render_pass(device: &Device, format: vk::Format) -> Result<vk::RenderPass, RendererError> {
    let attachments = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::GENERAL)];
    let color_refs = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::GENERAL)];
    let subpasses = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs)];
    let dependencies = [
        // Entry: the prior use of this buffer (a previous frame's present, or the
        // importer's read) must finish before we overwrite it.
        vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::MEMORY_READ)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
        // Exit: flush the colour writes to MEMORY_READ for the dma-buf importer.
        vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::BOTTOM_OF_PIPE)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ),
    ];
    let info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&dependencies);
    Ok(unsafe { device.create_render_pass(&info, None)? })
}

fn create_descriptor_sets(
    device: &Device,
    layout: vk::DescriptorSetLayout,
    count: usize,
) -> Result<(vk::DescriptorPool, Vec<vk::DescriptorSet>), RendererError> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: count as u32,
    }];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(count as u32);
    let pool = unsafe { device.create_descriptor_pool(&pool_info, None)? };

    let layouts = vec![layout; count];
    let alloc_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&layouts);
    let sets = unsafe { device.allocate_descriptor_sets(&alloc_info)? };
    Ok((pool, sets))
}
