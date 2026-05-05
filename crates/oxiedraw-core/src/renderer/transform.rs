//! GPU pipeline for the transform tool's apply step.
//!
//! Samples a source texture with a CPU-precomputed 2x3 inverse affine and
//! writes into a framebuffer (typically the active layer's image for the
//! canvas-clipped pass, plus an arbitrarily-sized AABB image for the
//! extension/off-canvas pass).
//!
//! Render-pass-compatible with the canvas render pass - any framebuffer
//! built around a `CANVAS_FORMAT` colour attachment can be the target.

use ash::{Device, vk};

use super::RendererError;

const VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/transform.frag.spv"));

/// Push constant block size in bytes: two `vec4`s (matrix rows packed with
/// the translation in `.z` and a `.w` padding for std140 alignment).
pub(super) const PUSH_BYTES: u32 = 32;

pub(super) struct TransformPipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub sampler: vk::Sampler,
}

impl TransformPipeline {
    pub(super) fn new(
        device: &Device,
        canvas_render_pass: vk::RenderPass,
    ) -> Result<Self, RendererError> {
        let descriptor_set_layout = create_descriptor_set_layout(device)?;
        let layout = create_pipeline_layout(device, descriptor_set_layout)?;
        let pipeline = create_pipeline(device, layout, canvas_render_pass)?;
        let sampler = create_sampler(device)?;
        Ok(Self {
            layout,
            pipeline,
            descriptor_set_layout,
            sampler,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing this pipeline is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
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
        .size(PUSH_BYTES)];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(&push_ranges);
    Ok(unsafe { device.create_pipeline_layout(&info, None)? })
}

fn create_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
) -> Result<vk::Pipeline, RendererError> {
    let vert = shader_module(device, VERT_SPV)?;
    let frag = shader_module(device, FRAG_SPV)?;

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

    // We REPLACE the framebuffer contents (it was cleared by the render pass
    // load op), so blending is disabled. RGBA writes only.
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
    // Linear filtering matches the CPU `sample_bilinear` path. CLAMP_TO_EDGE
    // means out-of-bounds reads replicate the (transparent) edge column,
    // which matches `sample_bilinear`'s `clamp(0, w-1)` behaviour.
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

fn shader_module(device: &Device, bytes: &[u8]) -> Result<vk::ShaderModule, RendererError> {
    assert!(bytes.len().is_multiple_of(4), "SPIR-V is 4-byte-aligned");
    let mut code = vec![0u32; bytes.len() / 4];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        code[i] = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}
