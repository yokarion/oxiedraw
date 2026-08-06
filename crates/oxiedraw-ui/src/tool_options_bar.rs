use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::brush_engine::BrushEngine;
use oxiedraw_core::liquify::{LiquifyMode, LiquifyState};
use oxiedraw_core::tools::{
    CropState, FillState, FillTool, GradientState, GradientType, ShapeState, Tool, ToolState,
    TransformFilter, TransformState,
};
use relm4::RelmWidgetExt;
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::brush_picker;
use crate::widgets::{slider, tool_chip};

const HEIGHT: i32 = 40;
const LABEL_MARGIN: i32 = 12;
const ROW_SPACING: i32 = 8;
const OPACITY_STEP: f64 = 0.01;
const SIZE_SLIDER_WIDTH: i32 = 260;

/// Brush size segments as `(size_lo, size_hi, step, pos_width)`. Each segment
/// covers a size range with its own increment and occupies `pos_width` of the
/// slider trough, so fine sizes get more travel than coarse ones. `pos_width`
/// values sum to 1.0.
const SIZE_SEGMENTS: [(f64, f64, f64, f64); 3] = [
    (1.0, 50.0, 1.0, 0.50),
    (50.0, 200.0, 10.0, 0.30),
    (200.0, 1000.0, 50.0, 0.20),
];

const OPACITY_SLIDER_WIDTH: i32 = 120;

const STACK_BRUSH: &str = "brush";
const STACK_CROP: &str = "crop";
const STACK_TRANSFORM: &str = "transform";
const STACK_FILL: &str = "fill";
const STACK_SHAPE: &str = "shape";
const STACK_GRADIENT: &str = "gradient";
const STACK_TEXT: &str = "text";
const STACK_GUIDE: &str = "guide";
const STACK_LIQUIFY: &str = "liquify";
const STACK_NONE: &str = "none";

const TOLERANCE_SLIDER_WIDTH: i32 = 320;

/// Tool properties bar shown above the canvas.
///
/// Returns the widget and a setter the toolbar uses to push the active tool
/// into the bar - updates the chip and switches the inner stack.
pub(crate) fn build(
    tools: &ToolState,
    brush_engine: &BrushEngine,
    crop: &CropState,
    on_crop_apply: Rc<dyn Fn()>,
    transform: &TransformState,
    on_transform_apply: Rc<dyn Fn()>,
    on_transform_cancel: Rc<dyn Fn()>,
    fill: &FillState,
    shape: &ShapeState,
    gradient: &GradientState,
    liquify: &LiquifyState,
    text_edit: &Rc<std::cell::RefCell<Option<crate::text_edit::TextEdit>>>,
    default_brush_name: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    toaster: crate::toaster::Toaster,
) -> (gtk::Box, Rc<dyn Fn(Tool)>) {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .height_request(HEIGHT)
        .build();
    bar.add_css_class("sidebar");

    let (chip_widget, update_chip) = tool_chip::build(tools.active.get());
    bar.append(&chip_widget);

    // Separator between chip and stack.
    let sep = gtk::Separator::new(gtk::Orientation::Vertical);
    sep.set_margin_top(8);
    sep.set_margin_bottom(8);
    sep.set_margin_start(2);
    sep.set_margin_end(2);
    bar.append(&sep);

    let stack = gtk::Stack::builder()
        .hhomogeneous(false)
        .transition_type(gtk::StackTransitionType::None)
        .hexpand(true)
        .build();
    stack.add_named(
        &build_brush_page(brush_engine, default_brush_name, toaster),
        Some(STACK_BRUSH),
    );
    stack.add_named(&build_crop_page(crop, on_crop_apply), Some(STACK_CROP));
    stack.add_named(
        &build_transform_page(transform, on_transform_apply, on_transform_cancel),
        Some(STACK_TRANSFORM),
    );
    stack.add_named(&build_fill_page(fill), Some(STACK_FILL));
    stack.add_named(&build_shape_page(shape), Some(STACK_SHAPE));
    stack.add_named(&build_gradient_page(gradient), Some(STACK_GRADIENT));
    stack.add_named(&build_liquify_page(liquify), Some(STACK_LIQUIFY));
    stack.add_named(&build_text_page(text_edit), Some(STACK_TEXT));
    stack.add_named(&build_guide_page(), Some(STACK_GUIDE));
    stack.add_named(
        &gtk::Box::new(gtk::Orientation::Horizontal, 0),
        Some(STACK_NONE),
    );
    stack.set_visible_child_name(stack_name_for(tools.active.get()));
    bar.append(&stack);

    let setter: Rc<dyn Fn(Tool)> = Rc::new(move |t: Tool| {
        update_chip(t);
        stack.set_visible_child_name(stack_name_for(t));
    });
    (bar, setter)
}

