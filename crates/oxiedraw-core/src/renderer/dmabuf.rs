use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use ash::khr::external_memory_fd;
use ash::{Device, Instance, vk};

use super::RendererError;

/// DRM fourcc for "8:8:8:8 ARGB" - bytes in memory order BGRA on
/// little-endian. GTK imports this as sRGB-encoded, premultiplied; the
/// present pass writes premultiplied-gamma pixels into a `B8G8R8A8_UNORM`
/// image (verbatim, no re-encode) so the stored bytes carry exactly that.
pub(super) const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;

/// Vulkan format of the display dmabuf image. UNORM so the present pass's
/// premultiplied-gamma output is stored without a second sRGB encode.
pub(super) const DISPLAY_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// `DRM_FORMAT_MOD_LINEAR` - the universal "no tiling, no compression"
/// layout. Every dmabuf-capable compositor speaks this. The trade-off
/// is the GPU can't use its native swizzled/compressed layout for the
/// display image, but we only blit into it once per frame so the
/// extra bandwidth is small compared to the readback path it replaces.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Display-side dmabuf image.
///
/// Allocated with `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` so the
/// memory layout is one the kernel + the display server can ingest
/// directly. The fd is held as `OwnedFd` so we don't accidentally
/// leak it; on drop it goes back to the kernel.
///
/// Memory is allocated manually (not via `gpu-allocator`) because we
/// need `VkExportMemoryAllocateInfo` + dedicated allocation, which the
/// allocator's normal block-suballocation scheme can't provide.
pub(super) struct DmabufImage {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub memory: vk::DeviceMemory,
    /// `Arc` so the UI side can hold the fd across frames without us
    /// dropping it on the next renderer mutation.
    pub fd: Arc<OwnedFd>,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub offset: u64,
    pub stride: u64,
}

/// Snapshot of everything GTK needs to wrap this image in a
/// `gdk::DmabufTexture`.
#[derive(Debug, Clone)]
pub struct DmabufDescriptor {
    pub fd: Arc<OwnedFd>,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
}

impl DmabufImage {
    pub(super) fn new(
        instance: &Instance,
        physical: vk::PhysicalDevice,
        device: &Device,
        external_memory_fd: &external_memory_fd::Device,
        width: u32,
        height: u32,
    ) -> Result<Self, RendererError> {
        // UNORM (not sRGB): the present pass writes already-sRGB-encoded,
        // premultiplied-gamma bytes and we want them stored verbatim.
        let format = DISPLAY_FORMAT;
        // COLOR_ATTACHMENT: the present pass renders straight into this image
        // (the colour-space conversion). TRANSFER/SAMPLED kept for flexibility.
        let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::SAMPLED;

        let modifier = pick_modifier(instance, physical, format, usage)?;
        tracing::info!(
            modifier = format!("0x{modifier:016x}"),
            "selected dmabuf modifier"
        );

        let image = create_image(device, format, width, height, usage, modifier)?;
        let (memory, size) = allocate_export_memory(instance, physical, device, image)?;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let fd = Arc::new(export_fd(external_memory_fd, memory)?);
        let layout = query_layout(device, image);
        let view = create_view(device, image, format)?;

        tracing::info!(
            width,
            height,
            size,
            offset = layout.offset,
            stride = layout.row_pitch,
            "dmabuf display image ready",
        );

        Ok(Self {
            image,
            view,
            memory,
            fd,
            width,
            height,
            fourcc: DRM_FORMAT_ARGB8888,
            modifier,
            offset: layout.offset,
            stride: layout.row_pitch,
        })
    }

    pub(super) fn descriptor(&self) -> DmabufDescriptor {
        DmabufDescriptor {
            fd: Arc::clone(&self.fd),
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
            modifier: self.modifier,
            offset: u32::try_from(self.offset).expect("offset fits"),
            stride: u32::try_from(self.stride).expect("stride fits"),
        }
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing this image is in flight.
    /// The fd may still be held by outstanding `DmabufDescriptor`s; that's
    /// fine, the `Arc` keeps it alive until the last reference drops.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
        // self.fd: Arc<OwnedFd> drops on this fn return - fd closes when
        // the last Arc reference goes away.
    }
}

