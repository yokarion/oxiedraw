
use relm4::gtk::graphene;

use crate::layout::{Corner, DockSide, FloatAnchor, Layout, LayoutNode, PanelId};

const EDGE_ZONE: f32 = 28.0;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DropTarget {
    Edge(DockSide),
    Split { leaf: LayoutNode, side: DockSide },
    Keep,
}

impl DropTarget {
    pub(crate) fn apply(&self, root: &mut LayoutNode, panel: PanelId) {
        match self {
            Self::Edge(side) => root.dock_panel(panel, *side),
            Self::Split { leaf, side } => {
                root.split_leaf(leaf, panel, *side);
            }
            Self::Keep => {}
        }
    }

    // Not "does the spec allow `side`": the move is played out on a copy of the
    // tree and the side the panel ends up on is what gets checked.
    fn allowed(&self, layout: &Layout, panel: PanelId) -> bool {
        if *self == Self::Keep {
            return true;
        }
        if let Self::Split { leaf, .. } = self {
            if *leaf == LayoutNode::Panel(panel) {
                return false;
            }
            let target_splittable = match leaf {
                LayoutNode::Panel(id) => id.spec().splittable,
                _ => true,
            };
            if !panel.spec().splittable || !target_splittable {
                return false;
            }
        }
        let mut probe = layout.root.clone();
        self.apply(&mut probe, panel);
        probe
            .side_of(panel)
            .is_some_and(|side| panel.spec().allows(side))
    }
}

pub(crate) struct DropHint {
    pub(crate) target: DropTarget,
    pub(crate) rect: graphene::Rect,
}

pub(crate) fn hint_at(
    point: (f32, f32),
    size: (f32, f32),
    leaves: &[(LayoutNode, graphene::Rect)],
    layout: &Layout,
    panel: PanelId,
) -> Option<DropHint> {
    let target = edge_at(point, size)
        .map_or_else(|| split_at(point, leaves), |side| Some(DropTarget::Edge(side)))?;
    let target = match target {
        DropTarget::Split { ref leaf, .. } if *leaf == LayoutNode::Panel(panel) => DropTarget::Keep,
        other => other,
    };
    if !target.allowed(layout, panel) {
        return None;
    }
    let own_place = || {
        leaves
            .iter()
            .find(|(leaf, _)| *leaf == LayoutNode::Panel(panel))
            .map(|(_, rect)| *rect)
    };
    let rect = match &target {
        DropTarget::Edge(side) => {
            let thickness = panel.spec().default_size as f32;
            strip(graphene::Rect::new(0.0, 0.0, size.0, size.1), *side, thickness)
        }
        DropTarget::Split { leaf, side } => {
            let bounds = leaves.iter().find(|(l, _)| l == leaf).map(|(_, r)| *r)?;
            strip(bounds, *side, panel.spec().default_size as f32)
        }
        DropTarget::Keep => own_place()?,
    };
    Some(DropHint { target, rect })
}

fn edge_at(point: (f32, f32), size: (f32, f32)) -> Option<DockSide> {
    let (x, y) = point;
    let (w, h) = size;
    let distances = [
        (DockSide::Left, x),
        (DockSide::Right, w - x),
        (DockSide::Top, y),
        (DockSide::Bottom, h - y),
    ];
    distances
        .into_iter()
        .filter(|(_, d)| *d >= 0.0 && *d < EDGE_ZONE)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(side, _)| side)
}

fn split_at(point: (f32, f32), leaves: &[(LayoutNode, graphene::Rect)]) -> Option<DropTarget> {
    let (x, y) = point;
    let (leaf, rect) = leaves
        .iter()
        .find(|(_, r)| r.contains_point(&graphene::Point::new(x, y)))?;
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return None;
    }
    let left = (x - rect.x()) / rect.width();
    let top = (y - rect.y()) / rect.height();
    let candidates = [
        (DockSide::Left, left),
        (DockSide::Right, 1.0 - left),
        (DockSide::Top, top),
        (DockSide::Bottom, 1.0 - top),
    ];
    let side = candidates
        .into_iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(side, _)| side)?;
    Some(DropTarget::Split {
        leaf: leaf.clone(),
        side,
    })
}

fn strip(rect: graphene::Rect, side: DockSide, thickness: f32) -> graphene::Rect {
    let span = if side.is_column() {
        rect.width()
    } else {
        rect.height()
    };
    let thickness = thickness.min(span * 0.5);
    match side {
        DockSide::Left => graphene::Rect::new(rect.x(), rect.y(), thickness, rect.height()),
        DockSide::Right => graphene::Rect::new(
            rect.x() + rect.width() - thickness,
            rect.y(),
            thickness,
            rect.height(),
        ),
        DockSide::Top => graphene::Rect::new(rect.x(), rect.y(), rect.width(), thickness),
        DockSide::Bottom => graphene::Rect::new(
            rect.x(),
            rect.y() + rect.height() - thickness,
            rect.width(),
            thickness,
        ),
    }
}