const fn stack_name_for(tool: Tool) -> &'static str {
    match tool {
        Tool::Brush => STACK_BRUSH,
        Tool::Crop => STACK_CROP,
        Tool::Transform => STACK_TRANSFORM,
        Tool::Fill(FillTool::Bucket) => STACK_FILL,
        Tool::Fill(FillTool::Gradient) => STACK_GRADIENT,
        Tool::Shapes(_) => STACK_SHAPE,
        Tool::Text => STACK_TEXT,
        Tool::Liquify => STACK_LIQUIFY,
        Tool::DrawingGuide => STACK_GUIDE,
        Tool::Cursor | Tool::Selection(_) | Tool::ColorPicker => STACK_NONE,
    }
}

// ---------------------------------------------------------------------------
// Brush page
// ---------------------------------------------------------------------------

fn build_brush_page(
    brush_engine: &BrushEngine,
    default_brush_name: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    toaster: crate::toaster::Toaster,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(ROW_SPACING)
        .margin_start(4)
        .margin_end(LABEL_MARGIN)
        .build();

    row.append(&brush_picker::build(brush_engine, default_brush_name, toaster));
    row.append(&build_eraser_toggle());
    row.append(&gtk::Label::new(Some("Size")));
    row.append(&build_size_slider(brush_engine));
    row.append(&gtk::Label::new(Some("Opacity")));
    row.append(&build_opacity_slider(brush_engine));

    row
}

/// Eraser-mode toggle. While active, brush strokes remove coverage from the
/// active layer instead of painting (same brush settings, inverted effect).
///
/// Inactive it is a plain flat icon button; active it fills with the system
/// accent color (`.suggested-action`, which follows the GNOME accent). It binds
/// to the stateful `app.eraser-toggle` action, so clicking it, the keybinding,
/// and every tab's copy of the button all stay in sync via GTK.
fn build_eraser_toggle() -> gtk::ToggleButton {
    let btn = gtk::ToggleButton::builder()
        .icon_name("oxiedraw-eraser-symbolic")
        .tooltip_text("Eraser (E)")
        .valign(gtk::Align::Center)
        .action_name("app.eraser-toggle")
        .build();
    apply_eraser_style(&btn, btn.is_active());
    // The action drives `active`; restyle whenever it changes (click or key).
    btn.connect_active_notify(|b| apply_eraser_style(b, b.is_active()));
    btn
}

/// Flat normally; full system-accent background while erasing.
fn apply_eraser_style(btn: &gtk::ToggleButton, active: bool) {
    if active {
        btn.remove_css_class("flat");
        btn.add_css_class("suggested-action");
    } else {
        btn.remove_css_class("suggested-action");
        btn.add_css_class("flat");
    }
}

fn build_size_slider(brush_engine: &BrushEngine) -> gtk::Scale {
    let size = brush_engine.size.clone();
    slider::build_mapped(
        f64::from(brush_engine.size.get()),
        SIZE_SLIDER_WIDTH,
        size_pos_to_value,
        size_value_to_pos,
        |value| {
            #[allow(clippy::cast_possible_truncation)]
            let v = value.round() as i32;
            format!("{v:>4}")
        },
        move |value| {
            #[allow(clippy::cast_possible_truncation)]
            size.set(value as f32);
        },
    )
}

/// Maps a `[0, 1]` trough position to a size snapped to the piecewise step of
/// the segment it falls in. `segments` is a table in the [`SIZE_SEGMENTS`]
/// shape, so tools with different ranges share this mapping.
fn segmented_pos_to_value(pos: f64, segments: &[(f64, f64, f64, f64)]) -> f64 {
    let mut pos_lo = 0.0;
    for &(size_lo, size_hi, step, pos_width) in segments {
        let pos_hi = pos_lo + pos_width;
        if pos <= pos_hi || pos_width <= 0.0 {
            let t = ((pos - pos_lo) / pos_width).clamp(0.0, 1.0);
            let raw = size_lo + t * (size_hi - size_lo);
            let snapped = size_lo + ((raw - size_lo) / step).round() * step;
            return snapped.clamp(size_lo, size_hi);
        }
        pos_lo = pos_hi;
    }
    segments[segments.len() - 1].1
}

