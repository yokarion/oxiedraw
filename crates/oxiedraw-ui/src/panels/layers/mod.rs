//! Layers panel: the layer/group tree, its cairo-drawn row list and gestures.
//! The tree is the source of display order; canvas z-order is synced from it.

mod actions;
mod components;
mod thumbnail;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::f64::consts::TAU;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AOrdering};

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::document::{BlendMode, LayerGroup, LayerState, LayerTreeNode};
use oxiedraw_core::enum_meta::EnumMeta;
use oxiedraw_core::history::{HistoryAction, HistoryStack, LayerExtension};
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

const PANEL_MARGIN: i32 = 8;
const TAB_SPACING: i32 = 6;

const LIST_PADDING: f64 = 8.0;
const LIST_PADDING_RIGHT: f64 = 0.8;
const ITEM_HEIGHT: f64 = 44.0;
const ITEM_GAP: f64 = 4.0;
const ITEM_RADIUS: f64 = 6.0;
const ITEM_INNER_PAD: f64 = 8.0;
pub(super) const SWATCH_SIZE: f64 = 28.0;
const SWATCH_RADIUS: f64 = 3.0;
const HANDLE_WIDTH: f64 = 18.0;
const HANDLE_LINE_THICKNESS: f64 = 1.5;
const HANDLE_LINE_GAP: f64 = 4.0;

const NEST_PAD: f64 = 6.0;
const NEST_INSET: f64 = NEST_PAD;
const NEST_RADIUS: f64 = 10.0;

const SHADOW_STEPS: usize = 8;
const SHADOW_SPREAD: f64 = 9.0;
const SHADOW_OFFSET_Y: f64 = 2.0;
const SHADOW_STEP_ALPHA: f64 = 0.05;

const DROP_SETTLE_SECS: f64 = 0.16;

const CONNECTOR_INSET: f64 = 3.0;
const CONNECTOR_WIDTH: f64 = 1.5;
const CONNECTOR_R: f64 = 3.0;

const CLIP_GUTTER_W: f64 = 20.0;

const LOCK_RING_D: f64 = 13.0;
const LOCK_DISC_D: f64 = 10.5;

const EYE_RADIUS: f64 = 9.0;
const EDIT_RADIUS: f64 = 8.0;
const MASK_RADIUS: f64 = 8.0;
const EDIT_EYE_GAP: f64 = 6.0;
const CHEVRON_SIZE: f64 = 12.0;
const FOLDER_W: f64 = 16.0;
const FOLDER_H: f64 = 13.0;
const INDENT_STEP: f64 = 10.0;

const SLOT_HEIGHT: f64 = ITEM_HEIGHT + ITEM_GAP;

const AUTOSCROLL_EDGE: f64 = 56.0;
const AUTOSCROLL_MAX_SPEED: f64 = 700.0;

const SCROLL_THUMB_MIN: f64 = 24.0;

type Rgb = (f64, f64, f64);

const FALLBACK_WINDOW_BG: Rgb = (0.92, 0.92, 0.92);
const FALLBACK_ROW_BG: Rgb = (0.96, 0.96, 0.96);
const FALLBACK_ACCENT_BG: Rgb = (0.21, 0.52, 0.89);
const FALLBACK_ACCENT_FG: Rgb = (1.0, 1.0, 1.0);
const FALLBACK_FG: Rgb = (0.18, 0.18, 0.20);

static GROUP_COUNTER: AtomicU64 = AtomicU64::new(1);
fn new_group_id() -> String {
    format!("g{:016x}", GROUP_COUNTER.fetch_add(1, AOrdering::Relaxed))
}

