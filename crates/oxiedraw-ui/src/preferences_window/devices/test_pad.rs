//! A square that lists the pen presses on it and the binding each one triggers.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{cairo, gdk, glib, pango};

use crate::input::{self, DeviceId, PenInput, PointerAction, Trigger};

pub(super) const SIDE: i32 = 290;
const MARGIN: f64 = 10.0;
const MAX_LINES: usize = 64;
const PLACEHOLDER: &str = "Draw here with the pen.\nPresses and buttons show up here.";
const FALLBACK_FONT_PX: f64 = 11.0;

pub(super) struct PadHooks {
    pub(super) on_pressure: Box<dyn Fn(Option<f32>)>,
    pub(super) on_news: Box<dyn Fn()>,
}

pub(super) fn build(tablet: &str, hooks: PadHooks) -> gtk::Widget {
    let lines: Rc<RefCell<VecDeque<String>>> = Rc::default();
    let pad = gtk::DrawingArea::builder()
        .content_width(SIDE)
        .content_height(SIDE)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["card", "monospace", "caption"])
        .build();
    {
        let lines = Rc::clone(&lines);
        pad.set_draw_func(move |area, cr, _, height| paint(area, cr, height, &lines.borrow()));
    }

    let tablet = DeviceId::Tablet(tablet.to_owned());
    let screen = pad.clone();
    let legacy = gtk::EventControllerLegacy::new();
    legacy.connect_event(move |_, event| {
        let line = match input::event_device(event) {
            Some(device @ DeviceId::Tablet(_)) => {
                if input::note_pen_event(event) {
                    (hooks.on_news)();
                }
                let Some(line) = describe(event, &hooks) else {
                    return glib::Propagation::Stop;
                };
                if device == tablet { line } else { format!("[{}] {line}", device.title()) }
            }
            Some(DeviceId::Mouse) => {
                let Some(line) = describe_mouse(event) else {
                    return glib::Propagation::Proceed;
                };
                format!("[Mouse] {line}")
            }
            _ => return glib::Propagation::Proceed,
        };
        let mut lines = lines.borrow_mut();
        lines.push_back(line);
        while lines.len() > MAX_LINES {
            lines.pop_front();
        }
        screen.queue_draw();
        glib::Propagation::Stop
    });
    pad.add_controller(legacy);
    pad.upcast()
}

fn paint(area: &gtk::DrawingArea, cr: &cairo::Context, height: i32, lines: &VecDeque<String>) {
    let font = area.pango_context().font_description();
    let family = font
        .as_ref()
        .and_then(pango::FontDescription::family)
        .map_or_else(|| "monospace".to_owned(), |family| family.to_string());
    cr.select_font_face(&family, cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(font_px(font.as_ref()) / 2.0);

    let color = area.color();
    let alpha = if lines.is_empty() { 0.55 } else { 1.0 };
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()) * alpha,
    );
    let (line_height, descent) = cr.font_extents().map_or((FALLBACK_FONT_PX / 2.0, 0.0), |e| (e.height(), e.descent()));
    let mut baseline = f64::from(height) - MARGIN - descent;
    let shown: Vec<&str> =
        if lines.is_empty() { PLACEHOLDER.lines().collect() } else { lines.iter().map(String::as_str).collect() };
    for line in shown.into_iter().rev() {
        if baseline < 0.0 {
            break;
        }
        cr.move_to(MARGIN, baseline);
        cr.show_text(line).ok();
        baseline -= line_height;
    }
}

fn font_px(font: Option<&pango::FontDescription>) -> f64 {
    let Some(font) = font.filter(|font| font.size() > 0) else {
        return FALLBACK_FONT_PX;
    };
    let size = f64::from(font.size()) / f64::from(pango::SCALE);
    if font.is_size_absolute() { size } else { size * 96.0 / 72.0 }
}