/// Places a size on the `[0, 1]` trough per the piecewise layout.
fn segmented_value_to_pos(size: f64, segments: &[(f64, f64, f64, f64)]) -> f64 {
    let mut pos_lo = 0.0;
    for &(size_lo, size_hi, _step, pos_width) in segments {
        if size <= size_hi {
            let t = ((size - size_lo) / (size_hi - size_lo)).clamp(0.0, 1.0);
            return pos_lo + t * pos_width;
        }
        pos_lo += pos_width;
    }
    1.0
}

fn size_pos_to_value(pos: f64) -> f64 {
    segmented_pos_to_value(pos, &SIZE_SEGMENTS)
}

fn size_value_to_pos(size: f64) -> f64 {
    segmented_value_to_pos(size, &SIZE_SEGMENTS)
}

fn build_opacity_slider(brush_engine: &BrushEngine) -> gtk::Scale {
    let opacity = brush_engine.opacity.clone();
    slider::build(
        (0.0, 1.0),
        OPACITY_STEP,
        f64::from(brush_engine.opacity.get()),
        OPACITY_SLIDER_WIDTH,
        |value| {
            #[allow(clippy::cast_possible_truncation)]
            let pct = (value * 100.0).round() as i32;
            format!("{pct:>3}%")
        },
        move |value| {
            #[allow(clippy::cast_possible_truncation)]
            opacity.set(value as f32);
        },
    )
}

// ---------------------------------------------------------------------------
// Crop page
// ---------------------------------------------------------------------------

/// Drawing Guide bottom-bar page: right-aligned Cancel / Done, styled like the
/// crop tool's Cancel / Apply. Buttons drive the window-level guide actions.
fn build_guide_page() -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(8)
        .valign(gtk::Align::Center)
        .build();

    let spacer = gtk::Box::builder().hexpand(true).build();
    row.append(&spacer);

    let cancel_btn = gtk::Button::builder()
        .label("Cancel")
        .valign(gtk::Align::Center)
        .action_name("app.guide-cancel")
        .build();
    cancel_btn.add_css_class("flat");
    row.append(&cancel_btn);

    let done_btn = gtk::Button::builder()
        .label("Done")
        .valign(gtk::Align::Center)
        .action_name("app.guide-done")
        .build();
    done_btn.add_css_class("suggested-action");
    row.append(&done_btn);

    row
}

