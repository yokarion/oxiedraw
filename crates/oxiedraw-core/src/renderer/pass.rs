//! Shared construction helpers for the fullscreen-triangle passes.
//!
//! Nearly every effect in this renderer is the same shape: no vertex input
//! (verts come from `gl_VertexIndex`), dynamic viewport/scissor, one colour
//! attachment, and some combined-image-samplers plus a push-constant block.
//! Only the shaders, the blend mode, the push size and the binding count
//! actually differ, so those are the parameters here and everything else is
//! fixed.
//!
//! Keeping pipeline construction in one place is also what makes
//! user-supplied shaders tractable later: a custom effect is a
//! [`FullscreenPass`] with a different `frag_spv`, not a new module.

use ash::{Device, vk};

use super::RendererError;

/// Turns SPIR-V bytes into a shader module. The caller owns the result and
/// must destroy it; [`FullscreenPass::build`] does that for its own modules.
pub(super) fn shader_module(
    device: &Device,
    bytes: &[u8],
) -> Result<vk::ShaderModule, RendererError> {
    assert!(bytes.len().is_multiple_of(4), "SPIR-V is 4-byte-aligned");
    let code: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}

/// Linear filtering, clamp-to-edge, no mips - the sampler every pass here
/// wants. Clamping means UVs at the image border replicate the edge texel
/// rather than pulling in garbage.
pub(super) fn linear_clamp_sampler(device: &Device) -> Result<vk::Sampler, RendererError> {
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

/// Point-sampled variant of [`linear_clamp_sampler`], for masks that must
/// not be interpolated between texels.
pub(super) fn nearest_clamp_sampler(device: &Device) -> Result<vk::Sampler, RendererError> {
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

/// Descriptor set layout of `count` fragment-stage combined-image-samplers
/// bound at 0..count.
pub(super) fn sampler_set_layout(
    device: &Device,
    count: u32,
) -> Result<vk::DescriptorSetLayout, RendererError> {
    let bindings: Vec<_> = (0..count)
        .map(|binding| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        })
        .collect();
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    Ok(unsafe { device.create_descriptor_set_layout(&info, None)? })
}

/// Pipeline layout with one set and a fragment-stage push range. Pass
/// `push_bytes: 0` for a pass that pushes nothing.
pub(super) fn pipeline_layout(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
    push_bytes: u32,
) -> Result<vk::PipelineLayout, RendererError> {
    let set_layouts = [set_layout];
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(push_bytes)];
    let mut info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
    if push_bytes > 0 {
        info = info.push_constant_ranges(&push_ranges);
    }
    Ok(unsafe { device.create_pipeline_layout(&info, None)? })
}

/// Pool sized for a single set holding `count` combined-image-samplers.
pub(super) fn sampler_descriptor_pool(
    device: &Device,
    count: u32,
) -> Result<vk::DescriptorPool, RendererError> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: count,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(1);
    Ok(unsafe { device.create_descriptor_pool(&info, None)? })
}

/// Allocates one set from `pool` and points binding `i` at `views[i]`.
pub(super) fn allocate_sampler_set(
    device: &Device,
    pool: vk::DescriptorPool,
    layout: vk::DescriptorSetLayout,
    views: &[vk::ImageView],
    sampler: vk::Sampler,
) -> Result<vk::DescriptorSet, RendererError> {
    let layouts = [layout];
    let info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&layouts);
    let set = unsafe { device.allocate_descriptor_sets(&info)? }[0];
    write_sampler_set(device, set, views, sampler);
    Ok(set)
}

/// Re-points an already-allocated set at `views`. Split out from
/// [`allocate_sampler_set`] because some passes rebind after a resize.
pub(super) fn write_sampler_set(
    device: &Device,
    set: vk::DescriptorSet,
    views: &[vk::ImageView],
    sampler: vk::Sampler,
) {
    // The `DescriptorImageInfo`s must outlive the `update_descriptor_sets`
    // call, so they are collected up front rather than built inside the map.
    let image_infos: Vec<_> = views
        .iter()
        .map(|&view| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::GENERAL)
                .sampler(sampler)]
        })
        .collect();
    let writes: Vec<_> = image_infos
        .iter()
        .enumerate()
        .map(|(i, info)| {
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(i as u32)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(info)
        })
        .collect();
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

/// Premultiplied OVER: `out = src + dst * (1 - src.a)`.
pub(super) fn over_blend() -> vk::PipelineColorBlendAttachmentState {
    vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
}

/// DST_OUT: `out = dst * (1 - src.a)`. The eraser's compositing - the
/// shader's premultiplied colour is discarded (src factor zero) and only
/// its alpha scales the target down.
pub(super) fn dst_out_blend() -> vk::PipelineColorBlendAttachmentState {
    vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ZERO)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ZERO)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
}

/// No blending - the fragment replaces whatever the load op left behind.
pub(super) fn replace_blend() -> vk::PipelineColorBlendAttachmentState {
    vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(false)
}

/// A fullscreen-triangle graphics pipeline.
///
/// Everything not named here is fixed: no vertex input, triangle list,
/// fill/no-cull raster, 1 sample, dynamic viewport + scissor, one colour
/// attachment with an RGBA write mask, subpass 0.
pub(super) struct FullscreenPass<'a> {
    pub vert_spv: &'a [u8],
    pub frag_spv: &'a [u8],
    pub render_pass: vk::RenderPass,
    pub layout: vk::PipelineLayout,
    pub blend: vk::PipelineColorBlendAttachmentState,
}

impl FullscreenPass<'_> {
    pub(super) fn build(&self, device: &Device) -> Result<vk::Pipeline, RendererError> {
        let vert = shader_module(device, self.vert_spv)?;
        let frag = shader_module(device, self.frag_spv)?;

        // Modules can be destroyed as soon as the pipeline is created, but
        // that must also happen if creation fails, hence the explicit result
        // binding instead of `?`.
        let result = self.create_with_modules(device, vert, frag);
        unsafe {
            device.destroy_shader_module(vert, None);
            device.destroy_shader_module(frag, None);
        }
        result
    }

    fn create_with_modules(
        &self,
        device: &Device,
        vert: vk::ShaderModule,
        frag: vk::ShaderModule,
    ) -> Result<vk::Pipeline, RendererError> {
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

        // The write mask is the caller's: most passes want RGBA (the blend
        // helpers set it), but the R8 selection-mask passes write R only.
        let blend_attachments = [self.blend];
        let blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);

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
            .layout(self.layout)
            .render_pass(self.render_pass)
            .subpass(0);
        let infos = [pipeline_info];
        let pipelines =
            unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &infos, None) }
                .map_err(|(_, e)| RendererError::Vulkan(e))?;
        Ok(pipelines[0])
    }
}
