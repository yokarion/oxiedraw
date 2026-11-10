//! Right-pane editor for the Manage Brushes window.
//!
//! Each field is bound to a widget that writes its value into the
//! in-memory `BrushPreset` and flags the editor dirty; nothing touches
//! disk until the user clicks Save. Discard restores the brush from the
//! `baseline` snapshot taken when it was selected (or last saved). The
//! host guards brush-switches and window-close against unsaved edits.
//!
//! A `loading` flag suppresses `connect_value_changed` callbacks
//! during `set_brush` so populating the widgets from a freshly
//! selected brush doesn't flag a phantom edit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::brush_engine::{
    BrushEngine, BrushFamily, BrushPreset, BrushPresetId, BrushRegistry, Curve,
    DEFAULT_AIRBRUSH_RATE, DynSource, Dynamics, Mapping, PaintMode, PatternData, TexturingMode,
    format, preview_renderer,
};
use oxiedraw_core::curves;
use oxiedraw_core::enum_meta::EnumMeta;
use relm4::gtk;
use relm4::gtk::glib;

use crate::brush_picker::shared as picker;
use crate::widgets::curve_editor::CurveEditor;

use super::preview;

/// Handles returned from `editor::build` so the host can drive the
/// editor from outside: load a brush, focus the name field after
/// creating a new one, and mediate unsaved edits (dirty flag + explicit
/// save/discard) when the host switches brushes or closes the window.
pub(super) struct EditorHandles {
    pub set_brush: Rc<dyn Fn(Option<&BrushPreset>)>,
    pub focus_name: Rc<dyn Fn()>,
    pub is_dirty: Rc<Cell<bool>>,
    pub save: Rc<dyn Fn()>,
    pub discard: Rc<dyn Fn()>,
}

const FALLBACK_ICON: &str = "oxiedraw-brush-symbolic";
const TAU_F32: f32 = std::f32::consts::TAU;
const DYN_SOURCES: &[(DynSource, &str)] = &[
    (DynSource::Pressure, "Pressure"),
    (DynSource::Speed, "Speed"),
    (DynSource::Direction, "Direction"),
    (DynSource::Distance, "Distance"),
    (DynSource::Random, "Random"),
    (DynSource::PenRotation, "Pen Rotation"),
    (DynSource::FakePenRotation, "Fake Pen Rotation"),
    (DynSource::Angle, "Angle"),
    (DynSource::FakeAngle, "Fake Angle"),
];

