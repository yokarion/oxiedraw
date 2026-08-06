//! GPU pipelines for the Liquify tool.
//!
//! Two passes share one descriptor-set layout and one 16-byte push constant, so
//! a single pipeline layout covers both and the per-session descriptor sets can
//! be reused across them:
//!
//! | Pass      | Reads                    | Writes            |
//! | --------- | ------------------------ | ----------------- |
//! | `compose` | field, selection, dabs   | field (ping-pong) |
//! | `warp`    | layer snapshot, field    | BGRA canvas image |
//!
//! The field is [`FIELD_FORMAT`] (RG16F) rather than a 32-bit float pair
//! because linear filtering on 16-bit float formats is mandatory in Vulkan
//! while on 32-bit float formats it is not, and the compose pass samples the
//! field at fractional positions. The precision cost is an eighth of a pixel at
//! a 200px displacement, which is well under what the warp can resolve.

use ash::{Device, vk};

use super::RendererError;
use super::pass::{FullscreenPass, linear_clamp_sampler, nearest_clamp_sampler};

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const COMPOSE_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/liquify_compose.frag.spv"));
const WARP_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/liquify_warp.frag.spv"));

/// Displacement-field format: two half floats per canvas pixel, in canvas
/// pixels. See the module docs for why not RG32F.
pub(in crate::renderer) const FIELD_FORMAT: vk::Format = vk::Format::R16G16_SFLOAT;

/// One `vec4`: `(canvas_w, canvas_h, dab_count, selection_active)`.
pub(in crate::renderer) const PUSH_BYTES: u32 = 16;

/// Bytes per [`crate::liquify::LiquifyDab`] in the storage buffer (three
/// `vec4`s, std430).
pub(in crate::renderer) const DAB_STRIDE: u64 = 48;

/// Pipelines + layouts for Liquify. Size-independent, so one instance outlives
/// any number of tool sessions; built lazily because most sessions never
/// liquify anything.
pub(in crate::renderer) struct LiquifyPipelines {
    pub set_layout: vk::DescriptorSetLayout,
    pub pipeline_layout: vk::PipelineLayout,
    /// Single-attachment RG16F pass the field ping-pong renders through.
    pub field_render_pass: vk::RenderPass,
    pub compose: vk::Pipeline,
    pub warp: vk::Pipeline,
    /// Linear + clamp-to-edge, for the field and the layer snapshot.
    pub linear_sampler: vk::Sampler,
    /// Point-sampled, for the R8 selection mask.
    pub nearest_sampler: vk::Sampler,
}

impl LiquifyPipelines {
    pub(in crate::renderer) fn new(
        device: &Device,
        canvas_render_pass: vk::RenderPass,
    ) -> Result<Self, RendererError> {
        let set_layout = create_set_layout(device)?;
        let set_layouts = [set_layout];
        let push_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(PUSH_BYTES)];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_ranges);
        let pipeline_layout = unsafe { device.create_pipeline_layout(&layout_info, None)? };

        let field_render_pass = create_field_render_pass(device)?;

        let compose = build(
            device,
            COMPOSE_FRAG_SPV,
            field_render_pass,
            pipeline_layout,
            replace(vk::ColorComponentFlags::R | vk::ColorComponentFlags::G),
        )?;
        let warp = build(
            device,
            WARP_FRAG_SPV,
            canvas_render_pass,
            pipeline_layout,
            replace(vk::ColorComponentFlags::RGBA),
        )?;
        Ok(Self {
            set_layout,
            pipeline_layout,
            field_render_pass,
            compose,
            warp,
            linear_sampler: linear_clamp_sampler(device)?,
            nearest_sampler: nearest_clamp_sampler(device)?,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these pipelines is in flight.
    pub(in crate::renderer) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_pipeline(self.compose, None);
            device.destroy_pipeline(self.warp, None);
            device.destroy_render_pass(self.field_render_pass, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_sampler(self.linear_sampler, None);
            device.destroy_sampler(self.nearest_sampler, None);
        }
    }
}

/// Bindings 0 and 1 are combined image samplers, binding 2 is the dab storage
/// buffer. The two passes read different images through the same slots
/// (compose: field + selection; warp: snapshot + field), and the warp doesn't
/// read the dab buffer at all - a bound-but-unused binding is harmless, and
/// sharing one layout keeps both passes on one pipeline layout.
fn create_set_layout(device: &Device) -> Result<vk::DescriptorSetLayout, RendererError> {
    let sampler = |slot: u32| {
        vk::DescriptorSetLayoutBinding::default()
            .binding(slot)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
    };
    let bindings = [
        sampler(0),
        sampler(1),
        vk::DescriptorSetLayoutBinding::default()
            .binding(2)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
    ];
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    Ok(unsafe { device.create_descriptor_set_layout(&info, None)? })
}

/// The field ping-pong's render pass. `DONT_CARE` on load because the compose
/// pass writes every fragment in its scissor rect; anything outside is carried
/// forward by an explicit copy (see `liquify_ops`).
fn create_field_render_pass(device: &Device) -> Result<vk::RenderPass, RendererError> {
    let attachments = [vk::AttachmentDescription::default()
        .format(FIELD_FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::LOAD)
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

fn replace(mask: vk::ColorComponentFlags) -> vk::PipelineColorBlendAttachmentState {
    vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(mask)
        .blend_enable(false)
}

fn build(
    device: &Device,
    frag_spv: &[u8],
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
    blend: vk::PipelineColorBlendAttachmentState,
) -> Result<vk::Pipeline, RendererError> {
    FullscreenPass {
        vert_spv: COMPOSITE_VERT_SPV,
        frag_spv,
        render_pass,
        layout,
        blend,
    }
    .build(device)
}
