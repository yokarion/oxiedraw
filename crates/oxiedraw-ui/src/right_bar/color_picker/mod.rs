//! Krita-style HSV color picker: a hue ring with a rotating HSV triangle,
//! a primary/secondary swatch, RGB spin buttons and a hex entry.
//!
//! Persistent colors live in core's `ColorState`; the picker wraps it in
//! `PickerState`, which adds working HSV so achromatic stored colors don't
//! clobber the hue, plus a `syncing` flag. All four input surfaces (wheel,
//! swatch, spinners, hex) write through the same state and end in one
//! `refresh` closure that repopulates every widget; `syncing` guards against
//! the resulting widget-callback recursion.

mod swatch;
mod wheel;

use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::color::{Color, ColorState};
use relm4::gtk;
use relm4::gtk::prelude::*;

use self::swatch::{build_swatch_widget, install_swatch_draw, install_swatch_input};
use self::wheel::{build_wheel_widget, install_wheel_draw, install_wheel_input};

const PANEL_MARGIN: i32 = 12;
pub(super) const WHEEL_SIZE: i32 = 200;
pub(super) const WHEEL_OUTER_RATIO: f64 = 0.48;
pub(super) const WHEEL_INNER_RATIO: f64 = 0.38;
pub(super) const TRIANGLE_INSET: f64 = 0.96;
pub(super) const HUE_INDICATOR_HALF_ANGLE: f64 = 0.06;
pub(super) const SV_INDICATOR_RADIUS: f64 = 5.5;
pub(super) const SWATCH_INNER: f64 = 30.0;
pub(super) const SWATCH_OFFSET: f64 = 14.0;
pub(super) const SWATCH_TOTAL: i32 = 44;

/// Wraps the core `ColorState` with picker-only working HSV and a `syncing`
/// flag used to break update loops between widget callbacks.
#[derive(Debug, Clone)]
pub(super) struct PickerState {
    pub(super) colors: ColorState,
    pub(super) hue: Rc<Cell<f32>>,
    pub(super) saturation: Rc<Cell<f32>>,
    pub(super) value: Rc<Cell<f32>>,
    pub(super) syncing: Rc<Cell<bool>>,
}

impl PickerState {
    fn new(colors: ColorState) -> Self {
        let (h, s, v) = colors.current().to_hsv();
        Self {
            colors,
            hue: Rc::new(Cell::new(h)),
            saturation: Rc::new(Cell::new(s)),
            value: Rc::new(Cell::new(v)),
            syncing: Rc::new(Cell::new(false)),
        }
    }

    pub(super) fn write_color_from_hsv(&self) {
        let c = Color::from_hsv(self.hue.get(), self.saturation.get(), self.value.get());
        self.commit_color(c);
    }

    /// Set the active colour and broadcast it on the `ColorState` bus so other
    /// listeners (e.g. live text recolouring) react. `syncing` is held across
    /// the notify so the picker's own change-listener skips reloading (it
    /// already holds the correct state); external changes leave it clear.
    pub(super) fn commit_color(&self, c: Color) {
        self.syncing.set(true);
        self.colors.set_current(c);
        self.colors.notify_changed();
        self.syncing.set(false);
    }

    /// Preserve hue to avoid jumps in achromatic regions.
    pub(super) fn load_hsv_from_current(&self) {
        let (h, s, v) = self.colors.current().to_hsv();
        if s > f32::EPSILON {
            self.hue.set(h);
        }
        self.saturation.set(s);
        self.value.set(v);
    }
}

pub(crate) fn build(colors: ColorState) -> gtk::Box {
    let state = PickerState::new(colors);

    // Outer panel fills the Paned slot edge-to-edge so the .sidebar
    // background has no gaps; inner box owns the padding.
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .vexpand(true)
        .hexpand(true)
        .build();
    panel.add_css_class("sidebar");

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(PANEL_MARGIN)
        .margin_bottom(PANEL_MARGIN)
        .margin_start(PANEL_MARGIN)
        .margin_end(PANEL_MARGIN)
        .build();

    let wheel = build_wheel_widget();
    let swatch = build_swatch_widget();

    let overlay = gtk::Overlay::builder().child(&wheel).build();
    swatch.set_halign(gtk::Align::Start);
    swatch.set_valign(gtk::Align::Start);
    overlay.add_overlay(&swatch);
    content.append(&overlay);

    let (rgb_hex_row, refresh_inputs) = build_inputs(&state, &wheel, &swatch);
    content.append(&rgb_hex_row);

    panel.append(&content);

    install_wheel_draw(&wheel, &state);
    install_wheel_input(&wheel, &state, &refresh_inputs);
    install_swatch_draw(&swatch, &state);
    install_swatch_input(&swatch, &state, &refresh_inputs);

    // Redraw the picker when the color is changed from outside the widget
    // (e.g. the canvas color-picker tool). Reload the working HSV first so
    // the wheel indicator tracks the picked color.
    {
        let colors = state.colors.clone();
        let state = state.clone();
        let refresh = Rc::clone(&refresh_inputs);
        colors.connect_changed(Box::new(move || {
            // Skip reloading when the picker itself is the source (it already
            // has the right HSV); only react to external changes.
            if state.syncing.get() {
                return;
            }
            state.load_hsv_from_current();
            refresh();
        }));
    }

    refresh_inputs();

    panel
}

