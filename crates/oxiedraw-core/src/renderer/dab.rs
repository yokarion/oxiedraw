use ash::{Device, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::Allocator;

use crate::brush_engine::Dab;

use super::RendererError;
use super::resources::Buffer;

/// One brush dab on the GPU.
///
/// Layout (offsets in bytes):
/// - `center`           0   vec2
/// - `radius`           8   f32
/// - `rotation`        12   f32 (radians; soft-round/pixel ignore)
/// - `aspect`          16   f32 (1.0 = round)
/// - `flow`            20   f32 (coverage multiplier, 0..=1)
/// - `color_premul`    24   vec4 (premultiplied linear RGBA)
/// - `texture_uv`      40   vec4 (u0,v0,u1,v1; unused by global-texture path)
/// - `hardness`        56   f32 (edge falloff; 1.0 = crisp)
/// - `tip`             60   f32 (textured tip: 0 round, 1 square)
/// - `texture_scale`   64   f32 (global grain tile size in canvas px)
/// - `texture_strength` 68  f32 (grain modulation 0..=1)
/// Total stride = 72, all components 4-byte aligned.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DabInstance {
    pub center: [f32; 2],
    pub radius: f32,
    pub rotation: f32,
    pub aspect: f32,
    pub flow: f32,
    pub color_premul: [f32; 4],
    pub texture_uv: [f32; 4],
    pub hardness: f32,
    pub tip: f32,
    pub texture_scale: f32,
    pub texture_strength: f32,
}

const _: () = assert!(std::mem::size_of::<DabInstance>() == DAB_INSTANCE_STRIDE as usize);

pub(super) const DAB_INSTANCE_STRIDE: u32 = 72;

/// Maximum dabs uploadable in a single call. ~4.7 MB at 72 bytes each.
pub(super) const MAX_INSTANCES: u32 = 64 * 1024;

const QUAD_VERTS: [[f32; 2]; 4] = [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]];

const DAB_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dab.vert.spv"));
const DAB_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dab.frag.spv"));
const DAB_PIXEL_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dab_pixel.vert.spv"));
const DAB_PIXEL_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dab_pixel.frag.spv"));
const DAB_TEXTURED_VERT_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/dab_textured.vert.spv"));
const DAB_TEXTURED_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/dab_textured.frag.spv"));

/// Renderer-side brush family. The bridge in `canvas/stamp.rs`
/// translates the brush-engine `BrushFamily` (which carries pattern
/// data) into this slimmer enum with the atlas slice already resolved.
#[derive(Debug, Clone, Copy)]
pub enum DabFamily {
    SoftRound,
    Pixel,
    Textured { slice: u32 },
}

impl DabFamily {
    pub const COUNT: usize = 3;

    pub const fn kind_index(self) -> usize {
        match self {
            Self::SoftRound => 0,
            Self::Pixel => 1,
            Self::Textured { .. } => 2,
        }
    }

    pub const fn slice(self) -> u32 {
        match self {
            Self::Textured { slice } => slice,
            _ => 0,
        }
    }

    pub const fn binds_atlas(self) -> bool {
        matches!(self, Self::Textured { .. })
    }
}

/// `vec2 inv_size + uint slice`, padded to 12 bytes. Shared across
/// every dab/mask pipeline so the renderer can push uniformly.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DabPushConstants {
    pub inv_size: [f32; 2],
    pub slice: u32,
}

impl DabPushConstants {
    pub(super) const SIZE: u32 = 12;

