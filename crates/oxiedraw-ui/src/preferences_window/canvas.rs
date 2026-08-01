//! Canvas defaults page (shape correction toggles).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use relm4::gtk;

use crate::settings::{AppSettings, PixelViewSettings};

/// Selectable rotation snap steps (degrees).
const SNAP_STEPS: [f32; 7] = [5.0, 10.0, 15.0, 22.5, 30.0, 45.0, 90.0];

pub(super) fn build_canvas_page(
    settings: Rc<RefCell<AppSettings>>,
    apply_pixel_view: Rc<dyn Fn(&PixelViewSettings)>,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("Canvas");
    page.set_icon_name(Some("applications-graphics-symbolic"));

    let drawing_group = adw::PreferencesGroup::new();
    drawing_group.set_title("Drawing");

    // -- Shape correction ExpanderRow ------------------------------------------
    let sc = settings.borrow().shape_correction.clone();

    let expander = adw::ExpanderRow::new();
    expander.set_title("Shape correction");
    expander.set_subtitle(
        "Hold still after drawing to snap lines, circles, and rectangles to perfect geometry",
    );
    expander.set_show_enable_switch(true);
    expander.set_enable_expansion(sc.enabled);

    // Sub-row: trigger delay
    let delay_row = adw::SpinRow::with_range(0.0, 10_000.0, 100.0);
    delay_row.set_title("Trigger delay");
    delay_row.set_subtitle("How long to hold still before correction fires (ms)");
    delay_row.set_value(f64::from(sc.trigger_delay_ms));

    // Sub-row: animation speed
    let anim_row = adw::SpinRow::with_range(0.0, 10_000.0, 10.0);
    anim_row.set_title("Animation speed");
    anim_row.set_subtitle("Total snap animation duration (ms, 0 = instant)");
    anim_row.set_value(f64::from(sc.animation_speed_ms));

    // Sub-rows: per-shape toggles
    let line_row = adw::SwitchRow::new();
    line_row.set_title("Correct lines");
    line_row.set_subtitle("Straighten near-straight strokes, smooth curved ones");
    line_row.set_active(sc.correct_line);

    let circle_row = adw::SwitchRow::new();
    circle_row.set_title("Correct circles and ovals");
    circle_row.set_subtitle("Snap round strokes; smooth intentional distortions");
    circle_row.set_active(sc.correct_circle);

    let rect_row = adw::SwitchRow::new();
    rect_row.set_title("Correct rectangles");
    rect_row.set_active(sc.correct_rectangle);

    expander.add_row(&delay_row);
    expander.add_row(&anim_row);
    expander.add_row(&line_row);
    expander.add_row(&circle_row);
    expander.add_row(&rect_row);

    // -- Wire callbacks --------------------------------------------------------

    // Main toggle: if turning on with all shapes off, enable all shapes.
    {
        let settings = Rc::clone(&settings);
        let line_row = line_row.clone();
        let circle_row = circle_row.clone();
        let rect_row = rect_row.clone();
        expander.connect_enable_expansion_notify(move |e| {
            let enabled = e.enables_expansion();
            settings.borrow_mut().shape_correction.enabled = enabled;
            if enabled {
                let none_active = {
                    let s = settings.borrow();
                    !s.shape_correction.correct_line
                        && !s.shape_correction.correct_circle
                        && !s.shape_correction.correct_rectangle
                };
                if none_active {
                    let mut s = settings.borrow_mut();
                    s.shape_correction.correct_line = true;
                    s.shape_correction.correct_circle = true;
                    s.shape_correction.correct_rectangle = true;
                    drop(s);
                    line_row.set_active(true);
                    circle_row.set_active(true);
                    rect_row.set_active(true);
                }
            }
            settings.borrow().save();
        });
    }

    // Trigger delay
    {
        let settings = Rc::clone(&settings);
        delay_row.connect_value_notify(move |r| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = r.value() as u32;
            settings.borrow_mut().shape_correction.trigger_delay_ms = v;
            settings.borrow().save();
        });
    }

    // Animation speed
    {
        let settings = Rc::clone(&settings);
        anim_row.connect_value_notify(move |r| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = r.value() as u32;
            settings.borrow_mut().shape_correction.animation_speed_ms = v;
            settings.borrow().save();
        });
    }

    // Per-shape toggle helper: if all shapes become disabled, turn off the main switch.
    let make_shape_cb =
        |settings: Rc<RefCell<AppSettings>>,
         expander: adw::ExpanderRow,
         field: fn(&mut crate::settings::ShapeCorrectionSettings, bool)| {
            move |r: &adw::SwitchRow| {
                field(&mut settings.borrow_mut().shape_correction, r.is_active());
                let all_off = {
                    let s = settings.borrow();
                    !s.shape_correction.correct_line
                        && !s.shape_correction.correct_circle
                        && !s.shape_correction.correct_rectangle
                };
                if all_off {
                    settings.borrow_mut().shape_correction.enabled = false;
                    expander.set_enable_expansion(false);
                }
                settings.borrow().save();
            }
        };

    line_row.connect_active_notify(make_shape_cb(
        Rc::clone(&settings),
        expander.clone(),
        |sc, v| sc.correct_line = v,
    ));
    circle_row.connect_active_notify(make_shape_cb(
        Rc::clone(&settings),
        expander.clone(),
        |sc, v| sc.correct_circle = v,
    ));
    rect_row.connect_active_notify(make_shape_cb(
        Rc::clone(&settings),
        expander.clone(),
        |sc, v| sc.correct_rectangle = v,
    ));

    drawing_group.add(&expander);
    page.add(&drawing_group);

    // -- Pixel view group -----------------------------------------------------
    let pv_group = adw::PreferencesGroup::new();
    pv_group.set_title("Pixel view");

    let pv = settings.borrow().pixel_view.clone();

    let pv_expander = adw::ExpanderRow::new();
    pv_expander.set_title("Pixel-perfect zoom");
    pv_expander.set_subtitle(
        "Switch to nearest-neighbour scaling and show a pixel grid when zoomed in",
    );
    pv_expander.set_show_enable_switch(true);
    pv_expander.set_enable_expansion(pv.enabled);

    let nn_row = adw::SpinRow::with_range(1.0, 64.0, 1.0);
    nn_row.set_title("Nearest-neighbour threshold");
    nn_row.set_subtitle("Switch to crisp pixel scaling at this zoom level or higher");
    nn_row.set_value(f64::from(pv.nearest_threshold));

    let grid_row = adw::SwitchRow::new();
    grid_row.set_title("Show pixel grid");
    grid_row.set_active(pv.grid_enabled);

    let grid_thr_row = adw::SpinRow::with_range(1.0, 128.0, 1.0);
    grid_thr_row.set_title("Pixel grid threshold");
    grid_thr_row.set_subtitle("Show the grid at this zoom level or higher");
    grid_thr_row.set_value(f64::from(pv.grid_threshold));

    pv_expander.add_row(&nn_row);
    pv_expander.add_row(&grid_row);
    pv_expander.add_row(&grid_thr_row);

    // Helper: persist + push current pixel_view to the live paintable.
    let push: Rc<dyn Fn()> = {
        let settings = Rc::clone(&settings);
        let apply = Rc::clone(&apply_pixel_view);
        Rc::new(move || {
            let s = settings.borrow();
            apply(&s.pixel_view);
            s.save();
        })
    };

    {
        let settings = Rc::clone(&settings);
        let push = Rc::clone(&push);
        pv_expander.connect_enable_expansion_notify(move |e| {
            settings.borrow_mut().pixel_view.enabled = e.enables_expansion();
            push();
        });
    }
    {
        let settings = Rc::clone(&settings);
        let push = Rc::clone(&push);
        nn_row.connect_value_notify(move |r| {
            #[allow(clippy::cast_possible_truncation)]
            let v = r.value() as f32;
            settings.borrow_mut().pixel_view.nearest_threshold = v;
            push();
        });
    }
    {
        let settings = Rc::clone(&settings);
        let push = Rc::clone(&push);
        grid_row.connect_active_notify(move |r| {
            settings.borrow_mut().pixel_view.grid_enabled = r.is_active();
            push();
        });
    }
    {
        let settings = Rc::clone(&settings);
        let push = Rc::clone(&push);
        grid_thr_row.connect_value_notify(move |r| {
            #[allow(clippy::cast_possible_truncation)]
            let v = r.value() as f32;
            settings.borrow_mut().pixel_view.grid_threshold = v;
            push();
        });
    }

    pv_group.add(&pv_expander);
    page.add(&pv_group);

    // -- Rotation group -------------------------------------------------------
    let rot_group = adw::PreferencesGroup::new();
    rot_group.set_title("Rotation");

    let labels: Vec<String> = SNAP_STEPS.iter().map(|d| format!("{d} deg")).collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();

    let snap_row = adw::ComboRow::new();
    snap_row.set_title("Snap step");
    snap_row.set_subtitle(
        "Angle increment for the rotator dial and snap-modifier rotation",
    );
    snap_row.set_model(Some(&gtk::StringList::new(&label_refs)));
    // Select the current value, falling back to 45 deg if it isn't a preset.
    let current = settings.borrow().rotation_snap_deg;
    let selected = SNAP_STEPS
        .iter()
        .position(|d| (d - current).abs() < 0.01)
        .or_else(|| SNAP_STEPS.iter().position(|d| (*d - 45.0).abs() < 0.01))
        .unwrap_or(0);
    #[allow(clippy::cast_possible_truncation)]
    snap_row.set_selected(selected as u32);
    {
        let settings = Rc::clone(&settings);
        snap_row.connect_selected_notify(move |r| {
            let idx = (r.selected() as usize).min(SNAP_STEPS.len() - 1);
            settings.borrow_mut().rotation_snap_deg = SNAP_STEPS[idx];
            settings.borrow().save();
        });
    }
    rot_group.add(&snap_row);
    page.add(&rot_group);

    page
}

// Appearance page
