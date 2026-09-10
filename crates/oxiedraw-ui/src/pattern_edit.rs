//! The Pattern tool's live curve: draw it, drag its points, see the pattern
//! grow along it, and bake it into the layer on Apply or on the way out.
//!
//! Nothing is committed while the tool is active. The curve and its handles are
//! a cairo overlay; the pattern under them is handed to the canvas as a GPU
//! overlay spliced in at the target layer's z-order, so the live pattern goes
//! through that layer's adjustments, blend mode, opacity, clipping mask and
//! enclosing folders exactly as the applied pixels will. Apply runs the same
//! pass straight into the layer, which is what keeps preview and result
//! identical.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::{Color, ColorState};
use oxiedraw_core::guides::{GuideState, Symmetry};
use oxiedraw_core::history::{HistoryAction, HistoryStack, LayerPatch};
use oxiedraw_core::patterns::{PatternState, symmetry_copies};
use oxiedraw_patterns::{Bindings, CoverageTile, PatternRequest, RasterOptions, Side, SpineNode};
use oxiedraw_utils::geometry::{Point, Size};

use crate::canvas::RedrawHandle;
use crate::canvas_paintable::CanvasPaintable;

/// How close, in screen pixels, the pointer must be to grab a handle.
const HANDLE_HIT_PX: f32 = 11.0;

/// Shortest move that adds a point while drawing, in canvas pixels. The spine
/// resamples at a fixed step anyway, so near-duplicates buy nothing but work.
const MIN_STEP: f32 = 3.0;

/// How far, in canvas pixels, the drawn line may stray from the points kept to
/// describe it. The one knob that decides how many points a stroke costs: a
/// straight run stays under it however long it is, and only real curvature
/// spends points.
const FIT_TOLERANCE: f32 = 4.0;

/// Closest two kept points may sit, in canvas pixels. A shaky hand on a
/// straight line otherwise plants a cluster of points chasing its own jitter.
const MIN_NODE_GAP: f32 = 7.0;

/// How close, in screen pixels, a click must be to the curve to add a point.
/// Wider than the node reach: the curve is a thin line and you are aiming at
/// a stretch of it, not a target.
const SEGMENT_HIT_PX: f32 = 8.0;

/// Gap between side markers along the curve, in canvas pixels.
const MARK_SPACING: f32 = 70.0;

/// One tick of the "this side is the body" marker: a point on the curve and the
/// unit normal pointing into the solid side. Until the fur is grown nothing else
/// on screen says which side that is. The normal is always the left one;
/// `Flip side` negates it at draw time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SideMark {
    pub at: Point,
    pub normal: Point,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Hover {
    Node(usize),
    /// `before` is the node a new point would be inserted in front of, `at` the
    /// spot on the curve under the pointer.
    Segment { before: usize, at: Point },
    Nothing,
}

/// Turns the stream of pointer samples into the few points that describe the
/// line, as it is being drawn.
///
/// Douglas-Peucker applied forwards: when the run since the last kept point no
/// longer fits a straight segment within `FIT_TOLERANCE`, keep the sample that
/// strayed furthest and start again from there. That sample is already behind
/// the pen, so later ones cannot improve on it and no look-back is needed.
struct CurveBuilder {
    /// Points that are final.
    kept: Vec<Point>,
    /// Samples since the last kept point, starting with it.
    run: Vec<Point>,
}

impl CurveBuilder {
    fn new(start: Point) -> Self {
        Self {
            kept: vec![start],
            run: vec![start],
        }
    }

    /// Feed a sample. Returns `true` when it caused a point to be placed.
    fn push(&mut self, at: Point) -> bool {
        self.run.push(at);
        if self.run.len() < 3 {
            return false;
        }
        let (from, to) = (self.run[0], at);
        let Some((index, deviation)) = furthest_from_chord(&self.run, from, to) else {
            return false;
        };
        if deviation <= FIT_TOLERANCE {
            return false;
        }
        let pivot = self.run[index];
        if (pivot.x - from.x).hypot(pivot.y - from.y) < MIN_NODE_GAP {
            return false;
        }
        self.kept.push(pivot);
        self.run.drain(..index);
        true
    }

    /// The points placed so far, plus the pen.
    fn nodes(&self) -> Vec<Point> {
        let mut nodes = self.kept.clone();
        if let Some(tip) = self.run.last()
            && nodes.last().is_none_or(|last| {
                (tip.x - last.x).hypot(tip.y - last.y) >= f32::EPSILON
            })
        {
            nodes.push(*tip);
        }
        nodes
    }

    /// The pen's last position becomes the final point.
    fn finish(self) -> Vec<Point> {
        self.nodes()
    }
}

/// Index of the point furthest from the chord `from`..`to`, and how far.
fn furthest_from_chord(points: &[Point], from: Point, to: Point) -> Option<(usize, f32)> {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let length = dx.hypot(dy);
    let mut best: Option<(usize, f32)> = None;
    for (index, point) in points.iter().enumerate() {
        let deviation = if length <= f32::EPSILON {
            (point.x - from.x).hypot(point.y - from.y)
        } else {
            // Perpendicular distance to the line through from..to.
            ((point.x - from.x) * dy - (point.y - from.y) * dx).abs() / length
        };
        if best.is_none_or(|(_, b)| deviation > b) {
            best = Some((index, deviation));
        }
    }
    best
}

/// What a selection lets through, as a per-pixel multiplier over the whole
/// canvas. `None` means nothing is holding the pattern back. Alpha lock is not
/// folded in: the GPU blend imposes that on the preview and on the applied
/// pixels alike, so it needs no readback.
///
/// A GPU readback, too expensive per motion event, so this is read once when a
/// curve begins. Nothing can change it in between: picking a selection leaves
/// the tool, which bakes.
type PreviewClip = Option<Vec<u8>>;

struct Live {
    /// Draggable points, in canvas pixels. Never assigned directly - go through
    /// [`Live::set_nodes`], which keeps the sampled curve in step.
    nodes: Vec<SpineNode>,
    /// The smooth curve the nodes describe, and where each node lands along it.
    /// Cached because hit testing and the overlay both read it, and building it
    /// means framing the whole spine - twice per motion event, on the UI thread.
    curve: Vec<Point>,
    anchors: Vec<usize>,
    /// Which side of the curve the body is on, for the left side. Negated at
    /// draw time when `Flip side` is set.
    marks: Vec<SideMark>,
    /// The generated pattern in ribbon space, kept between edits so a node drag
    /// re-maps it onto the new curve instead of re-running the generator.
    /// Cleared whenever a setting changes, which the geometry cannot answer for.
    geometry: Option<oxiedraw_patterns::PatternGeometry>,
    dragging: Option<usize>,
    /// Where the drag started, so a handle moves with the pointer rather than
    /// snapping its centre to it.
    grab_offset: Point,
    preview: Option<CoverageTile>,
    /// Present only while a fresh line is being drawn.
    builder: Option<CurveBuilder>,
    /// What a selection lets through, read once when this curve began.
    clip: PreviewClip,
}

