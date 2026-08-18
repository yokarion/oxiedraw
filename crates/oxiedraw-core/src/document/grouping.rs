//! Folder (group) structure layered over the flat z-ordered layer stack.
//!
//! Compositing and layer indexing stay flat; this tree only records how leaves
//! are grouped into folders so an adjustment layer can be scoped to its
//! enclosing folder instead of the whole canvas. Children are listed in canvas
//! order (bottom-to-top), matching layer index 0 = bottom of the stack.

use serde::{Deserialize, Serialize};

/// A node in the folder tree: either a leaf (a layer, by id) or a folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "lowercase")]
pub enum LayerTreeNode {
    Layer { id: String },
    Group(LayerGroup),
}

/// A folder: a named, collapsible scope wrapping an ordered list of children.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerGroup {
    pub id: String,
    pub name: String,
    #[serde(default = "crate::serde_defaults::default_true")]
    pub expanded: bool,
    pub children: Vec<LayerTreeNode>,
}

impl LayerTreeNode {
    /// Convenience constructor for a leaf node.
    #[must_use]
    pub fn layer(id: impl Into<String>) -> Self {
        Self::Layer { id: id.into() }
    }
}

/// Append every leaf id in `nodes`, in canvas order (bottom-to-top).
pub fn collect_leaf_ids(nodes: &[LayerTreeNode], out: &mut Vec<String>) {
    for node in nodes {
        match node {
            LayerTreeNode::Layer { id } => out.push(id.clone()),
            LayerTreeNode::Group(g) => collect_leaf_ids(&g.children, out),
        }
    }
}

/// A flattened composite instruction. `EnterGroup`/`ExitGroup` bracket a
/// folder's contents so the compositor can give it its own sub-accumulator;
/// `Layer` carries a renderer slot index plus, for a clipped layer, the slot
/// its content is masked by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeStep {
    Layer {
        idx: usize,
        /// Slot index of the clipping base: the nearest non-clipped sibling
        /// below. `None` for an ordinary (unclipped) layer.
        clip_base: Option<usize>,
    },
    EnterGroup,
    ExitGroup,
}

impl CompositeStep {
    /// An unclipped layer step.
    #[must_use]
    pub const fn layer(idx: usize) -> Self {
        Self::Layer { idx, clip_base: None }
    }

    /// The slot this step draws, if it draws one.
    #[must_use]
    pub const fn layer_index(self) -> Option<usize> {
        match self {
            Self::Layer { idx, .. } => Some(idx),
            _ => None,
        }
    }
}

/// Flatten `tree` into composite steps, resolving each leaf id to a slot index
/// via `resolve` (returns `None` for hidden or unknown layers, which are
/// dropped). Folders that end up with no visible content emit no group markers,
/// so empty folders cost nothing at composite time.
///
/// `visible` is the full set of visible slot indices in canvas order. Any one
/// the tree did not cover (a stale tree - e.g. a just-added layer the UI has
/// not pushed yet) is appended at the root, on top, in canvas order. So the
/// composite is always complete and correct even when the tree lags the stack;
/// folder scoping just may not apply to the not-yet-tracked layer.
/// `clipped` reports whether a leaf carries the clipping-mask flag. Its base is
/// the nearest non-clipped leaf below it in the same sibling list, resolved
/// here rather than in the renderer because the base has to be found *before*
/// hidden layers are dropped: a hidden base takes its whole clip stack with it.
pub fn build_composite_steps(
    tree: &[LayerTreeNode],
    resolve: &impl Fn(&str) -> Option<usize>,
    clipped: &impl Fn(&str) -> bool,
    visible: &[usize],
) -> Vec<CompositeStep> {
    let mut steps = Vec::new();
    let mut dropped = std::collections::HashSet::new();
    append_steps(tree, resolve, clipped, &mut steps, &mut dropped);

    let mut covered: std::collections::HashSet<usize> =
        steps.iter().filter_map(|s| s.layer_index()).collect();
    // A clipped layer whose base did not resolve was dropped on purpose. It
    // must not come back through the stale-tree fallback below, which would
    // composite it unclipped over everything.
    covered.extend(dropped);
    for &idx in visible {
        if !covered.contains(&idx) {
            steps.push(CompositeStep::layer(idx));
        }
    }
    steps
}

