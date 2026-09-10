use oxiedraw_core::enum_meta::EnumMeta;
use oxiedraw_core::tools::{FillTool, Tool};
use serde::{Deserialize, Serialize};

use super::panel::DockSide;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FloatAnchor {
    Corner(Corner),
    Edge(DockSide),
}

impl FloatAnchor {
    // Which way a window resizes, as the direction each axis grows with the
    // pointer. Never both: an edge takes its length from the canvas, and a
    // height is the content's business.
    pub(crate) const fn free_edges(self) -> (i8, i8) {
        match self {
            Self::Corner(Corner::TopLeft | Corner::BottomLeft) | Self::Edge(DockSide::Left) => {
                (1, 0)
            }
            Self::Corner(Corner::TopRight | Corner::BottomRight) | Self::Edge(DockSide::Right) => {
                (-1, 0)
            }
            Self::Edge(DockSide::Top) => (0, 1),
            Self::Edge(DockSide::Bottom) => (0, -1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ToolWindowId {
    Gradient,
    TextProperties,
    Crop,
    Pattern,
    Guide,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolWindowSpec {
    pub(crate) id: ToolWindowId,
    pub(crate) display_name: &'static str,
    pub(crate) default_anchor: FloatAnchor,
}

pub(crate) static TOOL_WINDOWS: [ToolWindowSpec; 5] = [
    ToolWindowSpec {
        id: ToolWindowId::Gradient,
        display_name: "Gradient",
        default_anchor: FloatAnchor::Corner(Corner::TopRight),
    },
    ToolWindowSpec {
        id: ToolWindowId::TextProperties,
        display_name: "Text",
        default_anchor: FloatAnchor::Edge(DockSide::Bottom),
    },
    ToolWindowSpec {
        id: ToolWindowId::Crop,
        display_name: "Crop",
        default_anchor: FloatAnchor::Corner(Corner::TopRight),
    },
    ToolWindowSpec {
        id: ToolWindowId::Pattern,
        display_name: "Pattern",
        default_anchor: FloatAnchor::Corner(Corner::TopRight),
    },
    ToolWindowSpec {
        id: ToolWindowId::Guide,
        display_name: "Drawing Guide",
        default_anchor: FloatAnchor::Corner(Corner::TopRight),
    },
];

impl ToolWindowId {
    pub(crate) fn spec(self) -> &'static ToolWindowSpec {
        TOOL_WINDOWS
            .iter()
            .find(|s| s.id == self)
            .unwrap_or(&TOOL_WINDOWS[0])
    }

    pub(crate) const fn for_tool(tool: Tool) -> Option<Self> {
        match tool {
            Tool::Fill(FillTool::Gradient) => Some(Self::Gradient),
            Tool::Crop => Some(Self::Crop),
            Tool::Pattern => Some(Self::Pattern),
            Tool::DrawingGuide => Some(Self::Guide),
            _ => None,
        }
    }
}

impl EnumMeta for ToolWindowId {
    const ALL: &'static [Self] = &[
        Self::Gradient,
        Self::TextProperties,
        Self::Crop,
        Self::Pattern,
        Self::Guide,
    ];

    fn label(self) -> &'static str {
        self.spec().display_name
    }
}

pub(crate) const MIN_FLOAT_SIZE: i32 = 160;
pub(crate) const MAX_FLOAT_SIZE: i32 = 4000;

// A maximum, not a minimum: a window takes this much where the canvas has room
// and shrinks where it does not. `None` is "as much as the content wants".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FloatSize {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) height: Option<i32>,
}

impl FloatSize {
    pub(crate) fn sanitize(&mut self) -> bool {
        let before = *self;
        self.width = self.width.map(|w| w.clamp(MIN_FLOAT_SIZE, MAX_FLOAT_SIZE));
        self.height = self.height.map(|h| h.clamp(MIN_FLOAT_SIZE, MAX_FLOAT_SIZE));
        *self != before
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FloatingPlacement {
    pub(crate) window: ToolWindowId,
    pub(crate) anchor: FloatAnchor,
    #[serde(default)]
    pub(crate) size: FloatSize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_window_has_its_own_spec() {
        for id in ToolWindowId::ALL {
            assert_eq!(id.spec().id, *id, "{id:?} resolved to another spec");
        }
        assert_eq!(TOOL_WINDOWS.len(), ToolWindowId::ALL.len());
    }

    #[test]
    fn tools_map_to_their_window() {
        assert_eq!(
            ToolWindowId::for_tool(Tool::Fill(FillTool::Gradient)),
            Some(ToolWindowId::Gradient)
        );
        assert_eq!(ToolWindowId::for_tool(Tool::Crop), Some(ToolWindowId::Crop));
        assert_eq!(ToolWindowId::for_tool(Tool::Brush), None);
        assert_eq!(
            ToolWindowId::for_tool(Tool::Fill(FillTool::Bucket)),
            None,
            "the bucket shares the Fill group but has no floating window"
        );
    }

    #[test]
    fn only_the_axis_the_anchor_leaves_free_can_be_resized() {
        assert_eq!(FloatAnchor::Edge(DockSide::Right).free_edges(), (-1, 0));
        assert_eq!(FloatAnchor::Edge(DockSide::Left).free_edges(), (1, 0));
        assert_eq!(FloatAnchor::Edge(DockSide::Bottom).free_edges(), (0, -1));
        assert_eq!(FloatAnchor::Edge(DockSide::Top).free_edges(), (0, 1));
        assert_eq!(FloatAnchor::Corner(Corner::TopRight).free_edges(), (-1, 0));
        assert_eq!(FloatAnchor::Corner(Corner::BottomLeft).free_edges(), (1, 0));
        for anchor in [
            FloatAnchor::Corner(Corner::TopLeft),
            FloatAnchor::Corner(Corner::BottomRight),
            FloatAnchor::Edge(DockSide::Top),
        ] {
            let (horizontal, vertical) = anchor.free_edges();
            assert!(
                horizontal == 0 || vertical == 0,
                "{anchor:?} gives on both axes at once"
            );
        }
    }

    #[test]
    fn a_placement_written_before_windows_could_be_resized_still_loads() {
        let placement: FloatingPlacement =
            serde_json::from_str(r#"{"window":"crop","anchor":{"corner":"top-right"}}"#)
                .expect("deserialise");
        assert_eq!(placement.size, FloatSize::default());
        assert_eq!(placement.size.width, None, "as big as its content wants");
        let back = serde_json::to_string(&placement).expect("serialise");
        assert!(!back.contains("width"), "{back}");
    }

    #[test]
    fn a_stored_size_is_kept_inside_the_rails() {
        let mut size = FloatSize {
            width: Some(20),
            height: Some(99_999),
        };
        assert!(size.sanitize());
        assert_eq!(size.width, Some(MIN_FLOAT_SIZE));
        assert_eq!(size.height, Some(MAX_FLOAT_SIZE));
        assert!(!size.sanitize(), "and stays there");

        let mut untouched = FloatSize::default();
        assert!(!untouched.sanitize(), "no size is a valid size");
    }

    #[test]
    fn anchors_round_trip_as_stable_keys() {
        let edge = serde_json::to_string(&FloatAnchor::Edge(DockSide::Bottom)).expect("serialise");
        assert_eq!(edge, r#"{"edge":"bottom"}"#);
        let corner =
            serde_json::to_string(&FloatAnchor::Corner(Corner::TopRight)).expect("serialise");
        assert_eq!(corner, r#"{"corner":"top-right"}"#);

        let back: FloatAnchor = serde_json::from_str(&corner).expect("deserialise");
        assert_eq!(back, FloatAnchor::Corner(Corner::TopRight));
    }
}
