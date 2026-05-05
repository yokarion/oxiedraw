//! GPU resources for the layer filters (HSV, invert, box blur, sharpen).
//!
//! A filter turns one layer image into a fully-filtered copy by running a
//! short chain of fullscreen passes that ping-pong between two canvas-sized
//! BGRA8 scratch images. Point filters take one pass; blur takes two
//! (separable); sharpen takes three (blur x2 + combine); every filter ends
//! with a mask-mix pass that blends the result back over the original
//! according to the selection mask. The final scratch image is then either
//! composited into the preview (live preview) or copied into the layer
//! image (apply).
//!
//! All filter pipelines share one descriptor-set layout with three sampler
//! bindings (primary source, secondary source, selection mask) and one
//! 16-byte push constant. A single descriptor set is rewritten before each
//! pass - safe because every pass is its own fence-waited submission.

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::resources::Image;

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const HSV_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/filter_hsv.frag.spv"));
const INVERT_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/filter_invert.frag.spv"));
const BOX_BLUR_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/filter_box_blur.frag.spv"));
const SHARPEN_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/filter_sharpen.frag.spv"));
const MASK_MIX_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/filter_mask_mix.frag.spv"));

/// 16 bytes: one `vec4` of parameters (meaning depends on the bound pipeline).
pub(super) const FILTER_PUSH_BYTES: u32 = 16;

/// Which scratch image currently holds a pass's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scratch {
    A,
    B,
}

pub(super) struct FilterResources {
    /// Ping-pong render targets, canvas-sized BGRA8 (same format as layers).
    pub scratch_a: Image,
    pub scratch_b: Image,
    pub framebuffer_a: vk::Framebuffer,
    pub framebuffer_b: vk::Framebuffer,

    pub sampler: vk::Sampler,

    /// Three-sampler input layout shared by every filter pipeline.
    pub input_set_layout: vk::DescriptorSetLayout,
    pub input_pool: vk::DescriptorPool,
    /// Reusable input set, rewritten via [`Self::write_input`] before each pass.
    pub input_set: vk::DescriptorSet,

    /// Layout shared by every filter pipeline (input set + 16-byte push).
    pub pipeline_layout: vk::PipelineLayout,

    pub hsv: vk::Pipeline,
    pub invert: vk::Pipeline,
    pub box_blur: vk::Pipeline,
    pub sharpen: vk::Pipeline,
    pub mask_mix: vk::Pipeline,

    /// Descriptor sets that bind each scratch image for the layer-composite
    /// pipeline, so a finished scratch image can be drawn into the preview.
    pub composite_pool: vk::DescriptorPool,
    pub composite_set_a: vk::DescriptorSet,
    pub composite_set_b: vk::DescriptorSet,
}

impl FilterResources {
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        canvas_extent: vk::Extent2D,
        canvas_render_pass: vk::RenderPass,
        layer_composite_set_layout: vk::DescriptorSetLayout,
        layer_composite_sampler: vk::Sampler,
    ) -> Result<Self, RendererError> {
        let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED;
        let scratch_a = Image::new_2d(
            device,
            allocator,
            "filter-scratch-a",
            super::vulkan::CANVAS_FORMAT,
            canvas_extent,
            usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let scratch_b = Image::new_2d(
            device,
            allocator,
            "filter-scratch-b",
            super::vulkan::CANVAS_FORMAT,
            canvas_extent,
            usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let framebuffer_a =
            create_framebuffer(device, canvas_render_pass, canvas_extent, scratch_a.view)?;
        let framebuffer_b =
            create_framebuffer(device, canvas_render_pass, canvas_extent, scratch_b.view)?;

        let sampler = create_sampler(device)?;
        let input_set_layout = create_input_set_layout(device)?;
        let input_pool = create_input_pool(device)?;
        let input_set = {
            let layouts = [input_set_layout];
            let info = vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(input_pool)
                .set_layouts(&layouts);
            let sets = unsafe { device.allocate_descriptor_sets(&info)? };
            sets[0]
        };

        let pipeline_layout = create_pipeline_layout(device, input_set_layout)?;
        let hsv = create_pipeline(device, pipeline_layout, canvas_render_pass, HSV_FRAG_SPV)?;
        let invert = create_pipeline(device, pipeline_layout, canvas_render_pass, INVERT_FRAG_SPV)?;
        let box_blur =
            create_pipeline(device, pipeline_layout, canvas_render_pass, BOX_BLUR_FRAG_SPV)?;
        let sharpen =
            create_pipeline(device, pipeline_layout, canvas_render_pass, SHARPEN_FRAG_SPV)?;
        let mask_mix =
            create_pipeline(device, pipeline_layout, canvas_render_pass, MASK_MIX_FRAG_SPV)?;

        let composite_pool = create_composite_pool(device)?;
        let composite_set_a = allocate_composite_set(
            device,
            composite_pool,
            layer_composite_set_layout,
            scratch_a.view,
            layer_composite_sampler,
        )?;
        let composite_set_b = allocate_composite_set(
            device,
            composite_pool,
            layer_composite_set_layout,
            scratch_b.view,
            layer_composite_sampler,
        )?;

        Ok(Self {
            scratch_a,
            scratch_b,
            framebuffer_a,
            framebuffer_b,
            sampler,
            input_set_layout,
            input_pool,
            input_set,
            pipeline_layout,
            hsv,
            invert,
            box_blur,
            sharpen,
            mask_mix,
            composite_pool,
            composite_set_a,
            composite_set_b,
        })
    }

    /// Image handle for a scratch slot (for barriers / copies).
    pub(super) const fn scratch_handle(&self, which: Scratch) -> vk::Image {
        match which {
            Scratch::A => self.scratch_a.handle,
            Scratch::B => self.scratch_b.handle,
        }
    }

    /// Framebuffer for rendering into a scratch slot.
    pub(super) const fn framebuffer(&self, which: Scratch) -> vk::Framebuffer {
        match which {
            Scratch::A => self.framebuffer_a,
            Scratch::B => self.framebuffer_b,
        }
    }

    /// Layer-composite descriptor set that binds a scratch slot.
    pub(super) const fn composite_set(&self, which: Scratch) -> vk::DescriptorSet {
        match which {
            Scratch::A => self.composite_set_a,
            Scratch::B => self.composite_set_b,
        }
    }

    /// Rewrite the three input bindings of the reusable filter set. Pass the
    /// same view for bindings that the active pipeline does not read.
    pub(super) fn write_input(
        &self,
        device: &Device,
        primary: vk::ImageView,
        secondary: vk::ImageView,
        mask: vk::ImageView,
    ) {
        let infos = [
            vk::DescriptorImageInfo::default()
                .image_view(primary)
                .image_layout(vk::ImageLayout::GENERAL)
                .sampler(self.sampler),
            vk::DescriptorImageInfo::default()
                .image_view(secondary)
                .image_layout(vk::ImageLayout::GENERAL)
                .sampler(self.sampler),
            vk::DescriptorImageInfo::default()
                .image_view(mask)
                .image_layout(vk::ImageLayout::GENERAL)
                .sampler(self.sampler),
        ];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.input_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[0..1]),
            vk::WriteDescriptorSet::default()
                .dst_set(self.input_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[1..2]),
            vk::WriteDescriptorSet::default()
                .dst_set(self.input_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[2..3]),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_pipeline(self.hsv, None);
            device.destroy_pipeline(self.invert, None);
            device.destroy_pipeline(self.box_blur, None);
            device.destroy_pipeline(self.sharpen, None);
            device.destroy_pipeline(self.mask_mix, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.composite_pool, None);
            device.destroy_descriptor_pool(self.input_pool, None);
            device.destroy_descriptor_set_layout(self.input_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_framebuffer(self.framebuffer_a, None);
            device.destroy_framebuffer(self.framebuffer_b, None);
            self.scratch_a.destroy(device, allocator);
            self.scratch_b.destroy(device, allocator);
        }
    }
}

