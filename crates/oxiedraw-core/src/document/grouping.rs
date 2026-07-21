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
/// `Layer` carries a renderer slot index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeStep {
    Layer(usize),
    EnterGroup,
    ExitGroup,
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
pub fn build_composite_steps(
    tree: &[LayerTreeNode],
    resolve: &impl Fn(&str) -> Option<usize>,
    visible: &[usize],
) -> Vec<CompositeStep> {
    let mut steps = Vec::new();
    append_steps(tree, resolve, &mut steps);

    let covered: std::collections::HashSet<usize> = steps
        .iter()
        .filter_map(|s| match s {
            CompositeStep::Layer(i) => Some(*i),
            _ => None,
        })
        .collect();
    for &idx in visible {
        if !covered.contains(&idx) {
            steps.push(CompositeStep::Layer(idx));
        }
    }
    steps
}

// Emit steps for a sibling list, skipping folders with no visible content.
fn append_steps(
    nodes: &[LayerTreeNode],
    resolve: &impl Fn(&str) -> Option<usize>,
    out: &mut Vec<CompositeStep>,
) {
    for node in nodes {
        match node {
            LayerTreeNode::Layer { id } => {
                if let Some(idx) = resolve(id) {
                    out.push(CompositeStep::Layer(idx));
                }
            }
            LayerTreeNode::Group(g) => {
                let mut inner = Vec::new();
                append_steps(&g.children, resolve, &mut inner);
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

    #[test]
    fn flat_tree_has_no_group_markers() {
        let tree = vec![LayerTreeNode::layer("a"), LayerTreeNode::layer("b")];
        let resolve = |id: &str| match id {
            "a" => Some(0),
            "b" => Some(1),
            _ => None,
        };
        let steps = build_composite_steps(&tree, &resolve, &[0, 1]);
        assert_eq!(steps, vec![CompositeStep::Layer(0), CompositeStep::Layer(1)]);
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
        let steps = build_composite_steps(&tree, &resolve, &[0, 1, 2, 3]);
        assert_eq!(
            steps,
            vec![
                CompositeStep::Layer(0),
                CompositeStep::EnterGroup,
                CompositeStep::Layer(1),
                CompositeStep::Layer(2),
                CompositeStep::ExitGroup,
                CompositeStep::Layer(3),
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
        let steps = build_composite_steps(&tree, &resolve, &[0]);
        assert_eq!(steps, vec![CompositeStep::Layer(0)]);
    }

    #[test]
    fn stale_tree_appends_uncovered_layers_at_root() {
        // Stack has indices 0 and 1 visible, but the tree only knows "a" (index
        // 0). Index 1 was just added; it composites at the root, on top.
        let tree = vec![LayerTreeNode::layer("a")];
        let resolve = |id: &str| if id == "a" { Some(0) } else { None };
        let steps = build_composite_steps(&tree, &resolve, &[0, 1]);
        assert_eq!(steps, vec![CompositeStep::Layer(0), CompositeStep::Layer(1)]);
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
        let steps = build_composite_steps(&tree, &resolve, &[0, 1]);
        assert_eq!(
            steps,
            vec![
                CompositeStep::EnterGroup,
                CompositeStep::Layer(0),
                CompositeStep::EnterGroup,
                CompositeStep::Layer(1),
                CompositeStep::ExitGroup,
                CompositeStep::ExitGroup,
            ]
        );
    }
}
