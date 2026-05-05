use ash::vk;

#[derive(Debug, thiserror::Error)]
pub enum RendererError {
    #[error("failed to load Vulkan loader: {0}")]
    Loader(#[from] ash::LoadingError),
    #[error("Vulkan API error: {0}")]
    Vulkan(#[from] vk::Result),
    #[error("no Vulkan-capable physical device found")]
    NoDevice,
    #[error("no graphics+transfer queue family on the selected device")]
    NoQueueFamily,
    #[error("GPU allocator: {0}")]
    Allocator(#[from] gpu_allocator::AllocationError),
    #[error("staging buffer is not host-visible")]
    StagingNotMapped,
    #[error("no DRM format modifier supports BGRA8 + DMA-BUF with the required usage flags")]
    NoCompatibleModifier,
    #[error("no Vulkan memory type satisfies the required property flags")]
    NoCompatibleMemory,
    #[error("vkGetMemoryFdKHR returned an invalid fd")]
    DmabufExportFailed,
    #[error("layer index out of range")]
    LayerIndexOutOfRange,
    #[error("hit the per-document layer limit")]
    LayerLimit,
    #[error("transform output dimension {requested} exceeds GPU image limit {limit}")]
    TransformTooLarge { requested: u32, limit: u32 },
    #[error("pattern atlas is full")]
    PatternAtlasFull,
}