fn pick_modifier(
    instance: &Instance,
    physical: vk::PhysicalDevice,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
) -> Result<u64, RendererError> {
    // Two-call query: first to learn the count, second to fill the
    // properties array.
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut props2 = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(physical, format, &mut props2) };
    let count = list.drm_format_modifier_count as usize;
    if count == 0 {
        return Err(RendererError::NoCompatibleModifier);
    }

    let mut props_vec = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_vec);
    let mut props2 = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(physical, format, &mut props2) };

    // Check each modifier against an actual image-format query. We
    // try LINEAR first because every dmabuf consumer (compositor,
    // GTK) supports it; vendor-specific compressed modifiers vary in
    // compositor support and would need a per-frame fallback path.
    let queue_family_indices: [u32; 0] = [];
    let mut ordered: Vec<u64> = props_vec.iter().map(|p| p.drm_format_modifier).collect();
    ordered.sort_by_key(|&m| u32::from(m != DRM_FORMAT_MOD_LINEAR));
    for &modifier in &ordered {
        let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .queue_family_indices(&queue_family_indices);
        let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .push_next(&mut modifier_info)
            .push_next(&mut external_info);
        let mut out = vk::ImageFormatProperties2::default();
        let result = unsafe {
            instance.get_physical_device_image_format_properties2(physical, &format_info, &mut out)
        };
        if result.is_ok() {
            return Ok(modifier);
        }
    }
    Err(RendererError::NoCompatibleModifier)
}

fn create_image(
    device: &Device,
    format: vk::Format,
    width: u32,
    height: u32,
    usage: vk::ImageUsageFlags,
    modifier: u64,
) -> Result<vk::Image, RendererError> {
    let modifiers = [modifier];
    let mut modifier_list =
        vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut modifier_list)
        .push_next(&mut external);
    Ok(unsafe { device.create_image(&info, None)? })
}

fn allocate_export_memory(
    instance: &Instance,
    physical: vk::PhysicalDevice,
    device: &Device,
    image: vk::Image,
) -> Result<(vk::DeviceMemory, vk::DeviceSize), RendererError> {
    let reqs = unsafe { device.get_image_memory_requirements(image) };
    let memory_type_index = find_memory_type(
        instance,
        physical,
        reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )?;

    let mut export_info = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // The spec recommends a dedicated allocation for any exported
    // image - and several drivers require it.
    let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut export_info)
        .push_next(&mut dedicated_info);
    let memory = unsafe { device.allocate_memory(&alloc_info, None)? };
    Ok((memory, reqs.size))
}

fn find_memory_type(
    instance: &Instance,
    physical: vk::PhysicalDevice,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Result<u32, RendererError> {
    let props = unsafe { instance.get_physical_device_memory_properties(physical) };
    for i in 0..props.memory_type_count {
        let allowed = (type_bits & (1 << i)) != 0;
        let has_props = props.memory_types[i as usize]
            .property_flags
            .contains(required);
        if allowed && has_props {
            return Ok(i);
        }
    }
    Err(RendererError::NoCompatibleMemory)
}

fn export_fd(
    loader: &external_memory_fd::Device,
    memory: vk::DeviceMemory,
) -> Result<OwnedFd, RendererError> {
    let info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw = unsafe { loader.get_memory_fd(&info)? };
    if raw < 0 {
        return Err(RendererError::DmabufExportFailed);
    }
    // SAFETY: vkGetMemoryFdKHR returns an fd we own per spec.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn query_layout(device: &Device, image: vk::Image) -> vk::SubresourceLayout {
    let subresource = vk::ImageSubresource::default()
        .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT)
        .mip_level(0)
        .array_layer(0);
    unsafe { device.get_image_subresource_layout(image, subresource) }
}

fn create_view(
    device: &Device,
    image: vk::Image,
    format: vk::Format,
) -> Result<vk::ImageView, RendererError> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    Ok(unsafe { device.create_image_view(&info, None)? })
}