fn build_crop_page(crop: &CropState, on_apply: Rc<dyn Fn()>) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(8)
        .valign(gtk::Align::Center)
        .build();

    // Aspect ratio dropdown.
    row.append(&build_ratio_dropdown(crop));

    row.append(&dim_sep());

    // W field.
    row.append(&small_label("W"));
    let w_spin = build_dim_spin(1.0, 32_000.0);
    row.append(&w_spin);
    row.append(&small_label("px"));

    // Swap button.
    let swap_btn = gtk::Button::builder()
        .icon_name("object-flip-horizontal-symbolic")
        .tooltip_text("Swap W <-> H")
        .valign(gtk::Align::Center)
        .build();
    swap_btn.add_css_class("flat");
    swap_btn.inline_css("padding: 2px;");
    {
        let crop_c = crop.clone();
        let w_c = w_spin.clone();
        swap_btn.connect_clicked(move |_| {
            if let Some(r) = crop_c.rect.get() {
                let n = r.normalized();
                use oxiedraw_core::tools::CropRect;
                crop_c.rect.set(Some(CropRect::new(n.x, n.y, n.h, n.w)));
                crop_c.notify_rect_changed();
            }
            let _ = w_c; // keep borrow alive
        });
    }
    row.append(&swap_btn);

    // H field.
    row.append(&small_label("H"));
    let h_spin = build_dim_spin(1.0, 32_000.0);
    row.append(&h_spin);
    row.append(&small_label("px"));

    row.append(&dim_sep());

    // Clear button.
    let clear_btn = gtk::Button::builder()
        .label("Clear")
        .valign(gtk::Align::Center)
        .build();
    clear_btn.add_css_class("flat");
    {
        let crop_c = crop.clone();
        clear_btn.connect_clicked(move |_| {
            crop_c.rect.set(None);
            crop_c.notify_rect_changed();
        });
    }
    row.append(&clear_btn);

    // Right-aligned: Cancel + Commit.
    let spacer = gtk::Box::builder().hexpand(true).build();
    row.append(&spacer);

    let cancel_btn = gtk::Button::builder()
        .label("Cancel")
        .valign(gtk::Align::Center)
        .build();
    cancel_btn.add_css_class("flat");
    {
        let crop_c = crop.clone();
        cancel_btn.connect_clicked(move |_| {
            crop_c.rect.set(None);
            crop_c.notify_rect_changed();
        });
    }
    row.append(&cancel_btn);

    let apply_btn = gtk::Button::builder()
        .label("Apply")
        .valign(gtk::Align::Center)
        .build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.connect_clicked(move |_| {
        on_apply();
    });
    row.append(&apply_btn);

    // Sync W/H spinners from crop rect when it changes.
    {
        let crop_c = crop.clone();
        let w_c = w_spin.clone();
        let h_c = h_spin.clone();
        let syncing = Rc::new(Cell::new(false));

        // Wire spin -> crop (only when NOT syncing from rect).
        {
            let crop_cc = crop_c.clone();
            let syncing_c = Rc::clone(&syncing);
            let h_cc = h_c.clone();
            w_spin.connect_value_changed(move |spin| {
                if syncing_c.get() {
                    return;
                }
                if let Some(r) = crop_cc.rect.get() {
                    let n = r.normalized();
                    use oxiedraw_core::tools::CropRect;
                    #[allow(clippy::cast_possible_truncation)]
                    crop_cc
                        .rect
                        .set(Some(CropRect::new(n.x, n.y, spin.value() as f32, n.h)));
                    // Avoid re-entering; do not notify here to prevent loop.
                }
                let _ = h_cc;
            });
        }
        {
            let crop_cc = crop_c.clone();
            let syncing_c = Rc::clone(&syncing);
            h_spin.connect_value_changed(move |spin| {
                if syncing_c.get() {
                    return;
                }
                if let Some(r) = crop_cc.rect.get() {
                    let n = r.normalized();
                    use oxiedraw_core::tools::CropRect;
                    #[allow(clippy::cast_possible_truncation)]
                    crop_cc
                        .rect
                        .set(Some(CropRect::new(n.x, n.y, n.w, spin.value() as f32)));
                }
            });
        }

        // Wire crop -> spinners via connect_rect_changed.
        crop.connect_rect_changed(Box::new(move || {
            syncing.set(true);
            if let Some(r) = crop_c.rect.get() {
                let n = r.normalized();
                w_c.set_value(f64::from(n.width_px()));
                h_c.set_value(f64::from(n.height_px()));
            } else {
                w_c.set_value(0.0);
                h_c.set_value(0.0);
            }
            syncing.set(false);
        }));
    }

    row
}

use oxiedraw_core::enum_meta::EnumMeta;
use oxiedraw_core::tools::CropAspectRatio;

fn build_ratio_dropdown(crop: &CropState) -> gtk::DropDown {
    let dropdown = gtk::DropDown::from_strings(&CropAspectRatio::labels());
    dropdown.set_selected(crop.aspect_ratio.get().to_index());

    let crop_c = crop.clone();
    dropdown.connect_selected_notify(move |d| {
        let ratio = CropAspectRatio::from_index(d.selected());
        crop_c.aspect_ratio.set(ratio);
        // Constrain existing rect to the new ratio (keep width, adjust height).
        if let (Some(r), Some(rx)) = (crop_c.rect.get(), ratio.ratio()) {
            let n = r.normalized();
            use oxiedraw_core::tools::CropRect;
            crop_c
                .rect
                .set(Some(CropRect::new(n.x, n.y, n.w, n.w / rx)));
            crop_c.notify_rect_changed();
        }
    });
    dropdown
}

fn build_dim_spin(min: f64, max: f64) -> gtk::SpinButton {
    let adj = gtk::Adjustment::new(0.0, min, max, 1.0, 10.0, 0.0);
    gtk::SpinButton::builder()
        .adjustment(&adj)
        .climb_rate(1.0)
        .digits(0)
        .numeric(true)
        .width_chars(6)
        .valign(gtk::Align::Center)
        .build()
}

fn small_label(text: &str) -> gtk::Label {
    let lbl = gtk::Label::new(Some(text));
    lbl.inline_css("font-size: 12px; opacity: 0.7;");
    lbl
}

