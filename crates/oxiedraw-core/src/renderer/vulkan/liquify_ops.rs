//! Liquify session: the displacement field, its ping-pong update, and the
//! warp that turns it back into pixels.
//!
//! A session snapshots the target layer once, then every dab batch runs one
//! scissored compose pass that folds the batch into the field.
//!
//! # Per-stroke baking
//!
//! [`VulkanRenderer::liquify_bake`] copies the warped result into the layer at
//! the end of each stroke, so every stroke becomes an ordinary undoable pixel
//! edit. The snapshot is *not* re-taken: the field keeps accumulating against
//! the pristine source, so N strokes still cost exactly one resample of the
//! original pixels. Baking repeatedly rewrites the layer, but never re-reads it
//! as the warp source, which is what would compound resampling blur.
//!
//! # Ping-pong carry-forward
//!
//! Composition has to read the old field while writing the new one, so the
//! field is a two-image ping-pong. Because each pass is scissored to the dabs it
//! applies, the destination is stale *outside* that rect - specifically, it is
//! missing exactly what the previous pass wrote. `pending_copy` remembers that
//! rect and copies it forward before each compose, which keeps the per-batch
//! cost proportional to the brush footprint instead of the canvas.

use ash::vk;
use gpu_allocator::MemoryLocation;

use super::super::RendererError;
use super::super::liquify::{DAB_STRIDE, FIELD_FORMAT, LiquifyPipelines};
use super::super::resources::{Buffer, Image};
use super::{CANVAS_FORMAT, VulkanRenderer, create_framebuffer_for_view, full_image_barrier};
use crate::document::CompositeStep;
use crate::liquify::{LiquifyDab, MAX_DABS_PER_PASS};

/// Per-session GPU resources. Allocated on [`VulkanRenderer::begin_liquify`]
/// and freed on `end_liquify`, so an app that never liquifies pays nothing.
pub(in crate::renderer) struct LiquifySession {
    target_idx: usize,
    /// Pre-liquify snapshot of the target layer; every warp resamples this, so
    /// repeated strokes never compound resampling blur.
    source: Image,
    field: [Image; 2],
    field_framebuffer: [vk::Framebuffer; 2],
    /// Which `field` slot holds the current state.
    cur: usize,
    /// Region written into `field[cur]` that `field[1 - cur]` is missing.
    pending_copy: Option<vk::Rect2D>,
    warped: Image,
    warped_framebuffer: vk::Framebuffer,
    dab_buffer: Buffer,
    descriptor_pool: vk::DescriptorPool,
    /// Compose input sets, indexed by which field slot is the *source*.
    compose_sets: [vk::DescriptorSet; 2],
    /// Warp input sets, indexed by which field slot is the *source*.
    warp_sets: [vk::DescriptorSet; 2],
    /// Layer-composite-layout set sampling `warped`, for the preview splice.
    warped_set: vk::DescriptorSet,
    /// Canvas region the field has changed in since the last bake, or `None`
    /// when the layer is already up to date. Doubles as the "is there anything
    /// to bake" flag and as the bounds of the next history patch.
    dirty: Option<vk::Rect2D>,
    /// Whether `preview` holds a full composite of this session (false until the
    /// first one, and after anything that invalidates the whole image).
    preview_valid: bool,
    /// Region of `warped` that has changed since the last preview composite.
    /// `None` means the preview is already current.
    preview_dirty: Option<vk::Rect2D>,
}

impl VulkanRenderer {
    /// Whether a liquify session is live.
    #[must_use]
    pub fn liquify_active(&self) -> bool {
        self.liquify.is_some()
    }

    /// The layer the live session targets.
    #[must_use]
    pub fn liquify_target(&self) -> Option<usize> {
        self.liquify.as_ref().map(|s| s.target_idx)
    }