impl Live {
    fn new(nodes: Vec<SpineNode>, builder: Option<CurveBuilder>, clip: PreviewClip) -> Self {
        let mut live = Self {
            nodes: Vec::new(),
            curve: Vec::new(),
            anchors: Vec::new(),
            marks: Vec::new(),
            geometry: None,
            dragging: None,
            grab_offset: Point::new(0.0, 0.0),
            preview: None,
            builder,
            clip,
        };
        live.set_nodes(nodes);
        live
    }

    /// Move the points and re-sample the curve they describe. Everything that
    /// touches a node goes through here: the sampled curve is what the overlay
    /// draws and what hit testing aims at, so a stale one lies about both.
    fn set_nodes(&mut self, nodes: Vec<SpineNode>) {
        self.nodes = nodes;
        let (curve, anchors, marks) = sample_curve(&self.nodes).unwrap_or_default();
        self.curve = curve;
        self.anchors = anchors;
        self.marks = marks;
    }
}

/// One step of in-tool undo: the whole curve as it stood, and what the edit
/// that left it was called. Whole states rather than deltas - a curve is a few
/// dozen points, and a state stack cannot drift out of step with it. An empty
/// node list means "there was no curve".
#[derive(Clone)]
struct CurveStep {
    label: &'static str,
    nodes: Vec<SpineNode>,
}

/// Undo/redo over whole curve states. Kept out of the controller, which needs a
/// live canvas to exist, so it can be tested on its own: stepping back has to
/// hand the current state to the redo side or one undo eats two edits.
#[derive(Clone, Default)]
struct CurveHistory {
    undo: Vec<CurveStep>,
    redo: Vec<CurveStep>,
}

impl CurveHistory {
    /// Record the state an edit moved away from. Any redo is now stale.
    fn push(&mut self, step: CurveStep) {
        self.undo.push(step);
        self.redo.clear();
    }

    fn undo(&mut self, current: Vec<SpineNode>) -> Option<CurveStep> {
        let step = self.undo.pop()?;
        self.redo.push(CurveStep {
            label: step.label,
            nodes: current,
        });
        Some(step)
    }

    fn redo(&mut self, current: Vec<SpineNode>) -> Option<CurveStep> {
        let step = self.redo.pop()?;
        self.undo.push(CurveStep {
            label: step.label,
            nodes: current,
        });
        Some(step)
    }

    fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// A curve set aside: its points and the edits that made them. Crossing the
/// bake boundary in either direction puts you back where you were, curve on
/// screen and editable, rather than leaving nothing to work on.
#[derive(Clone)]
struct Applied {
    nodes: Vec<SpineNode>,
    history: CurveHistory,
    /// The seed this curve's pattern was grown from. Carried because the tool
    /// rolls a fresh one after every bake, so a curve handed back would
    /// otherwise come back as a different pattern along the same line.
    seed: u64,
}

/// One undone bake: the curve whose pixels came off the layer, and whatever was
/// on screen in its place. Both sides are kept here so redoing needs nothing
/// from the caller - reading the baked side back off the overlay only holds
/// until something clears it, and then `baked` runs one short of the document.
struct Crossing {
    baked: Applied,
    displaced: Option<Applied>,
}

/// The two sides of the bake boundary: the curve, which is the tool's, and the
/// pixels, which are the document's. Undo hands back the curve whose pixels just
/// came off the layer; redo hands back the one that had replaced it on screen.
///
/// The invariant everything rests on: `baked` holds exactly one entry per
/// `Pattern` action on the document's undo stack, in the same order, and
/// `crossings` likewise for its redo stack. Every entry moved between those two
/// document stacks moves one entry between these two.
#[derive(Default)]
struct Boundary {
    /// Curves whose pixels are in the layer, oldest first.
    baked: Vec<Applied>,
    /// One per `Pattern` action now on the document's redo stack.
    crossings: Vec<Crossing>,
}

impl Boundary {
    /// A curve went into the layer. Recording the bake dropped the document's
    /// redo branch, so the curves waiting on the far side go with it.
    fn bake(&mut self, curve: Applied) {
        self.baked.push(curve);
        self.crossings.clear();
    }

    fn has_baked(&self) -> bool {
        !self.baked.is_empty()
    }

    /// The document took a bake off the layer. `live` is whatever was on screen
    /// instead; the curve that made those pixels comes back. Check
    /// [`Self::has_baked`] first, or `live` has nowhere to go.
    fn undo(&mut self, live: Option<Applied>) -> Option<Applied> {
        let curve = self.baked.pop()?;
        self.crossings.push(Crossing {
            baked: curve.clone(),
            displaced: live,
        });
        Some(curve)
    }

    /// The curve that made those pixels goes back to the baked side; whatever
    /// it had replaced comes back on screen.
    fn redo(&mut self) -> Option<Applied> {
        let crossing = self.crossings.pop()?;
        self.baked.push(crossing.baked);
        crossing.displaced
    }

    fn has_redo(&self) -> bool {
        !self.crossings.is_empty()
    }

    fn drop_redo(&mut self) {
        self.crossings.clear();
    }
}

#[derive(Clone)]
pub(crate) struct PatternEdit {
    live: Rc<RefCell<Option<Live>>>,
    boundary: Rc<RefCell<Boundary>>,
    /// Its own history, since nothing about the curve is in the document until
    /// it is applied.
    curve_history: Rc<RefCell<CurveHistory>>,
    /// The curve as it stood when the current gesture started, so pointer-up
    /// can record what to step back to.
    gesture_start: Rc<RefCell<Option<Vec<SpineNode>>>>,
    gesture_label: Rc<Cell<&'static str>>,
    /// True between pointer-down and pointer-up of a fresh freehand draw.
    drawing: Rc<Cell<bool>>,
    /// Set when the draw in progress had to bake the previous curve to clear
    /// the way. The two are one gesture and are undone as one.
    after_bake: Rc<Cell<bool>>,
    canvas: Rc<RefCell<Canvas>>,
    pattern: PatternState,
    colors: ColorState,
    /// The drawing guide, read on every regrow: an assisted symmetry guide
    /// reproduces the pattern the way it reproduces a brush stroke.
    guide: GuideState,
    history: Rc<RefCell<HistoryStack>>,
    paintable: CanvasPaintable,
    redraw: RedrawHandle,
    canvas_size: Rc<Cell<Size>>,
    zoom: Rc<Cell<f32>>,
}

impl PatternEdit {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        canvas: &Rc<RefCell<Canvas>>,
        pattern: &PatternState,
        colors: &ColorState,
        guide: &GuideState,
        history: &Rc<RefCell<HistoryStack>>,
        paintable: &CanvasPaintable,
        redraw: &RedrawHandle,
        canvas_size: &Rc<Cell<Size>>,
        zoom: &Rc<Cell<f32>>,
    ) -> Self {
        let edit = Self::build(
            canvas,
            pattern,
            colors,
            guide,
            history,
            paintable,
            redraw,
            canvas_size,
            zoom,
        );
        // Without this the overlay keeps the color it was tinted with and the
        // bake lays down the new one.
        colors.connect_changed(Box::new({
            let edit = edit.clone();
            move || edit.recolor()
        }));
        // The copies follow the guide, so moving or reshaping it has to re-grow
        // them - the same live sync an assisted brush stroke gets.
        guide.connect_changed(Box::new({
            let edit = edit.clone();
            move || edit.guide_changed()
        }));
        edit
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        canvas: &Rc<RefCell<Canvas>>,
        pattern: &PatternState,
        colors: &ColorState,
        guide: &GuideState,
        history: &Rc<RefCell<HistoryStack>>,
        paintable: &CanvasPaintable,
        redraw: &RedrawHandle,
        canvas_size: &Rc<Cell<Size>>,
        zoom: &Rc<Cell<f32>>,
    ) -> Self {
        Self {
            live: Rc::new(RefCell::new(None)),
            boundary: Rc::new(RefCell::new(Boundary::default())),
            curve_history: Rc::new(RefCell::new(CurveHistory::default())),
            gesture_start: Rc::new(RefCell::new(None)),
            gesture_label: Rc::new(Cell::new("Draw line")),
            drawing: Rc::new(Cell::new(false)),
            after_bake: Rc::new(Cell::new(false)),
            canvas: Rc::clone(canvas),
            pattern: pattern.clone(),
            colors: colors.clone(),
            guide: guide.clone(),
            history: Rc::clone(history),
            paintable: paintable.clone(),
            redraw: redraw.clone(),
            canvas_size: Rc::clone(canvas_size),
            zoom: Rc::clone(zoom),
        }
    }

