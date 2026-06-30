//! Live GPU transform preview.
//!
//! While the transform tool is dragging, the warped layer must show its real
//! blend mode + opacity - which the GSK overlay can't do (it blends in gamma
//! space and has no additive mode). So this path warps the source through the
//! transform pipeline into a canvas-sized scratch every frame, blends it over
//! the below-stack at the layer's mode/opacity, composites the layers above,
//! and presents the result - pixel-identical to the eventual commit.
//!
//! Resources (source image, warped scratch, descriptor sets) are allocated
//! once when the transform begins and reused for every drag frame, so the hot
//! path is a single submit with no per-frame allocation.

use ash::vk;

use super::super::RendererError;
use super::super::resources::{Buffer, Image};
use super::{CANVAS_BYTES_PER_PIXEL, CANVAS_FORMAT, VulkanRenderer, full_image_barrier};
use crate::document::CompositeStep;

/// Reusable GPU resources for one transform-preview session.
pub(in crate::renderer) struct TransformPreview {
    /// Upright source pixels (the layer/master being transformed).
    src: Image,
    /// Canvas-sized warp output, sampled by the blend pass.
    warped: Image,
    warped_framebuffer: vk::Framebuffer,
    descriptor_pool: vk::DescriptorPool,
    /// Transform-pipeline set sampling `src`.
    src_set: vk::DescriptorSet,
    /// Layer-composite-layout set sampling `warped` (the blend pass's src).
    warped_set: vk::DescriptorSet,
    /// Layer the transform targets (its slot supplies the blend mode + opacity).
    target_idx: usize,
    /// 2x3 inverse affine (canvas-output UV -> source UV) for the current rect.
    push: [f32; 8],
}

impl VulkanRenderer {
    /// Whether the live GPU transform preview is active.
    #[must_use]
    pub fn transform_preview_active(&self) -> bool {
        self.transform_preview.is_some()
    }

    /// The layer the active transform preview targets, if any.
    #[must_use]
    pub fn transform_preview_target(&self) -> Option<usize> {
        self.transform_preview.as_ref().map(|tp| tp.target_idx)
    }

    /// Begin a transform-preview session: upload `source_pixels` and allocate
    /// the reusable warp/blend resources. The caller has already cleared the
    /// target layer and composited the below-stack into the canvas.
    pub fn begin_transform_preview_gpu(
        &mut self,
        target_idx: usize,
        source_pixels: &[u8],
        src_w: u32,
        src_h: u32,
    ) -> Result<(), RendererError> {
        self.clear_transform_preview_gpu();
        if target_idx >= self.layer_stack.slots.len() || src_w == 0 || src_h == 0 {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        // The shared below-stack cache may hold a prior stroke's target; force a
        // rebuild for this transform's below-stack on the first frame.
        self.preview_cache_valid = false;
        self.scoped_cache_valid = false;

        let src_extent = vk::Extent2D {
            width: src_w,
            height: src_h,
        };
        let src = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "transform-preview-src",
            CANVAS_FORMAT,
            src_extent,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        let canvas_extent = self.canvas_extent_2d();
        let warped = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "transform-preview-warped",
            CANVAS_FORMAT,
            canvas_extent,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        let warped_framebuffer = super::create_framebuffer_for_view(
            &self.device,
            self.canvas_target.render_pass,
            canvas_extent,
            warped.view,
        )?;

        let (descriptor_pool, src_set, warped_set) = self.alloc_transform_preview_sets(
            src.view,
            warped.view,
        )?;

        let preview = TransformPreview {
            src,
            warped,
            warped_framebuffer,
            descriptor_pool,
            src_set,
            warped_set,
            target_idx,
            // Identity-ish until the first rect arrives; never rendered before.
            push: [0.0; 8],
        };

        self.upload_transform_preview_src(&preview, source_pixels, src_w, src_h)?;
        // Resting layout for the warp target.
        self.record_and_submit(|this| {
            unsafe {
                this.device.cmd_pipeline_barrier(
                    this.command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[full_image_barrier(
                        preview.warped.handle,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::GENERAL,
                    )],
                );
            }
            Ok(())
        })?;

        self.transform_preview = Some(preview);
        Ok(())
    }

    /// Update the affine for the current drag frame.
    pub fn set_transform_preview_push(&mut self, push: [f32; 8]) {
        if let Some(tp) = self.transform_preview.as_mut() {
            tp.push = push;
        }
    }

    /// Tear down the transform-preview session and free its GPU resources.
    pub fn clear_transform_preview_gpu(&mut self) {
        let Some(tp) = self.transform_preview.take() else {
            return;
        };
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_framebuffer(tp.warped_framebuffer, None);
            self.device.destroy_descriptor_pool(tp.descriptor_pool, None);
            tp.src.destroy(&self.device, &mut self.allocator);
            tp.warped.destroy(&self.device, &mut self.allocator);
        }
    }

