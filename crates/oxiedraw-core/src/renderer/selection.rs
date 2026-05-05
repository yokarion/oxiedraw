//! GPU resources backing the Photoshop-style selection mask.
//!
//! Three images live here:
//!
//! - `mask` - canvas-sized `R8_UNORM`. The authoritative "which pixels are
//!             currently selected" buffer. 0.0 = unselected, 1.0 = fully
//!             selected, intermediate = partial (feather / AA).
//! - `scratch` - canvas-sized `R8_UNORM`. Used as the source for blend
//!             operations: a CPU-rasterised shape is uploaded here, then
//!             a fullscreen pass blends it into `mask` with the requested
//!             boolean op (Replace / Add / Subtract / Intersect).
//! - `edges` - small `R8_UNORM` (canvas / `EDGES_DOWNSAMPLE` in each dim).
//!             The mask is sampled into this buffer with linear filtering,
//!             then read back to CPU for marching-squares contour
//!             extraction (drives the marching-ants overlay).
//!
//! Plus four blend pipelines, one per `SelectionBlendMode`, all sharing
//! one fragment shader.

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::resources::Image;
use super::targets::ImageTarget;

/// Linear factor between the mask resolution and the edges buffer. The
/// edges buffer trades resolution for a cheap readback; marching squares
/// runs on the smaller grid. 4 gives ants accurate to ~4 canvas pixels
/// which is below the typical zoom-out pixel-per-screen-pixel anyway.
pub(super) const EDGES_DOWNSAMPLE: u32 = 4;

const BLEND_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/selection_blend.frag.spv"));
const EDGES_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/selection_edges.frag.spv"));
const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));

/// Which boolean operation to use when blending an incoming shape (in
/// `scratch`) into the mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionBlendMode {
    /// Overwrite mask with scratch. Used for "Replace" mode and for the
    /// final step of `invert` (after a clear-to-1).
    Replace,
    /// `max(dst, src)` - saturating add.
    Add,
    /// `dst * (1 - src)` - subtract scratch from mask.
    Subtract,
    /// `min(dst, src)` - intersect.
    Intersect,
}

pub(super) struct SelectionResources {
    pub mask: Image,
    pub scratch: Image,
    pub edges: Image,

    /// Render target for the mask (full-res). Used by blend passes that
    /// write to `mask`.
    pub mask_target: ImageTarget,
    /// Render target for the edges buffer (downsampled).
    pub edges_target: ImageTarget,

    pub sampler: vk::Sampler,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub descriptor_pool: vk::DescriptorPool,
    /// Reads from `scratch`. Bound for `apply_*` ops.
    pub scratch_descriptor_set: vk::DescriptorSet,
    /// Reads from `mask`. Bound for the edges pass.
    pub mask_descriptor_set: vk::DescriptorSet,

    /// One pipeline per blend mode, all sharing the same fragment shader.
    /// Index by `SelectionBlendMode as usize`.
    pub blend_layout: vk::PipelineLayout,
    pub blend_pipelines: [vk::Pipeline; 4],

    pub edges_layout: vk::PipelineLayout,
    pub edges_pipeline: vk::Pipeline,

    pub edges_extent: vk::Extent2D,
}

impl SelectionResources {
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        canvas_extent: vk::Extent2D,
    ) -> Result<Self, RendererError> {
        let format = vk::Format::R8_UNORM;
        let mask_usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED;

        let mask = Image::new_2d(
            device,
            allocator,
            "selection-mask",
            format,
            canvas_extent,
            mask_usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let scratch = Image::new_2d(
            device,
            allocator,
            "selection-scratch",
            format,
            canvas_extent,
            mask_usage,
            vk::ImageAspectFlags::COLOR,
        )?;

        let edges_extent = vk::Extent2D {
            width: (canvas_extent.width / EDGES_DOWNSAMPLE).max(1),
            height: (canvas_extent.height / EDGES_DOWNSAMPLE).max(1),
        };
        let edges = Image::new_2d(
            device,
            allocator,
            "selection-edges",
            format,
            edges_extent,
            mask_usage,
            vk::ImageAspectFlags::COLOR,
        )?;

        let mask_target = ImageTarget::new(device, format, canvas_extent, mask.view)?;
        let edges_target = ImageTarget::new(device, format, edges_extent, edges.view)?;

        let sampler = create_sampler(device)?;
        let descriptor_set_layout = create_descriptor_set_layout(device)?;
        let descriptor_pool = create_descriptor_pool(device)?;
        let scratch_descriptor_set = allocate_and_update_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            scratch.view,
            sampler,
        )?;
        let mask_descriptor_set = allocate_and_update_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            mask.view,
            sampler,
        )?;

        let blend_layout = create_blend_pipeline_layout(device, descriptor_set_layout)?;
        let blend_pipelines = [
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Replace)?,
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Add)?,
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Subtract)?,
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Intersect)?,
        ];

        let edges_layout = create_edges_pipeline_layout(device, descriptor_set_layout)?;
        let edges_pipeline = create_edges_pipeline(device, edges_layout, edges_target.render_pass)?;

        Ok(Self {
            mask,
            scratch,
            edges,
            mask_target,
            edges_target,
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            scratch_descriptor_set,
            mask_descriptor_set,
            blend_layout,
            blend_pipelines,
            edges_layout,
            edges_pipeline,
            edges_extent,
        })
    }

    #[must_use]
    pub(super) const fn blend_pipeline(&self, mode: SelectionBlendMode) -> vk::Pipeline {
        self.blend_pipelines[mode as usize]
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            for &p in &self.blend_pipelines {
                device.destroy_pipeline(p, None);
            }
            device.destroy_pipeline_layout(self.blend_layout, None);
            device.destroy_pipeline(self.edges_pipeline, None);
            device.destroy_pipeline_layout(self.edges_layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            self.edges_target.destroy(device);
            self.mask_target.destroy(device);
            self.edges.destroy(device, allocator);
            self.scratch.destroy(device, allocator);
            self.mask.destroy(device, allocator);
        }
    }
}

