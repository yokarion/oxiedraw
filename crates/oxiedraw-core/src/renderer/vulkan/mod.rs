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

mod fill_ops;
mod filter_ops;
mod io;
mod layer_ops;
mod pattern_ops;
mod present;
mod preview;
mod selection_ops;
mod shape_ops;
mod stroke;
mod transform_ops;

pub use shape_ops::ShapeKind;

use std::mem::ManuallyDrop;

use ash::{Device, Entry, Instance, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocator, AllocatorCreateDesc};
use oxiedraw_utils::geometry::Size;

use super::RendererError;
use super::composite::CompositePipeline;
use super::dab::DabBuffers;
use super::device;
use super::dmabuf::DmabufImage;
use super::erase::ErasePreview;
use super::fill_overlay::FillOverlayResources;
use super::filters::FilterResources;
use super::instance::{self, DebugMessenger};
use super::layers::{LayerCompositePipeline, LayerStack};
use super::mask::{DabPipelineSet, MaskPipelineSet};
use super::pattern_atlas::PatternAtlas;
use super::resources::{Buffer, Image};
use super::selection::SelectionResources;
use super::shape_overlay::ShapeOverlayResources;
use super::targets::ImageTarget;
use super::transform::TransformPipeline;

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

/// BGRA8 is 4 bytes per pixel. Used to size staging buffers and the
/// readback `Vec<u8>` returned to callers.
pub(super) const CANVAS_BYTES_PER_PIXEL: u64 = 4;

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
    /// Cache mapping pattern data identity (raw `*const PatternData`)
    /// to its atlas slot. Lets `upload_pattern` no-op on re-uploads of
    /// the same `Rc<PatternData>`.
    pub(super) pattern_cache: std::collections::HashMap<usize, u32>,
    pub(super) composite_pipeline: ManuallyDrop<CompositePipeline>,
    pub(super) layer_composite_pipeline: ManuallyDrop<LayerCompositePipeline>,
    pub(super) transform_pipeline: ManuallyDrop<TransformPipeline>,
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

    pub(super) filter_resources: ManuallyDrop<FilterResources>,
    /// True while a filter popup is open. Gates the preview path so the
    /// affected layers are composited through the filter pipeline.
    pub(super) filter_active: bool,
    /// The filter + parameters being previewed (set by `begin_filter` /
    /// `update_filter_spec`).
    pub(super) filter_spec: crate::filters::FilterSpec,
    /// Layer indices the filter applies to (z-order independent).
    pub(super) filter_affected: Vec<usize>,

    /// Display-side dmabuf image. Per-frame `present_to_display` copies
    /// the chosen source (canvas or preview) into here.
    pub(super) display: ManuallyDrop<DmabufImage>,
    pub(super) display_initialised: bool,

    pub(super) command_pool: vk::CommandPool,
    pub(super) command_buffer: vk::CommandBuffer,
    pub(super) fence: vk::Fence,

    pub(super) queue: vk::Queue,
    #[allow(dead_code)]
    pub(super) queue_family: u32,

    pub(super) allocator: ManuallyDrop<Allocator>,
    pub(super) device: Device,

    pub(super) debug: Option<DebugMessenger>,
    pub(super) instance: Instance,
    #[allow(dead_code)]
    pub(super) physical_device: vk::PhysicalDevice,
    pub(super) device_name: String,
    /// `VkPhysicalDeviceLimits::maxImageDimension2D` - caps the transform
    /// render target size. Beyond this `vkCreateImage` fails.
    pub(super) max_image_dim: u32,
    pub(super) _entry: Entry,
}

