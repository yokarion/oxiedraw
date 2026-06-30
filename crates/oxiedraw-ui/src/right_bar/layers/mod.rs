//! Layers panel. Holds the layer/group tree, the cairo-drawn row list, and
//! input gestures (click, drag-reorder, double-click rename). The tree is the
//! authoritative source of display order; canvas z-order is synced from it.

mod actions;
mod thumbnail;

use std::cell::RefCell;
use std::collections::HashSet;
use std::f64::consts::TAU;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AOrdering};

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::document::{BlendMode, LayerGroup, LayerKind, LayerState, LayerTreeNode};
use oxiedraw_core::history::{HistoryAction, HistoryStack};
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::gdk;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::canvas::RedrawHandle;
use crate::clipboard::LayerClipboard;
use crate::settings::AppSettings;
use crate::settings::keybinds::accel_parts_for;
use crate::toaster::Toaster;

use self::thumbnail::start_thumbnail_refresh;

// --- Layout constants ---
const PANEL_MARGIN: i32 = 8;
const TAB_SPACING: i32 = 6;

const LIST_PADDING: f64 = 8.0;
const ITEM_HEIGHT: f64 = 44.0;
const ITEM_GAP: f64 = 4.0;
const ITEM_RADIUS: f64 = 6.0;
const ITEM_INNER_PAD: f64 = 8.0;
pub(super) const SWATCH_SIZE: f64 = 28.0;
const SWATCH_RADIUS: f64 = 3.0;
const HANDLE_WIDTH: f64 = 18.0;
const HANDLE_LINE_THICKNESS: f64 = 1.5;
const HANDLE_LINE_GAP: f64 = 4.0;

const EYE_RADIUS: f64 = 9.0;   // half the hit-box width/height
const EDIT_RADIUS: f64 = 8.0;  // adjustment "edit settings" sliders, sits left of the eye
const MASK_RADIUS: f64 = 8.0;  // adjustment "show mask" toggle, sits left of the sliders
const EDIT_EYE_GAP: f64 = 6.0; // gap between adjacent row icons
const CHEVRON_SIZE: f64 = 12.0;
const FOLDER_W: f64 = 16.0;
const FOLDER_H: f64 = 13.0;
const INDENT_STEP: f64 = 10.0;

const SLOT_HEIGHT: f64 = ITEM_HEIGHT + ITEM_GAP;

// --- Auto-scroll while drag-reordering ---
// Thickness of the hot zone at each viewport edge, and the scroll speed reached
// at the very edge (px/sec). Speed ramps linearly across the zone.
const AUTOSCROLL_EDGE: f64 = 56.0;
const AUTOSCROLL_MAX_SPEED: f64 = 700.0;

// --- Color types and fallbacks ---
type Rgb = (f64, f64, f64);

const FALLBACK_ROW_BG: Rgb = (0.96, 0.96, 0.96);
const FALLBACK_ACCENT_BG: Rgb = (0.21, 0.52, 0.89);
const FALLBACK_ACCENT_FG: Rgb = (1.0, 1.0, 1.0);
const FALLBACK_FG: Rgb = (0.18, 0.18, 0.20);

// --- Tree node types ---
static GROUP_COUNTER: AtomicU64 = AtomicU64::new(1);
fn new_group_id() -> String {
    format!("g{:016x}", GROUP_COUNTER.fetch_add(1, AOrdering::Relaxed))
}

#[derive(Debug, Clone)]
pub(super) struct GroupData {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) expanded: bool,
    pub(super) visible: bool,
    pub(super) children: Vec<LayerNode>,
    // Leaves we hid when the user toggled the group's eye off. Re-shown on toggle-on.
    pub(super) masked_leaves: HashSet<String>,
}

// Tree is stored in display order: index 0 is the topmost row (highest z).
#[derive(Debug, Clone)]
pub(super) enum LayerNode {
    Layer(String),
    Group(GroupData),
}

// --- Flat visible-row view ---
#[derive(Clone, Debug)]
pub(super) enum RowKind {
    Layer {
        id: String,
        name: String,
        visible: bool,
        flat_idx: usize,
    },
    Group {
        id: String,
        name: String,
        visible: bool,
        expanded: bool,
    },
}

#[derive(Clone, Debug)]
pub(super) struct VisibleRow {
    pub(super) kind: RowKind,
    pub(super) depth: usize,
    // Extra visual indent (in depth steps) from adjustment layers above this row
    // in its sibling list. Separate from `depth` so structural/drag logic keys
    // off the true tree depth while the row still draws indented.
    pub(super) adjust_indent: usize,
    // `None` parent means this row sits at the tree root.
    pub(super) parent_id: Option<String>,
    pub(super) idx_in_parent: usize,
}

// --- Click zone ---
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum HitZone {
    Handle,
    Eye,
    Edit,    // adjustment layers only: the "edit settings" sliders left of the eye
    Mask,    // adjustment layers only: the "show mask" toggle left of the sliders
    Chevron, // groups only
    Swatch,  // layers only
    Body,
}

// --- Drag state ---
#[derive(Clone, Debug)]
pub(super) struct Drag {
    from_row: usize,
    current_row: usize,
    pointer_y: f64,
    grab_offset_y: f64,
    zone: HitZone,
    // Smoothed indent offset of the floating preview, in depth units.
    animated_depth_offset: f64,
    // Per-row Y nudge from the row's natural slot, in pixels.
    row_y_anim: Vec<f64>,
    last_frame_time_us: i64,
}

// --- Ui ---
#[derive(Clone)]
pub(super) struct Ui {
    pub(super) state: LayerState,
    pub(super) tree: Rc<RefCell<Vec<LayerNode>>>,
    pub(super) drag: Rc<RefCell<Option<Drag>>>,
    pub(super) thumbnails: Rc<RefCell<Vec<Option<cairo::ImageSurface>>>>,
    pub(super) multi_selected: Rc<RefCell<HashSet<String>>>,
    // Mutually exclusive with `state.active()`: only one of layer/group is primary.
    pub(super) active_group: Rc<RefCell<Option<String>>>,
    // Late-bound callback that reloads the blend-mode/opacity controls from the
    // current selection. Invoked whenever the selection changes.
    pub(super) blend_sync: Rc<RefCell<Option<Rc<dyn Fn()>>>>,
    // Id of the adjustment layer whose mask is toggled into view (mirrors the
    // canvas state so the row's mask button can draw its on/off state).
    pub(super) mask_view: Rc<RefCell<Option<String>>>,
    // Icon currently under the pointer (row index + zone), for the hover
    // highlight. Only the clickable per-row icons (eye / sliders / mask) set it.
    pub(super) hover: Rc<RefCell<Option<(usize, HitZone)>>>,
}

impl Ui {
    fn new(state: LayerState) -> Self {
        let snapshot = state.snapshot();
        // Canvas index 0 is the bottom layer; flip so tree index 0 is the top.
        let tree: Vec<LayerNode> = snapshot
            .iter()
            .rev()
            .map(|l| LayerNode::Layer(l.id.clone()))
            .collect();
        Self {
            state,
            tree: Rc::new(RefCell::new(tree)),
            drag: Rc::new(RefCell::new(None)),
            thumbnails: Rc::new(RefCell::new(Vec::new())),
            multi_selected: Rc::new(RefCell::new(HashSet::new())),
            active_group: Rc::new(RefCell::new(None)),
            blend_sync: Rc::new(RefCell::new(None)),
            mask_view: Rc::new(RefCell::new(None)),
            hover: Rc::new(RefCell::new(None)),
        }
    }

    /// Reload the blend-mode/opacity controls from the current selection.
    fn sync_blend_controls(&self) {
        let cb = self.blend_sync.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    /// Selected layers as flat canvas indices, in stack order.
    fn selected_indices(&self) -> Vec<usize> {
        let snapshot = self.state.snapshot();
        self.selected_layer_ids_in_order()
            .iter()
            .filter_map(|id| snapshot.iter().position(|l| &l.id == id))
            .collect()
    }

    fn active_id(&self) -> Option<String> {
        let snap = self.state.snapshot();
        self.state.active().and_then(|i| snap.get(i).map(|l| l.id.clone()))
    }

    // Leaves the user has selected, in tree order; group selections expand to their leaves.
    pub(super) fn selected_layer_ids_in_order(&self) -> Vec<String> {
        let snapshot = self.state.snapshot();
        let tree = self.tree.borrow();
        let snap_ids: HashSet<&str> = snapshot.iter().map(|l| l.id.as_str()).collect();
        let mut wanted: HashSet<String> = HashSet::new();

        for id in self.multi_selected.borrow().iter() {
            if snap_ids.contains(id.as_str()) {
                wanted.insert(id.clone());
            } else {
                for leaf in group_leaf_ids(&tree, id) {
                    wanted.insert(leaf);
                }
            }
        }
        if let Some(id) = self.active_id() {
            wanted.insert(id);
        }
        if let Some(gid) = self.active_group.borrow().as_ref() {
            for leaf in group_leaf_ids(&tree, gid) {
                wanted.insert(leaf);
            }
        }

        if wanted.is_empty() {
            return Vec::new();
        }
        let rows = compute_visible_rows(&tree, &snapshot);
        let mut ordered = Vec::with_capacity(wanted.len());
        for r in &rows {
            if let RowKind::Layer { id, .. } = &r.kind
                && wanted.contains(id) {
                    ordered.push(id.clone());
                }
        }
        ordered
    }
}

// --- Tree utilities ---
fn compute_visible_rows(
    tree: &[LayerNode],
    snapshot: &[oxiedraw_core::document::Layer],
) -> Vec<VisibleRow> {
    let mut rows = Vec::new();
    collect_rows(tree, snapshot, 0, 0, None, &mut rows);
    rows
}

fn collect_rows(
    nodes: &[LayerNode],
    snapshot: &[oxiedraw_core::document::Layer],
    depth: usize,
    base_extra: usize,
    parent_id: Option<&str>,
    rows: &mut Vec<VisibleRow>,
) {
    // An adjustment layer affects everything below it (the rows after it in this
    // sibling list, which are lower in z-order), so those rows get an extra
    // indent step - as if the adjustment opened a group around them. This is
    // purely visual (`adjust_indent`); `depth` stays the true tree depth so the
    // drag/group logic is unaffected. `base_extra` carries an enclosing group's
    // accumulated adjustment indent down to its children.
    let mut extra = 0usize;
    for (i, node) in nodes.iter().enumerate() {
        let indent_here = base_extra + extra;
        match node {
            LayerNode::Layer(id) => {
                if let Some((flat_idx, layer)) = snapshot
                    .iter()
                    .enumerate()
                    .find(|(_, l)| &l.id == id)
                {
                    rows.push(VisibleRow {
                        kind: RowKind::Layer {
                            id: id.clone(),
                            name: layer.name.clone(),
                            visible: layer.visible,
                            flat_idx,
                        },
                        depth,
                        adjust_indent: indent_here,
                        parent_id: parent_id.map(str::to_string),
                        idx_in_parent: i,
                    });
                    if layer.is_adjustment() {
                        extra += 1;
                    }
                }
            }
            LayerNode::Group(g) => {
                rows.push(VisibleRow {
                    kind: RowKind::Group {
                        id: g.id.clone(),
                        name: g.name.clone(),
                        visible: g.visible,
                        expanded: g.expanded,
                    },
                    depth,
                    adjust_indent: indent_here,
                    parent_id: parent_id.map(str::to_string),
                    idx_in_parent: i,
                });
                if g.expanded {
                    collect_rows(&g.children, snapshot, depth + 1, indent_here, Some(&g.id), rows);
                }
            }
        }
    }
}

// Reconciles the tree with the canvas: drops gone layers, prepends new ones at root.
fn reconcile_tree(tree: &mut Vec<LayerNode>, snapshot: &[oxiedraw_core::document::Layer]) {
    let tree_ids = collect_leaf_ids(tree);
    let snap_ids: HashSet<&str> = snapshot.iter().map(|l| l.id.as_str()).collect();

    prune_absent(tree, &snap_ids);

    for layer in snapshot.iter().rev() {
        if !tree_ids.contains(layer.id.as_str()) {
            tree.insert(0, LayerNode::Layer(layer.id.clone()));
        }
    }
}

fn collect_leaf_ids(nodes: &[LayerNode]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for n in nodes {
        match n {
            LayerNode::Layer(id) => { ids.insert(id.clone()); }
            LayerNode::Group(g) => ids.extend(collect_leaf_ids(&g.children)),
        }
    }
    ids
}

fn prune_absent(nodes: &mut Vec<LayerNode>, present: &HashSet<&str>) {
    nodes.retain_mut(|n| match n {
        LayerNode::Layer(id) => present.contains(id.as_str()),
        LayerNode::Group(g) => {
            prune_absent(&mut g.children, present);
            true
        }
    });
}

fn take_node(nodes: &mut Vec<LayerNode>, id: &str) -> Option<LayerNode> {
    for i in 0..nodes.len() {
        let matches = match &nodes[i] {
            LayerNode::Layer(lid) => lid == id,
            LayerNode::Group(g) => g.id == id,
        };
        if matches {
            return Some(nodes.remove(i));
        }
        if let LayerNode::Group(g) = &mut nodes[i]
            && let Some(found) = take_node(&mut g.children, id) {
                return Some(found);
            }
    }
    None
}

// `idx_in_parent == usize::MAX` means "append".
fn insert_after(
    tree: &mut Vec<LayerNode>,
    node: LayerNode,
    parent_id: Option<&String>,
    idx_in_parent: usize,
) {
    match parent_id {
        None => {
            let pos = (idx_in_parent + 1).min(tree.len());
            tree.insert(pos, node);
        }
        Some(pid) => {
            insert_after_in_group(tree, node, pid, idx_in_parent);
        }
    }
}

fn find_group_mut<'a>(nodes: &'a mut [LayerNode], id: &str) -> Option<&'a mut GroupData> {
    for n in nodes.iter_mut() {
        if let LayerNode::Group(g) = n {
            if g.id == id {
                return Some(g);
            }
            if let Some(r) = find_group_mut(&mut g.children, id) {
                return Some(r);
            }
        }
    }
    None
}

