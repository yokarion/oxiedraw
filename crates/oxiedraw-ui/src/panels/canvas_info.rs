// Drawn, never labelled: a label queues a resize, which re-allocates the canvas
// Picture and cancels the stylus grab mid-drag.

use std::cell::Cell;
use std::f64::consts::FRAC_PI_2;
use std::rc::Rc;

use oxiedraw_utils::geometry::Size;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::prelude::*;

const DIAL_BOX: i32 = 18;
const TEXT_W: i32 = 76;

const CHIP_W: i32 = 104;
const CHIP_H: i32 = 22;

#[derive(Clone)]
pub(crate) struct CanvasInfoBar {
    root: gtk::Box,
    size_label: gtk::Label,
    rotator: gtk::DrawingArea,
    angle: Rc<Cell<f32>>,
    lock_chip: gtk::DrawingArea,
    alpha_locked: Rc<Cell<bool>>,
}

impl CanvasInfoBar {
    pub(crate) fn new(on_rotate: Rc<dyn Fn(f32)>) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["oxiedraw-chrome"])
            .build();
        root.set_margin_start(8);
        root.set_margin_end(8);
        root.set_margin_top(1);
        root.set_margin_bottom(1);

        let size_label = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .build();

        let spacer = gtk::Box::builder().hexpand(true).build();

        let angle = Rc::new(Cell::new(0.0_f32));
        let rotator = gtk::DrawingArea::builder()
            .content_width(DIAL_BOX + TEXT_W)
            .content_height(DIAL_BOX)
            .tooltip_text("Drag to rotate the canvas; double-click to reset")
            .build();
        rotator.set_cursor_from_name(Some("grab"));
        {
            let angle = Rc::clone(&angle);
            rotator.set_draw_func(move |area, cr, w, h| draw_rotator(area, cr, w, h, angle.get()));
        }

        install_dial_gestures(&rotator, &on_rotate);

        let alpha_locked = Rc::new(Cell::new(false));
        let lock_chip = gtk::DrawingArea::builder()
            .content_width(CHIP_W)
            .content_height(CHIP_H)
            .valign(gtk::Align::Center)
            .build();
        {
            let alpha_locked = Rc::clone(&alpha_locked);
            lock_chip.set_draw_func(move |area, cr, w, h| {
                if alpha_locked.get() {
                    draw_lock_chip(area, cr, w, h);
                }
            });
        }

        root.append(&size_label);
        root.append(&spacer);
        root.append(&lock_chip);
        root.append(&rotator);

        Self {
            root,
            size_label,
            rotator,
            angle,
            lock_chip,
            alpha_locked,
        }
    }

    pub(crate) fn set_alpha_locked(&self, locked: bool) {
        if self.alpha_locked.get() == locked {
            return;
        }
        self.alpha_locked.set(locked);
        self.lock_chip.queue_draw();
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    // Weak captures only: the observer lives in the viewport and the dial's
    // gesture already holds it, so a strong one leaks the whole document.
    pub(crate) fn observer(&self) -> Box<dyn Fn(Size, f32)> {
        let size_label = self.size_label.downgrade();
        let rotator = self.rotator.downgrade();
        let angle = Rc::clone(&self.angle);
        Box::new(move |size, rotation| {
            angle.set(rotation);
            if let Some(rotator) = rotator.upgrade() {
                rotator.queue_draw();
            }
            if let Some(label) = size_label.upgrade() {
                let text = format!("{} x {} px", size.width, size.height);
                if label.text().as_str() != text {
                    label.set_text(&text);
                }
            }
        })
    }
}

