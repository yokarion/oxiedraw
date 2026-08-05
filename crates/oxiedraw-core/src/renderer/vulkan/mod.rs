//! `VulkanRenderer` struct, construction, teardown, and the private
//! command helpers shared by every operation.
//!
//! Public operations live in sibling modules:
//!
//! - `io` - host-visible reads / writes (canvas, layer, stroke, preview)
//! - `stroke` - per-stroke ops (`paint_dabs`, `stamp_mask`, `composite_stroke`)
//! - `layer_ops` - layer add / remove / reorder / clear and composites
//! - `preview` - the in-flight stroke preview path
//! - `present` - the dmabuf display path
//! - `transform_ops` - GPU affine transform applied to a single layer

mod adjust_ops;
mod fill_ops;
mod filter_ops;
mod gradient_ops;
mod io;
mod layer_ops;
mod pattern_ops;
mod present;
mod preview;
mod selection_ops;
mod smudge_ops;
mod shape_ops;
mod stroke;
mod transform_ops;
mod transform_preview;

pub use gradient_ops::GradientKind;
pub use shape_ops::ShapeKind;
pub use smudge_ops::SmudgeDab;

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use ash::{Device, Instance, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocator, AllocatorCreateDesc};
use oxiedraw_utils::geometry::Size;

use super::RendererError;
use super::composite::CompositePipeline;
use super::dab::DabBuffers;
use super::device;
use super::dmabuf::{DISPLAY_FORMAT, DmabufImage};
use super::erase::ErasePreview;
use super::present_convert::PresentConvertPipeline;
use adjust_ops::{GroupAccumulator, MaskEditPreview};
use super::fill_overlay::FillOverlayResources;
use super::filters::FilterResources;
use super::instance;
use super::layers::{LayerBlendPipeline, LayerCompositePipeline, LayerStack};
use transform_preview::TransformPreview;
use super::mask::{DabPipelineSet, MaskPipelineSet};
use super::pattern_atlas::PatternAtlas;
use super::resources::{Buffer, Image};
use super::selection::SelectionResources;
use super::gradient_overlay::GradientOverlayResources;
use super::shape_overlay::ShapeOverlayResources;
use super::targets::ImageTarget;
use super::transform::TransformPipeline;

/// Process-wide Vulkan core (instance + logical device) shared by every
/// `VulkanRenderer`.
///
/// A canvas resize replaces the renderer (`self.renderer = VulkanRenderer::new`).
/// When `new` also built a fresh `VkInstance`/`VkDevice`, GTK saw a new DRM
/// device fd on every resize and had to share the canvas dmabuf across two GPU
/// contexts - the cross-context implicit sync stalled and jittered the present
/// path, worst at a stylus's high event rate (mouse stayed under the budget).
/// Creating instance + device once and reusing them keeps the fd stable, so a
/// resize only rebuilds the size-dependent GPU resources on the same device.
/// The app is single-threaded (GTK main thread), so a thread-local is enough
/// and avoids Send/Sync bounds on the raw vk handles.
struct SharedVk {
    inst: instance::InstanceBundle,
    dev: device::DeviceBundle,
    max_image_dim: u32,
}

thread_local! {
    static SHARED_VK: RefCell<Option<Rc<SharedVk>>> = const { RefCell::new(None) };
}

/// Get the shared Vulkan core, creating it on first use. The handles live for
/// the process lifetime - intentionally never destroyed, since teardown would
/// race GTK's still-imported dmabuf textures and only matters at exit anyway.
fn shared_vk() -> Result<Rc<SharedVk>, RendererError> {
    SHARED_VK.with(|cell| {
        if let Some(s) = cell.borrow().as_ref() {
            return Ok(Rc::clone(s));
        }
        let inst = instance::create()?;
        let dev = device::create(&inst.instance)?;
        let max_image_dim = unsafe {
            inst.instance
                .get_physical_device_properties(dev.physical)
                .limits
                .max_image_dimension2_d
        };
        let shared = Rc::new(SharedVk {
            inst,
            dev,
            max_image_dim,
        });
        *cell.borrow_mut() = Some(Rc::clone(&shared));
        Ok(shared)
    })
}

/// Result of a `compute_selection_edges` + readback. Caller runs marching
/// squares on these bytes; the buffer is `width x height` R8.
#[derive(Debug, Clone)]
pub struct EdgesBuffer {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Canvas color format. sRGB so the GPU does linear / sRGB conversion on
/// reads and writes; shaders work in linear space.
///
/// BGRA byte order (rather than RGBA) so the readback bytes match
/// cairo's `Format::ARgb32` on little-endian and `gdk::MemoryFormat::
/// B8g8r8a8` for zero-copy display.
pub const CANVAS_FORMAT: vk::Format = vk::Format::B8G8R8A8_SRGB;
/// Stroke alpha-mask format. Single 8-bit channel, linear (no sRGB).
pub const STROKE_FORMAT: vk::Format = vk::Format::R8_UNORM;
/// Jump-flood coordinate-buffer format. Stores signed pixel offsets (two per
/// pixel: nearest inside + nearest outside), so it needs float channels with
/// range past the canvas, not the 8-bit sRGB canvas format.
pub(crate) const JFA_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// BGRA8 is 4 bytes per pixel. Used to size staging buffers and the
/// readback `Vec<u8>` returned to callers.
pub(super) const CANVAS_BYTES_PER_PIXEL: u64 = 4;
/// Command buffers / fences kept in flight. The CPU may run up to this many
/// submits ahead of the GPU before a `record_and_submit` has to wait.
pub(super) const RING_FRAMES: usize = 3;

/// Rotating display dmabuf images. The real stall fix is the synchronous present
/// (`wait_last` before handing the fd to GTK); the rotation only avoids a
/// write-while-GTK-reads hazard. Handing the compositor a *different* fd every
/// frame made wlroots/Hyprland pace our window at every-3rd-vsync (24ms) after a
/// resize, so we keep this small. 1 = a single stable buffer (like a normal
/// app); the sync present + frame-clock pacing keep it correct.
/// Raising this disables the clipped present in `record_present_copy`.
pub(super) const DISPLAY_BUFFERS: usize = 1;

/// What source the dmabuf display image should mirror.
#[derive(Debug, Clone, Copy)]
pub enum PresentSource {
    /// The committed canvas (no in-flight stroke).
    Canvas,
    /// The preview image (canvas + tinted stroke), populated by a
    /// previous `render_preview`-style call.
    Preview,
}

/// Headless Vulkan renderer.
///
/// Owns the canvas image, the in-flight stroke mask, a host-visible
/// staging buffer, and the per-frame command unit. No window, no
/// swapchain - output is read back to host memory or copied to the
/// dmabuf display image.
pub struct VulkanRenderer {
    pub(super) canvas_size: Size,