fn insert_after_in_group(
    nodes: &mut [LayerNode],
    node: LayerNode,
    group_id: &str,
    idx_in_parent: usize,
) {
    if let Some(g) = find_group_mut(nodes, group_id) {
        let pos = (idx_in_parent + 1).min(g.children.len());
        g.children.insert(pos, node);
    }
}

fn toggle_group_expanded(nodes: &mut [LayerNode], id: &str) {
    if let Some(g) = find_group_mut(nodes, id) {
        g.expanded = !g.expanded;
    }
}

fn rename_group_in_tree(nodes: &mut [LayerNode], id: &str, new_name: String) {
    if let Some(g) = find_group_mut(nodes, id) {
        g.name = new_name;
    }
}

pub(super) fn leaf_ids_top_first(nodes: &[LayerNode]) -> Vec<String> {
    let mut ids = Vec::new();
    for n in nodes {
        match n {
            LayerNode::Layer(id) => ids.push(id.clone()),
            LayerNode::Group(g) => ids.extend(leaf_ids_top_first(&g.children)),
        }
    }
    ids
}

/// Sort flat `LayerNode::Layer` entries in the tree (including inside groups)
/// so their order matches the canvas snapshot. Canvas index 0 = bottom layer =
/// tree position last; canvas top = tree position 0. Group structure is preserved.
pub(super) fn sync_tree_order_from_canvas(tree: &mut [LayerNode], canvas: &Canvas) {
    let snap = canvas.layers().snapshot();
    // top-first order: canvas top (highest index) first
    let ordered: Vec<&str> = snap.iter().rev().map(|l| l.id.as_str()).collect();
    sort_nodes_by_canvas_order(tree, &ordered);
}

fn sort_nodes_by_canvas_order(nodes: &mut [LayerNode], order: &[&str]) {
    nodes.sort_by_key(|n| {
        let id = match n {
            LayerNode::Layer(id) => id.as_str(),
            LayerNode::Group(g) => first_leaf_id(&g.children).unwrap_or(""),
        };
        order.iter().position(|&o| o == id).unwrap_or(usize::MAX)
    });
    for n in nodes.iter_mut() {
        if let LayerNode::Group(g) = n {
            sort_nodes_by_canvas_order(&mut g.children, order);
        }
    }
}