/// Build the right pane. Returns the outer widget plus an
/// [`EditorHandles`] the host uses to drive the editor.
pub(super) fn build(
    parent: gtk::Window,
    brush_engine: BrushEngine,
    selected_id: Rc<Cell<Option<BrushPresetId>>>,
    selected_name: Rc<RefCell<Option<String>>>,
) -> (gtk::Widget, EditorHandles) {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();

    // Right-pane header bar - shows the window close button and
    // keeps the header-bar area on the right side so the left
    // sidebar header and right header sit at the same height.
    let right_header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .title_widget(&gtk::Label::new(None))
        .build();
    outer.append(&right_header);

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(24)
        .margin_end(24)
        .build();

    // -- Identity (name) -------------------------------------------------
    let identity_group = adw::PreferencesGroup::builder().build();
    let name_entry = adw::EntryRow::builder().title("Name").build();
    identity_group.add(&name_entry);
    body.append(&identity_group);

    // -- Loading flag + dirty / baseline state ---------------------------
    let loading = Rc::new(Cell::new(false));
    // True once the current brush has unsaved edits. `baseline` is the
    // last-saved (or as-selected) snapshot, used to revert on Discard.
    let dirty = Rc::new(Cell::new(false));
    let baseline: Rc<RefCell<Option<BrushPreset>>> = Rc::new(RefCell::new(None));

    // Save / Discard action bar - revealed (with an animated height
    // slide) only while there are unsaved edits. Built here so
    // `mark_dirty` can reveal it; its click handlers are wired once the
    // setter exists, further down.
    let (save_bar, save_btn, discard_btn) = build_save_bar();

    // Any edit flags dirty and slides the action bar in.
    let mark_dirty: Rc<dyn Fn()> = {
        let dirty = dirty.clone();
        let save_bar = save_bar.clone();
        Rc::new(move || {
            dirty.set(true);
            save_bar.set_reveal_child(true);
        })
    };

    // -- Icon ------------------------------------------------------------
    let icon_group = adw::PreferencesGroup::builder().title("Icon").build();
    let icon_row = adw::ActionRow::builder().title("Custom icon").build();
    let icon_image = gtk::Image::builder().pixel_size(48).build();
    icon_image.set_icon_name(Some(FALLBACK_ICON));
    icon_row.add_prefix(&icon_image);
    let choose_btn = gtk::Button::with_label("Choose...");
    choose_btn.add_css_class("flat");
    icon_row.add_suffix(&choose_btn);
    icon_group.add(&icon_row);
    body.append(&icon_group);

    // -- Properties (editable) -------------------------------------------
    let props_group = adw::PreferencesGroup::builder().title("Properties").build();

    let family_row = adw::ActionRow::builder().title("Family").build();
    let family_dropdown =
        gtk::DropDown::from_strings(&["Soft round", "Pixel", "Textured", "Smudge"]);
    family_dropdown.set_valign(gtk::Align::Center);
    family_row.add_suffix(&family_dropdown);
    props_group.add(&family_row);

    // Smudge only: stamped image tip, replaces the round hardness tip.
    let tip_row = adw::ActionRow::builder()
        .title("Brush tip")
        .subtitle("Grayscale image, dark paints")
        .build();
    let tip_thumb = gtk::Image::builder().pixel_size(36).build();
    tip_thumb.set_icon_name(Some("image-x-generic-symbolic"));
    tip_row.add_prefix(&tip_thumb);
    let tip_clear_btn = clear_button("Use the round tip");
    tip_row.add_suffix(&tip_clear_btn);
    let tip_btn = gtk::Button::with_label("Choose tip...");
    tip_btn.add_css_class("flat");
    tip_row.add_suffix(&tip_btn);
    tip_row.set_visible(false);
    props_group.add(&tip_row);
    let smear_alpha_row = adw::ActionRow::builder()
        .title("Smear alpha")
        .subtitle("Drag transparency along too, instead of filling it with the brush color")
        .build();
    let smear_alpha_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    smear_alpha_row.add_suffix(&smear_alpha_switch);
    smear_alpha_row.set_visible(false);
    props_group.add(&smear_alpha_row);

    // Pattern row - only visible for the textured, image-tip and smudge families.
    let pattern_row = adw::ActionRow::builder().title("Pattern").build();
    let pattern_thumb = gtk::Image::builder().pixel_size(36).build();
    pattern_thumb.set_icon_name(Some("image-x-generic-symbolic"));
    pattern_row.add_prefix(&pattern_thumb);
    let pattern_clear_btn = clear_button("Remove the texture");
    pattern_row.add_suffix(&pattern_clear_btn);
    let pattern_btn = gtk::Button::with_label("Choose pattern...");
    pattern_btn.add_css_class("flat");
    pattern_row.add_suffix(&pattern_btn);
    pattern_row.set_visible(false);
    props_group.add(&pattern_row);

    let (size_row, size_scale) = build_scale_row("Default size", 1.0, 1000.0, 0.5);
    props_group.add(&size_row);
    let (opacity_row, opacity_scale) = build_scale_row("Default opacity", 0.0, 1.0, 0.01);
    props_group.add(&opacity_row);
    let (spacing_row, spacing_scale) = build_scale_row("Spacing ratio", 0.0, 1.0, 0.01);
    props_group.add(&spacing_row);
    let (stabilizer_row, stabilizer_scale) =
        build_scale_row("Stabilizer", 0.0, 0.95, 0.01);
    props_group.add(&stabilizer_row);
    let (speed_smoothing_row, speed_smoothing_scale) =
        build_scale_row("Speed smoothing", 0.0, 1.0, 0.01);
    props_group.add(&speed_smoothing_row);
    let (hardness_row, hardness_scale) = build_scale_row("Hardness", 0.0, 1.0, 0.01);
    hardness_row.set_subtitle("Edge softness - 1.0 crisp, low is a soft airbrush edge");
    props_group.add(&hardness_row);
    // Soft round only: Krita's soft tip, which replaces hardness when on.
    let softness_row = adw::ExpanderRow::builder()
        .title("Softness curve")
        .subtitle("Coverage from the centre (left) to the edge (right), replaces hardness")
        .show_enable_switch(true)
        .enable_expansion(false)
        .build();
    let softness_editor = CurveEditor::new();
    softness_editor.widget.set_margin_top(8);
    softness_editor.widget.set_margin_bottom(8);
    softness_editor.widget.set_margin_start(12);
    softness_editor.widget.set_margin_end(12);
    softness_row.add_row(&softness_editor.widget);
    props_group.add(&softness_row);
    // Textured-only: size of the global canvas-anchored pattern (px per
    // tile) and how strongly it gates coverage. Hidden for other families.
    let (pattern_size_row, pattern_size_scale) =
        build_scale_row("Pattern size", 16.0, 1024.0, 1.0);
    pattern_size_row.set_subtitle("Grain tile size in canvas pixels");
    pattern_size_row.set_visible(false);
    props_group.add(&pattern_size_row);
    let (pattern_strength_row, pattern_strength_scale) =
        build_scale_row("Pattern strength", 0.0, 1.0, 0.01);
    pattern_strength_row.set_visible(false);
    props_group.add(&pattern_strength_row);
    let texture_mode_row = adw::ActionRow::builder()
        .title("Pattern mode")
        .subtitle("Multiply darkens coverage, subtract carves holes")
        .build();
    let texture_mode_dropdown = gtk::DropDown::from_strings(&TexturingMode::labels());
    texture_mode_dropdown.set_valign(gtk::Align::Center);
    texture_mode_row.add_suffix(&texture_mode_dropdown);
    props_group.add(&texture_mode_row);
    let (texture_brightness_row, texture_brightness_scale) =
        build_scale_row("Pattern brightness", -1.0, 1.0, 0.01);
    props_group.add(&texture_brightness_row);
    let (texture_contrast_row, texture_contrast_scale) =
        build_scale_row("Pattern contrast", 0.0, 2.0, 0.01);
    props_group.add(&texture_contrast_row);
    let texture_invert_row = adw::ActionRow::builder().title("Invert pattern").build();
    let texture_invert_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    texture_invert_row.add_suffix(&texture_invert_switch);
    props_group.add(&texture_invert_row);
    // Every row that only applies when the family has a pattern.
    let texture_rows: Rc<[gtk::Widget]> = Rc::new([
        pattern_row.clone().upcast(),
        pattern_size_row.clone().upcast(),
        pattern_strength_row.clone().upcast(),
        texture_mode_row.upcast(),
        texture_brightness_row.upcast(),
        texture_contrast_row.upcast(),
        texture_invert_row.upcast(),
    ]);
    for row in texture_rows.iter() {
        row.set_visible(false);
    }
    let paint_mode_row = adw::ActionRow::builder()
        .title("Paint mode")
        .subtitle("Build up stacks overlaps, wash builds up to the dab opacity (soft round only)")
        .build();
    let paint_mode_dropdown = gtk::DropDown::from_strings(&PaintMode::labels());
    paint_mode_dropdown.set_valign(gtk::Align::Center);
    paint_mode_row.add_suffix(&paint_mode_dropdown);
    props_group.add(&paint_mode_row);
    let airbrush_row = adw::ExpanderRow::builder()
        .title("Airbrush")
        .subtitle("Keep spraying while the pen holds still")
        .show_enable_switch(true)
        .enable_expansion(false)
        .build();
    let (airbrush_rate_row, airbrush_rate_scale) = build_scale_row("Rate", 1.0, 1000.0, 1.0);
    airbrush_rate_row.set_subtitle("Dabs per second");
    airbrush_rate_scale.set_digits(0);
    airbrush_row.add_row(&airbrush_rate_row);
    props_group.add(&airbrush_row);
    body.append(&props_group);

    // -- Name entry -> brush.name + shared selected_name ------------------
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let selected_name = selected_name.clone();
        let loading = loading.clone();
        let mark_dirty = mark_dirty.clone();
        name_entry.connect_changed(move |entry| {
            if loading.get() {
                return;
            }
            let Some(id) = selected_id.get() else { return };
            let new_name = entry.text().to_string();
            // Track the new name in `selected_name` so the
            // brushes-changed listener re-selects this brush after the
            // save -> watcher reload round-trip.
            *selected_name.borrow_mut() = Some(new_name.clone());
            let mut brushes = brush_engine.brushes.borrow_mut();
            if let Some(brush) = brushes.iter_mut().find(|p| p.id == id) {
                brush.name = new_name;
            }
            drop(brushes);
            mark_dirty();
        });
    }

    // Rows that depend on the family's tip / pattern images.
    let refresh_family_rows: Rc<dyn Fn(&BrushFamily)> = {
        let texture_rows = texture_rows.clone();
        let pattern_thumb = pattern_thumb.clone();
        let pattern_clear_btn = pattern_clear_btn.clone();
        let tip_row = tip_row.clone();
        let tip_thumb = tip_thumb.clone();
        let tip_clear_btn = tip_clear_btn.clone();
        let smear_alpha_row = smear_alpha_row.clone();
        Rc::new(move |family: &BrushFamily| {
            apply_pattern_visibility(&texture_rows, &pattern_thumb, &pattern_clear_btn, family);
            apply_tip_visibility(&tip_row, &tip_thumb, &tip_clear_btn, family);
            smear_alpha_row.set_visible(family.is_smudge());
        })
    };

    // Built here so tip / pattern edits can refresh it; appended under the body.
    let (preview_area, set_preview) = preview::build();
    let live_preview: Rc<dyn Fn()> = {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let set_preview = set_preview.clone();
        Rc::new(move || {
            if let Some(id) = selected_id.get() {
                let brushes = brush_engine.brushes.borrow();
                if let Some(p) = brushes.iter().find(|p| p.id == id) {
                    set_preview(Some(p));
                }
            }
        })
    };

    // -- Family dropdown -> brush.family ----------------------------------
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let loading = loading.clone();
        let mark_dirty = mark_dirty.clone();
        let refresh_family_rows = refresh_family_rows.clone();
        let pattern_size_scale = pattern_size_scale.clone();
        let pattern_strength_scale = pattern_strength_scale.clone();
        let loading_for_family = loading.clone();
        family_dropdown.connect_selected_notify(move |d| {
            if loading.get() {
                return;
            }
            let Some(id) = selected_id.get() else { return };
            let idx = d.selected();
            let mut brushes = brush_engine.brushes.borrow_mut();
            let Some(brush) = brushes.iter_mut().find(|p| p.id == id) else { return };
            brush.family = match idx {
                0 => BrushFamily::SoftRound,
                1 => BrushFamily::Pixel,
                3 => match &brush.family {
                    BrushFamily::Smudge { .. } => brush.family.clone(),
                    _ => BrushFamily::smudge(),
                },
                _ => match &brush.family {
                    BrushFamily::Textured(rc) => BrushFamily::Textured(rc.clone()),
                    // First-time switch to Textured: seed with the
                    // synthesised chalk grain so the brush stays visually
                    // meaningful until the user picks a pattern.
                    _ => BrushFamily::Textured(Rc::new(PatternData::chalk_grain(512))),
                },
            };
            // A textured brush with no pattern size shows no grain, so give
            // fresh conversions sensible defaults.
            if matches!(brush.family, BrushFamily::Textured(_)) && brush.texture_scale <= 0.0 {
                brush.texture_scale = 200.0;
                brush.texture_strength = 0.85;
                loading_for_family.set(true);
                pattern_size_scale.set_value(f64::from(brush.texture_scale));
                pattern_strength_scale.set_value(f64::from(brush.texture_strength));
                loading_for_family.set(false);
            }
            let updated_family = brush.family.clone();
            drop(brushes);
            refresh_family_rows(&updated_family);
            mark_dirty();
        });
    }

    // Apply an edit to the selected brush's family, then refresh its rows.
    let edit_family: Rc<dyn Fn(&dyn Fn(&mut BrushPreset))> = {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let mark_dirty = mark_dirty.clone();
        let refresh_family_rows = refresh_family_rows.clone();
        let live_preview = live_preview.clone();
        Rc::new(move |apply: &dyn Fn(&mut BrushPreset)| {
            let Some(id) = selected_id.get() else { return };
            let mut brushes = brush_engine.brushes.borrow_mut();
            let Some(brush) = brushes.iter_mut().find(|p| p.id == id) else { return };
            apply(brush);
            let family = brush.family.clone();
            drop(brushes);
            refresh_family_rows(&family);
            mark_dirty();
            live_preview();
        })
    };

    // -- Pattern picker -> brush.family -----------------------------------
    {
        let edit_family = edit_family.clone();
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let parent = parent.clone();
        pattern_btn.connect_clicked(move |_| {
            let Some(id) = selected_id.get() else { return };
            // Smudge textures follow Krita (grey over white); the textured
            // family keeps its alpha-mask patterns.
            let smudge = brush_engine
                .brushes
                .borrow()
                .iter()
                .any(|p| p.id == id && p.family.is_smudge());
            let decode: fn(&[u8]) -> Result<PatternData, String> = if smudge {
                |bytes| PatternData::texture_from_png_bytes(bytes, 0.0, 1.0, false)
            } else {
                PatternData::from_png_bytes
            };
            let edit_family = edit_family.clone();
            let parent_for_cb = parent.clone();
            choose_png(&parent, "Choose brush pattern", decode, move |result| match result {
                Ok(data) => {
                    let rc = Rc::new(data);
                    edit_family(&|brush| {
                        if let BrushFamily::Smudge { grain, .. } = &mut brush.family {
                            *grain = Some(rc.clone());
                            if brush.texture_scale <= 0.0 {
                                brush.texture_scale = 200.0;
                                brush.texture_strength = 1.0;
                            }
                        } else {
                            brush.family = BrushFamily::Textured(rc.clone());
                        }
                    });
                }
                Err(Some(e)) => {
                    super::show_simple_error(&parent_for_cb, "Couldn't load pattern", &e);
                }
                Err(None) => { /* user cancelled */ }
            });
        });
    }
    {
        let edit_family = edit_family.clone();
        pattern_clear_btn.connect_clicked(move |_| {
            edit_family(&|brush| {
                if let BrushFamily::Smudge { grain, .. } = &mut brush.family {
                    *grain = None;
                }
            });
        });
    }

    // -- Tip picker -> brush.family (Smudge) ------------------------------
    {
        let edit_family = edit_family.clone();
        let parent = parent.clone();
        tip_btn.connect_clicked(move |_| {
            let edit_family = edit_family.clone();
            let parent_for_cb = parent.clone();
            choose_png(&parent, "Choose brush tip", PatternData::tip_from_png_bytes, move |result| {
                match result {
                    Ok(data) => {
                        let rc = Rc::new(data);
                        edit_family(&|brush| {
                            if let BrushFamily::Smudge { tip, .. } = &mut brush.family {
                                *tip = Some(rc.clone());
                            }
                        });
                    }
                    Err(Some(e)) => {
                        super::show_simple_error(&parent_for_cb, "Couldn't load brush tip", &e);
                    }
                    Err(None) => { /* user cancelled */ }
                }
            });
        });
    }
    {
        let edit_family = edit_family.clone();
        let loading = loading.clone();
        smear_alpha_switch.connect_state_set(move |_, state| {
            if !loading.get() {
                edit_family(&|brush| {
                    if let BrushFamily::Smudge { smear_alpha, .. } = &mut brush.family {
                        *smear_alpha = state;
                    }
                });
            }
            glib::Propagation::Proceed
        });
    }
    {
        let edit_family = edit_family.clone();
        tip_clear_btn.connect_clicked(move |_| {
            edit_family(&|brush| {
                if let BrushFamily::Smudge { tip, .. } = &mut brush.family {
                    *tip = None;
                }
            });
        });
    }

    // Wire scales -> in-memory + debounced save.
    wire_scale_to_field(
        &size_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.default_size = v as f32,
    );
    wire_scale_to_field(
        &opacity_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.default_opacity = v as f32,
    );
    wire_scale_to_field(
        &spacing_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.spacing_ratio = v as f32,
    );
    wire_scale_to_field(
        &stabilizer_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.stabilizer = v as f32,
    );
    wire_scale_to_field(
        &speed_smoothing_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.speed_smoothing = v as f32,
    );
    wire_scale_to_field(
        &hardness_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.hardness = v as f32,
    );
    wire_scale_to_field(
        &pattern_size_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.texture_scale = v as f32,
    );
    wire_scale_to_field(
        &pattern_strength_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.texture_strength = v as f32,
    );
    wire_scale_to_field(
        &airbrush_rate_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.airbrush_rate = v as f32,
    );
    wire_scale_to_field(
        &texture_brightness_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.texture_adjust.brightness = v as f32,
    );
    wire_scale_to_field(
        &texture_contrast_scale,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
        |b, v| b.texture_adjust.contrast = v as f32,
    );
    let edit_brush = {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let loading = loading.clone();
        let mark_dirty = mark_dirty.clone();
        Rc::new(move |apply: &dyn Fn(&mut BrushPreset)| {
            if loading.get() {
                return;
            }
            let Some(id) = selected_id.get() else { return };
            if let Some(brush) = brush_engine.brushes.borrow_mut().iter_mut().find(|p| p.id == id) {
                apply(brush);
            }
            mark_dirty();
        })
    };
    {
        let edit_brush = edit_brush.clone();
        paint_mode_dropdown.connect_selected_notify(move |d| {
            let mode = PaintMode::from_index(d.selected());
            edit_brush(&|b| b.paint_mode = mode);
        });
    }
    {
        let edit_brush = edit_brush.clone();
        texture_mode_dropdown.connect_selected_notify(move |d| {
            let mode = TexturingMode::from_index(d.selected());
            edit_brush(&|b| b.texturing_mode = mode);
        });
    }
    {
        let edit_brush = edit_brush.clone();
        texture_invert_switch.connect_state_set(move |_, state| {
            edit_brush(&|b| b.texture_adjust.invert = state);
            glib::Propagation::Proceed
        });
    }
    {
        let edit_brush = edit_brush.clone();
        airbrush_row.connect_enable_expansion_notify(move |row| {
            let on = row.enables_expansion();
            edit_brush(&|b| b.airbrush = on);
        });
    }
    {
        let edit_brush = edit_brush.clone();
        let editor = softness_editor.clone();
        let hardness_row = hardness_row.clone();
        softness_row.connect_enable_expansion_notify(move |row| {
            let on = row.enables_expansion();
            hardness_row.set_sensitive(!on);
            let curve = on.then(default_softness_curve);
            if let Some(c) = &curve {
                editor.set_curve(to_editor_curve(c));
            }
            edit_brush(&|b| b.softness_curve.clone_from(&curve));
        });
    }
    {
        let edit_brush = edit_brush.clone();
        softness_editor.connect_changed(move |curve| {
            let curve = from_editor_curve(&curve).unwrap_or_else(default_softness_curve);
            edit_brush(&|b| b.softness_curve = Some(curve.clone()));
        });
    }

    // -- Dynamics expanders ----------------------------------------------
    let dyn_group = adw::PreferencesGroup::builder().title("Dynamics").build();
    let size_dyn = build_dynamics_row(
        "Size",
        "Multiplier on dab diameter",
        DynamicsField::Size,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
    );
    dyn_group.add(&size_dyn.row);
    let flow_dyn = build_dynamics_row(
        "Flow",
        "Per-dab coverage attenuation",
        DynamicsField::Flow,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
    );
    dyn_group.add(&flow_dyn.row);
    let opacity_dyn = build_dynamics_row(
        "Opacity",
        "Per-dab opacity, the ceiling a wash stroke builds up to",
        DynamicsField::Opacity,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
    );
    dyn_group.add(&opacity_dyn.row);
    let rotation_dyn = build_dynamics_row(
        "Rotation",
        "Additive rotation, degrees",
        DynamicsField::Rotation,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
    );
    dyn_group.add(&rotation_dyn.row);
    let scatter_dyn = build_dynamics_row(
        "Scatter",
        "Random offset from path, pixels",
        DynamicsField::Scatter,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
    );
    dyn_group.add(&scatter_dyn.row);
    let spacing_dyn = build_dynamics_row(
        "Spacing",
        "Dab step as a fraction of brush diameter",
        DynamicsField::Spacing,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
    );
    dyn_group.add(&spacing_dyn.row);
    let smudge_rate_dyn = build_dynamics_row(
        "Smudge rate",
        "How much existing paint is dragged along, 1 = full",
        DynamicsField::SmudgeRate,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
    );
    dyn_group.add(&smudge_rate_dyn.row);
    let color_rate_dyn = build_dynamics_row(
        "Color rate",
        "How much brush color mixes into the smear",
        DynamicsField::ColorRate,
        &brush_engine,
        &selected_id,
        &loading,
        &mark_dirty,
    );
    dyn_group.add(&color_rate_dyn.row);
    // Smudge rows follow the family dropdown, including while a brush loads.
    {
        let smudge_rows = [smudge_rate_dyn.row.clone(), color_rate_dyn.row.clone()];
        let softness_row = softness_row.clone();
        let sync = move |d: &gtk::DropDown| {
            let smudge = d.selected() == family_to_dropdown_index(&BrushFamily::smudge());
            for row in &smudge_rows {
                row.set_visible(smudge);
            }
            softness_row.set_visible(d.selected() == family_to_dropdown_index(&BrushFamily::SoftRound));
        };
        sync(&family_dropdown);
        family_dropdown.connect_selected_notify(sync);
    }
    body.append(&dyn_group);

    // -- Delete ----------------------------------------------------------
    let delete_btn = gtk::Button::with_label("Delete Brush");
    delete_btn.add_css_class("destructive-action");
    delete_btn.add_css_class("pill");
    delete_btn.set_halign(gtk::Align::Center);
    delete_btn.set_margin_top(8);
    body.append(&delete_btn);

    scrolled.set_child(Some(&body));
    outer.append(&scrolled);

    // -- Preview ---------------------------------------------------------
    let preview_frame = gtk::Frame::builder().build();
    preview_frame.set_child(Some(&preview_area));
    preview_frame.set_margin_start(24);
    preview_frame.set_margin_end(24);
    preview_frame.set_margin_top(8);
    preview_frame.set_margin_bottom(12);
    outer.append(&preview_frame);

    // Save / Discard action bar sits at the very bottom, under the preview.
    outer.append(&save_bar);

    // -- Setter: load a brush into all widgets ---------------------------
    let setter: Rc<dyn Fn(Option<&BrushPreset>)> = {
        let name_entry = name_entry.clone();
        let icon_image = icon_image.clone();
        let family_dropdown = family_dropdown.clone();
        let refresh_family_rows = refresh_family_rows.clone();
        let pattern_thumb = pattern_thumb.clone();
        let pattern_btn = pattern_btn.clone();
        let tip_btn = tip_btn.clone();
        let texture_mode_dropdown = texture_mode_dropdown.clone();
        let texture_brightness_scale = texture_brightness_scale.clone();
        let texture_contrast_scale = texture_contrast_scale.clone();
        let texture_invert_switch = texture_invert_switch.clone();
        let smear_alpha_switch = smear_alpha_switch.clone();
        let size_scale = size_scale.clone();
        let opacity_scale = opacity_scale.clone();
        let spacing_scale = spacing_scale.clone();
        let stabilizer_scale = stabilizer_scale.clone();
        let speed_smoothing_scale = speed_smoothing_scale.clone();
        let hardness_scale = hardness_scale.clone();
        let pattern_size_scale = pattern_size_scale.clone();
        let pattern_strength_scale = pattern_strength_scale.clone();
        let paint_mode_dropdown = paint_mode_dropdown.clone();
        let softness_row = softness_row.clone();
        let softness_editor = softness_editor.clone();
        let hardness_row = hardness_row.clone();
        let airbrush_row = airbrush_row.clone();
        let airbrush_rate_scale = airbrush_rate_scale.clone();
        let opacity_dyn = opacity_dyn.clone();
        let delete_btn = delete_btn.clone();
        let choose_btn = choose_btn.clone();
        let set_preview = set_preview.clone();
        let loading = loading.clone();
        let dirty = dirty.clone();
        let baseline = baseline.clone();
        let save_bar = save_bar.clone();
        let size_dyn = size_dyn.clone();
        let flow_dyn = flow_dyn.clone();
        let rotation_dyn = rotation_dyn.clone();
        let scatter_dyn = scatter_dyn.clone();
        let spacing_dyn = spacing_dyn.clone();
        let smudge_rate_dyn = smudge_rate_dyn.clone();
        let color_rate_dyn = color_rate_dyn.clone();
        Rc::new(move |maybe: Option<&BrushPreset>| {
            // Suppress widget callbacks while we populate.
            loading.set(true);
            // Loading a brush is the clean baseline for future edits:
            // snapshot it and hide the (now irrelevant) save bar.
            *baseline.borrow_mut() = maybe.cloned();
            dirty.set(false);
            save_bar.set_reveal_child(false);
            let enabled = maybe.is_some();
            delete_btn.set_sensitive(enabled);
            choose_btn.set_sensitive(enabled);
            pattern_btn.set_sensitive(enabled);
            tip_btn.set_sensitive(enabled);
            texture_mode_dropdown.set_sensitive(enabled);
            texture_brightness_scale.set_sensitive(enabled);
            texture_contrast_scale.set_sensitive(enabled);
            texture_invert_switch.set_sensitive(enabled);
            smear_alpha_switch.set_sensitive(enabled);
            name_entry.set_sensitive(enabled);
            family_dropdown.set_sensitive(enabled);
            size_scale.set_sensitive(enabled);
            opacity_scale.set_sensitive(enabled);
            spacing_scale.set_sensitive(enabled);
            stabilizer_scale.set_sensitive(enabled);
            speed_smoothing_scale.set_sensitive(enabled);
            hardness_scale.set_sensitive(enabled);
            pattern_size_scale.set_sensitive(enabled);
            pattern_strength_scale.set_sensitive(enabled);
            paint_mode_dropdown.set_sensitive(enabled);
            softness_row.set_sensitive(enabled);
            airbrush_row.set_sensitive(enabled);
            size_dyn.row.set_sensitive(enabled);
            flow_dyn.row.set_sensitive(enabled);
            opacity_dyn.row.set_sensitive(enabled);
            rotation_dyn.row.set_sensitive(enabled);
            scatter_dyn.row.set_sensitive(enabled);
            spacing_dyn.row.set_sensitive(enabled);
            smudge_rate_dyn.row.set_sensitive(enabled);
            color_rate_dyn.row.set_sensitive(enabled);

            if let Some(p) = maybe {
                name_entry.set_text(&p.name);
                picker::apply_icon_to_image(&icon_image, p, FALLBACK_ICON);
                family_dropdown.set_selected(family_to_dropdown_index(&p.family));
                refresh_family_rows(&p.family);
                texture_mode_dropdown.set_selected(p.texturing_mode.to_index());
                texture_brightness_scale.set_value(f64::from(p.texture_adjust.brightness));
                texture_contrast_scale.set_value(f64::from(p.texture_adjust.contrast));
                texture_invert_switch.set_active(p.texture_adjust.invert);
                smear_alpha_switch.set_active(matches!(
                    p.family,
                    BrushFamily::Smudge { smear_alpha: true, .. }
                ));
                size_scale.set_value(f64::from(p.default_size));
                opacity_scale.set_value(f64::from(p.default_opacity));
                spacing_scale.set_value(f64::from(p.spacing_ratio));
                stabilizer_scale.set_value(f64::from(p.stabilizer));
                speed_smoothing_scale.set_value(f64::from(p.speed_smoothing));
                hardness_scale.set_value(f64::from(p.hardness));
                pattern_size_scale.set_value(f64::from(p.texture_scale.max(16.0)));
                pattern_strength_scale.set_value(f64::from(p.texture_strength));
                paint_mode_dropdown.set_selected(p.paint_mode.to_index());
                softness_row.set_enable_expansion(p.softness_curve.is_some());
                hardness_row.set_sensitive(p.softness_curve.is_none());
                if let Some(c) = &p.softness_curve {
                    softness_editor.set_curve(to_editor_curve(c));
                }
                airbrush_row.set_enable_expansion(p.airbrush);
                airbrush_rate_scale.set_value(f64::from(p.airbrush_rate));
                size_dyn.apply(p.dynamics.size.as_ref());
                flow_dyn.apply(p.dynamics.flow.as_ref());
                opacity_dyn.apply(p.dynamics.opacity.as_ref());
                rotation_dyn.apply(p.dynamics.rotation.as_ref());
                scatter_dyn.apply(p.dynamics.scatter.as_ref());
                spacing_dyn.apply(p.dynamics.spacing.as_ref());
                smudge_rate_dyn.apply(p.dynamics.smudge_rate.as_ref());
                color_rate_dyn.apply(p.dynamics.color_rate.as_ref());
                set_preview(Some(p));
            } else {
                name_entry.set_text("");
                icon_image.set_icon_name(Some(FALLBACK_ICON));
                family_dropdown.set_selected(0);
                refresh_family_rows(&BrushFamily::SoftRound);
                pattern_thumb.set_icon_name(Some("image-x-generic-symbolic"));
                texture_mode_dropdown.set_selected(0);
                texture_brightness_scale.set_value(0.0);
                texture_contrast_scale.set_value(1.0);
                texture_invert_switch.set_active(false);
                smear_alpha_switch.set_active(false);
                size_scale.set_value(0.0);
                opacity_scale.set_value(0.0);
                spacing_scale.set_value(0.0);
                stabilizer_scale.set_value(0.0);
                speed_smoothing_scale.set_value(0.0);
                hardness_scale.set_value(1.0);
                pattern_size_scale.set_value(200.0);
                pattern_strength_scale.set_value(0.0);
                paint_mode_dropdown.set_selected(0);
                softness_row.set_enable_expansion(false);
                hardness_row.set_sensitive(true);
                airbrush_row.set_enable_expansion(false);
                airbrush_rate_scale.set_value(f64::from(DEFAULT_AIRBRUSH_RATE));
                size_dyn.apply(None);
                flow_dyn.apply(None);
                opacity_dyn.apply(None);
                rotation_dyn.apply(None);
                scatter_dyn.apply(None);
                spacing_dyn.apply(None);
                smudge_rate_dyn.apply(None);
                color_rate_dyn.apply(None);
                set_preview(None);
            }
            loading.set(false);
        })
    };

    // -- Focus the name entry - used after creating a new brush ----------
    let focus_name: Rc<dyn Fn()> = {
        let name_entry = name_entry.clone();
        Rc::new(move || {
            name_entry.grab_focus();
        })
    };

    // -- Delete + Choose Icon --------------------------------------------
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let parent = parent.clone();
        delete_btn.connect_clicked(move |_| {
            let Some(id) = selected_id.get() else { return };
            super::confirm_and_delete(&parent, &brush_engine, id);
        });
    }
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let parent = parent.clone();
        choose_btn.connect_clicked(move |_| {
            let Some(id) = selected_id.get() else { return };
            super::choose_icon(&parent, &brush_engine, id);
        });
    }

    // Hook live preview into every editable widget via the
    // value-changed channel they already fire on.
    install_live_preview(
        &[
            &size_scale,
            &opacity_scale,
            &spacing_scale,
            &stabilizer_scale,
            &speed_smoothing_scale,
            &hardness_scale,
            &pattern_size_scale,
            &pattern_strength_scale,
            &texture_brightness_scale,
            &texture_contrast_scale,
            &airbrush_rate_scale,
        ],
        &[
            &size_dyn,
            &flow_dyn,
            &opacity_dyn,
            &rotation_dyn,
            &scatter_dyn,
            &spacing_dyn,
            &smudge_rate_dyn,
            &color_rate_dyn,
        ],
        &loading,
        &live_preview,
    );
    {
        let live = live_preview.clone();
        let loading = loading.clone();
        let refresh = Rc::new(move || {
            if !loading.get() {
                live();
            }
        });
        let r = refresh.clone();
        paint_mode_dropdown.connect_selected_notify(move |_| r());
        let r = refresh.clone();
        texture_mode_dropdown.connect_selected_notify(move |_| r());
        let r = refresh.clone();
        texture_invert_switch.connect_state_set(move |_, _| {
            r();
            glib::Propagation::Proceed
        });
        let r = refresh.clone();
        airbrush_row.connect_enable_expansion_notify(move |_| r());
        let r = refresh.clone();
        softness_row.connect_enable_expansion_notify(move |_| r());
        softness_editor.connect_changed(move |_| refresh());
    }

    // -- Save: persist in-memory edits to disk ---------------------------
    let do_save: Rc<dyn Fn()> = {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let parent = parent.clone();
        let dirty = dirty.clone();
        let baseline = baseline.clone();
        let save_bar = save_bar.clone();
        Rc::new(move || {
            let Some(id) = selected_id.get() else { return };
            match save_brush_to_disk(&brush_engine, id) {
                Ok(()) => {
                    // Re-snapshot the just-saved state (save may have
                    // rewritten source_path / preview) as the new baseline.
                    if let Some(p) = brush_engine.brushes.borrow().iter().find(|p| p.id == id) {
                        *baseline.borrow_mut() = Some(p.clone());
                    }
                    dirty.set(false);
                    save_bar.set_reveal_child(false);
                }
                Err(e) => {
                    tracing::warn!(%e, "failed to save brush");
                    super::show_simple_error(&parent, "Couldn't save brush", &e.to_string());
                }
            }
        })
    };

    // -- Discard: revert in-memory brush to the saved baseline -----------
    let do_discard: Rc<dyn Fn()> = {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let baseline = baseline.clone();
        let setter = setter.clone();
        Rc::new(move || {
            let Some(id) = selected_id.get() else { return };
            let base = baseline.borrow().clone();
            let Some(base) = base else { return };
            if let Some(b) = brush_engine.brushes.borrow_mut().iter_mut().find(|p| p.id == id) {
                *b = base.clone();
            }
            // Repopulate the widgets from the restored brush; the setter
            // re-snapshots the baseline and clears the dirty flag.
            setter(Some(&base));
        })
    };

    // Wire the action-bar buttons now that save/discard exist.
    {
        let do_save = do_save.clone();
        save_btn.connect_clicked(move |_| do_save());
    }
    {
        let do_discard = do_discard.clone();
        discard_btn.connect_clicked(move |_| do_discard());
    }

    (outer.upcast(), EditorHandles {
        set_brush: setter,
        focus_name,
        is_dirty: dirty,
        save: do_save,
        discard: do_discard,
    })
}

