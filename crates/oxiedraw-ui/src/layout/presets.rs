use super::floating::ToolWindowId;
use super::panel::{Axis, DockSide, PanelId, TOOL_ICON_CELL};
use super::store::Layout;
use super::tree::LayoutNode;
use oxiedraw_core::enum_meta::EnumMeta;

pub(crate) const DEFAULT_LAYOUT_KEY: &str = "default";
pub(crate) const DEFAULT_LAYOUT_NAME: &str = "Default Layout";

const TOOL_OPTIONS_H: i32 = 40;
const SIDEBAR_W: i32 = 300;
const COLOR_PICKER_H: i32 = 372;
const PALETTE_H: i32 = 170;
const CANVAS_INFO_H: i32 = 24;

// The arrangement the app opened with before layouts were customisable, so a
// fresh install and a Reset both land somewhere familiar.
pub(crate) fn default_layout() -> Layout {
    let palette_over_layers = LayoutNode::Split {
        axis: Axis::Vertical,
        fixed: DockSide::Top.child(),
        size: PALETTE_H,
        first: Box::new(LayoutNode::Panel(PanelId::Palette)),
        second: Box::new(LayoutNode::Panel(PanelId::Layers)),
    };
    let sidebar = LayoutNode::Split {
        axis: Axis::Vertical,
        fixed: DockSide::Top.child(),
        size: COLOR_PICKER_H,
        first: Box::new(LayoutNode::Panel(PanelId::ColorPicker)),
        second: Box::new(palette_over_layers),
    };
    let document = LayoutNode::dock_sized(
        PanelId::CanvasInfo,
        DockSide::Bottom,
        CANVAS_INFO_H,
        LayoutNode::Canvas,
    );
    let middle = LayoutNode::Split {
        axis: Axis::Horizontal,
        fixed: DockSide::Right.child(),
        size: SIDEBAR_W,
        first: Box::new(document),
        second: Box::new(sidebar),
    };
    let with_tools =
        LayoutNode::dock_sized(PanelId::ToolBar, DockSide::Left, TOOL_ICON_CELL, middle);
    let root = LayoutNode::dock_sized(
        PanelId::ToolOptions,
        DockSide::Top,
        TOOL_OPTIONS_H,
        with_tools,
    );

    Layout {
        name: DEFAULT_LAYOUT_NAME.to_string(),
        preset: Some(DEFAULT_LAYOUT_KEY.to_string()),
        root,
        floating: ToolWindowId::ALL
            .iter()
            .map(|w| super::floating::FloatingPlacement {
                window: *w,
                anchor: w.spec().default_anchor,
                size: super::floating::FloatSize::default(),
            })
            .collect(),
    }
}

pub(crate) fn builtin_layouts() -> Vec<Layout> {
    vec![default_layout()]
}

pub(crate) fn preset_by_key(key: &str) -> Option<Layout> {
    builtin_layouts()
        .into_iter()
        .find(|l| l.preset.as_deref() == Some(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_preset_is_well_formed() {
        let mut layout = default_layout();
        assert!(
            !layout.root.sanitize(),
            "the shipped default must not need repairing"
        );
    }

    #[test]
    fn the_default_preset_reproduces_the_original_window() {
        let layout = default_layout();
        assert_eq!(layout.root.side_of(PanelId::ToolOptions), Some(DockSide::Top));
        assert_eq!(layout.root.side_of(PanelId::ToolBar), Some(DockSide::Left));
        assert_eq!(layout.root.side_of(PanelId::ColorPicker), Some(DockSide::Right));
        assert_eq!(layout.root.side_of(PanelId::Palette), Some(DockSide::Right));
        assert_eq!(layout.root.side_of(PanelId::Layers), Some(DockSide::Right));
        assert_eq!(
            layout.root.side_of(PanelId::CanvasInfo),
            Some(DockSide::Bottom)
        );
        assert_eq!(
            layout.root.panels().len(),
            PanelId::ALL.len(),
            "every panel is on screen by default"
        );
    }

    #[test]
    fn the_info_strip_stays_inside_the_document_column() {
        let layout = default_layout();
        let mut without_info = layout.root.clone();
        without_info.remove_panel(PanelId::CanvasInfo);
        assert!(without_info.contains_canvas());
        assert_eq!(without_info.side_of(PanelId::Layers), Some(DockSide::Right));
    }

    #[test]
    fn presets_are_found_by_key_not_by_name() {
        let found = preset_by_key(DEFAULT_LAYOUT_KEY).expect("default preset");
        assert_eq!(found.name, DEFAULT_LAYOUT_NAME);
        assert!(preset_by_key("nope").is_none());
    }

    #[test]
    fn every_floating_window_starts_at_its_default_anchor() {
        let layout = default_layout();
        assert_eq!(layout.floating.len(), ToolWindowId::ALL.len());
        for placement in &layout.floating {
            assert_eq!(placement.anchor, placement.window.spec().default_anchor);
        }
    }
}