    /// Grabs a handle if one is under the pointer, otherwise starts a new curve,
    /// which replaces whatever was there after committing it.
    pub(crate) fn pointer_press(&self, at: Point) {
        // The state this gesture steps back to, whatever it turns out to be.
        *self.gesture_start.borrow_mut() = Some(self.nodes_now());

        match self.hit_test(at) {
            Hover::Node(index) => {
                self.gesture_label.set("Move point");
                self.grab(index, at);
                return;
            }
            // Grabbed as well as added, so one gesture creates and places it.
            Hover::Segment { before, at: on_curve } => {
                if let Some(index) = self.insert_node(before, on_curve) {
                    self.gesture_label.set("Add point");
                    self.grab(index, on_curve);
                    self.regenerate();
                    return;
                }
            }
            Hover::Nothing => {}
        }
        self.gesture_label.set("Draw line");
        // Baking the previous line is part of this gesture, so the two undo
        // together and this draw records no step of its own - see
        // `pointer_release`.
        self.after_bake.set(self.commit());
        // Committing empties the curve's undo stack, so re-capture after it.
        *self.gesture_start.borrow_mut() = Some(self.nodes_now());
        let clip = self.read_clip();
        *self.live.borrow_mut() = Some(Live::new(
            vec![SpineNode::new(at, 1.0)],
            Some(CurveBuilder::new(at)),
            clip,
        ));
        self.drawing.set(true);
    }

    /// Keeps the node's offset from the pointer, so it moves with the hand
    /// rather than snapping its centre to it.
    fn grab(&self, index: usize, at: Point) {
        if let Some(live) = self.live.borrow_mut().as_mut()
            && let Some(node) = live.nodes.get(index)
        {
            let offset = Point::new(node.pos.x - at.x, node.pos.y - at.y);
            live.dragging = Some(index);
            live.grab_offset = offset;
        }
        self.drawing.set(false);
    }

    pub(crate) fn pointer_motion(&self, at: Point) {
        let moved = {
            let mut live = self.live.borrow_mut();
            let Some(live) = live.as_mut() else {
                return;
            };
            if let Some(index) = live.dragging {
                let target = Point::new(at.x + live.grab_offset.x, at.y + live.grab_offset.y);
                let mut nodes = std::mem::take(&mut live.nodes);
                nodes[index].pos = target;
                live.set_nodes(nodes);
                true
            } else if self.drawing.get() {
                let Some(builder) = live.builder.as_mut() else {
                    return;
                };
                // The fit does not improve for near-duplicate samples.
                let far_enough = builder.run.last().is_none_or(|last| {
                    let (dx, dy) = (at.x - last.x, at.y - last.y);
                    dx.mul_add(dx, dy * dy) >= MIN_STEP * MIN_STEP
                });
                if far_enough {
                    builder.push(at);
                    let nodes = builder.nodes();
                    live.set_nodes(nodes.into_iter().map(|p| SpineNode::new(p, 1.0)).collect());
                }
                far_enough
            } else {
                false
            }
        };
        if !moved {
            return;
        }
        // While the pen is still down a freehand line is only drawn, never
        // grown along: generating mid-drag is what starves stylus input.
        if self.drawing.get() {
            self.refresh_overlay();
        } else {
            self.regenerate();
        }
    }

    pub(crate) fn pointer_release(&self) {
        let was_drawing = self.drawing.replace(false);
        let after_bake = self.after_bake.replace(false);
        let before = self.gesture_start.borrow_mut().take();
        let enough = {
            let mut live = self.live.borrow_mut();
            let Some(live) = live.as_mut() else {
                return;
            };
            live.dragging = None;
            if was_drawing && let Some(builder) = live.builder.take() {
                let nodes = builder.finish();
                live.set_nodes(nodes.into_iter().map(|p| SpineNode::new(p, 1.0)).collect());
            }
            live.nodes.len() >= 2
        };
        if !enough {
            // A click with no drag leaves nothing to grow along.
            *self.live.borrow_mut() = None;
            self.refresh_overlay();
            return;
        }
        // A draw that had to bake the previous curve records nothing of its
        // own. What it stepped away from is the curve that was on screen, not
        // the empty one this started from, and getting back there means taking
        // the bake off the layer - which the document's history owns.
        if let Some(before) = before
            && before != self.nodes_now()
            && !after_bake
        {
            self.push_undo(CurveStep {
                label: self.gesture_label.get(),
                nodes: before,
            });
        }
        self.regenerate();
    }

    // -----------------------------------------------------------------
    // In-tool undo / redo
    // -----------------------------------------------------------------