fn first_leaf_id(nodes: &[LayerNode]) -> Option<&str> {
    for n in nodes {
        match n {
            LayerNode::Layer(id) => return Some(id.as_str()),
            LayerNode::Group(g) => {
                if let Some(id) = first_leaf_id(&g.children) {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// Record the canvas order change `before` -> `after` into history. Emits a
/// bare `LayerReorder` for a single-layer move, or a `Batch` of them for group
/// moves (which shift several layers). No-op when the order is unchanged.
fn record_reorder(
    history: &Rc<RefCell<HistoryStack>>,
    before: &[String],
    after: &[String],
) {
    let steps = reorder_steps(before, after);
    match steps.len() {
        0 => {}
        1 => {
            let (from, to) = steps[0];
            history.borrow_mut().record(HistoryAction::LayerReorder { from, to });
        }
        _ => {
            let actions = steps
                .into_iter()
                .map(|(from, to)| HistoryAction::LayerReorder { from, to })
                .collect();
            history.borrow_mut().record(HistoryAction::Batch {
                label: "Reorder layers".to_string(),
                actions,
            });
        }
    }
}

/// Compute a sequence of `(from, to)` single-layer moves that transform the
/// `before` id order into `after`. Each move is independently invertible, so a
/// `Batch` of the resulting `LayerReorder`s round-trips cleanly through undo.
pub(super) fn reorder_steps(before: &[String], after: &[String]) -> Vec<(usize, usize)> {
    let mut cur = before.to_vec();
    let mut steps = Vec::new();
    for (target_pos, id) in after.iter().enumerate() {
        let Some(cur_pos) = cur.iter().position(|x| x == id) else {
            continue;
        };
        if cur_pos != target_pos {
            steps.push((cur_pos, target_pos));
            let item = cur.remove(cur_pos);
            cur.insert(target_pos, item);
        }
    }
    steps
}

// Drives the canvas's flat layer order from the tree. Tree top = canvas top.
pub(super) fn sync_canvas_order(tree: &[LayerNode], canvas: &mut Canvas) {
    let top_first = leaf_ids_top_first(tree);
    let n = top_first.len();
    if n == 0 {
        return;
    }
    let mut current: Vec<String> =
        canvas.layers().snapshot().iter().map(|l| l.id.clone()).collect();
    for desired_pos in 0..n {
        let target_id = &top_first[n - 1 - desired_pos];
        let Some(cur_pos) = current.iter().position(|id| id == target_id) else { continue };
        if cur_pos != desired_pos && canvas.reorder_layer(cur_pos, desired_pos).is_ok() {
            let item = current.remove(cur_pos);
            current.insert(desired_pos, item);
        }
    }
}

// Convert the panel tree (top-first) into the core folder tree (canvas order,
// bottom-first) and push it to the canvas so adjustments scope to their folder.
fn node_to_core(node: &LayerNode) -> LayerTreeNode {
    match node {
        LayerNode::Layer(id) => LayerTreeNode::layer(id.clone()),
        LayerNode::Group(g) => LayerTreeNode::Group(LayerGroup {
            id: g.id.clone(),
            name: g.name.clone(),
            expanded: g.expanded,
            children: tree_to_core(&g.children),
        }),
    }
}

fn tree_to_core(nodes: &[LayerNode]) -> Vec<LayerTreeNode> {
    nodes.iter().rev().map(node_to_core).collect()
}

fn node_from_core(node: &LayerTreeNode) -> LayerNode {
    match node {
        LayerTreeNode::Layer { id } => LayerNode::Layer(id.clone()),
        LayerTreeNode::Group(g) => LayerNode::Group(GroupData {
            id: g.id.clone(),
            name: g.name.clone(),
            expanded: g.expanded,
            visible: true,
            children: tree_from_core(&g.children),
            masked_leaves: HashSet::new(),
        }),
    }
}

// Rebuild the panel tree (top-first) from the core folder tree (bottom-first),
// used when a saved document is loaded.
pub(super) fn tree_from_core(nodes: &[LayerTreeNode]) -> Vec<LayerNode> {
    nodes.iter().rev().map(node_from_core).collect()
}

// Groups only nest inside other groups, so a group anywhere in the tree implies
// one at the top level; a top-level scan answers "has folders" for either tree.
fn ui_tree_has_groups(nodes: &[LayerNode]) -> bool {
    nodes.iter().any(|n| matches!(n, LayerNode::Group(_)))
}

fn core_tree_has_groups(nodes: &[LayerTreeNode]) -> bool {
    nodes.iter().any(|n| matches!(n, LayerTreeNode::Group(_)))
}

// Push the current panel folder structure to the canvas (recomposites so
// folder-scoped adjustments update). Call after any folder-structure change.
pub(super) fn commit_groups(tree: &[LayerNode], canvas: &mut Canvas) {
    if let Err(e) = canvas.set_layer_tree(tree_to_core(tree)) {
        tracing::error!(error = %e, "failed to push folder tree to canvas");
    }
}

// Like `commit_groups` but for metadata-only changes (rename / expand) that
// don't change the composite: stores the tree for persistence without a
// recomposite.
pub(super) fn commit_groups_quiet(tree: &[LayerNode], canvas: &mut Canvas) {
    canvas.set_layer_tree_quiet(tree_to_core(tree));
}

// Wraps the listed nodes in a new group at the topmost position any of them held.
pub(super) fn group_nodes(
    tree: &mut Vec<LayerNode>,
    ids: &[String],
    group_name: &str,
) {
    if ids.is_empty() {
        return;
    }
    let insertion_info = find_first_insertion_position(tree, ids);

    let mut children = Vec::new();
    for id in ids {
        if let Some(node) = take_node(tree, id) {
            children.push(node);
        }
    }
    if children.is_empty() {
        return;
    }
    let group = LayerNode::Group(GroupData {
        id: new_group_id(),
        name: group_name.to_string(),
        expanded: true,
        visible: true,
        children,
        masked_leaves: HashSet::new(),
    });

    if let Some((parent_id, idx)) = insertion_info {
        match &parent_id {
            None => {
                let pos = idx.min(tree.len());
                tree.insert(pos, group);
            }
            Some(pid) => {
                insert_at_in_group(tree, group, pid, idx);
            }
        }
    } else {
        tree.insert(0, group);
    }
}

fn find_first_insertion_position(
    nodes: &[LayerNode],
    ids: &[String],
) -> Option<(Option<String>, usize)> {
    find_first_pos_inner(nodes, ids, None)
}

fn find_first_pos_inner(
    nodes: &[LayerNode],
    ids: &[String],
    parent_id: Option<&str>,
) -> Option<(Option<String>, usize)> {
    for (i, n) in nodes.iter().enumerate() {
        match n {
            LayerNode::Layer(id) if ids.contains(id) => {
                return Some((parent_id.map(str::to_string), i));
            }
            LayerNode::Group(g) if ids.contains(&g.id) => {
                return Some((parent_id.map(str::to_string), i));
            }
            LayerNode::Group(g) => {
                if let Some(r) = find_first_pos_inner(&g.children, ids, Some(&g.id)) {
                    return Some(r);
                }
            }
            LayerNode::Layer(_) => {}
        }
    }
    None
}

pub(super) fn insert_at_in_group(
    nodes: &mut [LayerNode],
    node: LayerNode,
    group_id: &str,
    idx: usize,
) {
    if let Some(g) = find_group_mut(nodes, group_id) {
        let pos = idx.min(g.children.len());
        g.children.insert(pos, node);
    }
}

pub(super) fn ungroup_node(nodes: &mut Vec<LayerNode>, id: &str) -> bool {
    if let Some(i) = nodes
        .iter()
        .position(|n| matches!(n, LayerNode::Group(g) if g.id == id))
    {
        let group = match nodes.remove(i) {
            LayerNode::Group(g) => g,
            LayerNode::Layer(_) => return false,
        };
        for (k, child) in group.children.into_iter().enumerate() {
            nodes.insert(i + k, child);
        }
        return true;
    }
    for n in nodes.iter_mut() {
        if let LayerNode::Group(g) = n
            && ungroup_node(&mut g.children, id) {
                return true;
            }
    }
    false
}

pub(super) fn find_group_position(
    nodes: &[LayerNode],
    id: &str,
) -> Option<(Option<String>, usize)> {
    find_group_position_inner(nodes, id, None)
}

fn find_group_position_inner(
    nodes: &[LayerNode],
    id: &str,
    parent_id: Option<&str>,
) -> Option<(Option<String>, usize)> {
    for (i, n) in nodes.iter().enumerate() {
        if let LayerNode::Group(g) = n {
            if g.id == id {
                return Some((parent_id.map(str::to_string), i));
            }
            if let Some(r) = find_group_position_inner(&g.children, id, Some(&g.id)) {
                return Some(r);
            }
        }
    }
    None
}

// Used by "duplicate group": each leaf is remapped through `id_map`, each group
// gets a fresh id so the copy and the original are independent.
pub(super) fn mirror_tree(
    nodes: &[LayerNode],
    id_map: &std::collections::HashMap<String, String>,
) -> Vec<LayerNode> {
    nodes
        .iter()
        .map(|n| match n {
            LayerNode::Layer(id) => {
                LayerNode::Layer(id_map.get(id).cloned().unwrap_or_else(|| id.clone()))
            }
            LayerNode::Group(g) => LayerNode::Group(GroupData {
                id: new_group_id(),
                name: g.name.clone(),
                expanded: g.expanded,
                visible: g.visible,
                children: mirror_tree(&g.children, id_map),
                masked_leaves: HashSet::new(),
            }),
        })
        .collect()
}

pub(super) fn find_group<'a>(
    nodes: &'a [LayerNode],
    id: &str,
) -> Option<&'a GroupData> {
    for n in nodes {
        if let LayerNode::Group(g) = n {
            if g.id == id {
                return Some(g);
            }
            if let Some(r) = find_group(&g.children, id) {
                return Some(r);
            }
        }
    }
    None
}

pub(super) fn group_leaf_ids(nodes: &[LayerNode], group_id: &str) -> Vec<String> {
    for n in nodes {
        if let LayerNode::Group(g) = n {
            if g.id == group_id {
                return leaf_ids_top_first(&g.children);
            }
            let inner = group_leaf_ids(&g.children, group_id);
            if !inner.is_empty() {
                return inner;
            }
        }
    }
    Vec::new()
}

// --- Public build entry point ---
/// Returns the panel widget and a callback that rebuilds the layer list from
/// current canvas state (for use after undo/redo).
pub(crate) fn build(
    layers: &LayerState,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    layer_clipboard: &Rc<RefCell<Option<LayerClipboard>>>,
    toaster: &Toaster,
    select_layer_content: &Rc<dyn Fn(usize)>,
    history: &Rc<RefCell<HistoryStack>>,
    components: &Rc<RefCell<oxiedraw_core::components::ComponentLibrary>>,
    on_edit_component: &Rc<dyn Fn(String)>,
    component_exit: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
    prepare_delete: &Rc<dyn Fn() -> bool>,
    prepare_reorder: &Rc<dyn Fn()>,
) -> (
    gtk::Box,
    Rc<dyn Fn()>,
    Rc<dyn Fn() -> Vec<String>>,
    Rc<dyn Fn()>,
    Rc<dyn Fn()>,
    Rc<dyn Fn(Option<String>)>,
    Rc<dyn Fn()>,
) {
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .vexpand(true)
        .hexpand(true)
        .build();
    panel.add_css_class("sidebar");

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(TAB_SPACING)
        .margin_top(PANEL_MARGIN)
        .margin_bottom(PANEL_MARGIN)
        .margin_start(PANEL_MARGIN)
        .margin_end(PANEL_MARGIN)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::None)
        .vexpand(true)
        .hexpand(true)
        .build();

    let (layers_page, refresh_layers, selected_ids, reinstall_actions, layer_begin_rename) =
        build_layers_page(
            layers,
            canvas,
            redraw,
            layer_clipboard,
            toaster,
            select_layer_content,
            history,
            on_edit_component,
            prepare_delete,
            prepare_reorder,
        );
    let (components_page, refresh_components, component_begin_rename) = super::components::build(
        Rc::clone(components),
        Rc::clone(on_edit_component),
        Rc::clone(history),
    );
    stack.add_named(&layers_page, Some("layers"));
    stack.add_named(&components_page, Some("components"));

    // Window-level F2 routes to whichever tab is showing.
    let begin_rename: Rc<dyn Fn()> = {
        let stack = stack.clone();
        Rc::new(move || {
            if stack.visible_child_name().as_deref() == Some("components") {
                component_begin_rename();
            } else {
                layer_begin_rename();
            }
        })
    };

    let (tabs, layers_btn, components_btn) = build_tab_bar(&stack);
    let (edit_banner, edit_banner_label) = build_edit_banner(component_exit);
    content.append(&tabs);
    content.append(&edit_banner);
    content.append(&stack);
    panel.append(&content);

    // Component-edit gating: while a component is open, replace the
    // Layers/Components toggle with an "Editing component: X [Done]" banner in
    // the same spot, force the Layers page, and disable nesting.
    let set_editing: Rc<dyn Fn(Option<String>)> = {
        let tabs = tabs.clone();
        let stack = stack.clone();
        Rc::new(move |editing: Option<String>| {
            if let Some(name) = editing {
                layers_btn.set_active(true);
                stack.set_visible_child_name("layers");
                components_btn.set_sensitive(false);
                tabs.set_visible(false);
                edit_banner_label.set_label(&format!("Editing component: {name}"));
                edit_banner.set_visible(true);
            } else {
                components_btn.set_sensitive(true);
                tabs.set_visible(true);
                edit_banner.set_visible(false);
            }
        })
    };

    (
        panel,
        refresh_layers,
        selected_ids,
        reinstall_actions,
        refresh_components,
        set_editing,
        begin_rename,
    )
}

/// Build the component edit-mode banner (hidden by default), shown in place of
/// the Layers/Components toggle while a component is open. The "Done" button
/// invokes the late-bound exit closure in `component_exit`.
fn build_edit_banner(
    component_exit: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
) -> (gtk::Box, gtk::Label) {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(TAB_SPACING)
        .build();
    bar.set_visible(false);

    let label = gtk::Label::builder()
        .label("Editing component")
        .halign(gtk::Align::Start)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let done = gtk::Button::builder().label("Done").build();
    done.add_css_class("suggested-action");
    {
        let component_exit = Rc::clone(component_exit);
        done.connect_clicked(move |_| {
            let cb = component_exit.borrow().clone();
            if let Some(cb) = cb {
                cb();
            }
        });
    }
    bar.append(&label);
    bar.append(&done);
    (bar, label)
}

fn build_tab_bar(stack: &gtk::Stack) -> (gtk::Box, gtk::ToggleButton, gtk::ToggleButton) {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(TAB_SPACING)
        .homogeneous(true)
        .build();

    let layers_btn = gtk::ToggleButton::builder()
        .label("Layers")
        .active(true)
        .build();
    let components_btn = gtk::ToggleButton::builder()
        .label("Components")
        .group(&layers_btn)
        .build();

    {
        let stack = stack.clone();
        layers_btn.connect_toggled(move |b| {
            if b.is_active() {
                stack.set_visible_child_name("layers");
            }
        });
    }
    {
        let stack = stack.clone();
        components_btn.connect_toggled(move |b| {
            if b.is_active() {
                stack.set_visible_child_name("components");
            }
        });
    }

    bar.append(&layers_btn);
    bar.append(&components_btn);
    (bar, layers_btn, components_btn)
}

fn build_layers_page(
    layers: &LayerState,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    layer_clipboard: &Rc<RefCell<Option<LayerClipboard>>>,
    toaster: &Toaster,
    select_layer_content: &Rc<dyn Fn(usize)>,
    history: &Rc<RefCell<HistoryStack>>,
    on_edit_component: &Rc<dyn Fn(String)>,
    prepare_delete: &Rc<dyn Fn() -> bool>,
    prepare_reorder: &Rc<dyn Fn()>,
) -> (
    gtk::Box,
    Rc<dyn Fn()>,
    Rc<dyn Fn() -> Vec<String>>,
    Rc<dyn Fn()>,
    Rc<dyn Fn()>,
) {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(TAB_SPACING)
        .vexpand(true)
        .hexpand(true)
        .build();

    let ui = Ui::new((*layers).clone());
    // A loaded document carries its folder tree on the canvas; seed the panel
    // from it so saved folders reappear. Reconciled below against the snapshot.
    {
        let core_tree = canvas.borrow().layer_tree().to_vec();
        if !core_tree.is_empty() {
            *ui.tree.borrow_mut() = tree_from_core(&core_tree);
        }
    }

    let area = gtk::DrawingArea::builder().hexpand(true).build();
    sync_height(&area, &ui);
    install_list_draw(&area, &ui);
    install_list_input(
        &area,
        &ui,
        Rc::clone(canvas),
        redraw,
        select_layer_content,
        history,
        on_edit_component,
        prepare_reorder,
    );
    actions::install_context_menu(&area, &ui, layer_clipboard);

    // Layer gio actions (copy/cut/paste/duplicate/...) are app-global by name.
    // With multiple documents each panel would clobber the previous one's, so
    // wrap installation in a closure the active tab re-runs to re-point the
    // shared action names at this document.
    let reinstall_actions: Rc<dyn Fn()> = {
        let area = area.clone();
        let ui = ui.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let layer_clipboard = Rc::clone(layer_clipboard);
        let toaster = toaster.clone();
        let history = Rc::clone(history);
        let prepare_delete = Rc::clone(prepare_delete);
        Rc::new(move || {
            actions::install_layer_actions(
                &area,
                &ui,
                &canvas,
                &redraw,
                &layer_clipboard,
                &toaster,
                &history,
                &prepare_delete,
            );
        })
    };
    reinstall_actions();
    start_thumbnail_refresh(&ui, Rc::clone(canvas), area.clone());

    page.append(&build_layers_header(&ui, &area, canvas, redraw, toaster, history));
    page.append(&build_blend_controls(&ui, canvas, redraw, history));

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .child(&area)
        .build();
    page.append(&scroll);
    page.append(&build_layers_footer());

    // Rebuild tree from canvas state, update height, and redraw. Called after
    // undo/redo so the panel stays in sync with mutations applied to the canvas.
    let refresh = {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        Rc::new(move || {
            let c = canvas.borrow();
            // A freshly loaded document gets its folder tree set on the canvas
            // after this panel was built, so the build-time seed saw an empty
            // tree. Adopt the canvas tree here, before the commit below would
            // push the panel's flat tree back over the saved folders.
            {
                let mut tree = ui.tree.borrow_mut();
                if !ui_tree_has_groups(&tree) && core_tree_has_groups(c.layer_tree()) {
                    *tree = tree_from_core(c.layer_tree());
                }
            }
            let snap = c.layers().snapshot();
            reconcile_tree(&mut ui.tree.borrow_mut(), &snap);
            sync_tree_order_from_canvas(&mut ui.tree.borrow_mut(), &c);
            drop(c);
            commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
            sync_height(&area, &ui);
            ui.sync_blend_controls();
            area.queue_draw();
        }) as Rc<dyn Fn()>
    };

    // Provider used by app-level actions (e.g. filters) that need the set of
    // layers the user currently has selected, including the active one.
    let selected_ids = {
        let ui = ui.clone();
        Rc::new(move || ui.selected_layer_ids_in_order()) as Rc<dyn Fn() -> Vec<String>>
    };

    // Triggered by the window-level F2 shortcut: rename the active layer/group.
    let begin_rename = {
        let area = area.clone();
        let ui = ui.clone();
        let canvas = Rc::clone(canvas);
        let history = Rc::clone(history);
        Rc::new(move || begin_rename_active(&area, &ui, &canvas, &history)) as Rc<dyn Fn()>
    };

    (page, refresh, selected_ids, reinstall_actions, begin_rename)
}

// Returns `"Label (Ctrl+G)"`, or just `label` when the action has no binding.
fn tooltip_with_accel(label: &str, action_id: &str) -> String {
    let settings = AppSettings::load();
    match accel_parts_for(action_id, &settings) {
        Some(parts) if !parts.is_empty() => format!("{label} ({})", parts.join("+")),
        _ => label.to_string(),
    }
}

fn build_layers_header(
    ui: &Ui,
    area: &gtk::DrawingArea,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    toaster: &Toaster,
    history: &Rc<RefCell<HistoryStack>>,
) -> gtk::Box {
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(TAB_SPACING)
        .build();

    let spacer = gtk::Label::builder()
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    header.append(&spacer);

    let group_btn = gtk::Button::builder()
        .icon_name("folder-new-symbolic")
        .tooltip_text(tooltip_with_accel("Group selected layers", "layer-group"))
        .css_classes(["flat", "circular"])
        .action_name("app.layer-group")
        .build();
    header.append(&group_btn);

    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add layer on top")
        .css_classes(["flat", "circular"])
        .build();
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let toaster = toaster.clone();
        let history = Rc::clone(history);
        add_btn.connect_clicked(move |_| {
            let next_n = ui.state.len() + 1;
            let name = format!("Layer {next_n}");
            let add_result = canvas.borrow_mut().add_layer(name.clone());
            match add_result {
                Ok(idx) => {
                    let (new_id, new_pixels) = {
                        let mut c = canvas.borrow_mut();
                        let id = c.layers().snapshot()
                            .get(idx).map(|l| l.id.clone()).unwrap_or_default();
                        let pixels = c.read_layer(idx).unwrap_or_default();
                        (id, pixels)
                    };
                    history.borrow_mut().record(HistoryAction::LayerAdd {
                        idx,
                        id: new_id,
                        name,
                        visible: true,
                        layer_kind: oxiedraw_core::document::LayerKind::Raster,
                        blend: oxiedraw_core::document::BlendMode::Normal,
                        opacity: 1.0,
                        pixels: new_pixels,
                    });
                    sync_height(&area, &ui);
                    commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
                    ui.sync_blend_controls();
                    area.queue_draw();
                    redraw.request();
                }
                Err(e) => {
                    if matches!(e, oxiedraw_core::renderer::RendererError::LayerLimit) {
                        toaster.layer_limit_reached();
                    }
                    tracing::error!(error = %e, "canvas.add_layer failed");
                }
            }
        });
    }
    header.append(&add_btn);

    let add_adjustment_btn = gtk::Button::builder()
        .icon_name("oxiedraw-layer-adjustment-symbolic")
        .tooltip_text("Add adjustment layer")
        .css_classes(["flat", "circular"])
        .action_name("app.layer-add-adjustment")
        .build();
    header.append(&add_adjustment_btn);

    header
}

// Button sensitivity is driven from the action's enabled flag.
fn build_layers_footer() -> gtk::Box {
    let footer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(TAB_SPACING)
        .homogeneous(true)
        .margin_top(4)
        .build();

    let dup_btn = gtk::Button::builder()
        .label("Duplicate")
        .tooltip_text(tooltip_with_accel("Duplicate active layer", "layer-duplicate"))
        .css_classes(["suggested-action"])
        .action_name("app.layer-duplicate")
        .build();

    let merge_btn = gtk::Button::builder()
        .label("Merge")
        .tooltip_text(tooltip_with_accel("Merge selected layers", "layers-merge"))
        .css_classes(["suggested-action"])
        .action_name("app.layers-merge")
        .build();

    footer.append(&dup_btn);
    footer.append(&merge_btn);
    footer
}

/// Blend-mode dropdown + opacity slider, shown between the header and the layer
/// list. Both span the panel width and apply to every selected layer; a control
/// is only pushed onto its layers when the user actually touches it (a guard
/// suppresses the programmatic updates the sync callback makes on selection).
fn build_blend_controls(
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    history: &Rc<RefCell<HistoryStack>>,
) -> gtk::Box {
    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(TAB_SPACING)
        .build();

    let labels: Vec<&str> = BlendMode::ALL.iter().map(|m| m.label()).collect();
    let mode_dropdown = gtk::DropDown::from_strings(&labels);
    mode_dropdown.set_hexpand(true);
    mode_dropdown.set_tooltip_text(Some("Blend mode of the selected layers"));

    let opacity = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    opacity.set_hexpand(true);
    opacity.set_draw_value(false);
    opacity.set_tooltip_text(Some("Opacity of the selected layers"));

    let opacity_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(TAB_SPACING)
        .build();
    let opacity_label = gtk::Label::builder()
        .label("100%")
        .width_chars(4)
        .xalign(1.0)
        .build();
    opacity_label.add_css_class("dim-label");
    opacity_row.append(&opacity);
    opacity_row.append(&opacity_label);

    controls.append(&mode_dropdown);
    controls.append(&opacity_row);

    // True while the sync callback is writing the widgets, so their change
    // handlers don't treat the programmatic update as a user edit.
    let guard = Rc::new(std::cell::Cell::new(false));
    // Per-layer (id, blend, opacity) captured at the start of a slider drag, so
    // the whole drag commits as one undoable history entry on settle. Keyed by
    // id so a reorder during the debounce window can't mis-attribute it.
    let drag_origin: Rc<RefCell<Option<Vec<(String, BlendMode, f32)>>>> =
        Rc::new(RefCell::new(None));
    let commit_source: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    // Record the in-progress opacity drag as one history entry, comparing each
    // origin layer (by id) against its current opacity. Safe when no drag is
    // active. Does not touch the debounce timer; callers manage that slot.
    let commit_opacity: Rc<dyn Fn()> = {
        let ui = ui.clone();
        let history = Rc::clone(history);
        let drag_origin = Rc::clone(&drag_origin);
        Rc::new(move || {
            let Some(origin) = drag_origin.borrow_mut().take() else {
                return;
            };
            let snapshot = ui.state.snapshot();
            let actions: Vec<HistoryAction> = origin
                .iter()
                .filter_map(|(id, old_blend, old_op)| {
                    let layer = snapshot.iter().find(|l| &l.id == id)?;
                    if (layer.opacity - old_op).abs() < f32::EPSILON {
                        return None;
                    }
                    Some(HistoryAction::LayerBlend {
                        id: id.clone(),
                        old_blend: *old_blend,
                        old_opacity: *old_op,
                        new_blend: layer.blend,
                        new_opacity: layer.opacity,
                    })
                })
                .collect();
            record_blend_actions(&history, actions);
        })
    };

    // --- sync: reload the controls from the current selection ---
    let sync: Rc<dyn Fn()> = {
        let ui = ui.clone();
        let guard = Rc::clone(&guard);
        let mode_dropdown = mode_dropdown.clone();
        let opacity = opacity.clone();
        let opacity_label = opacity_label.clone();
        let commit_opacity = Rc::clone(&commit_opacity);
        let commit_source = Rc::clone(&commit_source);
        Rc::new(move || {
            // A selection change ends any in-progress opacity drag: flush its
            // pending history entry now so the next drag starts clean.
            if let Some(src) = commit_source.borrow_mut().take() {
                src.remove();
            }
            commit_opacity();
            let indices = ui.selected_indices();
            let sensitive = !indices.is_empty();
            mode_dropdown.set_sensitive(sensitive);
            opacity.set_sensitive(sensitive);
            // Show the primary (active) layer's values, falling back to the
            // first selected one.
            let primary = ui.state.active().filter(|i| indices.contains(i))
                .or_else(|| indices.first().copied());
            let (blend, op) = primary
                .and_then(|i| ui.state.blend(i))
                .unwrap_or((BlendMode::Normal, 1.0));
            guard.set(true);
            mode_dropdown.set_selected(blend.to_index());
            opacity.set_value(f64::from(op));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            opacity_label.set_label(&format!("{}%", (op * 100.0).round() as i32));
            guard.set(false);
        })
    };
    sync();
    *ui.blend_sync.borrow_mut() = Some(Rc::clone(&sync));

    // --- blend mode changed: applies to every selected layer at once ---
    {
        let ui = ui.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let history = Rc::clone(history);
        let guard = Rc::clone(&guard);
        let sync = Rc::clone(&sync);
        mode_dropdown.connect_selected_notify(move |dd| {
            if guard.get() {
                return;
            }
            let new_blend = BlendMode::from_index(dd.selected());
            let indices = ui.selected_indices();
            if indices.is_empty() {
                return;
            }
            let mut changes = Vec::with_capacity(indices.len());
            let mut actions = Vec::with_capacity(indices.len());
            for &idx in &indices {
                let Some((old_blend, op)) = ui.state.blend(idx) else { continue };
                if old_blend == new_blend {
                    continue;
                }
                let Some(id) = ui.state.snapshot().get(idx).map(|l| l.id.clone()) else {
                    continue;
                };
                changes.push((idx, new_blend, op));
                actions.push(HistoryAction::LayerBlend {
                    id,
                    old_blend,
                    old_opacity: op,
                    new_blend,
                    new_opacity: op,
                });
            }
            if changes.is_empty() {
                return;
            }
            if let Err(e) = canvas.borrow_mut().set_layers_blend(&changes) {
                tracing::error!(error = %e, "set_layers_blend (mode) failed");
                return;
            }
            record_blend_actions(&history, actions);
            redraw.request();
            sync();
        });
    }

    // --- opacity changed: live apply each event, one history entry per drag ---
    {
        let ui = ui.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let guard = Rc::clone(&guard);
        let drag_origin = Rc::clone(&drag_origin);
        let commit_source = Rc::clone(&commit_source);
        let commit_opacity = Rc::clone(&commit_opacity);
        let opacity_label = opacity_label.clone();
        opacity.connect_value_changed(move |scale| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let new_op = scale.value() as f32;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            opacity_label.set_label(&format!("{}%", (new_op * 100.0).round() as i32));
            if guard.get() {
                return;
            }
            let indices = ui.selected_indices();
            if indices.is_empty() {
                return;
            }
            // Capture the pre-drag state once, on the first event of a drag.
            // Keyed by id so the commit survives a reorder mid-debounce.
            if drag_origin.borrow().is_none() {
                let snapshot = ui.state.snapshot();
                let origin: Vec<(String, BlendMode, f32)> = indices
                    .iter()
                    .filter_map(|&i| {
                        let layer = snapshot.get(i)?;
                        Some((layer.id.clone(), layer.blend, layer.opacity))
                    })
                    .collect();
                *drag_origin.borrow_mut() = Some(origin);
            }
            let changes: Vec<(usize, BlendMode, f32)> = indices
                .iter()
                .filter_map(|&i| ui.state.blend(i).map(|(b, _)| (i, b, new_op)))
                .collect();
            if let Err(e) = canvas.borrow_mut().set_layers_blend(&changes) {
                tracing::error!(error = %e, "set_layers_blend (opacity) failed");
                return;
            }
            redraw.request();

            // Debounce: commit one history entry once the drag settles.
            if let Some(src) = commit_source.borrow_mut().take() {
                src.remove();
            }
            let commit_opacity = Rc::clone(&commit_opacity);
            let commit_source_inner = Rc::clone(&commit_source);
            let src = glib::timeout_add_local_once(
                std::time::Duration::from_millis(300),
                move || {
                    commit_source_inner.borrow_mut().take();
                    commit_opacity();
                },
            );
            *commit_source.borrow_mut() = Some(src);
        });
    }

    controls
}

