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
//! one fragment shader, and the mask brush's feather pass (mask + brush
//! coverage in, blurred mask out).

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::pass::{
    FullscreenPass, allocate_sampler_set, linear_clamp_sampler, pipeline_layout, sampler_set_layout,
};
use super::resources::Image;
use super::targets::ImageTarget;

/// Linear factor between the mask resolution and the edges buffer. The
/// edges buffer trades resolution for a cheap readback; marching squares
/// runs on the smaller grid. 4 gives ants accurate to ~4 canvas pixels
/// which is below the typical zoom-out pixel-per-screen-pixel anyway.
pub(super) const EDGES_DOWNSAMPLE: u32 = 4;

const BLEND_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/selection_blend.frag.spv"));
const EDGES_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/selection_edges.frag.spv"));
const FEATHER_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/selection_feather.frag.spv"));
const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));

/// `vec2 step_uv + float sigma + float mix_scale`.
const FEATHER_PUSH_BYTES: u32 = 16;

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
    /// `dst + src * (1 - dst)` - src OVER dst. Unlike `Add` this compounds,
    /// so repeating it walks the mask toward fully selected: the mask brush
    /// builds up while you hold it somewhere.
    Accumulate,
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
    /// Reads from the brush stroke buffer, so the mask brush can blend its
    /// coverage in with the same pipelines a rasterised shape uses.
    pub stroke_descriptor_set: vk::DescriptorSet,

    /// Horizontal half of the blur brush's separable pass.
    pub feather_scratch: Image,
    pub feather_target: ImageTarget,
    /// Feather sets bind `(blur source, original mask, stroke coverage)`. The
    /// two passes differ only in the source: the mask copy, then its
    /// horizontally blurred form.
    pub feather_set_layout: vk::DescriptorSetLayout,
    pub feather_horizontal_set: vk::DescriptorSet,
    pub feather_vertical_set: vk::DescriptorSet,
    pub feather_layout: vk::PipelineLayout,
    pub feather_pipeline: vk::Pipeline,

    /// One pipeline per blend mode, all sharing the same fragment shader.
    /// Index by `SelectionBlendMode as usize`.
    pub blend_layout: vk::PipelineLayout,
    pub blend_pipelines: [vk::Pipeline; 5],

    pub edges_layout: vk::PipelineLayout,
    pub edges_pipeline: vk::Pipeline,

    pub edges_extent: vk::Extent2D,
}

impl SelectionResources {
    /// `stroke_view` is the brush stroke buffer (R8, canvas-sized); the mask
    /// brush blends its coverage straight out of it.
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        canvas_extent: vk::Extent2D,
        stroke_view: vk::ImageView,
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

        let feather_scratch = Image::new_2d(
            device,
            allocator,
            "selection-feather-scratch",
            format,
            canvas_extent,
            mask_usage,
            vk::ImageAspectFlags::COLOR,
        )?;

        let mask_target = ImageTarget::new(device, format, canvas_extent, mask.view)?;
        let edges_target = ImageTarget::new(device, format, edges_extent, edges.view)?;
        let feather_target =
            ImageTarget::new(device, format, canvas_extent, feather_scratch.view)?;

        let sampler = linear_clamp_sampler(device)?;
        let descriptor_set_layout = sampler_set_layout(device, 1)?;
        let descriptor_pool = create_descriptor_pool(device)?;
        let scratch_descriptor_set = allocate_sampler_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            &[scratch.view],
            sampler,
        )?;
        let mask_descriptor_set = allocate_sampler_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            &[mask.view],
            sampler,
        )?;
        let stroke_descriptor_set = allocate_sampler_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            &[stroke_view],
            sampler,
        )?;
        let feather_set_layout = sampler_set_layout(device, 3)?;
        let feather_horizontal_set = allocate_sampler_set(
            device,
            descriptor_pool,
            feather_set_layout,
            &[scratch.view, scratch.view, stroke_view],
            sampler,
        )?;
        let feather_vertical_set = allocate_sampler_set(
            device,
            descriptor_pool,
            feather_set_layout,
            &[feather_scratch.view, scratch.view, stroke_view],
            sampler,
        )?;
        let feather_layout = pipeline_layout(device, feather_set_layout, FEATHER_PUSH_BYTES)?;
        let feather_pipeline = create_feather_pipeline(device, feather_layout, mask_target.render_pass)?;

        let blend_layout = pipeline_layout(device, descriptor_set_layout, 0)?;
        let blend_pipelines = [
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Replace)?,
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Add)?,
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Subtract)?,
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Intersect)?,
            create_blend_pipeline(device, blend_layout, mask_target.render_pass, BlendMode::Accumulate)?,
        ];

        // Pure downsample - the shader takes no push data.
        let edges_layout = pipeline_layout(device, descriptor_set_layout, 0)?;
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
            stroke_descriptor_set,
            feather_scratch,
            feather_target,
            feather_set_layout,
            feather_horizontal_set,
            feather_vertical_set,
            feather_layout,
            feather_pipeline,
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
            device.destroy_pipeline(self.feather_pipeline, None);
            device.destroy_pipeline_layout(self.feather_layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.feather_set_layout, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            self.feather_target.destroy(device);
            self.edges_target.destroy(device);
            self.mask_target.destroy(device);
            self.feather_scratch.destroy(device, allocator);
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
    Accumulate,
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
            // out = src*(1-dst) + dst = dst + src*(1-dst)
            Self::Accumulate => base
                .src_color_blend_factor(vk::BlendFactor::ONE_MINUS_DST_COLOR)
                .dst_color_blend_factor(vk::BlendFactor::ONE)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_DST_ALPHA)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                .alpha_blend_op(vk::BlendOp::ADD),
        }
    }
}

/// Five sets: scratch, mask and stroke (one sampler each), plus the blur
/// brush's two passes (three each).
fn create_descriptor_pool(device: &Device) -> Result<vk::DescriptorPool, RendererError> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: 9,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(5);
    Ok(unsafe { device.create_descriptor_pool(&info, None)? })
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

fn create_feather_pipeline(
    device: &Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
) -> Result<vk::Pipeline, RendererError> {
    // Writes the mask outright (it carries the un-feathered pixels through).
    let attachment = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(false)
        .color_write_mask(vk::ColorComponentFlags::R);
    build_fullscreen_pipeline(
        device,
        layout,
        render_pass,
        COMPOSITE_VERT_SPV,
        FEATHER_FRAG_SPV,
        attachment,
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
    FullscreenPass {
        vert_spv,
        frag_spv,
        render_pass,
        layout,
        blend: blend_attachment,
    }
    .build(device)
}