fn dim_sep() -> gtk::Separator {
    let sep = gtk::Separator::new(gtk::Orientation::Vertical);
    sep.set_margin_top(10);
    sep.set_margin_bottom(10);
    sep.set_margin_start(4);
    sep.set_margin_end(4);
    sep
}

// ---------------------------------------------------------------------------
// Liquify page
// ---------------------------------------------------------------------------

const LIQUIFY_SLIDER_WIDTH: i32 = 110;

/// Liquify brush size segments, in the [`SIZE_SEGMENTS`] shape. Liquify works
/// at much larger radii than a brush - a single push often wants to cover a
/// whole limb or face - so this runs to 5000 px and gives the low end less of
/// the trough than the brush does.
const LIQUIFY_SIZE_SEGMENTS: [(f64, f64, f64, f64); 4] = [
    (1.0, 100.0, 1.0, 0.35),
    (100.0, 500.0, 10.0, 0.30),
    (500.0, 2000.0, 50.0, 0.20),
    (2000.0, oxiedraw_core::liquify::MAX_SIZE as f64, 100.0, 0.15),
];

/// Liquify bar: a linked (segmented) row of mode buttons on the left, the brush
/// knobs next, then Restore All / Cancel / Apply pushed to the right the way the
/// Crop tool does it.
fn build_liquify_page(liquify: &LiquifyState) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(8)
        .valign(gtk::Align::Center)
        .build();

    row.append(&build_liquify_modes(liquify));
    row.append(&dim_sep());

    // Plain labels, not the dimmed `small_label` the Crop / Shape bars use:
    // these sit next to brush-style sliders, so they match the Brush bar.
    row.append(&gtk::Label::new(Some("Size")));
    row.append(&build_liquify_size_slider(liquify));

    row.append(&gtk::Label::new(Some("Pressure")));
    row.append(&build_liquify_unit_slider(
        &liquify.strength,
        "Effect strength, and how hard stylus pressure pushes",
    ));

    row.append(&gtk::Label::new(Some("Density")));
    row.append(&build_liquify_unit_slider(
        &liquify.density,
        "Brush edge hardness: higher reaches further out at full strength",
    ));

    row.append(&gtk::Label::new(Some("Rate")));
    row.append(&build_liquify_unit_slider(
        &liquify.rate,
        "How fast Twirl / Pucker / Bloat keep applying when held still",
    ));

    // Right-aligned: Restore All + Cancel + Apply, matching the Crop bar.
    let spacer = gtk::Box::builder().hexpand(true).build();
    row.append(&spacer);

    let restore_btn = gtk::Button::builder()
        .label("Restore All")
        .tooltip_text("Undo every warp since the tool was picked up, staying in the tool")
        .valign(gtk::Align::Center)
        .action_name("app.liquify-restore")
        .build();
    restore_btn.add_css_class("flat");
    row.append(&restore_btn);

    let cancel_btn = gtk::Button::builder()
        .label("Cancel")
        .tooltip_text("Undo every warp since the tool was picked up and leave the tool")
        .valign(gtk::Align::Center)
        .action_name("app.liquify-cancel")
        .build();
    cancel_btn.add_css_class("flat");
    row.append(&cancel_btn);

    let apply_btn = gtk::Button::builder()
        .label("Apply")
        .valign(gtk::Align::Center)
        .action_name("app.liquify-apply")
        .build();
    apply_btn.add_css_class("suggested-action");
    row.append(&apply_btn);

    row
}

/// The linked mode selector. Radio-grouped toggles in a `.linked` box, so the
/// whole run reads as one segmented control.
fn build_liquify_modes(liquify: &LiquifyState) -> gtk::Box {
    let modes = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .build();

    // Set while a click is being applied programmatically, so pushing the
    // current mode back into the buttons can't re-enter the handler.
    let programmatic = Rc::new(Cell::new(false));
    let mut first: Option<gtk::ToggleButton> = None;
    let mut buttons: Vec<(LiquifyMode, gtk::ToggleButton)> = Vec::new();

    for &mode in LiquifyMode::ALL {
        let btn = gtk::ToggleButton::builder()
            .icon_name(mode.icon_name())
            .tooltip_text(liquify_tooltip(mode))
            .build();
        if let Some(ref f) = first {
            btn.set_group(Some(f));
        } else {
            first = Some(btn.clone());
        }
        btn.set_active(liquify.mode.get() == mode);
        {
            let liquify = liquify.clone();
            let programmatic = Rc::clone(&programmatic);
            btn.connect_toggled(move |b| {
                if programmatic.get() || !b.is_active() {
                    return;
                }
                liquify.mode.set(mode);
                liquify.notify_changed();
            });
        }
        modes.append(&btn);
        buttons.push((mode, btn));
    }

    // Alt-inverted modes and the keyboard both change the mode behind the bar's
    // back, so follow the state rather than assuming clicks are the only source.
    {
        let liquify = liquify.clone();
        liquify.clone().connect_changed(Box::new(move || {
            let active = liquify.mode.get();
            programmatic.set(true);
            for (mode, btn) in &buttons {
                btn.set_active(*mode == active);
            }
            programmatic.set(false);
        }));
    }

    modes
}