/// Record one or more `LayerBlend` actions as a single undoable unit (a bare
/// action for one layer, a `Batch` for several). No-op when empty.
fn record_blend_actions(history: &Rc<RefCell<HistoryStack>>, mut actions: Vec<HistoryAction>) {
    let action = match actions.len() {
        0 => return,
        1 => actions.remove(0),
        _ => HistoryAction::Batch {
            label: "Change layer blend".to_string(),
            actions,
        },
    };
    history.borrow_mut().record(action);
}

// --- Height ---
#[allow(clippy::cast_precision_loss)]
const fn count_f64(n: usize) -> f64 {
    n as f64
}

const fn slot_top(index: usize) -> f64 {
    count_f64(index).mul_add(SLOT_HEIGHT, LIST_PADDING)
}

pub(super) fn sync_height(area: &gtk::DrawingArea, ui: &Ui) {
    let snapshot = ui.state.snapshot();
    reconcile_tree(&mut ui.tree.borrow_mut(), &snapshot);
    let count = compute_visible_rows(&ui.tree.borrow(), &snapshot).len();
    let body = count_f64(count)
        .mul_add(ITEM_HEIGHT, count_f64(count.saturating_sub(1)) * ITEM_GAP);
    let total = LIST_PADDING.mul_add(2.0, body);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    area.set_content_height(total.ceil() as i32);
}

// --- Drawing ---
fn install_list_draw(area: &gtk::DrawingArea, ui: &Ui) {
    let ui = ui.clone();
    area.set_draw_func(move |widget, ctx, width_px, _h| {
        let palette = Palette::resolve(widget);
        let snapshot = ui.state.snapshot();
        let drag = ui.drag.borrow().clone();
        let active_id = ui.active_id();
        let active_group = ui.active_group.borrow().clone();
        let multi = ui.multi_selected.borrow();
        let thumbnails = ui.thumbnails.borrow();
        let width = f64::from(width_px);

        let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
        let count = rows.len();
        // No hover highlight mid-drag (rows are sliding around).
        let hover = if drag.is_none() { *ui.hover.borrow() } else { None };

        // Span covers a group header plus its expanded children when one is being dragged.
        let drag_handle = drag.as_ref().and_then(|d| {
            if d.zone == HitZone::Handle && d.from_row < count {
                let span = drag_span(&rows, d.from_row);
                let to = d.current_row.min(count.saturating_sub(span));
                Some((d.from_row, span, to))
            } else {
                None
            }
        });

        for (row_idx, row) in rows.iter().enumerate() {
            let is_dragged = drag_handle.is_some_and(|(from, span, _)| {
                row_idx >= from && row_idx < from + span
            });
            if is_dragged {
                continue;
            }
            let y_offset = drag
                .as_ref()
                .filter(|d| d.zone == HitZone::Handle)
                .and_then(|d| d.row_y_anim.get(row_idx).copied())
                .unwrap_or(0.0);
            let y = slot_top(row_idx) + y_offset;
            let thumb = match &row.kind {
                RowKind::Layer { flat_idx, .. } => {
                    thumbnails.get(*flat_idx).and_then(|o| o.as_ref())
                }
                RowKind::Group { .. } => None,
            };
            let is_active = match &row.kind {
                RowKind::Layer { id, .. } => active_id.as_deref() == Some(id.as_str()),
                RowKind::Group { id, .. } => active_group.as_deref() == Some(id.as_str()),
            };
            let is_multi = match &row.kind {
                RowKind::Layer { id, .. } | RowKind::Group { id, .. } => {
                    multi.contains(id.as_str())
                }
            };
            let is_component = row_is_component(&ui, row);
            let is_text = row_is_text(&ui, row);
            let is_adjustment = row_is_adjustment(&ui, row);
            let mask_active = is_adjustment && row_mask_active(&ui, row);
            let hover_zone = hover.and_then(|(hr, z)| (hr == row_idx).then_some(z));
            draw_row(ctx, &palette, width, y, row, (row.depth + row.adjust_indent) as f64, is_active, is_multi, thumb, is_component, is_text, is_adjustment, mask_active, hover_zone);
        }

        if let Some((from, span, _)) = drag_handle
            && let Some(d) = &drag {
                let max_top = if count > span {
                    slot_top(count - span)
                } else {
                    LIST_PADDING
                };
                let y_start = (d.pointer_y - d.grab_offset_y)
                    .clamp(LIST_PADDING, max_top);
                let anim = d.animated_depth_offset;
                for i in 0..span {
                    let row_idx = from + i;
                    if row_idx >= count {
                        break;
                    }
                    let row = &rows[row_idx];
                    let y = y_start + count_f64(i) * SLOT_HEIGHT;
                    let thumb = match &row.kind {
                        RowKind::Layer { flat_idx, .. } => {
                            thumbnails.get(*flat_idx).and_then(|o| o.as_ref())
                        }
                        RowKind::Group { .. } => None,
                    };
                    let is_active = match &row.kind {
                        RowKind::Layer { id, .. } => active_id.as_deref() == Some(id.as_str()),
                        RowKind::Group { id, .. } => active_group.as_deref() == Some(id.as_str()),
                    };
                    let depth_f = ((row.depth + row.adjust_indent) as f64 + anim).max(0.0);
                    let is_component = row_is_component(&ui, row);
                    let is_text = row_is_text(&ui, row);
                    let is_adjustment = row_is_adjustment(&ui, row);
                    let mask_active = is_adjustment && row_mask_active(&ui, row);
                    draw_row(ctx, &palette, width, y, row, depth_f, is_active, false, thumb, is_component, is_text, is_adjustment, mask_active, None);
                }
            }
    });
}

// 1 for a layer or collapsed group; expanded group covers header plus children.
fn drag_span(rows: &[VisibleRow], from_row: usize) -> usize {
    let Some(first) = rows.get(from_row) else { return 0 };
    let group_depth = match &first.kind {
        RowKind::Layer { .. } => return 1,
        RowKind::Group { .. } => first.depth,
    };
    let mut span = 1;
    for row in &rows[from_row + 1..] {
        if row.depth > group_depth {
            span += 1;
        } else {
            break;
        }
    }
    span
}

// Where a non-dragged row should sit while a span of rows is being moved.
fn displaced_row(row: usize, from: usize, span: usize, to: usize) -> usize {
    match from.cmp(&to) {
        std::cmp::Ordering::Less => {
            if row >= from + span && row < to + span { row - span } else { row }
        }
        std::cmp::Ordering::Greater => {
            if row >= to && row < from { row + span } else { row }
        }
        std::cmp::Ordering::Equal => row,
    }
}

#[derive(Clone, Copy)]
struct Palette {
    row_bg: Rgb,
    accent_bg: Rgb,
    accent_fg: Rgb,
    fg: Rgb,
}

impl Palette {
    fn resolve(widget: &gtk::DrawingArea) -> Self {
        Self {
            row_bg: lookup(widget, "window_bg_color").unwrap_or(FALLBACK_ROW_BG),
            accent_bg: lookup(widget, "accent_bg_color").unwrap_or(FALLBACK_ACCENT_BG),
            accent_fg: lookup(widget, "accent_fg_color").unwrap_or(FALLBACK_ACCENT_FG),
            fg: lookup(widget, "view_fg_color").unwrap_or(FALLBACK_FG),
        }
    }
}

fn lookup(widget: &gtk::DrawingArea, name: &str) -> Option<Rgb> {
    #[allow(deprecated)]
    let rgba = widget.style_context().lookup_color(name)?;
    Some((f64::from(rgba.red()), f64::from(rgba.green()), f64::from(rgba.blue())))
}

