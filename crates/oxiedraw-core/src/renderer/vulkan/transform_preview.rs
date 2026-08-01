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

/// GPU resources for one transformed layer within a preview session.
pub(in crate::renderer) struct PreviewLayer {
    /// Upright source pixels (the layer/master being transformed).
    src: Image,
    /// Canvas-sized warp output, sampled by the blend pass.
    warped: Image,
    warped_framebuffer: vk::Framebuffer,
    /// Transform-pipeline set sampling `src`.
    src_set: vk::DescriptorSet,
    /// Layer-composite-layout set sampling `warped` (the blend pass's src).
    warped_set: vk::DescriptorSet,
    /// Layer this warped source targets (its slot supplies blend mode + opacity).
    target_idx: usize,
}

/// Reusable GPU resources for one transform-preview session. Holds one
/// [`PreviewLayer`] per transformed layer; all share the same affine `push`
/// (a single drag moves every target rigidly together).
pub(in crate::renderer) struct TransformPreview {
    layers: Vec<PreviewLayer>,
    descriptor_pool: vk::DescriptorPool,
    /// Lowest target slot index; the strictly-below stack is cacheable up to it.
    lowest_target: usize,
    /// 2x3 inverse affine (canvas-output UV -> source UV) for the current rect.
    push: [f32; 8],
}

impl VulkanRenderer {
    /// Whether the live GPU transform preview is active.
    #[must_use]
    pub fn transform_preview_active(&self) -> bool {
        self.transform_preview.is_some()
    }

    /// Number of layers the active transform preview targets (0 when none).
    #[must_use]
    pub fn transform_preview_target_count(&self) -> usize {
        self.transform_preview.as_ref().map_or(0, |tp| tp.layers.len())
    }

    /// The slot index of the `i`-th transform-preview target, if in range. Paired
    /// with [`Self::transform_preview_target_count`] to test the targets without
    /// allocating a `Vec` each present frame.
    #[must_use]
    pub fn transform_preview_target_at(&self, i: usize) -> Option<usize> {
        self.transform_preview
            .as_ref()
            .and_then(|tp| tp.layers.get(i))
            .map(|l| l.target_idx)
    }

