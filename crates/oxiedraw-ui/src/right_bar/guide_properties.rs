//! Drawing Guide sidebar panel.
//!
//! Replaces the normal right panel while the Drawing Guide tool is active
//! (like the crop panel). Edits the per-document [`GuideState`]: guide type,
//! symmetry mode, mirror/rotational, assisted drawing, and appearance. The two
//! on-canvas nodes handle position and rotation; Cancel / Done live in the top
//! bar (see `top_bar`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::guides::{GuideConfig, GuideKind, GuideState, SymmetryMode};
use relm4::RelmWidgetExt;
use relm4::gtk;
use relm4::gtk::glib;

const PANEL_MARGIN: i32 = 12;

type Refreshers = Rc<RefCell<Vec<Box<dyn Fn(&GuideConfig)>>>>;

pub(crate) fn build(guide: &GuideState, canvas: &Rc<RefCell<Canvas>>) -> gtk::Box {
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    panel.add_css_class("sidebar");

    // Guard set while `refresh` writes widget values, so value-changed handlers
    // don't loop a notify back through the state.
    let syncing = Rc::new(Cell::new(false));
    let refreshers: Refreshers = Rc::new(RefCell::new(Vec::new()));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(PANEL_MARGIN)
        .margin_bottom(PANEL_MARGIN)
        .margin_start(PANEL_MARGIN)
        .margin_end(PANEL_MARGIN)
        .valign(gtk::Align::Start)
        .build();

    content.append(&build_header());

    // One unified card grid: the three symmetry variants plus grid / isometric
    // / perspective, each a selectable guide type (like the crop overlay cards).
    content.append(&section_label("Guide Type"));
    content.append(&build_guide_cards(guide, canvas, &syncing, &refreshers));

    // Behaviour switches (boxed list), only meaningful for symmetry.
    let behavior = build_behavior_list(guide, &syncing, &refreshers);
    content.append(&behavior);

    // Appearance: opacity + thickness (+ grid spacing for grid/iso), boxed list.
    content.append(&section_label("Appearance"));
    let (appearance, grid_row, rays_row) = build_appearance_list(guide, &syncing, &refreshers);
    content.append(&appearance);

    content.append(&build_position_section(guide));

    // Show/hide the kind-specific bits when the config changes.
    {
        let behavior = behavior.clone();
        let grid_row = grid_row.clone();
        let rays_row = rays_row.clone();
        refreshers.borrow_mut().push(Box::new(move |cfg: &GuideConfig| {
            behavior.set_visible(cfg.kind == GuideKind::Symmetry);
            grid_row.set_visible(matches!(cfg.kind, GuideKind::Grid2D | GuideKind::Isometric));
            rays_row.set_visible(cfg.kind == GuideKind::Perspective);
        }));
    }

    // Run all refreshers now and on any external change.
    {
        let guide_c = guide.clone();
        let syncing_c = Rc::clone(&syncing);
        let refreshers_c = Rc::clone(&refreshers);
        let run = move || {
            if let Some(cfg) = guide_c.config.borrow().as_ref() {
                syncing_c.set(true);
                for r in refreshers_c.borrow().iter() {
                    r(cfg);
                }
                syncing_c.set(false);
            }
        };
        run();
        guide.connect_changed(Box::new(run));
    }

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();
    scroll.set_child(Some(&content));
    panel.append(&scroll);
    panel
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn build_header() -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let icon = gtk::Image::from_icon_name("oxiedraw-guide-symbolic");
    icon.add_css_class("accent");
    icon.set_pixel_size(18);
    row.append(&icon);

    let title = gtk::Label::builder()
        .label("Drawing Guide")
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    title.inline_css("font-weight: 600;");
    row.append(&title);
    row
}

// ---------------------------------------------------------------------------
// Unified guide-type cards
// ---------------------------------------------------------------------------

/// A selectable guide type. The three symmetry variants and the grid / iso /
/// perspective kinds are all presented as one flat card grid.
#[derive(Clone, Copy)]
struct GuidePreset {
    kind: GuideKind,
    /// The symmetry mode, for `GuideKind::Symmetry` presets.
    symmetry: Option<SymmetryMode>,
    label: &'static str,
}

