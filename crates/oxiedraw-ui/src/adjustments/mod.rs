//! Adjustment-layer UI: create a non-destructive adjustment layer and edit its
//! effect stack in a floating split-view window.
//!
//! Unlike the destructive filter popups, edits here write straight to the
//! layer's stored effect stack via `Canvas::set_layer_effects`, which
//! re-composites - so the canvas behind the window is the live preview. The
//! stack is snapshotted on open: Cancel restores it, Apply records one
//! `EffectEdit` undo step.
//!
//! The working model always carries exactly three effects (Hue/Sat/Bright,
//! Blur, Stroke) in that order; each sidebar checkbox drives its `enabled`
//! flag, so disabled effects stay in the stack (and round-trip) but cost
//! nothing at composite time.

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
use crate::widgets::slider;

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

/// Working copy of the three effects, behind a `RefCell` so each control's
/// callback can mutate one field and push the whole stack to the canvas.
struct Working {
    hsb: Effect,
    blur: Effect,
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
            stroke: find(
                |k| matches!(k, EffectKind::Stroke { .. }),
                EffectKind::stroke_default(),
            ),
        }
    }

    fn assemble(&self) -> AdjustmentData {
        AdjustmentData {
            effects: vec![self.hsb.clone(), self.blur.clone(), self.stroke.clone()],
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
        .default_height(420)
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

    // Sidebar (checklist) | content stack.
    let sidebar = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .width_request(200)
        .build();
    sidebar.add_css_class("navigation-sidebar");
    let stack = gtk::Stack::builder()
        .hexpand(true)
        .vexpand(true)
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();

    add_effect_page(&sidebar, &stack, "hsb", &working.borrow().hsb);
    add_effect_page(&sidebar, &stack, "blur", &working.borrow().blur);
    add_effect_page(&sidebar, &stack, "stroke", &working.borrow().stroke);
    // Fill the pages + wire the checkboxes now that the rows exist.
    bind_pages(&sidebar, &stack, &working, &apply_live);

    {
        let stack = stack.clone();
        sidebar.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let name = row.widget_name();
                stack.set_visible_child_name(&name);
            }
        });
    }
    if let Some(first) = sidebar.row_at_index(0) {
        sidebar.select_row(Some(&first));
    }

    let split = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();
    split.append(&sidebar);
    split.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    split.append(&stack);

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

/// Build one sidebar row (icon + title + enable checkbox) plus an empty content
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
    let icon = gtk::Image::from_icon_name(effect.kind.icon_name());
    icon.add_css_class("adjustment-effect-icon");
    let label = gtk::Label::builder()
        .label(effect.kind.display_name())
        .xalign(0.0)
        .hexpand(true)
        .build();
    hbox.append(&check);
    hbox.append(&icon);
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