/// Whether a visible row maps to a component-instance layer (for the accent
/// swatch outline).
fn row_is_component(ui: &Ui, row: &VisibleRow) -> bool {
    match &row.kind {
        RowKind::Layer { flat_idx, .. } => ui
            .state
            .kind(*flat_idx)
            .is_some_and(|k| matches!(k, oxiedraw_core::document::LayerKind::Component(_))),
        RowKind::Group { .. } => false,
    }
}

fn row_is_text(ui: &Ui, row: &VisibleRow) -> bool {
    match &row.kind {
        RowKind::Layer { flat_idx, .. } => ui
            .state
            .kind(*flat_idx)
            .is_some_and(|k| matches!(k, oxiedraw_core::document::LayerKind::Text(_))),
        RowKind::Group { .. } => false,
    }
}

pub(super) fn row_is_adjustment(ui: &Ui, row: &VisibleRow) -> bool {
    match &row.kind {
        RowKind::Layer { flat_idx, .. } => ui
            .state
            .kind(*flat_idx)
            .is_some_and(|k| matches!(k, oxiedraw_core::document::LayerKind::Adjustment(_))),
        RowKind::Group { .. } => false,
    }
}

// Whether this row's adjustment mask is currently toggled into canvas view.
fn row_mask_active(ui: &Ui, row: &VisibleRow) -> bool {
    match &row.kind {
        RowKind::Layer { id, .. } => ui.mask_view.borrow().as_deref() == Some(id.as_str()),
        RowKind::Group { .. } => false,
    }
}

fn draw_row(
    ctx: &cairo::Context,
    palette: &Palette,
    width: f64,
    top: f64,
    row: &VisibleRow,
    depth_f: f64,
    is_active: bool,
    is_multi: bool,
    thumbnail: Option<&cairo::ImageSurface>,
    is_component: bool,
    is_text: bool,
    is_adjustment: bool,
    mask_active: bool,
    hover_zone: Option<HitZone>,
) {
    let indent = depth_f * INDENT_STEP;
    let left = LIST_PADDING;
    let row_w = LIST_PADDING.mul_add(-2.0, width).max(0.0);
    let right = left + row_w;

    let (visible, name) = match &row.kind {
        RowKind::Layer { visible, name, .. } | RowKind::Group { visible, name, .. } => (*visible, name.as_str()),
    };

    let dim = !visible;

    let bg = if is_active {
        palette.accent_bg
    } else if is_multi {
        lerp_rgb(palette.row_bg, palette.accent_bg, 0.25)
    } else {
        palette.row_bg
    };
    let text_color = if is_active { palette.accent_fg } else { palette.fg };
    let icon_color = text_color;

    let content_left = left + indent;
    let indented_w = (row_w - indent).max(0.0);
    rounded_rect(ctx, content_left, top, indented_w, ITEM_HEIGHT, ITEM_RADIUS);
    set_source(ctx, bg);
    ctx.fill().ok();

    match &row.kind {
        RowKind::Layer { .. } => {
            let sx = content_left + ITEM_INNER_PAD;
            let sy = top + (ITEM_HEIGHT - SWATCH_SIZE) / 2.0;
            if dim {
                ctx.push_group();
            }
            draw_swatch(ctx, sx, sy, thumbnail);
            if dim {
                ctx.pop_group_to_source().ok();
                ctx.paint_with_alpha(0.4).ok();
            }
            // Component instances get a 2px accent outline on the swatch to
            // distinguish them from raster layers.
            if is_component {
                rounded_rect(ctx, sx + 1.0, sy + 1.0, SWATCH_SIZE - 2.0, SWATCH_SIZE - 2.0, SWATCH_RADIUS);
                set_source(ctx, palette.accent_bg);
                ctx.set_line_width(2.0);
                ctx.stroke().ok();
            }

            let handle_left = right - ITEM_INNER_PAD - HANDLE_WIDTH;
            let eye_cx = handle_left - ITEM_INNER_PAD - EYE_RADIUS;
            let eye_cy = top + ITEM_HEIGHT / 2.0;

            // Adjustment layers carry two icons left of the eye: the "edit
            // settings" sliders (next to the eye) and the "show mask" toggle.
            let sliders_cx = eye_cx - EYE_RADIUS - EDIT_EYE_GAP - EDIT_RADIUS;
            let mask_cx = sliders_cx - EDIT_RADIUS - EDIT_EYE_GAP - MASK_RADIUS;
            let controls_left = if is_adjustment {
                mask_cx - MASK_RADIUS
            } else {
                eye_cx - EYE_RADIUS
            };

            let badge_letter: Option<&str> = if is_component {
                Some("C")
            } else if is_text {
                Some("T")
            } else {
                None
            };
            const BADGE_W: f64 = 14.0;
            const BADGE_GAP: f64 = 4.0;
            let badge_x = sx + SWATCH_SIZE + BADGE_GAP;
            let text_left = if badge_letter.is_some() {
                badge_x + BADGE_W + BADGE_GAP
            } else {
                sx + SWATCH_SIZE + ITEM_INNER_PAD
            };
            let text_max_w = controls_left - ITEM_INNER_PAD - text_left;

            if let Some(letter) = badge_letter {
                let badge_cy = top + ITEM_HEIGHT / 2.0;
                rounded_rect(ctx, badge_x, badge_cy - BADGE_W / 2.0, BADGE_W, BADGE_W, 3.0);
                let alpha = if dim { 0.08 } else { 0.15 };
                ctx.set_source_rgba(text_color.0, text_color.1, text_color.2, alpha);
                ctx.fill().ok();
                ctx.save().ok();
                ctx.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
                ctx.set_font_size(9.0);
                if let Ok(te) = ctx.text_extents(letter) {
                    let tx = badge_x + BADGE_W / 2.0 - te.width() / 2.0 - te.x_bearing();
                    let ty = badge_cy - te.height() / 2.0 - te.y_bearing();
                    let alpha = if dim { 0.4 } else { 1.0 };
                    ctx.set_source_rgba(text_color.0, text_color.1, text_color.2, alpha);
                    ctx.move_to(tx, ty);
                    ctx.show_text(letter).ok();
                }
                ctx.restore().ok();
            }

            if dim {
                ctx.save().ok();
                ctx.set_source_rgba(text_color.0, text_color.1, text_color.2, 0.4);
            } else {
                set_source(ctx, text_color);
            }
            draw_label(ctx, name, text_left, top + ITEM_HEIGHT / 2.0, text_max_w);
            if dim {
                ctx.restore().ok();
            }

            if hover_zone == Some(HitZone::Eye) {
                draw_icon_hover_bg(ctx, eye_cx, eye_cy, EYE_RADIUS, palette.fg);
            }
            draw_eye(ctx, eye_cx, eye_cy, EYE_RADIUS, visible, icon_color, dim);
            if is_adjustment {
                if hover_zone == Some(HitZone::Edit) {
                    draw_icon_hover_bg(ctx, sliders_cx, eye_cy, EDIT_RADIUS, palette.fg);
                }
                draw_sliders(ctx, sliders_cx, eye_cy, EDIT_RADIUS, icon_color, dim);
                if hover_zone == Some(HitZone::Mask) {
                    draw_icon_hover_bg(ctx, mask_cx, eye_cy, MASK_RADIUS, palette.fg);
                }
                // Toggle state: on = full icon, off = dimmed. Use icon_color so
                // it stays visible against the accent background of an active row.
                draw_mask(ctx, mask_cx, eye_cy, MASK_RADIUS, icon_color, dim || !mask_active);
            }
            set_source(ctx, icon_color);
            if dim {
                ctx.save().ok();
                ctx.set_source_rgba(icon_color.0, icon_color.1, icon_color.2, 0.3);
                draw_handle(ctx, handle_left, top + ITEM_HEIGHT / 2.0);
                ctx.restore().ok();
            } else {
                draw_handle(ctx, handle_left, top + ITEM_HEIGHT / 2.0);
            }
        }
        RowKind::Group { expanded, .. } => {
            let chevron_cx = content_left + ITEM_INNER_PAD + CHEVRON_SIZE / 2.0;
            let chevron_cy = top + ITEM_HEIGHT / 2.0;
            let folder_x = content_left + ITEM_INNER_PAD + CHEVRON_SIZE + 4.0;
            let folder_cx = folder_x + FOLDER_W / 2.0;
            let folder_cy = top + ITEM_HEIGHT / 2.0;
            let handle_left = right - ITEM_INNER_PAD - HANDLE_WIDTH;
            let eye_cx = handle_left - ITEM_INNER_PAD - EYE_RADIUS;
            let eye_cy = top + ITEM_HEIGHT / 2.0;
            let text_left = folder_x + FOLDER_W + ITEM_INNER_PAD;
            let text_max_w = eye_cx - EYE_RADIUS - ITEM_INNER_PAD - text_left;

            if dim {
                ctx.save().ok();
                ctx.set_source_rgba(icon_color.0, icon_color.1, icon_color.2, 0.4);
            } else {
                set_source(ctx, icon_color);
            }
            draw_chevron(ctx, chevron_cx, chevron_cy, CHEVRON_SIZE, *expanded);
            draw_folder(ctx, folder_cx, folder_cy, FOLDER_W, FOLDER_H);
            if dim {
                ctx.restore().ok();
            }

            if dim {
                ctx.save().ok();
                ctx.set_source_rgba(text_color.0, text_color.1, text_color.2, 0.4);
            } else {
                set_source(ctx, text_color);
            }
            draw_label(ctx, name, text_left, top + ITEM_HEIGHT / 2.0, text_max_w);
            if dim {
                ctx.restore().ok();
            }

            if hover_zone == Some(HitZone::Eye) {
                draw_icon_hover_bg(ctx, eye_cx, eye_cy, EYE_RADIUS, palette.fg);
            }
            draw_eye(ctx, eye_cx, eye_cy, EYE_RADIUS, visible, icon_color, dim);
            set_source(ctx, icon_color);
            if dim {
                ctx.save().ok();
                ctx.set_source_rgba(icon_color.0, icon_color.1, icon_color.2, 0.3);
                draw_handle(ctx, handle_left, top + ITEM_HEIGHT / 2.0);
                ctx.restore().ok();
            } else {
                draw_handle(ctx, handle_left, top + ITEM_HEIGHT / 2.0);
            }
        }
    }
}

fn lerp_rgb(a: Rgb, b: Rgb, t: f64) -> Rgb {
    (
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

fn draw_swatch(ctx: &cairo::Context, x: f64, y: f64, thumbnail: Option<&cairo::ImageSurface>) {
    draw_checkerboard(ctx, x, y);
    let Some(surf) = thumbnail else { return };
    ctx.save().ok();
    rounded_rect(ctx, x, y, SWATCH_SIZE, SWATCH_SIZE, SWATCH_RADIUS);
    ctx.clip();
    ctx.set_source_surface(surf, x, y).ok();
    ctx.paint().ok();
    ctx.restore().ok();
}

fn draw_checkerboard(ctx: &cairo::Context, x: f64, y: f64) {
    const CELL: f64 = 4.0;
    ctx.save().ok();
    rounded_rect(ctx, x, y, SWATCH_SIZE, SWATCH_SIZE, SWATCH_RADIUS);
    ctx.clip();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cols = (SWATCH_SIZE / CELL).ceil() as i32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rows = (SWATCH_SIZE / CELL).ceil() as i32;
    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                ctx.set_source_rgb(0.85, 0.85, 0.85);
            } else {
                ctx.set_source_rgb(1.0, 1.0, 1.0);
            }
            ctx.rectangle(
                x + f64::from(col) * CELL,
                y + f64::from(row) * CELL,
                CELL,
                CELL,
            );
            ctx.fill().ok();
        }
    }
    ctx.restore().ok();
}

fn draw_eye(ctx: &cairo::Context, cx: f64, cy: f64, r: f64, open: bool, color: Rgb, dim: bool) {
    ctx.save().ok();
    let alpha = if dim { 0.35 } else { 1.0 };
    ctx.set_source_rgba(color.0, color.1, color.2, alpha);
    ctx.set_line_width(1.5);
    if open {
        // Almond outline
        ctx.move_to(cx - r, cy);
        ctx.curve_to(cx - r * 0.5, cy - r * 0.65, cx + r * 0.5, cy - r * 0.65, cx + r, cy);
        ctx.curve_to(cx + r * 0.5, cy + r * 0.65, cx - r * 0.5, cy + r * 0.65, cx - r, cy);
        ctx.close_path();
        ctx.stroke().ok();
        // Pupil
        ctx.arc(cx, cy, r * 0.38, 0.0, TAU);
        ctx.fill().ok();
    } else {
        // Closed eyelid: the bottom curve of the almond, no strike-through.
        ctx.set_line_cap(cairo::LineCap::Round);
        ctx.move_to(cx - r, cy);
        ctx.curve_to(
            cx - r * 0.5, cy + r * 0.65,
            cx + r * 0.5, cy + r * 0.65,
            cx + r,       cy,
        );
        ctx.stroke().ok();
    }
    ctx.restore().ok();
}

// Subtle rounded background drawn behind a clickable row icon while it is
// hovered. `color` is the foreground tint, applied at a low alpha.
fn draw_icon_hover_bg(ctx: &cairo::Context, cx: f64, cy: f64, r: f64, color: Rgb) {
    ctx.arc(cx, cy, r + 4.0, 0.0, TAU);
    ctx.set_source_rgba(color.0, color.1, color.2, 0.14);
    ctx.fill().ok();
}