    pub(super) canvas: ManuallyDrop<Image>,
    pub(super) stroke: ManuallyDrop<Image>,
    /// Scratch image showing the in-flight stroke without committing
    /// to the canvas. Built by `render_preview` as `canvas + tinted(stroke)`.
    pub(super) preview: ManuallyDrop<Image>,
    /// Cached composite of the visible layers up to and including the
    /// active (stroke target) layer. Reused across every motion event of
    /// a single stroke - it only changes when the layer stack does. The
    /// stroke and the above-layers are re-composited on top each event.
    pub(super) preview_below: ManuallyDrop<Image>,
    pub(super) preview_below_framebuffer: vk::Framebuffer,
    /// False when `preview_below` must be rebuilt (start of a stroke or
    /// after any layer mutation).
    pub(super) preview_cache_valid: bool,
    /// Scratch resources for the eraser preview (target layer with the
    /// stroke punched out). Only touched while `stroke_erase` is true.
    pub(super) erase_preview: ManuallyDrop<ErasePreview>,
    /// True when the in-flight stroke erases (removes coverage from the
    /// target layer) instead of painting. Set per stroke at `begin_stroke`
    /// and read by the preview and commit paths.
    pub(super) stroke_erase: bool,
    /// AABB (canvas pixels, `min_x, min_y, max_x, max_y`) covering every
    /// dab quad stamped into the stroke buffer since the last
    /// `reset_stroke_dirty`. Lets `commit_stroke` build a tight history
    /// patch without a full-canvas readback + diff. `None` until the
    /// first dab of a stroke.
    pub(super) stroke_dirty: Option<(f32, f32, f32, f32)>,
    /// AABB of the dabs stamped since the last preview frame was presented.
    /// Drives the incremental preview: only this region is recomposited +
    /// copied to the display each frame instead of the whole canvas. Consumed
    /// (reset) by each preview build.
    pub(super) preview_pending_dirty: Option<(f32, f32, f32, f32)>,
    /// Forces the next preview frame to rebuild the whole canvas (stroke start
    /// or any layer mutation), after which incremental updates take over.
    pub(super) preview_needs_full: bool,
    /// Region the last present rewrote, or `None` for a full-canvas one.
    /// Handed to GTK as the dmabuf update region.
    pub(super) last_present_area: Option<vk::Rect2D>,
    /// When `Some`, `cmd_copy_image_full` / `cmd_begin_fullscreen_pass` /
    /// `record_present_copy` restrict their work to this canvas-pixel rect. Set
    /// only around the per-frame incremental preview update and reset right
    /// after, so every other path stays full-canvas.
    pub(super) clip: Option<vk::Rect2D>,
    pub(super) staging: ManuallyDrop<Buffer>,

    pub(super) canvas_target: ManuallyDrop<ImageTarget>,
    pub(super) stroke_target: ManuallyDrop<ImageTarget>,
    /// Framebuffer reusing `canvas_target.render_pass` with the preview
    /// image as its color attachment. Same format + extent makes the
    /// render pass compatible with both framebuffers.
    pub(super) preview_framebuffer: vk::Framebuffer,

    pub(super) dab_buffers: ManuallyDrop<DabBuffers>,
    pub(super) pattern_atlas: ManuallyDrop<PatternAtlas>,
    pub(super) dab_pipelines: ManuallyDrop<DabPipelineSet>,
    pub(super) mask_pipelines: ManuallyDrop<MaskPipelineSet>,
    /// Build-up variant: OVER-blend coverage so a build-up stroke
    /// accumulates in the stroke buffer and caps at the stroke opacity on
    /// the single final composite. Selected when `stroke_buildup` is set.
    pub(super) mask_pipelines_buildup: ManuallyDrop<MaskPipelineSet>,
    /// Whether the in-flight stroke uses build-up (OVER) mask blending.
    /// Set at stroke start; defaults to false (MAX blend).
    pub(super) stroke_buildup: bool,
    /// Cache mapping pattern data identity (raw `*const PatternData`)
    /// to its atlas slot. Lets `upload_pattern` no-op on re-uploads of
    /// the same `Rc<PatternData>`.
    pub(super) pattern_cache: std::collections::HashMap<usize, u32>,
    pub(super) composite_pipeline: ManuallyDrop<CompositePipeline>,
    pub(super) layer_composite_pipeline: ManuallyDrop<LayerCompositePipeline>,
    /// Blend pipeline (per-layer mode + opacity), driven by `cmd_compose_layer_blended`.
    pub(super) layer_blend_pipeline: ManuallyDrop<LayerBlendPipeline>,
    /// Canvas-sized scratch holding a copy of the current accumulator so the
    /// blend pass can sample the destination while writing it. Paired with
    /// `blend_scratch_dst_set` (binds its view as the blend pipeline's set 1).
    pub(super) blend_scratch: ManuallyDrop<Image>,
    pub(super) blend_scratch_dst_set: vk::DescriptorSet,
    pub(super) blend_descriptor_pool: vk::DescriptorPool,
    /// Lazily-grown pool of canvas-sized sub-accumulators, one per folder
    /// nesting level, used by the folder-scoped composite so an adjustment
    /// clips to its enclosing folder. Indexed by depth (0 = first folder).
    pub(super) group_accumulators: Vec<GroupAccumulator>,
    /// Per-stroke cache of finished folder composites that do NOT contain the
    /// stroke target, keyed by the folder's pre-order ordinal in the scoped step
    /// stream. A folder's interior is isolated (it never reads the parent
    /// backdrop), so when it doesn't contain the target its result is constant
    /// for the whole stroke: built once, then re-blended each frame instead of
    /// re-running its (often expensive) effect chain. Invalidated whenever
    /// `preview_cache_valid` is.
    pub(super) scoped_group_cache: Vec<GroupAccumulator>,
    pub(super) scoped_cache_valid: bool,
    /// Set only while building a live mask-edit preview: the adjustment slot
    /// whose mask the in-flight stroke is painting. `apply_adjustment_to` runs
    /// that slot's effect against the committed mask MERGED with the stroke, so
    /// the canvas shows the effect updating live instead of the grayscale mask.
    pub(super) mask_edit: Option<MaskEditPreview>,
    pub(super) transform_pipeline: ManuallyDrop<TransformPipeline>,
    /// Reusable resources for the live GPU transform preview, present only
    /// while the transform tool is dragging.
    pub(super) transform_preview: Option<TransformPreview>,
    pub(super) layer_stack: ManuallyDrop<LayerStack>,
    pub(super) selection: ManuallyDrop<SelectionResources>,
    /// Whether the selection mask currently holds a real selection. When
    /// false, the composite shader's `selection_active` push constant is
    /// 0 and the mask is ignored. Composite paths that pass through the
    /// stroke pipeline read this flag.
    pub(super) selection_active: bool,

    pub(super) fill_overlay: ManuallyDrop<FillOverlayResources>,
    /// True when a bucket-fill animation is in flight. Gates the preview
    /// path so it overlays the fill at the active layer's z-order.
    pub(super) fill_active: bool,
    /// Current reveal radius in normalised mask space (0.0..1.0).
    pub(super) fill_reveal: f32,
    /// Premultiplied fill colour pushed to the overlay shader each frame.
    pub(super) fill_color_premul: [f32; 4],
    /// Layer index the fill is being applied to (used to splice the
    /// overlay in at the right z-order during the preview composite).
    pub(super) fill_layer_idx: usize,
    /// True when the fill went underneath the layer's existing pixels
    /// rather than over them, so hiding it during the reveal means
    /// taking its share back out (DST_OUT) rather than painting the seed
    /// colour over it.
    pub(super) fill_behind: bool,