    pub(super) const fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` POD with only f32/u32 fields.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

impl DabInstance {
    /// Convert a brush-engine `Dab` to its GPU form. `Color` is opaque
    /// (no alpha channel), so the premul form has alpha=1 and the RGB
    /// channels need no scaling. Stroke-level opacity is applied later
    /// at composite, not per-dab.
    pub fn from_dab(dab: &Dab) -> Self {
        let [r, g, b] = dab.color.to_linear_rgb();
        Self {
            center: [dab.center.x, dab.center.y],
            radius: dab.radius,
            rotation: dab.rotation,
            aspect: dab.aspect,
            flow: dab.flow,
            color_premul: [r, g, b, 1.0],
            texture_uv: dab.texture_uv,
            hardness: dab.hardness,
            tip: dab.tip,
            texture_scale: dab.texture_scale,
            texture_strength: dab.texture_strength,
        }
    }
}

/// The unit-quad vertex buffer + the per-instance dab buffer. Shared
/// by every pipeline that consumes `DabInstance`s.
pub(super) struct DabBuffers {
    pub vertex: Buffer,
    pub instance: Buffer,
}

impl DabBuffers {
    pub(super) fn new(device: &Device, allocator: &mut Allocator) -> Result<Self, RendererError> {
        let vertex = create_vertex_buffer(device, allocator)?;
        let instance = create_instance_buffer(device, allocator)?;
        Ok(Self { vertex, instance })
    }

    /// Returns the number of instances actually uploaded (clamped to
    /// `MAX_INSTANCES`).
    pub(super) fn upload_instances(
        &mut self,
        instances: &[DabInstance],
    ) -> Result<u32, RendererError> {
        let n_usize = instances.len().min(MAX_INSTANCES as usize);
        let bytes = instances_as_bytes(&instances[..n_usize]);
        let dst = self
            .instance
            .mapped_mut()
            .ok_or(RendererError::StagingNotMapped)?;
        dst[..bytes.len()].copy_from_slice(bytes);
        Ok(u32::try_from(n_usize).expect("clamped above"))
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these buffers is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            self.vertex.destroy(device, allocator);
            self.instance.destroy(device, allocator);
        }
    }
}

pub(super) struct DabPipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

impl DabPipeline {
    /// Build the soft-round dab pipeline (anti-aliased, premul OVER).
    pub(super) fn new_round(
        device: &Device,
        render_pass: vk::RenderPass,
    ) -> Result<Self, RendererError> {
        Self::build(
            device,
            render_pass,
            &[],
            DAB_VERT_SPV,
            DAB_FRAG_SPV,
            premultiplied_over_blend(),
            vk::ColorComponentFlags::RGBA,
        )
    }

    /// Build the pixel-art dab pipeline (hard edge, integer-snapped centre).
    pub(super) fn new_pixel(
        device: &Device,
        render_pass: vk::RenderPass,
    ) -> Result<Self, RendererError> {
        Self::build(
            device,
            render_pass,
            &[],
            DAB_PIXEL_VERT_SPV,
            DAB_PIXEL_FRAG_SPV,
            premultiplied_over_blend(),
            vk::ColorComponentFlags::RGBA,
        )
    }

    /// Build the textured-dab pipeline (samples the pattern atlas).
    /// `atlas_set_layout` is the descriptor set layout from
    /// `PatternAtlas::descriptor_set_layout()`; the pipeline layout
    /// binds it at set 0.
    pub(super) fn new_textured(
        device: &Device,
        render_pass: vk::RenderPass,
        atlas_set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RendererError> {
        Self::build(
            device,
            render_pass,
            &[atlas_set_layout],
            DAB_TEXTURED_VERT_SPV,
            DAB_TEXTURED_FRAG_SPV,
            premultiplied_over_blend(),
            vk::ColorComponentFlags::RGBA,
        )
    }

    pub(super) fn build(
        device: &Device,
        render_pass: vk::RenderPass,
        set_layouts: &[vk::DescriptorSetLayout],
        vert_spv: &[u8],
        frag_spv: &[u8],
        blend_attachment: vk::PipelineColorBlendAttachmentState,
        write_mask: vk::ColorComponentFlags,
    ) -> Result<Self, RendererError> {
        let layout = create_pipeline_layout(device, set_layouts)?;
        let pipeline = build_dab_instanced_pipeline(
            device,
            layout,
            render_pass,
            vert_spv,
            frag_spv,
            blend_attachment,
            write_mask,
        )?;
        Ok(Self { layout, pipeline })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing this pipeline is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

pub(super) fn create_pipeline_layout(
    device: &Device,
    set_layouts: &[vk::DescriptorSetLayout],
) -> Result<vk::PipelineLayout, RendererError> {
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(DabPushConstants::SIZE)];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(set_layouts)
        .push_constant_ranges(&push_ranges);
    let layout = unsafe { device.create_pipeline_layout(&info, None)? };
    Ok(layout)
}

/// Build a graphics pipeline whose vertex input matches the shared
/// `DabBuffers` layout (binding 0 = unit quad, binding 1 = `DabInstance`).
pub(super) fn build_dab_instanced_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
    vert_spv: &[u8],
    frag_spv: &[u8],
    blend_attachment: vk::PipelineColorBlendAttachmentState,
    write_mask: vk::ColorComponentFlags,
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

    let bindings = [
        vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(8)
            .input_rate(vk::VertexInputRate::VERTEX),
        vk::VertexInputBindingDescription::default()
            .binding(1)
            .stride(DAB_INSTANCE_STRIDE)
            .input_rate(vk::VertexInputRate::INSTANCE),
    ];
    let attrs = [
        // binding 0
        vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(0),
        // binding 1
        vk::VertexInputAttributeDescription::default()
            .location(1)
            .binding(1)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(0), // center
        vk::VertexInputAttributeDescription::default()
            .location(2)
            .binding(1)
            .format(vk::Format::R32_SFLOAT)
            .offset(8), // radius
        vk::VertexInputAttributeDescription::default()
            .location(3)
            .binding(1)
            .format(vk::Format::R32_SFLOAT)
            .offset(12), // rotation
        vk::VertexInputAttributeDescription::default()
            .location(4)
            .binding(1)
            .format(vk::Format::R32_SFLOAT)
            .offset(16), // aspect
        vk::VertexInputAttributeDescription::default()
            .location(5)
            .binding(1)
            .format(vk::Format::R32_SFLOAT)
            .offset(20), // flow
        vk::VertexInputAttributeDescription::default()
            .location(6)
            .binding(1)
            .format(vk::Format::R32G32B32A32_SFLOAT)
            .offset(24), // color_premul
        vk::VertexInputAttributeDescription::default()
            .location(7)
            .binding(1)
            .format(vk::Format::R32G32B32A32_SFLOAT)
            .offset(40), // texture_uv
        vk::VertexInputAttributeDescription::default()
            .location(8)
            .binding(1)
            .format(vk::Format::R32_SFLOAT)
            .offset(56), // hardness
        vk::VertexInputAttributeDescription::default()
            .location(9)
            .binding(1)
            .format(vk::Format::R32_SFLOAT)
            .offset(60), // tip
        vk::VertexInputAttributeDescription::default()
            .location(10)
            .binding(1)
            .format(vk::Format::R32_SFLOAT)
            .offset(64), // texture_scale
        vk::VertexInputAttributeDescription::default()
            .location(11)
            .binding(1)
            .format(vk::Format::R32_SFLOAT)
            .offset(68), // texture_strength
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attrs);

    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);

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

    let attachment = blend_attachment.color_write_mask(write_mask);
    let blend_attachments = [attachment];
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

fn premultiplied_over_blend() -> vk::PipelineColorBlendAttachmentState {
    vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
}

fn create_vertex_buffer(
    device: &Device,
    allocator: &mut Allocator,
) -> Result<Buffer, RendererError> {
    let bytes = quad_as_bytes(&QUAD_VERTS);
    let mut buf = Buffer::new(
        device,
        allocator,
        "dab-quad",
        bytes.len() as u64,
        vk::BufferUsageFlags::VERTEX_BUFFER,
        MemoryLocation::CpuToGpu,
    )?;
    let dst = buf.mapped_mut().ok_or(RendererError::StagingNotMapped)?;
    dst[..bytes.len()].copy_from_slice(bytes);
    Ok(buf)
}

fn create_instance_buffer(
    device: &Device,
    allocator: &mut Allocator,
) -> Result<Buffer, RendererError> {
    let size = u64::from(MAX_INSTANCES) * u64::from(DAB_INSTANCE_STRIDE);
    Buffer::new(
        device,
        allocator,
        "dab-instances",
        size,
        vk::BufferUsageFlags::VERTEX_BUFFER,
        MemoryLocation::CpuToGpu,
    )
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

const fn instances_as_bytes(slice: &[DabInstance]) -> &[u8] {
    // SAFETY: DabInstance is repr(C) and contains only f32 fields, so it
    // is plain old data with a deterministic layout matching what the
    // vertex shader expects.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), std::mem::size_of_val(slice)) }
}

const fn quad_as_bytes(slice: &[[f32; 2]; 4]) -> &[u8] {
    // SAFETY: [[f32; 2]; 4] is a fixed-size array of f32, plain old data.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), std::mem::size_of_val(slice)) }
}