    /// Follow the target through a layer insertion at `at`.
    pub(super) fn liquify_shift_for_insert(&mut self, at: usize) {
        if let Some(session) = self.liquify.as_mut()
            && at <= session.target_idx
        {
            session.target_idx += 1;
        }
    }

    /// Follow the target through a layer removal at `at`. Returns true when the
    /// target itself was removed, in which case the caller ends the session -
    /// the field has nothing left to warp.
    pub(super) fn liquify_shift_for_remove(&mut self, at: usize) -> bool {
        let Some(session) = self.liquify.as_mut() else {
            return false;
        };
        if at == session.target_idx {
            return true;
        }
        if at < session.target_idx {
            session.target_idx -= 1;
        }
        false
    }

    /// Follow the target through a reorder. Mirrors `LayerStack::reorder`: the
    /// slot leaves `from` and is reinserted at `to`.
    pub(super) fn liquify_shift_for_reorder(&mut self, from: usize, to: usize) {
        let Some(session) = self.liquify.as_mut() else {
            return;
        };
        let idx = session.target_idx;
        session.target_idx = if idx == from {
            to
        } else if from < idx && idx <= to {
            idx - 1
        } else if to <= idx && idx < from {
            idx + 1
        } else {
            idx
        };
    }

    /// Whether the field has changed since the last bake, i.e. whether the
    /// layer is out of date with what the user is looking at.
    #[must_use]
    pub fn liquify_touched(&self) -> bool {
        self.liquify.as_ref().is_some_and(|s| s.dirty.is_some())
    }

