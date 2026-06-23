//! Per-document component library.
//!
//! A component is a small self-contained sub-document (its own size and stack
//! of raster layers) that, once edited, is flattened into a cached "master"
//! BGRA8 texture. Component *instances* placed on the main canvas re-render
//! from that master at an arbitrary [`Placement`], so they stay crisp when
//! rescaled and update live when the component is edited.

use std::sync::atomic::{AtomicU64, Ordering};

use oxiedraw_utils::geometry::{Size, TransformFilter, TransformRect};
use oxiedraw_utils::pixels::transform_bgra8;
use serde::{Deserialize, Serialize};

use crate::document::{BlendMode, Placement};

fn default_opacity() -> f32 {
    1.0
}

/// Default edit canvas for a freshly created component.
pub const DEFAULT_COMPONENT_SIZE: Size = Size {
    width: 512,
    height: 512,
};

static COMPONENT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn generate_component_id() -> String {
    let n = COMPONENT_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("c{n:015x}")
}

/// One raster layer inside a component, with its pixels held in CPU memory
/// (BGRA8, `component.size`, row-major, premultiplied).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentLayer {
    pub id: String,
    pub name: String,
    pub visible: bool,
    /// Blend mode + opacity, so a layer's blend survives leaving edit mode and
    /// shows in the flattened master (absent in older files -> Normal/1.0).
    #[serde(default)]
    pub blend: BlendMode,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    pub pixels: Vec<u8>,
}

/// Self-contained, serializable copy of a component for the undo stack (add /
/// remove / rename). Master is rebuilt on restore, so it isn't stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentSnapshot {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub layers: Vec<ComponentLayer>,
    pub active_layer: Option<usize>,
}

impl ComponentSnapshot {
    #[must_use]
    pub fn into_component(self) -> Component {
        Component::from_parts(
            self.id,
            self.name,
            Size { width: self.width, height: self.height },
            self.layers,
            self.active_layer,
        )
    }
}

/// A reusable component: a mini-document plus its cached flattened render.
#[derive(Debug, Clone)]
pub struct Component {
    pub id: String,
    pub name: String,
    pub size: Size,
    pub layers: Vec<ComponentLayer>,
    pub active_layer: Option<usize>,
    /// Flattened BGRA8 render of all visible layers at `size`. Source texture
    /// for every instance and the card preview.
    pub master: Vec<u8>,
    /// Bumped on every edit; instances re-render when their cached version is
    /// stale.
    pub version: u64,
}

impl Component {
    /// A new empty component (default size, one transparent layer).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self::with_size(name, DEFAULT_COMPONENT_SIZE)
    }

    #[must_use]
    pub fn with_size(name: impl Into<String>, size: Size) -> Self {
        let blank = vec![0u8; (size.width * size.height * 4) as usize];
        Self {
            id: generate_component_id(),
            name: name.into(),
            size,
            layers: vec![ComponentLayer {
                id: format!("{}-0", generate_component_id()),
                name: "Layer 1".to_string(),
                visible: true,
                blend: BlendMode::Normal,
                opacity: 1.0,
                pixels: blank.clone(),
            }],
            active_layer: Some(0),
            master: blank,
            version: 0,
        }
    }

    /// Build a component directly from existing parts (used by project load).
    #[must_use]
    pub fn from_parts(
        id: String,
        name: String,
        size: Size,
        layers: Vec<ComponentLayer>,
        active_layer: Option<usize>,
    ) -> Self {
        let mut c = Self {
            id,
            name,
            size,
            layers,
            active_layer,
            master: Vec::new(),
            version: 0,
        };
        c.rebuild_master();
        c
    }

    /// Re-flatten all visible layers into the cached master and bump version.
    pub fn rebuild_master(&mut self) {
        self.master = flatten(&self.layers, self.size.width, self.size.height);
        self.version = self.version.wrapping_add(1);
    }

    /// Replace the layers (and active selection) with the given set - used when
    /// leaving edit mode - then rebuild the master.
    pub fn set_layers(&mut self, layers: Vec<ComponentLayer>, active: Option<usize>) {
        self.layers = layers;
        self.active_layer = active;
        self.rebuild_master();
    }

    /// Capture a serializable copy for the undo stack.
    #[must_use]
    pub fn to_snapshot(&self) -> ComponentSnapshot {
        ComponentSnapshot {
            id: self.id.clone(),
            name: self.name.clone(),
            width: self.size.width,
            height: self.size.height,
            layers: self.layers.clone(),
            active_layer: self.active_layer,
        }
    }

    /// `(id, name, visible, blend, opacity, pixels)` tuples for loading into a
    /// `Canvas` edit surface.
    #[must_use]
    pub fn layer_tuples(&self) -> Vec<(String, String, bool, BlendMode, f32, Vec<u8>)> {
        self.layers
            .iter()
            .map(|l| {
                (
                    l.id.clone(),
                    l.name.clone(),
                    l.visible,
                    l.blend,
                    l.opacity,
                    l.pixels.clone(),
                )
            })
            .collect()
    }

    /// Render this component's master into a canvas-sized BGRA8 buffer at
    /// `placement`, ready to write into an instance layer's slot.
    #[must_use]
    pub fn render_instance(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        placement: Placement,
        filter: TransformFilter,
    ) -> Vec<u8> {
        render_instance(
            &self.master,
            self.size.width,
            self.size.height,
            canvas_w,
            canvas_h,
            placement,
            filter,
        )
    }

    /// A default placement centring the component at its natural size over a
    /// canvas of the given size.
    #[must_use]
    pub fn default_placement(&self, canvas_w: u32, canvas_h: u32) -> Placement {
        #[allow(clippy::cast_precision_loss)]
        Placement::new(
            canvas_w as f32 / 2.0,
            canvas_h as f32 / 2.0,
            self.size.width as f32,
            self.size.height as f32,
            0.0,
        )
    }
}