/// Build the Save / Discard action bar. It lives inside a `gtk::Revealer`
/// so showing/hiding it animates the row height (slide) rather than
/// snapping, which reads as much smoother.
fn build_save_bar() -> (gtk::Revealer, gtk::Button, gtk::Button) {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .margin_start(24)
        .margin_end(24)
        .margin_top(0)
        .margin_bottom(12)
        .build();

    // Small (non-pill) buttons matching the export dialog: Discard (red
    // text, light-red fill) on the left, Save suggested on the right,
    // pushed apart by a spacer.
    ensure_discard_css();
    let discard_btn = gtk::Button::with_label("Discard");
    discard_btn.add_css_class("brush-discard-btn");
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let save_btn = gtk::Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    bar.append(&discard_btn);
    bar.append(&spacer);
    bar.append(&save_btn);

    let revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(180)
        .reveal_child(false)
        .child(&bar)
        .build();
    (revealer, save_btn, discard_btn)
}

/// Red-on-light-red styling for the Discard button, installed once per
/// process (GTK de-dupes the provider by priority + display).
fn ensure_discard_css() {
    use std::sync::OnceLock;
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".brush-discard-btn {
                color: #c01c28;
                background-color: alpha(#c01c28, 0.15);
            }
            .brush-discard-btn:hover {
                background-color: alpha(#c01c28, 0.25);
            }
            .brush-discard-btn:active {
                background-color: alpha(#c01c28, 0.32);
            }",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

/// Seed for a freshly enabled softness curve: full coverage at the centre
/// fading straight to nothing at the edge.
fn default_softness_curve() -> Curve {
    Curve::smooth_from_points(vec![(0.0, 1.0), (1.0, 0.0)]).unwrap_or_default()
}

/// The curve widget works on 8-bit points; brush curves keep exact values
/// until the user drags a point.
fn to_editor_curve(curve: &Curve) -> curves::Curve {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let points: Vec<_> = curve
        .points()
        .iter()
        .map(|&(x, y)| curves::CurvePoint::new(to_u8(x), to_u8(y)))
        .collect();
    curves::Curve::from_points(&points)
}

fn from_editor_curve(curve: &curves::Curve) -> Option<Curve> {
    let points = curve
        .points()
        .iter()
        .map(|p| (f32::from(p.x) / 255.0, f32::from(p.y) / 255.0))
        .collect();
    Curve::smooth_from_points(points)
}

fn family_to_dropdown_index(family: &BrushFamily) -> u32 {
    match family {
        BrushFamily::SoftRound => 0,
        BrushFamily::Pixel => 1,
        // Image-tip brushes (built-in chalk) share the "Textured" slot in the
        // editor dropdown; the tip itself isn't user-editable yet.
        BrushFamily::Textured(_) | BrushFamily::ImageTip { .. } => 2,
        BrushFamily::Smudge { .. } => 3,
    }
}

// ---------------------------------------------------------------------------
// Pattern picker
// ---------------------------------------------------------------------------

fn apply_pattern_visibility(
    rows: &[gtk::Widget],
    thumb: &gtk::Image,
    clear_btn: &gtk::Button,
    family: &BrushFamily,
) {
    let textured = matches!(
        family,
        BrushFamily::Textured(_) | BrushFamily::ImageTip { .. } | BrushFamily::Smudge { .. }
    );
    for row in rows {
        row.set_visible(textured);
    }
    clear_btn.set_visible(family.is_smudge() && family.grain().is_some());
    match family {
        BrushFamily::Textured(rc) => apply_pattern_thumb(thumb, rc),
        // Show the grain texture thumb; fall back to the tip if there's no
        // grain so the row isn't blank.
        BrushFamily::ImageTip { tip, grain } => {
            apply_pattern_thumb(thumb, grain.as_ref().unwrap_or(tip));
        }
        BrushFamily::Smudge { grain, .. } => apply_optional_thumb(thumb, grain.as_deref()),
        _ => {}
    }
}

fn apply_tip_visibility(
    row: &adw::ActionRow,
    thumb: &gtk::Image,
    clear_btn: &gtk::Button,
    family: &BrushFamily,
) {
    let tip = match family {
        BrushFamily::Smudge { tip, .. } => tip.as_deref(),
        _ => None,
    };
    row.set_visible(family.is_smudge());
    clear_btn.set_visible(tip.is_some());
    apply_optional_thumb(thumb, tip);
}

fn apply_optional_thumb(thumb: &gtk::Image, data: Option<&PatternData>) {
    match data {
        Some(d) => apply_pattern_thumb(thumb, d),
        None => thumb.set_icon_name(Some("image-x-generic-symbolic")),
    }
}

fn clear_button(tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name("edit-clear-symbolic");
    button.add_css_class("flat");
    button.set_valign(gtk::Align::Center);
    button.set_tooltip_text(Some(tooltip));
    button.set_visible(false);
    button
}

fn apply_pattern_thumb(thumb: &gtk::Image, data: &PatternData) {
    // PatternData carries premultiplied RGBA; gdk::MemoryFormat lets
    // us upload that straight into a texture.
    let bytes = relm4::gtk::glib::Bytes::from(&data.rgba);
    let texture = gtk::gdk::MemoryTexture::new(
        data.width as i32,
        data.height as i32,
        gtk::gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &bytes,
        (data.width * 4) as usize,
    );
    thumb.set_paintable(Some(&texture));
}

/// Open a `gtk::FileDialog` filtered to PNG and decode the pick with `decode`.
/// The callback fires with:
/// - `Ok(PatternData)` on success,
/// - `Err(Some(msg))` on read / decode failure,
/// - `Err(None)` when the user cancels.
fn choose_png<F>(
    parent: &gtk::Window,
    title: &str,
    decode: fn(&[u8]) -> Result<PatternData, String>,
    callback: F,
) where
    F: FnOnce(Result<PatternData, Option<String>>) + 'static,
{
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("PNG image"));
    filter.add_mime_type("image/png");
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title(title)
        .modal(true)
        .filters(&filters)
        .build();
    dialog.open(
        Some(parent),
        gtk::gio::Cancellable::NONE,
        move |result| {
            let Ok(file) = result else {
                callback(Err(None));
                return;
            };
            let Some(path) = file.path() else {
                callback(Err(Some("file has no local path".into())));
                return;
            };
            let decoded = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| decode(&bytes));
            callback(decoded.map_err(Some));
        },
    );
}

// ---------------------------------------------------------------------------
// Scale row factory
// ---------------------------------------------------------------------------

fn build_scale_row(title: &str, lo: f64, hi: f64, step: f64) -> (adw::ActionRow, gtk::Scale) {
    let row = adw::ActionRow::builder().title(title).build();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, lo, hi, step);
    scale.set_size_request(180, -1);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_digits(2);
    scale.set_hexpand(false);
    row.add_suffix(&scale);
    (row, scale)
}

