//! Adjustment-layer UI: create a non-destructive adjustment layer and edit its
//! effect stack in a floating sidebar + content window. The sidebar is the
//! effect checklist; the content pane shows the selected effect's controls as a
//! libadwaita boxed list (the Hue/Sat/Bright panel is the exception - it keeps
//! the rainbow gradient sliders).
//!
//! Unlike the destructive filter popups, edits here write straight to the
//! layer's stored effect stack via `Canvas::set_layer_effects`, which
//! re-composites - so the canvas behind the window is the live preview. The
//! stack is snapshotted on open: Cancel restores it, Apply records one
//! `EffectEdit` undo step.
//!
//! The working model always carries one of every effect (Hue/Sat/Bright, Blur,
//! Sharpen, Invert, Stroke) in that order; each sidebar checkbox drives its
//! `enabled` flag, so disabled effects stay in the stack (and round-trip) but
//! cost nothing at composite time.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::Color;
use oxiedraw_core::effects::{AdjustmentData, Effect, EffectKind, StrokeSoftness};
use oxiedraw_core::history::{HistoryAction, HistoryStack};
use relm4::gtk;

use crate::canvas::RedrawHandle;
use crate::toaster::Toaster;
use crate::widgets::gradient_slider::{self, hsl_to_rgb};
use crate::widgets::{boxed_list, slider};

/// Shared handles the adjustment actions need, built per invocation from the
/// active document.
#[derive(Clone)]
pub(crate) struct AdjustmentContext {
    pub window: adw::ApplicationWindow,
    pub canvas: Rc<RefCell<Canvas>>,
    pub redraw: RedrawHandle,
    pub history: Rc<RefCell<HistoryStack>>,
    pub toaster: Toaster,
    pub refresh_layers: Rc<dyn Fn()>,
}

/// Menu/button entry point: if the active layer is already an adjustment layer,
/// open its editor; otherwise create a new one on top and open it.
pub(crate) fn add_or_edit(ctx: &AdjustmentContext) {
    let active_adjustment = {
        let c = ctx.canvas.borrow();
        c.layers()
            .active()
            .filter(|&idx| c.layer_effects(idx).is_some())
    };

    if let Some(idx) = active_adjustment {
        open_editor(ctx, idx);
        return;
    }

    let new_idx = {
        let mut c = ctx.canvas.borrow_mut();
        c.add_adjustment_layer("Adjustment")
    };
    match new_idx {
        Ok(idx) => {
            // Record the layer creation so it can be undone in one step.
            if let Some((id, name, visible, kind, blend, opacity, pixels)) =
                capture_layer(&ctx.canvas, idx)
            {
                ctx.history.borrow_mut().record(HistoryAction::LayerAdd {
                    idx,
                    id,
                    name,
                    visible,
                    layer_kind: kind,
                    blend,
                    opacity,
                    pixels,
                });
            }
            (ctx.refresh_layers)();
            ctx.redraw.request();
            open_editor(ctx, idx);
        }
        Err(e) => {
            tracing::error!(error = %e, "add adjustment layer failed");
            ctx.toaster.info("Could not add adjustment layer");
        }
    }
}

fn capture_layer(
    canvas: &Rc<RefCell<Canvas>>,
    idx: usize,
) -> Option<(
    String,
    String,
    bool,
    oxiedraw_core::document::LayerKind,
    oxiedraw_core::document::BlendMode,
    f32,
    Vec<u8>,
)> {
    let mut c = canvas.borrow_mut();
    let layer = c.layers().snapshot().get(idx)?.clone();
    let (blend, opacity) = c.layers().blend(idx).unwrap_or_default();
    let pixels = c.read_layer(idx).ok()?;
    Some((
        layer.id,
        layer.name,
        layer.visible,
        layer.kind,
        blend,
        opacity,
        pixels,
    ))
}

/// Working copy of every effect, behind a `RefCell` so each control's callback
/// can mutate one field and push the whole stack to the canvas. The field order
/// here is the composite order (bottom to top).
struct Working {
    hsb: Effect,
    blur: Effect,
    sharpen: Effect,
    invert: Effect,
    stroke: Effect,
}

