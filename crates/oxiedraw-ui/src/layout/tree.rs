use serde::{Deserialize, Serialize};

use super::panel::{Axis, DockSide, PanelId, SplitChild};

// A panel's dock side is never stored: it is read back from the nearest split
// that separates the panel from the canvas, so a move is a structural edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LayoutNode {
    Split {
        axis: Axis,
        fixed: SplitChild,
        size: i32,
        first: Box<Self>,
        second: Box<Self>,
    },
    Panel(PanelId),
    Canvas,
}

impl LayoutNode {
    pub(crate) fn dock(panel: PanelId, side: DockSide, rest: Self) -> Self {
        Self::dock_sized(panel, side, panel.spec().default_size, rest)
    }

    pub(crate) fn dock_sized(panel: PanelId, side: DockSide, px: i32, rest: Self) -> Self {
        let leaf = Self::Panel(panel);
        let (first, second) = if side.child() == SplitChild::First {
            (leaf, rest)
        } else {
            (rest, leaf)
        };
        Self::Split {
            axis: side.axis(),
            fixed: side.child(),
            size: px,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    pub(crate) fn walk(&self, visit: &mut impl FnMut(&Self)) {
        visit(self);
        if let Self::Split { first, second, .. } = self {
            first.walk(visit);
            second.walk(visit);
        }
    }

    pub(crate) fn panels(&self) -> Vec<PanelId> {
        let mut out = Vec::new();
        self.walk(&mut |node| {
            if let Self::Panel(id) = node {
                out.push(*id);
            }
        });
        out
    }

    pub(crate) fn contains_panel(&self, id: PanelId) -> bool {
        let mut found = false;
        self.walk(&mut |node| found |= matches!(node, Self::Panel(p) if *p == id));
        found
    }

    pub(crate) fn contains_canvas(&self) -> bool {
        let mut found = false;
        self.walk(&mut |node| found |= matches!(node, Self::Canvas));
        found
    }

    fn count_canvas(&self) -> usize {
        let mut n = 0;
        self.walk(&mut |node| {
            if matches!(node, Self::Canvas) {
                n += 1;
            }
        });
        n
    }

    fn count_panel(&self, id: PanelId) -> usize {
        let mut n = 0;
        self.walk(&mut |node| {
            if matches!(node, Self::Panel(p) if *p == id) {
                n += 1;
            }
        });
        n
    }

    pub(crate) fn side_of(&self, id: PanelId) -> Option<DockSide> {
        let Self::Split {
            axis,
            first,
            second,
            ..
        } = self
        else {
            return None;
        };
        let in_first = first.contains_panel(id);
        if !in_first && !second.contains_panel(id) {
            return None;
        }
        let (holding, other, child) = if in_first {
            (first, second, SplitChild::First)
        } else {
            (second, first, SplitChild::Second)
        };
        if other.contains_canvas() {
            return Some(axis.side(child));
        }
        holding.side_of(id)
    }

    pub(crate) fn remove_panel(&mut self, id: PanelId) -> bool {
        let Self::Split { first, second, .. } = self else {
            return false;
        };
        let leaf = Self::Panel(id);
        let keep_second = **first == leaf;
        let keep_first = !keep_second && **second == leaf;
        if keep_second || keep_first {
            let taken = std::mem::replace(self, Self::Canvas);
            if let Self::Split { first, second, .. } = taken {
                *self = if keep_second { *second } else { *first };
            }
            return true;
        }
        first.remove_panel(id) || second.remove_panel(id)
    }

    fn split_at_mut(&mut self, path: &[SplitChild]) -> Option<&mut Self> {
        let Some((turn, rest)) = path.split_first() else {
            return matches!(self, Self::Split { .. }).then_some(self);
        };
        let Self::Split { first, second, .. } = self else {
            return None;
        };
        match turn {
            SplitChild::First => first.split_at_mut(rest),
            SplitChild::Second => second.split_at_mut(rest),
        }
    }

    pub(crate) fn set_split_size(&mut self, path: &[SplitChild], px: i32) -> bool {
        let px = px.max(0);
        let Some(Self::Split { size, .. }) = self.split_at_mut(path) else {
            return false;
        };
        if *size == px {
            return false;
        }
        *size = px;
        true
    }

    fn leaf_mut(&mut self, leaf: &Self) -> Option<&mut Self> {
        if self == leaf {
            return Some(self);
        }
        let Self::Split { first, second, .. } = self else {
            return None;
        };
        first
            .leaf_mut(leaf)
            .or_else(|| second.leaf_mut(leaf))
    }

    pub(crate) fn dock_panel(&mut self, id: PanelId, side: DockSide) {
        self.remove_panel(id);
        let rest = std::mem::replace(self, Self::Canvas);
        *self = Self::dock(id, side, rest);
    }

    pub(crate) fn split_leaf(&mut self, target: &Self, id: PanelId, side: DockSide) -> bool {
        if *target == Self::Panel(id) || self.leaf_mut(target).is_none() {
            return false;
        }
        self.remove_panel(id);
        let Some(slot) = self.leaf_mut(target) else {
            return false;
        };
        let existing = std::mem::replace(slot, Self::Canvas);
        *slot = Self::dock(id, side, existing);
        true
    }

    pub(crate) fn from_defaults(panels: &[PanelId]) -> Self {
        let mut root = Self::Canvas;
        for id in panels {
            root.dock_panel(*id, id.spec().default_side);
        }
        root
    }

    fn clamp_sizes(&mut self) -> bool {
        let Self::Split {
            size, first, second, ..
        } = self
        else {
            return false;
        };
        let corrected = *size < 0;
        if corrected {
            *size = 0;
        }
        corrected | first.clamp_sizes() | second.clamp_sizes()
    }

    pub(crate) fn sanitize(&mut self) -> bool {
        let mut changed = self.clamp_sizes();

        for id in self.panels() {
            while self.count_panel(id) > 1 {
                self.remove_panel(id);
                changed = true;
            }
        }

        if self.count_canvas() != 1 {
            let panels = self.panels();
            tracing::warn!(
                canvases = self.count_canvas(),
                "layout tree has no single canvas; rebuilding from panel defaults"
            );
            *self = Self::from_defaults(&panels);
            return true;
        }

        // Looped: every removal collapses a split, which can make a placement
        // that was legal illegal.
        while let Some(id) = self.panels().into_iter().find(|id| !self.is_placed(*id)) {
            tracing::warn!(?id, "panel placed where no layout could put it; dropping it");
            self.remove_panel(id);
            changed = true;
        }
        changed
    }

    fn is_placed(&self, id: PanelId) -> bool {
        if !self.side_of(id).is_some_and(|side| id.spec().allows(side)) {
            return false;
        }
        id.spec().splittable || self.sibling_of(id).is_some_and(Self::contains_canvas)
    }

    fn sibling_of(&self, id: PanelId) -> Option<&Self> {
        let Self::Split { first, second, .. } = self else {
            return None;
        };
        if **first == Self::Panel(id) {
            return Some(second);
        }
        if **second == Self::Panel(id) {
            return Some(first);
        }
        first.sibling_of(id).or_else(|| second.sibling_of(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(axis: Axis, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            axis,
            fixed: SplitChild::First,
            size: 200,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn docking_puts_the_panel_on_the_side_it_was_asked_for() {
        let mut root = LayoutNode::Canvas;
        root.dock_panel(PanelId::Layers, DockSide::Right);
        root.dock_panel(PanelId::ToolBar, DockSide::Left);
        root.dock_panel(PanelId::ToolOptions, DockSide::Top);

        assert_eq!(root.side_of(PanelId::Layers), Some(DockSide::Right));
        assert_eq!(root.side_of(PanelId::ToolBar), Some(DockSide::Left));
        assert_eq!(root.side_of(PanelId::ToolOptions), Some(DockSide::Top));
    }

    #[test]
    fn nested_panels_report_the_side_of_the_branch_they_are_in() {
        let column = split(
            Axis::Vertical,
            LayoutNode::Panel(PanelId::ColorPicker),
            LayoutNode::Panel(PanelId::Layers),
        );
        let root = split(Axis::Horizontal, LayoutNode::Canvas, column);

        assert_eq!(root.side_of(PanelId::ColorPicker), Some(DockSide::Right));
        assert_eq!(root.side_of(PanelId::Layers), Some(DockSide::Right));
    }

    #[test]
    fn removing_a_panel_collapses_its_split_onto_the_sibling() {
        let mut root = LayoutNode::Canvas;
        root.dock_panel(PanelId::Layers, DockSide::Right);
        root.dock_panel(PanelId::ToolBar, DockSide::Left);

        assert!(root.remove_panel(PanelId::ToolBar));
        assert!(!root.contains_panel(PanelId::ToolBar));
        assert!(root.contains_canvas(), "the canvas survives the collapse");
        assert_eq!(root.side_of(PanelId::Layers), Some(DockSide::Right));
        assert!(!root.remove_panel(PanelId::ToolBar), "already gone");
    }

    #[test]
    fn moving_a_panel_does_not_leave_a_copy_behind() {
        let mut root = LayoutNode::Canvas;
        root.dock_panel(PanelId::Layers, DockSide::Right);
        root.dock_panel(PanelId::Layers, DockSide::Left);

        assert_eq!(root.panels(), vec![PanelId::Layers]);
        assert_eq!(root.side_of(PanelId::Layers), Some(DockSide::Left));
    }

    #[test]
    fn sanitize_drops_a_bar_made_to_share_its_area() {
        assert!(!PanelId::ToolOptions.spec().splittable);
        let mut root = split(
            Axis::Vertical,
            split(
                Axis::Horizontal,
                LayoutNode::Panel(PanelId::Layers),
                LayoutNode::Panel(PanelId::ToolOptions),
            ),
            LayoutNode::Canvas,
        );

        assert!(root.sanitize());
        assert!(!root.contains_panel(PanelId::ToolOptions));
        assert!(root.contains_canvas(), "the canvas is never the casualty");
    }

    #[test]
    fn sanitize_keeps_a_bar_that_spans_the_window() {
        let mut root = LayoutNode::Canvas;
        root.dock_panel(PanelId::ToolOptions, DockSide::Top);
        root.dock_panel(PanelId::Layers, DockSide::Right);
        let before = root.clone();

        assert!(!root.sanitize(), "this is what docking produces");
        assert_eq!(root, before);
    }

    #[test]
    fn splitting_against_a_leaf_that_is_not_there_changes_nothing() {
        let mut root = LayoutNode::Canvas;
        root.dock_panel(PanelId::Layers, DockSide::Right);
        let before = root.clone();

        let missing = LayoutNode::Panel(PanelId::ToolOptions);
        assert!(!root.split_leaf(&missing, PanelId::Layers, DockSide::Top));
        assert_eq!(root, before, "a refused split must not eat the panel");
    }

    #[test]
    fn splitting_a_panel_shares_its_area() {
        let mut root = LayoutNode::Canvas;
        root.dock_panel(PanelId::Layers, DockSide::Right);

        assert!(root.split_leaf(
            &LayoutNode::Panel(PanelId::Layers),
            PanelId::ColorPicker,
            DockSide::Top,
        ));
        assert_eq!(root.side_of(PanelId::ColorPicker), Some(DockSide::Right));
        assert_eq!(root.side_of(PanelId::Layers), Some(DockSide::Right));

        let LayoutNode::Split { second, .. } = &root else {
            panic!("expected the canvas/sidebar split")
        };
        let LayoutNode::Split { axis, first, .. } = second.as_ref() else {
            panic!("expected the sidebar to be split in two")
        };
        assert_eq!(*axis, Axis::Vertical);
        assert_eq!(**first, LayoutNode::Panel(PanelId::ColorPicker));
    }

    #[test]
    fn splitting_the_canvas_docks_against_the_document_area() {
        let mut root = LayoutNode::Canvas;
        root.dock_panel(PanelId::Layers, DockSide::Right);

        assert!(root.split_leaf(&LayoutNode::Canvas, PanelId::CanvasInfo, DockSide::Bottom));
        assert_eq!(root.side_of(PanelId::CanvasInfo), Some(DockSide::Bottom));
        assert_eq!(
            root.side_of(PanelId::Layers),
            Some(DockSide::Right),
            "the sidebar is unaffected"
        );
    }

    #[test]
    fn a_panel_cannot_be_split_against_itself() {
        let mut root = LayoutNode::Canvas;
        root.dock_panel(PanelId::Layers, DockSide::Right);
        assert!(!root.split_leaf(
            &LayoutNode::Panel(PanelId::Layers),
            PanelId::Layers,
            DockSide::Top
        ));
        assert_eq!(root.panels(), vec![PanelId::Layers]);
    }

    #[test]
    fn sanitize_drops_duplicate_panels() {
        let mut root = split(
            Axis::Horizontal,
            LayoutNode::Panel(PanelId::Layers),
            split(
                Axis::Horizontal,
                LayoutNode::Canvas,
                LayoutNode::Panel(PanelId::Layers),
            ),
        );
        assert!(root.sanitize());
        assert_eq!(root.count_panel(PanelId::Layers), 1);
        assert!(root.contains_canvas());
    }

    #[test]
    fn sanitize_drops_a_panel_docked_where_its_spec_forbids() {
        let mut root = split(
            Axis::Horizontal,
            LayoutNode::Panel(PanelId::ToolOptions),
            LayoutNode::Canvas,
        );
        assert!(root.sanitize());
        assert!(!root.contains_panel(PanelId::ToolOptions));
        assert_eq!(root, LayoutNode::Canvas);
    }

    #[test]
    fn sanitize_rebuilds_a_tree_with_no_canvas() {
        let mut root = split(
            Axis::Horizontal,
            LayoutNode::Panel(PanelId::ToolBar),
            LayoutNode::Panel(PanelId::Layers),
        );
        assert!(root.sanitize());
        assert_eq!(root.count_canvas(), 1);
        assert_eq!(root.side_of(PanelId::ToolBar), Some(DockSide::Left));
        assert_eq!(root.side_of(PanelId::Layers), Some(DockSide::Right));
    }

    #[test]
    fn sanitize_leaves_a_well_formed_tree_alone() {
        let mut root = LayoutNode::from_defaults(&[PanelId::ToolBar, PanelId::Layers]);
        let before = root.clone();
        assert!(!root.sanitize());
        assert_eq!(root, before);
    }

    #[test]
    fn negative_split_sizes_are_refused_and_repaired() {
        let mut root = LayoutNode::from_defaults(&[PanelId::Layers]);
        root.set_split_size(&[], -40);
        let LayoutNode::Split { size, .. } = &root else {
            panic!("expected a split")
        };
        assert_eq!(*size, 0, "stored sizes never go below zero");

        let mut stored = split(
            Axis::Horizontal,
            LayoutNode::Canvas,
            LayoutNode::Panel(PanelId::Layers),
        );
        if let LayoutNode::Split { size, .. } = &mut stored {
            *size = -1234;
        }
        assert!(stored.sanitize(), "a bad size on load is a repair");
        let LayoutNode::Split { size, .. } = &stored else {
            panic!("expected a split")
        };
        assert_eq!(*size, 0);
    }

    #[test]
    fn a_split_size_is_addressed_by_the_path_to_it() {
        let mut root = LayoutNode::from_defaults(&[PanelId::Layers, PanelId::ToolBar]);
        assert!(root.set_split_size(&[], 64), "the root split is the empty path");
        assert!(root.set_split_size(&[SplitChild::Second], 280));
        assert!(
            !root.set_split_size(&[SplitChild::Second], 280),
            "storing the same size again is not a change"
        );
        assert!(
            !root.set_split_size(&[SplitChild::First], 100),
            "the tool bar leaf is not a split"
        );

        let LayoutNode::Split { size, second, .. } = &root else {
            panic!("expected a split at the root")
        };
        assert_eq!(*size, 64);
        let LayoutNode::Split { size, .. } = second.as_ref() else {
            panic!("expected the inner split")
        };
        assert_eq!(*size, 280);
    }

    #[test]
    fn trees_round_trip_through_json() {
        let root = LayoutNode::from_defaults(&[PanelId::ToolBar, PanelId::ToolOptions]);
        let json = serde_json::to_string(&root).expect("serialise");
        let back: LayoutNode = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, root);
    }
}
