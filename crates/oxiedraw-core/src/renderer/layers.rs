//! Per-layer image storage + composite pipeline.
//!
//! Each `LayerSlot` is a canvas-sized BGRA image (premultiplied, sRGB),
//! plus a framebuffer (so the renderer can target it with the existing
//! composite-into-render-pass machinery) and a descriptor set (so the
//! [`LayerCompositePipeline`] can read it).
//!
//! `LayerStack` keeps the slot list strictly in lockstep with
//! `oxiedraw_core::document::LayerState` - index *i* in both refers to
//! the same logical layer.

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::resources::Image;

const VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/layer_composite.frag.spv"));
const BLEND_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/layer_blend.frag.spv"));

/// Push constant for the blend pipeline: blend-mode index + layer opacity.
/// 8 bytes (`uint` + `float`), fragment stage.
pub(super) const BLEND_PUSH_BYTES: u32 = 8;

/// Maximum number of layers per document. Fixed-size descriptor pool -
/// keeps the allocator dead-simple; can be made elastic later.
pub const MAX_LAYERS: u32 = 128;

/// Pipeline that samples one premultiplied BGRA layer image and blends
/// it onto the bound render target (canvas) with premultiplied OVER.
/// One pipeline, many descriptor sets (one per layer).
pub(super) struct LayerCompositePipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub sampler: vk::Sampler,
}