fn liquify_tooltip(mode: LiquifyMode) -> &'static str {
    match mode {
        LiquifyMode::ForwardWarp => "Warp - push pixels along the drag",
        LiquifyMode::Twirl => "Twirl - rotate pixels under the brush (Alt: reverse)",
        LiquifyMode::Pucker => "Pucker - pull pixels toward the centre (Alt: Bloat)",
        LiquifyMode::Bloat => "Bloat - push pixels away from the centre (Alt: Pucker)",
        LiquifyMode::PushLeft => "Push Left - shift pixels across the drag (Alt: right)",
        LiquifyMode::Reconstruct => "Reconstruct - ease warping back out",
    }
}

fn build_liquify_size_slider(liquify: &LiquifyState) -> gtk::Scale {
    let size = Rc::clone(&liquify.size);
    slider::build_mapped(
        f64::from(liquify.size.get()),
        SIZE_SLIDER_WIDTH,
        |pos| segmented_pos_to_value(pos, &LIQUIFY_SIZE_SEGMENTS),
        |value| segmented_value_to_pos(value, &LIQUIFY_SIZE_SEGMENTS),
        |value| {
            #[allow(clippy::cast_possible_truncation)]
            let v = value.round() as i32;
            format!("{v:>4}")
        },
        move |value| {
            #[allow(clippy::cast_possible_truncation)]
            size.set(value as f32);
        },
    )
}

/// A `0..=1` knob rendered as a percentage, bound to one of the liquify cells.
fn build_liquify_unit_slider(cell: &Rc<Cell<f32>>, tooltip: &str) -> gtk::Scale {
    let cell = Rc::clone(cell);
    let initial = f64::from(cell.get());
    let scale = slider::build(
        (0.0, 1.0),
        OPACITY_STEP,
        initial,
        LIQUIFY_SLIDER_WIDTH,
        |value| {
            #[allow(clippy::cast_possible_truncation)]
            let pct = (value * 100.0).round() as i32;
            format!("{pct:>3}%")
        },
        move |value| {
            #[allow(clippy::cast_possible_truncation)]
            cell.set(value as f32);
        },
    );
    scale.set_tooltip_text(Some(tooltip));
    scale
}

// ---------------------------------------------------------------------------
// Transform page
// ---------------------------------------------------------------------------

const TRANSFORM_FILTERS: [TransformFilter; 2] =
    [TransformFilter::Bilinear, TransformFilter::NearestNeighbor];

fn build_transform_page(
    transform: &TransformState,
    on_apply: Rc<dyn Fn()>,
    on_cancel: Rc<dyn Fn()>,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(8)
        .valign(gtk::Align::Center)
        .build();

    row.append(&small_label("Filter"));
    row.append(&build_filter_dropdown(transform.filter.clone()));

    let spacer = gtk::Box::builder().hexpand(true).build();
    row.append(&spacer);

    let cancel_btn = gtk::Button::builder()
        .label("Cancel")
        .valign(gtk::Align::Center)
        .build();
    cancel_btn.add_css_class("flat");
    cancel_btn.connect_clicked(move |_| {
        on_cancel();
    });
    row.append(&cancel_btn);

    let apply_btn = gtk::Button::builder()
        .label("Apply")
        .valign(gtk::Align::Center)
        .build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.connect_clicked(move |_| {
        on_apply();
    });
    row.append(&apply_btn);

    row
}

// ---------------------------------------------------------------------------
// Fill page
// ---------------------------------------------------------------------------