fn wire_scale_to_field(
    scale: &gtk::Scale,
    brush_engine: &BrushEngine,
    selected_id: &Rc<Cell<Option<BrushPresetId>>>,
    loading: &Rc<Cell<bool>>,
    mark_dirty: &Rc<dyn Fn()>,
    apply: impl Fn(&mut BrushPreset, f64) + 'static,
) {
    let brush_engine = brush_engine.clone();
    let selected_id = selected_id.clone();
    let loading = loading.clone();
    let mark_dirty = mark_dirty.clone();
    scale.connect_value_changed(move |s| {
        if loading.get() {
            return;
        }
        let Some(id) = selected_id.get() else { return };
        let v = s.value();
        let mut brushes = brush_engine.brushes.borrow_mut();
        if let Some(brush) = brushes.iter_mut().find(|p| p.id == id) {
            apply(brush, v);
        }
        drop(brushes);
        mark_dirty();
    });
}

// ---------------------------------------------------------------------------
// Dynamics expander rows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum DynamicsField {
    Size,
    Flow,
    Opacity,
    Rotation,
    Scatter,
    Spacing,
    SmudgeRate,
    ColorRate,
}

impl DynamicsField {
    fn get_mut(self, dynamics: &mut Dynamics) -> &mut Option<Mapping> {
        match self {
            Self::Size => &mut dynamics.size,
            Self::Flow => &mut dynamics.flow,
            Self::Opacity => &mut dynamics.opacity,
            Self::Rotation => &mut dynamics.rotation,
            Self::Scatter => &mut dynamics.scatter,
            Self::Spacing => &mut dynamics.spacing,
            Self::SmudgeRate => &mut dynamics.smudge_rate,
            Self::ColorRate => &mut dynamics.color_rate,
        }
    }