    pub(super) shape_overlay: ManuallyDrop<ShapeOverlayResources>,
    /// True while a shape drag is in flight. Gates the preview path so
    /// the shape is composited at `shape_layer_idx`'s z-order each frame.
    pub(super) shape_active: bool,
    pub(super) shape_layer_idx: usize,
    /// Push-constant buffers for the shape overlay: color (premul),
    /// rect (box or line endpoints), extra (kind/aa/line_width/sel_active).
    pub(super) shape_color_premul: [f32; 4],
    pub(super) shape_rect: [f32; 4],
    pub(super) shape_extra: [f32; 4],

    pub(super) gradient_overlay: ManuallyDrop<GradientOverlayResources>,
    /// True while a gradient drag is in flight. Gates the preview path so
    /// the ramp is composited at `gradient_layer_idx`'s z-order each frame.
    pub(super) gradient_active: bool,
    pub(super) gradient_layer_idx: usize,
    /// Push-constant buffers for the gradient overlay: endpoints (x0,y0,x1,y1)
    /// and extra (kind/sel_active/_/_).
    pub(super) gradient_endpoints: [f32; 4],
    pub(super) gradient_extra: [f32; 4],

    pub(super) filter_resources: ManuallyDrop<FilterResources>,
    /// True while a filter popup is open. Gates the preview path so the
    /// affected layers are composited through the filter pipeline.
    pub(super) filter_active: bool,
    /// The filter + parameters being previewed (set by `begin_filter` /
    /// `update_filter_spec`).
    pub(super) filter_spec: crate::filters::FilterSpec,
    /// Layer indices the filter applies to (z-order independent).
    pub(super) filter_affected: Vec<usize>,

    /// Colour-smudge dab pipeline `(layout, pipeline)`, built lazily on first
    /// smudge stroke (most sessions never use it). Samples `blend_scratch`
    /// (a per-dab copy of the target layer) + `smudge_before`, and deposits the
    /// dragged colour lerped from the pre-stroke layer by opacity.
    pub(super) smudge_pipeline: Option<(vk::PipelineLayout, vk::Pipeline)>,
    /// Pre-stroke snapshot of the smudged layer `(image, pool, set)`, taken at
    /// stroke start; the dab shader lerps from it so opacity is a ceiling.
    /// Lazily allocated on first smudge stroke.
    pub(super) smudge_before: Option<(ManuallyDrop<Image>, vk::DescriptorPool, vk::DescriptorSet)>,

    /// Display-side dmabuf image. Per-frame `present_to_display` copies
    /// the chosen source (canvas or preview) into here.
    /// Rotating pool of display dmabuf images (see [`DISPLAY_BUFFERS`]). Each
    /// present advances `display_cursor` and writes that buffer; GTK reads the
    /// one we just wrote while the next present targets a different buffer.
    pub(super) display: Vec<DmabufImage>,
    pub(super) display_cursor: usize,
    /// Present-time colour-space conversion (premultiplied-linear canvas ->
    /// premultiplied-gamma display) so GTK's sRGB-space checker composite is
    /// correct for semi-transparent pixels.
    pub(super) present_convert: ManuallyDrop<PresentConvertPipeline>,
    /// One framebuffer per `display` buffer, targeting that dmabuf's view with
    /// the present-convert render pass.
    pub(super) display_framebuffers: Vec<vk::Framebuffer>,

    pub(super) command_pool: vk::CommandPool,
    /// Ring of command buffers + fences for frames-in-flight: `record_and_submit`
    /// cycles through them so a submit doesn't have to fence-wait before the CPU
    /// continues. `command_buffer` / `fence` always point at the slot the current
    /// `record_and_submit` is using, so the cmd-recording helpers and closures
    /// keep working unchanged.
    pub(super) ring_cmds: Vec<vk::CommandBuffer>,
    pub(super) ring_fences: Vec<vk::Fence>,
    pub(super) ring_cursor: usize,
    /// Slot index of the most recent submit (for blocking waits / timestamp readback).
    pub(super) last_slot: usize,
    /// Timestamp query pool: 3 timestamps per ring slot (frame start, after the
    /// preview render, after the present copy) for the perf overlay.
    pub(super) timestamp_pool: vk::QueryPool,
    pub(super) timestamp_period: f32,
    /// Ring slot of the most recent frame that recorded timestamps.
    pub(super) frame_timing_slot: Option<usize>,
    pub(super) command_buffer: vk::CommandBuffer,
    pub(super) fence: vk::Fence,

    pub(super) queue: vk::Queue,
    #[allow(dead_code)]
    pub(super) queue_family: u32,

    pub(super) allocator: ManuallyDrop<Allocator>,
    pub(super) device: Device,