// Horizontal "sliders" icon (two tracks, each with an offset knob) marking an
// adjustment layer's edit-settings button.
fn draw_sliders(ctx: &cairo::Context, cx: f64, cy: f64, r: f64, color: Rgb, dim: bool) {
    ctx.save().ok();
    let alpha = if dim { 0.35 } else { 1.0 };
    ctx.set_source_rgba(color.0, color.1, color.2, alpha);
    ctx.set_line_width(1.4);
    ctx.set_line_cap(cairo::LineCap::Round);

    let left = cx - r;
    let right = cx + r;
    let knob = r * 0.34;
    // Top track: knob sits left of centre. Bottom track: knob sits right.
    for (ty, knob_x) in [(cy - r * 0.45, cx - r * 0.3), (cy + r * 0.45, cx + r * 0.3)] {
        ctx.move_to(left, ty);
        ctx.line_to(right, ty);
        ctx.stroke().ok();
        ctx.arc(knob_x, ty, knob, 0.0, TAU);
        ctx.fill().ok();
    }
    ctx.restore().ok();
}

// "Show mask" toggle: a ring with a filled right half. Same glyph on or off -
// only the alpha differs (full when on, dimmed when off), driven by `dim`.
fn draw_mask(ctx: &cairo::Context, cx: f64, cy: f64, r: f64, color: Rgb, dim: bool) {
    ctx.save().ok();
    let alpha = if dim { 0.35 } else { 1.0 };
    ctx.set_source_rgba(color.0, color.1, color.2, alpha);
    let rr = r * 0.85;
    // Outline ring.
    ctx.set_line_width(1.4);
    ctx.arc(cx, cy, rr, 0.0, TAU);
    ctx.stroke().ok();
    // Filled right half (top -> bottom along the right side).
    ctx.arc(cx, cy, rr - 0.7, -TAU / 4.0, TAU / 4.0);
    ctx.close_path();
    ctx.fill().ok();
    ctx.restore().ok();
}

fn draw_chevron(ctx: &cairo::Context, cx: f64, cy: f64, size: f64, expanded: bool) {
    ctx.save().ok();
    ctx.set_line_width(1.8);
    ctx.set_line_cap(cairo::LineCap::Round);
    let arm = size * 0.35;
    if expanded {
        ctx.move_to(cx - arm, cy - arm * 0.5);
        ctx.line_to(cx, cy + arm * 0.5);
        ctx.line_to(cx + arm, cy - arm * 0.5);
    } else {
        ctx.move_to(cx - arm * 0.5, cy - arm);
        ctx.line_to(cx + arm * 0.5, cy);
        ctx.line_to(cx - arm * 0.5, cy + arm);
    }
    ctx.stroke().ok();
    ctx.restore().ok();
}

fn draw_folder(ctx: &cairo::Context, cx: f64, cy: f64, w: f64, h: f64) {
    let x = cx - w / 2.0;
    let y = cy - h / 2.0;
    let tab_w = w * 0.42;
    let tab_h = h * 0.24;
    // Tab
    ctx.move_to(x, y + tab_h);
    ctx.line_to(x + tab_w * 0.75, y + tab_h);
    ctx.line_to(x + tab_w, y);
    ctx.line_to(x, y);
    ctx.close_path();
    ctx.fill().ok();
    // Body
    ctx.rectangle(x, y + tab_h, w, h - tab_h);
    ctx.fill().ok();
}

fn draw_label(ctx: &cairo::Context, text: &str, x: f64, cy: f64, max_w: f64) {
    if max_w <= 0.0 {
        return;
    }
    ctx.save().ok();
    ctx.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    ctx.set_font_size(13.0);
    let baseline = ctx
        .text_extents(text)
        .map_or(cy, |e| cy - e.height() / 2.0 - e.y_bearing());
    ctx.rectangle(x, cy - ITEM_HEIGHT / 2.0, max_w, ITEM_HEIGHT);
    ctx.clip();
    ctx.move_to(x, baseline);
    ctx.show_text(text).ok();
    ctx.restore().ok();
}

fn draw_handle(ctx: &cairo::Context, x: f64, cy: f64) {
    let total_h = HANDLE_LINE_THICKNESS.mul_add(3.0, HANDLE_LINE_GAP * 2.0);
    let mut y = cy - total_h / 2.0;
    for _ in 0..3 {
        rounded_rect(
            ctx,
            x,
            y,
            HANDLE_WIDTH,
            HANDLE_LINE_THICKNESS,
            HANDLE_LINE_THICKNESS / 2.0,
        );
        ctx.fill().ok();
        y += HANDLE_LINE_THICKNESS + HANDLE_LINE_GAP;
    }
}

fn set_source(ctx: &cairo::Context, color: Rgb) {
    ctx.set_source_rgb(color.0, color.1, color.2);
}

fn rounded_rect(ctx: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    ctx.new_sub_path();
    ctx.arc(x + width - radius, y + radius, radius, -TAU / 4.0, 0.0);
    ctx.arc(x + width - radius, y + height - radius, radius, 0.0, TAU / 4.0);
    ctx.arc(x + radius, y + height - radius, radius, TAU / 4.0, TAU / 2.0);
    ctx.arc(x + radius, y + radius, radius, TAU / 2.0, 3.0 * TAU / 4.0);
    ctx.close_path();
}

// --- Input ---
/// Which zone of a row was clicked?
fn hit_zone(
    x: f64,
    y_in_row: f64,
    widget_width: f64,
    depth: usize,
    is_group: bool,
    has_edit: bool,
) -> HitZone {
    let indent = count_f64(depth) * INDENT_STEP;
    let content_left = LIST_PADDING + indent;
    let row_w = LIST_PADDING.mul_add(-2.0, widget_width).max(0.0);
    let right = LIST_PADDING + row_w;

    // Handle (rightmost).
    let handle_left = right - ITEM_INNER_PAD - HANDLE_WIDTH;
    if x >= handle_left - 4.0 {
        return HitZone::Handle;
    }

    // Eye (just left of handle).
    let eye_cx = handle_left - ITEM_INNER_PAD - EYE_RADIUS;
    if (x - eye_cx).abs() <= EYE_RADIUS + 4.0 {
        return HitZone::Eye;
    }

    // Adjustment icons: edit-settings sliders (left of the eye), then the
    // show-mask toggle (left of the sliders).
    if has_edit {
        let sliders_cx = eye_cx - EYE_RADIUS - EDIT_EYE_GAP - EDIT_RADIUS;
        if (x - sliders_cx).abs() <= EDIT_RADIUS + 4.0 {
            return HitZone::Edit;
        }
        let mask_cx = sliders_cx - EDIT_RADIUS - EDIT_EYE_GAP - MASK_RADIUS;
        if (x - mask_cx).abs() <= MASK_RADIUS + 4.0 {
            return HitZone::Mask;
        }
    }

    // Chevron or swatch (leftmost content).
    if is_group {
        let chevron_right = content_left + ITEM_INNER_PAD + CHEVRON_SIZE + 4.0;
        if x <= chevron_right {
            return HitZone::Chevron;
        }
    } else {
        let sx = content_left + ITEM_INNER_PAD;
        let sy = (ITEM_HEIGHT - SWATCH_SIZE) / 2.0;
        if x >= sx && x <= sx + SWATCH_SIZE && y_in_row >= sy && y_in_row <= sy + SWATCH_SIZE {
            return HitZone::Swatch;
        }
    }

    HitZone::Body
}

/// Make `flat_idx` the active layer and open the adjustment editor on it. The
/// `layer-add-adjustment` action edits the active adjustment layer in place, so
/// selecting first targets the right one regardless of prior selection.
pub(super) fn open_adjustment_editor(ui: &Ui, area: &gtk::DrawingArea, flat_idx: usize) {
    ui.multi_selected.borrow_mut().clear();
    ui.state.select_index(flat_idx);
    *ui.active_group.borrow_mut() = None;
    area.queue_draw();
    if let Some(gio_app) = gio::Application::default()
        && let Ok(app) = gio_app.downcast::<gtk::Application>() {
            app.activate_action("layer-add-adjustment", None);
        }
}