    /// The label of what was undone, or `None` when there is nothing of the
    /// tool's own left and the caller should fall through to the document.
    pub(crate) fn undo(&self) -> Option<&'static str> {
        let current = self.nodes_now();
        let step = self.curve_history.borrow_mut().undo(current)?;
        self.apply_nodes(step.nodes);
        Some(step.label)
    }

    pub(crate) fn redo(&self) -> Option<&'static str> {
        let current = self.nodes_now();
        let step = self.curve_history.borrow_mut().redo(current)?;
        self.apply_nodes(step.nodes);
        Some(step.label)
    }

    fn push_undo(&self, step: CurveStep) {
        self.drop_redo_branch();
        self.curve_history.borrow_mut().push(step);
    }

    /// Give up the redo branch on the far side of a bake boundary. Reshaping a
    /// curve an undo handed back records nothing in the document, so nothing
    /// else clears a redo that would put back pixels no longer matching it.
    fn drop_redo_branch(&self) {
        if !self.boundary.borrow().has_redo() {
            return;
        }
        self.boundary.borrow_mut().drop_redo();
        self.history.borrow_mut().drop_redo();
    }

    fn take_live(&self) -> Option<Applied> {
        let live = self.live.borrow_mut().take()?;
        Some(Applied {
            nodes: live.nodes,
            history: std::mem::take(&mut self.curve_history.borrow_mut()),
            seed: self.pattern.seed.get(),
        })
    }

    fn restore(&self, curve: Applied) {
        self.pattern.seed.set(curve.seed);
        *self.curve_history.borrow_mut() = curve.history;
        self.apply_nodes(curve.nodes);
    }

    /// An empty list means no curve, which is a state worth stepping back to.
    fn nodes_now(&self) -> Vec<SpineNode> {
        self.live
            .borrow()
            .as_ref()
            .map(|live| live.nodes.clone())
            .unwrap_or_default()
    }

    fn apply_nodes(&self, nodes: Vec<SpineNode>) {
        if nodes.len() < 2 {
            *self.live.borrow_mut() = None;
            self.refresh_overlay();
            return;
        }
        // Read the clip before taking the borrow: it goes to the GPU.
        let fresh_clip = self.live.borrow().is_none().then(|| self.read_clip());
        let mut live = self.live.borrow_mut();
        if let Some(live) = live.as_mut() {
            live.set_nodes(nodes);
            live.dragging = None;
        } else {
            *live = Some(Live::new(nodes, None, fresh_clip.flatten()));
        }
        drop(live);
        self.regenerate();
    }

    /// Only the tint changes, so the pattern does not need re-growing.
    fn recolor(&self) {
        if self.live.borrow().is_some() {
            self.refresh_overlay();
        }
    }

    /// Read the selection mask a bake would be clipped by. One GPU readback, so
    /// it runs once per curve - see [`PreviewClip`].
    fn read_clip(&self) -> PreviewClip {
        let mut canvas = self.canvas.borrow_mut();
        if !canvas.selection_active() {
            return None;
        }
        canvas.read_selection_mask().ok()
    }

    /// A knob in the tool's panel moved: re-grow the curve under the new
    /// settings. Separate from [`Self::regenerate`] because it is an edit and
    /// gives up the redo branch, while `regenerate` also draws a curve an undo
    /// has just handed back and must leave that branch alone.
    pub(crate) fn settings_changed(&self) {
        self.drop_redo_branch();
        // The geometry answers for the curve, not for the knobs, so a settings
        // change is the one edit it cannot be re-mapped through.
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.geometry = None;
        }
        self.regenerate();
    }

    /// The drawing guide moved, changed mode, or had assist toggled. Only the
    /// copies move, so the geometry stands and this just re-maps it. Like a
    /// settings change it alters what a bake would lay down, so the redo branch
    /// on the far side of one goes with it.
    fn guide_changed(&self) {
        if self.live.borrow().is_none() {
            return;
        }
        self.drop_redo_branch();
        self.regenerate();
    }

    /// The transforms an assisted symmetry guide reproduces the pattern across,
    /// or `None` when no guide is up, assist is off, or it is a guide that snaps
    /// rather than reproduces.
    fn symmetry(&self) -> Option<Symmetry> {
        self.guide.config.borrow().as_ref().and_then(Symmetry::from_config)
    }

    /// Re-grow the pattern along the current curve and repaint.
    ///
    /// Generated once per curve; a later change of shape re-maps that geometry
    /// onto the new line instead of re-running scatter, sweeps and a scanline
    /// fill once per motion event. Dragging then moves the fur it already has
    /// rather than re-rolling it under the hand.
    pub(crate) fn regenerate(&self) {
        let size = self.canvas_size.get();
        let params = self.pattern.params();
        let bindings = Bindings::default();
        let style = self.pattern.style.borrow().to_stroke_style();
        let seed = self.pattern.seed.get();
        let symmetry = self.symmetry();

        {
            let mut slot = self.live.borrow_mut();
            let Some(live) = slot.as_mut() else { return };
            if live.nodes.len() < 2 {
                live.preview = None;
                live.geometry = None;
            } else {
                let remapped = live
                    .geometry
                    .as_mut()
                    .is_some_and(|geometry| geometry.remap(&live.nodes));
                if !remapped {
                    live.geometry = oxiedraw_patterns::generate(&PatternRequest {
                        pattern: self.pattern.pattern(),
                        params: &params,
                        bindings: &bindings,
                        nodes: &live.nodes,
                        seed,
                        side: self.pattern.side_for(&params),
                        style,
                        rest_length: None,
                    });
                }
                live.preview = live.geometry.as_ref().and_then(|geometry| {
                    let elements = geometry.to_canvas();
                    // One raster over the copies as well, so a copy that lands
                    // on the original reads as one silhouette rather than two.
                    let elements = match &symmetry {
                        Some(symmetry) => symmetry_copies(&elements, symmetry),
                        None => elements,
                    };
                    oxiedraw_patterns::rasterize(
                        &elements,
                        &RasterOptions {
                            canvas: Some((size.width, size.height)),
                            edge: 1.0,
                        },
                    )
                });
            }
        }
        self.refresh_overlay();
    }

    fn refresh_overlay(&self) {
        // Built while the curve is borrowed, then handed on once it is not:
        // pushing the pattern reaches the canvas, and the curve stays out of
        // that call's way.
        let drawn = {
            let live = self.live.borrow();
            live.as_ref().map(|live| {
                let nodes: Vec<Point> = live.nodes.iter().map(|n| n.pos).collect();
                // The sampled curve, not the polyline through the nodes: the
                // pattern follows the smooth one, and segments sit visibly off
                // it on a bend.
                let curve = if live.curve.is_empty() {
                    nodes.clone()
                } else {
                    live.curve.clone()
                };
                let size = self.canvas_size.get();
                let pixels = live.preview.as_ref().and_then(|tile| {
                    tint(tile, self.colors.current(), live.clip.as_deref(), size)
                });
                // The field is framed for the left side, so growing on the right
                // is the same marks turned round.
                let flipped = self.pattern.side_for(&self.pattern.params()) == Side::Right;
                let marks: Vec<SideMark> = live
                    .marks
                    .iter()
                    .map(|mark| SideMark {
                        at: mark.at,
                        normal: if flipped {
                            Point::new(-mark.normal.x, -mark.normal.y)
                        } else {
                            mark.normal
                        },
                    })
                    .collect();
                (curve, nodes, marks, pixels)
            })
        };
        let Some((curve, nodes, marks, pixels)) = drawn else {
            self.push_pattern(None);
            self.paintable
                .set_pattern_overlay(None, Vec::new(), Vec::new(), Vec::new(), Vec::new());
            self.redraw.request();
            return;
        };
        // Where the copies will land. Worth drawing even though the pattern
        // shows them once it is grown: while the pen is down there is no
        // pattern yet, only the line.
        let mirrors = self
            .symmetry()
            .map_or_else(Vec::new, |symmetry| symmetry.copy_points(&curve));
        self.push_pattern(pixels.as_ref());
        self.paintable
            .set_pattern_overlay(Some(()), curve, mirrors, nodes, marks);
        self.redraw.request();
    }

    /// Hand the tinted pattern to the canvas, which composites it at the active
    /// layer's z-order, or take it off screen when there is nothing to show.
    /// Re-armed on every refresh so it follows the active layer and its alpha
    /// lock rather than pinning whichever was current when the curve began.
    fn push_pattern(&self, pixels: Option<&PatternPixels>) {
        let mut canvas = self.canvas.borrow_mut();
        let target = pixels.zip(canvas.layers().active());
        let Some((pixels, idx)) = target else {
            if canvas.pattern_overlay_active() {
                canvas.cancel_pattern_overlay();
            }
            return;
        };
        if let Err(e) = canvas.set_pattern_overlay(
            idx,
            pixels.x,
            pixels.y,
            pixels.width,
            pixels.height,
            &pixels.bgra,
        ) {
            tracing::error!(error = %e, "pattern: overlay upload failed");
            canvas.cancel_pattern_overlay();
        }
    }

    /// Bake the pattern into the active layer and drop the curve, returning
    /// whether it baked one. What the bar's Apply button and Enter do; also
    /// called on leaving the tool, on starting a new line, and before anything
    /// else touches the layer stack. A no-op with no curve, and the tool is left
    /// ready for the next line either way.
    ///
    /// The curve stays on screen until the bake has definitely happened, since
    /// every step below can fail and a curve dropped on a failure is gone with
    /// nothing on the layer to show for it.
    pub(crate) fn commit(&self) -> bool {
        let has_preview = {
            let live = self.live.borrow();
            let Some(live) = live.as_ref() else {
                self.curve_history.borrow_mut().clear();
                return false;
            };
            live.preview.is_some()
        };
        if !has_preview {
            return false;
        }
        // Re-arm against whatever layer is active now. Picking a different layer
        // in the panel does not run through this tool, so the overlay could
        // still be aimed at the one the curve was drawn over - and the bake
        // below goes to the active one.
        self.refresh_overlay();

        let target = {
            let mut canvas = self.canvas.borrow_mut();
            // The overlay holds the very pixels the preview showed. Without one
            // armed there is nothing to apply, and applying anyway would put the
            // previous curve's pattern down a second time.
            if !canvas.pattern_overlay_active() {
                return false;
            }
            let Some(idx) = canvas.layers().active() else {
                return false;
            };
            let layer_id = canvas
                .layers()
                .snapshot()
                .get(idx)
                .map(|l| l.id.clone())
                .unwrap_or_default();
            let size = canvas.size();
            match canvas.read_layer(idx) {
                Ok(before) => Some((idx, layer_id, before, size)),
                Err(e) => {
                    tracing::error!(error = %e, "pattern: read_layer failed");
                    None
                }
            }
        };
        let Some((idx, layer_id, before, size)) = target else {
            return false;
        };

        // The same pass the preview ran, into the layer this time.
        if let Err(e) = self.canvas.borrow_mut().commit_pattern(idx) {
            tracing::error!(error = %e, "pattern: commit failed");
            return false;
        }

        // Read back rather than diffing what was sent: alpha lock may have held
        // some of it out, so the committed result is the only honest "after".
        let patch = match self.canvas.borrow_mut().read_layer(idx) {
            Ok(after) => LayerPatch::from_full_diff(&before, &after, size.width, size.height),
            Err(e) => {
                tracing::error!(error = %e, "pattern: read_layer after commit failed");
                None
            }
        };
        // No patch means the layer came out identical, so nothing may be set
        // aside either: the boundary holds one curve per `Pattern` entry in the
        // document's history, and an extra one hands back the wrong curve.
        let Some(patch) = patch else {
            tracing::info!(
                target: "oxiedraw::tool",
                "pattern: nothing landed on the layer, not recorded"
            );
            return false;
        };
        self.history
            .borrow_mut()
            .record(HistoryAction::Pattern { layer_id, patch });

        let Some(applied) = self.take_live() else {
            return false;
        };
        tracing::info!(
            target: "oxiedraw::tool",
            pattern = %self.pattern.pattern().id(),
            nodes = applied.nodes.len(),
            "pattern applied"
        );
        // Set aside so undoing this bake can put the curve back on screen
        // rather than only taking the pattern off the layer.
        self.boundary.borrow_mut().bake(applied);
        // So two lines with the same settings still differ.
        self.pattern.advance_seed();
        self.refresh_overlay();
        true
    }

    /// Leave the tool: bake what can be baked, then clear the overlay whatever
    /// happened. [`Self::commit`] keeps a curve on screen when a bake fails, but
    /// on the way out there is no tool left to edit it in and it would go on
    /// painting handles over every other tool.
    pub(crate) fn leave(&self) {
        self.commit();
        if let Some(dropped) = self.take_live() {
            tracing::warn!(
                target: "oxiedraw::tool",
                nodes = dropped.nodes.len(),
                "pattern: nothing baked on the way out, curve dropped"
            );
        }
        self.curve_history.borrow_mut().clear();
        self.refresh_overlay();
    }

    /// Throw the curve away without applying it: the pattern comes off the
    /// canvas and the layer is left untouched. What the bar's Cancel button and
    /// Escape do. Returns whether there was a curve to discard.
    pub(crate) fn cancel(&self) -> bool {
        let had_curve = self.take_live().is_some();
        self.curve_history.borrow_mut().clear();
        self.refresh_overlay();
        if had_curve {
            tracing::info!(target: "oxiedraw::tool", "pattern: curve discarded");
        }
        had_curve
    }

    /// Put the most recently baked curve back on screen, editable, with its own
    /// edits behind it again. Returns whether it had one to hand back. Called
    /// when the document's history takes the bake off the layer, which would
    /// otherwise leave the pattern gone and nothing to carry on from.
    pub(crate) fn restore_last_applied(&self) -> bool {
        if !self.boundary.borrow().has_baked() {
            return false;
        }
        // Whatever is on screen took this curve's place, so it goes to the redo
        // side rather than being dropped.
        let displaced = self.take_live();
        let Some(baked) = self.boundary.borrow_mut().undo(displaced) else {
            return false;
        };
        self.restore(baked);
        true
    }

    /// Take the live curve off screen and put back whatever it had replaced,
    /// returning whether that left a curve on screen. Called when the document's
    /// history puts a bake back on the layer: the curve that made those pixels
    /// must not stay live too, or the pattern is on screen twice.
    pub(crate) fn stash_live(&self) -> bool {
        // Dropped, not kept: the boundary already holds a copy of this one.
        drop(self.take_live());
        let displaced = self.boundary.borrow_mut().redo();
        let Some(displaced) = displaced else {
            self.refresh_overlay();
            return false;
        };
        self.restore(displaced);
        true
    }

    /// Tolerances are in screen pixels so they do not shrink as you zoom out,
    /// and nodes win over the curve they sit on.
    pub(crate) fn hit_test(&self, at: Point) -> Hover {
        let live = self.live.borrow();
        let Some(live) = live.as_ref() else {
            return Hover::Nothing;
        };
        let zoom = self.zoom.get().max(0.01);
        let node_reach = HANDLE_HIT_PX / zoom;

        let mut best: Option<(usize, f32)> = None;
        for (index, node) in live.nodes.iter().enumerate() {
            let distance = (node.pos.x - at.x).hypot(node.pos.y - at.y);
            if distance <= node_reach && best.is_none_or(|(_, b)| distance < b) {
                best = Some((index, distance));
            }
        }
        if let Some((index, _)) = best {
            return Hover::Node(index);
        }

        let Some((sample, point, distance)) = nearest_on_polyline(&live.curve, at) else {
            return Hover::Nothing;
        };
        if distance > SEGMENT_HIT_PX / zoom {
            return Hover::Nothing;
        }
        // Which pair of nodes that stretch of curve runs between.
        let before = live
            .anchors
            .iter()
            .position(|anchor| *anchor > sample)
            .unwrap_or_else(|| live.nodes.len().saturating_sub(1))
            .max(1);
        Hover::Segment { before, at: point }
    }

    /// Read by the cursor, which holds still for the whole of a stroke rather
    /// than answering what the pen happens to be over.
    pub(crate) fn is_drawing(&self) -> bool {
        self.drawing.get()
    }

    /// Returns the new index, so the caller can start dragging it straight away.
    fn insert_node(&self, before: usize, at: Point) -> Option<usize> {
        let mut live = self.live.borrow_mut();
        let live = live.as_mut()?;
        let index = before.min(live.nodes.len());
        let mut nodes = std::mem::take(&mut live.nodes);
        nodes.insert(index, SpineNode::new(at, 1.0));
        live.set_nodes(nodes);
        Some(index)
    }

    /// A curve needs two points to grow anything along, so the last two are not
    /// removable - the way out is undo, or drawing a new line.
    pub(crate) fn delete_node(&self, at: Point) {
        let Hover::Node(index) = self.hit_test(at) else {
            return;
        };
        let before = self.nodes_now();
        {
            let mut live = self.live.borrow_mut();
            let Some(live) = live.as_mut() else { return };
            if live.nodes.len() <= 2 {
                return;
            }
            let mut nodes = std::mem::take(&mut live.nodes);
            nodes.remove(index);
            live.set_nodes(nodes);
            live.dragging = None;
        }
        self.push_undo(CurveStep {
            label: "Delete point",
            nodes: before,
        });
        self.regenerate();
    }
}