fn build_fill_page(fill: &FillState) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(ROW_SPACING)
        .margin_start(6)
        .margin_end(LABEL_MARGIN)
        .valign(gtk::Align::Center)
        .build();

    row.append(&gtk::Label::new(Some("Tolerance")));

    let tolerance = fill.tolerance.clone();
    let initial = f64::from(tolerance.get());
    let slider = slider::build(
        (0.0, 255.0),
        1.0,
        initial,
        TOLERANCE_SLIDER_WIDTH,
        |value| {
            #[allow(clippy::cast_possible_truncation)]
            let v = value.round() as i32;
            format!("{v:>3}")
        },
        move |value| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let v = value.round().clamp(0.0, 255.0) as u8;
            tolerance.set(v);
        },
    );
    row.append(&slider);

    // Dragging sideways during a fill adjusts the same threshold; let it
    // move the slider so the two never show different numbers.
    {
        let scale = slider.clone();
        *fill.tolerance_display.borrow_mut() = Some(Box::new(move |value: u8| {
            scale.set_value(f64::from(value));
        }));
    }

    // What makes fills meet anti-aliased line art cleanly. No radius or
    // feather to go with it - the edge pass reads the outline itself.
    let auto_edge = fill.auto_edge.clone();
    let auto_check = gtk::CheckButton::builder()
        .label("Smart Edges")
        .tooltip_text(
            "Carry the fill across anti-aliased outlines and keep their edge blending intact",
        )
        .active(auto_edge.get())
        .valign(gtk::Align::Center)
        .build();
    auto_check.connect_toggled(move |c| auto_edge.set(c.is_active()));
    row.append(&auto_check);

    // Sample the composite of all visible layers instead of just the
    // active one when deciding which pixels to fill.
    let all_layers = fill.sample_all_layers.clone();
    let check = gtk::CheckButton::builder()
        .label("Use all Layers")
        .active(all_layers.get())
        .valign(gtk::Align::Center)
        .build();
    check.connect_toggled(move |c| all_layers.set(c.is_active()));
    row.append(&check);

    row
}

fn build_filter_dropdown(filter: Rc<Cell<TransformFilter>>) -> gtk::DropDown {
    let names: Vec<&str> = TRANSFORM_FILTERS.iter().map(|f| f.display_name()).collect();
    let dropdown = gtk::DropDown::from_strings(&names);
    let initial = TRANSFORM_FILTERS
        .iter()
        .position(|&f| f == filter.get())
        .unwrap_or(0);
    #[allow(clippy::cast_possible_truncation)]
    dropdown.set_selected(initial as u32);

    dropdown.connect_selected_notify(move |d| {
        let idx = d.selected() as usize;
        if let Some(&f) = TRANSFORM_FILTERS.get(idx) {
            filter.set(f);
        }
    });
    dropdown
}

// ---------------------------------------------------------------------------
// Shape page
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Text page (B / I / U)
// ---------------------------------------------------------------------------

fn build_text_page(
    text_edit: &Rc<std::cell::RefCell<Option<crate::text_edit::TextEdit>>>,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .margin_start(6)
        .margin_end(LABEL_MARGIN)
        .valign(gtk::Align::Center)
        .build();

    use crate::text_edit::TextEdit;
    row.append(&style_button("<b>B</b>", text_edit, TextEdit::toggle_bold));
    row.append(&style_button("<i>I</i>", text_edit, TextEdit::toggle_italic));
    row.append(&style_button("<u>U</u>", text_edit, TextEdit::toggle_underline));
    row
}

/// A B/I/U toolbar button: a flat button with a Pango-markup label that
/// dispatches to the (late-bound) text-edit controller on click.
fn style_button(
    markup: &str,
    text_edit: &Rc<std::cell::RefCell<Option<crate::text_edit::TextEdit>>>,
    op: impl Fn(&crate::text_edit::TextEdit) + 'static,
) -> gtk::Button {
    let label = gtk::Label::new(None);
    label.set_markup(markup);
    let btn = gtk::Button::builder().child(&label).valign(gtk::Align::Center).build();
    btn.add_css_class("flat");
    let text_edit = Rc::clone(text_edit);
    btn.connect_clicked(move |_| {
        if let Some(te) = text_edit.borrow().as_ref() {
            op(te);
        }
    });
    btn
}

fn build_shape_page(shape: &ShapeState) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(ROW_SPACING)
        .margin_start(6)
        .margin_end(LABEL_MARGIN)
        .valign(gtk::Align::Center)
        .build();

    row.append(&small_label("Edges"));
    row.append(&build_filter_dropdown(shape.filter.clone()));

    row
}

// ---------------------------------------------------------------------------
// Gradient page
// ---------------------------------------------------------------------------