fn install_list_input(
    area: &gtk::DrawingArea,
    ui: &Ui,
    canvas: Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    select_layer_content: &Rc<dyn Fn(usize)>,
    history: &Rc<RefCell<HistoryStack>>,
    on_edit_component: &Rc<dyn Fn(String)>,
    prepare_reorder: &Rc<dyn Fn()>,
) {
    let drag_gesture = gtk::GestureDrag::new();
    drag_gesture.set_button(gdk::BUTTON_PRIMARY);

    // --- drag_begin ---
    {
        let area_w = area.clone();
        let ui = ui.clone();
        drag_gesture.connect_drag_begin(move |_gesture, x, y| {
            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
            let Some(row_idx) = item_at(y, rows.len()) else { return };
            let row = &rows[row_idx];
            #[allow(deprecated)]
            let width = f64::from(area_w.allocated_width());
            let y_in_row = y - slot_top(row_idx);
            let is_group = matches!(row.kind, RowKind::Group { .. });
            let has_edit = row_is_adjustment(&ui, row);
            let zone = hit_zone(x, y_in_row, width, row.depth + row.adjust_indent, is_group, has_edit);

            *ui.drag.borrow_mut() = Some(Drag {
                from_row: row_idx,
                current_row: row_idx,
                pointer_y: y,
                grab_offset_y: y - slot_top(row_idx),
                zone,
                animated_depth_offset: 0.0,
                row_y_anim: vec![0.0; rows.len()],
                last_frame_time_us: 0,
            });
            if zone == HitZone::Handle {
                area_w.set_cursor(gtk::gdk::Cursor::from_name("row-resize", None).as_ref());

                // Runs every frame while a handle drag is in flight; smooths
                // the floating preview's indent and the "make room" Y nudges.
                let tick_ui = ui.clone();
                area_w.add_tick_callback(move |widget, clock| {
                    // Borrow only briefly so the tree/state borrow below is free.
                    let (from_row, current_row, last_us) = {
                        let d = tick_ui.drag.borrow();
                        match d.as_ref() {
                            Some(d) if d.zone == HitZone::Handle => {
                                (d.from_row, d.current_row, d.last_frame_time_us)
                            }
                            _ => return glib::ControlFlow::Break,
                        }
                    };

                    let now = clock.frame_time();
                    let dt = if last_us == 0 {
                        0.0_f64
                    } else {
                        ((now - last_us) as f64 * 1e-6).clamp(0.0, 0.1)
                    };

                    // Auto-scroll when the grabbed row nears a viewport edge. Speed
                    // ramps linearly from zero at the zone boundary to max at the edge.
                    let mut current_row = current_row;
                    if let Some(scroll) = widget
                        .ancestor(gtk::ScrolledWindow::static_type())
                        .and_downcast::<gtk::ScrolledWindow>()
                    {
                        let vadj = scroll.vadjustment();
                        let page = vadj.page_size();
                        let pointer_y =
                            tick_ui.drag.borrow().as_ref().map_or(0.0, |d| d.pointer_y);
                        let pointer_in_view = pointer_y - vadj.value();

                        let velocity = if pointer_in_view < AUTOSCROLL_EDGE {
                            let t = ((AUTOSCROLL_EDGE - pointer_in_view) / AUTOSCROLL_EDGE)
                                .clamp(0.0, 1.0);
                            -AUTOSCROLL_MAX_SPEED * t
                        } else if pointer_in_view > page - AUTOSCROLL_EDGE {
                            let t = ((pointer_in_view - (page - AUTOSCROLL_EDGE)) / AUTOSCROLL_EDGE)
                                .clamp(0.0, 1.0);
                            AUTOSCROLL_MAX_SPEED * t
                        } else {
                            0.0
                        };

                        if velocity != 0.0 && dt > 0.0 {
                            let max_val = (vadj.upper() - page).max(vadj.lower());
                            let new_val =
                                (vadj.value() + velocity * dt).clamp(vadj.lower(), max_val);
                            let applied = new_val - vadj.value();
                            if applied != 0.0 {
                                vadj.set_value(new_val);
                                // Keep the drop indicator under the stationary pointer
                                // as content scrolls beneath it.
                                let snapshot = tick_ui.state.snapshot();
                                let count =
                                    compute_visible_rows(&tick_ui.tree.borrow(), &snapshot).len();
                                let mut d = tick_ui.drag.borrow_mut();
                                if let Some(d) = d.as_mut() {
                                    d.pointer_y += applied;
                                    d.current_row =
                                        insertion_index(d.pointer_y - d.grab_offset_y, count);
                                    current_row = d.current_row;
                                }
                            }
                        }
                    }

                    let (target_depth_offset, span, clamped_to) = {
                        let snapshot = tick_ui.state.snapshot();
                        let rows = compute_visible_rows(&tick_ui.tree.borrow(), &snapshot);
                        let count = rows.len();
                        let span = drag_span(&rows, from_row);
                        let to = current_row.min(count.saturating_sub(span));
                        let orig_depth = rows.get(from_row).map_or(0, |r| r.depth);
                        let rows2: Vec<VisibleRow> = rows
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| *i < from_row || *i >= from_row + span)
                            .map(|(_, r)| r.clone())
                            .collect();
                        let (ins_parent, _) = resolve_insert_target(&rows2, to);
                        let target_depth: usize = ins_parent.as_ref().map_or(0, |gid| {
                            rows2
                                .iter()
                                .find(|r| matches!(&r.kind, RowKind::Group { id, .. } if id == gid))
                                .map_or(1, |r| r.depth + 1)
                        });
                        (target_depth as f64 - orig_depth as f64, span, to)
                    };

                    // Exponential smoothing, ~95% in 0.1s.
                    let alpha = 1.0 - (-dt / (0.1 / 3.0)).exp();

                    {
                        let mut d = tick_ui.drag.borrow_mut();
                        if let Some(d) = d.as_mut() {
                            d.animated_depth_offset +=
                                (target_depth_offset - d.animated_depth_offset) * alpha;

                            for (r, anim_y) in d.row_y_anim.iter_mut().enumerate() {
                                if r >= from_row && r < from_row + span {
                                    continue;
                                }
                                let target_slot =
                                    displaced_row(r, from_row, span, clamped_to);
                                let target_y = slot_top(target_slot) - slot_top(r);
                                *anim_y += (target_y - *anim_y) * alpha;
                            }

                            d.last_frame_time_us = now;
                        }
                    }

                    widget.queue_draw();
                    glib::ControlFlow::Continue
                });
            }
            area_w.queue_draw();
        });
    }

    // --- drag_update ---
    {
        let area_w = area.clone();
        let ui = ui.clone();
        drag_gesture.connect_drag_update(move |gesture, _dx, dy| {
            let Some((_sx, sy)) = gesture.start_point() else { return };
            let mut drag_ref = ui.drag.borrow_mut();
            let Some(d) = drag_ref.as_mut() else { return };
            if d.zone != HitZone::Handle {
                return;
            }
            let pointer_y = sy + dy;
            d.pointer_y = pointer_y;
            let snapshot = ui.state.snapshot();
            let count = compute_visible_rows(&ui.tree.borrow(), &snapshot).len();
            d.current_row = insertion_index(pointer_y - d.grab_offset_y, count);
            drop(drag_ref);
            area_w.queue_draw();
        });
    }

    // --- drag_end ---
    {
        let area_w = area.clone();
        let ui = ui.clone();
        let redraw = redraw.clone();
        let canvas = Rc::clone(&canvas);
        let select_layer_content = Rc::clone(select_layer_content);
        let history = Rc::clone(history);
        let prepare_reorder = Rc::clone(prepare_reorder);
        drag_gesture.connect_drag_end(move |gesture, dx, dy| {
            let drag_state = ui.drag.borrow_mut().take();
            let Some(d) = drag_state else { return };
            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);

            let modifiers = gesture
                .current_event()
                .map_or(gdk::ModifierType::empty(), |ev| ev.modifier_state());
            let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
            let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);

            let is_click = dx.abs() < 4.0 && dy.abs() < 4.0;

            match d.zone {
                HitZone::Eye => {
                    if d.from_row < rows.len() {
                        let row = &rows[d.from_row];
                        match &row.kind {
                            RowKind::Layer { id, flat_idx, visible, .. } => {
                                let new_vis = !visible;
                                if let Err(e) = canvas
                                    .borrow_mut()
                                    .set_layer_visible(*flat_idx, new_vis)
                                {
                                    tracing::error!(error = %e, "set_layer_visible failed");
                                } else {
                                    history.borrow_mut().record(HistoryAction::LayerVisibility {
                                        id: id.clone(),
                                        old: *visible,
                                        new: new_vis,
                                    });
                                    area_w.queue_draw();
                                    redraw.request();
                                }
                            }
                            RowKind::Group { id, visible, .. } => {
                                toggle_group_visibility(&ui, &canvas, id, !visible);
                                area_w.queue_draw();
                                redraw.request();
                            }
                        }
                    }
                }
                HitZone::Edit => {
                    if is_click
                        && d.from_row < rows.len()
                        && let RowKind::Layer { flat_idx, .. } = &rows[d.from_row].kind {
                            open_adjustment_editor(&ui, &area_w, *flat_idx);
                        }
                }
                HitZone::Mask => {
                    if is_click
                        && d.from_row < rows.len()
                        && let RowKind::Layer { id, flat_idx, .. } = &rows[d.from_row].kind {
                            let turn_on = ui.mask_view.borrow().as_deref() != Some(id.as_str());
                            let new_view = turn_on.then(|| id.clone());
                            // Editing the mask paints the active layer, so adopt
                            // this one when turning the view on.
                            if turn_on {
                                ui.multi_selected.borrow_mut().clear();
                                ui.state.select_index(*flat_idx);
                                *ui.active_group.borrow_mut() = None;
                                actions::refresh_action_sensitivity(&ui);
                                ui.sync_blend_controls();
                            }
                            ui.mask_view.borrow_mut().clone_from(&new_view);
                            canvas.borrow_mut().set_mask_view(new_view);
                            area_w.queue_draw();
                            redraw.request();
                        }
                }
                HitZone::Chevron => {
                    if d.from_row < rows.len()
                        && let RowKind::Group { id, .. } = &rows[d.from_row].kind {
                            let id = id.clone();
                            toggle_group_expanded(&mut ui.tree.borrow_mut(), &id);
                            commit_groups_quiet(&ui.tree.borrow(), &mut canvas.borrow_mut());
                            sync_height(&area_w, &ui);
                            area_w.queue_draw();
                        }
                }
                HitZone::Handle => {
                    if d.from_row != d.current_row {
                        // A reorder mutates the layer stack; commit any in-progress
                        // transform first so it can't write onto a shifted index.
                        prepare_reorder();
                        let dragged_row = rows.get(d.from_row);
                        let dragged_id = if let Some(r) = dragged_row { match &r.kind {
                            RowKind::Layer { id, .. } | RowKind::Group { id, .. } => id.clone(),
                        } } else {
                            area_w.set_cursor(None);
                            area_w.queue_draw();
                            return;
                        };

                        // Canvas id order before the reorder. Works for both single
                        // layers and groups (which move several layers at once).
                        let before_order: Vec<String> = canvas.borrow().layers()
                            .snapshot().iter().map(|l| l.id.clone()).collect();

                        let dragged_node = take_node(&mut ui.tree.borrow_mut(), &dragged_id);
                        if let Some(node) = dragged_node {
                            let snap2 = ui.state.snapshot();
                            let rows2 = compute_visible_rows(&ui.tree.borrow(), &snap2);

                            let (ins_parent, ins_idx) = resolve_insert_target(&rows2, d.current_row);

                            if ins_idx == usize::MAX {
                                match &ins_parent {
                                    None => ui.tree.borrow_mut().insert(0, node),
                                    Some(pid) => {
                                        insert_at_in_group(&mut ui.tree.borrow_mut(), node, pid, 0);
                                    }
                                }
                            } else {
                                insert_after(&mut ui.tree.borrow_mut(), node, ins_parent.as_ref(), ins_idx);
                            }

                            sync_canvas_order(
                                &ui.tree.borrow().clone(),
                                &mut canvas.borrow_mut(),
                            );
                            commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());

                            let after_order: Vec<String> = canvas.borrow().layers()
                                .snapshot().iter().map(|l| l.id.clone()).collect();
                            record_reorder(&history, &before_order, &after_order);

                            sync_height(&area_w, &ui);
                            redraw.request();
                        }
                    }
                    area_w.set_cursor(None);
                    area_w.queue_draw();
                }
                HitZone::Swatch if is_click => {
                    if d.from_row < rows.len()
                        && let RowKind::Layer { flat_idx, .. } = &rows[d.from_row].kind {
                            select_layer_content(*flat_idx);
                        }
                }
                HitZone::Body | HitZone::Swatch => {
                    if is_click && d.from_row < rows.len() {
                        let row = &rows[d.from_row];
                        let row_id = match &row.kind {
                            RowKind::Layer { id, .. } | RowKind::Group { id, .. } => id.clone(),
                        };
                        if shift {
                            // Range from current primary anchor to the clicked row, inclusive.
                            let anchor_id = ui
                                .active_id()
                                .or_else(|| ui.active_group.borrow().clone());
                            let anchor_row = anchor_id.and_then(|aid| {
                                rows.iter().position(|r| matches!(&r.kind,
                                    RowKind::Layer { id, .. } | RowKind::Group { id, .. } if id == &aid))
                            });
                            let mut ms = ui.multi_selected.borrow_mut();
                            ms.clear();
                            let (lo, hi) = match anchor_row {
                                Some(a) if a <= d.from_row => (a, d.from_row),
                                Some(a) => (d.from_row, a),
                                None => (d.from_row, d.from_row),
                            };
                            for r in &rows[lo..=hi] {
                                let rid = match &r.kind {
                                    RowKind::Layer { id, .. }
                                    | RowKind::Group { id, .. } => id.clone(),
                                };
                                ms.insert(rid);
                            }
                        } else if ctrl {
                            // Promote the current primary into the set so adding to it accumulates.
                            let mut ms = ui.multi_selected.borrow_mut();
                            if let Some(aid) = ui.active_id() {
                                ms.insert(aid);
                            }
                            if let Some(gid) = ui.active_group.borrow().clone() {
                                ms.insert(gid);
                            }
                            if ms.contains(&row_id) {
                                ms.remove(&row_id);
                            } else {
                                ms.insert(row_id.clone());
                            }
                            drop(ms);
                            if let RowKind::Layer { flat_idx, .. } = &row.kind {
                                ui.state.select_index(*flat_idx);
                                *ui.active_group.borrow_mut() = None;
                            } else if let RowKind::Group { id, .. } = &row.kind {
                                *ui.active_group.borrow_mut() = Some(id.clone());
                                ui.state.set_active(None);
                            }
                        } else {
                            ui.multi_selected.borrow_mut().clear();
                            if let RowKind::Layer { flat_idx, .. } = &row.kind {
                                ui.state.select_index(*flat_idx);
                                *ui.active_group.borrow_mut() = None;
                            } else if let RowKind::Group { id, .. } = &row.kind {
                                *ui.active_group.borrow_mut() = Some(id.clone());
                                ui.state.set_active(None);
                            }
                        }
                        actions::refresh_action_sensitivity(&ui);
                        ui.sync_blend_controls();
                        area_w.queue_draw();
                        // Re-present in case the selection change affects the
                        // canvas. Cheap when nothing changed: present() short-circuits.
                        redraw.request();
                    }
                }
            }
        });
    }

    area.add_controller(drag_gesture);

    // Double-click to rename.
    let rename_click = gtk::GestureClick::new();
    rename_click.set_button(gdk::BUTTON_PRIMARY);
    {
        let area_w = area.clone();
        let ui_c = ui.clone();
        let canvas_c = Rc::clone(&canvas);
        let history = Rc::clone(history);
        let on_edit_component = Rc::clone(on_edit_component);
        rename_click.connect_pressed(move |gesture, n_press, _x, y| {
            if n_press != 2 {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let snapshot = ui_c.state.snapshot();
            let rows = compute_visible_rows(&ui_c.tree.borrow(), &snapshot);
            let Some(row_idx) = item_at(y, rows.len()) else { return };
            let row = &rows[row_idx];
            // Double-clicking a component layer opens the component for editing
            // instead of renaming the layer.
            if let RowKind::Layer { flat_idx, .. } = &row.kind
                && let Some(LayerKind::Component(inst)) = ui_c.state.kind(*flat_idx)
            {
                on_edit_component(inst.component_id);
                return;
            }
            let (row_id, current_name, is_layer) = match &row.kind {
                RowKind::Layer { id, name, .. } => (id.clone(), name.clone(), true),
                RowKind::Group { id, name, .. } => (id.clone(), name.clone(), false),
            };
            show_rename_popover(
                &area_w,
                row_id,
                current_name,
                is_layer,
                &ui_c,
                &canvas_c,
                slot_top(row_idx),
                &history,
            );
        });
    }
    area.add_controller(rename_click);

    let motion = gtk::EventControllerMotion::new();
    {
        let area_w = area.clone();
        let ui = ui.clone();
        motion.connect_motion(move |_, x, y| {
            if ui.drag.borrow().as_ref().is_some_and(|d| d.zone == HitZone::Handle) {
                return;
            }
            #[allow(deprecated)]
            let width = f64::from(area_w.allocated_width());
            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
            let hit = item_at(y, rows.len()).map(|row_idx| {
                let row = &rows[row_idx];
                let y_in_row = y - slot_top(row_idx);
                let is_group = matches!(row.kind, RowKind::Group { .. });
                let has_edit = row_is_adjustment(&ui, row);
                let zone = hit_zone(x, y_in_row, width, row.depth + row.adjust_indent, is_group, has_edit);
                (row_idx, zone)
            });
            // The handle previews the drag (row-resize); the other clickable
            // zones use the pointer hand; everything else keeps the default.
            let cursor_name = hit.and_then(|(_, zone)| match zone {
                HitZone::Handle => Some("row-resize"),
                HitZone::Eye | HitZone::Edit | HitZone::Mask | HitZone::Chevron | HitZone::Swatch => {
                    Some("pointer")
                }
                HitZone::Body => None,
            });
            // Only the interactive (non-drag) icons take a hover highlight.
            let hover = hit.filter(|(_, zone)| {
                matches!(zone, HitZone::Eye | HitZone::Edit | HitZone::Mask)
            });
            if *ui.hover.borrow() != hover {
                *ui.hover.borrow_mut() = hover;
                area_w.queue_draw();
            }
            let c = cursor_name.and_then(|name| gtk::gdk::Cursor::from_name(name, None));
            area_w.set_cursor(c.as_ref());
        });
    }
    {
        let area_w = area.clone();
        let ui = ui.clone();
        motion.connect_leave(move |_| {
            if ui.hover.borrow().is_some() {
                *ui.hover.borrow_mut() = None;
                area_w.queue_draw();
            }
            area_w.set_cursor(None);
        });
    }
    area.add_controller(motion);
}

