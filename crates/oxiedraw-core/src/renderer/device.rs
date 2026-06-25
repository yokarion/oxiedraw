use std::ffi::CStr;

use ash::ext::image_drm_format_modifier;
use ash::khr::external_memory_fd;
use ash::{Device, Instance, vk};

use super::RendererError;

/// Extensions required for the dmabuf zero-copy display path.
///
/// `VK_KHR_external_memory` is core in 1.1 so we don't enable it
/// here. The rest are EXT/KHR opt-ins.
const REQUIRED_DEVICE_EXTENSIONS: &[&CStr] = &[
    external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    image_drm_format_modifier::NAME,
];

pub(super) struct DeviceBundle {
    pub physical: vk::PhysicalDevice,
    pub device_name: String,
    pub queue_family: u32,
    pub device: Device,
    pub queue: vk::Queue,
    /// Nanoseconds per timestamp-query tick (0 if the queue lacks timestamps).
    pub timestamp_period: f32,
    /// Loader for `VK_KHR_external_memory_fd` - used by the dmabuf
    /// display image to export an fd from the allocated memory.
    pub external_memory_fd: external_memory_fd::Device,
    /// Loader for `VK_EXT_image_drm_format_modifier` - currently only
    /// used to query modifier properties of created images (for logging).
    /// Kept for symmetry with the modifier-probe code in `dmabuf.rs`.
    #[allow(dead_code)]
    pub image_drm_format_modifier: image_drm_format_modifier::Device,
}

pub(super) fn create(instance: &Instance) -> Result<DeviceBundle, RendererError> {
    let candidates = unsafe { instance.enumerate_physical_devices()? };
    if candidates.is_empty() {
        return Err(RendererError::NoDevice);
    }

    let needed_queue = vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER;
    let mut best: Option<(vk::PhysicalDevice, u32, i32)> = None;
    for pd in candidates {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        if !device_has_required_extensions(instance, pd)? {
            continue;
        }
        let q_props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let queue_family = q_props.iter().enumerate().find_map(|(i, p)| {
            if p.queue_flags.contains(needed_queue) {
                u32::try_from(i).ok()
            } else {
                None
            }
        });
        if let Some(qi) = queue_family {
            let score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            };
            if best.is_none_or(|(_, _, s)| score > s) {
                best = Some((pd, qi, score));
            }
        }
    }
    let (physical, queue_family, _) = best.ok_or(RendererError::NoQueueFamily)?;

    let props = unsafe { instance.get_physical_device_properties(physical) };
    let device_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    let priorities = [1.0_f32];
    let queue_infos = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family)
        .queue_priorities(&priorities)];
    let ext_ptrs: Vec<*const i8> = REQUIRED_DEVICE_EXTENSIONS
        .iter()
        .map(|n| n.as_ptr())
        .collect();
    let create_info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_infos)
        .enabled_extension_names(&ext_ptrs);
    let device = unsafe { instance.create_device(physical, &create_info, None)? };
    let queue = unsafe { device.get_device_queue(queue_family, 0) };

    let external_memory_fd = external_memory_fd::Device::new(instance, &device);
    let image_drm_format_modifier = image_drm_format_modifier::Device::new(instance, &device);

    tracing::info!(device = %device_name, queue_family, "Vulkan device created");

    Ok(DeviceBundle {
        physical,
        device_name,
        queue_family,
        device,
        queue,
        timestamp_period: props.limits.timestamp_period,
        external_memory_fd,
        image_drm_format_modifier,
    })
}

fn device_has_required_extensions(
    instance: &Instance,
    device: vk::PhysicalDevice,
) -> Result<bool, RendererError> {
    let available = unsafe { instance.enumerate_device_extension_properties(device)? };
    let names: std::collections::HashSet<&CStr> = available
        .iter()
        .map(|p| unsafe { CStr::from_ptr(p.extension_name.as_ptr()) })
        .collect();
    Ok(REQUIRED_DEVICE_EXTENSIONS
        .iter()
        .all(|need| names.contains(need)))
}
