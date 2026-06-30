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
const JFA_SEED_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/jfa_seed.frag.spv"));
const JFA_FLOOD_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/jfa_flood.frag.spv"));
const JFA_RESOLVE_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/jfa_resolve.frag.spv"));
const MASK_MIX_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/filter_mask_mix.frag.spv"));

/// 16 bytes: one `vec4` of parameters (meaning depends on the bound pipeline).
pub(super) const FILTER_PUSH_BYTES: u32 = 16;
/// Size of the input-set ring. Caps how many filter passes the batched
/// adjustment preview can record into a single submit before falling back to
/// the per-submit path.
pub(super) const INPUT_RING: usize = 16;
/// Stroke resolve push: 3x vec4 (color, params, texel). See `jfa_resolve.frag`.
pub(super) const STROKE_PUSH_BYTES: u32 = 48;

/// Which scratch image currently holds a pass's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scratch {
    A,
    B,
}

impl Scratch {
    /// The opposite slot - the natural destination when ping-ponging.
    pub(super) const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// Which jump-flood coordinate buffer is the current source while ping-ponging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JfaSlot {
    A,
    B,
}

impl JfaSlot {
    pub(super) const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
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
    /// Ring of input sets. The submit-per-pass filters only ever need one (they
    /// fence between passes, so the set can be rewritten), but the batched
    /// adjustment preview records several passes into one command buffer and so
    /// needs a distinct set per pass. `write_input` targets a chosen set.
    pub input_sets: Vec<vk::DescriptorSet>,

    /// Layout shared by every filter pipeline (input set + 16-byte push).
    pub pipeline_layout: vk::PipelineLayout,

    pub hsv: vk::Pipeline,
    pub invert: vk::Pipeline,
    pub box_blur: vk::Pipeline,
    pub sharpen: vk::Pipeline,
    pub mask_mix: vk::Pipeline,