impl LayerCompositePipeline {
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

/// Pipeline that samples a premultiplied layer (set 0) and an accumulator
/// (set 1) and writes the src-over-dst result of the layer's blend mode +
/// opacity. Blending is disabled (replace); callers ping-pong the accumulator
/// through a scratch copy. Reuses [`LayerCompositePipeline`]'s single-sampler
/// set layout for both descriptor slots.
pub(super) struct LayerBlendPipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

impl LayerBlendPipeline {
    pub(super) fn new(
        device: &Device,
        canvas_render_pass: vk::RenderPass,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RendererError> {
        let layout = create_blend_pipeline_layout(device, set_layout)?;
        let pipeline = create_blend_pipeline(device, layout, canvas_render_pass)?;
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

pub(super) struct LayerSlot {
    pub image: Image,
    pub framebuffer: vk::Framebuffer,
    pub descriptor_set: vk::DescriptorSet,
    /// Bumped on every write to this slot's image. Lets the layers panel
    /// re-read only the layers that actually changed (it travels with the slot
    /// through reorder; new slots start at 0).
    pub content_version: u64,
    /// Blend-mode index (matches `layer_blend.frag`) used when this layer is
    /// composited. Travels with the slot through reorder; new slots start at 0
    /// (Normal).
    pub blend_mode: u32,
    /// Layer opacity in `0.0..=1.0`. New slots start at 1.0 (opaque).
    pub opacity: f32,
}

pub(super) struct LayerStack {
    pub slots: Vec<LayerSlot>,
    descriptor_pool: vk::DescriptorPool,
}

impl LayerStack {
    pub(super) fn new(device: &Device) -> Result<Self, RendererError> {
        let descriptor_pool = create_descriptor_pool(device)?;
        Ok(Self {
            slots: Vec::new(),
            descriptor_pool,
        })
    }

    /// Mark slot `idx`'s image as changed (cheap monotonic counter).
    pub(super) fn touch(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx) {
            slot.content_version = slot.content_version.wrapping_add(1);
        }
    }

    /// Current content version of slot `idx` (0 if out of range).
    pub(super) fn version(&self, idx: usize) -> u64 {
        self.slots.get(idx).map_or(0, |s| s.content_version)
    }

    /// Set the blend-mode index + opacity of slot `idx`. No-op if out of range.
    pub(super) fn set_blend(&mut self, idx: usize, mode: u32, opacity: f32) {
        if let Some(slot) = self.slots.get_mut(idx) {
            slot.blend_mode = mode;
            slot.opacity = opacity.clamp(0.0, 1.0);
        }
    }

    /// Blend-mode index + opacity of slot `idx` (Normal / opaque if out of range).
    pub(super) fn blend(&self, idx: usize) -> (u32, f32) {
        self.slots.get(idx).map_or((0, 1.0), |s| (s.blend_mode, s.opacity))
    }

    /// Allocate a new layer slot and append it to the stack. Returns
    /// the new layer's index.
    pub(super) fn add(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        pipeline: &LayerCompositePipeline,
        canvas_render_pass: vk::RenderPass,
        extent: vk::Extent2D,
        format: vk::Format,
    ) -> Result<usize, RendererError> {
        if u32::try_from(self.slots.len()).unwrap_or(u32::MAX) >= MAX_LAYERS {
            return Err(RendererError::LayerLimit);
        }
        let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED;
        let image = Image::new_2d(
            device,
            allocator,
            "layer",
            format,
            extent,
            usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let framebuffer = create_framebuffer(device, canvas_render_pass, extent, image.view)?;
        let descriptor_set = allocate_descriptor_set(
            device,
            self.descriptor_pool,
            pipeline.descriptor_set_layout,
            image.view,
            pipeline.sampler,
        )?;
        self.slots.push(LayerSlot {
            image,
            framebuffer,
            descriptor_set,
            content_version: 0,
            blend_mode: 0,
            opacity: 1.0,
        });
        Ok(self.slots.len() - 1)
    }

    /// Remove the slot at `idx`. Slots above shift down - descriptor
    /// sets and framebuffers stay bound to the same physical images,
    /// so no descriptor rewrites are needed.
    ///
    /// # Safety
    /// Caller must ensure no GPU work referencing this slot is in flight
    /// (typically by issuing `device_wait_idle` first).
    pub(super) unsafe fn remove(
        &mut self,
        idx: usize,
        device: &Device,
        allocator: &mut Allocator,
    ) -> Result<(), RendererError> {
        if idx >= self.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        let slot = self.slots.remove(idx);
        unsafe {
            device.destroy_framebuffer(slot.framebuffer, None);
            slot.image.destroy(device, allocator);
            device
                .free_descriptor_sets(self.descriptor_pool, &[slot.descriptor_set])
                .ok();
        }
        Ok(())
    }

    /// Move slot at `from` to position `to`. Pure metadata - same
    /// image data, same descriptor sets, just a reordered Vec. The
    /// composite shader iterates slots in array order, so this changes
    /// z-order.
    pub(super) fn reorder(&mut self, from: usize, to: usize) {
        if from >= self.slots.len() || to >= self.slots.len() || from == to {
            return;
        }
        let slot = self.slots.remove(from);
        self.slots.insert(to, slot);
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing any slot is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            for slot in self.slots {
                device.destroy_framebuffer(slot.framebuffer, None);
                slot.image.destroy(device, allocator);
            }
            device.destroy_descriptor_pool(self.descriptor_pool, None);
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline construction helpers
// ---------------------------------------------------------------------------

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
    let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
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

    // Premultiplied OVER: out = src + dst * (1 - src.a).
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

fn create_blend_pipeline_layout(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
) -> Result<vk::PipelineLayout, RendererError> {
    // Two descriptor sets (src layer, dst accumulator) reusing the same
    // single-sampler layout, plus the mode/opacity push constant.
    let set_layouts = [set_layout, set_layout];
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(BLEND_PUSH_BYTES)];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(&push_ranges);
    Ok(unsafe { device.create_pipeline_layout(&info, None)? })
}

fn create_blend_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
) -> Result<vk::Pipeline, RendererError> {
    let vert = shader_module(device, VERT_SPV)?;
    let frag = shader_module(device, BLEND_FRAG_SPV)?;

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

    // Replace, not blend: the shader fully computes the src-over-dst result.
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

fn create_descriptor_pool(device: &Device) -> Result<vk::DescriptorPool, RendererError> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: MAX_LAYERS,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        // FREE_DESCRIPTOR_SET so `vkFreeDescriptorSets` works on remove.
        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
        .pool_sizes(&sizes)
        .max_sets(MAX_LAYERS);
    Ok(unsafe { device.create_descriptor_pool(&info, None)? })
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

fn allocate_descriptor_set(
    device: &Device,
    pool: vk::DescriptorPool,
    layout: vk::DescriptorSetLayout,
    image_view: vk::ImageView,
    sampler: vk::Sampler,
) -> Result<vk::DescriptorSet, RendererError> {
    let layouts = [layout];
    let alloc_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&layouts);
    let set = unsafe { device.allocate_descriptor_sets(&alloc_info)? }[0];

    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(image_view)
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

fn shader_module(device: &Device, bytes: &[u8]) -> Result<vk::ShaderModule, RendererError> {
    assert!(bytes.len().is_multiple_of(4), "SPIR-V is 4-byte-aligned");
    let mut code = vec![0u32; bytes.len() / 4];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        code[i] = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}