    /// Begin a transform-preview session for one or more layers: upload each
    /// source and allocate the reusable warp/blend resources. The caller has
    /// already cleared every target layer and composited the below-stack.
    pub fn begin_transform_preview_gpu(
        &mut self,
        sources: &[(usize, &[u8], u32, u32)],
    ) -> Result<(), RendererError> {
        self.clear_transform_preview_gpu();
        if sources.is_empty() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        for &(target_idx, _, w, h) in sources {
            if target_idx >= self.layer_stack.slots.len() || w == 0 || h == 0 {
                return Err(RendererError::LayerIndexOutOfRange);
            }
        }
        // The shared below-stack cache may hold a prior stroke's target; force a
        // rebuild for this transform's below-stack on the first frame.
        self.preview_cache_valid = false;
        self.scoped_cache_valid = false;

        let n = sources.len() as u32;
        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: 2 * n,
        }];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&sizes)
            .max_sets(2 * n);
        let descriptor_pool = unsafe { self.device.create_descriptor_pool(&pool_info, None)? };
        let canvas_extent = self.canvas_extent_2d();

        let mut layers = Vec::with_capacity(sources.len());
        for &(target_idx, pixels, width, height) in sources {
            let src_extent = vk::Extent2D { width, height };
            let src = Image::new_2d(
                &self.device,
                &mut self.allocator,
                "transform-preview-src",
                CANVAS_FORMAT,
                src_extent,
                vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
                vk::ImageAspectFlags::COLOR,
            )?;
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
            let src_set = self.alloc_one_sampler_set(
                descriptor_pool,
                self.transform_pipeline.descriptor_set_layout,
                src.view,
                self.transform_pipeline.sampler,
            )?;
            let warped_set = self.alloc_one_sampler_set(
                descriptor_pool,
                self.layer_composite_pipeline.descriptor_set_layout,
                warped.view,
                self.layer_composite_pipeline.sampler,
            )?;
            let layer = PreviewLayer {
                src,
                warped,
                warped_framebuffer,
                src_set,
                warped_set,
                target_idx,
            };
            self.upload_transform_preview_src(&layer, pixels, width, height)?;
            // Resting layout for this warp target.
            let warped_handle = layer.warped.handle;
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
                            warped_handle,
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::GENERAL,
                        )],
                    );
                }
                Ok(())
            })?;
            layers.push(layer);
        }

        let lowest_target = layers.iter().map(|l| l.target_idx).min().unwrap_or(0);
        self.transform_preview = Some(TransformPreview {
            layers,
            descriptor_pool,
            lowest_target,
            // Identity-ish until the first rect arrives; never rendered before.
            push: [0.0; 8],
        });
        Ok(())
    }

    /// Update the affine for the current drag frame (shared by all targets).
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
            for layer in tp.layers {
                self.device.destroy_framebuffer(layer.warped_framebuffer, None);
                layer.src.destroy(&self.device, &mut self.allocator);
                layer.warped.destroy(&self.device, &mut self.allocator);
            }
            self.device.destroy_descriptor_pool(tp.descriptor_pool, None);
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
        let push = tp.push;
        let lowest = tp.lowest_target;
        // (target_idx, warped_fb, warped_img, src_set, warped_set, mode, opacity)
        let mut warps = Vec::with_capacity(tp.layers.len());
        for layer in &tp.layers {
            let (mode, opacity) = self.layer_stack.blend(layer.target_idx);
            warps.push((
                layer.target_idx,
                layer.warped_framebuffer,
                layer.warped.handle,
                layer.src_set,
                layer.warped_set,
                mode,
                opacity,
            ));
        }
        // From the layer stack (never the canvas image, which other work may
        // have changed mid-drag). The strictly-below stack (below the lowest
        // target) is cached in `preview_below` and rebuilt only when invalid.
        let visible = self.visible_layer_indices(visibilities);

        self.record_and_submit(|this| {
            // 1. Warp each source into its canvas-sized scratch (replace blend).
            for &(_, fb, img, src_set, _, _, _) in &warps {
                this.cmd_transform_warp(fb, src_set, push);
                this.barrier(img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            }
            // 2. Cache the layers strictly below the lowest target, in z-order.
            if !this.preview_cache_valid {
                this.cmd_clear_image(this.preview_below.handle, [0.0, 0.0, 0.0, 0.0]);
                for &idx in &visible {
                    if idx >= lowest {
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
            // 4. From the lowest target up, in z-order: for a transformed layer,
            //    compose its stored image first (the pixels left behind - e.g. the
            //    unselected part of a selection-lift; transparent no-op for a
            //    normally-cleared whole-layer transform), then blend the warped
            //    source over it. Compose every other layer normally.
            for &idx in &visible {
                if idx < lowest {
                    continue;
                }
                this.preview_compose_layer(this.preview.handle, this.preview_framebuffer, idx);
                // A transformed layer (in the visible walk, so its slot is
                // visible): blend its warped source over the stored pixels.
                if let Some(&(_, _, _, _, warped_set, mode, opacity)) =
                    warps.iter().find(|w| w.0 == idx)
                {
                    this.cmd_compose_layer_blended(
                        this.preview.handle,
                        this.preview_framebuffer,
                        warped_set,
                        mode,
                        opacity,
                    );
                }
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
        let push = tp.push;
        // Warp every target's source up front, then build the scoped composite
        // with each target's warped layer spliced in at its place in the tree so
        // adjustments (incl. folder-scoped) apply live over the transform.
        let mut warps = Vec::with_capacity(tp.layers.len());
        for layer in &tp.layers {
            let (mode, opacity) = self.layer_stack.blend(layer.target_idx);
            let visible = visibilities.get(layer.target_idx).copied().unwrap_or(false);
            warps.push((
                layer.target_idx,
                layer.warped_framebuffer,
                layer.warped.handle,
                layer.src_set,
                super::adjust_ops::PreviewTarget::Warp { set: layer.warped_set, mode, opacity, visible },
            ));
        }
        self.record_and_submit(|this| {
            for &(_, fb, img, src_set, _) in &warps {
                this.cmd_transform_warp(fb, src_set, push);
                this.barrier(img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            }
            Ok(())
        })?;
        let targets: Vec<(usize, super::adjust_ops::PreviewTarget)> =
            warps.iter().map(|&(idx, _, _, _, target)| (idx, target)).collect();
        self.build_preview_scoped_multi(steps, &targets)
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
        preview: &PreviewLayer,
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