/// The set of components owned by one document.
#[derive(Debug, Clone, Default)]
pub struct ComponentLibrary {
    pub components: Vec<Component>,
}

impl ComponentLibrary {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    pub fn len(&self) -> usize {
        self.components.len()
    }

    /// Create and insert a new empty component, returning its id.
    pub fn add_new(&mut self, name: impl Into<String>) -> String {
        let c = Component::new(name);
        let id = c.id.clone();
        self.components.push(c);
        id
    }

    pub fn push(&mut self, component: Component) {
        self.components.push(component);
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Component> {
        self.components.iter_mut().find(|c| c.id == id)
    }

    /// Remove the component with `id`. Returns it if present.
    pub fn remove(&mut self, id: &str) -> Option<Component> {
        let pos = self.components.iter().position(|c| c.id == id)?;
        Some(self.components.remove(pos))
    }

    /// Restore a snapshot (undo) at its original index, clamped to the end.
    pub fn insert_snapshot(&mut self, index: usize, snapshot: &ComponentSnapshot) {
        let pos = index.min(self.components.len());
        self.components.insert(pos, snapshot.clone().into_component());
    }

    /// Duplicate the component with `id`, inserting the copy directly after it.
    /// The copy gets fresh component and layer ids and a " copy" name suffix.
    /// Returns the new component's id if the source exists.
    pub fn duplicate(&mut self, id: &str) -> Option<String> {
        let pos = self.components.iter().position(|c| c.id == id)?;
        let src = &self.components[pos];
        let new_id = generate_component_id();
        let layers = src
            .layers
            .iter()
            .map(|l| ComponentLayer {
                id: format!("{}-{}", generate_component_id(), l.id),
                name: l.name.clone(),
                visible: l.visible,
                blend: l.blend,
                opacity: l.opacity,
                pixels: l.pixels.clone(),
            })
            .collect();
        let copy = Component {
            id: new_id.clone(),
            name: format!("{} copy", src.name),
            size: src.size,
            layers,
            active_layer: src.active_layer,
            master: src.master.clone(),
            version: 0,
        };
        self.components.insert(pos + 1, copy);
        Some(new_id)
    }
}

/// Composite visible layers bottom-to-top into a fresh `w x h` BGRA8 buffer,
/// honoring each layer's blend mode + opacity. Matches the GPU `layer_blend.frag`
/// (blend math in linear light) so the master looks like the edit-mode preview.
/// Layers that mismatch the expected length are skipped.
#[must_use]
pub fn flatten(layers: &[ComponentLayer], w: u32, h: u32) -> Vec<u8> {
    let n = (w * h * 4) as usize;
    let mut out = vec![0u8; n];
    for layer in layers {
        if !layer.visible || layer.pixels.len() != n {
            continue;
        }
        blend_layer_over(&mut out, &layer.pixels, layer.blend, layer.opacity);
    }
    out
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

// Blend `src` (premultiplied BGRA8) over `dst` in place using `mode` + `opacity`.
// Mirrors layer_blend.frag: unpremultiply, blend in linear light, W3C src-over.
fn blend_layer_over(dst: &mut [u8], src: &[u8], mode: BlendMode, opacity: f32) {
    let opacity = opacity.clamp(0.0, 1.0);
    let n = dst.len().min(src.len()) / 4;
    for px in 0..n {
        let o = px * 4;
        // Premultiplied linear samples (sRGB decode mimics the GPU sampler).
        let sa = f32::from(src[o + 3]) / 255.0 * opacity;
        let da = f32::from(dst[o + 3]) / 255.0;
        let s = [
            srgb_to_linear(f32::from(src[o]) / 255.0) * opacity,
            srgb_to_linear(f32::from(src[o + 1]) / 255.0) * opacity,
            srgb_to_linear(f32::from(src[o + 2]) / 255.0) * opacity,
        ];
        let d = [
            srgb_to_linear(f32::from(dst[o]) / 255.0),
            srgb_to_linear(f32::from(dst[o + 1]) / 255.0),
            srgb_to_linear(f32::from(dst[o + 2]) / 255.0),
        ];
        // Unpremultiply for the blend math.
        let unpre = |c: f32, a: f32| if a > 0.0 { c / a } else { 0.0 };
        let mut out = [0u8; 4];
        for ch in 0..3 {
            let sc = unpre(s[ch], sa);
            let dc = unpre(d[ch], da);
            let blended = match mode {
                BlendMode::Normal => sc,
                BlendMode::Multiply => sc * dc,
                BlendMode::Addition => (sc + dc).min(1.0),
                BlendMode::Darken => sc.min(dc),
                BlendMode::Screen => sc + dc - sc * dc,
                BlendMode::Overlay => {
                    if dc <= 0.5 {
                        2.0 * dc * sc
                    } else {
                        1.0 - 2.0 * (1.0 - dc) * (1.0 - sc)
                    }
                }
            };
            // Source colour mixed toward the blended colour by backdrop alpha,
            // then standard premultiplied src-over.
            let src_color = sc + (blended - sc) * da;
            let out_lin = src_color * sa + d[ch] * (1.0 - sa);
            out[ch] = (linear_to_srgb(out_lin).clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        let out_a = sa + da * (1.0 - sa);
        out[3] = (out_a.clamp(0.0, 1.0) * 255.0).round() as u8;
        dst[o..o + 4].copy_from_slice(&out);
    }
}

/// Affine-resample a master texture into a `canvas_w x canvas_h` slot at
/// `placement`. Identity (same size, centred, full coverage) reproduces the
/// master exactly.
#[must_use]
pub fn render_instance(
    master: &[u8],
    master_w: u32,
    master_h: u32,
    canvas_w: u32,
    canvas_h: u32,
    placement: Placement,
    filter: TransformFilter,
) -> Vec<u8> {
    #[allow(clippy::cast_precision_loss)]
    let original_rect = TransformRect::new(
        master_w as f32 / 2.0,
        master_h as f32 / 2.0,
        master_w as f32,
        master_h as f32,
        0.0,
    );
    transform_bgra8(
        master,
        master_w,
        master_h,
        canvas_w,
        canvas_h,
        original_rect,
        placement.to_rect(),
        filter,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Placement;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            v.extend_from_slice(&bgra);
        }
        v
    }

    #[test]
    fn placement_rect_roundtrip() {
        let p = Placement::new(10.0, 20.0, 30.0, 40.0, 0.5);
        let r = p.to_rect();
        let back = Placement::from_rect(r);
        assert_eq!(p, back);
    }

    #[test]
    fn flatten_opaque_top_wins() {
        let w = 2;
        let h = 1;
        let bottom = ComponentLayer {
            id: "b".into(),
            name: "b".into(),
            visible: true,
            blend: BlendMode::Normal,
            opacity: 1.0,
            pixels: solid(w, h, [255, 0, 0, 255]), // blue
        };
        let top = ComponentLayer {
            id: "t".into(),
            name: "t".into(),
            visible: true,
            blend: BlendMode::Normal,
            opacity: 1.0,
            pixels: solid(w, h, [0, 0, 255, 255]), // red
        };
        let out = flatten(&[bottom, top], w, h);
        // Opaque top fully covers: every pixel is red.
        assert_eq!(&out[0..4], &[0, 0, 255, 255]);
        assert_eq!(&out[4..8], &[0, 0, 255, 255]);
    }

    #[test]
    fn flatten_honors_blend_mode_and_opacity() {
        let (w, h) = (1, 1);
        let grey = |blend, opacity| ComponentLayer {
            id: "x".into(),
            name: "x".into(),
            visible: true,
            blend,
            opacity,
            pixels: solid(w, h, [180, 180, 180, 255]),
        };
        let base = grey(BlendMode::Normal, 1.0);

        // Multiply of two mid-greys is darker than the plain (Normal) top.
        let normal = flatten(&[base.clone(), grey(BlendMode::Normal, 1.0)], w, h);
        let multiply = flatten(&[base.clone(), grey(BlendMode::Multiply, 1.0)], w, h);
        assert!(
            multiply[0] < normal[0],
            "multiply should darken the master: {} !< {}",
            multiply[0],
            normal[0]
        );

        // A fully transparent (opacity 0) top leaves the base untouched.
        let faded = flatten(&[base, grey(BlendMode::Multiply, 0.0)], w, h);
        assert_eq!(&faded[0..3], &[180, 180, 180], "opacity 0 top is invisible");
    }

    #[test]
    fn flatten_skips_hidden() {
        let w = 1;
        let h = 1;
        let bottom = ComponentLayer {
            id: "b".into(),
            name: "b".into(),
            visible: true,
            blend: BlendMode::Normal,
            opacity: 1.0,
            pixels: solid(w, h, [255, 0, 0, 255]),
        };
        let hidden = ComponentLayer {
            id: "t".into(),
            name: "t".into(),
            visible: false,
            blend: BlendMode::Normal,
            opacity: 1.0,
            pixels: solid(w, h, [0, 0, 255, 255]),
        };
        let out = flatten(&[bottom, hidden], w, h);
        assert_eq!(&out[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn render_instance_identity_reproduces_master() {
        let w = 4;
        let h = 4;
        let master = solid(w, h, [10, 20, 30, 255]);
        let placement = Placement::new(2.0, 2.0, 4.0, 4.0, 0.0);
        let out = render_instance(
            &master,
            w,
            h,
            w,
            h,
            placement,
            TransformFilter::NearestNeighbor,
        );
        assert_eq!(out, master);
    }

    #[test]
    fn render_instance_into_larger_canvas_offsets_content() {
        let w = 2;
        let h = 2;
        let master = solid(w, h, [0, 0, 255, 255]);
        // Place the 2x2 master centred in an 8x8 canvas at natural size.
        let placement = Placement::new(4.0, 4.0, 2.0, 2.0, 0.0);
        let out = render_instance(
            &master,
            w,
            h,
            8,
            8,
            placement,
            TransformFilter::NearestNeighbor,
        );
        // Corner is transparent; centre pixel (3,3) is opaque red.
        assert_eq!(&out[0..4], &[0, 0, 0, 0]);
        let centre = ((3 * 8 + 3) * 4) as usize;
        assert_eq!(out[centre + 3], 255, "centre alpha should be opaque");
    }

    #[test]
    fn library_add_get_remove() {
        let mut lib = ComponentLibrary::new();
        let id = lib.add_new("Star");
        assert_eq!(lib.len(), 1);
        assert_eq!(lib.get(&id).unwrap().name, "Star");
        lib.get_mut(&id).unwrap().name = "Renamed".to_string();
        assert_eq!(lib.get(&id).unwrap().name, "Renamed");
        let removed = lib.remove(&id).unwrap();
        assert_eq!(removed.id, id);
        assert!(lib.is_empty());
    }

    #[test]
    fn library_duplicate_inserts_independent_copy_after_source() {
        let mut lib = ComponentLibrary::new();
        let a = lib.add_new("A");
        let b = lib.add_new("B");
        let copy_id = lib.duplicate(&a).unwrap();

        // Copy lands right after its source, with fresh id and " copy" name.
        assert_eq!(lib.len(), 3);
        assert_ne!(copy_id, a);
        let order: Vec<&str> = lib.components.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, vec![a.as_str(), copy_id.as_str(), b.as_str()]);
        assert_eq!(lib.get(&copy_id).unwrap().name, "A copy");

        // Layer ids are remapped so editing the copy can't touch the original.
        let src_layer = lib.get(&a).unwrap().layers[0].id.clone();
        let copy_layer = lib.get(&copy_id).unwrap().layers[0].id.clone();
        assert_ne!(src_layer, copy_layer);

        assert!(lib.duplicate("missing").is_none());
    }

    #[test]
    fn new_component_has_one_layer_and_master() {
        let c = Component::new("X");
        assert_eq!(c.size, DEFAULT_COMPONENT_SIZE);
        assert_eq!(c.layers.len(), 1);
        assert_eq!(c.active_layer, Some(0));
        assert_eq!(
            c.master.len(),
            (DEFAULT_COMPONENT_SIZE.width * DEFAULT_COMPONENT_SIZE.height * 4) as usize
        );
    }

    #[test]
    fn set_layers_rebuilds_master_and_bumps_version() {
        let mut c = Component::with_size("X", Size::new(2, 2));
        let v0 = c.version;
        c.set_layers(
            vec![ComponentLayer {
                id: "l".into(),
                name: "l".into(),
                visible: true,
                blend: BlendMode::Normal,
                opacity: 1.0,
                pixels: solid(2, 2, [0, 0, 255, 255]),
            }],
            Some(0),
        );
        assert!(c.version > v0);
        assert_eq!(&c.master[0..4], &[0, 0, 255, 255]);
    }
}
