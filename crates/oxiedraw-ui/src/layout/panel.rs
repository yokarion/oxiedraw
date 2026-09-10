use oxiedraw_core::enum_meta::EnumMeta;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SplitChild {
    First,
    Second,
}

impl Axis {
    pub(crate) const fn side(self, child: SplitChild) -> DockSide {
        match (self, child) {
            (Self::Horizontal, SplitChild::First) => DockSide::Left,
            (Self::Horizontal, SplitChild::Second) => DockSide::Right,
            (Self::Vertical, SplitChild::First) => DockSide::Top,
            (Self::Vertical, SplitChild::Second) => DockSide::Bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DockSide {
    Left,
    Right,
    Top,
    Bottom,
}

impl DockSide {
    pub(crate) const ANY: &'static [Self] = &[Self::Left, Self::Right, Self::Top, Self::Bottom];
    pub(crate) const BARS: &'static [Self] = &[Self::Top, Self::Bottom];

    pub(crate) const fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Top | Self::Bottom => Axis::Vertical,
        }
    }

    pub(crate) const fn child(self) -> SplitChild {
        match self {
            Self::Left | Self::Top => SplitChild::First,
            Self::Right | Self::Bottom => SplitChild::Second,
        }
    }

    // Left and right panels are sized across, top and bottom ones down; that is
    // the axis every size in a `PanelSpec` is measured on.
    pub(crate) const fn is_column(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PanelId {
    ToolBar,
    ToolOptions,
    ColorPicker,
    Layers,
    CanvasInfo,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PanelSpec {
    pub(crate) id: PanelId,
    pub(crate) display_name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) sides: &'static [DockSide],
    pub(crate) splittable: bool,
    pub(crate) resizable: bool,
    pub(crate) snap_step: Option<i32>,
    pub(crate) min_size: i32,
    pub(crate) default_size: i32,
    pub(crate) default_side: DockSide,
    pub(crate) removable: bool,
}

impl PanelSpec {
    pub(crate) fn allows(&self, side: DockSide) -> bool {
        self.sides.contains(&side)
    }

    pub(crate) fn allows_horizontal(&self) -> bool {
        self.allows(DockSide::Top) || self.allows(DockSide::Bottom)
    }

    pub(crate) fn allows_vertical(&self) -> bool {
        self.allows(DockSide::Left) || self.allows(DockSide::Right)
    }

    pub(crate) fn snap(&self, size: i32) -> i32 {
        let step = self.snap_step.unwrap_or(1).max(1);
        let snapped = (size + step / 2) / step * step;
        snapped.max(self.min_size)
    }
}

pub(crate) const TOOL_ICON_CELL: i32 = 40;

pub(crate) static SPECS: [PanelSpec; 5] = [
    PanelSpec {
        id: PanelId::ToolBar,
        display_name: "Tools",
        description: "Brush, selection, transform and the rest of the tool buttons.",
        sides: DockSide::ANY,
        splittable: true,
        resizable: true,
        snap_step: Some(TOOL_ICON_CELL),
        min_size: TOOL_ICON_CELL,
        default_size: TOOL_ICON_CELL,
        default_side: DockSide::Left,
        removable: true,
    },
    PanelSpec {
        id: PanelId::ToolOptions,
        display_name: "Tool Properties",
        description: "Size, opacity and the rest of the active tool's settings.",
        sides: DockSide::BARS,
        splittable: false,
        resizable: false,
        snap_step: None,
        min_size: 40,
        default_size: 40,
        default_side: DockSide::Top,
        removable: true,
    },
    PanelSpec {
        id: PanelId::ColorPicker,
        display_name: "Color",
        description: "Hue wheel, primary and secondary swatches, RGB and hex.",
        sides: DockSide::ANY,
        splittable: true,
        resizable: true,
        snap_step: None,
        min_size: crate::panels::color_picker::MIN_SIZE,
        default_size: 300,
        default_side: DockSide::Right,
        removable: true,
    },
    PanelSpec {
        id: PanelId::Layers,
        display_name: "Layers",
        description: "Layer stack, blend mode and opacity, plus the component library.",
        sides: DockSide::ANY,
        splittable: true,
        resizable: true,
        snap_step: None,
        min_size: 180,
        default_size: 300,
        default_side: DockSide::Right,
        removable: true,
    },
    PanelSpec {
        id: PanelId::CanvasInfo,
        display_name: "Canvas Info",
        description: "Canvas size and the view rotation dial.",
        sides: DockSide::BARS,
        splittable: false,
        resizable: true,
        snap_step: None,
        min_size: 24,
        default_size: 24,
        default_side: DockSide::Bottom,
        removable: true,
    },
];

impl PanelId {
    pub(crate) fn spec(self) -> &'static PanelSpec {
        SPECS
            .iter()
            .find(|s| s.id == self)
            .unwrap_or(&SPECS[0])
    }
}

impl EnumMeta for PanelId {
    const ALL: &'static [Self] = &[
        Self::ToolBar,
        Self::ToolOptions,
        Self::ColorPicker,
        Self::Layers,
        Self::CanvasInfo,
    ];

    fn label(self) -> &'static str {
        self.spec().display_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_panel_has_its_own_spec() {
        for id in PanelId::ALL {
            assert_eq!(id.spec().id, *id, "{id:?} resolved to another panel's spec");
        }
        assert_eq!(SPECS.len(), PanelId::ALL.len());
    }

    #[test]
    fn dock_sides_map_onto_split_positions() {
        for side in DockSide::ANY {
            assert_eq!(side.axis().side(side.child()), *side);
        }
        assert!(DockSide::Left.is_column());
        assert!(!DockSide::Bottom.is_column());
    }

    #[test]
    fn bars_are_horizontal_only_and_columns_upright_only() {
        let bar = PanelId::ToolOptions.spec();
        assert!(bar.allows_horizontal());
        assert!(!bar.allows_vertical());
        assert!(!bar.allows(DockSide::Left));

        let tools = PanelId::ToolBar.spec();
        assert!(tools.allows_horizontal() && tools.allows_vertical());
    }

    #[test]
    fn the_tool_properties_bar_does_not_resize() {
        assert!(!PanelId::ToolOptions.spec().resizable);
        assert!(PanelId::Layers.spec().resizable);
    }

    #[test]
    fn snapping_rounds_to_the_step_and_honours_the_minimum() {
        let tools = PanelId::ToolBar.spec();
        assert_eq!(tools.snap(59), 40, "under half a cell rounds down");
        assert_eq!(tools.snap(61), 80, "over half a cell rounds up");
        assert_eq!(tools.snap(4), 40, "never below the minimum");

        let free = PanelId::Layers.spec();
        assert_eq!(free.snap(233), 233, "no step means no rounding");
        assert_eq!(free.snap(10), free.min_size);
    }

    #[test]
    fn panel_ids_round_trip_as_stable_kebab_case_keys() {
        let json = serde_json::to_string(&PanelId::ColorPicker).expect("serialise");
        assert_eq!(json, "\"color-picker\"");
        let back: PanelId = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, PanelId::ColorPicker);
    }
}
