//! GPU integration tests for the selection mask's boolean operations, driven
//! through the public `Canvas` API.
//!
//! The four mask ops (Replace / Add / Subtract / Intersect) are separate
//! pipelines that differ only in their color blend state, and they write the
//! R channel only because the mask is R8. A pipeline that widened that write
//! mask, or picked the wrong blend op, would corrupt the mask silently - the
//! composite would still render, just against the wrong region. These pin the
//! resulting region down by erasing through it and probing the alpha.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::brush_engine::Dab;
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::renderer::MaskBrushMode;
use oxiedraw_core::selection::{
    RectShape, SelectionShape, mask_brush_dabs, pixel_perfect_contours,
};
use oxiedraw_core::tools::SelectionMode;
use oxiedraw_utils::geometry::{Point, Size};

const SIZE: Size = Size {
    width: 16,
    height: 16,
};

/// Full-canvas opaque BGRA8 buffer.
fn opaque() -> Vec<u8> {
    let n = (SIZE.width * SIZE.height) as usize;
    let mut px = vec![0u8; n * 4];
    for chunk in px.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[0, 0, 255, 255]);
    }
    px
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> SelectionShape {
    SelectionShape::Rect(RectShape { x, y, w, h })
}

/// Erase through the current selection and return an alpha probe at (x, y).
fn erase_and_probe(canvas: &mut Canvas, idx: usize) -> impl Fn(usize, usize) -> u8 {
    canvas.erase_selection_in_layer(idx).unwrap();
    let out = canvas.read_layer(idx).unwrap();
    move |x: usize, y: usize| out[(y * SIZE.width as usize + x) * 4 + 3]
}

/// Add: left half OR right half covers everything, so the whole layer erases.
#[test]
fn add_unions_two_regions() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &opaque()).unwrap();

    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas
        .apply_selection_shape(&rect(8.0, 0.0, 8.0, 16.0), SelectionMode::Add)
        .unwrap();

    let alpha = erase_and_probe(&mut canvas, idx);
    assert_eq!(alpha(2, 8), 0, "left half should be erased");
    assert_eq!(alpha(13, 8), 0, "right half should be erased too (Add)");
}

/// Subtract: whole canvas minus the left half leaves only the right selected.
#[test]
fn subtract_removes_a_region() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &opaque()).unwrap();

    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 16.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Subtract)
        .unwrap();

    let alpha = erase_and_probe(&mut canvas, idx);
    assert_eq!(alpha(2, 8), 255, "left half was subtracted, must survive");
    assert_eq!(alpha(13, 8), 0, "right half stays selected and erases");
}

/// Intersect: left half AND top half leaves only the top-left quadrant.
#[test]
fn intersect_keeps_only_the_overlap() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &opaque()).unwrap();

    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 16.0, 8.0), SelectionMode::Intersect)
        .unwrap();

    let alpha = erase_and_probe(&mut canvas, idx);
    assert_eq!(alpha(2, 2), 0, "top-left overlap should be erased");
    assert_eq!(alpha(2, 13), 255, "bottom-left is outside the intersection");
    assert_eq!(alpha(13, 2), 255, "top-right is outside the intersection");
    assert_eq!(alpha(13, 13), 255, "bottom-right is outside both");
}