    /// Sensible "create when toggled on" default for each field.
    fn default_mapping(self, brush: &BrushPreset) -> Mapping {
        match self {
            Self::Size | Self::Opacity => Mapping::pressure_linear(),
            Self::Flow => Mapping {
                source: DynSource::Pressure,
                curve: Curve::linear(),
                range: (0.0, 1.0),
                invert: false,
            },
            Self::Rotation => Mapping {
                source: DynSource::Random,
                curve: Curve::linear(),
                range: (0.0, TAU_F32),
                invert: false,
            },
            Self::Scatter => Mapping {
                source: DynSource::Random,
                curve: Curve::linear(),
                range: (0.0, brush.default_size.max(1.0)),
                invert: false,
            },
            // Seed the spacing mapping with the brush's current static
            // spacing as both bounds so toggling the dynamic on doesn't
            // visibly change the stroke until the user widens the range.
            Self::Spacing => Mapping {
                source: DynSource::Pressure,
                curve: Curve::linear(),
                range: (brush.spacing_ratio, brush.spacing_ratio),
                invert: false,
            },
            Self::SmudgeRate => Mapping {
                source: DynSource::Pressure,
                curve: Curve::linear(),
                range: (0.5, 1.0),
                invert: false,
            },
            Self::ColorRate => Mapping {
                source: DynSource::Pressure,
                curve: Curve::linear(),
                range: (0.0, 0.1),
                invert: false,
            },
        }
    }