impl Working {
    fn from_data(data: &AdjustmentData) -> Self {
        let find = |pick: fn(&EffectKind) -> bool, default: EffectKind| -> Effect {
            data.effects
                .iter()
                .find(|e| pick(&e.kind))
                .cloned()
                .unwrap_or_else(|| {
                    let mut e = Effect::new(default);
                    e.enabled = false;
                    e
                })
        };
        Self {
            hsb: find(
                |k| matches!(k, EffectKind::HueSatBright { .. }),
                EffectKind::hue_sat_bright_identity(),
            ),
            blur: find(
                |k| matches!(k, EffectKind::Blur { .. }),
                EffectKind::blur_default(),
            ),
            sharpen: find(
                |k| matches!(k, EffectKind::Sharpen { .. }),
                EffectKind::sharpen_default(),
            ),
            invert: find(|k| matches!(k, EffectKind::Invert), EffectKind::Invert),
            stroke: find(
                |k| matches!(k, EffectKind::Stroke { .. }),
                EffectKind::stroke_default(),
            ),
        }
    }

    fn assemble(&self) -> AdjustmentData {
        AdjustmentData {
            effects: vec![
                self.hsb.clone(),
                self.blur.clone(),
                self.sharpen.clone(),
                self.invert.clone(),
                self.stroke.clone(),
            ],
        }
    }
}

/// Resolve a layer's current index from its stable id (it could have moved
/// while the non-modal editor was open).
fn layer_idx(canvas: &Rc<RefCell<Canvas>>, id: &str) -> Option<usize> {
    canvas
        .borrow()
        .layers()
        .snapshot()
        .iter()
        .position(|l| l.id == id)
}

fn open_editor(ctx: &AdjustmentContext, idx: usize) {
    let (layer_id, before) = {
        let c = ctx.canvas.borrow();
        let id = c
            .layers()
            .snapshot()
            .get(idx)
            .map(|l| l.id.clone())
            .unwrap_or_default();
        let data = c.layer_effects(idx).unwrap_or_default();
        (id, data)
    };

    let working = Rc::new(RefCell::new(Working::from_data(&before)));

    // Push the current working stack to the canvas and redraw (live preview).
    let apply_live: Rc<dyn Fn()> = {
        let ctx = ctx.clone();
        let working = Rc::clone(&working);
        let layer_id = layer_id.clone();
        Rc::new(move || {
            if let Some(idx) = layer_idx(&ctx.canvas, &layer_id) {
                let data = working.borrow().assemble();
                if let Err(e) = ctx.canvas.borrow_mut().set_layer_effects(idx, data) {
                    tracing::error!(error = %e, "set_layer_effects failed");
                }
                ctx.redraw.request();
            }
        })
    };

    let window = adw::Window::builder()
        .transient_for(&ctx.window)
        .modal(false)
        .title("Adjustment Effects")
        .default_width(640)
        .default_height(460)
        .resizable(true)
        .build();

    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();
    let cancel_btn = gtk::Button::with_label("Cancel");
    let apply_btn = gtk::Button::with_label("Apply");
    apply_btn.add_css_class("suggested-action");
    header.pack_start(&cancel_btn);
    header.pack_end(&apply_btn);

    // Sidebar (effect checklist) | content stack of boxed-list panels.
    let sidebar = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .width_request(210)
        .build();
    sidebar.add_css_class("sidebar");
    let stack = gtk::Stack::builder()
        .hexpand(true)
        .vexpand(true)
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();

    add_effect_page(&sidebar, &stack, "hsb", &working.borrow().hsb);
    add_effect_page(&sidebar, &stack, "blur", &working.borrow().blur);
    add_effect_page(&sidebar, &stack, "sharpen", &working.borrow().sharpen);
    add_effect_page(&sidebar, &stack, "invert", &working.borrow().invert);
    add_effect_page(&sidebar, &stack, "stroke", &working.borrow().stroke);
    // Fill the pages + wire the checkboxes now that the rows exist.
    bind_pages(&sidebar, &stack, &working, &apply_live);

    {
        let stack = stack.clone();
        sidebar.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                stack.set_visible_child_name(&row.widget_name());
            }
        });
    }
    if let Some(first) = sidebar.row_at_index(0) {
        sidebar.select_row(Some(&first));
    }

    let content_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&stack)
        .build();

    let split = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();
    split.append(&sidebar);
    split.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    split.append(&content_scroll);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&split));
    window.set_content(Some(&toolbar));

    let applied = Rc::new(std::cell::Cell::new(false));

    {
        let window = window.clone();
        let applied = Rc::clone(&applied);
        let ctx = ctx.clone();
        let working = Rc::clone(&working);
        let layer_id = layer_id.clone();
        let before = before.clone();
        apply_btn.connect_clicked(move |_| {
            applied.set(true);
            let after = working.borrow().assemble();
            if after != before {
                ctx.history.borrow_mut().record(HistoryAction::EffectEdit {
                    layer_id: layer_id.clone(),
                    before: before.clone(),
                    after,
                });
            }
            (ctx.refresh_layers)();
            window.close();
        });
    }
    {
        let window = window.clone();
        cancel_btn.connect_clicked(move |_| window.close());
    }
    {
        let ctx = ctx.clone();
        let applied = Rc::clone(&applied);
        let layer_id = layer_id.clone();
        let before = before.clone();
        window.connect_close_request(move |_| {
            if !applied.get() {
                // Cancel: restore the stack as it was on open.
                if let Some(idx) = layer_idx(&ctx.canvas, &layer_id) {
                    let _ = ctx
                        .canvas
                        .borrow_mut()
                        .set_layer_effects(idx, before.clone());
                    ctx.redraw.request();
                }
            }
            gtk::glib::Propagation::Proceed
        });
    }
    {
        let key = gtk::EventControllerKey::new();
        let window_c = window.clone();
        key.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::Escape {
                window_c.close();
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        window.add_controller(key);
    }

    window.present();
}