const PRESETS: [GuidePreset; 6] = [
    GuidePreset { kind: GuideKind::Symmetry, symmetry: Some(SymmetryMode::Axis), label: "Axis" },
    GuidePreset { kind: GuideKind::Symmetry, symmetry: Some(SymmetryMode::Quadrant), label: "Quadrant" },
    GuidePreset { kind: GuideKind::Symmetry, symmetry: Some(SymmetryMode::Radial), label: "Radial" },
    GuidePreset { kind: GuideKind::Grid2D, symmetry: None, label: "2D Grid" },
    GuidePreset { kind: GuideKind::Isometric, symmetry: None, label: "Isometric" },
    GuidePreset { kind: GuideKind::Perspective, symmetry: None, label: "Perspective" },
];

/// Index of the preset matching the current config (symmetry mode for the
/// Symmetry kind, otherwise the first card for that kind).
fn preset_index(cfg: &GuideConfig) -> usize {
    PRESETS
        .iter()
        .position(|p| {
            p.kind == cfg.kind
                && (p.kind != GuideKind::Symmetry || p.symmetry == Some(cfg.symmetry))
        })
        .unwrap_or(0)
}

fn build_guide_cards(
    guide: &GuideState,
    canvas: &Rc<RefCell<Canvas>>,
    syncing: &Rc<Cell<bool>>,
    refreshers: &Refreshers,
) -> gtk::Box {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();

    let buttons: Vec<gtk::ToggleButton> = PRESETS.iter().map(|p| make_preset_card(*p)).collect();
    for b in buttons.iter().skip(1) {
        b.set_group(Some(&buttons[0]));
    }

    // Lay the cards out three per row.
    let mut row = new_card_row();
    for (i, (btn, preset)) in buttons.iter().zip(PRESETS.iter()).enumerate() {
        {
            let guide = guide.clone();
            let canvas = Rc::clone(canvas);
            let syncing = Rc::clone(syncing);
            let preset = *preset;
            // `clicked` (not `toggled`) so re-picking the already-selected card
            // after a Reset re-creates the guide.
            btn.connect_clicked(move |_| {
                if syncing.get() {
                    return;
                }
                // Reset clears the config; picking a card starts a fresh guide
                // of that type centred on the canvas.
                if guide.config.borrow().is_none() {
                    let cs = canvas.borrow().size();
                    *guide.config.borrow_mut() = Some(GuideConfig::centered(cs.width, cs.height));
                }
                guide.update(|c| {
                    c.kind = preset.kind;
                    if let Some(m) = preset.symmetry {
                        c.symmetry = m;
                    }
                    // Perspective starts with one vanishing point; tapping the
                    // canvas adds more (up to three).
                    if preset.kind == GuideKind::Perspective && c.vanishing_points.is_empty() {
                        c.vanishing_points
                            .push(oxiedraw_core::guides::VanishingPoint::new(c.origin.x, c.origin.y));
                    }
                });
            });
        }
        btn.set_hexpand(true);
        // Keep the cards square as the sidebar width changes.
        btn.add_tick_callback(|b, _| {
            let w = b.width();
            if w > 0 && b.height_request() != w {
                b.set_size_request(-1, w);
            }
            glib::ControlFlow::Continue
        });
        row.append(btn);
        if i % 3 == 2 {
            outer.append(&row);
            row = new_card_row();
        }
    }
    if row.first_child().is_some() {
        outer.append(&row);
    }

    let buttons = Rc::new(buttons);
    {
        let buttons = Rc::clone(&buttons);
        refreshers.borrow_mut().push(Box::new(move |cfg: &GuideConfig| {
            buttons[preset_index(cfg)].set_active(true);
        }));
    }
    outer
}

fn new_card_row() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .valign(gtk::Align::Start)
        .build()
}

fn make_preset_card(preset: GuidePreset) -> gtk::ToggleButton {
    let drawing = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .can_target(false)
        .build();
    drawing.set_draw_func(move |_, cr, w, h| draw_preset_icon(cr, w, h, preset));

    let label = gtk::Label::builder().label(preset.label).build();
    label.inline_css("font-size: 9px; font-weight: 600;");

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    inner.append(&drawing);
    inner.append(&label);

    let btn = gtk::ToggleButton::builder().build();
    btn.set_child(Some(&inner));
    btn.add_css_class("flat");
    btn.inline_css("border-radius: 8px;");
    btn
}