impl VulkanRenderer {
    #[allow(clippy::too_many_lines)]
    pub fn new(canvas_size: Size) -> Result<Self, RendererError> {
        let inst = instance::create()?;
        let dev = device::create(&inst.instance)?;

        let dev_props = unsafe { inst.instance.get_physical_device_properties(dev.physical) };
        let max_image_dim = dev_props.limits.max_image_dimension2_d;

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
        let composite_pipeline = CompositePipeline::new(
            &dev.device,
            canvas_target.render_pass,
            stroke.view,
            selection.mask.view,
        )?;
        let layer_composite_pipeline =
            LayerCompositePipeline::new(&dev.device, canvas_target.render_pass)?;
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
        let display = DmabufImage::new(
            &inst.instance,
            dev.physical,
            &dev.device,
            &dev.external_memory_fd,
            canvas_size.width,
            canvas_size.height,
        )?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(dev.queue_family);
        let command_pool = unsafe { dev.device.create_command_pool(&pool_info, None)? };
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = unsafe { dev.device.allocate_command_buffers(&alloc_info)? }[0];
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        let fence = unsafe { dev.device.create_fence(&fence_info, None)? };

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
            staging: ManuallyDrop::new(staging),
            canvas_target: ManuallyDrop::new(canvas_target),
            stroke_target: ManuallyDrop::new(stroke_target),
            preview_framebuffer,
            dab_buffers: ManuallyDrop::new(dab_buffers),
            pattern_atlas: ManuallyDrop::new(pattern_atlas),
            dab_pipelines: ManuallyDrop::new(dab_pipelines),
            mask_pipelines: ManuallyDrop::new(mask_pipelines),
            pattern_cache: std::collections::HashMap::new(),
            composite_pipeline: ManuallyDrop::new(composite_pipeline),
            layer_composite_pipeline: ManuallyDrop::new(layer_composite_pipeline),
            transform_pipeline: ManuallyDrop::new(transform_pipeline),
            layer_stack: ManuallyDrop::new(layer_stack),
            selection: ManuallyDrop::new(selection),
            selection_active: false,
            fill_overlay: ManuallyDrop::new(fill_overlay),
            fill_active: false,
            fill_reveal: 0.0,
            fill_color_premul: [0.0; 4],
            fill_layer_idx: 0,
            shape_overlay: ManuallyDrop::new(shape_overlay),
            shape_active: false,
            shape_layer_idx: 0,
            shape_color_premul: [0.0; 4],
            shape_rect: [0.0; 4],
            shape_extra: [0.0; 4],
            filter_resources: ManuallyDrop::new(filter_resources),
            filter_active: false,
            filter_spec: crate::filters::FilterSpec::Invert,
            filter_affected: Vec::new(),
            display: ManuallyDrop::new(display),
            display_initialised: false,
            command_pool,
            command_buffer,
            fence,
            queue: dev.queue,
            queue_family: dev.queue_family,
            allocator: ManuallyDrop::new(allocator),
            device: dev.device,
            debug: inst.debug,
            instance: inst.instance,
            physical_device: dev.physical,
            device_name: dev.device_name,
            max_image_dim,
            _entry: inst.entry,
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
                    this.erase_preview.scratch.handle,
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
        let copy = vk::ImageCopy::default()
            .src_subresource(subresource)
            .src_offset(vk::Offset3D::default())
            .dst_subresource(subresource)
            .dst_offset(vk::Offset3D::default())
            .extent(self.canvas.extent);
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
        let scissor = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(render_pass)
            .framebuffer(framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            });
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

    /// Draw the fullscreen triangle (3 vertices, 1 instance) and end the
    /// render pass started by `cmd_begin_fullscreen_pass`.
    pub(super) fn cmd_end_fullscreen_pass(&self) {
        unsafe {
            self.device.cmd_draw(self.command_buffer, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(self.command_buffer);
        }
    }

    /// Record a one-shot command buffer, submit, fence-wait. Used by
    /// every public operation while we don't have proper frame pacing.
    pub(super) fn record_and_submit<F>(&mut self, record: F) -> Result<(), RendererError>
    where
        F: FnOnce(&mut Self) -> Result<(), RendererError>,
    {
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
        unsafe { self.device.wait_for_fences(&[self.fence], true, u64::MAX)? };
        Ok(())
    }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
            ManuallyDrop::take(&mut self.layer_stack).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.transform_pipeline).destroy(&self.device);
            ManuallyDrop::take(&mut self.layer_composite_pipeline).destroy(&self.device);
            ManuallyDrop::take(&mut self.composite_pipeline).destroy(&self.device);
            ManuallyDrop::take(&mut self.filter_resources)
                .destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.shape_overlay).destroy(&self.device);
            ManuallyDrop::take(&mut self.fill_overlay).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.selection).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.mask_pipelines).destroy(&self.device);
            ManuallyDrop::take(&mut self.dab_pipelines).destroy(&self.device);
            ManuallyDrop::take(&mut self.pattern_atlas).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.dab_buffers).destroy(&self.device, &mut self.allocator);
            ManuallyDrop::take(&mut self.display).destroy(&self.device);
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
            self.device.destroy_device(None);
            if let Some(d) = self.debug.take() {
                d.loader.destroy_debug_utils_messenger(d.messenger, None);
            }
            self.instance.destroy_instance(None);
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