    #[allow(dead_code)]
    pub(super) instance: Instance,
    #[allow(dead_code)]
    pub(super) physical_device: vk::PhysicalDevice,
    pub(super) device_name: String,
    /// `VkPhysicalDeviceLimits::maxImageDimension2D` - caps the transform
    /// render target size. Beyond this `vkCreateImage` fails.
    pub(super) max_image_dim: u32,
    /// Keeps the shared instance/device alive for this renderer's lifetime.
    _shared: Rc<SharedVk>,
}

impl VulkanRenderer {
    #[allow(clippy::too_many_lines)]
    pub fn new(canvas_size: Size) -> Result<Self, RendererError> {
        let shared = shared_vk()?;
        let inst = &shared.inst;
        let dev = &shared.dev;
        let max_image_dim = shared.max_image_dim;

        let mut allocator = Allocator::new(&AllocatorCreateDesc {
            instance: inst.instance.clone(),
            device: dev.device.clone(),
            physical_device: dev.physical,
            debug_settings: gpu_allocator::AllocatorDebugSettings::default(),
            buffer_device_address: false,
            allocation_sizes: gpu_allocator::AllocationSizes::default(),
        })?;

        let extent = vk::Extent2D {
            width: canvas_size.width,
            height: canvas_size.height,
        };
        let image_usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED;
        let canvas = Image::new_2d(
            &dev.device,
            &mut allocator,
            "canvas",
            CANVAS_FORMAT,
            extent,
            image_usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let stroke = Image::new_2d(
            &dev.device,
            &mut allocator,
            "stroke",
            STROKE_FORMAT,
            extent,
            image_usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let preview = Image::new_2d(
            &dev.device,
            &mut allocator,
            "preview",
            CANVAS_FORMAT,
            extent,
            image_usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let preview_below = Image::new_2d(
            &dev.device,
            &mut allocator,
            "preview-below",
            CANVAS_FORMAT,
            extent,
            image_usage,
            vk::ImageAspectFlags::COLOR,
        )?;

        let staging_size = canvas_size.area() * CANVAS_BYTES_PER_PIXEL;
        let staging = Buffer::new(
            &dev.device,
            &mut allocator,
            "canvas-readback",
            staging_size,
            vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::GpuToCpu,
        )?;

        let canvas_target = ImageTarget::new(&dev.device, CANVAS_FORMAT, extent, canvas.view)?;
        let stroke_target = ImageTarget::new(&dev.device, STROKE_FORMAT, extent, stroke.view)?;
        let preview_framebuffer = create_framebuffer_for_view(
            &dev.device,
            canvas_target.render_pass,
            extent,
            preview.view,
        )?;
        let preview_below_framebuffer = create_framebuffer_for_view(
            &dev.device,
            canvas_target.render_pass,
            extent,
            preview_below.view,
        )?;
        let dab_buffers = DabBuffers::new(&dev.device, &mut allocator)?;
        let pattern_atlas = PatternAtlas::new(&dev.device, &mut allocator)?;
        let dab_pipelines = DabPipelineSet::new(
            &dev.device,
            canvas_target.render_pass,
            pattern_atlas.descriptor_set_layout(),
        )?;
        let mask_pipelines = MaskPipelineSet::new(
            &dev.device,
            stroke_target.render_pass,
            pattern_atlas.descriptor_set_layout(),
        )?;
        let mask_pipelines_buildup = MaskPipelineSet::new_buildup(
            &dev.device,
            stroke_target.render_pass,
            pattern_atlas.descriptor_set_layout(),
        )?;
        let selection = SelectionResources::new(&dev.device, &mut allocator, extent)?;
        let fill_overlay = FillOverlayResources::new(
            &dev.device,
            &mut allocator,
            extent,
            canvas_target.render_pass,
        )?;
        let shape_overlay = ShapeOverlayResources::new(
            &dev.device,
            canvas_target.render_pass,
            selection.mask.view,
        )?;
        let gradient_overlay = GradientOverlayResources::new(
            &dev.device,
            &mut allocator,
            canvas_target.render_pass,
            selection.mask.view,
        )?;
        let composite_pipeline = CompositePipeline::new(
            &dev.device,
            canvas_target.render_pass,
            stroke.view,
            selection.mask.view,
        )?;
        let layer_composite_pipeline =
            LayerCompositePipeline::new(&dev.device, canvas_target.render_pass)?;
        let layer_blend_pipeline = LayerBlendPipeline::new(
            &dev.device,
            canvas_target.render_pass,
            layer_composite_pipeline.descriptor_set_layout,
        )?;
        let blend_scratch = Image::new_2d(
            &dev.device,
            &mut allocator,
            "blend-scratch",
            CANVAS_FORMAT,
            extent,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        let (blend_descriptor_pool, blend_scratch_dst_set) = create_sampled_image_set(
            &dev.device,
            layer_composite_pipeline.descriptor_set_layout,
            layer_composite_pipeline.sampler,
            blend_scratch.view,
        )?;
        let erase_preview = ErasePreview::new(
            &dev.device,
            &mut allocator,
            extent,
            canvas_target.render_pass,
            layer_composite_pipeline.descriptor_set_layout,
            layer_composite_pipeline.sampler,
        )?;
        let filter_resources = FilterResources::new(
            &dev.device,
            &mut allocator,
            extent,
            canvas_target.render_pass,
            layer_composite_pipeline.descriptor_set_layout,
            layer_composite_pipeline.sampler,
        )?;
        let transform_pipeline = TransformPipeline::new(&dev.device, canvas_target.render_pass)?;
        let layer_stack = LayerStack::new(&dev.device)?;
        let display = (0..DISPLAY_BUFFERS)
            .map(|_| {
                DmabufImage::new(
                    &inst.instance,
                    dev.physical,
                    &dev.device,
                    &dev.external_memory_fd,
                    canvas_size.width,
                    canvas_size.height,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;

        let present_convert = PresentConvertPipeline::new(&dev.device, DISPLAY_FORMAT)?;
        let display_framebuffers = display
            .iter()
            .map(|buf| {
                let views = [buf.view];
                let fb_info = vk::FramebufferCreateInfo::default()
                    .render_pass(present_convert.render_pass)
                    .attachments(&views)
                    .width(canvas_size.width)
                    .height(canvas_size.height)
                    .layers(1);
                unsafe { dev.device.create_framebuffer(&fb_info, None) }
            })
            .collect::<Result<Vec<_>, _>>()?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(dev.queue_family);
        let command_pool = unsafe { dev.device.create_command_pool(&pool_info, None)? };
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(RING_FRAMES as u32);
        let ring_cmds = unsafe { dev.device.allocate_command_buffers(&alloc_info)? };
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        let ring_fences = (0..RING_FRAMES)
            .map(|_| unsafe { dev.device.create_fence(&fence_info, None) })
            .collect::<Result<Vec<_>, _>>()?;
        let command_buffer = ring_cmds[0];
        let fence = ring_fences[0];

        let ts_info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(RING_FRAMES as u32 * 3);
        let timestamp_pool = unsafe { dev.device.create_query_pool(&ts_info, None)? };

        let mut renderer = Self {
            canvas_size,
            canvas: ManuallyDrop::new(canvas),
            stroke: ManuallyDrop::new(stroke),
            preview: ManuallyDrop::new(preview),
            preview_below: ManuallyDrop::new(preview_below),
            preview_below_framebuffer,
            preview_cache_valid: false,
            erase_preview: ManuallyDrop::new(erase_preview),
            stroke_erase: false,
            stroke_dirty: None,
            preview_pending_dirty: None,
            last_present_area: None,
            preview_needs_full: true,
            clip: None,
            staging: ManuallyDrop::new(staging),
            canvas_target: ManuallyDrop::new(canvas_target),
            stroke_target: ManuallyDrop::new(stroke_target),
            preview_framebuffer,
            dab_buffers: ManuallyDrop::new(dab_buffers),
            pattern_atlas: ManuallyDrop::new(pattern_atlas),
            dab_pipelines: ManuallyDrop::new(dab_pipelines),
            mask_pipelines: ManuallyDrop::new(mask_pipelines),
            mask_pipelines_buildup: ManuallyDrop::new(mask_pipelines_buildup),
            stroke_buildup: false,
            pattern_cache: std::collections::HashMap::new(),
            composite_pipeline: ManuallyDrop::new(composite_pipeline),
            layer_composite_pipeline: ManuallyDrop::new(layer_composite_pipeline),
            layer_blend_pipeline: ManuallyDrop::new(layer_blend_pipeline),
            blend_scratch: ManuallyDrop::new(blend_scratch),
            blend_scratch_dst_set,
            group_accumulators: Vec::new(),
            scoped_group_cache: Vec::new(),
            scoped_cache_valid: false,
            mask_edit: None,
            blend_descriptor_pool,
            transform_pipeline: ManuallyDrop::new(transform_pipeline),
            transform_preview: None,
            layer_stack: ManuallyDrop::new(layer_stack),
            selection: ManuallyDrop::new(selection),
            selection_active: false,
            fill_overlay: ManuallyDrop::new(fill_overlay),
            fill_active: false,
            fill_reveal: 0.0,
            fill_color_premul: [0.0; 4],
            fill_layer_idx: 0,
            fill_behind: false,
            shape_overlay: ManuallyDrop::new(shape_overlay),
            shape_active: false,
            shape_layer_idx: 0,
            shape_color_premul: [0.0; 4],
            shape_rect: [0.0; 4],
            shape_extra: [0.0; 4],
            gradient_overlay: ManuallyDrop::new(gradient_overlay),
            gradient_active: false,
            gradient_layer_idx: 0,
            gradient_endpoints: [0.0; 4],
            gradient_extra: [0.0; 4],
            filter_resources: ManuallyDrop::new(filter_resources),
            filter_active: false,
            filter_spec: crate::filters::FilterSpec::Invert,
            filter_affected: Vec::new(),
            smudge_pipeline: None,
            smudge_before: None,
            display,
            present_convert: ManuallyDrop::new(present_convert),
            display_framebuffers,
            // First present rotates to 0, so buffer 0 is written first.
            display_cursor: DISPLAY_BUFFERS - 1,
            command_pool,
            ring_cmds,
            ring_fences,
            ring_cursor: 0,
            last_slot: 0,
            timestamp_pool,
            timestamp_period: dev.timestamp_period,
            frame_timing_slot: None,
            command_buffer,
            fence,
            queue: dev.queue,
            queue_family: dev.queue_family,
            allocator: ManuallyDrop::new(allocator),
            device: dev.device.clone(),
            instance: inst.instance.clone(),
            physical_device: dev.physical,
            device_name: dev.device_name.clone(),
            max_image_dim,
            _shared: Rc::clone(&shared),
        };

        renderer.transition_to_resting()?;

        tracing::info!(
            device = %renderer.device_name,
            w = canvas_size.width,
            h = canvas_size.height,
            "Vulkan renderer initialized",
        );
        Ok(renderer)
    }

    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    #[must_use]
    pub const fn canvas_size(&self) -> Size {
        self.canvas_size
    }

    /// Largest dimension (width or height) a GPU image can have on this
    /// device. The transform render target is bounded by this. Typical
    /// values: 16384 (AMD / Intel) or 32768 (NVIDIA).
    #[must_use]
    pub const fn max_image_dim(&self) -> u32 {
        self.max_image_dim
    }

    fn transition_to_resting(&mut self) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            let barriers = [
                full_image_barrier(
                    this.canvas.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.stroke.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.preview.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.preview_below.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.selection.mask.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.selection.scratch.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.selection.edges.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.fill_overlay.mask.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.gradient_overlay.lut.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.filter_resources.scratch_a.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.filter_resources.scratch_b.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.filter_resources.coord_a.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.filter_resources.coord_b.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.erase_preview.scratch.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
                full_image_barrier(
                    this.blend_scratch.handle,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                ),
            ];
            unsafe {
                this.device.cmd_pipeline_barrier(
                    this.command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barriers,
                );
            }
            // Pattern atlas wants SHADER_READ_ONLY_OPTIMAL, not GENERAL,
            // so it's a separate transition.
            this.pattern_atlas.cmd_prime_layout(&this.device, this.command_buffer);
            Ok(())
        })
    }

    // ------------------------------------------------------------------
    // Private command-recording helpers shared across sibling modules.
    // ------------------------------------------------------------------

    pub(super) fn barrier(&self, image: vk::Image, old: vk::ImageLayout, new: vk::ImageLayout) {
        let b = full_image_barrier(image, old, new);
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[b],
            );
        }
    }

    /// `canvas.extent` as a 2-D extent (render passes want `Extent2D`).
    pub(super) fn canvas_extent_2d(&self) -> vk::Extent2D {
        vk::Extent2D {
            width: self.canvas.extent.width,
            height: self.canvas.extent.height,
        }
    }

    /// Inverse canvas dimensions for the dab vertex push-constant
    /// (`[2/w, 2/h]` converts pixel coords to NDC).
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn canvas_inv_size(&self) -> [f32; 2] {
        let e = self.canvas.extent;
        [2.0 / e.width as f32, 2.0 / e.height as f32]
    }

    /// Clear `image` from `GENERAL -> TRANSFER_DST -> GENERAL`. Caller
    /// must be inside a `record_and_submit` closure.
    pub(super) fn cmd_clear_image(&self, image: vk::Image, color: [f32; 4]) {
        self.barrier(
            image,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        );
        let clear = vk::ClearColorValue { float32: color };
        unsafe {
            self.device.cmd_clear_color_image(
                self.command_buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &clear,
                &[full_subresource_range()],
            );
        }
        self.barrier(
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::GENERAL,
        );
    }

    /// Mark the cached `preview_below` stale so the next in-flight stroke
    /// preview rebuilds it. Called at stroke start and after any layer
    /// mutation.
    pub fn invalidate_preview_cache(&mut self) {
        self.preview_cache_valid = false;
        self.scoped_cache_valid = false;
        // The below-stack changed, so the next preview frame must rebuild the
        // whole canvas before incremental updates resume.
        self.preview_needs_full = true;
        self.preview_pending_dirty = None;
    }

    /// The clip rect for the next incremental preview frame: the pending dab
    /// region clamped to the canvas, or `None` (rebuild the whole canvas) when a
    /// full frame is forced or nothing was stamped. Consumes the pending state.
    pub(super) fn take_preview_clip(&mut self) -> Option<vk::Rect2D> {
        if self.preview_needs_full {
            self.preview_needs_full = false;
            self.preview_pending_dirty = None;
            return None;
        }
        let dirty = self.preview_pending_dirty.take();
        let (min_x, min_y, max_x, max_y) = dirty?;
        #[allow(clippy::cast_precision_loss)]
        let (cw, ch) = (self.canvas.extent.width as f32, self.canvas.extent.height as f32);
        let x0 = min_x.floor().clamp(0.0, cw);
        let y0 = min_y.floor().clamp(0.0, ch);
        let x1 = max_x.ceil().clamp(0.0, cw);
        let y1 = max_y.ceil().clamp(0.0, ch);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(vk::Rect2D {
            offset: vk::Offset2D {
                x: x0 as i32,
                y: y0 as i32,
            },
            extent: vk::Extent2D {
                width: (x1 - x0) as u32,
                height: (y1 - y0) as u32,
            },
        })
    }

    /// Copy one canvas-sized image into another. Both transition
    /// `GENERAL -> TRANSFER -> GENERAL`. Caller must be inside a
    /// `record_and_submit` closure.
    pub(super) fn cmd_copy_image_full(&self, src: vk::Image, dst: vk::Image) {
        self.barrier(
            src,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        );
        self.barrier(
            dst,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        );
        let subresource = vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        // Honor an active clip rect so the incremental preview copies only the
        // dirty region; otherwise copy the whole canvas.
        let (offset, extent) = self.clip.map_or_else(
            || (vk::Offset3D::default(), self.canvas.extent),
            |r| {
                (
                    vk::Offset3D {
                        x: r.offset.x,
                        y: r.offset.y,
                        z: 0,
                    },
                    vk::Extent3D {
                        width: r.extent.width,
                        height: r.extent.height,
                        depth: 1,
                    },
                )
            },
        );
        let copy = vk::ImageCopy::default()
            .src_subresource(subresource)
            .src_offset(offset)
            .dst_subresource(subresource)
            .dst_offset(offset)
            .extent(extent);
        unsafe {
            self.device.cmd_copy_image(
                self.command_buffer,
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            );
        }
        self.barrier(
            src,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::GENERAL,
        );
        self.barrier(
            dst,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::GENERAL,
        );
    }

    /// Begin a fullscreen (canvas-sized) render pass and bind `pipeline`.
    /// Pair with `cmd_end_fullscreen_pass`.
    pub(super) fn cmd_begin_fullscreen_pass(
        &self,
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        pipeline: vk::Pipeline,
    ) {
        let extent = self.canvas_extent_2d();
        #[allow(clippy::cast_precision_loss)]
        let viewport = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        // The viewport stays full-canvas (the fullscreen triangle's UVs must map
        // to canvas pixels); the clip rect only narrows the scissor + render
        // area so fragments outside the dirty region are not touched.
        let area = self.clip.unwrap_or_else(|| vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        });
        let scissor = area;
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(render_pass)
            .framebuffer(framebuffer)
            .render_area(area);
        unsafe {
            self.device.cmd_begin_render_pass(
                self.command_buffer,
                &begin,
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline,
            );
            self.device
                .cmd_set_viewport(self.command_buffer, 0, &[viewport]);
            self.device
                .cmd_set_scissor(self.command_buffer, 0, &[scissor]);
        }
    }

    /// Composite one premultiplied BGRA layer (via `src_set`) onto the
    /// accumulator image `acc_img` (rendered through `acc_fb`) using the
    /// layer's blend `mode` + `opacity`. Because the blend math needs to read
    /// the destination while writing it, the accumulator is first copied into
    /// `blend_scratch`, which the blend pass samples as its second input.
    /// Caller must be inside a `record_and_submit` closure.
    /// Composite one premultiplied BGRA image (via `descriptor_set`) onto
    /// `framebuffer` with the layer-composite pipeline (premultiplied OVER).
    pub(super) fn cmd_compose_image(
        &self,
        framebuffer: vk::Framebuffer,
        descriptor_set: vk::DescriptorSet,
    ) {
        let render_pass = self.canvas_target.render_pass;
        let pipeline = self.layer_composite_pipeline.pipeline;
        let layout = self.layer_composite_pipeline.layout;
        self.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[descriptor_set],
                &[],
            );
        }
        self.cmd_end_fullscreen_pass();
    }