    /// Canvas region `(x, y, w, h)` the pending bake would rewrite, for a
    /// bounded history patch. `None` when there is nothing to bake.
    #[must_use]
    pub fn liquify_dirty_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let rect = self.liquify.as_ref()?.dirty?;
        #[allow(clippy::cast_sign_loss)]
        Some((
            rect.offset.x as u32,
            rect.offset.y as u32,
            rect.extent.width,
            rect.extent.height,
        ))
    }

    /// Start liquifying `layer_idx`: snapshot its pixels and allocate the field.
    /// Replaces any session already running.
    pub fn begin_liquify(&mut self, layer_idx: usize) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        self.end_liquify();
        self.ensure_liquify_pipelines()?;
        // The preview splices the warped layer in place of the stored one, so
        // the shared below-stack cache has to be rebuilt for this target.
        self.preview_cache_valid = false;
        self.scoped_cache_valid = false;

        let extent = self.canvas_extent_2d();
        let pipelines = self.liquify_pipelines.as_ref().expect("ensured above");
        let field_render_pass = pipelines.field_render_pass;
        let set_layout = pipelines.set_layout;
        let linear = pipelines.linear_sampler;
        let nearest = pipelines.nearest_sampler;
        let canvas_render_pass = self.canvas_target.render_pass;
        let composite_layout = self.layer_composite_pipeline.descriptor_set_layout;
        let composite_sampler = self.layer_composite_pipeline.sampler;
        let selection_view = self.selection.mask.view;

        let source = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "liquify-source",
            CANVAS_FORMAT,
            extent,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        let field_usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST;
        let field_a = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "liquify-field-a",
            FIELD_FORMAT,
            extent,
            field_usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let field_b = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "liquify-field-b",
            FIELD_FORMAT,
            extent,
            field_usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let warped = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "liquify-warped",
            CANVAS_FORMAT,
            extent,
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC,
            vk::ImageAspectFlags::COLOR,
        )?;
        let field_framebuffer = [
            create_framebuffer_for_view(&self.device, field_render_pass, extent, field_a.view)?,
            create_framebuffer_for_view(&self.device, field_render_pass, extent, field_b.view)?,
        ];
        let warped_framebuffer =
            create_framebuffer_for_view(&self.device, canvas_render_pass, extent, warped.view)?;

        let dab_buffer = Buffer::new(
            &self.device,
            &mut self.allocator,
            "liquify-dabs",
            DAB_STRIDE * MAX_DABS_PER_PASS as u64,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            MemoryLocation::CpuToGpu,
        )?;

        // Four sets on the shared liquify layout (two samplers + one storage
        // buffer each) plus one on the layer-composite layout for the splice.
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 9,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 4,
            },
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&pool_sizes)
            .max_sets(5);
        let descriptor_pool = unsafe { self.device.create_descriptor_pool(&pool_info, None)? };

        let field_views = [field_a.view, field_b.view];
        let mut compose_sets = [vk::DescriptorSet::null(); 2];
        let mut warp_sets = [vk::DescriptorSet::null(); 2];
        for slot in 0..2 {
            compose_sets[slot] = self.alloc_liquify_set(
                descriptor_pool,
                set_layout,
                [(field_views[slot], linear), (selection_view, nearest)],
                &dab_buffer,
            )?;
            warp_sets[slot] = self.alloc_liquify_set(
                descriptor_pool,
                set_layout,
                [(source.view, linear), (field_views[slot], linear)],
                &dab_buffer,
            )?;
        }
        let warped_set = {
            let layouts = [composite_layout];
            let info = vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(descriptor_pool)
                .set_layouts(&layouts);
            let set = unsafe { self.device.allocate_descriptor_sets(&info)? }[0];
            let image_info = [vk::DescriptorImageInfo::default()
                .image_view(warped.view)
                .image_layout(vk::ImageLayout::GENERAL)
                .sampler(composite_sampler)];
            let writes = [vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&image_info)];
            unsafe { self.device.update_descriptor_sets(&writes, &[]) };
            set
        };

        let layer_image = self.layer_stack.slots[layer_idx].image.handle;
        let (source_h, warped_h) = (source.handle, warped.handle);
        let (field_a_h, field_b_h) = (field_a.handle, field_b.handle);
        self.record_and_submit(|this| {
            this.clip = None;
            for image in [source_h, warped_h, field_a_h, field_b_h] {
                unsafe {
                    this.device.cmd_pipeline_barrier(
                        this.command_buffer,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        vk::PipelineStageFlags::ALL_COMMANDS,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[full_image_barrier(
                            image,
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::GENERAL,
                        )],
                    );
                }
            }
            this.cmd_copy_image_full(layer_image, source_h);
            for image in [field_a_h, field_b_h] {
                this.cmd_clear_image(image, [0.0, 0.0, 0.0, 0.0]);
            }
            Ok(())
        })?;

        self.liquify = Some(LiquifySession {
            target_idx: layer_idx,
            source,
            field: [field_a, field_b],
            field_framebuffer,
            cur: 0,
            pending_copy: None,
            warped,
            warped_framebuffer,
            dab_buffer,
            descriptor_pool,
            compose_sets,
            warp_sets,
            warped_set,
            dirty: None,
            preview_valid: false,
            preview_dirty: None,
        });
        // The layer still holds its pre-liquify pixels, but the display must
        // start showing the (currently identity) warp so the splice is live.
        self.render_liquify_warp(None)
    }

    /// Fold a batch of dabs into the field. `group` is the stride of one
    /// symmetry group in `dabs` (see [`crate::liquify::expand`]); chunking never
    /// crosses a group boundary, because a dab and its mirror copy can be a
    /// whole canvas apart and would blow up the pass's scissor rect.
    pub fn liquify_dabs(&mut self, dabs: &[LiquifyDab], group: usize) -> Result<(), RendererError> {
        if self.liquify.is_none() || dabs.is_empty() {
            return Ok(());
        }
        let group = group.clamp(1, dabs.len());
        let stride = if self.split_by_symmetry_group(dabs, group) {
            group
        } else {
            dabs.len()
        };
        let mut touched: Option<vk::Rect2D> = None;
        for symmetry_group in dabs.chunks(stride) {
            for chunk in symmetry_group.chunks(MAX_DABS_PER_PASS) {
                if let Some(rect) = self.liquify_dab_chunk(chunk)? {
                    touched = Some(touched.map_or(rect, |prev| union_rect(prev, rect)));
                }
            }
        }
        // Only the field texels this batch wrote can change the warp, so the
        // resample is clipped to them plus a pixel of sampler slack.
        let Some(rect) = touched else {
            return Ok(());
        };
        let clip = grow_rect(rect, 1, self.canvas_extent_2d());
        // The preview only has to recomposite what the warp just redrew.
        if let Some(session) = self.liquify.as_mut() {
            session.preview_dirty =
                Some(session.preview_dirty.map_or(clip, |prev| union_rect(prev, clip)));
        }
        self.render_liquify_warp(Some(clip))
    }

    /// Zero the whole field - Photoshop's "Restore All". The layer goes back to
    /// its pristine shape on the next bake, so the whole canvas is marked dirty.
    pub fn liquify_restore_all(&mut self) -> Result<(), RendererError> {
        let Some(session) = self.liquify.as_ref() else {
            return Ok(());
        };
        let (a, b) = (session.field[0].handle, session.field[1].handle);
        self.record_and_submit(|this| {
            this.clip = None;
            this.cmd_clear_image(a, [0.0, 0.0, 0.0, 0.0]);
            this.cmd_clear_image(b, [0.0, 0.0, 0.0, 0.0]);
            Ok(())
        })?;
        let extent = self.canvas_extent_2d();
        if let Some(session) = self.liquify.as_mut() {
            session.cur = 0;
            session.pending_copy = None;
            session.dirty = Some(vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            });
            // The whole warp changed, so the preview can't be patched up.
            session.preview_valid = false;
            session.preview_dirty = None;
        }
        self.render_liquify_warp(None)
    }

    /// Copy the warped result into the target layer, **keeping the session
    /// open**. Called at the end of each stroke so every warp becomes an
    /// ordinary undoable pixel edit.
    ///
    /// The snapshot is deliberately not re-taken: the field keeps accumulating
    /// against the original pixels, so a run of strokes is still one resample
    /// rather than a stack of them.
    pub fn liquify_bake(&mut self) -> Result<(), RendererError> {
        let Some(session) = self.liquify.as_ref() else {
            return Ok(());
        };
        let Some(dirty) = session.dirty else {
            return Ok(());
        };
        let target_idx = session.target_idx;
        let warped = session.warped.handle;
        let Some(slot) = self.layer_stack.slots.get(target_idx) else {
            // The stack shrank under a live session. Callers close the session
            // before mutating the stack; refusing here keeps a missed one from
            // panicking inside the renderer.
            return Err(RendererError::LayerIndexOutOfRange);
        };
        let layer_image = slot.image.handle;
        // Clipped to the region the field actually changed, for two reasons.
        // Correctness: a full-canvas copy would restore warp(snapshot) over the
        // *whole* layer, silently reverting any other edit made during the
        // session (and leaving that revert out of the bounded history patch).
        // Cost: it also keeps the copy proportional to the brush rather than the
        // canvas. `warped` is already current - every field mutation re-runs the
        // warp - so this is a straight copy of what the user is looking at.
        self.record_and_submit(|this| {
            this.clip = Some(dirty);
            this.cmd_copy_image_full(warped, layer_image);
            this.clip = None;
            Ok(())
        })?;
        self.layer_stack.touch(target_idx);
        if let Some(session) = self.liquify.as_mut() {
            session.dirty = None;
        }
        Ok(())
    }

    /// Free the session's GPU resources without baking. Safe to call with no
    /// session. Callers that might have unbaked dabs run [`Self::liquify_bake`]
    /// first.
    pub fn end_liquify(&mut self) {
        let Some(session) = self.liquify.take() else {
            return;
        };
        self.invalidate_preview_cache();
        unsafe {
            let _ = self.device.device_wait_idle();
            for fb in session.field_framebuffer {
                self.device.destroy_framebuffer(fb, None);
            }
            self.device.destroy_framebuffer(session.warped_framebuffer, None);
            self.device.destroy_descriptor_pool(session.descriptor_pool, None);
            session.dab_buffer.destroy(&self.device, &mut self.allocator);
            let [field_a, field_b] = session.field;
            field_a.destroy(&self.device, &mut self.allocator);
            field_b.destroy(&self.device, &mut self.allocator);
            session.source.destroy(&self.device, &mut self.allocator);
            session.warped.destroy(&self.device, &mut self.allocator);
        }
    }

    /// Composite the preview with the warped layer spliced in at the target's
    /// z-order and blend mode. Caller presents `PresentSource::Preview`.
    ///
    /// Incremental: only the region the warp has touched since the last
    /// composite is redrawn, exactly like the brush's dab-clipped preview. This
    /// runs on every motion event, so a full-canvas composite here dominates the
    /// whole tool - it costs far more than the dab passes it is showing.
    pub fn render_liquify_preview(&mut self, visibilities: &[bool]) -> Result<(), RendererError> {
        let Some(session) = self.liquify.as_ref() else {
            return Ok(());
        };
        let target_idx = session.target_idx;
        let warped_set = session.warped_set;
        // A stale below-stack cache means something outside the warp changed, so
        // the whole preview has to be rebuilt; otherwise redraw just the dirty
        // region. `preview_valid` covers the first composite of a session.
        let full = !self.preview_cache_valid || !session.preview_valid;
        let clip = if full { None } else { session.preview_dirty };
        if !full && clip.is_none() {
            return Ok(()); // preview already shows the current warp
        }
        let (mode, opacity) = self.layer_stack.blend(target_idx);
        let visible = self.visible_layer_indices(visibilities);

        self.record_and_submit(|this| {
            this.clip = None;
            // Layers strictly below the target are constant for the whole
            // session, so they are cached exactly like a stroke's below-stack.
            if !this.preview_cache_valid {
                this.cmd_clear_image(this.preview_below.handle, [0.0, 0.0, 0.0, 0.0]);
                for &idx in &visible {
                    if idx >= target_idx {
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
            // Everything from here rebuilds only `clip`: outside it the preview
            // still holds the previous frame's result, which is unchanged.
            this.clip = clip;
            this.cmd_copy_image_full(this.preview_below.handle, this.preview.handle);
            for &idx in &visible {
                if idx < target_idx {
                    continue;
                }
                if idx == target_idx {
                    // The warp replaces the layer outright - unlike a transform,
                    // nothing is left behind on the stored image.
                    this.cmd_compose_layer_blended(
                        this.preview.handle,
                        this.preview_framebuffer,
                        warped_set,
                        mode,
                        opacity,
                    );
                } else {
                    this.preview_compose_layer(
                        this.preview.handle,
                        this.preview_framebuffer,
                        idx,
                    );
                }
            }
            this.clip = None;
            Ok(())
        })?;
        if let Some(session) = self.liquify.as_mut() {
            session.preview_valid = true;
            session.preview_dirty = None;
        }
        Ok(())
    }

    /// Adjustment-aware variant: runs the folder-scoped composite walk with the
    /// warped layer standing in for the target, so an adjustment above (or
    /// scoped around) it previews exactly as it will after commit.
    pub fn render_liquify_preview_scoped(
        &mut self,
        steps: &[CompositeStep],
        visibilities: &[bool],
    ) -> Result<(), RendererError> {
        let Some(session) = self.liquify.as_ref() else {
            return Ok(());
        };
        let target_idx = session.target_idx;
        let warped_img = session.warped.handle;
        let warped_set = session.warped_set;
        let visible = visibilities.get(target_idx).copied().unwrap_or(false);
        let (mode, opacity) = self.layer_stack.blend(target_idx);
        // `Filter` is the target kind that replaces the layer's pixels wholesale
        // rather than compositing over them, which is exactly liquify's shape.
        // A hidden target composes nothing; it must not fall back to the flat
        // preview, which skips adjustment slots and folder scope entirely.
        let target = super::adjust_ops::PreviewTarget::Filter {
            src_img: warped_img,
            set: warped_set,
            mode,
            opacity,
            visible,
        };
        self.build_preview_scoped(steps, target_idx, target)
    }

    /// Render the preview and read it back as BGRA8 (tests / diagnostics).
    pub fn read_liquify_preview(
        &mut self,
        visibilities: &[bool],
    ) -> Result<Vec<u8>, RendererError> {
        self.render_liquify_preview(visibilities)?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes()
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn ensure_liquify_pipelines(&mut self) -> Result<(), RendererError> {
        if self.liquify_pipelines.is_some() {
            return Ok(());
        }
        let pipelines = LiquifyPipelines::new(&self.device, self.canvas_target.render_pass)?;
        self.liquify_pipelines = Some(pipelines);
        Ok(())
    }

    fn alloc_liquify_set(
        &self,
        pool: vk::DescriptorPool,
        layout: vk::DescriptorSetLayout,
        images: [(vk::ImageView, vk::Sampler); 2],
        dabs: &Buffer,
    ) -> Result<vk::DescriptorSet, RendererError> {
        let layouts = [layout];
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let set = unsafe { self.device.allocate_descriptor_sets(&info)? }[0];
        // Each `DescriptorImageInfo` must outlive `update_descriptor_sets`, so
        // they are materialised before the writes reference them.
        let image_infos: Vec<[vk::DescriptorImageInfo; 1]> = images
            .iter()
            .map(|&(view, sampler)| {
                [vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(vk::ImageLayout::GENERAL)
                    .sampler(sampler)]
            })
            .collect();
        let buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(dabs.handle)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let mut writes: Vec<vk::WriteDescriptorSet> = image_infos
            .iter()
            .enumerate()
            .map(|(i, info)| {
                #[allow(clippy::cast_possible_truncation)]
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(i as u32)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(info)
            })
            .collect();
        writes.push(
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&buffer_info),
        );
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        Ok(set)
    }

    /// Copy `dabs` into the session's storage buffer. The liquify passes all
    /// submit synchronously, so the previous batch's read has already finished.
    fn upload_liquify_dabs(&mut self, dabs: &[LiquifyDab]) -> Result<(), RendererError> {
        let Some(session) = self.liquify.as_mut() else {
            return Ok(());
        };
        let mapped = session
            .dab_buffer
            .mapped_mut()
            .ok_or(RendererError::StagingNotMapped)?;
        let bytes = std::mem::size_of_val(dabs);
        // SAFETY: `LiquifyDab` is a `repr(C)` block of `f32`s matching the
        // shader's std430 layout, so its bytes are the buffer contents.
        let src = unsafe { std::slice::from_raw_parts(dabs.as_ptr().cast::<u8>(), bytes) };
        let n = src.len().min(mapped.len());
        mapped[..n].copy_from_slice(&src[..n]);
        Ok(())
    }

    /// Whether to run one compose pass per symmetry group instead of one pass
    /// for the whole batch.
    ///
    /// Splitting keeps each pass's scissor tight - a dab and its mirror copy can
    /// be a whole canvas apart, and unioning them rasterizes everything between.
    /// But every pass is its own submit *and fence wait*, so on a small batch the
    /// extra round-trips cost more than the pixels they save. Compare the two
    /// plans directly, charging a fixed pixel-equivalent for each extra wait.
    fn split_by_symmetry_group(&self, dabs: &[LiquifyDab], group: usize) -> bool {
        // Rough pixel-equivalent of one extra submit + fence wait. A blocking
        // round-trip between input events is worth well more than a small pass.
        const SUBMIT_COST_PX: u64 = 250_000;
        if group >= dabs.len() {
            return false; // one group; nothing to split
        }
        let area = |rect: Option<vk::Rect2D>| {
            rect.map_or(0, |r| u64::from(r.extent.width) * u64::from(r.extent.height))
        };
        let whole = area(self.liquify_dab_rect(dabs));
        let mut split = 0u64;
        let mut passes = 0u64;
        for symmetry_group in dabs.chunks(group) {
            split += area(self.liquify_dab_rect(symmetry_group));
            passes += 1;
        }
        split + passes.saturating_sub(1) * SUBMIT_COST_PX < whole
    }

    /// Canvas-clamped scissor rect covering every dab in `dabs`, or `None` when
    /// the batch lands entirely off-canvas.
    fn liquify_dab_rect(&self, dabs: &[LiquifyDab]) -> Option<vk::Rect2D> {
        let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
        let (mut max_x, mut max_y) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for dab in dabs {
            let (x0, y0, x1, y1) = dab.bounds();
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x1);
            max_y = max_y.max(y1);
        }
        let extent = self.canvas_extent_2d();
        #[allow(clippy::cast_precision_loss)]
        let (cw, ch) = (extent.width as f32, extent.height as f32);
        // A pixel of slack absorbs the fragment-centre offset at the rim.
        let x0 = (min_x - 1.0).floor().clamp(0.0, cw);
        let y0 = (min_y - 1.0).floor().clamp(0.0, ch);
        let x1 = (max_x + 1.0).ceil().clamp(0.0, cw);
        let y1 = (max_y + 1.0).ceil().clamp(0.0, ch);
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

    /// Push constants shared by every liquify pass.
    #[allow(clippy::cast_precision_loss)]
    fn liquify_push(&self, dab_count: usize) -> [f32; 4] {
        let extent = self.canvas_extent_2d();
        [
            extent.width as f32,
            extent.height as f32,
            dab_count as f32,
            if self.selection_active { 1.0 } else { 0.0 },
        ]
    }

    /// One compose pass: carry the previous rect forward into the destination,
    /// then fold this chunk into it. Returns the rect it wrote, or `None` when
    /// the chunk landed entirely off-canvas.
    fn liquify_dab_chunk(&mut self, dabs: &[LiquifyDab]) -> Result<Option<vk::Rect2D>, RendererError> {
        let Some(rect) = self.liquify_dab_rect(dabs) else {
            return Ok(None);
        };
        self.upload_liquify_dabs(dabs)?;
        let push = self.liquify_push(dabs.len());

        let Some(session) = self.liquify.as_ref() else {
            return Ok(None);
        };
        let src_slot = session.cur;
        let dst_slot = 1 - src_slot;
        let src_image = session.field[src_slot].handle;
        let dst_image = session.field[dst_slot].handle;
        let dst_framebuffer = session.field_framebuffer[dst_slot];
        let compose_set = session.compose_sets[src_slot];
        let carry = session.pending_copy;

        let pipelines = self.liquify_pipelines.as_ref().expect("session implies pipelines");
        let (render_pass, pipeline, layout) = (
            pipelines.field_render_pass,
            pipelines.compose,
            pipelines.pipeline_layout,
        );

        self.record_and_submit(|this| {
            // Bring the destination up to date outside the new rect: it is
            // missing exactly what the previous pass wrote into the source.
            if let Some(carry) = carry {
                this.clip = Some(carry);
                this.cmd_copy_image_full(src_image, dst_image);
            }
            this.clip = Some(rect);
            this.cmd_begin_fullscreen_pass(render_pass, dst_framebuffer, pipeline);
            unsafe {
                this.device.cmd_bind_descriptor_sets(
                    this.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[compose_set],
                    &[],
                );
                this.device.cmd_push_constants(
                    this.command_buffer,
                    layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push_bytes(&push),
                );
            }
            this.cmd_end_fullscreen_pass();
            this.clip = None;
            this.barrier(dst_image, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            Ok(())
        })?;

        if let Some(session) = self.liquify.as_mut() {
            session.cur = dst_slot;
            session.pending_copy = Some(rect);
            session.dirty = Some(session.dirty.map_or(rect, |prev| union_rect(prev, rect)));
        }
        Ok(Some(rect))
    }

    /// Re-resample the snapshot through the current field. `clip` bounds the
    /// pass to the field texels that changed: the warp is pointwise
    /// (`out(p) = source(p + D(p))`) over a snapshot that never changes, so
    /// every pixel outside that rect would recompute the value it already holds.
    /// `None` means full canvas, which only the initial warp and Restore All need.
    fn render_liquify_warp(&mut self, clip: Option<vk::Rect2D>) -> Result<(), RendererError> {
        let push = self.liquify_push(0);
        let Some(session) = self.liquify.as_ref() else {
            return Ok(());
        };
        let warp_set = session.warp_sets[session.cur];
        let framebuffer = session.warped_framebuffer;
        let warped_image = session.warped.handle;
        let pipelines = self.liquify_pipelines.as_ref().expect("session implies pipelines");
        let (pipeline, layout) = (pipelines.warp, pipelines.pipeline_layout);
        let render_pass = self.canvas_target.render_pass;

        self.record_and_submit(|this| {
            this.clip = clip;
            this.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
            unsafe {
                this.device.cmd_bind_descriptor_sets(
                    this.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[warp_set],
                    &[],
                );
                this.device.cmd_push_constants(
                    this.command_buffer,
                    layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push_bytes(&push),
                );
            }
            this.cmd_end_fullscreen_pass();
            this.clip = None;
            this.barrier(
                warped_image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })
    }
}

/// Expand `rect` by `pad` pixels on every side, clamped to the canvas. The warp
/// samples the field with a linear filter, so a texel that changed can bleed one
/// pixel outward.
fn grow_rect(rect: vk::Rect2D, pad: i32, extent: vk::Extent2D) -> vk::Rect2D {
    #[allow(clippy::cast_possible_wrap)]
    let (cw, ch) = (extent.width as i32, extent.height as i32);
    #[allow(clippy::cast_possible_wrap)]
    let x1 = (rect.offset.x + rect.extent.width as i32 + pad).clamp(0, cw);
    #[allow(clippy::cast_possible_wrap)]
    let y1 = (rect.offset.y + rect.extent.height as i32 + pad).clamp(0, ch);
    let x0 = (rect.offset.x - pad).clamp(0, x1);
    let y0 = (rect.offset.y - pad).clamp(0, y1);
    #[allow(clippy::cast_sign_loss)]
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    }
}

/// Smallest rect covering both inputs. Used to grow the pending-bake region as
/// a stroke lays down more dabs.
fn union_rect(a: vk::Rect2D, b: vk::Rect2D) -> vk::Rect2D {
    let x0 = a.offset.x.min(b.offset.x);
    let y0 = a.offset.y.min(b.offset.y);
    #[allow(clippy::cast_possible_wrap)]
    let x1 = (a.offset.x + a.extent.width as i32).max(b.offset.x + b.extent.width as i32);
    #[allow(clippy::cast_possible_wrap)]
    let y1 = (a.offset.y + a.extent.height as i32).max(b.offset.y + b.extent.height as i32);
    #[allow(clippy::cast_sign_loss)]
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    }
}

/// View a push-constant `vec4` as the bytes `cmd_push_constants` wants.
fn push_bytes(push: &[f32; 4]) -> &[u8] {
    // SAFETY: `[f32; 4]` is 16 contiguous bytes with no padding or invalid
    // bit patterns.
    unsafe { std::slice::from_raw_parts(push.as_ptr().cast::<u8>(), std::mem::size_of_val(push)) }
}