// ---------------------------------------------------------------------------
// Sidebar rows + content panels. Each effect gets a sidebar row (enable
// checkbox + icon + name) and a Stack page; the page holds its controls as a
// boxed list, except Hue/Sat/Bright which uses the rainbow gradient sliders.
// ---------------------------------------------------------------------------

/// Build one sidebar row (enable checkbox + icon + title) plus an empty content
/// page named `name`; the actual panel widgets are added by `bind_pages`.
fn add_effect_page(sidebar: &gtk::ListBox, stack: &gtk::Stack, name: &str, effect: &Effect) {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(name);
    let hbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let check = gtk::CheckButton::builder().active(effect.enabled).build();
    check.set_widget_name(&format!("{name}-check"));
    let label = gtk::Label::builder()
        .label(effect.kind.display_name())
        .xalign(0.0)
        .hexpand(true)
        .build();
    hbox.append(&check);
    hbox.append(&label);
    row.set_child(Some(&hbox));
    sidebar.append(&row);

    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    stack.add_named(&page, Some(name));
}

/// Wire each row's checkbox to its effect's `enabled` flag and fill each content
/// page with the effect's controls. Kept separate so the row widgets exist
/// before their callbacks capture the working state.
fn bind_pages(
    sidebar: &gtk::ListBox,
    stack: &gtk::Stack,
    working: &Rc<RefCell<Working>>,
    apply_live: &Rc<dyn Fn()>,
) {
    let selectors: [(&str, Select); 5] = [
        ("hsb", |w| &mut w.hsb),
        ("blur", |w| &mut w.blur),
        ("sharpen", |w| &mut w.sharpen),
        ("invert", |w| &mut w.invert),
        ("stroke", |w| &mut w.stroke),
    ];
    for (name, select) in selectors {
        if let Some(check) = find_row(sidebar, name).and_then(|r| first_check(&r)) {
            let working = Rc::clone(working);
            let apply_live = Rc::clone(apply_live);
            check.connect_toggled(move |c| {
                select(&mut working.borrow_mut()).enabled = c.is_active();
                apply_live();
            });
        }
    }

    if let Some(page) = page_box(stack, "hsb") {
        build_hsb_panel(&page, working, apply_live);
    }
    if let Some(page) = page_box(stack, "blur") {
        build_blur_panel(&page, working, apply_live);
    }
    if let Some(page) = page_box(stack, "sharpen") {
        build_sharpen_panel(&page, working, apply_live);
    }
    if let Some(page) = page_box(stack, "invert") {
        build_invert_panel(&page);
    }
    if let Some(page) = page_box(stack, "stroke") {
        build_stroke_panel(&page, working, apply_live);
    }
}

/// Selects which effect in the working stack a checkbox/control mutates.
type Select = fn(&mut Working) -> &mut Effect;

fn page_box(stack: &gtk::Stack, name: &str) -> Option<gtk::Box> {
    stack.child_by_name(name)?.downcast::<gtk::Box>().ok()
}