    /// Whether the `range` values should be shown as degrees (rotation
    /// is stored in radians internally).
    fn range_in_degrees(self) -> bool {
        matches!(self, Self::Rotation)
    }
}

#[derive(Clone)]
struct DynamicsRowHandles {
    row: adw::ExpanderRow,
    field: DynamicsField,
    source_dropdown: gtk::DropDown,
    min_spin: gtk::SpinButton,
    max_spin: gtk::SpinButton,
    invert_switch: gtk::Switch,
    curve_editor: CurveEditor,
    loading: Rc<Cell<bool>>,
}

impl DynamicsRowHandles {
    fn apply(&self, mapping: Option<&Mapping>) {
        self.loading.set(true);
        let enabled = mapping.is_some();
        self.row.set_enable_expansion(enabled);
        if let Some(m) = mapping {
            let src_idx = DYN_SOURCES
                .iter()
                .position(|(s, _)| *s == m.source)
                .unwrap_or(0);
            self.source_dropdown.set_selected(src_idx as u32);
            let (lo, hi) = m.range;
            let (lo_d, hi_d) = if self.field.range_in_degrees() {
                (f64::from(lo.to_degrees()), f64::from(hi.to_degrees()))
            } else {
                (f64::from(lo), f64::from(hi))
            };
            self.min_spin.set_value(lo_d);
            self.max_spin.set_value(hi_d);
            self.invert_switch.set_active(m.invert);
            self.curve_editor.set_curve(to_editor_curve(&m.curve));
        }
        self.loading.set(false);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_dynamics_row(
    title: &str,
    subtitle: &str,
    field: DynamicsField,
    brush_engine: &BrushEngine,
    selected_id: &Rc<Cell<Option<BrushPresetId>>>,
    loading: &Rc<Cell<bool>>,
    mark_dirty: &Rc<dyn Fn()>,
) -> DynamicsRowHandles {
    let row = adw::ExpanderRow::builder()
        .title(title)
        .subtitle(subtitle)
        .show_enable_switch(true)
        .enable_expansion(false)
        .build();

    // Source dropdown.
    let source_row = adw::ActionRow::builder().title("Source").build();
    let source_dropdown = gtk::DropDown::from_strings(
        &DYN_SOURCES.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
    );
    source_dropdown.set_valign(gtk::Align::Center);
    source_row.add_suffix(&source_dropdown);
    row.add_row(&source_row);

    // Range min/max spins.
    let (min_spin, max_spin) = match field {
        DynamicsField::Rotation => (
            gtk::SpinButton::with_range(-360.0, 360.0, 1.0),
            gtk::SpinButton::with_range(-360.0, 360.0, 1.0),
        ),
        DynamicsField::Scatter => (
            gtk::SpinButton::with_range(0.0, 1000.0, 0.5),
            gtk::SpinButton::with_range(0.0, 1000.0, 0.5),
        ),
        DynamicsField::Spacing => (
            gtk::SpinButton::with_range(0.01, 1.0, 0.01),
            gtk::SpinButton::with_range(0.01, 1.0, 0.01),
        ),
        DynamicsField::SmudgeRate | DynamicsField::ColorRate => (
            gtk::SpinButton::with_range(0.0, 1.0, 0.005),
            gtk::SpinButton::with_range(0.0, 1.0, 0.005),
        ),
        _ => (
            gtk::SpinButton::with_range(0.0, 4.0, 0.05),
            gtk::SpinButton::with_range(0.0, 4.0, 0.05),
        ),
    };
    // Rates sit near 0.01, so they need a third digit.
    let digits = if matches!(field, DynamicsField::SmudgeRate | DynamicsField::ColorRate) { 3 } else { 2 };
    min_spin.set_digits(digits);
    max_spin.set_digits(digits);
    let min_row = adw::ActionRow::builder().title("Range min").build();
    min_row.add_suffix(&min_spin);
    row.add_row(&min_row);
    let max_row = adw::ActionRow::builder().title("Range max").build();
    max_row.add_suffix(&max_spin);
    row.add_row(&max_row);

    // Invert switch.
    let invert_row = adw::ActionRow::builder().title("Invert source").build();
    let invert_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    invert_row.add_suffix(&invert_switch);
    row.add_row(&invert_row);

    // Response curve: source (left to right) to the min..max range (bottom to top).
    let curve_editor = CurveEditor::new();
    curve_editor.widget.set_margin_top(8);
    curve_editor.widget.set_margin_bottom(8);
    curve_editor.widget.set_margin_start(12);
    curve_editor.widget.set_margin_end(12);
    row.add_row(&curve_editor.widget);

    let handles = DynamicsRowHandles {
        row,
        field,
        source_dropdown,
        min_spin,
        max_spin,
        invert_switch,
        curve_editor,
        loading: loading.clone(),
    };

    // -- Toggle: enable / disable ----------------------------------------
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let loading = loading.clone();
        let mark_dirty = mark_dirty.clone();
        let handles_for_apply = handles.clone();
        handles.row.connect_enable_expansion_notify(move |row| {
            if loading.get() {
                return;
            }
            let Some(id) = selected_id.get() else { return };
            let now_enabled = row.enables_expansion();
            let mut brushes = brush_engine.brushes.borrow_mut();
            let Some(brush) = brushes.iter_mut().find(|p| p.id == id) else { return };
            if now_enabled {
                let default_mapping = field.default_mapping(brush);
                let slot = field.get_mut(&mut brush.dynamics);
                if slot.is_none() {
                    *slot = Some(default_mapping);
                }
                let snapshot = slot.clone();
                drop(brushes);
                handles_for_apply.apply(snapshot.as_ref());
            } else {
                *field.get_mut(&mut brush.dynamics) = None;
                drop(brushes);
            }
            mark_dirty();
        });
    }

    // -- Source dropdown -------------------------------------------------
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let loading = loading.clone();
        let mark_dirty = mark_dirty.clone();
        handles
            .source_dropdown
            .connect_selected_notify(move |d| {
                if loading.get() {
                    return;
                }
                let Some(id) = selected_id.get() else { return };
                let idx = d.selected() as usize;
                let Some((src, _)) = DYN_SOURCES.get(idx) else { return };
                let mut brushes = brush_engine.brushes.borrow_mut();
                if let Some(brush) = brushes.iter_mut().find(|p| p.id == id)
                    && let Some(m) = field.get_mut(&mut brush.dynamics) {
                        m.source = *src;
                    }
                drop(brushes);
                mark_dirty();
            });
    }

