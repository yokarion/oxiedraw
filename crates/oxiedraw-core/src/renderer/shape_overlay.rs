//! Shape-overlay GPU resources: a fullscreen pipeline that rasterises
//! one of the shape primitives directly from push constants and OVER-
//! blends the result into the bound target (preview image during drag,
//! layer image at commit).
//!
//! No textures are uploaded - the entire shape lives in 48 bytes of
//! push data. The only binding is the existing selection mask, used
//! to clip the fill against an active selection.

use ash::{Device, vk};

use super::RendererError;

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const SHAPE_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shape.frag.spv"));

/// 48 bytes: 3x vec4 (color, rect, extra). See `shape.frag`.
pub(super) const SHAPE_PUSH_BYTES: u32 = 48;

pub(super) struct ShapeOverlayResources {
    pub sampler: vk::Sampler,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

impl ShapeOverlayResources {
    pub(super) fn new(
        device: &Device,
        canvas_render_pass: vk::RenderPass,
        selection_mask_view: vk::ImageView,
    ) -> Result<Self, RendererError> {
        let sampler = create_sampler(device)?;
        let descriptor_set_layout = create_descriptor_set_layout(device)?;
        let descriptor_pool = create_descriptor_pool(device)?;
        let descriptor_set = allocate_and_update_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            selection_mask_view,
            sampler,
        )?;
        let layout = create_pipeline_layout(device, descriptor_set_layout)?;
        let pipeline = create_pipeline(device, layout, canvas_render_pass)?;
        Ok(Self {
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            descriptor_set,
            layout,
            pipeline,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
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

fn create_pipeline_layout(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
) -> Result<vk::PipelineLayout, RendererError> {
    let set_layouts = [set_layout];
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(SHAPE_PUSH_BYTES)];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(&push_ranges);
    Ok(unsafe { device.create_pipeline_layout(&info, None)? })
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
    // Linear so the selection-mask edge is interpolated, matching the
    // brush composite path; the shape itself is rasterised in the
    // fragment shader and doesn't go through this sampler.
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

fn create_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
) -> Result<vk::Pipeline, RendererError> {
    let vert = shader_module(device, COMPOSITE_VERT_SPV)?;
    let frag = shader_module(device, SHAPE_FRAG_SPV)?;

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

    // Premultiplied OVER - colour is `coverage * push.color`, alpha is
    // `coverage * push.color.a`, both blend identically.
    let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
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

fn shader_module(device: &Device, bytes: &[u8]) -> Result<vk::ShaderModule, RendererError> {
    assert!(bytes.len().is_multiple_of(4), "SPIR-V is 4-byte-aligned");
    let mut code = vec![0u32; bytes.len() / 4];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        code[i] = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}