fn find_row(sidebar: &gtk::ListBox, name: &str) -> Option<gtk::ListBoxRow> {
    let mut i = 0;
    while let Some(row) = sidebar.row_at_index(i) {
        if row.widget_name() == name {
            return Some(row);
        }
        i += 1;
    }
    None
}

fn first_check(row: &gtk::ListBoxRow) -> Option<gtk::CheckButton> {
    let hbox = row.child()?.downcast::<gtk::Box>().ok()?;
    let mut child = hbox.first_child();
    while let Some(w) = child {
        if let Ok(c) = w.clone().downcast::<gtk::CheckButton>() {
            return Some(c);
        }
        child = w.next_sibling();
    }
    None
}

/// A section heading above a boxed list.
fn section(title: &str) -> gtk::Label {
    let lbl = gtk::Label::builder().label(title).xalign(0.0).build();
    lbl.add_css_class("heading");
    lbl
}

fn build_hsb_panel(page: &gtk::Box, working: &Rc<RefCell<Working>>, apply_live: &Rc<dyn Fn()>) {
    let EffectKind::HueSatBright {
        hue_degrees,
        saturation,
        brightness,
    } = working.borrow().hsb.kind
    else {
        return;
    };

    // The hue ramp shifts with the rotation value, so every slider shares this
    // cell and the saturation bar is refreshed when hue moves.
    let hue = Rc::new(Cell::new(f64::from(hue_degrees)));

    let hue_slider = gradient_slider::build(
        "_Hue",
        (-180.0, 180.0),
        1.0,
        0,
        f64::from(hue_degrees),
        {
            let hue = Rc::clone(&hue);
            move |t| hsl_to_rgb(t * 360.0 + hue.get(), 1.0, 0.5)
        },
        {
            let working = Rc::clone(working);
            let apply_live = Rc::clone(apply_live);
            let hue = Rc::clone(&hue);
            move |v| {
                hue.set(v);
                set_hsb(&working, |k| {
                    if let EffectKind::HueSatBright { hue_degrees, .. } = k {
                        *hue_degrees = v as f32;
                    }
                });
                apply_live();
            }
        },
    );

    let sat_slider = gradient_slider::build(
        "_Saturation",
        (0.0, 2.0),
        0.01,
        2,
        f64::from(saturation),
        {
            let hue = Rc::clone(&hue);
            move |t| hsl_to_rgb(hue.get(), t, 0.5)
        },
        {
            let working = Rc::clone(working);
            let apply_live = Rc::clone(apply_live);
            move |v| {
                set_hsb(&working, |k| {
                    if let EffectKind::HueSatBright { saturation, .. } = k {
                        *saturation = v as f32;
                    }
                });
                apply_live();
            }
        },
    );

    let bright_slider = gradient_slider::build(
        "_Brightness",
        (0.0, 2.0),
        0.01,
        2,
        f64::from(brightness),
        |t| (t, t, t),
        {
            let working = Rc::clone(working);
            let apply_live = Rc::clone(apply_live);
            move |v| {
                set_hsb(&working, |k| {
                    if let EffectKind::HueSatBright { brightness, .. } = k {
                        *brightness = v as f32;
                    }
                });
                apply_live();
            }
        },
    );

    // Moving hue restyles the saturation ramp (it samples the current hue).
    let sat_area = sat_slider.area();
    hue_slider.connect_changed(move |_| sat_area.queue_draw());

    page.append(&section("Adjust"));
    page.append(&hue_slider.widget);
    page.append(&sat_slider.widget);
    page.append(&bright_slider.widget);
}

fn set_hsb(working: &Rc<RefCell<Working>>, f: impl FnOnce(&mut EffectKind)) {
    f(&mut working.borrow_mut().hsb.kind);
}