    pub(super) fn cmd_compose_layer_blended(
        &self,
        acc_img: vk::Image,
        acc_fb: vk::Framebuffer,
        src_set: vk::DescriptorSet,
        mode: u32,
        opacity: f32,
    ) {
        // Normal at full opacity is plain premultiplied OVER, which the
        // fixed-function blend pipeline does without reading the destination.
        // Skip the scratch copy + extra pass on this (overwhelmingly common)
        // path.
        if mode == 0 && opacity >= 1.0 {
            self.cmd_compose_image(acc_fb, src_set);
            self.barrier(acc_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            return;
        }
        // blend_scratch := current accumulator (the destination).
        self.cmd_copy_image_full(acc_img, self.blend_scratch.handle);

        let render_pass = self.canvas_target.render_pass;
        let pipeline = self.layer_blend_pipeline.pipeline;
        let layout = self.layer_blend_pipeline.layout;
        self.cmd_begin_fullscreen_pass(render_pass, acc_fb, pipeline);
        let sets = [src_set, self.blend_scratch_dst_set];
        let mut push = [0u8; 8];
        push[0..4].copy_from_slice(&mode.to_ne_bytes());
        push[4..8].copy_from_slice(&opacity.clamp(0.0, 1.0).to_ne_bytes());
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &sets,
                &[],
            );
            self.device.cmd_push_constants(
                self.command_buffer,
                layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                &push,
            );
        }
        self.cmd_end_fullscreen_pass();
        // Make this layer's write visible to the next composite's sampler.
        self.barrier(acc_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
    }