    /// Adjustment-layer stroke via jump flooding. `jfa_seed` and `jfa_flood`
    /// render into the `coord_*` ping-pong buffers (16-bit float, RG = offset to
    /// nearest inside pixel, BA = nearest outside) through `jfa_render_pass`;
    /// `jfa_resolve` reads the converged field and writes the coloured band into
    /// `Scratch::A` through the canvas render pass. Resolve needs the wider
    /// 48-byte push, so it reuses `stroke_layout`.
    pub stroke_layout: vk::PipelineLayout,
    pub jfa_seed: vk::Pipeline,
    pub jfa_flood: vk::Pipeline,
    pub jfa_resolve: vk::Pipeline,
    pub jfa_render_pass: vk::RenderPass,
    pub coord_a: Image,
    pub coord_b: Image,
    pub coord_fb_a: vk::Framebuffer,
    pub coord_fb_b: vk::Framebuffer,

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
        let input_sets = {
            let layouts = vec![input_set_layout; INPUT_RING];
            let info = vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(input_pool)
                .set_layouts(&layouts);
            unsafe { device.allocate_descriptor_sets(&info)? }
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

        // Jump-flood resources. The coordinate buffers store signed pixel
        // offsets, so they need a float format, not the 8-bit sRGB canvas one.
        let coord_usage =
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
        let coord_a = Image::new_2d(
            device,
            allocator,
            "jfa-coord-a",
            super::vulkan::JFA_FORMAT,
            canvas_extent,
            coord_usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let coord_b = Image::new_2d(
            device,
            allocator,
            "jfa-coord-b",
            super::vulkan::JFA_FORMAT,
            canvas_extent,
            coord_usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let jfa_render_pass = create_color_render_pass(device, super::vulkan::JFA_FORMAT)?;
        let coord_fb_a = create_framebuffer(device, jfa_render_pass, canvas_extent, coord_a.view)?;
        let coord_fb_b = create_framebuffer(device, jfa_render_pass, canvas_extent, coord_b.view)?;

        let jfa_seed = create_pipeline(device, pipeline_layout, jfa_render_pass, JFA_SEED_FRAG_SPV)?;
        let jfa_flood =
            create_pipeline(device, pipeline_layout, jfa_render_pass, JFA_FLOOD_FRAG_SPV)?;
        let stroke_layout =
            create_pipeline_layout_sized(device, input_set_layout, STROKE_PUSH_BYTES)?;
        let jfa_resolve =
            create_pipeline(device, stroke_layout, canvas_render_pass, JFA_RESOLVE_FRAG_SPV)?;

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
            input_sets,
            pipeline_layout,
            hsv,
            invert,
            box_blur,
            sharpen,
            mask_mix,
            stroke_layout,
            jfa_seed,
            jfa_flood,
            jfa_resolve,
            jfa_render_pass,
            coord_a,
            coord_b,
            coord_fb_a,
            coord_fb_b,
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

    /// Image view for a scratch slot (for binding as a sampler source).
    pub(super) const fn scratch_view(&self, which: Scratch) -> vk::ImageView {
        match which {
            Scratch::A => self.scratch_a.view,
            Scratch::B => self.scratch_b.view,
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
    /// The input set at ring position `i` (wraps).
    pub(super) fn input_set(&self, i: usize) -> vk::DescriptorSet {
        self.input_sets[i % self.input_sets.len()]
    }

    pub(super) fn write_input(
        &self,
        device: &Device,
        set: vk::DescriptorSet,
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
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[0..1]),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[1..2]),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
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
            device.destroy_pipeline(self.jfa_seed, None);
            device.destroy_pipeline(self.jfa_flood, None);
            device.destroy_pipeline(self.jfa_resolve, None);
            device.destroy_pipeline_layout(self.stroke_layout, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.composite_pool, None);
            device.destroy_descriptor_pool(self.input_pool, None);
            device.destroy_descriptor_set_layout(self.input_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_framebuffer(self.framebuffer_a, None);
            device.destroy_framebuffer(self.framebuffer_b, None);
            device.destroy_framebuffer(self.coord_fb_a, None);
            device.destroy_framebuffer(self.coord_fb_b, None);
            device.destroy_render_pass(self.jfa_render_pass, None);
            self.scratch_a.destroy(device, allocator);
            self.scratch_b.destroy(device, allocator);
            self.coord_a.destroy(device, allocator);
            self.coord_b.destroy(device, allocator);
        }
    }

    /// Image handle for a jump-flood coordinate slot (for barriers).
    pub(super) const fn coord_handle(&self, which: JfaSlot) -> vk::Image {
        match which {
            JfaSlot::A => self.coord_a.handle,
            JfaSlot::B => self.coord_b.handle,
        }
    }

    /// Image view for a jump-flood coordinate slot (sampler source).
    pub(super) const fn coord_view(&self, which: JfaSlot) -> vk::ImageView {
        match which {
            JfaSlot::A => self.coord_a.view,
            JfaSlot::B => self.coord_b.view,
        }
    }

    /// Framebuffer to render a jump-flood pass into a coordinate slot.
    pub(super) const fn coord_framebuffer(&self, which: JfaSlot) -> vk::Framebuffer {
        match which {
            JfaSlot::A => self.coord_fb_a,
            JfaSlot::B => self.coord_fb_b,
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
        descriptor_count: 3 * INPUT_RING as u32,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(INPUT_RING as u32);
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
    create_pipeline_layout_sized(device, set_layout, FILTER_PUSH_BYTES)
}

fn create_pipeline_layout_sized(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
    push_bytes: u32,
) -> Result<vk::PipelineLayout, RendererError> {
    let set_layouts = [set_layout];
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(push_bytes)];
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

/// A single-attachment render pass for `format`, resting in GENERAL like the
/// rest of the renderer. Used for the jump-flood coordinate buffers, whose
/// float format differs from the canvas render pass's.
fn create_color_render_pass(
    device: &Device,
    format: vk::Format,
) -> Result<vk::RenderPass, RendererError> {
    let attachments = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::GENERAL)
        .final_layout(vk::ImageLayout::GENERAL)];
    let color_refs = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::GENERAL)];
    let subpasses = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs)];
    let info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses);
    Ok(unsafe { device.create_render_pass(&info, None)? })
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