fn build_blur_panel(page: &gtk::Box, working: &Rc<RefCell<Working>>, apply_live: &Rc<dyn Fn()>) {
    let EffectKind::Blur { radius_x, radius_y } = working.borrow().blur.kind else {
        return;
    };

    let list = boxed_list::list();

    let type_combo = gtk::DropDown::from_strings(&["Box Blur"]);
    list.append(&boxed_list::row("Type", &type_combo, &[]));

    // Lock links the two radii so they move together (default on when equal).
    let locked = Rc::new(Cell::new((radius_x - radius_y).abs() < f32::EPSILON));
    let h_scale = slider::build((0.0, 100.0), 1.0, f64::from(radius_x), 200, fmt_px, {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        move |v| {
            set_blur(&working, |b| b.0 = v as f32);
            apply_live();
        }
    });
    let v_scale = slider::build((0.0, 100.0), 1.0, f64::from(radius_y), 200, fmt_px, {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        move |v| {
            set_blur(&working, |b| b.1 = v as f32);
            apply_live();
        }
    });

    wire_radius_lock(&h_scale, &v_scale, &locked);

    let lock_btn = gtk::ToggleButton::builder()
        .icon_name(if locked.get() {
            "changes-prevent-symbolic"
        } else {
            "changes-allow-symbolic"
        })
        .active(locked.get())
        .tooltip_text("Link horizontal and vertical blur")
        .build();
    lock_btn.add_css_class("flat");
    {
        let locked = Rc::clone(&locked);
        let v_scale = v_scale.clone();
        let h_scale = h_scale.clone();
        lock_btn.connect_toggled(move |b| {
            locked.set(b.is_active());
            b.set_icon_name(if b.is_active() {
                "changes-prevent-symbolic"
            } else {
                "changes-allow-symbolic"
            });
            if b.is_active() {
                v_scale.set_value(h_scale.value());
            }
        });
    }

    list.append(&boxed_list::row(
        "Horizontal",
        &h_scale,
        &[lock_btn.upcast_ref()],
    ));
    list.append(&boxed_list::row("Vertical", &v_scale, &[]));
    page.append(&list);
}

/// Mutate the blur radii through a `(radius_x, radius_y)` view.
fn set_blur(working: &Rc<RefCell<Working>>, f: impl FnOnce(&mut (f32, f32))) {
    if let EffectKind::Blur { radius_x, radius_y } = &mut working.borrow_mut().blur.kind {
        let mut pair = (*radius_x, *radius_y);
        f(&mut pair);
        *radius_x = pair.0;
        *radius_y = pair.1;
    }
}

/// Keep two scales in sync while locked, guarding the re-entrant value-changed
/// the programmatic set would trigger.
fn wire_radius_lock(h_scale: &gtk::Scale, v_scale: &gtk::Scale, locked: &Rc<Cell<bool>>) {
    let syncing = Rc::new(Cell::new(false));
    {
        let other = v_scale.clone();
        let locked = Rc::clone(locked);
        let syncing = Rc::clone(&syncing);
        h_scale.connect_value_changed(move |s| {
            if locked.get() && !syncing.get() {
                syncing.set(true);
                other.set_value(s.value());
                syncing.set(false);
            }
        });
    }
    {
        let other = h_scale.clone();
        let locked = Rc::clone(locked);
        let syncing = Rc::clone(&syncing);
        v_scale.connect_value_changed(move |s| {
            if locked.get() && !syncing.get() {
                syncing.set(true);
                other.set_value(s.value());
                syncing.set(false);
            }
        });
    }
}

#[allow(clippy::cast_possible_truncation)]
fn fmt_px(v: f64) -> String {
    format!("{} px", v.round() as i64)
}

fn build_sharpen_panel(page: &gtk::Box, working: &Rc<RefCell<Working>>, apply_live: &Rc<dyn Fn()>) {
    let EffectKind::Sharpen { amount } = working.borrow().sharpen.kind else {
        return;
    };

    let list = boxed_list::list();

    let type_combo = gtk::DropDown::from_strings(&["Unsharp Mask"]);
    list.append(&boxed_list::row("Type", &type_combo, &[]));

    let strength = slider::build(
        (0.0, 100.0),
        0.05,
        f64::from(amount),
        200,
        |v| format!("{v:.2}"),
        {
            let working = Rc::clone(working);
            let apply_live = Rc::clone(apply_live);
            move |v| {
                if let EffectKind::Sharpen { amount } = &mut working.borrow_mut().sharpen.kind {
                    *amount = v as f32;
                }
                apply_live();
            }
        },
    );
    list.append(&boxed_list::row("Strength", &strength, &[]));
    page.append(&list);
}

fn build_invert_panel(page: &gtk::Box) {
    let list = boxed_list::list();
    list.append(&boxed_list::info_row(
        "Inverts the colors of everything below. No options.",
    ));
    page.append(&list);
}