fn create_input_set_layout(device: &Device) -> Result<vk::DescriptorSetLayout, RendererError> {
    let bindings = [
        binding(0),
        binding(1),
        binding(2),
    ];
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    Ok(unsafe { device.create_descriptor_set_layout(&info, None)? })
}

fn binding(slot: u32) -> vk::DescriptorSetLayoutBinding<'static> {
    vk::DescriptorSetLayoutBinding::default()
        .binding(slot)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
}

fn create_input_pool(device: &Device) -> Result<vk::DescriptorPool, RendererError> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: 3,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(1);
    Ok(unsafe { device.create_descriptor_pool(&info, None)? })
}

fn create_composite_pool(device: &Device) -> Result<vk::DescriptorPool, RendererError> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: 2,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(2);
    Ok(unsafe { device.create_descriptor_pool(&info, None)? })
}

fn allocate_composite_set(
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
        .image_layout(vk::ImageLayout::GENERAL)
        .sampler(sampler)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(&image_info)];
    unsafe { device.update_descriptor_sets(&writes, &[]) };
    Ok(set)
}

fn create_pipeline_layout(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
) -> Result<vk::PipelineLayout, RendererError> {
    let set_layouts = [set_layout];
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(FILTER_PUSH_BYTES)];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(&push_ranges);
    Ok(unsafe { device.create_pipeline_layout(&info, None)? })
}

fn create_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
    frag_spv: &[u8],
) -> Result<vk::Pipeline, RendererError> {
    let vert = shader_module(device, COMPOSITE_VERT_SPV)?;
    let frag = shader_module(device, frag_spv)?;

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag)
            .name(entry),
    ];

    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    // Replace, not blend: a filter pass fully overwrites its target.
    let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(false)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);

    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);
    let infos = [pipeline_info];
    let pipelines =
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &infos, None) }
            .map_err(|(_, e)| RendererError::Vulkan(e))?;
    unsafe {
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
    }
    Ok(pipelines[0])
}

fn create_sampler(device: &Device) -> Result<vk::Sampler, RendererError> {
    // NEAREST so box-blur taps hit exact texels and the mask is read crisply.
    let info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::NEAREST)
        .min_filter(vk::Filter::NEAREST)
        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
        .min_lod(0.0)
        .max_lod(0.0);
    Ok(unsafe { device.create_sampler(&info, None)? })
}

fn create_framebuffer(
    device: &Device,
    render_pass: vk::RenderPass,
    extent: vk::Extent2D,
    view: vk::ImageView,
) -> Result<vk::Framebuffer, RendererError> {
    let views = [view];
    let info = vk::FramebufferCreateInfo::default()
        .render_pass(render_pass)
        .attachments(&views)
        .width(extent.width)
        .height(extent.height)
        .layers(1);
    Ok(unsafe { device.create_framebuffer(&info, None)? })
}

fn shader_module(device: &Device, bytes: &[u8]) -> Result<vk::ShaderModule, RendererError> {
    assert!(bytes.len().is_multiple_of(4), "SPIR-V is 4-byte-aligned");
    let mut code = vec![0u32; bytes.len() / 4];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        code[i] = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}