/// Wire each row's checkbox to its effect's `enabled` flag and fill each
/// content page with the effect's controls. Kept separate so the row widgets
/// exist before their callbacks capture the working state.
fn bind_pages(
    sidebar: &gtk::ListBox,
    stack: &gtk::Stack,
    working: &Rc<RefCell<Working>>,
    apply_live: &Rc<dyn Fn()>,
) {
    // Checkboxes.
    for (name, set_enabled) in [
        ("hsb", 0u8),
        ("blur", 1u8),
        ("stroke", 2u8),
    ] {
        if let Some(row) = find_row(sidebar, name) {
            if let Some(check) = first_check(&row) {
                let working = Rc::clone(working);
                let apply_live = Rc::clone(apply_live);
                check.connect_toggled(move |c| {
                    let mut w = working.borrow_mut();
                    let target = match set_enabled {
                        0 => &mut w.hsb,
                        1 => &mut w.blur,
                        _ => &mut w.stroke,
                    };
                    target.enabled = c.is_active();
                    drop(w);
                    apply_live();
                });
            }
        }
    }

    if let Some(page) = stack
        .child_by_name("hsb")
        .and_then(|w| w.downcast::<gtk::Box>().ok())
    {
        build_hsb_panel(&page, working, apply_live);
    }
    if let Some(page) = stack
        .child_by_name("blur")
        .and_then(|w| w.downcast::<gtk::Box>().ok())
    {
        build_blur_panel(&page, working, apply_live);
    }
    if let Some(page) = stack
        .child_by_name("stroke")
        .and_then(|w| w.downcast::<gtk::Box>().ok())
    {
        build_stroke_panel(&page, working, apply_live);
    }
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

fn labeled(label: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let lbl = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .width_request(96)
        .build();
    row.append(&lbl);
    let w = control.as_ref();
    w.set_hexpand(true);
    row.append(w);
    row
}

fn section(title: &str) -> gtk::Label {
    let lbl = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .build();
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

    page.append(&section("Adjust"));

    let hue = slider::build((-180.0, 180.0), 1.0, f64::from(hue_degrees), 220, |v| format!("{v:.0} deg"), {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        move |v| {
            set_hsb(&working, |k| {
                if let EffectKind::HueSatBright { hue_degrees, .. } = k {
                    *hue_degrees = v as f32;
                }
            });
            apply_live();
        }
    });
    page.append(&labeled("Hue", &hue));

    let sat = slider::build((0.0, 2.0), 0.01, f64::from(saturation), 220, |v| format!("{v:.2}"), {
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
    });
    page.append(&labeled("Saturation", &sat));

    let bright = slider::build((0.0, 2.0), 0.01, f64::from(brightness), 220, |v| format!("{v:.2}"), {
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
    });
    page.append(&labeled("Brightness", &bright));
}

fn set_hsb(working: &Rc<RefCell<Working>>, f: impl FnOnce(&mut EffectKind)) {
    f(&mut working.borrow_mut().hsb.kind);
}

fn build_blur_panel(page: &gtk::Box, working: &Rc<RefCell<Working>>, apply_live: &Rc<dyn Fn()>) {
    let EffectKind::Blur { radius } = working.borrow().blur.kind else {
        return;
    };
    page.append(&section("Blur"));
    let r = slider::build((0.0, 50.0), 0.5, f64::from(radius), 220, |v| format!("{v:.1} px"), {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        move |v| {
            if let EffectKind::Blur { radius } = &mut working.borrow_mut().blur.kind {
                *radius = v as f32;
            }
            apply_live();
        }
    });
    page.append(&labeled("Radius", &r));
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

    let color_dialog = gtk::ColorDialog::new();
    let color_btn = gtk::ColorDialogButton::new(Some(color_dialog));
    color_btn.set_rgba(&gtk::gdk::RGBA::new(
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    ));
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
    page.append(&labeled("Color", &color_btn));

    let op = slider::build((0.0, 1.0), 0.01, f64::from(opacity), 220, |v| format!("{:.0}%", v * 100.0), {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        move |v| {
            if let EffectKind::Stroke { opacity, .. } = &mut working.borrow_mut().stroke.kind {
                *opacity = v as f32;
            }
            apply_live();
        }
    });
    page.append(&labeled("Opacity", &op));

    // --- Stroke Design section ---
    page.append(&section("Stroke Design"));

    // Thickness: slider with +/- steppers for gradual control.
    let thickness_adj = gtk::Adjustment::new(f64::from(thickness), 0.0, 100.0, 1.0, 5.0, 0.0);
    let thickness_scale =
        gtk::Scale::new(gtk::Orientation::Horizontal, Some(&thickness_adj));
    thickness_scale.set_draw_value(true);
    thickness_scale.set_value_pos(gtk::PositionType::Right);
    thickness_scale.set_hexpand(true);
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
    {
        let adj = thickness_adj.clone();
        minus.connect_clicked(move |_| adj.set_value(adj.value() - 1.0));
    }
    {
        let adj = thickness_adj.clone();
        plus.connect_clicked(move |_| adj.set_value(adj.value() + 1.0));
    }
    let thickness_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let tl = gtk::Label::builder()
        .label("Thickness")
        .xalign(0.0)
        .width_request(96)
        .build();
    thickness_row.append(&tl);
    thickness_row.append(&minus);
    thickness_row.append(&thickness_scale);
    thickness_row.append(&plus);
    page.append(&thickness_row);

    // Offset: -1 inside .. 0 center .. +1 outside.
    let off = slider::build((-1.0, 1.0), 0.01, f64::from(offset), 220, |v| {
        let where_ = if v < -0.33 {
            "inside"
        } else if v > 0.33 {
            "outside"
        } else {
            "center"
        };
        format!("{v:.2} ({where_})")
    }, {
        let working = Rc::clone(working);
        let apply_live = Rc::clone(apply_live);
        move |v| {
            if let EffectKind::Stroke { offset, .. } = &mut working.borrow_mut().stroke.kind {
                *offset = v as f32;
            }
            apply_live();
        }
    });
    page.append(&labeled("Offset", &off));

    // Softness dropdown.
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
    page.append(&labeled("Softness", &dropdown));
}