    /// Render the live preview into the preview image: warp the source, blend
    /// it over the below-stack (already in the canvas) at the target layer's
    /// mode + opacity, then composite the visible layers above. Caller presents
    /// `PresentSource::Preview` afterwards.
    pub fn render_transform_preview(
        &mut self,
        visibilities: &[bool],
    ) -> Result<(), RendererError> {
        let Some(tp) = self.transform_preview.as_ref() else {
            return Ok(());
        };
        let target = tp.target_idx;
        let push = tp.push;
        let warped_fb = tp.warped_framebuffer;
        let warped_img = tp.warped.handle;
        let src_set = tp.src_set;
        let warped_set = tp.warped_set;
        let (mode, opacity) = self.layer_stack.blend(target);
        let target_visible = visibilities.get(target).copied().unwrap_or(false);
        // From the layer stack (never the canvas image, which other work may
        // have changed mid-drag). The strictly-below stack is cached in
        // `preview_below` and rebuilt only when invalid (see begin), so the hot
        // path is warp + copy + blend target + the (few) layers above.
        let visible = self.visible_layer_indices(visibilities);

        self.record_and_submit(|this| {
            // 1. Warp the source into the canvas-sized scratch (replace blend).
            this.cmd_transform_warp(warped_fb, src_set, push);
            this.barrier(warped_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            // 2. Cache the layers strictly below the target, in z-order.
            if !this.preview_cache_valid {
                this.cmd_clear_image(this.preview_below.handle, [0.0, 0.0, 0.0, 0.0]);
                for &idx in &visible {
                    if idx >= target {
                        break;
                    }
                    this.preview_compose_layer(
                        this.preview_below.handle,
                        this.preview_below_framebuffer,
                        idx,
                    );
                }
                this.preview_cache_valid = true;
            }
            // 3. preview := cached below stack.
            this.cmd_copy_image_full(this.preview_below.handle, this.preview.handle);
            // 4. Blend the warped target over the below stack at its mode/opacity.
            if target_visible {
                this.cmd_compose_layer_blended(
                    this.preview.handle,
                    this.preview_framebuffer,
                    warped_set,
                    mode,
                    opacity,
                );
            }
            // 5. Layers above the target, in z-order.
            for &idx in &visible {
                if idx <= target {
                    continue;
                }
                this.preview_compose_layer(this.preview.handle, this.preview_framebuffer, idx);
            }
            Ok(())
        })
    }

    /// Folder-scoped transform preview: warp the source, then run the same
    /// accumulator-stack walk as the stroke preview with the warped layer as the
    /// in-flight target content, so an adjustment above the transformed layer is
    /// applied (and clipped to its folder). Builds into the preview image; the
    /// caller presents `PresentSource::Preview`. No-op without an active session.
    pub fn render_transform_preview_scoped(
        &mut self,
        steps: &[CompositeStep],
        visibilities: &[bool],
    ) -> Result<(), RendererError> {
        let Some(tp) = self.transform_preview.as_ref() else {
            return Ok(());
        };
        let target_idx = tp.target_idx;
        let warped_fb = tp.warped_framebuffer;
        let warped_img = tp.warped.handle;
        let src_set = tp.src_set;
        let warped_set = tp.warped_set;
        let push = tp.push;
        let (mode, opacity) = self.layer_stack.blend(target_idx);
        let visible = visibilities.get(target_idx).copied().unwrap_or(false);
        // Warp the source into the scratch up front; the walk then composes it.
        self.record_and_submit(|this| {
            this.cmd_transform_warp(warped_fb, src_set, push);
            this.barrier(warped_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            Ok(())
        })?;
        let target = super::adjust_ops::PreviewTarget::Warp {
            set: warped_set,
            mode,
            opacity,
            visible,
        };
        self.build_preview_scoped(steps, target_idx, target)
    }

    /// As [`Self::render_transform_preview_scoped`] but reads the preview back
    /// to host memory instead of presenting (diagnostic / tests).
    pub fn read_transform_preview_scoped(
        &mut self,
        steps: &[CompositeStep],
        visibilities: &[bool],
    ) -> Result<Vec<u8>, RendererError> {
        self.render_transform_preview_scoped(steps, visibilities)?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes()
    }

    /// Render the transform preview and read it back as BGRA8. Test/diagnostic
    /// helper - the live path presents the preview image rather than reading it.
    pub fn read_transform_preview(
        &mut self,
        visibilities: &[bool],
    ) -> Result<Vec<u8>, RendererError> {
        self.render_transform_preview(visibilities)?;
        let image = self.preview.handle;
        let extent = self.canvas.extent;
        self.read_image_to_staging(image, extent)?;
        self.copy_staging_bytes()
    }

    /// Warp the source into `framebuffer` (the canvas-sized warp scratch) with
    /// the transform pipeline. The shader writes transparent outside the source
    /// footprint, so a full-canvas pass produces a positioned warped layer.
    fn cmd_transform_warp(
        &self,
        framebuffer: vk::Framebuffer,
        src_set: vk::DescriptorSet,
        push: [f32; 8],
    ) {
        let render_pass = self.canvas_target.render_pass;
        let pipeline = self.transform_pipeline.pipeline;
        let layout = self.transform_pipeline.layout;
        self.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[src_set],
                &[],
            );
            let push_bytes = std::slice::from_raw_parts(
                push.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&push),
            );
            self.device.cmd_push_constants(
                self.command_buffer,
                layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                push_bytes,
            );
        }
        self.cmd_end_fullscreen_pass();
    }

    /// Allocate the descriptor pool + the source (transform-layout) and warped
    /// (layer-composite-layout) sets for a transform-preview session.
    fn alloc_transform_preview_sets(
        &self,
        src_view: vk::ImageView,
        warped_view: vk::ImageView,
    ) -> Result<(vk::DescriptorPool, vk::DescriptorSet, vk::DescriptorSet), RendererError> {
        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: 2,
        }];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&sizes)
            .max_sets(2);
        let pool = unsafe { self.device.create_descriptor_pool(&pool_info, None)? };

        let src_set = self.alloc_one_sampler_set(
            pool,
            self.transform_pipeline.descriptor_set_layout,
            src_view,
            self.transform_pipeline.sampler,
        )?;
        let warped_set = self.alloc_one_sampler_set(
            pool,
            self.layer_composite_pipeline.descriptor_set_layout,
            warped_view,
            self.layer_composite_pipeline.sampler,
        )?;
        Ok((pool, src_set, warped_set))
    }

    fn alloc_one_sampler_set(
        &self,
        pool: vk::DescriptorPool,
        set_layout: vk::DescriptorSetLayout,
        view: vk::ImageView,
        sampler: vk::Sampler,
    ) -> Result<vk::DescriptorSet, RendererError> {
        let layouts = [set_layout];
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let set = unsafe { self.device.allocate_descriptor_sets(&info)? }[0];
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::GENERAL)
            .sampler(sampler)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        Ok(set)
    }

    /// Upload the source pixels into the preview's source image (one-time).
    fn upload_transform_preview_src(
        &mut self,
        preview: &TransformPreview,
        pixels: &[u8],
        src_w: u32,
        src_h: u32,
    ) -> Result<(), RendererError> {
        let bytes = u64::from(src_w) * u64::from(src_h) * CANVAS_BYTES_PER_PIXEL;
        let mut staging = Buffer::new(
            &self.device,
            &mut self.allocator,
            "transform-preview-upload",
            bytes,
            vk::BufferUsageFlags::TRANSFER_SRC,
            gpu_allocator::MemoryLocation::CpuToGpu,
        )?;
        {
            let mapped = staging
                .mapped_mut()
                .ok_or(RendererError::StagingNotMapped)?;
            let n = pixels.len().min(bytes as usize).min(mapped.len());
            mapped[..n].copy_from_slice(&pixels[..n]);
        }
        let image = preview.src.handle;
        let extent = vk::Extent3D {
            width: src_w,
            height: src_h,
            depth: 1,
        };
        let staging_handle = staging.handle;
        let result = self.record_and_submit(|this| {
            this.barrier(
                image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(extent);
            unsafe {
                this.device.cmd_copy_buffer_to_image(
                    this.command_buffer,
                    staging_handle,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
            }
            this.barrier(
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        });
        unsafe { staging.destroy(&self.device, &mut self.allocator) };
        result
    }
}
