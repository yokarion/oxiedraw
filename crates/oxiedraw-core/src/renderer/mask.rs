use ash::{Device, vk};

use super::RendererError;
use super::dab::{self, DabFamily};

const MASK_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mask.frag.spv"));
const MASK_PIXEL_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mask_pixel.frag.spv"));
const MASK_TEXTURED_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/mask_textured.frag.spv"));
const DAB_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dab.vert.spv"));
const DAB_PIXEL_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dab_pixel.vert.spv"));
const DAB_TEXTURED_VERT_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/dab_textured.vert.spv"));

/// Stamps coverage from instanced dabs into an R8 stroke buffer with
/// MAX blending - overlapping dabs saturate to the highest coverage
/// rather than accumulating.
pub(super) struct MaskPipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

/// Saturating coverage blend: `dst = max(dst, src)`. Overlapping dabs in
/// one stroke settle to the highest coverage rather than accumulating -
/// the default (non-build-up) behaviour.
fn max_blend() -> vk::PipelineColorBlendAttachmentState {
    vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE)
        .color_blend_op(vk::BlendOp::MAX)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE)
        .alpha_blend_op(vk::BlendOp::MAX)
}

/// Accumulating coverage blend: `dst = src + dst*(1 - src)` (alpha OVER on
/// the coverage channel). Overlapping dabs build up toward full coverage
/// (clamped by R8_UNORM), so build-up strokes darken where they overlap
/// while the single final composite still caps at the stroke opacity.
fn over_blend() -> vk::PipelineColorBlendAttachmentState {
    vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_COLOR)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
}

impl MaskPipeline {
    /// Soft-round coverage mask (anti-aliased edge).
    pub(super) fn new_round(
        device: &Device,
        render_pass: vk::RenderPass,
        blend: vk::PipelineColorBlendAttachmentState,
    ) -> Result<Self, RendererError> {
        Self::build(device, render_pass, &[], DAB_VERT_SPV, MASK_FRAG_SPV, blend)
    }

    /// Pixel-art coverage mask (hard edge, integer-snapped centre).
    pub(super) fn new_pixel(
        device: &Device,
        render_pass: vk::RenderPass,
        blend: vk::PipelineColorBlendAttachmentState,
    ) -> Result<Self, RendererError> {
        Self::build(
            device,
            render_pass,
            &[],
            DAB_PIXEL_VERT_SPV,
            MASK_PIXEL_FRAG_SPV,
            blend,
        )
    }

    /// Textured coverage mask - samples the pattern atlas alpha.
    pub(super) fn new_textured(
        device: &Device,
        render_pass: vk::RenderPass,
        atlas_set_layout: vk::DescriptorSetLayout,
        blend: vk::PipelineColorBlendAttachmentState,
    ) -> Result<Self, RendererError> {
        Self::build(
            device,
            render_pass,
            &[atlas_set_layout],
            DAB_TEXTURED_VERT_SPV,
            MASK_TEXTURED_FRAG_SPV,
            blend,
        )
    }

    fn build(
        device: &Device,
        render_pass: vk::RenderPass,
        set_layouts: &[vk::DescriptorSetLayout],
        vert_spv: &[u8],
        frag_spv: &[u8],
        blend: vk::PipelineColorBlendAttachmentState,
    ) -> Result<Self, RendererError> {
        let layout = dab::create_pipeline_layout(device, set_layouts)?;
        let pipeline = dab::build_dab_instanced_pipeline(
            device,
            layout,
            render_pass,
            vert_spv,
            frag_spv,
            blend,
            // R8_UNORM has only the R channel - write to it only.
            vk::ColorComponentFlags::R,
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

/// One pipeline per `DabFamily` for each role. Indexed by
/// `DabFamily::kind_index()`.
pub(super) struct MaskPipelineSet {
    pipelines: [MaskPipeline; DabFamily::COUNT],
}

impl MaskPipelineSet {
    /// The default (non-build-up) mask set: MAX-blend coverage.
    pub(super) fn new(
        device: &Device,
        render_pass: vk::RenderPass,
        atlas_set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RendererError> {
        Self::with_blend(device, render_pass, atlas_set_layout, max_blend())
    }

    /// The build-up mask set: OVER-blend coverage so overlapping dabs
    /// accumulate in the stroke buffer instead of saturating.
    pub(super) fn new_buildup(
        device: &Device,
        render_pass: vk::RenderPass,
        atlas_set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RendererError> {
        Self::with_blend(device, render_pass, atlas_set_layout, over_blend())
    }

    fn with_blend(
        device: &Device,
        render_pass: vk::RenderPass,
        atlas_set_layout: vk::DescriptorSetLayout,
        blend: vk::PipelineColorBlendAttachmentState,
    ) -> Result<Self, RendererError> {
        let round = MaskPipeline::new_round(device, render_pass, blend)?;
        let pixel = MaskPipeline::new_pixel(device, render_pass, blend)?;
        let textured = MaskPipeline::new_textured(device, render_pass, atlas_set_layout, blend)?;
        // Index order must match `DabFamily::kind_index`.
        Ok(Self {
            pipelines: [round, pixel, textured],
        })
    }

    pub(super) const fn get(&self, family: DabFamily) -> &MaskPipeline {
        &self.pipelines[family.kind_index()]
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these pipelines is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            for p in self.pipelines {
                p.destroy(device);
            }
        }
    }
}

/// Mirror of `MaskPipelineSet` for the dab pipeline (premul OVER blend).
pub(super) struct DabPipelineSet {
    pipelines: [dab::DabPipeline; DabFamily::COUNT],
}

impl DabPipelineSet {
    pub(super) fn new(
        device: &Device,
        render_pass: vk::RenderPass,
        atlas_set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RendererError> {
        let round = dab::DabPipeline::new_round(device, render_pass)?;
        let pixel = dab::DabPipeline::new_pixel(device, render_pass)?;
        let textured = dab::DabPipeline::new_textured(device, render_pass, atlas_set_layout)?;
        Ok(Self {
            pipelines: [round, pixel, textured],
        })
    }

    pub(super) const fn get(&self, family: DabFamily) -> &dab::DabPipeline {
        &self.pipelines[family.kind_index()]
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these pipelines is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            for p in self.pipelines {
                p.destroy(device);
            }
        }
    }
}