#[derive(Clone, Copy)]
enum BlendMode {
    Replace,
    Add,
    Subtract,
    Intersect,
}

impl BlendMode {
    fn attachment(self) -> vk::PipelineColorBlendAttachmentState {
        // All modes write only the R channel since the mask is R8.
        let base = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .color_write_mask(vk::ColorComponentFlags::R);
        match self {
            // out = src + dst*0 = src
            Self::Replace => base
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ZERO)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
                .alpha_blend_op(vk::BlendOp::ADD),
            // factors ignored for MAX
            Self::Add => base
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE)
                .color_blend_op(vk::BlendOp::MAX)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                .alpha_blend_op(vk::BlendOp::MAX),
            // out = src*0 + dst*(1-src) = dst*(1-src)
            Self::Subtract => base
                .src_color_blend_factor(vk::BlendFactor::ZERO)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_COLOR)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ZERO)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD),
            // factors ignored for MIN
            Self::Intersect => base
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE)
                .color_blend_op(vk::BlendOp::MIN)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                .alpha_blend_op(vk::BlendOp::MIN),
        }
    }
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
        descriptor_count: 2,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(2);
    Ok(unsafe { device.create_descriptor_pool(&info, None)? })
}

fn allocate_and_update_set(
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

fn create_sampler(device: &Device) -> Result<vk::Sampler, RendererError> {
    let info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::LINEAR)
        .min_filter(vk::Filter::LINEAR)
        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
        .min_lod(0.0)
        .max_lod(0.0);
    Ok(unsafe { device.create_sampler(&info, None)? })
}

fn create_blend_pipeline_layout(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
) -> Result<vk::PipelineLayout, RendererError> {
    let set_layouts = [set_layout];
    let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
    Ok(unsafe { device.create_pipeline_layout(&info, None)? })
}

fn create_edges_pipeline_layout(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
) -> Result<vk::PipelineLayout, RendererError> {
    let set_layouts = [set_layout];
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(8)];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(&push_ranges);
    Ok(unsafe { device.create_pipeline_layout(&info, None)? })
}

fn create_blend_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
    mode: BlendMode,
) -> Result<vk::Pipeline, RendererError> {
    build_fullscreen_pipeline(
        device,
        layout,
        render_pass,
        COMPOSITE_VERT_SPV,
        BLEND_FRAG_SPV,
        mode.attachment(),
    )
}

fn create_edges_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
) -> Result<vk::Pipeline, RendererError> {
    let attachment = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(false)
        .color_write_mask(vk::ColorComponentFlags::R);
    build_fullscreen_pipeline(
        device,
        layout,
        render_pass,
        COMPOSITE_VERT_SPV,
        EDGES_FRAG_SPV,
        attachment,
    )
}

fn build_fullscreen_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
    vert_spv: &[u8],
    frag_spv: &[u8],
    blend_attachment: vk::PipelineColorBlendAttachmentState,
) -> Result<vk::Pipeline, RendererError> {
    let vert = shader_module(device, vert_spv)?;
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

    let blend_attachments = [blend_attachment];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);

    let info = vk::GraphicsPipelineCreateInfo::default()
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
    let infos = [info];
    let pipelines =
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &infos, None) }
            .map_err(|(_, e)| RendererError::Vulkan(e))?;
    unsafe {
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
    }
    Ok(pipelines[0])
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