/// The curve as it is actually drawn and grown along, sampled from the same
/// `SpineField` the generator builds. Returns the sampled points, where each
/// node lands among them, and the side markers (see [`SideMark`]).
fn sample_curve(nodes: &[SpineNode]) -> Option<(Vec<Point>, Vec<usize>, Vec<SideMark>)> {
    let field = oxiedraw_patterns::SpineField::new(nodes, 1.0, None)?;
    let curve: Vec<Point> = field.samples().iter().map(|f| f.pos).collect();
    if curve.len() < 2 {
        return None;
    }
    // The field is built for the left side, so the right side is the same marks
    // negated: flipping the side must not mean re-framing the spine.
    let mut marks = Vec::new();
    let mut next_at = 0.0;
    for sample in field.samples() {
        if sample.s + f32::EPSILON < next_at {
            continue;
        }
        next_at = sample.s + MARK_SPACING;
        marks.push(SideMark {
            at: sample.pos,
            normal: sample.normal,
        });
    }
    // The curve passes through every node, and both are in order, so one
    // forward scan finds each node's nearest sample.
    let mut anchors = Vec::with_capacity(nodes.len());
    let mut from = 0;
    for node in nodes {
        let mut best = (from, f32::MAX);
        for (index, point) in curve.iter().enumerate().skip(from) {
            let distance = (point.x - node.pos.x).hypot(point.y - node.pos.y);
            if distance < best.1 {
                best = (index, distance);
            }
        }
        anchors.push(best.0);
        from = best.0;
    }
    Some((curve, anchors, marks))
}

