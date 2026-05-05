/// Internal layer clipboard. Holds full-canvas BGRA8 pixels plus enough
/// metadata to recreate the layer exactly. Used by copy/paste to preserve
/// all layer properties when pasting within the same app session.
#[derive(Clone)]
pub(crate) struct LayerClipboard {
    pub(crate) name: String,
    /// Premultiplied BGRA8, `canvas_width x canvas_height`, row-major.
    pub(crate) pixels: Vec<u8>,
    pub(crate) canvas_width: u32,
    pub(crate) canvas_height: u32,
}
