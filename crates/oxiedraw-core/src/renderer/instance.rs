use std::ffi::{CStr, c_void};

use ash::ext::debug_utils;
use ash::{Entry, Instance, vk};

use super::RendererError;

const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";

pub(super) struct InstanceBundle {
    pub entry: Entry,
    pub instance: Instance,
    pub debug: Option<DebugMessenger>,
}

pub(super) struct DebugMessenger {
    pub loader: debug_utils::Instance,
    pub messenger: vk::DebugUtilsMessengerEXT,
}

pub(super) fn create() -> Result<InstanceBundle, RendererError> {
    let entry = unsafe { Entry::load()? };
    let want_validation = validation_enabled();

    let app_info = vk::ApplicationInfo::default()
        .application_name(c"OxieDraw")
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(c"OxieDraw Core")
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(vk::API_VERSION_1_3);

    let mut layer_ptrs: Vec<*const i8> = Vec::new();
    let mut ext_ptrs: Vec<*const i8> = Vec::new();

    let validation = want_validation && validation_layer_available(&entry)?;
    if want_validation && !validation {
        tracing::warn!("validation requested but VK_LAYER_KHRONOS_validation not available");
    }
    if validation {
        layer_ptrs.push(VALIDATION_LAYER.as_ptr());
        ext_ptrs.push(debug_utils::NAME.as_ptr());
    }

    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_layer_names(&layer_ptrs)
        .enabled_extension_names(&ext_ptrs);

    let instance = unsafe { entry.create_instance(&create_info, None)? };

    let debug = if validation {
        Some(create_debug_messenger(&entry, &instance)?)
    } else {
        None
    };

    Ok(InstanceBundle {
        entry,
        instance,
        debug,
    })
}

fn validation_layer_available(entry: &Entry) -> Result<bool, RendererError> {
    let layers = unsafe { entry.enumerate_instance_layer_properties()? };
    Ok(layers.iter().any(|p| {
        let name = unsafe { CStr::from_ptr(p.layer_name.as_ptr()) };
        name == VALIDATION_LAYER
    }))
}

fn create_debug_messenger(
    entry: &Entry,
    instance: &Instance,
) -> Result<DebugMessenger, RendererError> {
    let loader = debug_utils::Instance::new(entry, instance);
    let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(
            vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                | vk::DebugUtilsMessageSeverityFlagsEXT::INFO,
        )
        .message_type(
            vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
        )
        .pfn_user_callback(Some(debug_callback));
    let messenger = unsafe { loader.create_debug_utils_messenger(&info, None)? };
    Ok(DebugMessenger { loader, messenger })
}

fn validation_enabled() -> bool {
    if cfg!(debug_assertions) {
        return true;
    }
    std::env::var("OXIEDRAW_VK_VALIDATION")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    ty: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user: *mut c_void,
) -> vk::Bool32 {
    let msg = unsafe { CStr::from_ptr((*data).p_message) }.to_string_lossy();
    let kind = if ty.contains(vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE) {
        "perf"
    } else if ty.contains(vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION) {
        "valid"
    } else {
        "gen"
    };
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        tracing::error!(target: "vulkan", kind, "{msg}");
    } else if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        tracing::warn!(target: "vulkan", kind, "{msg}");
    } else if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::INFO) {
        tracing::info!(target: "vulkan", kind, "{msg}");
    } else {
        tracing::debug!(target: "vulkan", kind, "{msg}");
    }
    vk::FALSE
}