// Emit steps for a sibling list, skipping folders with no visible content.
//
// `base` tracks the nearest non-clipped leaf below the current node, by id, so
// a run of clipped leaves all resolve to the same base. A folder clears it: a
// folder cannot serve as a clip base (there is no single slot whose alpha would
// be the mask), so a layer clipped directly above one has no base and does not
// draw.
fn append_steps(
    nodes: &[LayerTreeNode],
    resolve: &impl Fn(&str) -> Option<usize>,
    clipped: &impl Fn(&str) -> bool,
    out: &mut Vec<CompositeStep>,
    dropped: &mut std::collections::HashSet<usize>,
) {
    let mut base: Option<&str> = None;
    for node in nodes {
        match node {
            LayerTreeNode::Layer { id } => {
                if !clipped(id) {
                    if let Some(idx) = resolve(id) {
                        out.push(CompositeStep::layer(idx));
                    }
                    // A hidden layer is still the base for what clips to it -
                    // resolving that base then fails, which is what hides the
                    // stack along with it.
                    base = Some(id);
                    continue;
                }
                // Clipped: draw only when both it and its base are visible.
                if let Some(idx) = resolve(id) {
                    match base.and_then(resolve) {
                        Some(base_idx) => out.push(CompositeStep::Layer {
                            idx,
                            clip_base: Some(base_idx),
                        }),
                        None => {
                            dropped.insert(idx);
                        }
                    }
                }
            }
            LayerTreeNode::Group(g) => {
                let mut inner = Vec::new();
                append_steps(&g.children, resolve, clipped, &mut inner, dropped);
                base = None;
                if inner.is_empty() {
                    continue;
                }
                out.push(CompositeStep::EnterGroup);
                out.extend(inner);
                out.push(CompositeStep::ExitGroup);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, children: Vec<LayerTreeNode>) -> LayerTreeNode {
        LayerTreeNode::Group(LayerGroup {
            id: id.to_string(),
            name: id.to_string(),
            expanded: true,
            children,
        })
    }

    // Nothing is clipped, for the tests that only care about folder structure.
    fn no_clips(_: &str) -> bool {
        false
    }

    #[test]
    fn flat_tree_has_no_group_markers() {
        let tree = vec![LayerTreeNode::layer("a"), LayerTreeNode::layer("b")];
        let resolve = |id: &str| match id {
            "a" => Some(0),
            "b" => Some(1),
            _ => None,
        };
        let steps = build_composite_steps(&tree, &resolve, &no_clips, &[0, 1]);
        assert_eq!(
            steps,
            vec![CompositeStep::layer(0), CompositeStep::layer(1)]
        );
    }

    #[test]
    fn group_brackets_its_children() {
        // bottom: a, then folder{b, adj}, then top: c
        let tree = vec![
            LayerTreeNode::layer("a"),
            group("g", vec![LayerTreeNode::layer("b"), LayerTreeNode::layer("adj")]),
            LayerTreeNode::layer("c"),
        ];
        let resolve = |id: &str| match id {
            "a" => Some(0),
            "b" => Some(1),
            "adj" => Some(2),
            "c" => Some(3),
            _ => None,
        };
        let steps = build_composite_steps(&tree, &resolve, &no_clips, &[0, 1, 2, 3]);
        assert_eq!(
            steps,
            vec![
                CompositeStep::layer(0),
                CompositeStep::EnterGroup,
                CompositeStep::layer(1),
                CompositeStep::layer(2),
                CompositeStep::ExitGroup,
                CompositeStep::layer(3),
            ]
        );
    }

    #[test]
    fn hidden_only_group_emits_nothing() {
        let tree = vec![
            LayerTreeNode::layer("a"),
            group("g", vec![LayerTreeNode::layer("hidden")]),
        ];
        // "hidden" resolves to None; group is empty -> no markers. Covered = {0}.
        let resolve = |id: &str| if id == "a" { Some(0) } else { None };
        let steps = build_composite_steps(&tree, &resolve, &no_clips, &[0]);
        assert_eq!(steps, vec![CompositeStep::layer(0)]);
    }

    #[test]
    fn stale_tree_appends_uncovered_layers_at_root() {
        // Stack has indices 0 and 1 visible, but the tree only knows "a" (index
        // 0). Index 1 was just added; it composites at the root, on top.
        let tree = vec![LayerTreeNode::layer("a")];
        let resolve = |id: &str| if id == "a" { Some(0) } else { None };
        let steps = build_composite_steps(&tree, &resolve, &no_clips, &[0, 1]);
        assert_eq!(
            steps,
            vec![CompositeStep::layer(0), CompositeStep::layer(1)]
        );
    }

    #[test]
    fn nested_groups_bracket_each_level() {
        let tree = vec![group(
            "outer",
            vec![
                LayerTreeNode::layer("a"),
                group("inner", vec![LayerTreeNode::layer("b")]),
            ],
        )];
        let resolve = |id: &str| match id {
            "a" => Some(0),
            "b" => Some(1),
            _ => None,
        };
        let steps = build_composite_steps(&tree, &resolve, &no_clips, &[0, 1]);
        assert_eq!(
            steps,
            vec![
                CompositeStep::EnterGroup,
                CompositeStep::layer(0),
                CompositeStep::EnterGroup,
                CompositeStep::layer(1),
                CompositeStep::ExitGroup,
                CompositeStep::ExitGroup,
            ]
        );
    }

    #[test]
    fn clipped_layer_binds_to_the_layer_below() {
        let tree = vec![LayerTreeNode::layer("base"), LayerTreeNode::layer("shade")];
        let resolve = |id: &str| match id {
            "base" => Some(0),
            "shade" => Some(1),
            _ => None,
        };
        let steps = build_composite_steps(&tree, &resolve, &|id| id == "shade", &[0, 1]);
        assert_eq!(
            steps,
            vec![
                CompositeStep::layer(0),
                CompositeStep::Layer {
                    idx: 1,
                    clip_base: Some(0)
                },
            ]
        );
    }

    #[test]
    fn a_clip_stack_shares_one_base() {
        let tree = vec![
            LayerTreeNode::layer("base"),
            LayerTreeNode::layer("c1"),
            LayerTreeNode::layer("c2"),
            LayerTreeNode::layer("c3"),
        ];
        let resolve = |id: &str| match id {
            "base" => Some(0),
            "c1" => Some(1),
            "c2" => Some(2),
            "c3" => Some(3),
            _ => None,
        };
        let steps = build_composite_steps(&tree, &resolve, &|id| id != "base", &[0, 1, 2, 3]);
        assert_eq!(
            steps,
            vec![
                CompositeStep::layer(0),
                CompositeStep::Layer { idx: 1, clip_base: Some(0) },
                CompositeStep::Layer { idx: 2, clip_base: Some(0) },
                CompositeStep::Layer { idx: 3, clip_base: Some(0) },
            ]
        );
    }

    #[test]
    fn hiding_the_base_drops_its_whole_clip_stack() {
        let tree = vec![
            LayerTreeNode::layer("base"),
            LayerTreeNode::layer("c1"),
            LayerTreeNode::layer("c2"),
        ];
        // "base" is hidden, so it does not resolve; c1/c2 still resolve.
        let resolve = |id: &str| match id {
            "c1" => Some(1),
            "c2" => Some(2),
            _ => None,
        };
        let steps = build_composite_steps(&tree, &resolve, &|id| id != "base", &[1, 2]);
        assert!(steps.is_empty(), "clipped layers follow their base: {steps:?}");
    }

    #[test]
    fn a_clipped_layer_with_nothing_below_does_not_draw() {
        let tree = vec![LayerTreeNode::layer("lonely"), LayerTreeNode::layer("top")];
        let resolve = |id: &str| match id {
            "lonely" => Some(0),
            "top" => Some(1),
            _ => None,
        };
        // The bottom-most layer is clipped: no base exists, so it is skipped,
        // and "top" (unclipped) still composites normally.
        let steps = build_composite_steps(&tree, &resolve, &|id| id == "lonely", &[0, 1]);
        assert_eq!(steps, vec![CompositeStep::layer(1)]);
    }

    #[test]
    fn clipping_resolves_within_the_folder_only() {
        // A clipped layer inside a folder cannot reach a base outside it.
        let tree = vec![
            LayerTreeNode::layer("outside"),
            group("g", vec![LayerTreeNode::layer("inside")]),
        ];
        let resolve = |id: &str| match id {
            "outside" => Some(0),
            "inside" => Some(1),
            _ => None,
        };
        let steps = build_composite_steps(&tree, &resolve, &|id| id == "inside", &[0, 1]);
        assert_eq!(steps, vec![CompositeStep::layer(0)]);
    }
}