const GRADIENT_TYPES: [GradientType; 3] =
    [GradientType::Linear, GradientType::Radial, GradientType::Square];

fn build_gradient_page(gradient: &GradientState) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(ROW_SPACING)
        .margin_start(6)
        .margin_end(LABEL_MARGIN)
        .valign(gtk::Align::Center)
        .build();

    row.append(&small_label("Type"));

    let names: Vec<&str> = GRADIENT_TYPES.iter().map(|t| t.display_name()).collect();
    let dropdown = gtk::DropDown::from_strings(&names);
    let initial = GRADIENT_TYPES
        .iter()
        .position(|&t| t == gradient.gradient_type.get())
        .unwrap_or(0);
    #[allow(clippy::cast_possible_truncation)]
    dropdown.set_selected(initial as u32);
    {
        let gradient = gradient.clone();
        dropdown.connect_selected_notify(move |d| {
            let idx = d.selected() as usize;
            if let Some(&t) = GRADIENT_TYPES.get(idx) {
                gradient.gradient_type.set(t);
            }
        });
    }
    row.append(&dropdown);

    row
}

#[cfg(test)]
// Exact comparisons are the point: the segment tables are authored so that
// specific trough positions land on specific snapped sizes.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    /// A segment table's widths have to cover the trough exactly, or part of it
    /// maps nowhere and the top of the range becomes unreachable.
    fn assert_widths_sum_to_one(segments: &[(f64, f64, f64, f64)]) {
        let total: f64 = segments.iter().map(|s| s.3).sum();
        assert!((total - 1.0).abs() < 1e-9, "segment widths sum to {total}");
    }

    #[test]
    fn segment_tables_cover_the_whole_trough() {
        assert_widths_sum_to_one(&SIZE_SEGMENTS);
        assert_widths_sum_to_one(&LIQUIFY_SIZE_SEGMENTS);
    }

    /// Adjacent segments must share a boundary value, otherwise dragging across
    /// one jumps.
    #[test]
    fn segment_tables_are_contiguous() {
        for table in [&SIZE_SEGMENTS[..], &LIQUIFY_SIZE_SEGMENTS[..]] {
            for pair in table.windows(2) {
                assert_eq!(pair[0].1, pair[1].0, "gap between segments in {table:?}");
            }
        }
    }

    #[test]
    fn liquify_size_reaches_five_thousand() {
        assert_eq!(
            segmented_pos_to_value(1.0, &LIQUIFY_SIZE_SEGMENTS),
            f64::from(oxiedraw_core::liquify::MAX_SIZE),
        );
        // ... and the brush is unchanged by the refactor.
        assert_eq!(segmented_pos_to_value(1.0, &SIZE_SEGMENTS), 1000.0);
    }

    #[test]
    fn liquify_size_snaps_to_its_segment_step() {
        // Inside the first segment (step 1) every value is a whole pixel.
        for i in 0..=35 {
            let v = segmented_pos_to_value(f64::from(i) / 100.0, &LIQUIFY_SIZE_SEGMENTS);
            assert_eq!(v, v.round(), "step-1 segment produced {v}");
        }
        // The last segment steps in hundreds.
        let v = segmented_pos_to_value(0.93, &LIQUIFY_SIZE_SEGMENTS);
        assert_eq!(v % 100.0, 0.0, "step-100 segment produced {v}");
        assert!((2000.0..=5000.0).contains(&v), "{v} outside the last segment");
    }

    /// Position and value have to be inverses at the segment boundaries, or the
    /// thumb jumps when the slider is seeded from a stored size.
    #[test]
    fn liquify_position_round_trips_at_boundaries() {
        for &(lo, ..) in &LIQUIFY_SIZE_SEGMENTS {
            let pos = segmented_value_to_pos(lo, &LIQUIFY_SIZE_SEGMENTS);
            let back = segmented_pos_to_value(pos, &LIQUIFY_SIZE_SEGMENTS);
            assert_eq!(back, lo, "{lo} round-tripped to {back}");
        }
    }

    #[test]
    fn liquify_default_size_is_in_range() {
        let default = f64::from(oxiedraw_core::liquify::DEFAULT_SIZE);
        let pos = segmented_value_to_pos(default, &LIQUIFY_SIZE_SEGMENTS);
        assert!((0.0..=1.0).contains(&pos));
        assert_eq!(segmented_pos_to_value(pos, &LIQUIFY_SIZE_SEGMENTS), default);
    }
}