fn build_stroke_panel(page: &gtk::Box, working: &Rc<RefCell<Working>>, apply_live: &Rc<dyn Fn()>) {
    let EffectKind::Stroke {
        color,
        opacity,
        thickness,
        offset,
        softness,
    } = working.borrow().stroke.kind
    else {
        return;
    };

    // --- Color section ---
    page.append(&section("Color"));
    let color_list = boxed_list::list();

    let color_dialog = gtk::ColorDialog::new();
    let color_btn = gtk::ColorDialogButton::new(Some(color_dialog));
    color_btn.set_rgba(&gtk::gdk::RGBA::new(
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    ));
    color_btn.set_hexpand(false);
    color_btn.set_halign(gtk::Align::End);
    {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        color_btn.connect_rgba_notify(move |btn| {
            let rgba = btn.rgba();
            if let EffectKind::Stroke { color, .. } = &mut working.borrow_mut().stroke.kind {
                *color = Color {
                    r: (rgba.red() * 255.0).round() as u8,
                    g: (rgba.green() * 255.0).round() as u8,
                    b: (rgba.blue() * 255.0).round() as u8,
                };
            }
            apply_live();
        });
    }
    color_list.append(&boxed_list::row("Color", &color_btn, &[]));

    let op = slider::build(
        (0.0, 1.0),
        0.01,
        f64::from(opacity),
        200,
        |v| format!("{:.0}%", v * 100.0),
        {
            let working = Rc::clone(working);
            let apply_live = Rc::clone(apply_live);
            move |v| {
                if let EffectKind::Stroke { opacity, .. } = &mut working.borrow_mut().stroke.kind {
                    *opacity = v as f32;
                }
                apply_live();
            }
        },
    );
    color_list.append(&boxed_list::row("Opacity", &op, &[]));
    page.append(&color_list);

    // --- Stroke Design section ---
    page.append(&section("Stroke Design"));
    let design_list = boxed_list::list();

    // Thickness: slider with +/- steppers for gradual control.
    let thickness_adj = gtk::Adjustment::new(f64::from(thickness), 0.0, 100.0, 1.0, 5.0, 0.0);
    let thickness_scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&thickness_adj));
    thickness_scale.set_draw_value(true);
    thickness_scale.set_value_pos(gtk::PositionType::Right);
    {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        thickness_adj.connect_value_changed(move |a| {
            if let EffectKind::Stroke { thickness, .. } = &mut working.borrow_mut().stroke.kind {
                *thickness = a.value() as f32;
            }
            apply_live();
        });
    }
    let minus = gtk::Button::from_icon_name("list-remove-symbolic");
    let plus = gtk::Button::from_icon_name("list-add-symbolic");
    minus.add_css_class("flat");
    plus.add_css_class("flat");
    {
        let adj = thickness_adj.clone();
        minus.connect_clicked(move |_| adj.set_value(adj.value() - 1.0));
    }
    {
        let adj = thickness_adj.clone();
        plus.connect_clicked(move |_| adj.set_value(adj.value() + 1.0));
    }
    let thickness_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    thickness_box.append(&minus);
    thickness_scale.set_hexpand(true);
    thickness_box.append(&thickness_scale);
    thickness_box.append(&plus);
    design_list.append(&boxed_list::row("Thickness", &thickness_box, &[]));

    // Offset: -1 inside .. 0 center .. +1 outside.
    let off = slider::build(
        (-1.0, 1.0),
        0.01,
        f64::from(offset),
        200,
        |v| {
            let where_ = if v < -0.33 {
                "inside"
            } else if v > 0.33 {
                "outside"
            } else {
                "center"
            };
            format!("{v:.2} ({where_})")
        },
        {
            let working = Rc::clone(working);
            let apply_live = Rc::clone(apply_live);
            move |v| {
                if let EffectKind::Stroke { offset, .. } = &mut working.borrow_mut().stroke.kind {
                    *offset = v as f32;
                }
                apply_live();
            }
        },
    );
    design_list.append(&boxed_list::row("Offset", &off, &[]));

    // Softness.
    let labels: Vec<&str> = StrokeSoftness::ALL.iter().map(|s| s.label()).collect();
    let dropdown = gtk::DropDown::from_strings(&labels);
    dropdown.set_selected(softness.to_index());
    {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        dropdown.connect_selected_notify(move |d| {
            let s = StrokeSoftness::from_index(d.selected());
            if let EffectKind::Stroke { softness, .. } = &mut working.borrow_mut().stroke.kind {
                *softness = s;
            }
            apply_live();
        });
    }
    design_list.append(&boxed_list::row("Softness", &dropdown, &[]));
    page.append(&design_list);
}