/// The folder-icon path (`select_from_layers_alpha`) must select the union of
/// the layers' actual painted alpha, not their bounding box or the whole canvas.
#[test]
fn select_from_layers_alpha_follows_the_painted_shape() {
    let mut canvas = Canvas::headless(SIZE).unwrap();

    // Two sparse layers: one paints the top-left quadrant, one the bottom-right.
    let mut a = vec![0u8; (SIZE.width * SIZE.height) as usize * 4];
    let mut b = a.clone();
    for y in 0..8usize {
        for x in 0..8usize {
            let i = (y * SIZE.width as usize + x) * 4;
            a[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    for y in 8..16usize {
        for x in 8..16usize {
            let i = (y * SIZE.width as usize + x) * 4;
            b[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    let ia = canvas.add_layer_with_pixels("a", &a).unwrap();
    let ib = canvas.add_layer_with_pixels("b", &b).unwrap();

    // A third, fully opaque layer to erase through - it shows the mask shape.
    let target = canvas.add_layer_with_pixels("target", &opaque()).unwrap();
    canvas.select_from_layers_alpha(&[ia, ib]).unwrap();

    let alpha = erase_and_probe(&mut canvas, target);
    assert_eq!(alpha(2, 2), 0, "top-left is painted on layer a: selected");
    assert_eq!(alpha(13, 13), 0, "bottom-right is painted on layer b: selected");
    assert_eq!(
        alpha(13, 2),
        255,
        "top-right is unpainted on both layers - selecting it means the mask \
         took the bounding box or the whole canvas instead of the alpha shape"
    );
    assert_eq!(alpha(2, 13), 255, "bottom-left is unpainted on both layers");
}

/// A folder containing an adjustment layer must still select only the painted
/// artwork. An adjustment layer's slot is a grayscale mask that starts fully
/// opaque, so unioning its alpha in would select the entire canvas.
#[test]
fn adjustment_layer_in_the_set_does_not_select_everything() {
    let mut canvas = Canvas::headless(SIZE).unwrap();

    // Artwork covering only the top-left quadrant.
    let mut art = vec![0u8; (SIZE.width * SIZE.height) as usize * 4];
    for y in 0..8usize {
        for x in 0..8usize {
            let i = (y * SIZE.width as usize + x) * 4;
            art[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    let painted = canvas.add_layer_with_pixels("art", &art).unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    let target = canvas.add_layer_with_pixels("target", &opaque()).unwrap();

    // Exactly what the folder icon passes for a group holding both.
    canvas.select_from_layers_alpha(&[painted, adj]).unwrap();

    let alpha = erase_and_probe(&mut canvas, target);
    assert_eq!(alpha(2, 2), 0, "the painted quadrant should be selected");
    assert_eq!(
        alpha(13, 13),
        255,
        "unpainted area must stay unselected - the adjustment layer's full-canvas \
         mask leaked into the union"
    );
}

// ---------------------------------------------------------------------------
// Mask brush (Add / Erase / Blur)
// ---------------------------------------------------------------------------

/// One centred dab covering most of the canvas, at `strength`.
fn centre_dab(strength: f32) -> Vec<Dab> {
    let mut dabs = Vec::new();
    let centre = Point::new(8.0, 8.0);
    mask_brush_dabs(centre, centre, (1.0, 1.0), 10.0, strength, f32::INFINITY, &mut dabs);
    dabs
}

fn mask_at(mask: &[u8], x: usize, y: usize) -> u8 {
    mask[y * SIZE.width as usize + x]
}

/// Add paints coverage into the mask with no prior selection: the mask starts
/// from empty rather than from whatever stale bytes it happened to hold.
#[test]
fn mask_brush_add_paints_from_empty() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas.begin_mask_brush().unwrap();
    canvas
        .stamp_mask_brush(&centre_dab(1.0), MaskBrushMode::Add, 5.0)
        .unwrap();
    canvas.end_mask_brush().unwrap();

    assert!(canvas.selection_active(), "painting starts a selection");
    let mask = canvas.read_selection_mask().unwrap();
    assert!(mask_at(&mask, 8, 8) > 250, "centre should be fully selected");
    assert_eq!(mask_at(&mask, 0, 0), 0, "the far corner is outside the dab");
}

/// Erase takes coverage back out of a live selection.
#[test]
fn mask_brush_erase_cuts_a_hole() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas.select_all().unwrap();
    canvas.begin_mask_brush().unwrap();
    canvas
        .stamp_mask_brush(&centre_dab(1.0), MaskBrushMode::Erase, 5.0)
        .unwrap();
    canvas.end_mask_brush().unwrap();

    let mask = canvas.read_selection_mask().unwrap();
    assert!(mask_at(&mask, 8, 8) < 5, "centre should be erased");
    assert_eq!(mask_at(&mask, 0, 0), 255, "outside the dab stays selected");
}

/// Strength builds up: holding the brush somewhere keeps pushing the mask
/// toward fully selected, like Blender's weight brush. A pass that only ever
/// reached `strength` would stall after the first event.
#[test]
fn mask_brush_add_builds_up_over_time() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas.begin_mask_brush().unwrap();

    let dabs = centre_dab(0.25);
    canvas
        .stamp_mask_brush(&dabs, MaskBrushMode::Add, 5.0)
        .unwrap();
    let once = mask_at(&canvas.read_selection_mask().unwrap(), 8, 8);
    canvas
        .stamp_mask_brush(&dabs, MaskBrushMode::Add, 5.0)
        .unwrap();
    let twice = mask_at(&canvas.read_selection_mask().unwrap(), 8, 8);
    for _ in 0..12 {
        canvas
            .stamp_mask_brush(&dabs, MaskBrushMode::Add, 5.0)
            .unwrap();
    }
    let held = mask_at(&canvas.read_selection_mask().unwrap(), 8, 8);
    canvas.end_mask_brush().unwrap();

    assert!(once > 50 && once < 80, "one pass at 25% strength, got {once}");
    assert!(twice > once + 20, "a second pass must add to the first: {once} -> {twice}");
    assert!(held > 240, "holding the brush should reach full, got {held}");
}

/// Same for Erase, in the other direction: repeated passes walk the mask down
/// to nothing rather than stopping at `1 - strength`.
#[test]
fn mask_brush_erase_builds_up_over_time() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas.select_all().unwrap();
    canvas.begin_mask_brush().unwrap();

    let dabs = centre_dab(0.25);
    canvas
        .stamp_mask_brush(&dabs, MaskBrushMode::Erase, 5.0)
        .unwrap();
    let once = mask_at(&canvas.read_selection_mask().unwrap(), 8, 8);
    canvas
        .stamp_mask_brush(&dabs, MaskBrushMode::Erase, 5.0)
        .unwrap();
    let twice = mask_at(&canvas.read_selection_mask().unwrap(), 8, 8);
    for _ in 0..24 {
        canvas
            .stamp_mask_brush(&dabs, MaskBrushMode::Erase, 5.0)
            .unwrap();
    }
    let held = mask_at(&canvas.read_selection_mask().unwrap(), 8, 8);
    canvas.end_mask_brush().unwrap();

    assert!(once > 175 && once < 210, "one pass at 25% strength, got {once}");
    assert!(twice + 20 < once, "a second pass must take more off: {once} -> {twice}");
    assert!(held < 15, "holding the brush should erase away, got {held}");
}

/// Erase and Blur need something to act on. Running them with no selection
/// used to make the (empty) mask live, which selects NOTHING - every later
/// stroke, fill and filter would be silently clipped away with only the ants
/// to explain it, and they draw nothing for an empty mask.
#[test]
fn mask_brush_erase_without_a_selection_stays_inactive() {
    for mode in [MaskBrushMode::Erase, MaskBrushMode::Feather] {
        let mut canvas = Canvas::headless(SIZE).unwrap();
        assert!(!canvas.selection_active(), "nothing selected to begin with");

        canvas.begin_mask_brush().unwrap();
        canvas.stamp_mask_brush(&centre_dab(1.0), mode, 5.0).unwrap();
        canvas.end_mask_brush().unwrap();

        assert!(
            !canvas.selection_active(),
            "{mode:?} with no selection left a live mask that selects nothing"
        );
    }

    // Add is the one mode that legitimately starts a selection from nothing.
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas.begin_mask_brush().unwrap();
    canvas
        .stamp_mask_brush(&centre_dab(1.0), MaskBrushMode::Add, 5.0)
        .unwrap();
    canvas.end_mask_brush().unwrap();
    assert!(canvas.selection_active(), "Add must start a selection");
}

/// The dab pass picks its blend from the renderer's build-up flag, which only a
/// paint stroke ever sets - and nothing resets it. The mask brush has to pin it
/// down, or its coverage (and so the strength slider) depends on which brush
/// preset the user last painted with.
#[test]
fn mask_brush_ignores_the_last_paint_preset() {
    // A short drag, so its dabs overlap heavily. A single dab can't tell the
    // two blends apart - they only differ where dabs stack.
    let mut dabs = Vec::new();
    mask_brush_dabs(
        Point::new(4.0, 8.0),
        Point::new(12.0, 8.0),
        (1.0, 1.0),
        10.0,
        0.25,
        f32::INFINITY,
        &mut dabs,
    );
    assert!(dabs.len() > 5, "need overlapping dabs, got {}", dabs.len());

    let coverage_after = |buildup: bool| {
        let mut canvas = Canvas::headless(SIZE).unwrap();
        // Stand in for a paint stroke with a build-up preset.
        canvas.set_stroke_buildup(buildup);
        canvas.begin_mask_brush().unwrap();
        canvas
            .stamp_mask_brush(&dabs, MaskBrushMode::Add, 5.0)
            .unwrap();
        let mask = canvas.read_selection_mask().unwrap();
        canvas.end_mask_brush().unwrap();
        mask_at(&mask, 8, 8)
    };

    let plain = coverage_after(false);
    let after_buildup_preset = coverage_after(true);
    assert_eq!(
        plain, after_buildup_preset,
        "one pass at 25% strength laid down {plain} normally but \
         {after_buildup_preset} after a build-up preset - the mask brush is \
         inheriting the paint stroke's dab blend"
    );
    assert!(
        plain < 128,
        "25% strength should be well short of full, got {plain}"
    );
}

/// Dab density has to follow the path, not the motion-event rate. Without a
/// spacing residual carried across events, a slow drag (many short segments)
/// deposits several times the coverage of a fast one over the same pixels, and
/// every segment boundary gets stamped twice.
#[test]
fn mask_brush_coverage_is_independent_of_event_rate() {
    let (from, to) = (Point::new(2.0, 8.0), Point::new(14.0, 8.0));
    let walk = |segments: usize| {
        let mut dabs = Vec::new();
        let mut carry = f32::INFINITY;
        for i in 0..segments {
            #[allow(clippy::cast_precision_loss)]
            let (t0, t1) = (i as f32 / segments as f32, (i + 1) as f32 / segments as f32);
            let a = Point::new((to.x - from.x).mul_add(t0, from.x), from.y);
            let b = Point::new((to.x - from.x).mul_add(t1, from.x), from.y);
            carry = mask_brush_dabs(a, b, (1.0, 1.0), 10.0, 1.0, carry, &mut dabs);
        }
        dabs.len()
    };

    // The same 12px path delivered as one fast event or 24 slow ones.
    let fast = walk(1);
    let slow = walk(24);
    assert!(
        slow.abs_diff(fast) <= 1,
        "the same path laid {fast} dabs in one event but {slow} across 24 - \
         dab spacing is restarting per event instead of carrying over"
    );
}

/// Blur softens whatever edge the brush covers, and keeps softening it while
/// the brush is held there: each pass blurs the mask the last one produced.
#[test]
fn mask_brush_blur_feathers_an_edge() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();

    // Brush over the middle of the vertical edge at x = 8.
    let mut dabs = Vec::new();
    let centre = Point::new(8.0, 8.0);
    mask_brush_dabs(centre, centre, (1.0, 1.0), 8.0, 1.0, f32::INFINITY, &mut dabs);

    canvas.begin_mask_brush().unwrap();
    canvas
        .stamp_mask_brush(&dabs, MaskBrushMode::Feather, 4.0)
        .unwrap();
    let once = canvas.read_selection_mask().unwrap();
    for _ in 0..8 {
        canvas
            .stamp_mask_brush(&dabs, MaskBrushMode::Feather, 4.0)
            .unwrap();
    }
    let held = canvas.read_selection_mask().unwrap();
    canvas.end_mask_brush().unwrap();

    let edge = mask_at(&once, 7, 8);
    assert!(
        edge < 250 && edge > 5,
        "the covered edge should be feathered, got {edge}"
    );
    assert!(
        mask_at(&once, 8, 8) > 5,
        "blur should bleed selection past the old boundary, got {}",
        mask_at(&once, 8, 8)
    );
    assert!(
        mask_at(&held, 7, 8) < edge,
        "holding the brush should keep softening: {edge} -> {}",
        mask_at(&held, 7, 8)
    );
    assert_eq!(
        mask_at(&held, 0, 0),
        255,
        "the far side is outside the brush and must be untouched"
    );
}

/// The blur must not show the shape of its own sampling grid. A wide brush
/// used to tap several pixels apart, which stamped visible blocks into the
/// gradient; the profile across a blurred edge has to stay monotonic.
#[test]
fn mask_brush_blur_leaves_a_smooth_gradient() {
    let size = Size { width: 64, height: 64 };
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .apply_selection_shape(
            &SelectionShape::Rect(RectShape { x: 0.0, y: 0.0, w: 32.0, h: 64.0 }),
            SelectionMode::Replace,
        )
        .unwrap();

    // A big brush over the whole edge - the case that used to go blocky.
    let mut dabs = Vec::new();
    mask_brush_dabs(
        Point::new(32.0, 0.0),
        Point::new(32.0, 64.0),
        (1.0, 1.0),
        64.0,
        1.0,
        f32::INFINITY,
        &mut dabs,
    );
    canvas.begin_mask_brush().unwrap();
    canvas
        .stamp_mask_brush(&dabs, MaskBrushMode::Feather, 32.0)
        .unwrap();
    canvas.end_mask_brush().unwrap();

    let mask = canvas.read_selection_mask().unwrap();
    let row = 32usize;
    let at = |x: usize| i32::from(mask[row * 64 + x]);
    for x in 1..64 {
        assert!(
            at(x) <= at(x - 1),
            "profile is not monotonic at x={x}: {} then {}",
            at(x - 1),
            at(x)
        );
    }

    // Blockiness is the sampling grid showing through: flat plateaus inside
    // the ramp, one per tap, with jumps between them. A blur whose taps are
    // about a pixel apart can't produce them.
    let (mut longest_run, mut run) = (0usize, 0usize);
    for x in 1..64 {
        let inside = at(x) > 10 && at(x) < 245;
        run = if inside && at(x) == at(x - 1) { run + 1 } else { 0 };
        longest_run = longest_run.max(run);
    }
    assert!(
        longest_run <= 3,
        "the ramp has a {longest_run}px flat step in it - the blur is sampling in blocks"
    );
}

// ---------------------------------------------------------------------------
// Live marching ants (downsampled edges readback)
// ---------------------------------------------------------------------------

/// Non-zero bounds of an R8 buffer as `(x0, y0, x1, y1)`, in its own pixels.
fn nonzero_bounds(buf: &[u8], w: u32) -> (f32, f32, f32, f32) {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (i, &v) in buf.iter().enumerate() {
        if v == 0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let (x, y) = ((i % w as usize) as f32, (i / w as usize) as f32);
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x + 1.0);
        y1 = y1.max(y + 1.0);
    }
    (x0, y0, x1, y1)
}

/// While the mask brush paints, the ants are retraced from the downsampled
/// edges buffer rather than the full mask - the full-resolution tracer costs
/// roughly ten times as much, which a live stroke can't afford. So the buffer
/// has to reflect the pass that just ran: it is read mid-stroke, one submit
/// after the blend wrote the mask.
#[test]
fn selection_edges_track_the_brush_mid_stroke() {
    let size = Size { width: 64, height: 64 };
    let mut canvas = Canvas::headless(size).unwrap();
    canvas.begin_mask_brush().unwrap();

    let empty = canvas.read_selection_edges().unwrap();
    assert_eq!(
        (empty.width, empty.height),
        (16, 16),
        "the edges buffer is the canvas downsampled 4x in each dim"
    );
    assert!(
        empty.bytes.iter().all(|&v| v == 0),
        "nothing is painted yet, so the trace has nothing to outline"
    );

    let mut dabs = Vec::new();
    let centre = Point::new(32.0, 32.0);
    mask_brush_dabs(centre, centre, (1.0, 1.0), 24.0, 1.0, f32::INFINITY, &mut dabs);
    canvas
        .stamp_mask_brush(&dabs, MaskBrushMode::Add, 12.0)
        .unwrap();

    // Deliberately no `end_mask_brush`: this is exactly what the live refresh
    // sees, mid-stroke, with the mask still being painted.
    let edges = canvas.read_selection_edges().unwrap();
    let at = |x: usize, y: usize| edges.bytes[y * edges.width as usize + x];
    assert!(
        at(8, 8) > 250,
        "the dab centre should read as selected, got {}",
        at(8, 8)
    );
    assert_eq!(at(0, 0), 0, "the far corner is outside the dab");

    // The UI traces this buffer and scales the result back into canvas space,
    // so the outline it draws has to land where the mask actually is.
    let contours = pixel_perfect_contours(&edges.bytes, edges.width, edges.height, 1);
    assert_eq!(contours.len(), 1, "one dab should trace to one loop");
    #[allow(clippy::cast_precision_loss)]
    let scale = size.width as f32 / edges.width as f32;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in &contours[0] {
        x0 = x0.min(p.x * scale);
        y0 = y0.min(p.y * scale);
        x1 = x1.max(p.x * scale);
        y1 = y1.max(p.y * scale);
    }

    let mask = canvas.read_selection_mask().unwrap();
    let (mx0, my0, mx1, my1) = nonzero_bounds(&mask, size.width);
    // One downsampled cell of slack in each direction: the 4x4 block only
    // carries one filtered value, so a faint fringe can fall below its floor.
    let slack = scale * 1.5;
    for (live, full, edge) in [(x0, mx0, "left"), (y0, my0, "top"), (x1, mx1, "right"), (y1, my1, "bottom")] {
        assert!(
            (live - full).abs() <= slack,
            "the live outline's {edge} edge is at {live}, the mask's at {full}"
        );
    }
}

/// The edges pass has to box-average its whole 4x4 block. A single bilinear
/// tap only covers the middle 2x2, so a mask thinner than the block can miss
/// every sampled texel and leave the live outline blank for the whole stroke.
#[test]
fn selection_edges_keep_thin_features() {
    let size = Size { width: 64, height: 64 };
    let mut canvas = Canvas::headless(size).unwrap();

    // A 1px column at x = 4: inside a 4x4 block (x = 4..8), but outside the
    // middle 2x2 (x = 5..7) that one linear tap would sample.
    let w = size.width as usize;
    let mut bytes = vec![0u8; w * size.height as usize];
    for row in bytes.chunks_exact_mut(w) {
        row[4] = 255;
    }
    canvas
        .apply_selection_shape(&SelectionShape::Mask(bytes), SelectionMode::Replace)
        .unwrap();

    let edges = canvas.read_selection_edges().unwrap();
    let column = edges.bytes[8 * edges.width as usize + 1];
    assert!(
        column > 0,
        "a 1px selection column vanished from the downsample - the live ants \
         would show nothing for it"
    );
    assert_eq!(
        edges.bytes[8 * edges.width as usize + 3],
        0,
        "blocks with no selection in them must stay empty"
    );
}

// ---------------------------------------------------------------------------
// Heatmap overlay (Blender-style mask preview)
// ---------------------------------------------------------------------------

/// Full-canvas opaque white BGRA8 buffer - a neutral backdrop for the tint.
fn white() -> Vec<u8> {
    let n = (SIZE.width * SIZE.height) as usize;
    vec![255u8; n * 4]
}

/// Full-canvas opaque black. Nothing of the backdrop survives the blend, so
/// the display shows the ramp color scaled by coverage and nothing else.
fn black() -> Vec<u8> {
    let n = (SIZE.width * SIZE.height) as usize;
    let mut px = vec![0u8; n * 4];
    for chunk in px.chunks_exact_mut(4) {
        chunk[3] = 255;
    }
    px
}

/// Present and read the display buffer back (BGRA8, premultiplied gamma).
fn present_and_read(canvas: &mut Canvas) -> Vec<u8> {
    canvas.present().unwrap();
    canvas.read_display().unwrap()
}

fn px(buf: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * SIZE.width as usize + x) * 4;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

/// A mask whose coverage steps up per column band, so one present exercises the
/// whole ramp: 0 (nothing), 25%, 50%, 100%.
fn stepped_mask() -> SelectionShape {
    let w = SIZE.width as usize;
    let mut bytes = vec![0u8; w * SIZE.height as usize];
    for row in bytes.chunks_exact_mut(w) {
        for (x, px) in row.iter_mut().enumerate() {
            *px = match x / 4 {
                0 => 0,
                1 => 64,
                2 => 128,
                _ => 255,
            };
        }
    }
    SelectionShape::Mask(bytes)
}

/// Marching ants only mark the 50% boundary, so the heatmap is what makes a
/// partial mask legible: coverage tints transparent -> blue -> green -> red.
#[test]
fn heatmap_tints_the_display_by_coverage() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas.add_layer_with_pixels("t", &white()).unwrap();
    canvas
        .apply_selection_shape(&stepped_mask(), SelectionMode::Replace)
        .unwrap();

    // Armed but not enabled: the canvas must render exactly as before.
    let plain = present_and_read(&mut canvas);
    for x in [1usize, 5, 9, 13] {
        assert_eq!(px(&plain, x, 8), [255, 255, 255, 255], "column {x} tinted early");
    }

    canvas.set_selection_heatmap(true);
    let heat = present_and_read(&mut canvas);

    assert_eq!(
        px(&heat, 1, 8),
        [255, 255, 255, 255],
        "zero coverage must draw nothing"
    );
    assert_eq!(
        px(&heat, 13, 8),
        [0, 0, 255, 255],
        "full coverage must be solid red"
    );

    let [b, g, r, a] = px(&heat, 9, 8);
    assert!(g > r && g > b, "half coverage should read green, got {:?}", [b, g, r]);
    assert_eq!(a, 255, "the overlay composites over an opaque layer");

    let [b, g, r, a] = px(&heat, 5, 8);
    assert!(b > r && g > r, "quarter coverage should read cyan, got {:?}", [b, g, r]);
    assert_eq!(a, 255, "the overlay composites over an opaque layer");
}

/// The ramp walks blue -> cyan -> green -> yellow -> red, so red and blue are
/// never lit together: no magenta or purple anywhere in it. Lerping blue
/// straight to red (or blue to green to red) breaks exactly this.
#[test]
fn heatmap_ramp_never_goes_purple() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas.add_layer_with_pixels("t", &black()).unwrap();

    // 16x16 = 256 pixels, so one pixel per possible coverage value.
    let every_value: Vec<u8> = (0..=255).collect();
    canvas
        .apply_selection_shape(&SelectionShape::Mask(every_value), SelectionMode::Replace)
        .unwrap();
    canvas.set_selection_heatmap(true);

    let heat = present_and_read(&mut canvas);
    for y in 0..SIZE.height as usize {
        for x in 0..SIZE.width as usize {
            let [b, _, r, _] = px(&heat, x, y);
            let coverage = y * SIZE.width as usize + x;
            assert!(
                b <= 1 || r <= 1,
                "coverage {coverage} mixed red and blue: {:?}",
                px(&heat, x, y)
            );
        }
    }

    // The named stops, at their exact positions on the ramp.
    let at = |coverage: usize| px(&heat, coverage % 16, coverage / 16);
    assert_eq!(at(0), [0, 0, 0, 255], "no coverage draws nothing");
    assert_eq!(at(255), [0, 0, 255, 255], "full coverage is red");
    // Quarter/half/three-quarter land on cyan/green/yellow, scaled by coverage.
    let [b, g, r, _] = at(64);
    assert!(b > 32 && g > 32 && r == 0, "quarter should be cyan, got {:?}", [b, g, r]);
    let [b, g, r, _] = at(128);
    assert!(g > 100 && r <= 1 && b <= 1, "half should be green, got {:?}", [b, g, r]);
    let [b, g, r, _] = at(191);
    assert!(g > 100 && r > 100 && b == 0, "three quarters should be yellow, got {:?}", [b, g, r]);
}

/// The mask image keeps its bytes after a deselect (they are don't-care), so
/// the overlay has to gate on the selection being live or it paints a ghost.
#[test]
fn heatmap_needs_a_live_selection() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas.add_layer_with_pixels("t", &white()).unwrap();
    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 16.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas.set_selection_heatmap(true);

    let selected = present_and_read(&mut canvas);
    assert_eq!(
        px(&selected, 8, 8),
        [0, 0, 255, 255],
        "expected the tint while selected"
    );

    canvas.deselect();
    let after = present_and_read(&mut canvas);
    assert_eq!(
        px(&after, 8, 8),
        [255, 255, 255, 255],
        "stale mask still tinting after deselect"
    );
}

/// Replace really replaces rather than accumulating with what came before.
#[test]
fn replace_discards_the_previous_mask() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &opaque()).unwrap();

    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas
        .apply_selection_shape(&rect(8.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();

    let alpha = erase_and_probe(&mut canvas, idx);
    assert_eq!(alpha(2, 8), 255, "the first (left) selection must be gone");
    assert_eq!(alpha(13, 8), 0, "only the second (right) selection erases");
}