/// Draw a mini preview of a guide preset inside a card.
fn draw_preset_icon(cr: &gtk::cairo::Context, w: i32, h: i32, preset: GuidePreset) {
    use std::f64::consts::{FRAC_PI_2, FRAC_PI_3, FRAC_PI_4};

    let wf = f64::from(w);
    let hf = f64::from(h);
    let cx = wf / 2.0;
    let cy = hf / 2.0;
    let r = (wf.min(hf) / 2.0) - 6.0;
    let (x1, y1, x2, y2) = (6.0, 6.0, wf - 6.0, hf - 6.0);

    // Faint frame like the crop cards.
    cr.set_source_rgba(0.5, 0.5, 0.5, 0.35);
    cr.set_line_width(1.0);
    cr.rectangle(4.0, 4.0, wf - 8.0, hf - 8.0);
    cr.stroke().ok();

    cr.set_source_rgba(0.8, 0.8, 0.8, 0.85);
    cr.set_line_width(1.2);

    let line = |cr: &gtk::cairo::Context, a: f64| {
        let (s, c) = a.sin_cos();
        cr.move_to(cx - c * r, cy - s * r);
        cr.line_to(cx + c * r, cy + s * r);
    };

    match (preset.kind, preset.symmetry) {
        (GuideKind::Symmetry, Some(SymmetryMode::Axis)) => line(cr, FRAC_PI_2),
        (GuideKind::Symmetry, Some(SymmetryMode::Quadrant)) => {
            line(cr, 0.0);
            line(cr, FRAC_PI_2);
        }
        (GuideKind::Symmetry, _) => {
            for k in 0..4 {
                line(cr, f64::from(k) * FRAC_PI_4);
            }
        }
        (GuideKind::Grid2D, _) => {
            for i in 1..3 {
                let t = f64::from(i) / 3.0;
                cr.move_to(x1 + (x2 - x1) * t, y1);
                cr.line_to(x1 + (x2 - x1) * t, y2);
                cr.move_to(x1, y1 + (y2 - y1) * t);
                cr.line_to(x2, y1 + (y2 - y1) * t);
            }
        }
        (GuideKind::Isometric, _) => {
            // An isometric cube: a hexagon with a central Y (three visible edges).
            let verts: Vec<(f64, f64)> = (0..6)
                .map(|k| {
                    let a = FRAC_PI_2 + f64::from(k) * FRAC_PI_3;
                    (cx + a.cos() * r, cy - a.sin() * r)
                })
                .collect();
            cr.move_to(verts[0].0, verts[0].1);
            for v in &verts[1..] {
                cr.line_to(v.0, v.1);
            }
            cr.close_path();
            for &k in &[0usize, 2, 4] {
                cr.move_to(cx, cy);
                cr.line_to(verts[k].0, verts[k].1);
            }
        }
        (GuideKind::Perspective, _) => {
            // Rays converging toward a point above the card.
            let vx = cx;
            let vy = y1 - 2.0;
            for x in [x1, cx, x2] {
                cr.move_to(x, y2);
                cr.line_to(vx, vy);
            }
        }
    }
    cr.stroke().ok();
}

// ---------------------------------------------------------------------------
// Behaviour switches
// ---------------------------------------------------------------------------

fn build_behavior_list(guide: &GuideState, syncing: &Rc<Cell<bool>>, refreshers: &Refreshers) -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);

    list.append(&switch_row(
        "Rotational Symmetry",
        "Rotate copies instead of mirroring them",
        guide,
        syncing,
        refreshers,
        |c| c.rotational,
        |c, v| c.rotational = v,
    ));
    list.append(&switch_row(
        "Assisted Drawing",
        "Snap or mirror strokes onto the guide",
        guide,
        syncing,
        refreshers,
        |c| c.assisted,
        |c, v| c.assisted = v,
    ));
    list
}

// ---------------------------------------------------------------------------
// Appearance (opacity / thickness / grid spacing)
// ---------------------------------------------------------------------------