fn observe_group_id(id: &str) {
    if let Some(hex) = id.strip_prefix('g')
        && let Ok(n) = u64::from_str_radix(hex, 16)
    {
        let target = n.saturating_add(1);
        let mut current = GROUP_COUNTER.load(AOrdering::Relaxed);
        while current < target {
            match GROUP_COUNTER.compare_exchange_weak(
                current,
                target,
                AOrdering::Relaxed,
                AOrdering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct GroupData {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) expanded: bool,
    pub(super) visible: bool,
    pub(super) children: Vec<LayerNode>,
    pub(super) masked_leaves: HashSet<String>,
}

#[derive(Debug, Clone)]
pub(super) enum LayerNode {
    Layer(String),
    Group(GroupData),
}

#[derive(Clone, Debug)]
pub(super) enum RowKind {
    Layer {
        id: String,
        name: String,
        visible: bool,
        flat_idx: usize,
        clipped: bool,
        alpha_locked: bool,
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
    pub(super) adjust_indent: usize,
    pub(super) parent_id: Option<String>,
    pub(super) idx_in_parent: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum HitZone {
    Handle,
    Eye,
    Edit,
    Mask,
    Clip,
    Chevron,
    Folder,
    Swatch,
    Body,
}

#[derive(Clone, Debug)]
pub(super) struct Drag {
    from_row: usize,
    current_row: usize,
    pointer_y: f64,
    grab_offset_y: f64,
    zone: HitZone,
    animated_depth_offset: f64,
    row_y_anim: Vec<f64>,
    last_frame_time_us: i64,
}

#[derive(Clone, Debug)]
pub(super) struct DropSettle {
    from_row: usize,
    span: usize,
    id: String,
    offset_y: f64,
    depth_offset: f64,
    row_y: Vec<f64>,
    progress: f64,
    last_frame_time_us: i64,
}

impl DropSettle {
    fn remaining(&self) -> f64 {
        let t = self.progress.clamp(0.0, 1.0);
        (1.0 - t).powi(3)
    }

    fn matches(&self, rows: &[VisibleRow]) -> bool {
        self.span > 0
            && self.row_y.len() == rows.len()
            && self.from_row + self.span <= rows.len()
            && rows.get(self.from_row).is_some_and(|r| row_id(r) == self.id)
    }
}

struct FloatBlock {
    from: usize,
    span: usize,
    top: f64,
    depth_offset: f64,
    lift: f64,
}

#[derive(Clone)]
pub(super) struct Ui {
    pub(super) state: LayerState,
    pub(super) tree: Rc<RefCell<Vec<LayerNode>>>,
    pub(super) drag: Rc<RefCell<Option<Drag>>>,
    pub(super) drop_settle: Rc<RefCell<Option<DropSettle>>>,
    pub(super) settle_ticking: Rc<Cell<bool>>,
    pub(super) thumbnails: Rc<RefCell<Vec<Option<cairo::ImageSurface>>>>,
    pub(super) multi_selected: Rc<RefCell<HashSet<String>>>,
    pub(super) active_group: Rc<RefCell<Option<String>>>,
    pub(super) blend_sync: Rc<RefCell<Option<Rc<dyn Fn()>>>>,
    pub(super) mask_view: Rc<RefCell<Option<String>>>,
    pub(super) hover: Rc<RefCell<Option<(usize, HitZone)>>>,
    pub(super) vadj: gtk::Adjustment,
    synced_selection_sig: Rc<Cell<Option<u64>>>,
}

impl Ui {
    fn new(state: LayerState) -> Self {
        let snapshot = state.snapshot();
        let tree: Vec<LayerNode> = snapshot
            .iter()
            .rev()
            .map(|l| LayerNode::Layer(l.id.clone()))
            .collect();
        Self {
            state,
            tree: Rc::new(RefCell::new(tree)),
            drag: Rc::new(RefCell::new(None)),
            drop_settle: Rc::new(RefCell::new(None)),
            settle_ticking: Rc::new(Cell::new(false)),
            thumbnails: Rc::new(RefCell::new(Vec::new())),
            multi_selected: Rc::new(RefCell::new(HashSet::new())),
            active_group: Rc::new(RefCell::new(None)),
            blend_sync: Rc::new(RefCell::new(None)),
            mask_view: Rc::new(RefCell::new(None)),
            hover: Rc::new(RefCell::new(None)),
            vadj: gtk::Adjustment::new(0.0, 0.0, 0.0, SLOT_HEIGHT, 0.0, 0.0),
            synced_selection_sig: Rc::new(Cell::new(None)),
        }
    }

    fn selection_sig(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.state.active().hash(&mut h);
        self.active_group.borrow().hash(&mut h);
        let mut acc = h.finish();
        let ms = self.multi_selected.borrow();
        for id in ms.iter() {
            let mut ih = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut ih);
            acc ^= ih.finish();
        }
        acc.wrapping_add(ms.len() as u64)
    }

    fn scroll_offset(&self) -> f64 {
        self.vadj.value()
    }

    fn sync_blend_controls(&self) {
        self.sync_selection_to_state_forced();
        let cb = self.blend_sync.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

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

    pub(super) fn selected_leaf_ids_all(&self) -> Vec<String> {
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
        leaf_ids_top_first(&tree)
            .into_iter()
            .filter(|id| wanted.contains(id))
            .collect()
    }

    pub(super) fn sync_selection_to_state(&self) {
        let sig = self.selection_sig();
        if self.synced_selection_sig.get() == Some(sig) {
            return;
        }
        self.state.set_selected_leaves(self.selected_leaf_ids_all());
        self.synced_selection_sig.set(Some(sig));
    }

    pub(super) fn sync_selection_to_state_forced(&self) {
        self.state.set_selected_leaves(self.selected_leaf_ids_all());
        self.synced_selection_sig.set(Some(self.selection_sig()));
    }

    pub(super) fn invalidate_selection_sync(&self) {
        self.synced_selection_sig.set(None);
    }

    pub(super) fn selected_nodes_for_grouping(&self) -> Vec<String> {
        let tree = self.tree.borrow();
        let mut wanted: HashSet<String> = self.multi_selected.borrow().clone();
        if let Some(id) = self.active_id() {
            wanted.insert(id);
        }
        if let Some(gid) = self.active_group.borrow().as_ref() {
            wanted.insert(gid.clone());
        }
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut ordered = Vec::new();
        collect_top_selected(&tree, &wanted, &mut ordered);
        ordered
    }
}

fn collect_top_selected(nodes: &[LayerNode], wanted: &HashSet<String>, out: &mut Vec<String>) {
    for n in nodes {
        match n {
            LayerNode::Layer(id) => {
                if wanted.contains(id) {
                    out.push(id.clone());
                }
            }
            LayerNode::Group(g) => {
                if wanted.contains(&g.id) {
                    out.push(g.id.clone());
                } else {
                    collect_top_selected(&g.children, wanted, out);
                }
            }
        }
    }
}

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
                            clipped: layer.clipped,
                            alpha_locked: layer.alpha_lock_active(),
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

fn tree_depth(row: &VisibleRow) -> usize {
    row.depth + row.adjust_indent
}

fn row_id(row: &VisibleRow) -> &str {
    match &row.kind {
        RowKind::Layer { id, .. } | RowKind::Group { id, .. } => id.as_str(),
    }
}

fn contained_rows(rows: &[VisibleRow], i: usize) -> usize {
    let depth = tree_depth(&rows[i]);
    rows.iter()
        .skip(i + 1)
        .take_while(|r| tree_depth(r) > depth)
        .count()
}

fn opens_box(rows: &[VisibleRow], i: usize) -> bool {
    matches!(rows[i].kind, RowKind::Group { .. }) || contained_rows(rows, i) > 0
}

pub(super) fn box_depth(rows: &[VisibleRow], i: usize) -> usize {
    tree_depth(&rows[i]) + usize::from(opens_box(rows, i))
}

fn nest_left(level: usize) -> f64 {
    nest_left_f(count_f64(level))
}

fn nest_left_f(level: f64) -> f64 {
    LIST_PADDING + level * NEST_INSET
}

fn container_bottom(rows: &[VisibleRow], layout: &RowLayout, i: usize) -> f64 {
    let last = i + contained_rows(rows, i);
    let depth = tree_depth(&rows[i]);
    let inside = (0..rows.len())
        .filter(|&j| j != i && opens_box(rows, j))
        .filter(|&j| j + contained_rows(rows, j) == last && tree_depth(&rows[j]) > depth)
        .count();
    layout.bottom(last) + NEST_PAD * count_f64(inside + 1)
}

fn nest_right(width: f64) -> f64 {
    (width - LIST_PADDING_RIGHT).max(LIST_PADDING)
}

#[derive(Clone, Copy, Default)]
pub(super) struct ClipInfo {
    pub(super) clipped: bool,
    pub(super) clipped_above: bool,
    pub(super) clipped_below: bool,
    pub(super) has_base: bool,
    pub(super) base_hidden: bool,
    pub(super) is_base: bool,
}

fn same_sibling_list(a: &VisibleRow, b: &VisibleRow) -> bool {
    a.depth == b.depth && a.parent_id == b.parent_id
}

fn row_clipped(row: &VisibleRow) -> bool {
    matches!(row.kind, RowKind::Layer { clipped: true, .. })
}

pub(super) fn clip_info(rows: &[VisibleRow], i: usize) -> ClipInfo {
    let row = &rows[i];
    if !matches!(row.kind, RowKind::Layer { .. }) {
        return ClipInfo::default();
    }
    let clipped = row_clipped(row);

    let clipped_above = i
        .checked_sub(1)
        .is_some_and(|a| same_sibling_list(row, &rows[a]) && row_clipped(&rows[a]));

    if !clipped {
        return ClipInfo {
            is_base: clipped_above,
            ..ClipInfo::default()
        };
    }
    let clipped_below = rows
        .get(i + 1)
        .is_some_and(|b| same_sibling_list(row, b) && row_clipped(b));

    let mut has_base = false;
    let mut base_hidden = false;
    for below in rows.iter().skip(i + 1) {
        if !same_sibling_list(row, below) {
            break;
        }
        if row_clipped(below) {
            continue;
        }
        if let RowKind::Layer { visible, .. } = below.kind {
            has_base = true;
            base_hidden = !visible;
        }
        break;
    }

    ClipInfo {
        clipped: true,
        clipped_above,
        clipped_below,
        has_base,
        base_hidden,
        is_base: false,
    }
}

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

pub(super) fn sync_tree_order_from_canvas(tree: &mut [LayerNode], canvas: &Canvas) {
    let snap = canvas.layers().snapshot();
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

fn record_reorder(
    history: &Rc<RefCell<HistoryStack>>,
    before: &[String],
    after: &[String],
    tree_before: Vec<LayerTreeNode>,
    tree_after: Vec<LayerTreeNode>,
) {
    let mut actions: Vec<HistoryAction> = reorder_steps(before, after)
        .into_iter()
        .map(|(from, to)| HistoryAction::LayerReorder { from, to })
        .collect();
    if tree_before != tree_after {
        actions.push(HistoryAction::LayerTreeEdit {
            before: tree_before,
            after: tree_after,
        });
    }
    let action = match actions.len() {
        0 => None,
        1 => actions.into_iter().next(),
        _ => Some(HistoryAction::Batch {
            label: "Move layers".to_string(),
            actions,
        }),
    };
    if let Some(action) = action {
        history.borrow_mut().record(action);
    }
}

fn format_tree_inline(nodes: &[LayerNode], names: &HashMap<&str, &str>) -> String {
    nodes
        .iter()
        .map(|n| match n {
            LayerNode::Layer(id) => names
                .get(id.as_str())
                .map_or_else(|| id.clone(), |name| (*name).to_string()),
            LayerNode::Group(g) => {
                format!("{}[{}]", g.name, format_tree_inline(&g.children, names))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn log_layer_move(
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    dragged_group: bool,
    dragged_name: &str,
    before: &[String],
    after: &[String],
) {
    let snapshot = canvas.borrow().layers().snapshot();
    let names: HashMap<&str, &str> = snapshot
        .iter()
        .map(|l| (l.id.as_str(), l.name.as_str()))
        .collect();
    let tree = format_tree_inline(&ui.tree.borrow(), &names);
    let moved = reorder_steps(before, after).len();
    if dragged_group {
        tracing::info!(
            target: "oxiedraw::layers",
            group = dragged_name,
            layers_moved = moved,
            tree = %tree,
            "group moved"
        );
    } else if moved > 1 {
        tracing::info!(
            target: "oxiedraw::layers",
            layer = dragged_name,
            layers_moved = moved,
            tree = %tree,
            "layers moved"
        );
    } else {
        tracing::info!(
            target: "oxiedraw::layers",
            layer = dragged_name,
            tree = %tree,
            "layer moved"
        );
    }
}

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

pub(super) fn sync_canvas_order(tree: &[LayerNode], canvas: &mut Canvas) {
    let mut current: Vec<String> =
        canvas.layers().snapshot().iter().map(|l| l.id.clone()).collect();
    let on_canvas: HashSet<&String> = current.iter().collect();
    let mut seen: HashSet<String> = HashSet::new();
    let top_first: Vec<String> = leaf_ids_top_first(tree)
        .into_iter()
        .filter(|id| on_canvas.contains(id) && seen.insert(id.clone()))
        .collect();
    let n = top_first.len();
    if n == 0 {
        return;
    }
    for desired_pos in 0..n {
        let target_id = &top_first[n - 1 - desired_pos];
        let Some(cur_pos) = current.iter().position(|id| id == target_id) else { continue };
        if cur_pos != desired_pos && canvas.reorder_layer(cur_pos, desired_pos).is_ok() {
            let item = current.remove(cur_pos);
            current.insert(desired_pos, item);
        }
    }
}

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

pub(super) fn tree_to_core(nodes: &[LayerNode]) -> Vec<LayerTreeNode> {
    nodes.iter().rev().map(node_to_core).collect()
}

pub(super) fn record_tree_edit(
    history: &Rc<RefCell<HistoryStack>>,
    before: Vec<LayerTreeNode>,
    after: Vec<LayerTreeNode>,
    label: &str,
) {
    if before == after {
        return;
    }
    history.borrow_mut().record(HistoryAction::Batch {
        label: label.to_string(),
        actions: vec![HistoryAction::LayerTreeEdit { before, after }],
    });
}

fn collect_group_masks(nodes: &[LayerNode], out: &mut std::collections::HashMap<String, HashSet<String>>) {
    for n in nodes {
        if let LayerNode::Group(g) = n {
            out.insert(g.id.clone(), g.masked_leaves.clone());
            collect_group_masks(&g.children, out);
        }
    }
}

fn overlay_group_masks(nodes: &mut [LayerNode], masks: &std::collections::HashMap<String, HashSet<String>>) {
    for n in nodes {
        if let LayerNode::Group(g) = n {
            if let Some(masked) = masks.get(&g.id) {
                g.masked_leaves = masked.clone();
            }
            overlay_group_masks(&mut g.children, masks);
        }
    }
}

fn derive_group_visibility(nodes: &mut [LayerNode], visible: &HashSet<String>) -> bool {
    let mut any = false;
    for n in nodes.iter_mut() {
        match n {
            LayerNode::Layer(id) => {
                if visible.contains(id) {
                    any = true;
                }
            }
            LayerNode::Group(g) => {
                let child_any = derive_group_visibility(&mut g.children, visible);
                g.visible = child_any;
                any |= child_any;
            }
        }
    }
    any
}

fn node_from_core(node: &LayerTreeNode) -> LayerNode {
    match node {
        LayerTreeNode::Layer { id } => LayerNode::Layer(id.clone()),
        LayerTreeNode::Group(g) => {
            observe_group_id(&g.id);
            LayerNode::Group(GroupData {
                id: g.id.clone(),
                name: g.name.clone(),
                expanded: g.expanded,
                visible: true,
                children: tree_from_core(&g.children),
                masked_leaves: HashSet::new(),
            })
        }
    }
}

pub(super) fn tree_from_core(nodes: &[LayerTreeNode]) -> Vec<LayerNode> {
    let mut tree: Vec<LayerNode> = nodes.iter().rev().map(node_from_core).collect();
    dedup_group_ids(&mut tree, &mut HashSet::new());
    tree
}

fn dedup_group_ids(nodes: &mut [LayerNode], seen: &mut HashSet<String>) {
    for node in nodes {
        if let LayerNode::Group(g) = node {
            if !seen.insert(g.id.clone()) {
                g.id = new_group_id();
                seen.insert(g.id.clone());
            }
            dedup_group_ids(&mut g.children, seen);
        }
    }
}

pub(super) fn commit_groups(tree: &[LayerNode], canvas: &mut Canvas) {
    if let Err(e) = canvas.set_layer_tree(tree_to_core(tree)) {
        tracing::error!(error = %e, "failed to push folder tree to canvas");
    }
}

pub(super) fn commit_groups_quiet(tree: &[LayerNode], canvas: &mut Canvas) {
    canvas.set_layer_tree_quiet(tree_to_core(tree));
}

#[derive(Debug, Clone, Copy)]
pub(super) enum NewLayerKind {
    Raster,
    Adjustment,
}

enum InsertAnchor {
    Top,
    Above { parent_id: Option<String>, idx: usize },
}

fn capture_insert_anchor(ui: &Ui) -> InsertAnchor {
    let tree = ui.tree.borrow();
    if let Some(gid) = ui.active_group.borrow().as_ref()
        && let Some((parent_id, idx)) =
            find_first_insertion_position(&tree, std::slice::from_ref(gid))
    {
        return InsertAnchor::Above { parent_id, idx };
    }
    if let Some(id) = ui.active_id()
        && let Some((parent_id, idx)) =
            find_first_insertion_position(&tree, std::slice::from_ref(&id))
    {
        return InsertAnchor::Above { parent_id, idx };
    }
    InsertAnchor::Top
}

fn insert_node_at_anchor(tree: &mut Vec<LayerNode>, node: LayerNode, anchor: &InsertAnchor) {
    match anchor {
        InsertAnchor::Top => tree.insert(0, node),
        InsertAnchor::Above { parent_id: None, idx } => {
            let pos = (*idx).min(tree.len());
            tree.insert(pos, node);
        }
        InsertAnchor::Above { parent_id: Some(pid), idx } => {
            insert_at_in_group(tree, node, pid, *idx);
        }
    }
}

fn capture_layer_add(canvas: &Rc<RefCell<Canvas>>, idx: usize) -> Option<HistoryAction> {
    let mut c = canvas.borrow_mut();
    let layer = c.layers().snapshot().get(idx)?.clone();
    let (blend, opacity) = c.layers().blend(idx).unwrap_or_default();
    let pixels = c.read_layer(idx).ok()?;
    Some(HistoryAction::LayerAdd {
        idx,
        id: layer.id,
        name: layer.name,
        visible: layer.visible,
        layer_kind: layer.kind,
        blend,
        opacity,
        pixels,
    })
}

pub(super) fn create_layer_at_selection(
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    history: &Rc<RefCell<HistoryStack>>,
    kind: NewLayerKind,
) -> Result<usize, oxiedraw_core::renderer::RendererError> {
    let anchor = capture_insert_anchor(ui);

    let name = match kind {
        NewLayerKind::Raster => format!("Layer {}", ui.state.len() + 1),
        NewLayerKind::Adjustment => "Adjustment".to_string(),
    };
    let name_for_log = name.clone();
    let top_idx = {
        let mut c = canvas.borrow_mut();
        match kind {
            NewLayerKind::Raster => c.add_layer(name)?,
            NewLayerKind::Adjustment => c.add_adjustment_layer(name)?,
        }
    };
    let new_id = canvas
        .borrow()
        .layers()
        .snapshot()
        .get(top_idx)
        .map(|l| l.id.clone())
        .unwrap_or_default();

    insert_node_at_anchor(
        &mut ui.tree.borrow_mut(),
        LayerNode::Layer(new_id.clone()),
        &anchor,
    );
    *ui.active_group.borrow_mut() = None;

    sync_canvas_order(&ui.tree.borrow().clone(), &mut canvas.borrow_mut());
    commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());

    let final_idx = canvas
        .borrow()
        .layers()
        .snapshot()
        .iter()
        .position(|l| l.id == new_id)
        .unwrap_or(top_idx);
    if let Some(action) = capture_layer_add(canvas, final_idx) {
        history.borrow_mut().record(action);
    }
    ui.state.set_active(Some(final_idx));

    tracing::info!(
        target: "oxiedraw::layers",
        name = %name_for_log,
        kind = ?kind,
        idx = final_idx,
        total = ui.state.len(),
        "layer created"
    );
    Ok(final_idx)
}

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

pub(super) fn mirror_tree(
    nodes: &[LayerNode],
    id_map: &std::collections::HashMap<String, String>,
) -> Vec<LayerNode> {
    nodes
        .iter()
        .filter_map(|n| match n {
            LayerNode::Layer(id) => id_map.get(id).map(|new_id| LayerNode::Layer(new_id.clone())),
            LayerNode::Group(g) => Some(LayerNode::Group(GroupData {
                id: new_group_id(),
                name: g.name.clone(),
                expanded: g.expanded,
                visible: g.visible,
                children: mirror_tree(&g.children, id_map),
                masked_leaves: HashSet::new(),
            })),
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

pub(crate) fn build(
    layers: &LayerState,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    layer_clipboard: &Rc<RefCell<Option<LayerClipboard>>>,
    toaster: &Toaster,
    select_layer_content: &Rc<dyn Fn(usize)>,
    select_folder_content: &Rc<dyn Fn(Vec<usize>)>,
    history: &Rc<RefCell<HistoryStack>>,
    layer_extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
    components: &Rc<RefCell<oxiedraw_core::components::ComponentLibrary>>,
    on_edit_component: &Rc<dyn Fn(String)>,
    component_exit: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
    prepare_delete: &Rc<dyn Fn() -> bool>,
    prepare_reorder: &Rc<dyn Fn()>,
    alpha_lock_observer: &Rc<RefCell<Option<Rc<dyn Fn(bool)>>>>,
) -> (
    gtk::Box,
    Rc<dyn Fn()>,
    Rc<dyn Fn() -> Vec<String>>,
    Rc<dyn Fn()>,
    Rc<dyn Fn()>,
    Rc<dyn Fn(Option<String>)>,
    Rc<dyn Fn()>,
    Rc<dyn Fn() -> Option<usize>>,
) {
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .vexpand(true)
        .hexpand(true)
        .build();
    panel.add_css_class("oxiedraw-chrome");

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

    let (
        layers_page,
        refresh_layers,
        selected_ids,
        reinstall_actions,
        layer_begin_rename,
        create_adjustment,
    ) = build_layers_page(
            layers,
            canvas,
            redraw,
            layer_clipboard,
            toaster,
            select_layer_content,
            select_folder_content,
            history,
            layer_extensions,
            prepare_delete,
            prepare_reorder,
            alpha_lock_observer,
        );
    let (components_page, refresh_components, component_begin_rename) = components::build(
        Rc::clone(components),
        Rc::clone(on_edit_component),
        Rc::clone(history),
    );
    stack.add_named(&layers_page, Some("layers"));
    stack.add_named(&components_page, Some("components"));

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
        create_adjustment,
    )
}

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
    select_folder_content: &Rc<dyn Fn(Vec<usize>)>,
    history: &Rc<RefCell<HistoryStack>>,
    layer_extensions: &Rc<RefCell<HashMap<String, LayerExtension>>>,
    prepare_delete: &Rc<dyn Fn() -> bool>,
    prepare_reorder: &Rc<dyn Fn()>,
    alpha_lock_observer: &Rc<RefCell<Option<Rc<dyn Fn(bool)>>>>,
) -> (
    gtk::Box,
    Rc<dyn Fn()>,
    Rc<dyn Fn() -> Vec<String>>,
    Rc<dyn Fn()>,
    Rc<dyn Fn()>,
    Rc<dyn Fn() -> Option<usize>>,
) {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(TAB_SPACING)
        .vexpand(true)
        .hexpand(true)
        .build();

    let ui = Ui::new((*layers).clone());
    {
        let core_tree = canvas.borrow().layer_tree().to_vec();
        if !core_tree.is_empty() {
            *ui.tree.borrow_mut() = tree_from_core(&core_tree);
        }
    }

    let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
    install_list_draw(&area, &ui);
    sync_height(&area, &ui);

    install_list_input(
        &area,
        &ui,
        Rc::clone(canvas),
        redraw,
        select_layer_content,
        select_folder_content,
        history,
        prepare_reorder,
    );
    actions::install_context_menu(&area, &ui, layer_clipboard);

    let reinstall_actions: Rc<dyn Fn()> = {
        let area = area.clone();
        let ui = ui.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let layer_clipboard = Rc::clone(layer_clipboard);
        let toaster = toaster.clone();
        let history = Rc::clone(history);
        let layer_extensions = Rc::clone(layer_extensions);
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
                &layer_extensions,
                &prepare_delete,
            );
        })
    };
    reinstall_actions();
    start_thumbnail_refresh(&ui, Rc::clone(canvas), area.clone());

    let (header, lock_btn) =
        build_layers_header(&ui, &area, canvas, redraw, toaster, history);
    page.append(&header);
    page.append(&build_blend_controls(
        &ui,
        canvas,
        redraw,
        history,
        alpha_lock_observer,
        &lock_btn,
    ));

    // Scroll by draw offset, not a ScrolledWindow: re-allocating the area during
    // autoscroll cancels an in-progress stylus reorder (as does connect_cancel).
    let list_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    list_row.set_vexpand(true);
    list_row.set_hexpand(true);
    list_row.append(&area);
    let scrollbar = gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&ui.vadj));
    install_scrollbar_pen_drag(&scrollbar);
    list_row.append(&scrollbar);
    page.append(&list_row);
    page.append(&build_layers_footer());

    {
        let area = area.clone();
        ui.vadj.connect_value_changed(move |_| area.queue_draw());
    }
    {
        let scrollbar = scrollbar.clone();
        let update = move |adj: &gtk::Adjustment| {
            scrollbar.set_visible(adj.upper() > adj.page_size() + 0.5);
        };
        update(&ui.vadj);
        ui.vadj.connect_changed(move |adj| update(adj));
    }
    {
        let ui = ui.clone();
        let area_w = area.clone();
        area.connect_resize(move |_, _w, h| {
            update_list_metrics(&ui, f64::from(h));
            area_w.queue_draw();
        });
    }
    {
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let ui = ui.clone();
        wheel.connect_scroll(move |_, _dx, dy| {
            let adj = &ui.vadj;
            let max = (adj.upper() - adj.page_size()).max(0.0);
            adj.set_value((adj.value() + dy * ITEM_HEIGHT).clamp(0.0, max));
            glib::Propagation::Stop
        });
        area.add_controller(wheel);
    }

    let refresh = {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        Rc::new(move || {
            let c = canvas.borrow();
            {
                let mut tree = ui.tree.borrow_mut();
                if !c.layer_tree().is_empty() {
                    let mut masks = std::collections::HashMap::new();
                    collect_group_masks(&tree, &mut masks);
                    let mut adopted = tree_from_core(c.layer_tree());
                    overlay_group_masks(&mut adopted, &masks);
                    *tree = adopted;
                }
            }
            let snap = c.layers().snapshot();
            reconcile_tree(&mut ui.tree.borrow_mut(), &snap);
            sync_tree_order_from_canvas(&mut ui.tree.borrow_mut(), &c);
            let visible_ids: HashSet<String> =
                snap.iter().filter(|l| l.visible).map(|l| l.id.clone()).collect();
            derive_group_visibility(&mut ui.tree.borrow_mut(), &visible_ids);
            drop(c);
            commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
            sync_height(&area, &ui);
            ui.sync_blend_controls();
            area.queue_draw();
        }) as Rc<dyn Fn()>
    };

    let selected_ids = {
        let ui = ui.clone();
        Rc::new(move || ui.selected_layer_ids_in_order()) as Rc<dyn Fn() -> Vec<String>>
    };

    let begin_rename = {
        let area = area.clone();
        let ui = ui.clone();
        let canvas = Rc::clone(canvas);
        let history = Rc::clone(history);
        Rc::new(move || begin_rename_active(&area, &ui, &canvas, &history)) as Rc<dyn Fn()>
    };

    let create_adjustment = {
        let ui = ui.clone();
        let canvas = Rc::clone(canvas);
        let history = Rc::clone(history);
        let refresh = Rc::clone(&refresh);
        Rc::new(move || {
            match create_layer_at_selection(&ui, &canvas, &history, NewLayerKind::Adjustment) {
                Ok(idx) => {
                    refresh();
                    Some(idx)
                }
                Err(e) => {
                    tracing::error!(error = %e, "add adjustment layer failed");
                    None
                }
            }
        }) as Rc<dyn Fn() -> Option<usize>>
    };

    (page, refresh, selected_ids, reinstall_actions, begin_rename, create_adjustment)
}

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
) -> (gtk::Box, gtk::ToggleButton) {
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(TAB_SPACING)
        .build();

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
            match create_layer_at_selection(&ui, &canvas, &history, NewLayerKind::Raster) {
                Ok(_) => {
                    sync_height(&area, &ui);
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

    let group_btn = gtk::Button::builder()
        .icon_name("folder-new-symbolic")
        .tooltip_text(tooltip_with_accel("Group selected layers", "layer-group"))
        .css_classes(["flat", "circular"])
        .action_name("app.layer-group")
        .build();
    header.append(&group_btn);

    let spacer = gtk::Label::builder()
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    header.append(&spacer);

    let lock_btn = gtk::ToggleButton::builder()
        .icon_name("oxiedraw-alpha-lock-symbolic")
        .tooltip_text("Lock alpha - paint only where this layer already has pixels")
        .css_classes(["flat", "alpha-lock"])
        .width_request(34)
        .height_request(34)
        .build();
    load_lock_toggle_css();
    header.append(&lock_btn);

    (header, lock_btn)
}

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

fn build_blend_controls(
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    history: &Rc<RefCell<HistoryStack>>,
    alpha_lock_observer: &Rc<RefCell<Option<Rc<dyn Fn(bool)>>>>,
    lock_btn: &gtk::ToggleButton,
) -> gtk::Box {
    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(TAB_SPACING)
        .build();

    let labels = BlendMode::labels();
    let mode_dropdown = gtk::DropDown::from_strings(&labels);
    mode_dropdown.set_hexpand(true);
    mode_dropdown.set_tooltip_text(Some("Blend mode of the selected layers"));

    let opacity = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    opacity.set_hexpand(true);
    opacity.set_draw_value(false);
    opacity.set_tooltip_text(Some("Opacity of the selected layers"));
    crate::widgets::slider::install_pen_drag(&opacity);

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

    let guard = Rc::new(std::cell::Cell::new(false));
    let drag_origin: Rc<RefCell<Option<Vec<(String, BlendMode, f32)>>>> =
        Rc::new(RefCell::new(None));
    let commit_source: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

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

    let sync: Rc<dyn Fn()> = {
        let ui = ui.clone();
        let guard = Rc::clone(&guard);
        let mode_dropdown = mode_dropdown.clone();
        let opacity = opacity.clone();
        let opacity_label = opacity_label.clone();
        let lock_btn = lock_btn.clone();
        let alpha_lock_observer = Rc::clone(alpha_lock_observer);
        let commit_opacity = Rc::clone(&commit_opacity);
        let commit_source = Rc::clone(&commit_source);
        Rc::new(move || {
            if let Some(src) = commit_source.borrow_mut().take() {
                src.remove();
            }
            commit_opacity();
            let indices = ui.selected_indices();
            let sensitive = !indices.is_empty();
            mode_dropdown.set_sensitive(sensitive);
            opacity.set_sensitive(sensitive);
            let primary = ui.state.active().filter(|i| indices.contains(i))
                .or_else(|| indices.first().copied());
            let (blend, op) = primary
                .and_then(|i| ui.state.blend(i))
                .unwrap_or((BlendMode::Normal, 1.0));
            let snapshot = ui.state.snapshot();
            let lockable: Vec<_> = indices
                .iter()
                .filter_map(|i| snapshot.get(*i))
                .filter(|l| l.can_alpha_lock())
                .collect();
            let locked_count = lockable.iter().filter(|l| l.alpha_locked).count();
            let all_locked = !lockable.is_empty() && locked_count == lockable.len();
            let mixed = locked_count > 0 && !all_locked;
            lock_btn.set_sensitive(!lockable.is_empty());
            if mixed {
                lock_btn.add_css_class("mixed");
            } else {
                lock_btn.remove_css_class("mixed");
            }

            guard.set(true);
            mode_dropdown.set_selected(blend.to_index());
            opacity.set_value(f64::from(op));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            opacity_label.set_label(&format!("{}%", (op * 100.0).round() as i32));
            lock_btn.set_active(all_locked);
            guard.set(false);

            let active_locked = ui
                .state
                .active()
                .and_then(|i| snapshot.get(i))
                .is_some_and(oxiedraw_core::document::Layer::alpha_lock_active);
            let observer = alpha_lock_observer.borrow().clone();
            if let Some(observer) = observer {
                observer(active_locked);
            }
        })
    };
    {
        let guard = Rc::clone(&guard);
        lock_btn.connect_toggled(move |_| {
            if guard.get() {
                return;
            }
            if let Some(gio_app) = gio::Application::default()
                && let Ok(app) = gio_app.downcast::<gtk::Application>()
            {
                app.activate_action("layer-alpha-lock", None);
            }
        });
    }

    sync();
    *ui.blend_sync.borrow_mut() = Some(Rc::clone(&sync));

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

#[allow(clippy::cast_precision_loss)]
const fn count_f64(n: usize) -> f64 {
    n as f64
}

pub(super) struct RowLayout {
    tops: Vec<f64>,
    total: f64,
}

impl RowLayout {
    pub(super) fn new(rows: &[VisibleRow]) -> Self {
        let mut opening = vec![0.0; rows.len()];
        let mut closing_pad = vec![0.0; rows.len()];
        for i in 0..rows.len() {
            if opens_box(rows, i) {
                opening[i] += NEST_PAD;
                closing_pad[i + contained_rows(rows, i)] += NEST_PAD;
            }
        }

        let mut tops = Vec::with_capacity(rows.len());
        let mut y = LIST_PADDING;
        for i in 0..rows.len() {
            if i > 0 {
                y += ITEM_HEIGHT + ITEM_GAP + closing_pad[i - 1];
            }
            y += opening[i];
            tops.push(y);
        }
        let total = tops.last().map_or(LIST_PADDING * 2.0, |last| {
            last + ITEM_HEIGHT
                + closing_pad.last().copied().unwrap_or(0.0)
                + LIST_PADDING
        });
        Self { tops, total }
    }

    pub(super) fn top(&self, index: usize) -> f64 {
        self.tops
            .get(index)
            .copied()
            .unwrap_or(self.total - LIST_PADDING)
    }

    fn bottom(&self, index: usize) -> f64 {
        self.top(index) + ITEM_HEIGHT
    }

    pub(super) fn slot_nearest(&self, top_y: f64) -> usize {
        let mut best = 0;
        let mut best_d = f64::MAX;
        for (i, &t) in self.tops.iter().enumerate() {
            let d = (top_y - t).abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    }

    pub(super) const fn total_height(&self) -> f64 {
        self.total
    }

    pub(super) fn at(&self, y: f64) -> Option<usize> {
        self.tops
            .iter()
            .position(|&t| y >= t && y <= t + ITEM_HEIGHT)
    }

}

fn update_list_metrics(ui: &Ui, viewport_h: f64) {
    let snapshot = ui.state.snapshot();
    let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
    let total = RowLayout::new(&rows).total_height();
    let adj = &ui.vadj;
    if (adj.upper() - total).abs() > 0.5 {
        adj.set_upper(total);
    }
    if viewport_h > 0.0 && (adj.page_size() - viewport_h).abs() > 0.5 {
        adj.set_page_size(viewport_h);
        adj.set_page_increment(viewport_h);
    }
    let max = (adj.upper() - adj.page_size()).max(0.0);
    if adj.value() > max {
        adj.set_value(max);
    }
}

pub(super) fn sync_height(area: &gtk::DrawingArea, ui: &Ui) {
    let snapshot = ui.state.snapshot();
    reconcile_tree(&mut ui.tree.borrow_mut(), &snapshot);
    update_list_metrics(ui, f64::from(area.height()));
    area.queue_draw();
}

fn install_list_draw(area: &gtk::DrawingArea, ui: &Ui) {
    let ui = ui.clone();
    area.set_draw_func(move |widget, ctx, width_px, _h| {
        let palette = Palette::resolve(widget);
        ui.sync_selection_to_state();
        let snapshot = ui.state.snapshot();
        let drag = ui.drag.borrow().clone();
        let settle = ui.drop_settle.borrow().clone();
        let active_id = ui.active_id();
        let active_group = ui.active_group.borrow().clone();
        let multi = ui.multi_selected.borrow();
        let thumbnails = ui.thumbnails.borrow();
        let width = f64::from(width_px);

        set_source(ctx, palette.window_bg);
        ctx.paint().ok();

        ctx.translate(0.0, -ui.scroll_offset());

        let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
        let layout = RowLayout::new(&rows);
        let count = rows.len();
        let hover = if drag.is_none() { *ui.hover.borrow() } else { None };

        let float = drag
            .as_ref()
            .filter(|d| d.zone == HitZone::Handle && d.from_row < count)
            .map(|d| {
                let span = drag_span(&rows, d.from_row);
                let max_top = if count > span {
                    layout.top(count - span)
                } else {
                    LIST_PADDING
                };
                FloatBlock {
                    from: d.from_row,
                    span,
                    top: (d.pointer_y - d.grab_offset_y).clamp(LIST_PADDING, max_top),
                    depth_offset: d.animated_depth_offset,
                    lift: 1.0,
                }
            })
            .or_else(|| {
                let s = settle.as_ref().filter(|s| s.matches(&rows))?;
                let left = s.remaining();
                Some(FloatBlock {
                    from: s.from_row,
                    span: s.span,
                    top: layout.top(s.from_row) + s.offset_y * left,
                    depth_offset: s.depth_offset * left,
                    lift: left,
                })
            });

        let row_offset = |r: usize| -> f64 {
            if let Some(d) = drag.as_ref().filter(|d| d.zone == HitZone::Handle) {
                return d.row_y_anim.get(r).copied().unwrap_or(0.0);
            }
            settle
                .as_ref()
                .filter(|s| s.matches(&rows))
                .map_or(0.0, |s| s.row_y.get(r).copied().unwrap_or(0.0) * s.remaining())
        };

        for (row_idx, _) in rows.iter().enumerate() {
            if !opens_box(&rows, row_idx) {
                continue;
            }
            let held = contained_rows(&rows, row_idx);
            if float.as_ref().is_some_and(|f| {
                row_idx >= f.from && row_idx + held < f.from + f.span
            }) {
                continue;
            }
            let level = tree_depth(&rows[row_idx]);
            draw_nest_box(
                ctx,
                count_f64(level),
                width,
                layout.top(row_idx) - NEST_PAD + row_offset(row_idx),
                container_bottom(&rows, &layout, row_idx) + row_offset(row_idx + held),
                palette.surface(level),
            );
        }

        for (row_idx, row) in rows.iter().enumerate() {
            let is_floating = float
                .as_ref()
                .is_some_and(|f| row_idx >= f.from && row_idx < f.from + f.span);
            if is_floating {
                continue;
            }
            let y = layout.top(row_idx) + row_offset(row_idx);
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
            let nest = box_depth(&rows, row_idx);
            let own_bg = if nest == 0 { 1.0 } else { 0.0 };
            draw_row(ctx, &palette, width, y, row, count_f64(nest), is_active, is_multi, thumb, is_component, is_text, is_adjustment, mask_active, hover_zone, clip_info(&rows, row_idx), nest, own_bg);
        }

        if let Some(f) = &float {
            let (from, span) = (f.from, f.span);
            let anim = f.depth_offset;

            let block: Vec<VisibleRow> = rows[from..(from + span).min(count)].to_vec();
            let block_layout = RowLayout::new(&block);
            let shift = f.top - block_layout.top(0);
            let block_base = tree_depth(&block[0]);

            {
                let level = (count_f64(tree_depth(&block[0])) + anim).max(0.0);
                let (sx, sy, sw, sh, radius) = if opens_box(&block, 0) {
                    let top = shift + block_layout.top(0) - NEST_PAD;
                    let bottom = shift + container_bottom(&block, &block_layout, 0);
                    let x = nest_left_f(level);
                    (x, top, nest_right(width) - x, bottom - top, NEST_RADIUS)
                } else {
                    let clipped = clip_info(&block, 0).clipped;
                    let x = nest_left_f(level) + if clipped { INDENT_STEP } else { 0.0 };
                    let top = shift + block_layout.top(0);
                    (x, top, nest_right(width) - x, ITEM_HEIGHT, ITEM_RADIUS)
                };
                draw_drop_shadow(ctx, sx, sy, sw, sh, radius, f.lift);
            }

            for (j, _) in block.iter().enumerate() {
                if !opens_box(&block, j) {
                    continue;
                }
                let level = count_f64(tree_depth(&block[j])) + anim;
                draw_nest_box(
                    ctx,
                    level.max(0.0),
                    width,
                    shift + block_layout.top(j) - NEST_PAD,
                    shift + container_bottom(&block, &block_layout, j),
                    palette.surface(tree_depth(&block[j])),
                );
            }

            for i in 0..span {
                let row_idx = from + i;
                if row_idx >= count {
                    break;
                }
                let row = &rows[row_idx];
                let y = shift + block_layout.top(i);
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
                let nest = box_depth(&rows, row_idx);
                let depth_f = (count_f64(nest) + anim).max(0.0);
                let lifted_bg = if box_depth(&block, i) == block_base { 1.0 } else { 0.0 };
                let resting_bg = if nest == 0 { 1.0 } else { 0.0 };
                let own_bg = resting_bg + (lifted_bg - resting_bg) * f.lift;
                let is_component = row_is_component(&ui, row);
                let is_text = row_is_text(&ui, row);
                let is_adjustment = row_is_adjustment(&ui, row);
                let mask_active = is_adjustment && row_mask_active(&ui, row);
                let mut clip = clip_info(&rows, row_idx);
                clip.clipped_above = false;
                clip.clipped_below = false;
                clip.is_base = false;
                draw_row(ctx, &palette, width, y, row, depth_f, is_active, false, thumb, is_component, is_text, is_adjustment, mask_active, None, clip, nest, own_bg);
            }
        }
    });
}

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

fn reordered_rows(
    rows: &[VisibleRow],
    from: usize,
    span: usize,
    to: usize,
    depth_delta: isize,
) -> Vec<VisibleRow> {
    let mut placed: Vec<Option<VisibleRow>> = vec![None; rows.len()];
    for (r, row) in rows.iter().enumerate() {
        let dragged = r >= from && r < from + span;
        let new_idx = if dragged {
            to + (r - from)
        } else {
            displaced_row(r, from, span, to)
        };
        let mut row = row.clone();
        if dragged {
            row.depth = usize::try_from(
                isize::try_from(row.depth).unwrap_or(0).saturating_add(depth_delta),
            )
            .unwrap_or(0);
        }
        if let Some(slot) = placed.get_mut(new_idx) {
            *slot = Some(row);
        }
    }
    placed
        .into_iter()
        .enumerate()
        .map(|(i, slot)| slot.unwrap_or_else(|| rows[i].clone()))
        .collect()
}

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

struct ReleaseVisual {
    row_tops: HashMap<String, f64>,
    block_top: f64,
    block_depth: f64,
}

fn release_visual(rows: &[VisibleRow], drag: &Drag, span: usize) -> ReleaseVisual {
    let layout = RowLayout::new(rows);
    let count = rows.len();
    let row_tops = rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let nudge = drag.row_y_anim.get(r).copied().unwrap_or(0.0);
            (row_id(row).to_string(), layout.top(r) + nudge)
        })
        .collect();
    let max_top = if count > span {
        layout.top(count - span)
    } else {
        LIST_PADDING
    };
    let depth = rows.get(drag.from_row).map_or(0, tree_depth);
    ReleaseVisual {
        row_tops,
        block_top: (drag.pointer_y - drag.grab_offset_y).clamp(LIST_PADDING, max_top),
        block_depth: count_f64(depth) + drag.animated_depth_offset,
    }
}

fn settle_from_release(
    rows: &[VisibleRow],
    before: &ReleaseVisual,
    dragged_id: &str,
) -> Option<DropSettle> {
    let from_row = rows.iter().position(|r| row_id(r) == dragged_id)?;
    let layout = RowLayout::new(rows);
    let span = drag_span(rows, from_row);

    let row_y: Vec<f64> = rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            if r >= from_row && r < from_row + span {
                return 0.0;
            }
            before
                .row_tops
                .get(row_id(row))
                .map_or(0.0, |was| was - layout.top(r))
        })
        .collect();

    let offset_y = before.block_top - layout.top(from_row);
    let depth_offset = before.block_depth - count_f64(tree_depth(&rows[from_row]));

    let still = offset_y.abs() < 0.5
        && depth_offset.abs() < 0.01
        && row_y.iter().all(|y| y.abs() < 0.5);
    if still {
        return None;
    }

    Some(DropSettle {
        from_row,
        span,
        id: dragged_id.to_string(),
        offset_y,
        depth_offset,
        row_y,
        progress: 0.0,
        last_frame_time_us: 0,
    })
}

fn start_drop_settle(
    ui: &Ui,
    area: &gtk::DrawingArea,
    before: &ReleaseVisual,
    dragged_id: &str,
) {
    let settle = {
        let snapshot = ui.state.snapshot();
        let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
        settle_from_release(&rows, before, dragged_id)
    };
    let Some(settle) = settle else {
        *ui.drop_settle.borrow_mut() = None;
        return;
    };
    *ui.drop_settle.borrow_mut() = Some(settle);

    if ui.settle_ticking.replace(true) {
        return;
    }
    let tick_ui = ui.clone();
    area.add_tick_callback(move |widget, clock| {
        let now = clock.frame_time();
        let done = {
            let mut settle = tick_ui.drop_settle.borrow_mut();
            let Some(s) = settle.as_mut() else {
                tick_ui.settle_ticking.set(false);
                return glib::ControlFlow::Break;
            };
            let dt = if s.last_frame_time_us == 0 {
                0.0
            } else {
                ((now - s.last_frame_time_us) as f64 * 1e-6).clamp(0.0, 0.1)
            };
            s.last_frame_time_us = now;
            s.progress += dt / DROP_SETTLE_SECS;
            s.progress >= 1.0
        };
        if done {
            *tick_ui.drop_settle.borrow_mut() = None;
            tick_ui.settle_ticking.set(false);
            widget.queue_draw();
            return glib::ControlFlow::Break;
        }
        widget.queue_draw();
        glib::ControlFlow::Continue
    });
}

#[derive(Clone, Copy)]
struct Palette {
    window_bg: Rgb,
    row_bg: Rgb,
    accent_bg: Rgb,
    accent_fg: Rgb,
    fg: Rgb,
    lock_accent: Rgb,
    lock_glyph: Rgb,
}

impl Palette {
    fn resolve(widget: &gtk::DrawingArea) -> Self {
        let fg = lookup(widget, "view_fg_color").unwrap_or(FALLBACK_FG);
        let window_bg = lookup(widget, "window_bg_color").unwrap_or(FALLBACK_WINDOW_BG);
        let row_bg = lookup_rgba(widget, "card_bg_color")
            .map_or(FALLBACK_ROW_BG, |c| over(c, window_bg));
        Self {
            window_bg,
            row_bg,
            accent_bg: lookup(widget, "accent_bg_color").unwrap_or(FALLBACK_ACCENT_BG),
            accent_fg: lookup(widget, "accent_fg_color").unwrap_or(FALLBACK_ACCENT_FG),
            fg,
            lock_accent: crate::theme::warning_accent(widget),
            lock_glyph: crate::theme::warning_fg(widget),
        }
    }

    fn surface(&self, level: usize) -> Rgb {
        if level == 0 {
            return self.row_bg;
        }
        let t = (0.06 * count_f64(level)).min(0.24);
        lerp_rgb(self.row_bg, self.fg, t)
    }

    fn backdrop(&self, nest: usize) -> Rgb {
        nest.checked_sub(1)
            .map_or(self.window_bg, |level| self.surface(level))
    }
}

fn lookup(widget: &gtk::DrawingArea, name: &str) -> Option<Rgb> {
    lookup_rgba(widget, name).map(|(r, g, b, _)| (r, g, b))
}

fn lookup_rgba(widget: &gtk::DrawingArea, name: &str) -> Option<(f64, f64, f64, f64)> {
    #[allow(deprecated)]
    let rgba = widget.style_context().lookup_color(name)?;
    Some((
        f64::from(rgba.red()),
        f64::from(rgba.green()),
        f64::from(rgba.blue()),
        f64::from(rgba.alpha()),
    ))
}

fn over((r, g, b, a): (f64, f64, f64, f64), bg: Rgb) -> Rgb {
    (
        a.mul_add(r - bg.0, bg.0),
        a.mul_add(g - bg.1, bg.1),
        a.mul_add(b - bg.2, bg.2),
    )
}

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

fn row_mask_active(ui: &Ui, row: &VisibleRow) -> bool {
    match &row.kind {
        RowKind::Layer { id, .. } => ui.mask_view.borrow().as_deref() == Some(id.as_str()),
        RowKind::Group { .. } => false,
    }
}

#[allow(clippy::fn_params_excessive_bools)]
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
    clip: ClipInfo,
    nest: usize,
    own_bg: f64,
) {
    let left = LIST_PADDING + depth_f * NEST_INSET;
    let right = nest_right(width);
    let indent = if clip.clipped { INDENT_STEP } else { 0.0 };
    let row_w = (right - left).max(0.0);

    let (visible, name) = match &row.kind {
        RowKind::Layer { visible, name, .. } | RowKind::Group { visible, name, .. } => (*visible, name.as_str()),
    };
    let alpha_locked = matches!(row.kind, RowKind::Layer { alpha_locked: true, .. });

    let eye_dim = !visible;
    let dim = eye_dim || clip.base_hidden || (clip.clipped && !clip.has_base);

    let surface = palette.surface(nest);
    let bg = if is_active {
        Some(palette.accent_bg)
    } else if is_multi {
        Some(lerp_rgb(surface, palette.accent_bg, 0.25))
    } else if own_bg > 0.0 {
        Some(lerp_rgb(palette.backdrop(nest), palette.row_bg, own_bg))
    } else {
        None
    };
    let text_color = if is_active { palette.accent_fg } else { palette.fg };
    let icon_color = text_color;

    let content_left = left + indent;
    let indented_w = (row_w - indent).max(0.0);
    if let Some(bg) = bg {
        rounded_rect(ctx, content_left, top, indented_w, ITEM_HEIGHT, ITEM_RADIUS);
        set_source(ctx, bg);
        ctx.fill().ok();
    }

    let (rod, rod_alpha) = if is_active {
        (palette.accent_fg, 0.85)
    } else {
        (palette.fg, 0.55)
    };
    let rod_alpha = if dim { rod_alpha * 0.4 } else { rod_alpha };
    if clip.is_base {
        draw_clip_terminus(ctx, left, top, rod, rod_alpha);
    }

    match &row.kind {
        RowKind::Layer { .. } => {
            if clip.clipped {
                let alpha = if hover_zone == Some(HitZone::Clip) { 1.0 } else { rod_alpha };
                draw_clip_rod(ctx, left, top, clip, rod, alpha);
            }

            let sx = content_left + ITEM_INNER_PAD;
            let sy = top + (ITEM_HEIGHT - SWATCH_SIZE) / 2.0;
            if dim {
                ctx.push_group();
            }
            draw_swatch(ctx, sx, sy, thumbnail);
            if alpha_locked {
                draw_alpha_lock_badge(
                    ctx,
                    sx,
                    sy,
                    palette.lock_accent,
                    bg.unwrap_or(surface),
                    palette.lock_glyph,
                    1.0,
                );
            }
            if dim {
                ctx.pop_group_to_source().ok();
                ctx.paint_with_alpha(0.4).ok();
            }
            if is_component {
                rounded_rect(ctx, sx + 1.0, sy + 1.0, SWATCH_SIZE - 2.0, SWATCH_SIZE - 2.0, SWATCH_RADIUS);
                set_source(ctx, palette.accent_bg);
                ctx.set_line_width(2.0);
                ctx.stroke().ok();
            }

            let handle_left = right - ITEM_INNER_PAD - HANDLE_WIDTH;
            let eye_cx = handle_left - ITEM_INNER_PAD - EYE_RADIUS;
            let eye_cy = top + ITEM_HEIGHT / 2.0;

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
            draw_label_underlined(
                ctx,
                name,
                text_left,
                top + ITEM_HEIGHT / 2.0,
                text_max_w,
                clip.is_base && !is_active,
            );
            if dim {
                ctx.restore().ok();
            }

            if hover_zone == Some(HitZone::Eye) {
                draw_icon_hover_bg(ctx, eye_cx, eye_cy, EYE_RADIUS, palette.fg);
            }
            draw_eye(ctx, eye_cx, eye_cy, EYE_RADIUS, visible, icon_color, eye_dim);
            if is_adjustment {
                if hover_zone == Some(HitZone::Edit) {
                    draw_icon_hover_bg(ctx, sliders_cx, eye_cy, EDIT_RADIUS, palette.fg);
                }
                draw_sliders(ctx, sliders_cx, eye_cy, EDIT_RADIUS, icon_color, dim);
                if hover_zone == Some(HitZone::Mask) {
                    draw_icon_hover_bg(ctx, mask_cx, eye_cy, MASK_RADIUS, palette.fg);
                }
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

fn connector_x(nest_left: f64) -> f64 {
    nest_left + CONNECTOR_INSET
}

fn draw_drop_shadow(
    ctx: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    radius: f64,
    alpha: f64,
) {
    if w <= 0.0 || h <= 0.0 || alpha <= 0.0 {
        return;
    }
    ctx.save().ok();
    for i in (0..SHADOW_STEPS).rev() {
        let grow = SHADOW_SPREAD * count_f64(i + 1) / count_f64(SHADOW_STEPS);
        rounded_rect(
            ctx,
            x - grow,
            y - grow + SHADOW_OFFSET_Y,
            w + grow * 2.0,
            h + grow * 2.0,
            radius + grow,
        );
        ctx.set_source_rgba(0.0, 0.0, 0.0, SHADOW_STEP_ALPHA * alpha);
        ctx.fill().ok();
    }
    ctx.restore().ok();
}

fn draw_nest_box(ctx: &cairo::Context, level: f64, width: f64, top: f64, bottom: f64, color: Rgb) {
    let left = nest_left_f(level);
    let w = nest_right(width) - left;
    if w <= 0.0 || bottom <= top {
        return;
    }
    rounded_rect(ctx, left, top, w, bottom - top, NEST_RADIUS);
    set_source(ctx, color);
    ctx.fill().ok();
}

fn draw_clip_rod(ctx: &cairo::Context, nest_left: f64, top: f64, info: ClipInfo, color: Rgb, alpha: f64) {
    let x = connector_x(nest_left);
    ctx.save().ok();
    ctx.set_source_rgba(color.0, color.1, color.2, alpha);
    ctx.set_line_width(CONNECTOR_WIDTH);
    ctx.set_line_cap(cairo::LineCap::Round);

    let start = if info.clipped_above { top - ITEM_GAP } else { top + 10.0 };
    ctx.move_to(x, start);
    if info.has_base {
        ctx.line_to(x, top + ITEM_HEIGHT);
    } else {
        ctx.line_to(x, top + ITEM_HEIGHT - 10.0);
    }
    ctx.stroke().ok();
    ctx.restore().ok();
}

fn draw_clip_terminus(ctx: &cairo::Context, nest_left: f64, top: f64, color: Rgb, alpha: f64) {
    let x = connector_x(nest_left);
    let cy = top + ITEM_HEIGHT / 2.0;
    let content_left = nest_left;
    ctx.save().ok();
    ctx.set_source_rgba(color.0, color.1, color.2, alpha);
    ctx.set_line_width(CONNECTOR_WIDTH);
    ctx.move_to(x, top - ITEM_GAP);
    ctx.line_to(x, cy - CONNECTOR_R);
    ctx.arc_negative(
        x + CONNECTOR_R,
        cy - CONNECTOR_R,
        CONNECTOR_R,
        std::f64::consts::PI,
        std::f64::consts::FRAC_PI_2,
    );
    ctx.line_to(content_left + ITEM_INNER_PAD - 1.0, cy);
    ctx.stroke().ok();
    ctx.restore().ok();
}

fn draw_alpha_lock_badge(
    ctx: &cairo::Context,
    sx: f64,
    sy: f64,
    fg: Rgb,
    bg: Rgb,
    glyph: Rgb,
    alpha: f64,
) {
    let cx = sx + SWATCH_SIZE - 12.0;
    let cy = sy + SWATCH_SIZE - 12.0;

    ctx.save().ok();
    ctx.set_source_rgb(bg.0, bg.1, bg.2);
    ctx.arc(cx, cy, LOCK_RING_D / 2.0, 0.0, TAU);
    ctx.fill().ok();

    ctx.set_source_rgba(fg.0, fg.1, fg.2, alpha);
    ctx.arc(cx, cy, LOCK_DISC_D / 2.0, 0.0, TAU);
    ctx.fill().ok();

    ctx.set_source_rgba(glyph.0, glyph.1, glyph.2, 0.8 * alpha);
    ctx.set_line_width(1.3);
    ctx.new_path();
    ctx.arc(cx, cy - 1.1, 2.1, std::f64::consts::PI, TAU);
    ctx.stroke().ok();
    rounded_rect(ctx, cx - 3.3, cy - 0.6, 6.6, 4.4, 1.0);
    ctx.fill().ok();
    ctx.restore().ok();
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
        ctx.move_to(cx - r, cy);
        ctx.curve_to(cx - r * 0.5, cy - r * 0.65, cx + r * 0.5, cy - r * 0.65, cx + r, cy);
        ctx.curve_to(cx + r * 0.5, cy + r * 0.65, cx - r * 0.5, cy + r * 0.65, cx - r, cy);
        ctx.close_path();
        ctx.stroke().ok();
        ctx.arc(cx, cy, r * 0.38, 0.0, TAU);
        ctx.fill().ok();
    } else {
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

fn draw_icon_hover_bg(ctx: &cairo::Context, cx: f64, cy: f64, r: f64, color: Rgb) {
    ctx.arc(cx, cy, r + 4.0, 0.0, TAU);
    ctx.set_source_rgba(color.0, color.1, color.2, 0.14);
    ctx.fill().ok();
}

fn draw_sliders(ctx: &cairo::Context, cx: f64, cy: f64, r: f64, color: Rgb, dim: bool) {
    ctx.save().ok();
    let alpha = if dim { 0.35 } else { 1.0 };
    ctx.set_source_rgba(color.0, color.1, color.2, alpha);
    ctx.set_line_width(1.4);
    ctx.set_line_cap(cairo::LineCap::Round);

    let left = cx - r;
    let right = cx + r;
    let knob = r * 0.34;
    for (ty, knob_x) in [(cy - r * 0.45, cx - r * 0.3), (cy + r * 0.45, cx + r * 0.3)] {
        ctx.move_to(left, ty);
        ctx.line_to(right, ty);
        ctx.stroke().ok();
        ctx.arc(knob_x, ty, knob, 0.0, TAU);
        ctx.fill().ok();
    }
    ctx.restore().ok();
}

fn draw_mask(ctx: &cairo::Context, cx: f64, cy: f64, r: f64, color: Rgb, dim: bool) {
    ctx.save().ok();
    let alpha = if dim { 0.35 } else { 1.0 };
    ctx.set_source_rgba(color.0, color.1, color.2, alpha);
    let rr = r * 0.85;
    ctx.set_line_width(1.4);
    ctx.arc(cx, cy, rr, 0.0, TAU);
    ctx.stroke().ok();
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
    ctx.move_to(x, y + tab_h);
    ctx.line_to(x + tab_w * 0.75, y + tab_h);
    ctx.line_to(x + tab_w, y);
    ctx.line_to(x, y);
    ctx.close_path();
    ctx.fill().ok();
    ctx.rectangle(x, y + tab_h, w, h - tab_h);
    ctx.fill().ok();
}

fn draw_label(ctx: &cairo::Context, text: &str, x: f64, cy: f64, max_w: f64) {
    draw_label_underlined(ctx, text, x, cy, max_w, false);
}

fn draw_label_underlined(
    ctx: &cairo::Context,
    text: &str,
    x: f64,
    cy: f64,
    max_w: f64,
    underline: bool,
) {
    if max_w <= 0.0 {
        return;
    }
    ctx.save().ok();
    ctx.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    ctx.set_font_size(13.0);
    let extents = ctx.text_extents(text);
    let baseline = extents
        .as_ref()
        .map_or(cy, |e| cy - e.height() / 2.0 - e.y_bearing());
    ctx.rectangle(x, cy - ITEM_HEIGHT / 2.0, max_w, ITEM_HEIGHT);
    ctx.clip();
    ctx.move_to(x, baseline);
    ctx.show_text(text).ok();
    if underline && let Ok(e) = extents {
        let w = e.x_advance().min(max_w);
        ctx.rectangle(x, baseline + 1.0, w, 1.0);
        ctx.fill().ok();
    }
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

fn hit_zone(
    x: f64,
    y_in_row: f64,
    widget_width: f64,
    nest: usize,
    is_group: bool,
    has_edit: bool,
    clipped: bool,
) -> HitZone {
    let nest_left = nest_left(nest);
    let content_left = nest_left + if clipped { INDENT_STEP } else { 0.0 };
    let right = nest_right(widget_width);

    let handle_left = right - ITEM_INNER_PAD - HANDLE_WIDTH;
    if x >= handle_left - 4.0 {
        return HitZone::Handle;
    }

    let eye_cx = handle_left - ITEM_INNER_PAD - EYE_RADIUS;
    if (x - eye_cx).abs() <= EYE_RADIUS + 4.0 {
        return HitZone::Eye;
    }

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

    if clipped && x >= nest_left && x <= nest_left + CLIP_GUTTER_W {
        return HitZone::Clip;
    }

    if is_group {
        let chevron_right = content_left + ITEM_INNER_PAD + CHEVRON_SIZE + 4.0;
        if x <= chevron_right {
            return HitZone::Chevron;
        }
        let folder_x = chevron_right;
        if x >= folder_x && x <= folder_x + FOLDER_W {
            return HitZone::Folder;
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

fn load_lock_toggle_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "button.alpha-lock:checked {
            background: alpha(@warning_bg_color, 0.26);
            color: @warning_color;
        }

        button.alpha-lock:checked:hover {
            background: alpha(@warning_bg_color, 0.38);
            color: @warning_color;
        }

        button.alpha-lock.mixed {
            background: alpha(@warning_bg_color, 0.12);
            color: @warning_color;
            box-shadow: inset 0 -2px 0 0 @warning_color;
        }

        button.alpha-lock.mixed:hover {
            background: alpha(@warning_bg_color, 0.22);
            color: @warning_color;
            box-shadow: inset 0 -2px 0 0 @warning_color;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub(super) fn set_layer_clipped(
    canvas: &Rc<RefCell<Canvas>>,
    history: &Rc<RefCell<HistoryStack>>,
    id: &str,
    flat_idx: usize,
    clipped: bool,
) {
    let old = canvas.borrow().layers().clipped(flat_idx).unwrap_or(false);
    if old == clipped {
        return;
    }
    if let Err(e) = canvas.borrow_mut().set_layer_clipped(flat_idx, clipped) {
        tracing::error!(error = %e, "set_layer_clipped failed");
        return;
    }
    history.borrow_mut().record(HistoryAction::LayerClip {
        id: id.to_string(),
        old,
        new: clipped,
    });
}

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

fn install_scrollbar_pen_drag(sb: &gtk::Scrollbar) {
    let stylus = gtk::GestureStylus::new();
    stylus.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pressed = Rc::new(std::cell::Cell::new(false));

    let set_from_y: Rc<dyn Fn(f64)> = {
        let sb = sb.clone();
        Rc::new(move |y: f64| {
            let height = f64::from(sb.height());
            let adj = sb.adjustment();
            let lower = adj.lower();
            let page = adj.page_size();
            let span = adj.upper() - lower;
            if height <= 0.0 || span <= page {
                return;
            }
            let thumb = (page / span * height).clamp(SCROLL_THUMB_MIN, height);
            let travel = (height - thumb).max(1.0);
            let t = ((y - thumb / 2.0) / travel).clamp(0.0, 1.0);
            adj.set_value(lower + t * (span - page));
        })
    };

    {
        let pressed = Rc::clone(&pressed);
        let set_from_y = Rc::clone(&set_from_y);
        stylus.connect_down(move |_, _x, y| {
            pressed.set(true);
            set_from_y(y);
        });
    }
    {
        let pressed = Rc::clone(&pressed);
        let set_from_y = Rc::clone(&set_from_y);
        stylus.connect_motion(move |_, _x, y| {
            if pressed.get() {
                set_from_y(y);
            }
        });
    }
    {
        let pressed = Rc::clone(&pressed);
        stylus.connect_up(move |_, _, _| pressed.set(false));
    }
    sb.add_controller(stylus);
}

fn event_is_stylus(controller: &impl IsA<gtk::EventController>) -> bool {
    controller
        .current_event_device()
        .is_some_and(|d| d.source() == gdk::InputSource::Pen)
}

fn install_list_input(
    area: &gtk::DrawingArea,
    ui: &Ui,
    canvas: Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    select_layer_content: &Rc<dyn Fn(usize)>,
    select_folder_content: &Rc<dyn Fn(Vec<usize>)>,
    history: &Rc<RefCell<HistoryStack>>,
    prepare_reorder: &Rc<dyn Fn()>,
) {
    let on_begin: Rc<dyn Fn(f64, f64)> = {
        let area_w = area.clone();
        let ui = ui.clone();
        Rc::new(move |x: f64, y: f64| {
            if ui.drag.borrow().is_some() {
                return;
            }
            ui.drop_settle.borrow_mut().take();
            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
            let layout = RowLayout::new(&rows);
            let Some(row_idx) = layout.at(y) else { return };
            let row = &rows[row_idx];
            #[allow(deprecated)]
            let width = f64::from(area_w.allocated_width());
            let y_in_row = y - layout.top(row_idx);
            let is_group = matches!(row.kind, RowKind::Group { .. });
            let has_edit = row_is_adjustment(&ui, row);
            let zone = hit_zone(
                x,
                y_in_row,
                width,
                box_depth(&rows, row_idx),
                is_group,
                has_edit,
                clip_info(&rows, row_idx).clipped,
            );

            *ui.drag.borrow_mut() = Some(Drag {
                from_row: row_idx,
                current_row: row_idx,
                pointer_y: y,
                grab_offset_y: y - layout.top(row_idx),
                zone,
                animated_depth_offset: 0.0,
                row_y_anim: vec![0.0; rows.len()],
                last_frame_time_us: 0,
            });
            if zone == HitZone::Handle {
                area_w.set_cursor(gtk::gdk::Cursor::from_name("row-resize", None).as_ref());

                let tick_ui = ui.clone();
                area_w.add_tick_callback(move |widget, clock| {
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

                    let mut current_row = current_row;
                    {
                        let vadj = tick_ui.vadj.clone();
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
                                let snapshot = tick_ui.state.snapshot();
                                let scroll_rows =
                                    compute_visible_rows(&tick_ui.tree.borrow(), &snapshot);
                                let scroll_layout = RowLayout::new(&scroll_rows);
                                let mut d = tick_ui.drag.borrow_mut();
                                if let Some(d) = d.as_mut() {
                                    d.pointer_y += applied;
                                    d.current_row = insertion_index(
                                        &scroll_layout,
                                        d.pointer_y - d.grab_offset_y,
                                    );
                                    current_row = d.current_row;
                                }
                            }
                        }
                    }

                    let (target_depth_offset, span, clamped_to, layout, after_layout) = {
                        let snapshot = tick_ui.state.snapshot();
                        let rows = compute_visible_rows(&tick_ui.tree.borrow(), &snapshot);
                        let layout = RowLayout::new(&rows);
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
                        let depth_delta = isize::try_from(target_depth).unwrap_or(0)
                            - isize::try_from(orig_depth).unwrap_or(0);
                        let after = reordered_rows(&rows, from_row, span, to, depth_delta);
                        let after_layout = RowLayout::new(&after);
                        (
                            target_depth as f64 - orig_depth as f64,
                            span,
                            to,
                            layout,
                            after_layout,
                        )
                    };

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
                                let target_y = after_layout.top(target_slot) - layout.top(r);
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
        })
    };

    let on_update: Rc<dyn Fn(f64)> = {
        let area_w = area.clone();
        let ui = ui.clone();
        Rc::new(move |pointer_y: f64| {
            let mut drag_ref = ui.drag.borrow_mut();
            let Some(d) = drag_ref.as_mut() else { return };
            if d.zone != HitZone::Handle {
                return;
            }
            d.pointer_y = pointer_y;
            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
            d.current_row =
                insertion_index(&RowLayout::new(&rows), pointer_y - d.grab_offset_y);
            drop(drag_ref);
            area_w.queue_draw();
        })
    };

    let on_end: Rc<dyn Fn(f64, f64, gdk::ModifierType)> = {
        let area_w = area.clone();
        let ui = ui.clone();
        let redraw = redraw.clone();
        let canvas = Rc::clone(&canvas);
        let select_layer_content = Rc::clone(select_layer_content);
        let select_folder_content = Rc::clone(select_folder_content);
        let history = Rc::clone(history);
        let prepare_reorder = Rc::clone(prepare_reorder);
        Rc::new(move |dx: f64, dy: f64, modifiers: gdk::ModifierType| {
            let drag_state = ui.drag.borrow_mut().take();
            let Some(d) = drag_state else { return };
            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);

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
                                toggle_group_visibility(&ui, &canvas, &history, id, !visible);
                                area_w.queue_draw();
                                redraw.request();
                            }
                        }
                    }
                }
                HitZone::Clip => {
                    if is_click
                        && d.from_row < rows.len()
                        && let RowKind::Layer { id, flat_idx, .. } = &rows[d.from_row].kind
                    {
                        set_layer_clipped(
                            &canvas,
                            &history,
                            id,
                            *flat_idx,
                            false,
                        );
                        area_w.queue_draw();
                        redraw.request();
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
                    let span = drag_span(&rows, d.from_row);
                    let released = release_visual(&rows, &d, span);
                    let held_id = rows.get(d.from_row).map(|r| row_id(r).to_string());

                    if d.from_row != d.current_row {
                        prepare_reorder();
                        let dragged_row = rows.get(d.from_row);
                        let Some((dragged_id, dragged_name, dragged_group)) =
                            dragged_row.map(|r| match &r.kind {
                                RowKind::Layer { id, name, .. } => {
                                    (id.clone(), name.clone(), false)
                                }
                                RowKind::Group { id, name, .. } => {
                                    (id.clone(), name.clone(), true)
                                }
                            })
                        else {
                            area_w.set_cursor(None);
                            area_w.queue_draw();
                            return;
                        };

                        let before_order: Vec<String> = canvas.borrow().layers()
                            .snapshot().iter().map(|l| l.id.clone()).collect();
                        let tree_before = tree_to_core(&ui.tree.borrow());

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
                            let tree_after = tree_to_core(&ui.tree.borrow());
                            record_reorder(
                                &history,
                                &before_order,
                                &after_order,
                                tree_before,
                                tree_after,
                            );
                            log_layer_move(
                                &ui,
                                &canvas,
                                dragged_group,
                                &dragged_name,
                                &before_order,
                                &after_order,
                            );

                            ui.invalidate_selection_sync();
                            sync_height(&area_w, &ui);
                            redraw.request();
                        }
                    }
                    if let Some(id) = held_id {
                        start_drop_settle(&ui, &area_w, &released, &id);
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
                HitZone::Folder if is_click => {
                    if d.from_row < rows.len()
                        && let RowKind::Group { id, .. } = &rows[d.from_row].kind {
                            let leaf_ids = group_leaf_ids(&ui.tree.borrow(), id);
                            let order: Vec<String> = canvas.borrow().layers()
                                .snapshot().iter().map(|l| l.id.clone()).collect();
                            let indices: Vec<usize> = leaf_ids.iter()
                                .filter_map(|lid| order.iter().position(|o| o == lid))
                                .collect();
                            if !indices.is_empty() {
                                select_folder_content(indices);
                            }
                        }
                }
                HitZone::Folder => {}
                HitZone::Body | HitZone::Swatch => {
                    if is_click && d.from_row < rows.len() {
                        let row = &rows[d.from_row];
                        let row_id = match &row.kind {
                            RowKind::Layer { id, .. } | RowKind::Group { id, .. } => id.clone(),
                        };
                        if shift {
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
                        redraw.request();
                    }
                }
            }
        })
    };

    let drag_owner = Rc::new(std::cell::Cell::new(0u8));

    let drag_gesture = gtk::GestureDrag::new();
    drag_gesture.set_button(gdk::BUTTON_PRIMARY);
    {
        let on_begin = Rc::clone(&on_begin);
        let drag_owner = Rc::clone(&drag_owner);
        let ui = ui.clone();
        drag_gesture.connect_drag_begin(move |g, x, y| {
            if drag_owner.get() != 0 || event_is_stylus(g) {
                return;
            }
            drag_owner.set(1);
            on_begin(x, y + ui.scroll_offset());
        });
    }
    {
        let on_update = Rc::clone(&on_update);
        let drag_owner = Rc::clone(&drag_owner);
        let ui = ui.clone();
        drag_gesture.connect_drag_update(move |g, _dx, dy| {
            if drag_owner.get() != 1 {
                return;
            }
            let Some((_sx, sy)) = g.start_point() else { return };
            on_update(sy + dy + ui.scroll_offset());
        });
    }
    {
        let on_end = Rc::clone(&on_end);
        let drag_owner = Rc::clone(&drag_owner);
        drag_gesture.connect_drag_end(move |g, dx, dy| {
            if drag_owner.get() != 1 {
                return;
            }
            drag_owner.set(0);
            let modifiers = g
                .current_event()
                .map_or(gdk::ModifierType::empty(), |ev| ev.modifier_state());
            on_end(dx, dy, modifiers);
        });
    }
    area.add_controller(drag_gesture);

    let stylus = gtk::GestureStylus::new();
    {
        let start = Rc::new(std::cell::Cell::new((0.0_f64, 0.0_f64)));
        {
            let drag_owner = Rc::clone(&drag_owner);
            let start = Rc::clone(&start);
            let on_begin = Rc::clone(&on_begin);
            let ui = ui.clone();
            stylus.connect_down(move |_, x, y| {
                if drag_owner.get() != 0 {
                    return;
                }
                drag_owner.set(2);
                start.set((x, y));
                on_begin(x, y + ui.scroll_offset());
            });
        }
        {
            let drag_owner = Rc::clone(&drag_owner);
            let on_update = Rc::clone(&on_update);
            let ui = ui.clone();
            stylus.connect_motion(move |_, _x, y| {
                if drag_owner.get() == 2 {
                    on_update(y + ui.scroll_offset());
                }
            });
        }
        {
            let drag_owner = Rc::clone(&drag_owner);
            let start = Rc::clone(&start);
            let on_end = Rc::clone(&on_end);
            stylus.connect_up(move |g, x, y| {
                if drag_owner.get() != 2 {
                    return;
                }
                drag_owner.set(0);
                let (sx, sy) = start.get();
                let modifiers = g
                    .current_event()
                    .map_or(gdk::ModifierType::empty(), |ev| ev.modifier_state());
                on_end(x - sx, y - sy, modifiers);
            });
        }
        // No `connect_cancel`: the real pen-up finishes the drag.
    }
    area.add_controller(stylus);

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
            let cy = y + ui.scroll_offset();
            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
            let layout = RowLayout::new(&rows);
            let hit = layout.at(cy).map(|row_idx| {
                let row = &rows[row_idx];
                let y_in_row = cy - layout.top(row_idx);
                let is_group = matches!(row.kind, RowKind::Group { .. });
                let has_edit = row_is_adjustment(&ui, row);
                let zone = hit_zone(
                x,
                y_in_row,
                width,
                box_depth(&rows, row_idx),
                is_group,
                has_edit,
                clip_info(&rows, row_idx).clipped,
            );
                (row_idx, zone)
            });
            let cursor_name = hit.and_then(|(_, zone)| match zone {
                HitZone::Handle => Some("row-resize"),
                HitZone::Eye
                | HitZone::Edit
                | HitZone::Mask
                | HitZone::Clip
                | HitZone::Chevron
                | HitZone::Folder
                | HitZone::Swatch => Some("pointer"),
                HitZone::Body => None,
            });
            let hover = hit.filter(|(_, zone)| {
                matches!(zone, HitZone::Eye | HitZone::Edit | HitZone::Mask | HitZone::Clip)
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

    area.set_has_tooltip(true);
    {
        let ui = ui.clone();
        area.connect_query_tooltip(move |area, x, _y, _keyboard, tooltip| {
            #[allow(deprecated)]
            let width = f64::from(area.allocated_width());
            let Some(row_idx) = ui.hover.borrow().map(|(r, _)| r) else {
                return false;
            };
            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
            let Some(row) = rows.get(row_idx) else { return false };
            let info = clip_info(&rows, row_idx);
            if !info.clipped {
                return false;
            }
            let is_group = matches!(row.kind, RowKind::Group { .. });
            let has_edit = row_is_adjustment(&ui, row);
            let zone = hit_zone(
                f64::from(x),
                0.0,
                width,
                box_depth(&rows, row_idx),
                is_group,
                has_edit,
                true,
            );
            if zone != HitZone::Clip {
                return false;
            }
            let text = if info.has_base {
                let base = clip_base_name(&rows, row_idx).unwrap_or_default();
                format!("Clipped to \"{base}\"")
            } else {
                "No layer below to clip to".to_string()
            };
            tooltip.set_text(Some(&text));
            true
        });
    }
}

fn clip_base_name(rows: &[VisibleRow], i: usize) -> Option<String> {
    let row = rows.get(i)?;
    for below in rows.iter().skip(i + 1) {
        if !same_sibling_list(row, below) {
            return None;
        }
        match &below.kind {
            RowKind::Layer { clipped: true, .. } => {}
            RowKind::Layer { name, .. } => return Some(name.clone()),
            RowKind::Group { .. } => return None,
        }
    }
    None
}

fn toggle_group_visibility(
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    history: &Rc<RefCell<HistoryStack>>,
    group_id: &str,
    new_vis: bool,
) {
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

    let mut changes: Vec<HistoryAction> = Vec::new();
    let new_mask: HashSet<String> = if new_vis {
        let restore_all = prior_mask.is_empty();
        for (lid, idx) in &leaf_indices {
            if (restore_all || prior_mask.contains(lid))
                && snap.get(*idx).is_some_and(|l| !l.visible)
            {
                if let Err(e) = c.set_layer_visible(*idx, true) {
                    tracing::error!(error = %e, leaf = %lid, "group eye: show failed");
                    continue;
                }
                changes.push(HistoryAction::LayerVisibility {
                    id: lid.clone(),
                    old: false,
                    new: true,
                });
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
                changes.push(HistoryAction::LayerVisibility {
                    id: lid.clone(),
                    old: true,
                    new: false,
                });
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

    let action = match changes.len() {
        0 => None,
        1 => changes.into_iter().next(),
        _ => Some(HistoryAction::Batch {
            label: "Toggle group visibility".to_string(),
            actions: changes,
        }),
    };
    if let Some(action) = action {
        history.borrow_mut().record(action);
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
            let before = tree_to_core(&ui.tree.borrow());
            rename_group_in_tree(&mut ui.tree.borrow_mut(), &id, new_name.trim().to_string());
            commit_groups_quiet(&ui.tree.borrow(), &mut canvas.borrow_mut());
            let after = tree_to_core(&ui.tree.borrow());
            record_tree_edit(&history, before, after, "Rename group");
        }
        area.queue_draw();
        pop_c.popdown();
    });

    glib::idle_add_local_once(move || {
        popover_rc.popup();
        entry.grab_focus();
    });
}

pub(super) fn begin_rename_active(
    area: &gtk::DrawingArea,
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    history: &Rc<RefCell<HistoryStack>>,
) {
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
    let layout = RowLayout::new(&rows);
    show_rename_popover(area, target_id, current_name, is_layer, ui, canvas, layout.top(row_idx) - ui.scroll_offset(), history);
}

fn insertion_index(layout: &RowLayout, top_y: f64) -> usize {
    layout.slot_nearest(top_y)
}

fn resolve_insert_target(rows2: &[VisibleRow], current_row: usize) -> (Option<String>, usize) {
    if current_row == 0 || rows2.is_empty() {
        return (None, usize::MAX);
    }
    let above_idx = (current_row - 1).min(rows2.len() - 1);
    let Some(above) = rows2.get(above_idx) else { return (None, rows2.len()) };

    if let RowKind::Group { id, expanded: true, .. } = &above.kind {
        return (Some(id.clone()), usize::MAX);
    }

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

    (above.parent_id.clone(), above.idx_in_parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer_row(parent_id: Option<&str>, idx_in_parent: usize, depth: usize) -> VisibleRow {
        named_layer_row("l", parent_id, idx_in_parent, depth, false, true)
    }

    fn named_layer_row(
        id: &str,
        parent_id: Option<&str>,
        idx_in_parent: usize,
        depth: usize,
        clipped: bool,
        visible: bool,
    ) -> VisibleRow {
        VisibleRow {
            kind: RowKind::Layer {
                id: id.into(),
                name: id.into(),
                visible,
                flat_idx: 0,
                clipped,
                alpha_locked: false,
            },
            depth,
            adjust_indent: 0,
            parent_id: parent_id.map(str::to_owned),
            idx_in_parent,
        }
    }

    #[test]
    fn a_container_shares_a_nesting_level_with_what_it_holds() {
        let rows = vec![
            group_row("g", true, None, 0, 0),
            named_layer_row("a", Some("g"), 0, 1, false, true),
            group_row("sub", true, Some("g"), 1, 1),
            named_layer_row("b", Some("sub"), 0, 2, false, true),
            group_row("g2", true, None, 1, 0),
            named_layer_row("c", Some("g2"), 0, 1, false, true),
        ];

        assert_eq!(box_depth(&rows, 0), 1, "Group is inside its own box");
        assert_eq!(box_depth(&rows, 1), 1, "and so is what it holds");
        assert_eq!(contained_rows(&rows, 0), 3, "a, Subgroup and b");

        assert_eq!(box_depth(&rows, 2), 2, "Subgroup opens a box inside Group's");
        assert_eq!(box_depth(&rows, 3), 2);
        assert_eq!(contained_rows(&rows, 2), 1);

        assert_eq!(box_depth(&rows, 4), 1);
        assert_eq!(contained_rows(&rows, 4), 1, "Group2 stops before nothing");
        assert_eq!(box_depth(&rows, 5), 1);
    }

    #[test]
    fn a_container_adds_its_padding_to_the_rows_it_wraps() {
        let flat = vec![
            named_layer_row("a", None, 0, 0, false, true),
            named_layer_row("b", None, 1, 0, false, true),
        ];
        let flat_layout = RowLayout::new(&flat);
        let pitch = flat_layout.top(1) - flat_layout.top(0);
        assert!(
            (pitch - (ITEM_HEIGHT + ITEM_GAP)).abs() < f64::EPSILON,
            "ungrouped rows keep the plain pitch"
        );

        let nested = vec![
            group_row("g", true, None, 0, 0),
            named_layer_row("a", Some("g"), 0, 1, false, true),
            named_layer_row("after", None, 1, 0, false, true),
        ];
        let layout = RowLayout::new(&nested);
        assert!(
            (layout.top(0) - (LIST_PADDING + NEST_PAD)).abs() < f64::EPSILON,
            "the container's top padding sits above its header"
        );
        let after_gap = layout.top(2) - layout.bottom(1);
        assert!(
            (after_gap - (ITEM_GAP + NEST_PAD)).abs() < f64::EPSILON,
            "the container's bottom padding clears the row after it"
        );
    }

    #[test]
    fn the_drag_animation_lands_where_the_drop_will_put_things() {
        let rows = vec![
            group_row("g", true, None, 0, 0),
            named_layer_row("Layer 3", Some("g"), 0, 1, false, true),
            named_layer_row("Layer 2", Some("g"), 1, 1, false, true),
            named_layer_row("Background", None, 1, 0, false, true),
        ];
        let (from, span, to) = (3usize, 1usize, 0usize);
        let before = RowLayout::new(&rows);
        let after = RowLayout::new(&reordered_rows(&rows, from, span, to, 0));

        let pitch = ITEM_HEIGHT + ITEM_GAP;
        for r in 0..from {
            let slot = displaced_row(r, from, span, to);
            let target = after.top(slot) - before.top(r);
            assert!(
                (target - pitch).abs() < f64::EPSILON,
                "row {r} eases by {target}, expected {pitch}"
            );
        }

        assert!((after.top(to) - LIST_PADDING).abs() < f64::EPSILON);
    }

    #[test]
    fn reordering_moves_the_block_and_closes_the_hole() {
        let rows = vec![
            named_layer_row("a", None, 0, 0, false, true),
            named_layer_row("b", None, 1, 0, false, true),
            named_layer_row("c", None, 2, 0, false, true),
        ];
        let names = |rs: &[VisibleRow]| -> Vec<String> {
            rs.iter()
                .map(|r| match &r.kind {
                    RowKind::Layer { name, .. } | RowKind::Group { name, .. } => name.clone(),
                })
                .collect()
        };
        assert_eq!(names(&reordered_rows(&rows, 2, 1, 0, 0)), ["c", "a", "b"]);
        assert_eq!(names(&reordered_rows(&rows, 0, 1, 2, 0)), ["b", "c", "a"]);
        assert_eq!(names(&reordered_rows(&rows, 0, 2, 1, 0)), ["c", "a", "b"]);
    }

    #[test]
    fn the_drag_animation_accounts_for_dropping_into_a_folder() {
        let rows = vec![
            named_layer_row("Layer 5", None, 0, 0, false, true),
            group_row("g", true, None, 1, 0),
            named_layer_row("Layer 4", Some("g"), 0, 1, false, true),
            named_layer_row("Layer 3", Some("g"), 1, 1, false, true),
            named_layer_row("Layer 2", Some("g"), 2, 1, false, true),
            named_layer_row("Background", None, 2, 0, false, true),
        ];
        let (from, span, to, delta) = (0usize, 1usize, 1usize, 1isize);
        let before = RowLayout::new(&rows);
        let after_rows = reordered_rows(&rows, from, span, to, delta);
        assert_eq!(
            contained_rows(&after_rows, 0),
            4,
            "the folder must be seen to hold the dropped row plus its own three"
        );

        let after = RowLayout::new(&after_rows);
        let header = after.top(displaced_row(1, from, span, to)) - before.top(1);
        assert!(
            (header + ITEM_HEIGHT + ITEM_GAP).abs() < f64::EPSILON,
            "folder header eases by {header}"
        );
        for r in [2usize, 3, 4, 5] {
            let target = after.top(displaced_row(r, from, span, to)) - before.top(r);
            assert!(
                target.abs() < f64::EPSILON,
                "row {r} should not move at all, but eases by {target}"
            );
        }
    }

    fn drag_mid_flight(from_row: usize, to: usize, block_top: f64, eased: &[f64]) -> Drag {
        Drag {
            from_row,
            current_row: to,
            pointer_y: block_top,
            grab_offset_y: 0.0,
            zone: HitZone::Handle,
            animated_depth_offset: 0.0,
            row_y_anim: eased.to_vec(),
            last_frame_time_us: 0,
        }
    }

    #[test]
    fn the_drop_settle_starts_from_where_the_drag_left_off() {
        let rows = vec![
            group_row("g", true, None, 0, 0),
            named_layer_row("Layer 3", Some("g"), 0, 1, false, true),
            named_layer_row("Layer 2", Some("g"), 1, 1, false, true),
            named_layer_row("Background", None, 1, 0, false, true),
        ];
        let (from, span, to) = (3usize, 1usize, 0usize);
        let drag = drag_mid_flight(from, to, 20.0, &[30.0, 30.0, 30.0, 0.0]);
        let released = release_visual(&rows, &drag, span);

        let after_rows = reordered_rows(&rows, from, span, to, 0);
        let settle = settle_from_release(&after_rows, &released, "Background")
            .expect("the release was mid-slide, so there is a gap to close");
        let after = RowLayout::new(&after_rows);

        assert_eq!(settle.from_row, 0, "Background landed at the top");
        assert!(
            (after.top(settle.from_row) + settle.offset_y - released.block_top).abs() < 1e-9,
            "the block would jump {}px on release",
            after.top(settle.from_row) + settle.offset_y - released.block_top
        );
        for (r, row) in after_rows.iter().enumerate() {
            if r == settle.from_row {
                continue;
            }
            let drawn = after.top(r) + settle.row_y[r];
            let was = released.row_tops[row_id(row)];
            assert!((drawn - was).abs() < 1e-9, "row {r} jumps {}px", drawn - was);
        }

        let mut landed = settle;
        landed.progress = 1.0;
        assert!(landed.remaining().abs() < f64::EPSILON);
    }

    #[test]
    fn the_drop_settle_carries_an_unfinished_reparent_indent() {
        let rows = vec![
            named_layer_row("Layer 5", None, 0, 0, false, true),
            group_row("g", true, None, 1, 0),
            named_layer_row("Layer 4", Some("g"), 0, 1, false, true),
        ];
        let (from, span, to) = (0usize, 1usize, 1usize);
        let mut drag = drag_mid_flight(from, to, 8.0, &[0.0, 0.0, 0.0]);
        drag.animated_depth_offset = 0.7;
        let released = release_visual(&rows, &drag, span);

        let after_rows = reordered_rows(&rows, from, span, to, 1);
        let settle = settle_from_release(&after_rows, &released, "Layer 5")
            .expect("the indent is still 0.3 of a level short");
        assert!(
            (settle.depth_offset + 0.3).abs() < 1e-9,
            "indent offset was {}, expected -0.3",
            settle.depth_offset
        );
    }

    #[test]
    fn a_handle_press_that_moved_nothing_does_not_animate() {
        let rows = vec![
            named_layer_row("a", None, 0, 0, false, true),
            named_layer_row("b", None, 1, 0, false, true),
        ];
        let layout = RowLayout::new(&rows);
        let drag = drag_mid_flight(0, 0, layout.top(0), &[0.0, 0.0]);
        let released = release_visual(&rows, &drag, 1);
        assert!(settle_from_release(&rows, &released, "a").is_none());
    }

    fn test_palette() -> Palette {
        Palette {
            window_bg: (0.10, 0.10, 0.11),
            row_bg: (0.18, 0.18, 0.19),
            accent_bg: (0.21, 0.52, 0.89),
            accent_fg: (1.0, 1.0, 1.0),
            fg: (0.90, 0.90, 0.92),
            lock_accent: (1.0, 0.76, 0.32),
            lock_glyph: (0.0, 0.0, 0.0),
        }
    }

    fn rgb_eq(a: Rgb, b: Rgb) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9 && (a.2 - b.2).abs() < 1e-9
    }

    fn painter_of(rows: &[VisibleRow], i: usize) -> Option<usize> {
        if opens_box(rows, i) {
            return Some(i);
        }
        (0..i)
            .rev()
            .find(|&j| opens_box(rows, j) && j + contained_rows(rows, j) >= i)
    }

    #[test]
    fn a_lifted_card_fades_into_whatever_is_actually_behind_the_row() {
        let rows = vec![
            group_row("g", true, None, 0, 0),
            group_row("sub", true, Some("g"), 0, 1),
            named_layer_row("b", Some("sub"), 0, 2, false, true),
            named_layer_row("a", Some("g"), 1, 1, false, true),
            named_layer_row("root", None, 1, 0, false, true),
        ];
        let palette = test_palette();
        for i in 0..rows.len() {
            let painted = painter_of(&rows, i)
                .map_or(palette.window_bg, |j| palette.surface(tree_depth(&rows[j])));
            let faded = palette.backdrop(box_depth(&rows, i));
            assert!(
                rgb_eq(faded, painted),
                "row {i} fades to {faded:?} but sits on {painted:?}"
            );
        }
    }

    #[test]
    fn a_stale_drop_settle_is_ignored() {
        let rows = vec![
            named_layer_row("a", None, 0, 0, false, true),
            named_layer_row("b", None, 1, 0, false, true),
        ];
        let settle = DropSettle {
            from_row: 0,
            span: 1,
            id: "a".into(),
            offset_y: 20.0,
            depth_offset: 0.0,
            row_y: vec![0.0, 12.0],
            progress: 0.0,
            last_frame_time_us: 0,
        };
        assert!(settle.matches(&rows));
        assert!(!settle.matches(&rows[..1]), "a row went away");
        assert!(
            !settle.matches(&[rows[1].clone(), rows[0].clone()]),
            "the block is no longer the row it was animating"
        );
    }

    #[test]
    fn containers_ending_together_stagger_their_bottoms() {
        let rows = vec![
            group_row("outer", true, None, 0, 0),
            group_row("mid", true, Some("outer"), 0, 1),
            group_row("inner", true, Some("mid"), 0, 2),
            named_layer_row("leaf", Some("inner"), 0, 3, false, true),
        ];
        let layout = RowLayout::new(&rows);
        let leaf_bottom = layout.bottom(3);
        for (i, expected_pads) in [(2usize, 1.0_f64), (1, 2.0), (0, 3.0)] {
            let got = container_bottom(&rows, &layout, i);
            let want = expected_pads.mul_add(NEST_PAD, leaf_bottom);
            assert!(
                (got - want).abs() < f64::EPSILON,
                "container {i}: bottom {got} but expected {want}"
            );
        }
    }

    #[test]
    fn nested_containers_each_pay_their_own_padding() {
        let rows = vec![
            group_row("g", true, None, 0, 0),
            group_row("sub", true, Some("g"), 0, 1),
            named_layer_row("deep", Some("sub"), 0, 2, false, true),
            named_layer_row("after", None, 1, 0, false, true),
        ];
        let layout = RowLayout::new(&rows);
        let after_gap = layout.top(3) - layout.bottom(2);
        assert!(
            (after_gap - (ITEM_GAP + NEST_PAD * 2.0)).abs() < f64::EPSILON,
            "both the inner and outer folder end on `deep`"
        );
    }

    #[test]
    fn the_gap_between_rows_belongs_to_no_row() {
        let rows = vec![
            group_row("g", true, None, 0, 0),
            named_layer_row("a", Some("g"), 0, 1, false, true),
        ];
        let layout = RowLayout::new(&rows);
        assert_eq!(layout.at(layout.top(0) + 1.0), Some(0));
        assert_eq!(layout.at(layout.top(1) + 1.0), Some(1));
        assert_eq!(
            layout.at(layout.bottom(0) + ITEM_GAP / 2.0),
            None,
            "a click in the gap hits nothing"
        );
        assert_eq!(layout.at(LIST_PADDING / 2.0), None, "nor does one above the list");
    }

    #[test]
    fn a_collapsed_folder_keeps_its_container() {
        let rows = vec![
            group_row("collapsed", false, None, 0, 0),
            named_layer_row("after", None, 1, 0, false, true),
        ];
        assert_eq!(contained_rows(&rows, 0), 0, "nothing visible inside it");
        assert!(opens_box(&rows, 0), "but it is still a container");
        assert_eq!(box_depth(&rows, 0), 1);
        assert_eq!(box_depth(&rows, 1), 0, "the row after it is not inside");
    }

    #[test]
    fn an_adjustment_covering_nothing_gets_no_container() {
        let mut adjustment = named_layer_row("adj", None, 0, 0, false, true);
        adjustment.adjust_indent = 0;
        let rows = vec![adjustment];
        assert!(!opens_box(&rows, 0));
        assert_eq!(box_depth(&rows, 0), 0);
    }

    #[test]
    fn a_box_stops_at_the_end_of_its_own_nesting() {
        let rows = vec![
            group_row("g", true, None, 0, 0),
            named_layer_row("inside", Some("g"), 0, 1, false, true),
            named_layer_row("outside", None, 1, 0, false, true),
        ];
        assert_eq!(contained_rows(&rows, 0), 1, "the root row below is not held");
        assert_eq!(box_depth(&rows, 2), 0);
    }

    #[test]
    fn adjustment_scope_nests_like_a_folder() {
        let mut adjustment = named_layer_row("adj", None, 0, 0, false, true);
        adjustment.adjust_indent = 0;
        let mut affected = named_layer_row("below", None, 1, 0, false, true);
        affected.adjust_indent = 1;
        let rows = vec![adjustment, affected];

        assert_eq!(contained_rows(&rows, 0), 1, "the adjustment covers the row below");
        assert_eq!(box_depth(&rows, 0), 1, "and is boxed together with it");
        assert_eq!(box_depth(&rows, 1), 1);
    }

    #[test]
    fn clip_bracket_spans_a_stack_and_corners_once() {
        let rows = vec![
            named_layer_row("c1", None, 0, 0, true, true),
            named_layer_row("c2", None, 1, 0, true, true),
            named_layer_row("base", None, 2, 0, false, true),
        ];

        let top = clip_info(&rows, 0);
        assert!(top.clipped && !top.clipped_above);
        assert!(top.clipped_below, "the run continues into c2");
        assert!(top.has_base);

        let bottom = clip_info(&rows, 1);
        assert!(bottom.clipped_above);
        assert!(!bottom.clipped_below, "c2 is the row that turns the corner");
        assert!(bottom.has_base);

        let base = clip_info(&rows, 2);
        assert!(!base.clipped);
        assert!(base.is_base, "the base underlines its name");
    }

    #[test]
    fn clip_without_a_base_is_inactive() {
        let rows = vec![named_layer_row("lonely", None, 0, 0, true, true)];
        let info = clip_info(&rows, 0);
        assert!(info.clipped);
        assert!(!info.has_base, "no corner is drawn without a base");
    }

    #[test]
    fn a_hidden_base_is_reported_so_its_stack_dims() {
        let rows = vec![
            named_layer_row("shade", None, 0, 0, true, true),
            named_layer_row("base", None, 1, 0, false, false),
        ];
        let info = clip_info(&rows, 0);
        assert!(info.has_base);
        assert!(info.base_hidden);
    }

    #[test]
    fn clipping_does_not_reach_across_sibling_lists() {
        let rows = vec![
            named_layer_row("inside", Some("g"), 0, 1, true, true),
            named_layer_row("outside", None, 1, 0, false, true),
        ];
        let info = clip_info(&rows, 0);
        assert!(info.clipped);
        assert!(!info.has_base);
        assert!(!clip_info(&rows, 1).is_base);
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

    fn core_group(id: &str, children: Vec<LayerTreeNode>) -> LayerTreeNode {
        LayerTreeNode::Group(LayerGroup {
            id: id.to_string(),
            name: id.to_string(),
            expanded: false,
            children,
        })
    }

    fn collect_group_ids(nodes: &[LayerNode], out: &mut Vec<String>) {
        for node in nodes {
            if let LayerNode::Group(g) = node {
                out.push(g.id.clone());
                collect_group_ids(&g.children, out);
            }
        }
    }

    #[test]
    fn tree_from_core_heals_duplicate_group_ids() {
        let core = vec![
            core_group("g0000000000000001", vec![]),
            core_group("g0000000000000002", vec![]),
            core_group("g0000000000000001", vec![]),
        ];
        let tree = tree_from_core(&core);

        let mut ids = Vec::new();
        collect_group_ids(&tree, &mut ids);
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(unique.len(), 3, "group ids should be unique after load: {ids:?}");
    }

    #[test]
    fn tree_from_core_heals_nested_duplicate_group_ids() {
        let core = vec![core_group(
            "g0000000000000001",
            vec![core_group("g0000000000000001", vec![])],
        )];
        let tree = tree_from_core(&core);

        let mut ids = Vec::new();
        collect_group_ids(&tree, &mut ids);
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "nested group ids should be unique: {ids:?}");
    }

    fn panel_group(id: &str, children: Vec<LayerNode>) -> LayerNode {
        LayerNode::Group(GroupData {
            id: id.to_string(),
            name: "G".into(),
            expanded: true,
            visible: true,
            children,
            masked_leaves: HashSet::new(),
        })
    }

    #[test]
    fn mirror_tree_drops_leaves_that_were_not_duplicated() {
        let src = vec![
            LayerNode::Layer("a".into()),
            panel_group("g1", vec![LayerNode::Layer("b".into()), LayerNode::Layer("c".into())]),
        ];
        let id_map: std::collections::HashMap<String, String> =
            [("a".to_string(), "a2".to_string()), ("c".to_string(), "c2".to_string())]
                .into_iter()
                .collect();

        let mirror = mirror_tree(&src, &id_map);
        assert_eq!(leaf_ids_top_first(&mirror), vec!["a2".to_string(), "c2".to_string()]);
    }

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
        let rows = vec![
            group_row("outer", true, None, 0, 0),
            group_row("inner", true, Some("outer"), 0, 1),
            layer_row(Some("inner"), 0, 2),
            layer_row(None, 1, 0),
        ];
        assert_eq!(drag_span(&rows, 0), 3);
        assert_eq!(drag_span(&rows, 1), 2);
        assert_eq!(drag_span(&rows, 3), 1);
    }

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
        let before = ids(&["a", "b", "c", "d"]);
        let after = ids(&["c", "a", "b", "d"]);
        let steps = reorder_steps(&before, &after);
        assert_eq!(apply_steps(&before, &steps), after);
    }

    #[test]
    fn displaced_row_no_movement() {
        for i in 0..5 {
            assert_eq!(displaced_row(i, 2, 1, 2), i);
        }
    }

    #[test]
    fn displaced_row_move_down() {
        assert_eq!(displaced_row(0, 1, 1, 3), 0);
        assert_eq!(displaced_row(2, 1, 1, 3), 1);
        assert_eq!(displaced_row(3, 1, 1, 3), 2);
        assert_eq!(displaced_row(4, 1, 1, 3), 4);
    }

    #[test]
    fn displaced_row_move_up() {
        assert_eq!(displaced_row(0, 3, 1, 1), 0);
        assert_eq!(displaced_row(1, 3, 1, 1), 2);
        assert_eq!(displaced_row(2, 3, 1, 1), 3);
        assert_eq!(displaced_row(4, 3, 1, 1), 4);
    }

    #[test]
    fn displaced_row_group_span_move_down() {
        assert_eq!(displaced_row(2, 0, 2, 2), 0);
        assert_eq!(displaced_row(3, 0, 2, 2), 1);
    }

    #[test]
    fn resolve_prepend_at_row_zero() {
        let rows = vec![layer_row(None, 0, 0)];
        let (parent, idx) = resolve_insert_target(&rows, 0);
        assert_eq!(parent, None);
        assert_eq!(idx, usize::MAX);
    }

    #[test]
    fn resolve_after_plain_layer() {
        let rows = vec![layer_row(None, 0, 0)];
        let (parent, idx) = resolve_insert_target(&rows, 1);
        assert_eq!(parent, None);
        assert_eq!(idx, 0);
    }

    #[test]
    fn resolve_first_child_after_expanded_group_header() {
        let rows = vec![
            group_row("g1", true, None, 0, 0),
            layer_row(Some("g1"), 0, 1),
        ];
        let (parent, idx) = resolve_insert_target(&rows, 1);
        assert_eq!(parent, Some("g1".to_owned()));
        assert_eq!(idx, usize::MAX);
    }

    #[test]
    fn resolve_escape_group_when_above_is_last_child() {
        let rows = vec![
            group_row("g1", true, None, 0, 0),
            layer_row(Some("g1"), 0, 1),
            layer_row(None, 1, 0),
        ];
        let (parent, idx) = resolve_insert_target(&rows, 2);
        assert_eq!(parent, None);
        assert_eq!(idx, 0);
    }

    #[test]
    fn resolve_inside_group_middle_child() {
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

    fn ui_group(id: &str, children: Vec<LayerNode>) -> LayerNode {
        LayerNode::Group(GroupData {
            id: id.to_string(),
            name: id.to_string(),
            expanded: true,
            visible: true,
            children,
            masked_leaves: HashSet::new(),
        })
    }

    #[test]
    fn collect_top_selected_keeps_subgroups_intact() {
        let tree = vec![
            LayerNode::Layer("a".into()),
            ui_group("g", vec![LayerNode::Layer("c".into()), LayerNode::Layer("d".into())]),
            LayerNode::Layer("b".into()),
        ];
        let wanted: HashSet<String> =
            ["a".to_string(), "g".to_string(), "b".to_string()].into_iter().collect();
        let mut out = Vec::new();
        collect_top_selected(&tree, &wanted, &mut out);
        assert_eq!(out, vec!["a".to_string(), "g".to_string(), "b".to_string()]);
    }

    #[test]
    fn collect_top_selected_skips_nested_selection_under_selected_group() {
        let tree = vec![ui_group(
            "g",
            vec![LayerNode::Layer("c".into()), LayerNode::Layer("d".into())],
        )];
        let wanted: HashSet<String> =
            ["g".to_string(), "c".to_string()].into_iter().collect();
        let mut out = Vec::new();
        collect_top_selected(&tree, &wanted, &mut out);
        assert_eq!(out, vec!["g".to_string()]);
    }

    #[test]
    fn group_nodes_wraps_subgroup_intact() {
        let mut tree = vec![
            LayerNode::Layer("a".into()),
            ui_group("g", vec![LayerNode::Layer("c".into()), LayerNode::Layer("d".into())]),
            LayerNode::Layer("b".into()),
        ];
        group_nodes(&mut tree, &["a".into(), "g".into(), "b".into()], "Wrap");

        assert_eq!(tree.len(), 1, "all selected nodes collapse into one new group");
        let LayerNode::Group(outer) = &tree[0] else {
            panic!("expected a group at root");
        };
        assert_eq!(outer.name, "Wrap");
        assert_eq!(outer.children.len(), 3);
        assert!(matches!(&outer.children[0], LayerNode::Layer(id) if id == "a"));
        let LayerNode::Group(inner) = &outer.children[1] else {
            panic!("subgroup g should survive as a group, not be flattened");
        };
        assert_eq!(inner.id, "g");
        assert_eq!(inner.children.len(), 2);
        assert!(matches!(&inner.children[0], LayerNode::Layer(id) if id == "c"));
        assert!(matches!(&inner.children[1], LayerNode::Layer(id) if id == "d"));
        assert!(matches!(&outer.children[2], LayerNode::Layer(id) if id == "b"));
    }
}