// Toggling a group's eye must not overwrite per-leaf state. We record which
// leaves we hid and restore exactly those on toggle-on; anything the user
// flipped manually while the group was hidden stays as they left it.
fn toggle_group_visibility(ui: &Ui, canvas: &Rc<RefCell<Canvas>>, group_id: &str, new_vis: bool) {
    let leaves = group_leaf_ids(&ui.tree.borrow(), group_id);
    let mut c = canvas.borrow_mut();
    let snap = c.layers().snapshot();
    let leaf_indices: Vec<(String, usize)> = leaves
        .iter()
        .filter_map(|lid| snap.iter().position(|l| &l.id == lid).map(|i| (lid.clone(), i)))
        .collect();

    let prior_mask = {
        let mut tree = ui.tree.borrow_mut();
        match find_group_mut(&mut tree, group_id) {
            Some(g) => std::mem::take(&mut g.masked_leaves),
            None => return,
        }
    };

    let new_mask: HashSet<String> = if new_vis {
        for (lid, idx) in &leaf_indices {
            if prior_mask.contains(lid)
                && let Err(e) = c.set_layer_visible(*idx, true) {
                    tracing::error!(error = %e, leaf = %lid, "group eye: show failed");
                }
        }
        HashSet::new()
    } else {
        let mut mask = HashSet::new();
        for (lid, idx) in leaf_indices {
            if snap.get(idx).is_some_and(|l| l.visible) {
                if let Err(e) = c.set_layer_visible(idx, false) {
                    tracing::error!(error = %e, leaf = %lid, "group eye: hide failed");
                    continue;
                }
                mask.insert(lid);
            }
        }
        mask
    };
    drop(c);

    if let Some(g) = find_group_mut(&mut ui.tree.borrow_mut(), group_id) {
        g.visible = new_vis;
        g.masked_leaves = new_mask;
    }
}

fn show_rename_popover(
    area: &gtk::DrawingArea,
    id: String,
    current_name: String,
    is_layer: bool,
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    row_top: f64,
    history: &Rc<RefCell<HistoryStack>>,
) {
    let entry = gtk::Entry::builder()
        .text(current_name.as_str())
        .width_chars(20)
        .build();
    entry.select_region(0, -1);

    let popover = gtk::Popover::new();
    popover.set_child(Some(&entry));
    popover.set_parent(area);
    #[allow(clippy::cast_possible_truncation)]
    let rect = gdk::Rectangle::new(8, row_top as i32, 200, ITEM_HEIGHT as i32);
    popover.set_pointing_to(Some(&rect));
    popover.set_has_arrow(false);

    let popover_rc = Rc::new(popover);

    let ui = ui.clone();
    let area = area.clone();
    let canvas = Rc::clone(canvas);
    let pop_c = Rc::clone(&popover_rc);
    let history = Rc::clone(history);
    entry.connect_activate(move |e| {
        let new_name = e.text().to_string();
        if new_name.trim().is_empty() {
            pop_c.popdown();
            return;
        }
        if is_layer {
            let snap = ui.state.snapshot();
            if let Some((idx, _)) = snap.iter().enumerate().find(|(_, l)| l.id == id) {
                ui.state.rename(idx, new_name.trim());
                history.borrow_mut().record(HistoryAction::LayerRename {
                    id: id.clone(),
                    old_name: current_name.clone(),
                    new_name: new_name.trim().to_string(),
                });
            }
        } else {
            rename_group_in_tree(&mut ui.tree.borrow_mut(), &id, new_name.trim().to_string());
            // Folder name is metadata-only (no composite change) but must persist.
            commit_groups_quiet(&ui.tree.borrow(), &mut canvas.borrow_mut());
        }
        area.queue_draw();
        pop_c.popdown();
    });

    // Dismissal is handled by the popover's autohide (click-away / Escape). A
    // focus-leave handler dismisses too eagerly: when opened via F2 or the
    // context menu the panel isn't focused, so the entry's focus bounces during
    // mapping and would instantly close the popover.

    // Defer the popup so it survives a context menu closing in the same tick:
    // popping up a second autohide popover synchronously gets it dismissed.
    glib::idle_add_local_once(move || {
        popover_rc.popup();
        entry.grab_focus();
    });
}

/// Open the rename popover on the currently active layer or group. Used by the
/// F2 shortcut and the context-menu "Rename" entries.
pub(super) fn begin_rename_active(
    area: &gtk::DrawingArea,
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    history: &Rc<RefCell<HistoryStack>>,
) {
    // Unmapped means the layers panel is hidden (e.g. the Crop tool swapped in
    // its own sidebar): there's no row to anchor a rename popover to.
    if !area.is_mapped() {
        return;
    }
    let active_group = ui.active_group.borrow().clone();
    let (target_id, is_layer) = if let Some(gid) = active_group {
        (gid, false)
    } else if let Some(lid) = ui.active_id() {
        (lid, true)
    } else {
        return;
    };

    let snapshot = ui.state.snapshot();
    let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
    let Some(row_idx) = rows.iter().position(|r| match &r.kind {
        RowKind::Layer { id, .. } | RowKind::Group { id, .. } => id == &target_id,
    }) else {
        return;
    };
    let current_name = match &rows[row_idx].kind {
        RowKind::Layer { name, .. } | RowKind::Group { name, .. } => name.clone(),
    };
    show_rename_popover(area, target_id, current_name, is_layer, ui, canvas, slot_top(row_idx), history);
}

pub(super) fn item_at(y: f64, count: usize) -> Option<usize> {
    if count == 0 || y < LIST_PADDING {
        return None;
    }
    let rel = y - LIST_PADDING;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let slot = (rel / SLOT_HEIGHT).floor() as usize;
    if slot >= count {
        return None;
    }
    let within = count_f64(slot).mul_add(-SLOT_HEIGHT, rel);
    if within > ITEM_HEIGHT {
        return None;
    }
    Some(slot)
}

fn insertion_index(top_y: f64, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let rel = (top_y - LIST_PADDING) / SLOT_HEIGHT;
    let max = count_f64(count - 1);
    let clamped = rel.round().clamp(0.0, max);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    { clamped as usize }
}

// Picks the parent/index where a dragged node should land, given the row
// immediately above the drop point. Returns `idx == usize::MAX` to mean
// "insert as first child" (used for prepend-to-root or drop-into-empty-group).
fn resolve_insert_target(rows2: &[VisibleRow], current_row: usize) -> (Option<String>, usize) {
    if current_row == 0 || rows2.is_empty() {
        return (None, usize::MAX);
    }
    let above_idx = (current_row - 1).min(rows2.len() - 1);
    let Some(above) = rows2.get(above_idx) else { return (None, rows2.len()) };

    // Drop right under an expanded group header lands as that group's first child.
    if let RowKind::Group { id, expanded: true, .. } = &above.kind {
        return (Some(id.clone()), usize::MAX);
    }

    // Last child of a group: escape up so the drop becomes a sibling of the group.
    if above.parent_id.is_some() {
        let is_last_child = rows2
            .get(above_idx + 1)
            .is_none_or(|next| next.depth < above.depth);
        if is_last_child {
            let gid = above.parent_id.as_deref().expect("parent_id set when above.parent_id.is_some()");
            if let Some(grp) = rows2
                .iter()
                .find(|r| matches!(&r.kind, RowKind::Group { id, .. } if id == gid))
            {
                return (grp.parent_id.clone(), grp.idx_in_parent);
            }
        }
    }

    // Case 3: sibling after the above row.
    (above.parent_id.clone(), above.idx_in_parent)
}

// --- Tests ---
#[cfg(test)]
mod tests {
    use super::*;

    fn layer_row(parent_id: Option<&str>, idx_in_parent: usize, depth: usize) -> VisibleRow {
        VisibleRow {
            kind: RowKind::Layer {
                id: "l".into(),
                name: "Layer".into(),
                visible: true,
                flat_idx: 0,
            },
            depth,
            adjust_indent: 0,
            parent_id: parent_id.map(str::to_owned),
            idx_in_parent,
        }
    }

    fn group_row(
        id: &str,
        expanded: bool,
        parent_id: Option<&str>,
        idx_in_parent: usize,
        depth: usize,
    ) -> VisibleRow {
        VisibleRow {
            kind: RowKind::Group {
                id: id.into(),
                name: "Group".into(),
                visible: true,
                expanded,
            },
            depth,
            adjust_indent: 0,
            parent_id: parent_id.map(str::to_owned),
            idx_in_parent,
        }
    }

    // --- drag_span ---
    #[test]
    fn drag_span_layer_is_one() {
        let rows = vec![layer_row(None, 0, 0)];
        assert_eq!(drag_span(&rows, 0), 1);
    }

    #[test]
    fn drag_span_collapsed_group_is_one() {
        let rows = vec![group_row("g1", false, None, 0, 0)];
        assert_eq!(drag_span(&rows, 0), 1);
    }

    #[test]
    fn drag_span_expanded_group_includes_children() {
        let rows = vec![
            group_row("g1", true, None, 0, 0),
            layer_row(Some("g1"), 0, 1),
            layer_row(Some("g1"), 1, 1),
        ];
        assert_eq!(drag_span(&rows, 0), 3);
    }

    #[test]
    fn drag_span_nested_groups() {
        // outer(depth 0) > inner(depth 1) > leaf(depth 2); sibling at depth 0
        let rows = vec![
            group_row("outer", true, None, 0, 0),
            group_row("inner", true, Some("outer"), 0, 1),
            layer_row(Some("inner"), 0, 2),
            layer_row(None, 1, 0), // sibling after outer
        ];
        assert_eq!(drag_span(&rows, 0), 3); // outer header + inner header + leaf
        assert_eq!(drag_span(&rows, 1), 2); // inner header + leaf
        assert_eq!(drag_span(&rows, 3), 1); // plain sibling layer
    }

    // --- reorder_steps ---
    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn apply_steps(before: &[String], steps: &[(usize, usize)]) -> Vec<String> {
        let mut cur = before.to_vec();
        for &(from, to) in steps {
            let item = cur.remove(from);
            cur.insert(to, item);
        }
        cur
    }

    #[test]
    fn reorder_steps_no_change_is_empty() {
        let before = ids(&["a", "b", "c"]);
        assert!(reorder_steps(&before, &before).is_empty());
    }

    #[test]
    fn reorder_steps_single_move() {
        let before = ids(&["a", "b", "c"]);
        let after = ids(&["c", "a", "b"]);
        let steps = reorder_steps(&before, &after);
        assert_eq!(steps.len(), 1);
        assert_eq!(apply_steps(&before, &steps), after);
    }

    #[test]
    fn reorder_steps_group_move_reconstructs_target() {
        // Two layers moved together past a third - a group drag.
        let before = ids(&["a", "b", "c", "d"]);
        let after = ids(&["c", "a", "b", "d"]);
        let steps = reorder_steps(&before, &after);
        assert_eq!(apply_steps(&before, &steps), after);
    }

    // --- displaced_row ---
    #[test]
    fn displaced_row_no_movement() {
        for i in 0..5 {
            assert_eq!(displaced_row(i, 2, 1, 2), i);
        }
    }

    #[test]
    fn displaced_row_move_down() {
        // from=1, span=1, to=3 - rows [2,3] shift up by 1
        assert_eq!(displaced_row(0, 1, 1, 3), 0);
        assert_eq!(displaced_row(2, 1, 1, 3), 1);
        assert_eq!(displaced_row(3, 1, 1, 3), 2);
        assert_eq!(displaced_row(4, 1, 1, 3), 4);
    }

    #[test]
    fn displaced_row_move_up() {
        // from=3, span=1, to=1 - rows [1,2] shift down by 1
        assert_eq!(displaced_row(0, 3, 1, 1), 0);
        assert_eq!(displaced_row(1, 3, 1, 1), 2);
        assert_eq!(displaced_row(2, 3, 1, 1), 3);
        assert_eq!(displaced_row(4, 3, 1, 1), 4);
    }

    #[test]
    fn displaced_row_group_span_move_down() {
        // from=0, span=2 (group header + child), to=2
        // Non-dragged rows [2,4) all shift up by 2.
        assert_eq!(displaced_row(2, 0, 2, 2), 0);
        assert_eq!(displaced_row(3, 0, 2, 2), 1);
    }

    // --- resolve_insert_target ---
    #[test]
    fn resolve_prepend_at_row_zero() {
        let rows = vec![layer_row(None, 0, 0)];
        let (parent, idx) = resolve_insert_target(&rows, 0);
        assert_eq!(parent, None);
        assert_eq!(idx, usize::MAX);
    }

    #[test]
    fn resolve_after_plain_layer() {
        // One root layer; drop after it (current_row = 1).
        let rows = vec![layer_row(None, 0, 0)];
        let (parent, idx) = resolve_insert_target(&rows, 1);
        assert_eq!(parent, None);
        assert_eq!(idx, 0);
    }

    #[test]
    fn resolve_first_child_after_expanded_group_header() {
        // current_row = 1 -> "above" is the expanded group header.
        // Drop just below an expanded header -> first child of the group.
        let rows = vec![
            group_row("g1", true, None, 0, 0),
            layer_row(Some("g1"), 0, 1),
        ];
        let (parent, idx) = resolve_insert_target(&rows, 1);
        assert_eq!(parent, Some("g1".to_owned()));
        assert_eq!(idx, usize::MAX); // first-child sentinel
    }

    #[test]
    fn resolve_escape_group_when_above_is_last_child() {
        // rows: [Group(expanded, root), Child(depth1), Sibling(depth0)]
        // current_row = 2 -> "above" is Child, the last child (next row is depth 0).
        // Should escape to root, after the group.
        let rows = vec![
            group_row("g1", true, None, 0, 0),
            layer_row(Some("g1"), 0, 1),
            layer_row(None, 1, 0),
        ];
        let (parent, idx) = resolve_insert_target(&rows, 2);
        assert_eq!(parent, None);
        assert_eq!(idx, 0); // after the group (group is root[0])
    }

    #[test]
    fn resolve_inside_group_middle_child() {
        // current_row = 2 -> "above" is Child1, NOT the last child (Child2 follows).
        // Should insert as sibling after Child1 inside the group.
        let rows = vec![
            group_row("g1", true, None, 0, 0),
            layer_row(Some("g1"), 0, 1), // Child1
            layer_row(Some("g1"), 1, 1), // Child2 (same depth -> Child1 not last)
            layer_row(None, 1, 0),
        ];
        let (parent, idx) = resolve_insert_target(&rows, 2);
        assert_eq!(parent, Some("g1".to_owned()));
        assert_eq!(idx, 0);
    }
}