fn build_inputs(
    state: &PickerState,
    wheel: &gtk::DrawingArea,
    swatch: &gtk::DrawingArea,
) -> (gtk::Box, Rc<dyn Fn()>) {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .build();

    let rgb_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .hexpand(true)
        .build();

    let r_spin = build_channel_row(&rgb_col, "R");
    let g_spin = build_channel_row(&rgb_col, "G");
    let b_spin = build_channel_row(&rgb_col, "B");

    row.append(&rgb_col);

    let hex_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    let hex_label = gtk::Label::builder()
        .label("Hex:")
        .halign(gtk::Align::Start)
        .build();
    let hex_entry = gtk::Entry::builder()
        .max_length(7)
        .width_chars(8)
        .placeholder_text("#000000")
        .build();
    hex_col.append(&hex_label);
    hex_col.append(&hex_entry);
    row.append(&hex_col);

    let refresh: Rc<dyn Fn()> = {
        let state = state.clone();
        let r_spin = r_spin.clone();
        let g_spin = g_spin.clone();
        let b_spin = b_spin.clone();
        let hex_entry = hex_entry.clone();
        let wheel = wheel.clone();
        let swatch = swatch.clone();
        Rc::new(move || {
            state.syncing.set(true);
            let c = state.colors.current();
            r_spin.set_value(f64::from(c.r));
            g_spin.set_value(f64::from(c.g));
            b_spin.set_value(f64::from(c.b));
            hex_entry.set_text(&c.to_hex());
            wheel.queue_draw();
            swatch.queue_draw();
            state.syncing.set(false);
        })
    };

    let on_rgb_changed: Rc<dyn Fn()> = {
        let state = state.clone();
        let r_spin = r_spin.clone();
        let g_spin = g_spin.clone();
        let b_spin = b_spin.clone();
        let refresh = Rc::clone(&refresh);
        Rc::new(move || {
            if state.syncing.get() {
                return;
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let c = Color::new(
                r_spin.value().round().clamp(0.0, 255.0) as u8,
                g_spin.value().round().clamp(0.0, 255.0) as u8,
                b_spin.value().round().clamp(0.0, 255.0) as u8,
            );
            state.commit_color(c);
            state.load_hsv_from_current();
            refresh();
        })
    };

    for spin in [&r_spin, &g_spin, &b_spin] {
        let cb = Rc::clone(&on_rgb_changed);
        spin.connect_value_changed(move |_| cb());
    }

    {
        let state = state.clone();
        let refresh = Rc::clone(&refresh);
        let hex_entry_cb = hex_entry.clone();
        hex_entry.connect_activate(move |_| {
            if state.syncing.get() {
                return;
            }
            if let Some(c) = Color::from_hex(&hex_entry_cb.text()) {
                state.commit_color(c);
                state.load_hsv_from_current();
            }
            refresh();
        });
    }

    {
        let state = state.clone();
        let refresh = Rc::clone(&refresh);
        let hex_entry_cb = hex_entry.clone();
        // Apply on focus loss as well as Enter.
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            if state.syncing.get() {
                return;
            }
            if let Some(c) = Color::from_hex(&hex_entry_cb.text()) {
                state.commit_color(c);
                state.load_hsv_from_current();
            }
            refresh();
        });
        hex_entry.add_controller(focus);
    }

    (row, refresh)
}

fn build_channel_row(parent: &gtk::Box, label: &str) -> gtk::SpinButton {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let lbl = gtk::Label::builder()
        .label(format!("{label}:"))
        .width_chars(2)
        .halign(gtk::Align::Start)
        .build();
    let adjustment = gtk::Adjustment::new(0.0, 0.0, 255.0, 1.0, 16.0, 0.0);
    let spin = gtk::SpinButton::builder()
        .adjustment(&adjustment)
        .climb_rate(1.0)
        .digits(0)
        .numeric(true)
        .hexpand(true)
        .build();
    row.append(&lbl);
    row.append(&spin);
    parent.append(&row);
    spin
}