    // -- Min / Max spins -------------------------------------------------
    wire_range_spin(
        &handles.min_spin,
        true,
        field,
        brush_engine,
        selected_id,
        loading,
        mark_dirty,
    );
    wire_range_spin(
        &handles.max_spin,
        false,
        field,
        brush_engine,
        selected_id,
        loading,
        mark_dirty,
    );

    // -- Invert switch ---------------------------------------------------
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let loading = loading.clone();
        let mark_dirty = mark_dirty.clone();
        handles.invert_switch.connect_state_set(move |_, state| {
            if loading.get() {
                return glib::Propagation::Proceed;
            }
            if let Some(id) = selected_id.get() {
                let mut brushes = brush_engine.brushes.borrow_mut();
                if let Some(brush) = brushes.iter_mut().find(|p| p.id == id)
                    && let Some(m) = field.get_mut(&mut brush.dynamics) {
                        m.invert = state;
                    }
                drop(brushes);
                mark_dirty();
            }
            glib::Propagation::Proceed
        });
    }

    // -- Response curve --------------------------------------------------
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let loading = loading.clone();
        let mark_dirty = mark_dirty.clone();
        handles.curve_editor.connect_changed(move |curve| {
            if loading.get() {
                return;
            }
            let Some(id) = selected_id.get() else { return };
            let curve = from_editor_curve(&curve).unwrap_or_default();
            let mut brushes = brush_engine.brushes.borrow_mut();
            if let Some(brush) = brushes.iter_mut().find(|p| p.id == id)
                && let Some(m) = field.get_mut(&mut brush.dynamics) {
                    m.curve = curve;
                }
            drop(brushes);
            mark_dirty();
        });
    }

    handles
}