pub(crate) fn anchor_at(point: (f32, f32), canvas: graphene::Rect) -> FloatAnchor {
    if canvas.width() <= 0.0 || canvas.height() <= 0.0 {
        return FloatAnchor::Corner(Corner::TopRight);
    }
    let across = ((point.0 - canvas.x()) / canvas.width()).clamp(0.0, 1.0);
    let down = ((point.1 - canvas.y()) / canvas.height()).clamp(0.0, 1.0);
    let column = third(across);
    let row = third(down);

    match (column, row) {
        (0, 0) => FloatAnchor::Corner(Corner::TopLeft),
        (2, 0) => FloatAnchor::Corner(Corner::TopRight),
        (0, 2) => FloatAnchor::Corner(Corner::BottomLeft),
        (2, 2) => FloatAnchor::Corner(Corner::BottomRight),
        (1, 0) => FloatAnchor::Edge(DockSide::Top),
        (1, 2) => FloatAnchor::Edge(DockSide::Bottom),
        (0, 1) => FloatAnchor::Edge(DockSide::Left),
        (2, 1) => FloatAnchor::Edge(DockSide::Right),
        _ => FloatAnchor::Edge(nearest_edge(across, down)),
    }
}

// `size` must be the window's UNSTRETCHED size: fed the size it has, a window
// already pinned along an edge previews every anchor as most of the canvas.
pub(crate) fn anchor_preview(
    canvas: graphene::Rect,
    size: (f32, f32),
    anchor: FloatAnchor,
    margin: f32,
) -> graphene::Rect {
    let inner = graphene::Rect::new(
        canvas.x() + margin,
        canvas.y() + margin,
        (canvas.width() - margin * 2.0).max(1.0),
        (canvas.height() - margin * 2.0).max(1.0),
    );
    let (w, h) = (
        size.0.min(inner.width()).max(1.0),
        size.1.min(inner.height()).max(1.0),
    );
    match anchor {
        FloatAnchor::Corner(corner) => {
            let x = match corner {
                Corner::TopLeft | Corner::BottomLeft => inner.x(),
                Corner::TopRight | Corner::BottomRight => inner.x() + inner.width() - w,
            };
            let y = match corner {
                Corner::TopLeft | Corner::TopRight => inner.y(),
                Corner::BottomLeft | Corner::BottomRight => inner.y() + inner.height() - h,
            };
            graphene::Rect::new(x, y, w, h)
        }
        FloatAnchor::Edge(side) => match side {
            DockSide::Top => graphene::Rect::new(inner.x(), inner.y(), inner.width(), h),
            DockSide::Bottom => graphene::Rect::new(
                inner.x(),
                inner.y() + inner.height() - h,
                inner.width(),
                h,
            ),
            DockSide::Left => graphene::Rect::new(inner.x(), inner.y(), w, inner.height()),
            DockSide::Right => {
                graphene::Rect::new(inner.x() + inner.width() - w, inner.y(), w, inner.height())
            }
        },
    }
}

fn third(value: f32) -> u8 {
    if value < 1.0 / 3.0 {
        0
    } else if value < 2.0 / 3.0 {
        1
    } else {
        2
    }
}

