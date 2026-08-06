mod tool_button;

use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::tools::{FillTool, SelectionTool, ShapeTool, Tool, ToolState};
use relm4::gtk;
use relm4::gtk::prelude::*;

const WIDTH: i32 = 40;

struct ToolGroupSpec {
    name: &'static str,
    subtools: &'static [Tool],
    action_id: Option<&'static str>,
}

static SELECTION_SUBTOOLS: [Tool; 3] = [
    Tool::Selection(SelectionTool::Square),
    Tool::Selection(SelectionTool::Circle),
    Tool::Selection(SelectionTool::Free),
];

static FILL_SUBTOOLS: [Tool; 2] = [Tool::Fill(FillTool::Bucket), Tool::Fill(FillTool::Gradient)];

static SHAPE_SUBTOOLS: [Tool; 4] = [
    Tool::Shapes(ShapeTool::Rectangle),
    Tool::Shapes(ShapeTool::Line),
    Tool::Shapes(ShapeTool::Circle),
    Tool::Shapes(ShapeTool::Triangle),
];

static GROUPS: [ToolGroupSpec; 11] = [
    ToolGroupSpec {
        name: "Cursor",
        subtools: &[Tool::Cursor],
        action_id: Some("select-cursor"),
    },
    ToolGroupSpec {
        name: "Selection",
        subtools: &SELECTION_SUBTOOLS,
        action_id: Some("select-selection"),
    },
    ToolGroupSpec {
        name: "Transform",
        subtools: &[Tool::Transform],
        action_id: Some("select-transform"),
    },
    ToolGroupSpec {
        name: "Brush",
        subtools: &[Tool::Brush],
        action_id: Some("select-brush"),
    },
    ToolGroupSpec {
        name: "Color Picker",
        subtools: &[Tool::ColorPicker],
        action_id: Some("select-picker"),
    },
    ToolGroupSpec {
        name: "Fill",
        subtools: &FILL_SUBTOOLS,
        action_id: Some("select-fill"),
    },
    ToolGroupSpec {
        name: "Shapes",
        subtools: &SHAPE_SUBTOOLS,
        action_id: None,
    },
    ToolGroupSpec {
        name: "Text",
        subtools: &[Tool::Text],
        action_id: Some("select-text"),
    },
    ToolGroupSpec {
        name: "Crop",
        subtools: &[Tool::Crop],
        action_id: Some("select-crop"),
    },
    ToolGroupSpec {
        name: "Liquify",
        subtools: &[Tool::Liquify],
        action_id: Some("select-liquify"),
    },
    ToolGroupSpec {
        name: "Drawing Guide",
        subtools: &[Tool::DrawingGuide],
        action_id: Some("select-guide"),
    },
];

/// Build the left toolbar.
///
/// Returns the bar widget and a setter closure. Call the setter with a `Tool`
/// to programmatically activate the matching toggle button without triggering
/// the `on_change` callback (safe to call from inside `on_change`).
pub(crate) fn build(
    tools: &ToolState,
    on_change: &Rc<dyn Fn(Tool)>,
) -> (gtk::Box, impl Fn(Tool) + use<>) {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(WIDTH)
        .build();
    bar.add_css_class("sidebar");

    // Shared guard: when true, `toggled` handlers skip `on_change`.
    let programmatic = Rc::new(Cell::new(false));

    let mut first_btn: Option<gtk::ToggleButton> = None;
    let mut groups: Vec<(&'static [Tool], gtk::ToggleButton, Rc<Cell<Tool>>)> = Vec::new();

    for spec in &GROUPS {
        let (overlay, toggle, active_sub) = tool_button::build(
            spec.name,
            spec.action_id,
            spec.subtools,
            tools,
            on_change,
            Rc::clone(&programmatic),
        );
        if let Some(ref first) = first_btn {
            toggle.set_group(Some(first));
        } else {
            first_btn = Some(toggle.clone());
        }
        groups.push((spec.subtools, toggle, active_sub));
        bar.append(&overlay);
    }

    let setter = {
        let prog = Rc::clone(&programmatic);
        move |t: Tool| {
            for (subtools, btn, active_sub) in &groups {
                if subtools.contains(&t) {
                    active_sub.set(t);
                    btn.set_icon_name(t.icon_name());
                    btn.set_tooltip_text(Some(t.display_name()));
                    prog.set(true);
                    btn.set_active(true);
                    prog.set(false);
                    return;
                }
            }
        }
    };

    (bar, setter)
}
