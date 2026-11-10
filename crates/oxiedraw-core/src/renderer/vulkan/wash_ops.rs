//! Wash paint mode: runs of dabs replayed per pixel on a scratch copy of the
//! stroke buffer, since alpha-darken needs the destination blending can't read.

use ash::vk;
use std::mem::ManuallyDrop;

use super::super::RendererError;
use super::super::dab::{DAB_INSTANCE_STRIDE, DabInstance, MAX_INSTANCES, instance_slot_offset};
use super::super::pass::{FullscreenPass, nearest_clamp_sampler, replace_blend};
use super::super::resources::Image;
use super::{STROKE_FORMAT, VulkanRenderer};

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const WASH_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wash.frag.spv"));

/// Dabs replayed per pass, keeping the per-pixel loop short.
const RUN_LEN: usize = 16;

const PUSH_BYTES: u32 = 12;

pub(in crate::renderer) struct WashPass {
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    sampler: vk::Sampler,
    scratch: ManuallyDrop<Image>,
}

impl WashPass {
    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(
        mut self,
        device: &ash::Device,
        allocator: &mut gpu_allocator::vulkan::Allocator,
    ) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_pool(self.pool, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_sampler(self.sampler, None);
            ManuallyDrop::take(&mut self.scratch).destroy(device, allocator);
        }
    }
}

impl VulkanRenderer {
    /// Build the wash pass on first use. Submits, so call before uploading dabs.
    pub(super) fn ensure_wash_pass(&mut self) -> Result<(), RendererError> {
        if self.wash_pass.is_some() {
            return Ok(());
        }
        let extent = self.canvas_extent_2d();
        let scratch = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "wash-scratch",
            STROKE_FORMAT,
            extent,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER_DYNAMIC)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let set_layout = unsafe { self.device.create_descriptor_set_layout(&layout_info, None)? };
        let sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER_DYNAMIC,
                descriptor_count: 1,
            },
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&sizes)
            .max_sets(1);
        let pool = unsafe { self.device.create_descriptor_pool(&pool_info, None)? };
        let layouts = [set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let set = unsafe { self.device.allocate_descriptor_sets(&alloc_info)? }[0];
        let sampler = nearest_clamp_sampler(&self.device)?;
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(scratch.view)
            .image_layout(vk::ImageLayout::GENERAL)
            .sampler(sampler)];
        let buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.dab_buffers.instance.handle)
            .offset(0)
            .range(u64::from(MAX_INSTANCES) * u64::from(DAB_INSTANCE_STRIDE))];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER_DYNAMIC)
                .buffer_info(&buffer_info),
        ];
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };

        let set_layouts = [set_layout, self.falloff_lut.set_layout()];
        let push_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(PUSH_BYTES)];
        let info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_ranges);
        let layout = unsafe { self.device.create_pipeline_layout(&info, None)? };
        let pipeline = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: WASH_FRAG_SPV,
            render_pass: self.stroke_target.render_pass,
            layout,
            blend: replace_blend(),
        }
        .build(&self.device)?;

        let scratch_handle = scratch.handle;
        self.wash_pass = Some(WashPass {
            layout,
            pipeline,
            set_layout,
            pool,
            set,
            sampler,
            scratch: ManuallyDrop::new(scratch),
        });
        self.record_and_submit(|this| {
            this.barrier(scratch_handle, vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL);
            Ok(())
        })
    }

    /// Record the wash of `instances`, already uploaded to the current ring slot.
    pub(super) fn cmd_wash_pass(&mut self, instances: &[DabInstance]) {
        let Some(wash) = self.wash_pass.as_ref() else {
            return;
        };
        let (layout, pipeline, set) = (wash.layout, wash.pipeline, wash.set);
        let scratch = wash.scratch.handle;
        let falloff_set = self.falloff_lut.set();
        let stroke = self.stroke.handle;
        let render_pass = self.stroke_target.render_pass;
        let framebuffer = self.stroke_target.framebuffer;
        #[allow(clippy::cast_possible_truncation)]
        let slot_offset = instance_slot_offset(self.ring_cursor) as u32;
        let falloff = u32::from(self.stroke_falloff);
        let size = self.canvas_size;
        let saved_clip = self.clip;
        let count = instances.len().min(MAX_INSTANCES as usize);
        for first in (0..count).step_by(RUN_LEN) {
            let run = &instances[first..(first + RUN_LEN).min(count)];
            let Some(rect) = run_rect(run, size) else {
                continue;
            };
            self.clip = Some(rect);
            self.cmd_copy_image_full(stroke, scratch);
            #[allow(clippy::cast_possible_truncation)]
            let push = [first as u32, run.len() as u32, falloff];
            self.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
            unsafe {
                self.device.cmd_bind_descriptor_sets(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[set, falloff_set],
                    &[slot_offset],
                );
                self.device.cmd_push_constants(
                    self.command_buffer,
                    layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    u32_bytes(&push),
                );
            }
            self.cmd_end_fullscreen_pass();
            self.barrier(stroke, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        }
        self.clip = saved_clip;
    }
}

fn u32_bytes(values: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and any byte pattern is a valid u8.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values)) }
}

/// Union of the run's dab squares on the canvas, `None` when fully off it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
fn run_rect(run: &[DabInstance], size: oxiedraw_utils::geometry::Size) -> Option<vk::Rect2D> {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for d in run {
        let half = d.radius + 2.0;
        x0 = x0.min(d.center[0] - half);
        y0 = y0.min(d.center[1] - half);
        x1 = x1.max(d.center[0] + half);
        y1 = y1.max(d.center[1] + half);
    }
    let ix0 = x0.floor().max(0.0) as i32;
    let iy0 = y0.floor().max(0.0) as i32;
    let ix1 = x1.ceil().min(size.width as f32) as i32;
    let iy1 = y1.ceil().min(size.height as f32) as i32;
    if ix1 <= ix0 || iy1 <= iy0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: ix0, y: iy0 },
        extent: vk::Extent2D {
            width: (ix1 - ix0) as u32,
            height: (iy1 - iy0) as u32,
        },
    })
}