fn nearest_edge(across: f32, down: f32) -> DockSide {
    let distances = [
        (DockSide::Left, across),
        (DockSide::Right, 1.0 - across),
        (DockSide::Top, down),
        (DockSide::Bottom, 1.0 - down),
    ];
    distances
        .into_iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(DockSide::Bottom, |(side, _)| side)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::layout::presets::default_layout;

    const SIZE: (f32, f32) = (1000.0, 800.0);

    fn leaves() -> Vec<(LayoutNode, graphene::Rect)> {
        vec![
            (
                LayoutNode::Canvas,
                graphene::Rect::new(40.0, 40.0, 660.0, 736.0),
            ),
            (
                LayoutNode::Panel(PanelId::Layers),
                graphene::Rect::new(700.0, 370.0, 300.0, 430.0),
            ),
            (
                LayoutNode::Panel(PanelId::ToolBar),
                graphene::Rect::new(0.0, 40.0, 40.0, 760.0),
            ),
        ]
    }

    #[test]
    fn the_window_edge_wins_over_whatever_is_under_it() {
        let layout = default_layout();
        let hint = hint_at((6.0, 400.0), SIZE, &leaves(), &layout, PanelId::Layers)
            .expect("left edge accepts the layers panel");
        assert_eq!(hint.target, DropTarget::Edge(DockSide::Left));
        assert_eq!(hint.rect.x(), 0.0);
        assert_eq!(hint.rect.height(), SIZE.1, "an edge dock spans the window");
    }

    #[test]
    fn dropping_inside_a_panel_splits_it_towards_the_nearest_edge() {
        let layout = default_layout();
        let leaves = leaves();
        let hint = hint_at((850.0, 400.0), SIZE, &leaves, &layout, PanelId::ColorPicker)
            .expect("the layers panel can be split");
        assert_eq!(
            hint.target,
            DropTarget::Split {
                leaf: LayoutNode::Panel(PanelId::Layers),
                side: DockSide::Top,
            }
        );
        assert_eq!(hint.rect.y(), 370.0);
        assert_eq!(hint.rect.height(), 215.0);
    }

    #[test]
    fn a_split_preview_is_the_size_the_panel_will_be() {
        let layout = default_layout();
        let leaves = leaves();
        let hint = hint_at((200.0, 400.0), SIZE, &leaves, &layout, PanelId::ToolBar)
            .expect("the canvas can be split");
        assert_eq!(
            hint.target,
            DropTarget::Split {
                leaf: LayoutNode::Canvas,
                side: DockSide::Left,
            }
        );
        assert_eq!(
            hint.rect.width(),
            PanelId::ToolBar.spec().default_size as f32,
            "a tool bar's width, not half of what it was dropped into"
        );
        assert_eq!(hint.rect.height(), 736.0, "the full height of the canvas");
    }

    #[test]
    fn a_bar_cannot_be_dropped_down_the_side_of_the_window() {
        let layout = default_layout();
        assert!(
            hint_at((6.0, 400.0), SIZE, &leaves(), &layout, PanelId::ToolOptions).is_none(),
            "the tool properties bar is top/bottom only"
        );
        assert!(
            hint_at((500.0, 6.0), SIZE, &leaves(), &layout, PanelId::ToolOptions).is_some(),
            "but it takes the top edge"
        );
    }

    #[test]
    fn a_split_is_judged_by_where_the_panel_ends_up() {
        let layout = default_layout();
        let leaves = leaves();
        assert!(
            hint_at((850.0, 400.0), SIZE, &leaves, &layout, PanelId::ToolOptions).is_none(),
            "splitting the sidebar would leave the bar docked right"
        );
    }

    #[test]
    fn a_panel_dropped_on_itself_stays_where_it_is() {
        let layout = default_layout();
        let leaves = leaves();
        let hint = hint_at((850.0, 400.0), SIZE, &leaves, &layout, PanelId::Layers)
            .expect("its own place is somewhere it can be dropped");
        assert_eq!(hint.target, DropTarget::Keep);
        assert_eq!(hint.rect, leaves[1].1, "exactly the space it already has");

        let mut root = layout.root.clone();
        let before = root.clone();
        hint.target.apply(&mut root, PanelId::Layers);
        assert_eq!(root, before, "and nothing moves");
    }

    #[test]
    fn an_unsplittable_panel_can_still_be_docked_against_an_edge() {
        let layout = default_layout();
        let leaves = leaves();
        let hint = hint_at((500.0, 794.0), SIZE, &leaves, &layout, PanelId::CanvasInfo)
            .expect("the info strip takes the bottom edge");
        assert_eq!(hint.target, DropTarget::Edge(DockSide::Bottom));
    }

    #[test]
    fn a_floating_window_takes_the_corner_or_the_edge_it_is_dropped_on() {
        let canvas = graphene::Rect::new(0.0, 0.0, 900.0, 600.0);
        assert_eq!(
            anchor_at((880.0, 20.0), canvas),
            FloatAnchor::Corner(Corner::TopRight)
        );
        assert_eq!(
            anchor_at((20.0, 580.0), canvas),
            FloatAnchor::Corner(Corner::BottomLeft)
        );
        assert_eq!(
            anchor_at((450.0, 580.0), canvas),
            FloatAnchor::Edge(DockSide::Bottom),
            "the middle of an edge stretches the window along it"
        );
        assert_eq!(anchor_at((20.0, 300.0), canvas), FloatAnchor::Edge(DockSide::Left));
        assert_eq!(
            anchor_at((450.0, 320.0), canvas),
            FloatAnchor::Edge(DockSide::Bottom),
            "the dead centre falls to the nearest edge"
        );
    }

    #[test]
    fn a_stretched_window_runs_the_length_of_its_edge() {
        let canvas = graphene::Rect::new(10.0, 20.0, 900.0, 600.0);
        let corner = anchor_preview(
            canvas,
            (300.0, 200.0),
            FloatAnchor::Corner(Corner::TopRight),
            12.0,
        );
        assert_eq!(corner.width(), 300.0);
        assert_eq!(corner.x(), 10.0 + 900.0 - 12.0 - 300.0);
        assert_eq!(corner.y(), 32.0);

        let edge = anchor_preview(canvas, (300.0, 200.0), FloatAnchor::Edge(DockSide::Bottom), 12.0);
        assert_eq!(edge.width(), 900.0 - 24.0, "full width, inside the margin");
        assert_eq!(edge.height(), 200.0);
        assert_eq!(edge.y(), 20.0 + 600.0 - 12.0 - 200.0);
    }

    #[test]
    fn dropping_on_the_canvas_docks_beside_it() {
        let layout = default_layout();
        let leaves = leaves();
        let hint = hint_at((80.0, 400.0), SIZE, &leaves, &layout, PanelId::Layers)
            .expect("the canvas can be split");
        assert_eq!(
            hint.target,
            DropTarget::Split {
                leaf: LayoutNode::Canvas,
                side: DockSide::Left,
            }
        );
    }
}
