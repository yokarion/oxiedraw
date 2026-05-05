//! GPU affine transform applied to a single layer.

use ash::vk;
use gpu_allocator::MemoryLocation;

use super::super::RendererError;
use super::super::resources::{Buffer, Image};
use super::{CANVAS_BYTES_PER_PIXEL, CANVAS_FORMAT, VulkanRenderer, full_subresource_range};

impl VulkanRenderer {
    /// Apply an affine transform entirely on the GPU.
    ///
    /// Allocates transient resources (source image, target image+framebuffer,
    /// descriptor set, staging buffer), uploads `source_pixels`, runs the
    /// transform pipeline into an `out_w x out_h` render target, blits
    /// the canvas-overlap region directly into the layer image
    /// (GPU -> GPU), then copies the full target back through staging.
    ///
    /// `ext_x`/`ext_y` are the top-left of the output buffer in canvas
    /// coordinates (may be negative for off-canvas content).
    /// `push_constants` is the precomputed 2x3 inverse affine that maps
    /// the output framebuffer UV to the source UV.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn apply_layer_transform_gpu(
        &mut self,
        layer_idx: usize,
        source_pixels: &[u8],
        src_w: u32,
        src_h: u32,
        out_w: u32,
        out_h: u32,
        ext_x: i32,
        ext_y: i32,
        push_constants: [f32; 8],
    ) -> Result<Vec<u8>, RendererError> {
        if layer_idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        if src_w == 0 || src_h == 0 || out_w == 0 || out_h == 0 {
            return Ok(Vec::new());
        }
        let limit = self.max_image_dim;
        for dim in [src_w, src_h, out_w, out_h] {
            if dim > limit {
                return Err(RendererError::TransformTooLarge {
                    requested: dim,
                    limit,
                });
            }
        }

        let src_extent2 = vk::Extent2D {
            width: src_w,
            height: src_h,
        };
        let out_extent2 = vk::Extent2D {
            width: out_w,
            height: out_h,
        };
        let src_extent3 = vk::Extent3D {
            width: src_w,
            height: src_h,
            depth: 1,
        };
        let out_extent3 = vk::Extent3D {
            width: out_w,
            height: out_h,
            depth: 1,
        };
        let canvas_extent = self.canvas_extent_2d();

        let src_bytes = u64::from(src_w) * u64::from(src_h) * CANVAS_BYTES_PER_PIXEL;
        let out_bytes = u64::from(out_w) * u64::from(out_h) * CANVAS_BYTES_PER_PIXEL;
        let staging_size = src_bytes.max(out_bytes);

        let src_image = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "transform-source",
            CANVAS_FORMAT,
            src_extent2,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            vk::ImageAspectFlags::COLOR,
        )?;

        let target_image = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "transform-target",
            CANVAS_FORMAT,
            out_extent2,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
            vk::ImageAspectFlags::COLOR,
        )?;

        let attachments = [target_image.view];
        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(self.canvas_target.render_pass)
            .attachments(&attachments)
            .width(out_w)
            .height(out_h)
            .layers(1);
        let target_framebuffer = unsafe { self.device.create_framebuffer(&fb_info, None)? };

        // Two descriptor sets: one for the transform pipeline (samples
        // `src_image`), and one for the layer_composite pipeline (samples
        // `target_image` when we OVER-blend the AABB onto the layer in
        // step 5 below).
        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: 2,
        }];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&pool_sizes)
            .max_sets(2);
        let descriptor_pool = unsafe { self.device.create_descriptor_pool(&pool_info, None)? };
        let set_layouts = [self.transform_pipeline.descriptor_set_layout];
        let set_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&set_layouts);
        let descriptor_set = unsafe { self.device.allocate_descriptor_sets(&set_alloc)? }[0];
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(src_image.view)
            .image_layout(vk::ImageLayout::GENERAL)
            .sampler(self.transform_pipeline.sampler)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };

        // Layer-composite descriptor set: samples `target_image` (the
        // AABB-sized transform output) for the OVER-blend pass.
        let lc_set_layouts = [self.layer_composite_pipeline.descriptor_set_layout];
        let lc_set_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&lc_set_layouts);
        let lc_descriptor_set =
            unsafe { self.device.allocate_descriptor_sets(&lc_set_alloc)? }[0];
        let lc_image_info = [vk::DescriptorImageInfo::default()
            .image_view(target_image.view)
            .image_layout(vk::ImageLayout::GENERAL)
            .sampler(self.layer_composite_pipeline.sampler)];
        let lc_writes = [vk::WriteDescriptorSet::default()
            .dst_set(lc_descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&lc_image_info)];
        unsafe { self.device.update_descriptor_sets(&lc_writes, &[]) };

        let mut staging = Buffer::new(
            &self.device,
            &mut self.allocator,
            "transform-staging",
            staging_size,
            vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
            MemoryLocation::GpuToCpu,
        )?;

        {
            let mapped = staging
                .mapped_mut()
                .ok_or(RendererError::StagingNotMapped)?;
            let copy_len = source_pixels
                .len()
                .min(src_bytes as usize)
                .min(mapped.len());
            mapped[..copy_len].copy_from_slice(&source_pixels[..copy_len]);
        }

        let dst_x = ext_x.max(0);
        let dst_y = ext_y.max(0);
        let end_x = ext_x.saturating_add(out_w as i32);
        let end_y = ext_y.saturating_add(out_h as i32);
        let dst_end_x = end_x.min(canvas_extent.width as i32);
        let dst_end_y = end_y.min(canvas_extent.height as i32);
        let do_blit = dst_x < dst_end_x && dst_y < dst_end_y;
        let blit_w = (dst_end_x - dst_x).max(0) as u32;
        let blit_h = (dst_end_y - dst_y).max(0) as u32;
        let blit_src_x = (dst_x - ext_x) as u32;
        let blit_src_y = (dst_y - ext_y) as u32;

        let src_image_h = src_image.handle;
        let target_image_h = target_image.handle;
        let layer_framebuffer = self.layer_stack.slots[layer_idx].framebuffer;
        let staging_h = staging.handle;
        let render_pass = self.canvas_target.render_pass;
        let pipeline = self.transform_pipeline.pipeline;
        let layout = self.transform_pipeline.layout;
        let lc_pipeline = self.layer_composite_pipeline.pipeline;
        let lc_layout = self.layer_composite_pipeline.layout;

        let submit_result = self.record_and_submit(|this| {
            // 1. Source: UNDEFINED -> TRANSFER_DST, copy staging -> src.
            this.barrier(
                src_image_h,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let upload = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D::default())
                .image_extent(src_extent3);
            unsafe {
                this.device.cmd_copy_buffer_to_image(
                    this.command_buffer,
                    staging_h,
                    src_image_h,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[upload],
                );
            }
            this.barrier(
                src_image_h,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );

            // 2. Target: UNDEFINED -> TRANSFER_DST, clear to transparent.
            this.barrier(
                target_image_h,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let clear = vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 0.0],
            };
            unsafe {
                this.device.cmd_clear_color_image(
                    this.command_buffer,
                    target_image_h,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &clear,
                    &[full_subresource_range()],
                );
            }
            this.barrier(
                target_image_h,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );

            // 3. Render pass into the AABB-sized target framebuffer.
            #[allow(clippy::cast_precision_loss)]
            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: out_w as f32,
                height: out_h as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let scissor = vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: out_extent2,
            };
            let begin = vk::RenderPassBeginInfo::default()
                .render_pass(render_pass)
                .framebuffer(target_framebuffer)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: out_extent2,
                });
            unsafe {
                this.device.cmd_begin_render_pass(
                    this.command_buffer,
                    &begin,
                    vk::SubpassContents::INLINE,
                );
                this.device.cmd_bind_pipeline(
                    this.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipeline,
                );
                this.device
                    .cmd_set_viewport(this.command_buffer, 0, &[viewport]);
                this.device
                    .cmd_set_scissor(this.command_buffer, 0, &[scissor]);
                this.device.cmd_bind_descriptor_sets(
                    this.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[descriptor_set],
                    &[],
                );
                let push_bytes = std::slice::from_raw_parts(
                    push_constants.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(&push_constants),
                );
                this.device.cmd_push_constants(
                    this.command_buffer,
                    layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push_bytes,
                );
                this.device.cmd_draw(this.command_buffer, 3, 1, 0, 0);
                this.device.cmd_end_render_pass(this.command_buffer);
            }

            // 4. Memory barrier: the transform render pass wrote
            //    `target_image` (color attachment) and the OVER-blend
            //    pass below reads it as a sampled image. Same layout
            //    (GENERAL -> GENERAL) but the barrier flushes writes
            //    so the shader fetch sees them.
            this.barrier(
                target_image_h,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
            );
            // Suppress the `do_blit`/`blit_src_*` warnings that were
            // used by the old `cmd_copy_image` path; layout-clipping is
            // now handled by the viewport/scissor below.
            let _ = (do_blit, blit_w, blit_h, blit_src_x, blit_src_y);

            // 5. OVER-blend the AABB transform output onto the active
            //    layer using `layer_composite_pipeline`. We render a
            //    fullscreen triangle whose v_uv spans [0,1] across the
            //    AABB rect placed at (dst_x, dst_y) in canvas pixels,
            //    with the viewport set to the *clipped* on-canvas
            //    portion. Premultiplied OVER preserves existing layer
            //    content outside the transformed shape (critical for
            //    the selection-lift path, where the unmasked pixels
            //    that remain on the layer must survive Apply).
            if dst_x < dst_end_x && dst_y < dst_end_y {
                // Viewport extends across the *full* AABB in canvas
                // coordinates (origin at ext_x, ext_y; size out_w x out_h),
                // even when part of it is off-canvas. The scissor below
                // clips rendering to the on-canvas region. This keeps
                // v_uv in [0,1] sampling 1:1 with the AABB source pixels.
                #[allow(clippy::cast_precision_loss)]
                let lc_viewport = vk::Viewport {
                    x: ext_x as f32,
                    y: ext_y as f32,
                    width: out_w as f32,
                    height: out_h as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                };
                #[allow(clippy::cast_sign_loss)]
                let lc_scissor = vk::Rect2D {
                    offset: vk::Offset2D {
                        x: dst_x,
                        y: dst_y,
                    },
                    extent: vk::Extent2D {
                        width: (dst_end_x - dst_x) as u32,
                        height: (dst_end_y - dst_y) as u32,
                    },
                };
                let canvas_render_area = vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: canvas_extent,
                };
                let lc_begin = vk::RenderPassBeginInfo::default()
                    .render_pass(render_pass)
                    .framebuffer(layer_framebuffer)
                    .render_area(canvas_render_area);
                unsafe {
                    this.device.cmd_begin_render_pass(
                        this.command_buffer,
                        &lc_begin,
                        vk::SubpassContents::INLINE,
                    );
                    this.device.cmd_bind_pipeline(
                        this.command_buffer,
                        vk::PipelineBindPoint::GRAPHICS,
                        lc_pipeline,
                    );
                    this.device
                        .cmd_set_viewport(this.command_buffer, 0, &[lc_viewport]);
                    this.device
                        .cmd_set_scissor(this.command_buffer, 0, &[lc_scissor]);
                    this.device.cmd_bind_descriptor_sets(
                        this.command_buffer,
                        vk::PipelineBindPoint::GRAPHICS,
                        lc_layout,
                        0,
                        &[lc_descriptor_set],
                        &[],
                    );
                    this.device.cmd_draw(this.command_buffer, 3, 1, 0, 0);
                    this.device.cmd_end_render_pass(this.command_buffer);
                }
            }

            // 6. Read back full AABB: target -> TRANSFER_SRC -> staging.
            this.barrier(
                target_image_h,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            );
            let readback = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D::default())
                .image_extent(out_extent3);
            unsafe {
                this.device.cmd_copy_image_to_buffer(
                    this.command_buffer,
                    target_image_h,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    staging_h,
                    &[readback],
                );
            }

            Ok(())
        });

        let result_pixels = match submit_result {
            Ok(()) => {
                self.layer_stack.touch(layer_idx);
                let mapped = staging.mapped().ok_or(RendererError::StagingNotMapped)?;
                let len = out_bytes as usize;
                Ok(mapped[..len].to_vec())
            }
            Err(e) => Err(e),
        };

        unsafe {
            self.device.destroy_framebuffer(target_framebuffer, None);
            self.device.destroy_descriptor_pool(descriptor_pool, None);
            staging.destroy(&self.device, &mut self.allocator);
            target_image.destroy(&self.device, &mut self.allocator);
            src_image.destroy(&self.device, &mut self.allocator);
        }

        result_pixels
    }
}