/// Closest point on a polyline to `at`: the index of the segment's first
/// vertex, the point itself, and how far away it is.
fn nearest_on_polyline(points: &[Point], at: Point) -> Option<(usize, Point, f32)> {
    let mut best: Option<(usize, Point, f32)> = None;
    for (index, pair) in points.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len2 = dx.mul_add(dx, dy * dy);
        let t = if len2 <= f32::EPSILON {
            0.0
        } else {
            (((at.x - a.x) * dx + (at.y - a.y) * dy) / len2).clamp(0.0, 1.0)
        };
        let point = Point::new(dx.mul_add(t, a.x), dy.mul_add(t, a.y));
        let distance = (point.x - at.x).hypot(point.y - at.y);
        if best.is_none_or(|(_, _, b)| distance < b) {
            best = Some((index, point, distance));
        }
    }
    best
}

/// Turn a coverage tile into a premultiplied BGRA block in the active color,
/// clipped to the canvas, ready to upload as the overlay. `clip` is the same
/// per-pixel limit the bake applies (see [`PreviewClip`]), or the preview shows
/// fur that vanishes when it is applied.
///
/// Layer images are premultiplied sRGB BGRA, so these bytes go in the same way
/// a layer's own pixels do and the GPU handles the linear conversion.
fn tint(
    tile: &CoverageTile,
    color: Color,
    clip: Option<&[u8]>,
    canvas: Size,
) -> Option<PatternPixels> {
    let (canvas_w, canvas_h) = (canvas.width as i32, canvas.height as i32);
    let x0 = tile.x.max(0);
    let y0 = tile.y.max(0);
    let x1 = (tile.x + tile.width as i32).min(canvas_w);
    let y1 = (tile.y + tile.height as i32).min(canvas_h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (width, height) = ((x1 - x0) as u32, (y1 - y0) as u32);
    let mut bgra = vec![0_u8; (width as usize) * (height as usize) * 4];
    let (r, g, b) = (
        u32::from(color.r),
        u32::from(color.g),
        u32::from(color.b),
    );
    for y in y0..y1 {
        for x in x0..x1 {
            let mut a = u32::from(tile.coverage_at(x, y));
            if let Some(clip) = clip {
                let at = y as usize * canvas.width as usize + x as usize;
                let allowed = clip.get(at).copied().unwrap_or(0);
                a = (a * u32::from(allowed) + 127) / 255;
            }
            if a == 0 {
                continue;
            }
            let i = ((y - y0) as usize * width as usize + (x - x0) as usize) * 4;
            bgra[i] = ((b * a + 127) / 255) as u8;
            bgra[i + 1] = ((g * a + 127) / 255) as u8;
            bgra[i + 2] = ((r * a + 127) / 255) as u8;
            bgra[i + 3] = a as u8;
        }
    }
    Some(PatternPixels {
        x: x0,
        y: y0,
        width,
        height,
        bgra,
    })
}

/// A premultiplied BGRA block and where it sits in canvas pixels. Always inside
/// the canvas - the upload takes it as a plain sub-rectangle.
struct PatternPixels {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn draw(samples: &[Point]) -> Vec<Point> {
        let mut builder = CurveBuilder::new(samples[0]);
        for at in &samples[1..] {
            builder.push(*at);
        }
        builder.finish()
    }

    #[test]
    fn a_straight_line_costs_two_points() {
        let samples: Vec<Point> = (0..200).map(|i| Point::new(i as f32 * 4.0, 0.0)).collect();
        assert_eq!(draw(&samples).len(), 2);
    }

    #[test]
    fn curvature_is_what_spends_points() {
        let samples: Vec<Point> = (0_u8..=200)
            .map(|i| {
                let a = std::f32::consts::PI * f32::from(i) / 200.0;
                Point::new(200.0 * a.cos(), 200.0 * a.sin())
            })
            .collect();
        let arc = draw(&samples);
        assert!(
            arc.len() > 6,
            "an arc came back with only {} points",
            arc.len()
        );
        // The fit keeps real samples; it does not invent positions.
        for point in &arc {
            let radius = point.x.hypot(point.y);
            assert!((radius - 200.0).abs() < 1.0, "point off the arc: {radius}");
        }
    }

    #[test]
    fn jitter_on_a_straight_line_does_not_spray_points() {
        let samples: Vec<Point> = (0..200)
            .map(|i| {
                let wobble = if i % 2 == 0 { 1.5 } else { -1.5 };
                Point::new(i as f32 * 4.0, wobble)
            })
            .collect();
        let drawn = draw(&samples);
        assert!(
            drawn.len() <= 4,
            "jitter produced {} points on a straight line",
            drawn.len()
        );
    }

    // Otherwise points visibly jump around as you draw.
    #[test]
    fn points_placed_while_drawing_do_not_move_afterwards() {
        let samples: Vec<Point> = (0_u8..=120)
            .map(|i| {
                let a = std::f32::consts::PI * f32::from(i) / 120.0;
                Point::new(150.0 * a.cos(), 150.0 * a.sin())
            })
            .collect();

        let mut builder = CurveBuilder::new(samples[0]);
        let mut seen: Vec<Vec<Point>> = Vec::new();
        for at in &samples[1..] {
            builder.push(*at);
            seen.push(builder.kept.clone());
        }
        let placed = builder.kept.clone();
        for stage in seen {
            assert!(
                stage.len() <= placed.len(),
                "the placed points shrank mid-draw"
            );
            for (early, final_point) in stage.iter().zip(&placed) {
                assert!(
                    (early.x - final_point.x).abs() < 1e-4
                        && (early.y - final_point.y).abs() < 1e-4,
                    "a placed point moved after the fact"
                );
            }
        }
    }

    fn curve(xs: &[f32]) -> Vec<SpineNode> {
        xs.iter()
            .map(|x| SpineNode::new(Point::new(*x, 0.0), 1.0))
            .collect()
    }

    fn xs(nodes: &[SpineNode]) -> Vec<f32> {
        nodes.iter().map(|n| n.pos.x).collect()
    }

    // Undo hands the current state to the redo side as it pops. Get that wrong
    // and one undo eats two edits.
    #[test]
    fn stepping_back_and_forward_walks_one_edit_at_a_time() {
        let mut history = CurveHistory::default();
        // Draw, then move a point twice.
        history.push(CurveStep { label: "Draw line", nodes: curve(&[]) });
        history.push(CurveStep { label: "Move point", nodes: curve(&[0.0, 10.0]) });
        history.push(CurveStep { label: "Move point", nodes: curve(&[0.0, 20.0]) });
        let mut current = curve(&[0.0, 30.0]);

        for expected in [vec![0.0, 20.0], vec![0.0, 10.0], vec![]] {
            let step = history.undo(current).expect("a step back");
            assert_eq!(xs(&step.nodes), expected);
            current = step.nodes;
        }
        assert!(history.undo(current.clone()).is_none(), "walked past the start");

        for expected in [vec![0.0, 10.0], vec![0.0, 20.0], vec![0.0, 30.0]] {
            let step = history.redo(current).expect("a step forward");
            assert_eq!(xs(&step.nodes), expected);
            current = step.nodes;
        }
        assert!(history.redo(current).is_none(), "walked past the end");
    }

    // Leaving the branch redoable would put back a curve that no longer follows
    // from what is on screen.
    #[test]
    fn a_fresh_edit_drops_what_was_stepped_back_from() {
        let mut history = CurveHistory::default();
        history.push(CurveStep { label: "Draw line", nodes: curve(&[]) });
        let stepped_back = history.undo(curve(&[0.0, 10.0])).expect("a step back");
        assert!(stepped_back.nodes.is_empty());

        history.push(CurveStep { label: "Draw line", nodes: curve(&[]) });
        assert!(
            history.redo(curve(&[5.0, 15.0])).is_none(),
            "a stale redo survived a new edit"
        );
    }

    // The spine bends between the nodes, so a polyline through them would sit
    // visibly off the fur on a real curve.
    #[test]
    fn the_drawn_curve_bends_between_the_nodes() {
        // A polyline would pass exactly through the corner; a curve cuts inside.
        let corner = curve(&[0.0, 50.0, 100.0]);
        let mut nodes = corner;
        nodes[0].pos = Point::new(0.0, 0.0);
        nodes[1].pos = Point::new(50.0, 0.0);
        nodes[2].pos = Point::new(50.0, 50.0);

        let (sampled, anchors, _) = sample_curve(&nodes).expect("a curve");
        assert!(
            sampled.len() > nodes.len(),
            "the curve was not densified: {} points",
            sampled.len()
        );
        assert_eq!(anchors.len(), nodes.len());
        // Anchors are in order and land on their nodes.
        for pair in anchors.windows(2) {
            assert!(pair[1] > pair[0], "anchors out of order: {anchors:?}");
        }
        for (node, anchor) in nodes.iter().zip(&anchors) {
            let point = sampled[*anchor];
            assert!(
                (point.x - node.pos.x).hypot(point.y - node.pos.y) < 2.0,
                "node not on its own curve"
            );
        }
    }

    #[test]
    fn side_markers_point_across_the_line_at_intervals() {
        // Derived from the constant, so tuning it does not break this.
        let span = MARK_SPACING * 8.0;
        let mut nodes = curve(&[0.0, span * 0.5, span]);
        nodes[0].pos = Point::new(0.0, 0.0);
        nodes[1].pos = Point::new(span * 0.5, 0.0);
        nodes[2].pos = Point::new(span, 0.0);

        let (_, _, marks) = sample_curve(&nodes).expect("a curve");
        assert!(
            marks.len() >= 8,
            "only {} markers over {span}px at {MARK_SPACING}px spacing",
            marks.len()
        );
        for mark in &marks {
            let length = mark.normal.x.hypot(mark.normal.y);
            assert!((length - 1.0).abs() < 1e-3, "normal not unit: {length}");
            // The line runs along x, so its normal must be all y.
            assert!(mark.normal.x.abs() < 1e-3, "normal not across the line");
            assert!(mark.at.y.abs() < 1e-3, "marker off the line");
        }
        // Spaced out, not piled up.
        for pair in marks.windows(2) {
            let gap = (pair[1].at.x - pair[0].at.x).abs();
            assert!(gap >= MARK_SPACING - 1.0, "markers {gap}px apart");
        }
    }

    #[test]
    fn the_nearest_point_on_a_line_is_found_and_clamped_to_it() {
        let line = vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)];
        let (index, at, distance) =
            nearest_on_polyline(&line, Point::new(4.0, 3.0)).expect("a nearest point");
        assert_eq!(index, 0);
        assert!((at.x - 4.0).abs() < 1e-4 && at.y.abs() < 1e-4);
        assert!((distance - 3.0).abs() < 1e-4);

        // Clamped to the endpoint, not running off along the infinite line.
        let (_, at, _) =
            nearest_on_polyline(&line, Point::new(99.0, 0.0)).expect("a nearest point");
        assert!((at.x - 10.0).abs() < 1e-4);
    }

    fn applied(xs: &[f32], seed: u64) -> Applied {
        Applied {
            nodes: curve(xs),
            history: CurveHistory::default(),
            seed,
        }
    }

    // The scenario the boundary exists for: a second line bakes the first to
    // clear the way, and since the two are one gesture, one undo puts the first
    // back on screen editable. The seed rides along, or the same line comes back
    // wearing a different pattern.
    #[test]
    fn undoing_a_second_line_hands_the_first_one_back() {
        let mut boundary = Boundary::default();
        // Line one is baked as line two starts.
        boundary.bake(applied(&[0.0, 10.0], 11));
        // Undo: line two is on screen, line one comes back.
        let back = boundary
            .undo(Some(applied(&[50.0, 60.0], 22)))
            .expect("line one");
        assert_eq!(xs(&back.nodes), vec![0.0, 10.0]);
        assert_eq!(back.seed, 11, "line one came back with another seed");
        assert!(!boundary.has_baked(), "line one is still counted as baked");
        assert!(boundary.has_redo(), "line two was dropped");

        // Redo: line one's pixels go back, so line two returns to the screen.
        let forward = boundary.redo().expect("line two");
        assert_eq!(xs(&forward.nodes), vec![50.0, 60.0]);
        assert_eq!(forward.seed, 22, "line two came back with another seed");
        assert!(boundary.has_baked(), "line one is not baked again");
        assert!(!boundary.has_redo());
    }

    // Redo gives back the displaced curve, not the baked one. Sharing a stack
    // for both sides returns them in the order taken, which is the wrong way
    // round and puts the same curve on screen twice.
    #[test]
    fn crossing_the_boundary_both_ways_keeps_the_two_sides_apart() {
        let mut boundary = Boundary::default();
        boundary.bake(applied(&[1.0, 2.0], 1));
        boundary.bake(applied(&[3.0, 4.0], 2));

        let second = boundary
            .undo(Some(applied(&[9.0, 9.0], 3)))
            .expect("the second");
        assert_eq!(xs(&second.nodes), vec![3.0, 4.0]);
        let first = boundary.undo(Some(second)).expect("the first");
        assert_eq!(xs(&first.nodes), vec![1.0, 2.0]);
        assert_eq!(first.seed, 1);

        // Forward again, in the order they went in.
        let back_to_second = boundary.redo().expect("the second");
        assert_eq!(xs(&back_to_second.nodes), vec![3.0, 4.0]);
        assert_eq!(back_to_second.seed, 2);
        let back_to_live = boundary.redo().expect("what was live");
        assert_eq!(xs(&back_to_live.nodes), vec![9.0, 9.0]);
        assert_eq!(back_to_live.seed, 3);
        assert!(!boundary.has_redo());
    }

    #[test]
    fn a_bake_with_nothing_on_screen_comes_back_and_goes_away_again() {
        let mut boundary = Boundary::default();
        boundary.bake(applied(&[7.0, 8.0], 5));
        let back = boundary.undo(None).expect("the curve");
        assert_eq!(xs(&back.nodes), vec![7.0, 8.0]);
        assert_eq!(back.seed, 5);
        assert!(boundary.redo().is_none(), "a curve came from nowhere");
        assert!(boundary.has_baked(), "the curve was lost on the way back");
    }

    // The baked side is kept here rather than read off the overlay when the redo
    // comes, so whatever became of the curve on screen in between, the boundary
    // still has one entry per `Pattern` action in the document's history.
    #[test]
    fn redo_restores_the_baked_side_whatever_became_of_the_overlay() {
        let mut boundary = Boundary::default();
        boundary.bake(applied(&[1.0, 2.0], 1));
        let back = boundary.undo(None).expect("the curve");
        // The curve the undo handed over is gone - dropped, never re-supplied.
        drop(back);
        assert!(boundary.redo().is_none(), "there was nothing displaced");
        assert!(boundary.has_baked(), "the baked side lost its entry");
        let again = boundary.undo(None).expect("the curve, a second time");
        assert_eq!(xs(&again.nodes), vec![1.0, 2.0]);
        assert_eq!(again.seed, 1);
    }

    #[test]
    fn a_fresh_bake_drops_the_curves_waiting_on_the_far_side() {
        let mut boundary = Boundary::default();
        boundary.bake(applied(&[1.0, 2.0], 1));
        boundary.undo(Some(applied(&[5.0, 6.0], 2)));
        assert!(boundary.has_redo());

        boundary.bake(applied(&[1.0, 2.0], 3));
        assert!(!boundary.has_redo(), "a stale curve survived a new bake");
        assert!(boundary.redo().is_none());
    }

    // With nothing baked, the curve on screen must stay there rather than being
    // swallowed by a boundary that had no room for it.
    #[test]
    fn an_empty_boundary_hands_nothing_back() {
        let mut boundary = Boundary::default();
        assert!(!boundary.has_baked());
        assert!(boundary.undo(None).is_none());
        assert!(boundary.redo().is_none());
    }

    #[test]
    fn undoing_the_first_draw_leaves_no_curve() {
        let mut history = CurveHistory::default();
        history.push(CurveStep { label: "Draw line", nodes: Vec::new() });
        let step = history.undo(curve(&[0.0, 10.0, 20.0])).expect("a step back");
        assert!(step.nodes.is_empty());
    }

    /// A tile at `(x, y)` with uniform coverage.
    fn tile(x: i32, y: i32, width: u32, height: u32, coverage: u8) -> CoverageTile {
        CoverageTile {
            x,
            y,
            width,
            height,
            data: vec![coverage; (width * height) as usize],
        }
    }

    // The upload takes the block as a plain sub-rectangle, so a tile hanging off
    // the edge has to come back trimmed, not with negative coordinates.
    #[test]
    fn a_tile_off_the_edge_is_trimmed_to_the_canvas() {
        let canvas = Size::new(20, 20);
        let out = tint(&tile(-4, -6, 10, 10, 255), Color::new(255, 0, 0), None, canvas)
            .expect("a block");
        assert_eq!((out.x, out.y), (0, 0));
        assert_eq!((out.width, out.height), (6, 4));
        assert_eq!(out.bgra.len(), 6 * 4 * 4);

        // Entirely off the canvas: nothing to upload at all.
        assert!(tint(&tile(40, 40, 4, 4, 255), Color::new(255, 0, 0), None, canvas).is_none());
    }

    #[test]
    fn coverage_becomes_premultiplied_paint_in_the_active_color() {
        let canvas = Size::new(8, 8);
        let out = tint(&tile(0, 0, 4, 4, 128), Color::new(255, 0, 0), None, canvas)
            .expect("a block");
        // Premultiplied BGRA: red at half coverage, blue and green empty.
        assert_eq!(&out.bgra[0..4], &[0, 0, 128, 128]);
    }

    // The selection is applied here rather than on the GPU, so the preview and
    // the applied pixels are clipped by the same buffer.
    #[test]
    fn a_selection_holds_the_pattern_back() {
        let canvas = Size::new(8, 8);
        // Let only the left half through.
        let mut mask = vec![0_u8; 64];
        for y in 0..8 {
            for x in 0..4 {
                mask[y * 8 + x] = 255;
            }
        }
        let out = tint(
            &tile(0, 0, 8, 8, 255),
            Color::new(255, 0, 0),
            Some(&mask),
            canvas,
        )
        .expect("a block");
        let alpha_at = |x: usize, y: usize| out.bgra[(y * 8 + x) * 4 + 3];
        assert_eq!(alpha_at(1, 3), 255, "the mask ate paint it should have let by");
        assert_eq!(alpha_at(6, 3), 0, "the mask did not hold the pattern back");
    }
}