fn install_dial_gestures(rotator: &gtk::DrawingArea, on_rotate: &Rc<dyn Fn(f32)>) {
    let drag = gtk::GestureDrag::new();
    let start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    {
        let start = Rc::clone(&start);
        drag.connect_drag_begin(move |_, x, y| start.set((x, y)));
    }
    {
        let start = Rc::clone(&start);
        let on_rotate = Rc::clone(on_rotate);
        drag.connect_drag_update(move |gesture, dx, dy| {
            if dx.hypot(dy) < 4.0 {
                return;
            }
            let (sx, sy) = start.get();
            let h = f64::from(gesture.widget().map_or(DIAL_BOX, |w| w.height()));
            let c = h / 2.0;
            let px = sx + dx - c;
            let py = sy + dy - c;
            if px == 0.0 && py == 0.0 {
                return;
            }
            #[allow(clippy::cast_possible_truncation)]
            let theta = (py.atan2(px) + FRAC_PI_2) as f32;
            on_rotate(theta);
        });
    }
    rotator.add_controller(drag);

    let reset = gtk::GestureClick::new();
    reset.set_button(gdk::BUTTON_SECONDARY);
    {
        let on_rotate = Rc::clone(on_rotate);
        reset.connect_pressed(move |_, _, _, _| on_rotate(0.0));
    }
    rotator.add_controller(reset);

    let dbl = gtk::GestureClick::new();
    {
        let on_rotate = Rc::clone(on_rotate);
        dbl.connect_pressed(move |_, n_press, _, _| {
            if n_press >= 2 {
                on_rotate(0.0);
            }
        });
    }
    rotator.add_controller(dbl);
}

fn draw_rotator(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, _w: i32, h: i32, rotation: f32) {
    let cx = f64::from(h) / 2.0;
    let cy = f64::from(h) / 2.0;
    let r = cy - 1.5;
    if r <= 0.0 {
        return;
    }
    let fg = area.color();
    let (fr, fg_, fb) = (f64::from(fg.red()), f64::from(fg.green()), f64::from(fg.blue()));

    cr.set_source_rgba(fr, fg_, fb, 0.35);
    cr.set_line_width(1.0);
    cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
    cr.stroke().ok();

    let theta = f64::from(rotation);
    let (s, c) = theta.sin_cos();
    cr.set_source_rgba(fr, fg_, fb, 0.9);
    cr.set_line_width(1.6);
    cr.move_to(cx, cy);
    cr.line_to(cx + r * s, cy - r * c);
    cr.stroke().ok();
    cr.arc(cx, cy, 1.3, 0.0, std::f64::consts::TAU);
    cr.fill().ok();

    let deg = normalize_deg(theta.to_degrees());
    let text = format!("{deg:.2} deg");
    cr.set_font_size(11.0);
    cr.set_source_rgba(fr, fg_, fb, 0.75);
    let ty = cy + cr.text_extents(&text).map_or(4.0, |e| e.height() / 2.0);
    cr.move_to(f64::from(h) + 4.0, ty);
    cr.show_text(&text).ok();
}

fn draw_lock_chip(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: i32, h: i32) {
    let (w, h) = (f64::from(w), f64::from(h));
    let ground = crate::theme::warning_ground(area);
    let (ar, ag, ab) = crate::theme::warning_accent(area);

    let r = h / 2.0;
    cr.new_sub_path();
    cr.arc(w - r, r, r, -std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2);
    cr.arc(r, r, r, std::f64::consts::FRAC_PI_2, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();
    cr.set_source_rgba(ground.0, ground.1, ground.2, crate::theme::WARNING_WASH_ALPHA);
    cr.fill().ok();

    let cx = 12.0;
    let cy = h / 2.0;
    cr.set_source_rgb(ar, ag, ab);
    cr.set_line_width(1.3);
    cr.new_path();
    cr.arc(cx, cy - 1.6, 2.6, std::f64::consts::PI, std::f64::consts::TAU);
    cr.stroke().ok();
    cr.rectangle(cx - 3.8, cy - 0.8, 7.6, 5.4);
    cr.fill().ok();

    cr.set_font_size(11.0);
    cr.set_source_rgb(ar, ag, ab);
    let text = "Alpha locked";
    let ty = cy + cr.text_extents(text).map_or(4.0, |e| e.height() / 2.0);
    cr.move_to(cx + 9.0, ty);
    cr.show_text(text).ok();
}

fn normalize_deg(deg: f64) -> f64 {
    #[allow(clippy::cast_possible_truncation)]
    let deg = f64::from(oxiedraw_utils::math::wrap_pi((deg as f32).to_radians()).to_degrees());
    if deg.abs() < 0.005 { 0.0 } else { deg }
}
