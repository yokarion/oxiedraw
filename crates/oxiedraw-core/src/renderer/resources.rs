use ash::{Device, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator};

use super::RendererError;

pub(super) struct Image {
    pub handle: vk::Image,
    pub view: vk::ImageView,
    #[allow(dead_code)]
    pub format: vk::Format,
    pub extent: vk::Extent3D,
    allocation: Option<Allocation>,
}

impl Image {
    pub(super) fn new_2d(
        device: &Device,
        allocator: &mut Allocator,
        name: &str,
        format: vk::Format,
        extent: vk::Extent2D,
        usage: vk::ImageUsageFlags,
        aspect: vk::ImageAspectFlags,
    ) -> Result<Self, RendererError> {
        let extent_3d = vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        };
        let create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(extent_3d)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let handle = unsafe { device.create_image(&create_info, None)? };
        let requirements = unsafe { device.get_image_memory_requirements(handle) };
        let allocation = allocator.allocate(&AllocationCreateDesc {
            name,
            requirements,
            location: MemoryLocation::GpuOnly,
            linear: false,
            allocation_scheme: AllocationScheme::GpuAllocatorManaged,
        })?;
        unsafe { device.bind_image_memory(handle, allocation.memory(), allocation.offset())? };

        let view_info = vk::ImageViewCreateInfo::default()
            .image(handle)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let view = unsafe { device.create_image_view(&view_info, None)? };

        Ok(Self {
            handle,
            view,
            format,
            extent: extent_3d,
            allocation: Some(allocation),
        })
    }

    /// # Safety
    /// Caller must ensure the image is no longer in flight on the GPU.
    pub(super) unsafe fn destroy(mut self, device: &Device, allocator: &mut Allocator) {
        unsafe { device.destroy_image_view(self.view, None) };
        unsafe { device.destroy_image(self.handle, None) };
        if let Some(a) = self.allocation.take() {
            let _ = allocator.free(a);
        }
    }
}

pub(super) struct Buffer {
    pub handle: vk::Buffer,
    #[allow(dead_code)]
    pub size: vk::DeviceSize,
    allocation: Option<Allocation>,
}

impl Buffer {
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        name: &str,
        size: vk::DeviceSize,
        usage: vk::BufferUsageFlags,
        location: MemoryLocation,
    ) -> Result<Self, RendererError> {
        let create_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let handle = unsafe { device.create_buffer(&create_info, None)? };
        let requirements = unsafe { device.get_buffer_memory_requirements(handle) };
        let allocation = allocator.allocate(&AllocationCreateDesc {
            name,
            requirements,
            location,
            linear: true,
            allocation_scheme: AllocationScheme::GpuAllocatorManaged,
        })?;
        unsafe { device.bind_buffer_memory(handle, allocation.memory(), allocation.offset())? };
        Ok(Self {
            handle,
            size,
            allocation: Some(allocation),
        })
    }

    pub(super) fn mapped(&self) -> Option<&[u8]> {
        self.allocation.as_ref().and_then(Allocation::mapped_slice)
    }

    pub(super) fn mapped_mut(&mut self) -> Option<&mut [u8]> {
        self.allocation
            .as_mut()
            .and_then(Allocation::mapped_slice_mut)
    }

    /// # Safety
    /// Caller must ensure the buffer is no longer in flight on the GPU.
    pub(super) unsafe fn destroy(mut self, device: &Device, allocator: &mut Allocator) {
        unsafe { device.destroy_buffer(self.handle, None) };
        if let Some(a) = self.allocation.take() {
            let _ = allocator.free(a);
        }
    }
}