fn describe(event: &gdk::Event, hooks: &PadHooks) -> Option<String> {
    #[allow(clippy::cast_possible_truncation)]
    let pressure = event.axis(gdk::AxisUse::Pressure).map(|p| p as f32);
    let eraser = input::is_eraser(event);
    match event.event_type() {
        gdk::EventType::ProximityIn => Some(proximity_line(eraser)),
        gdk::EventType::ProximityOut => Some("Out of range".to_owned()),
        gdk::EventType::ButtonPress => {
            let button = event.downcast_ref::<gdk::ButtonEvent>()?.button();
            let pen_input = PenInput::from_press(button, eraser);
            let tip = pen_input.is_some_and(|i| !i.is_button());
            let raw = pressure.filter(|_| tip);
            if let Some(raw) = raw {
                (hooks.on_pressure)(Some(raw));
            }
            let used = raw.map(|raw| (raw, input::map_pressure(Some(event), raw)));
            Some(press_line(button, pen_input, input::resolve_event(event), used))
        }
        gdk::EventType::ButtonRelease => {
            let button = event.downcast_ref::<gdk::ButtonEvent>()?.button();
            let pen_input = PenInput::from_press(button, eraser);
            if pen_input.is_some_and(|i| !i.is_button()) {
                (hooks.on_pressure)(None);
            }
            Some(format!("{} up", input_name(button, pen_input)))
        }
        gdk::EventType::MotionNotify => {
            if event.modifier_state().contains(gdk::ModifierType::BUTTON1_MASK) {
                (hooks.on_pressure)(pressure);
            }
            None
        }
        _ => None,
    }
}

fn describe_mouse(event: &gdk::Event) -> Option<String> {
    let button = event.downcast_ref::<gdk::ButtonEvent>()?.button();
    let name = input::mouse_button_label(button).map_or_else(|| format!("Button {button}"), str::to_owned);
    match event.event_type() {
        gdk::EventType::ButtonPress => {
            let does = input::resolve_event(event).map_or("unbound", PointerAction::label);
            Some(format!("{name} down -> {does}"))
        }
        gdk::EventType::ButtonRelease => Some(format!("{name} up")),
        _ => None,
    }
}

fn proximity_line(eraser: bool) -> String {
    format!("In range: {}", if eraser { "eraser end" } else { "pen tip" })
}

fn input_name(button: u32, pen_input: Option<PenInput>) -> String {
    match pen_input {
        Some(i) if i.is_button() => format!("{} ({button})", i.label()),
        Some(i) => i.label().to_owned(),
        None => format!("Button {button}"),
    }
}

fn press_line(
    button: u32,
    pen_input: Option<PenInput>,
    action: Option<PointerAction>,
    pressure: Option<(f32, f32)>,
) -> String {
    let does = match (pen_input, action) {
        (None, _) => "not supported",
        (Some(_), None) => "unbound",
        (Some(_), Some(action)) => action.label(),
    };
    let line = format!("{} down -> {does}", input_name(button, pen_input));
    match pressure {
        Some((raw, used)) => format!("{line}, pressure {} -> {}", percent(raw), percent(used)),
        None => line,
    }
}

fn percent(value: f32) -> String {
    format!("{:.0}%", value.clamp(0.0, 1.0) * 100.0)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::PanMode;

    #[test]
    fn press_lines_name_the_input_and_what_it_does() {
        assert_eq!(
            press_line(1, Some(PenInput::Tip), Some(PointerAction::Draw), Some((0.42, 0.31))),
            "Pen press down -> Draw, pressure 42% -> 31%"
        );
        assert_eq!(press_line(1, Some(PenInput::EraserTip), None, Some((1.0, 1.0))), "Eraser tip down -> unbound, pressure 100% -> 100%");
        assert_eq!(
            press_line(3, Some(PenInput::LowerButton), Some(PointerAction::QuickMenu), None),
            "Lower barrel button (3) down -> Quick menu"
        );
        assert_eq!(
            press_line(2, Some(PenInput::UpperButton), Some(PointerAction::Pan(PanMode::Toggle)), None),
            "Upper barrel button (2) down -> Pan (toggle)"
        );
        assert_eq!(press_line(9, None, None, None), "Button 9 down -> not supported");
    }

    #[test]
    fn font_sizes_convert_points_to_pixels() {
        let mut font = pango::FontDescription::new();
        assert!((font_px(None) - FALLBACK_FONT_PX).abs() < 1e-9);
        assert!((font_px(Some(&font)) - FALLBACK_FONT_PX).abs() < 1e-9);
        font.set_size(9 * pango::SCALE);
        assert!((font_px(Some(&font)) - 12.0).abs() < 1e-9);
        font.set_absolute_size(f64::from(14 * pango::SCALE));
        assert!((font_px(Some(&font)) - 14.0).abs() < 1e-9);
    }

    #[test]
    fn other_lines() {
        assert_eq!(proximity_line(true), "In range: eraser end");
        assert_eq!(proximity_line(false), "In range: pen tip");
        assert_eq!(input_name(8, Some(PenInput::ThirdButton)), "Third barrel button (8)");
        assert_eq!(percent(1.7), "100%");
        assert_eq!(percent(-0.2), "0%");
    }
}