fn wire_range_spin(
    spin: &gtk::SpinButton,
    is_min: bool,
    field: DynamicsField,
    brush_engine: &BrushEngine,
    selected_id: &Rc<Cell<Option<BrushPresetId>>>,
    loading: &Rc<Cell<bool>>,
    mark_dirty: &Rc<dyn Fn()>,
) {
    let brush_engine = brush_engine.clone();
    let selected_id = selected_id.clone();
    let loading = loading.clone();
    let mark_dirty = mark_dirty.clone();
    spin.connect_value_changed(move |s| {
        if loading.get() {
            return;
        }
        let Some(id) = selected_id.get() else { return };
        let mut display_value = s.value();
        if field.range_in_degrees() {
            display_value = display_value.to_radians();
        }
        let mut brushes = brush_engine.brushes.borrow_mut();
        if let Some(brush) = brushes.iter_mut().find(|p| p.id == id)
            && let Some(m) = field.get_mut(&mut brush.dynamics) {
                if is_min {
                    m.range.0 = display_value as f32;
                } else {
                    m.range.1 = display_value as f32;
                }
            }
        drop(brushes);
        mark_dirty();
    });
}

// ---------------------------------------------------------------------------
// Live preview wiring
// ---------------------------------------------------------------------------

fn install_live_preview(
    scales: &[&gtk::Scale],
    dynamics_rows: &[&DynamicsRowHandles],
    loading: &Rc<Cell<bool>>,
    live_preview: &Rc<dyn Fn()>,
) {
    for scale in scales {
        let loading = loading.clone();
        let live = live_preview.clone();
        scale.connect_value_changed(move |_| {
            if !loading.get() {
                live();
            }
        });
    }

    for handles in dynamics_rows {
        let l = loading.clone();
        let live = live_preview.clone();
        handles.curve_editor.connect_changed(move |_| {
            if !l.get() {
                live();
            }
        });
        let l = loading.clone();
        let live = live_preview.clone();
        handles.row.connect_enable_expansion_notify(move |_| {
            if !l.get() {
                live();
            }
        });
        let l = loading.clone();
        let live = live_preview.clone();
        handles
            .source_dropdown
            .connect_selected_notify(move |_| {
                if !l.get() {
                    live();
                }
            });
        let l = loading.clone();
        let live = live_preview.clone();
        handles.min_spin.connect_value_changed(move |_| {
            if !l.get() {
                live();
            }
        });
        let l = loading.clone();
        let live = live_preview.clone();
        handles.max_spin.connect_value_changed(move |_| {
            if !l.get() {
                live();
            }
        });
        let l = loading.clone();
        let live = live_preview.clone();
        handles.invert_switch.connect_state_set(move |_, _| {
            if !l.get() {
                live();
            }
            glib::Propagation::Proceed
        });
    }
}

// ---------------------------------------------------------------------------
// Save to disk
// ---------------------------------------------------------------------------

fn save_brush_to_disk(
    brush_engine: &BrushEngine,
    id: BrushPresetId,
) -> Result<(), format::BrushError> {
    // Read current state.
    let mut preset = brush_engine
        .brushes
        .borrow()
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .ok_or_else(|| {
            format::BrushError::MissingEntry("brush id not found in engine".to_string())
        })?;
    // Re-render preview from the latest in-memory state so the cache
    // tracks user edits. Failure is non-fatal - fall back to the
    // previous cached preview (or none) and keep going with the save.
    match preview_renderer::render_preview_png(&preset) {
        Ok(png) => {
            preset.preview = Some(png.clone());
            if let Some(b) = brush_engine
                .brushes
                .borrow_mut()
                .iter_mut()
                .find(|p| p.id == id)
            {
                b.preview = Some(png);
            }
        }
        Err(e) => {
            tracing::warn!(brush = %preset.name, %e, "preview re-render failed; saving without refresh");
        }
    }
    let dir = if let Some(d) = preset.source_path.as_ref().and_then(|p| p.parent()) { d.to_path_buf() } else {
        let d = BrushRegistry::config_dir().ok_or_else(|| {
            format::BrushError::MissingEntry(
                "XDG config dir not resolvable".to_string(),
            )
        })?;
        std::fs::create_dir_all(&d)?;
        d
    };
    let desired_path = dir.join(format!("{}.oxiebrush", sanitize_filename(&preset.name)));

    // If we'd be renaming and the target is occupied by a *different*
    // brush, fall back to the original path so we don't clobber it.
    // (Same brush's path = identity; bail out of the rename branch.)
    let final_path = match preset.source_path.as_ref() {
        Some(old_path) if old_path != &desired_path => {
            if desired_path.exists() {
                tracing::warn!(
                    old = ?old_path,
                    new = ?desired_path,
                    "brush rename target exists; keeping old filename"
                );
                old_path.clone()
            } else {
                std::fs::rename(old_path, &desired_path)?;
                // Propagate the new path back into the engine so
                // subsequent saves/deletes operate on the right file.
                if let Some(b) = brush_engine
                    .brushes
                    .borrow_mut()
                    .iter_mut()
                    .find(|p| p.id == id)
                {
                    b.source_path = Some(desired_path.clone());
                }
                desired_path
            }
        }
        // New brush or already at the right filename.
        Some(p) => p.clone(),
        None => {
            // No prior path - write to derived path and record it.
            if let Some(b) = brush_engine
                .brushes
                .borrow_mut()
                .iter_mut()
                .find(|p| p.id == id)
            {
                b.source_path = Some(desired_path.clone());
            }
            desired_path
        }
    };

    format::save(&preset, &final_path)
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