    /// Draw the fullscreen triangle (3 vertices, 1 instance) and end the
    /// render pass started by `cmd_begin_fullscreen_pass`.
    pub(super) fn cmd_end_fullscreen_pass(&self) {
        unsafe {
            self.device.cmd_draw(self.command_buffer, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(self.command_buffer);
        }
    }

    /// Record a one-shot command buffer into the next ring slot and submit it.
    /// Does NOT wait. Private primitive shared by the blocking + async wrappers.
    fn submit_to_ring<F>(&mut self, record: F) -> Result<(), RendererError>
    where
        F: FnOnce(&mut Self) -> Result<(), RendererError>,
    {
        let slot = self.ring_cursor;
        self.command_buffer = self.ring_cmds[slot];
        self.fence = self.ring_fences[slot];
        // The slot's previous submission must finish before we reuse its command
        // buffer. Fences start signaled, so the first use is a no-op.
        unsafe { self.device.wait_for_fences(&[self.fence], true, u64::MAX)? };
        unsafe { self.device.reset_fences(&[self.fence])? };
        unsafe {
            self.device
                .reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())?;
        }
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin)?;
        }

        record(self)?;

        unsafe { self.device.end_command_buffer(self.command_buffer)? };
        let cbs = [self.command_buffer];
        let submit = vk::SubmitInfo::default().command_buffers(&cbs);
        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], self.fence)?;
        }
        self.last_slot = slot;
        self.ring_cursor = (slot + 1) % RING_FRAMES;
        Ok(())
    }

    /// Record, submit, and **block** until the GPU finishes. The default for all
    /// paths - safe for descriptor-rewrite-between-passes and CPU readback.
    pub(super) fn record_and_submit<F>(&mut self, record: F) -> Result<(), RendererError>
    where
        F: FnOnce(&mut Self) -> Result<(), RendererError>,
    {
        self.submit_to_ring(record)?;
        self.wait_last()
    }

    /// Record + submit and return WITHOUT waiting (frames-in-flight). Use ONLY on
    /// the hot drawing path (stamp / preview composite / present), which binds
    /// fixed per-slot descriptors - no shared set is rewritten between submits,
    /// so same-queue submission order keeps it correct.
    pub(super) fn record_and_submit_async<F>(&mut self, record: F) -> Result<(), RendererError>
    where
        F: FnOnce(&mut Self) -> Result<(), RendererError>,
    {
        self.submit_to_ring(record)
    }

    /// Slot the next `submit_to_ring` will record into.
    pub(super) const fn current_ring_slot(&self) -> usize {
        self.ring_cursor
    }

    /// Wait for slot `slot`'s last submission to finish so its instance region
    /// is safe to overwrite. submit_to_ring waits too, but only after the upload.
    pub(super) fn wait_ring_slot(&self, slot: usize) -> Result<(), RendererError> {
        unsafe {
            self.device
                .wait_for_fences(&[self.ring_fences[slot]], true, u64::MAX)?;
        }
        Ok(())
    }

    /// Wait for the most recent submission to finish.
    pub fn wait_last(&self) -> Result<(), RendererError> {
        unsafe {
            self.device
                .wait_for_fences(&[self.ring_fences[self.last_slot]], true, u64::MAX)?;
        }
        Ok(())
    }

    /// Poll (non-blocking) whether the most recent submission has finished.
    #[must_use]
    pub fn last_submit_done(&self) -> bool {
        unsafe {
            self.device
                .get_fence_status(self.ring_fences[self.last_slot])
                .unwrap_or(true)
        }
    }

    // -- Frame GPU timestamps (perf overlay) --------------------------------
    // Three timestamps per ring slot: [0] frame start, [1] after the preview
    // render, [2] after the present copy. Recorded inside the frame command
    // buffer; read back (non-blocking) one or more frames later.

    fn timing_base(&self) -> u32 {
        self.ring_cursor as u32 * 3
    }

    /// Reset this slot's queries and stamp the frame-start timestamp. Call at the
    /// very start of a frame command buffer (outside any render pass).
    pub(super) fn cmd_frame_timing_begin(&self) {
        if self.timestamp_period <= 0.0 {
            return;
        }
        let base = self.timing_base();
        unsafe {
            self.device
                .cmd_reset_query_pool(self.command_buffer, self.timestamp_pool, base, 3);
            self.device.cmd_write_timestamp(
                self.command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                self.timestamp_pool,
                base,
            );
        }
    }

    /// Stamp timestamp `n` (1 = after render, 2 = after present) of this frame.
    pub(super) fn cmd_frame_timing_mark(&self, n: u32) {
        if self.timestamp_period <= 0.0 {
            return;
        }
        unsafe {
            self.device.cmd_write_timestamp(
                self.command_buffer,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.timestamp_pool,
                self.timing_base() + n,
            );
        }
    }

    /// Mark the just-submitted frame as the one to read timings from next.
    pub(super) fn note_frame_timing(&mut self) {
        if self.timestamp_period > 0.0 {
            self.frame_timing_slot = Some(self.last_slot);
        }
    }

    /// Non-blocking read of the most recent timestamped frame's GPU durations
    /// `(render_ms, present_ms)`. `None` until the results are available.
    #[must_use]
    pub fn poll_frame_timings(&self) -> Option<(f32, f32)> {
        let slot = self.frame_timing_slot?;
        let base = slot as u32 * 3;
        let mut data = [0u64; 3];
        let ok = unsafe {
            self.device.get_query_pool_results(
                self.timestamp_pool,
                base,
                &mut data,
                vk::QueryResultFlags::TYPE_64,
            )
        };
        if ok.is_err() {
            return None; // NOT_READY or error
        }
        // timestamp_period is ns/tick; convert tick deltas to milliseconds.
        let to_ms =
            |ticks: u64| (ticks as f64 * f64::from(self.timestamp_period) / 1_000_000.0) as f32;
        let render = to_ms(data[1].saturating_sub(data[0]));
        let present = to_ms(data[2].saturating_sub(data[1]));
        Some((render, present))
    }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        self.clear_transform_preview_gpu();
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_query_pool(self.timestamp_pool, None);
            for &f in &self.ring_fences {
                self.device.destroy_fence(f, None);
            }
            self.device.destroy_command_pool(self.command_pool, None);
            ManuallyDrop::take(&mut self.layer_stack).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.transform_pipeline).destroy(&self.device);
            self.device
                .destroy_descriptor_pool(self.blend_descriptor_pool, None);
            ManuallyDrop::take(&mut self.blend_scratch).destroy(&self.device, &mut self.allocator);
            for mut ga in self.group_accumulators.drain(..) {
                ga.destroy(&self.device, &mut self.allocator);
            }
            for mut ga in self.scoped_group_cache.drain(..) {
                ga.destroy(&self.device, &mut self.allocator);
            }
            if let Some((layout, pipeline)) = self.smudge_pipeline.take() {
                self.device.destroy_pipeline(pipeline, None);
                self.device.destroy_pipeline_layout(layout, None);
            }
            if let Some((mut image, pool, _)) = self.smudge_before.take() {
                self.device.destroy_descriptor_pool(pool, None);
                ManuallyDrop::take(&mut image).destroy(&self.device, &mut self.allocator);
            }
            ManuallyDrop::take(&mut self.layer_blend_pipeline).destroy(&self.device);
            ManuallyDrop::take(&mut self.layer_composite_pipeline).destroy(&self.device);
            ManuallyDrop::take(&mut self.composite_pipeline).destroy(&self.device);
            ManuallyDrop::take(&mut self.filter_resources)
                .destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.gradient_overlay)
                .destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.shape_overlay).destroy(&self.device);
            ManuallyDrop::take(&mut self.fill_overlay).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.selection).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.mask_pipelines).destroy(&self.device);
            ManuallyDrop::take(&mut self.mask_pipelines_buildup).destroy(&self.device);
            ManuallyDrop::take(&mut self.dab_pipelines).destroy(&self.device);
            ManuallyDrop::take(&mut self.pattern_atlas).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.dab_buffers).destroy(&self.device, &mut self.allocator);
            for fb in self.display_framebuffers.drain(..) {
                self.device.destroy_framebuffer(fb, None);
            }
            ManuallyDrop::take(&mut self.present_convert).destroy(&self.device);
            for img in self.display.drain(..) {
                img.destroy(&self.device);
            }
            self.device
                .destroy_framebuffer(self.preview_framebuffer, None);
            self.device
                .destroy_framebuffer(self.preview_below_framebuffer, None);
            ManuallyDrop::take(&mut self.preview_below).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.erase_preview).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.stroke_target).destroy(&self.device);
            ManuallyDrop::take(&mut self.canvas_target).destroy(&self.device);
            ManuallyDrop::take(&mut self.staging).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.preview).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.stroke).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.canvas).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::drop(&mut self.allocator);
            // Instance + device are process-shared (see `SharedVk`); they
            // outlive this renderer and are intentionally not destroyed here.
        }
    }
}