fn build_appearance_list(
    guide: &GuideState,
    syncing: &Rc<Cell<bool>>,
    refreshers: &Refreshers,
) -> (gtk::ListBox, adw::ActionRow, adw::ActionRow) {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);

    let op = slider_row("Opacity", 0.0, 1.0, 0.01, 2, guide, syncing, refreshers, |c| f64::from(c.opacity), |c, v| c.opacity = v as f32);
    list.append(&op);
    let th = slider_row("Thickness", 0.5, 8.0, 0.1, 1, guide, syncing, refreshers, |c| f64::from(c.thickness), |c, v| c.thickness = v as f32);
    list.append(&th);
    let grid = slider_row("Grid Spacing", 8.0, 256.0, 1.0, 0, guide, syncing, refreshers, |c| f64::from(c.grid_spacing), |c, v| c.grid_spacing = v as f32);
    list.append(&grid);
    let rays = slider_row("Rays", 2.0, 24.0, 1.0, 0, guide, syncing, refreshers, |c| f64::from(c.perspective_rays), |c, v| c.perspective_rays = v.round() as u32);
    list.append(&rays);

    (list, grid, rays)
}

// ---------------------------------------------------------------------------
// Position
// ---------------------------------------------------------------------------

fn build_position_section(guide: &GuideState) -> gtk::Box {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    let hint = gtk::Label::builder()
        .label("Drag the node to move the guide; the outer node rotates it. Pick a Guide Type above to start a new one.")
        .wrap(true)
        .xalign(0.0)
        .build();
    hint.add_css_class("dim-label");
    hint.inline_css("font-size: 12px;");
    outer.append(&hint);

    // Reset clears the guide entirely (hides all guidelines). Re-selecting a
    // Guide Type card starts a fresh one.
    let reset = gtk::Button::with_label("Reset");
    reset.set_halign(gtk::Align::Start);
    reset.add_css_class("destructive-action");
    {
        let guide = guide.clone();
        reset.connect_clicked(move |_| {
            *guide.config.borrow_mut() = None;
            guide.notify_changed();
        });
    }
    outer.append(&reset);
    outer
}

// ---------------------------------------------------------------------------
// Small widget helpers
// ---------------------------------------------------------------------------

fn section_label(text: &str) -> gtk::Label {
    let lbl = gtk::Label::builder().label(text).halign(gtk::Align::Start).build();
    lbl.add_css_class("heading");
    lbl
}

/// An `AdwActionRow` with a trailing switch bound to a bool field.
fn switch_row(
    title: &str,
    subtitle: &str,
    guide: &GuideState,
    syncing: &Rc<Cell<bool>>,
    refreshers: &Refreshers,
    get: impl Fn(&GuideConfig) -> bool + 'static,
    set: impl Fn(&mut GuideConfig, bool) + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    let sw = gtk::Switch::builder().valign(gtk::Align::Center).build();
    {
        let guide = guide.clone();
        let syncing = Rc::clone(syncing);
        sw.connect_active_notify(move |s| {
            if !syncing.get() {
                let v = s.is_active();
                guide.update(|c| set(c, v));
            }
        });
    }
    row.add_suffix(&sw);
    row.set_activatable_widget(Some(&sw));
    {
        let sw = sw.clone();
        refreshers.borrow_mut().push(Box::new(move |cfg: &GuideConfig| sw.set_active(get(cfg))));
    }
    row
}

/// An `AdwActionRow` with a trailing horizontal slider bound to an f32 field.
#[allow(clippy::too_many_arguments)]
fn slider_row(
    title: &str,
    min: f64,
    max: f64,
    step: f64,
    digits: i32,
    guide: &GuideState,
    syncing: &Rc<Cell<bool>>,
    refreshers: &Refreshers,
    get: impl Fn(&GuideConfig) -> f64 + 'static,
    set: impl Fn(&mut GuideConfig, f64) + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    scale.set_hexpand(true);
    scale.set_size_request(150, -1);
    scale.set_draw_value(true);
    scale.set_digits(digits);
    scale.set_round_digits(digits);
    scale.set_value_pos(gtk::PositionType::Left);
    scale.set_valign(gtk::Align::Center);
    {
        let guide = guide.clone();
        let syncing = Rc::clone(syncing);
        scale.connect_value_changed(move |s| {
            if !syncing.get() {
                let v = s.value();
                guide.update(|c| set(c, v));
            }
        });
    }
    row.add_suffix(&scale);
    {
        let scale = scale.clone();
        refreshers.borrow_mut().push(Box::new(move |cfg: &GuideConfig| scale.set_value(get(cfg))));
    }
    row
}