impl std::fmt::Debug for VulkanRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VulkanRenderer")
            .field("device", &self.device_name)
            .field("canvas", &self.canvas_size)
            .finish_non_exhaustive()
    }
}

pub(super) const fn full_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

pub(in crate::renderer) fn create_framebuffer_for_view(
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

/// Allocate a one-set descriptor pool plus a set that binds `view` at
/// binding 0 as a combined image sampler (the layer-composite input
/// layout). Used by scratch images that need to be sampled like a layer.
pub(in crate::renderer) fn create_sampled_image_set(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    view: vk::ImageView,
) -> Result<(vk::DescriptorPool, vk::DescriptorSet), RendererError> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: 1,
    }];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .pool_sizes(&sizes)
        .max_sets(1);
    let pool = unsafe { device.create_descriptor_pool(&pool_info, None)? };

    let layouts = [set_layout];
    let alloc_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&layouts);
    let set = unsafe { device.allocate_descriptor_sets(&alloc_info)? }[0];

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

    Ok((pool, set))
}

pub(super) fn full_image_barrier(
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(full_subresource_range())
}

#[cfg(test)]
mod tests {
    use super::super::dab::DabInstance;
    use super::*;
    use crate::renderer::DabFamily;

    fn round_dab(center: [f32; 2], radius: f32, color_premul: [f32; 4]) -> DabInstance {
        DabInstance {
            center,
            radius,
            rotation: 0.0,
            aspect: 1.0,
            flow: 1.0,
            color_premul,
            texture_uv: [0.0, 0.0, 1.0, 1.0],
            hardness: 1.0,
            tip: 0.0,
            texture_scale: 0.0,
            texture_strength: 0.0,
            texturing_mode: 0.0,
        }
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn construct_and_drop() {
        let r = VulkanRenderer::new(Size::new(64, 64)).expect("renderer init");
        drop(r);
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn clear_and_read_white() {
        let mut r = VulkanRenderer::new(Size::new(64, 64)).expect("renderer init");
        let bytes = r.clear_and_read([1.0, 1.0, 1.0, 1.0]).expect("clear");
        assert_eq!(bytes.len(), 64 * 64 * 4);
        assert_eq!(&bytes[..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn clear_and_read_black() {
        let mut r = VulkanRenderer::new(Size::new(64, 64)).expect("renderer init");
        let bytes = r.clear_and_read([0.0, 0.0, 0.0, 1.0]).expect("clear");
        assert_eq!(&bytes[..4], &[0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn paint_one_dab() {
        let size = Size::new(64, 64);
        let mut r = VulkanRenderer::new(size).expect("renderer init");
        r.clear_canvas([0.0, 0.0, 0.0, 0.0]).expect("clear");
        let dab = round_dab([32.0, 32.0], 20.0, [1.0, 0.0, 0.0, 1.0]);
        r.paint_dabs(DabFamily::SoftRound, &[dab]).expect("paint");
        let bytes = r.read_canvas().expect("readback");

        let i = (32 * 64 + 32) * 4;
        assert!(bytes[i] <= 0x10, "center B={}", bytes[i]);
        assert!(bytes[i + 1] <= 0x10, "center G={}", bytes[i + 1]);
        assert!(bytes[i + 2] >= 0xF0, "center R={}", bytes[i + 2]);
        assert!(bytes[i + 3] >= 0xF0, "center A={}", bytes[i + 3]);
        assert_eq!(&bytes[..4], &[0x00, 0x00, 0x00, 0x00], "corner not cleared");
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn composite_stroke_red() {
        let size = Size::new(64, 64);
        let mut r = VulkanRenderer::new(size).expect("renderer init");
        r.clear_canvas([0.0, 0.0, 0.0, 1.0]).expect("clear canvas");
        r.clear_stroke().expect("clear stroke");
        let dab = round_dab([32.0, 32.0], 20.0, [1.0, 1.0, 1.0, 1.0]);
        r.stamp_mask(DabFamily::SoftRound, &[dab]).expect("stamp");
        r.composite_stroke([1.0, 0.0, 0.0], 1.0).expect("composite");
        let bytes = r.read_canvas().expect("readback");

        let i = (32 * 64 + 32) * 4;
        assert!(bytes[i] <= 0x10);
        assert!(bytes[i + 1] <= 0x10);
        assert!(bytes[i + 2] >= 0xF0);
        assert!(bytes[i + 3] >= 0xF0);
        assert_eq!(&bytes[..4], &[0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn select_all_fills_mask() {
        let mut r = VulkanRenderer::new(Size::new(32, 32)).expect("renderer init");
        r.select_all().expect("select_all");
        assert!(r.selection_active());
        let mask = r.read_selection_mask().expect("read mask");
        assert_eq!(mask[0], 0xFF, "expected fully selected mask");
        assert_eq!(mask[mask.len() - 1], 0xFF);
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn rect_shape_selects_interior_only() {
        use crate::renderer::selection::SelectionBlendMode;
        let mut r = VulkanRenderer::new(Size::new(32, 32)).expect("renderer init");
        // Build a CPU mask: a 16x16 square in the centre.
        let mut shape = vec![0u8; 32 * 32];
        for y in 8..24 {
            for x in 8..24 {
                shape[y * 32 + x] = 0xFF;
            }
        }
        r.apply_selection_shape(&shape, SelectionBlendMode::Replace)
            .expect("apply shape");
        assert!(r.selection_active());
        let mask = r.read_selection_mask().expect("read mask");
        // Inside the 8..24 square should be 0xFF, outside should be 0.
        assert_eq!(mask[16 * 32 + 16], 0xFF, "center of square");
        assert_eq!(mask[0], 0x00, "outside corner");
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn invert_flips_mask() {
        use crate::renderer::selection::SelectionBlendMode;
        let mut r = VulkanRenderer::new(Size::new(16, 16)).expect("renderer init");
        let mut shape = vec![0u8; 16 * 16];
        for y in 0..8 {
            for x in 0..16 {
                shape[y * 16 + x] = 0xFF;
            }
        }
        r.apply_selection_shape(&shape, SelectionBlendMode::Replace)
            .expect("apply");
        r.invert_selection().expect("invert");
        let mask = r.read_selection_mask().expect("read");
        // Was 0xFF in top half, 0 in bottom half; now flipped.
        assert!(mask[0] < 0x10, "top should now be deselected");
        assert!(mask[15 * 16] > 0xF0, "bottom should now be selected");
    }

    // Fire many single-dab stamps back to back. If an upload overwrites the
    // instance buffer before the previous async draw has run, dabs get dropped
    // and the row shows a hole. Repro for the fast-stroke end gaps.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn burst_stamp_no_dropped_dabs() {
        let size = Size::new(256, 8);
        let mut r = VulkanRenderer::new(size).expect("renderer init");
        r.clear_stroke().expect("clear stroke");
        // One dab every 2px across the row, each its own submit (250 submits).
        let radius = 2.0;
        let mut x = 2.0;
        while x < 254.0 {
            let dab = round_dab([x, 4.0], radius, [1.0, 1.0, 1.0, 1.0]);
            r.stamp_mask(DabFamily::SoftRound, &[dab]).expect("stamp");
            x += 2.0;
        }
        let stroke = r.read_stroke().expect("readback");
        // read_stroke is a single-channel coverage mask (one u8 per pixel). The
        // row (y=4) should be continuously covered: no wide hole where a dab was
        // dropped.
        let width = size.width as usize;
        let row = 4 * width;
        let mut worst_run = 0;
        let mut run = 0;
        for px in 3..253usize {
            let a = stroke[row + px];
            if a < 0x40 {
                run += 1;
                worst_run = worst_run.max(run);
            } else {
                run = 0;
            }
        }
        assert!(
            worst_run <= 1,
            "dropped dabs: {worst_run}px continuous hole in the stamped row",
        );
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn stamp_mask_saturates() {
        let size = Size::new(64, 64);
        let mut r = VulkanRenderer::new(size).expect("renderer init");
        r.clear_stroke().expect("clear stroke");
        let dabs = [
            round_dab([32.0, 32.0], 20.0, [1.0, 1.0, 1.0, 1.0]),
            round_dab([32.0, 32.0], 20.0, [1.0, 1.0, 1.0, 1.0]),
        ];
        r.stamp_mask(DabFamily::SoftRound, &dabs).expect("stamp");
        let stroke = r.read_stroke().expect("readback");

        let i = 32 * 64 + 32;
        assert!(stroke[i] >= 0xF0);
        assert_eq!(stroke[0], 0x00);
    }
}
