//! Per-device input bindings and the live copy the canvas reads.

mod bindings;
mod devices;
mod mouse;
mod pen;
mod touchscreen;

use std::cell::RefCell;
use std::rc::Rc;

use oxiedraw_core::curves::{LUT_SIZE, sample_baked};
use relm4::gtk::gdk;
use relm4::gtk::prelude::*;

pub(crate) use bindings::{Bindings, Displaced, InputAction, PanMode, Trigger};
pub(crate) use devices::{DeviceId, InputSettings, PointerAction};
pub(crate) use mouse::MouseAction;
pub(crate) use pen::{PenAction, PenInput};
pub(crate) use touchscreen::{Navigation, TouchAction, TouchPlan};

use crate::settings::AppSettings;

struct Live {
    settings: InputSettings,
    generation: u64,
    unsaved: bool,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
    static PRESSURE: RefCell<PressureCache> = RefCell::new(PressureCache::default());
}

fn with_live<R>(f: impl FnOnce(&mut Live) -> R) -> R {
    LIVE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let live = slot.get_or_insert_with(|| Live {
            settings: AppSettings::load().input,
            generation: 0,
            unsaved: false,
        });
        f(live)
    })
}

pub(crate) fn read<R>(f: impl FnOnce(&InputSettings) -> R) -> R {
    with_live(|live| f(&live.settings))
}

fn generation() -> u64 {
    with_live(|live| live.generation)
}

pub(crate) fn update<R>(f: impl FnOnce(&mut InputSettings) -> R) -> R {
    let result = update_unsaved(f);
    save_pending();
    result
}

pub(crate) fn update_unsaved<R>(f: impl FnOnce(&mut InputSettings) -> R) -> R {
    with_live(|live| {
        let before = live.settings.clone();
        let result = f(&mut live.settings);
        if live.settings != before {
            live.generation += 1;
            live.unsaved = true;
        }
        result
    })
}

pub(crate) fn save_pending() {
    let pending = with_live(|live| std::mem::take(&mut live.unsaved).then(|| live.settings.clone()));
    if let Some(settings) = pending {
        let mut app = AppSettings::load();
        app.input = settings;
        app.save();
    }
}

pub(crate) fn device_id(device: &gdk::Device) -> Option<DeviceId> {
    match device.source() {
        gdk::InputSource::Pen => Some(tablet_id(device)),
        gdk::InputSource::Touchscreen => Some(DeviceId::Touchscreen),
        gdk::InputSource::Keyboard | gdk::InputSource::TabletPad => None,
        _ => Some(DeviceId::Mouse),
    }
}

// Fix: a device tool on the event, not the device source, marks tablet input.
pub(crate) fn event_device(event: &gdk::Event) -> Option<DeviceId> {
    let device = event.device()?;
    let id = if event.device_tool().is_some() { Some(tablet_id(&device)) } else { device_id(&device) };
    tracing::trace!(name = %device.name(), source = ?device.source(), ?id, "input event device");
    id
}

// Fix: events carry the logical pointer, so both names map to one tablet.
fn tablet_id(device: &gdk::Device) -> DeviceId {
    DeviceId::Tablet(devices::tablet_name(&device.name()).to_owned())
}

pub(crate) fn mouse_button_label(button: u32) -> Option<&'static str> {
    mouse::MouseButton::from_number(button).map(Trigger::label)
}

pub(crate) fn is_eraser(event: &gdk::Event) -> bool {
    event
        .device_tool()
        .is_some_and(|tool| tool.tool_type() == gdk::DeviceToolType::Eraser)
}

fn press_button(event: &gdk::Event) -> Option<u32> {
    if let Some(button) = event.downcast_ref::<gdk::ButtonEvent>() {
        return Some(button.button());
    }
    matches!(
        event.event_type(),
        gdk::EventType::TouchBegin | gdk::EventType::TouchUpdate | gdk::EventType::TouchEnd
    )
    .then_some(pen::TIP_BUTTON)
}

pub(crate) fn resolve_event(event: &gdk::Event) -> Option<PointerAction> {
    let device = event_device(event).unwrap_or(DeviceId::Mouse);
    let button = press_button(event)?;
    read(|s| s.resolve_press(&device, button, is_eraser(event)))
}

const BUTTON_MASKS: [(u32, gdk::ModifierType); 5] = [
    (1, gdk::ModifierType::BUTTON1_MASK),
    (2, gdk::ModifierType::BUTTON2_MASK),
    (3, gdk::ModifierType::BUTTON3_MASK),
    (4, gdk::ModifierType::BUTTON4_MASK),
    (5, gdk::ModifierType::BUTTON5_MASK),
];

pub(crate) fn paint_button_held(event: Option<&gdk::Event>, state: gdk::ModifierType) -> bool {
    let device = event.and_then(event_device).unwrap_or(DeviceId::Mouse);
    let eraser = event.is_some_and(is_eraser);
    read(|s| held_buttons_paint(s, &device, eraser, state))
}

fn held_buttons_paint(settings: &InputSettings, device: &DeviceId, eraser: bool, state: gdk::ModifierType) -> bool {
    BUTTON_MASKS.iter().any(|&(button, mask)| {
        state.contains(mask) && settings.resolve_press(device, button, eraser).is_some_and(PointerAction::paints)
    })
}

pub(crate) fn eraser_erases(event: Option<&gdk::Event>) -> bool {
    let Some(device) = event.filter(|e| is_eraser(e)).and_then(event_device) else {
        return false;
    };
    read(|s| s.resolve_press(&device, pen::TIP_BUTTON, true) == Some(PointerAction::Erase))
}

pub(crate) fn note_pen_event(event: &gdk::Event) -> bool {
    let Some(DeviceId::Tablet(name)) = event_device(event) else {
        return false;
    };
    let eraser = is_eraser(event);
    let button = event.downcast_ref::<gdk::ButtonEvent>().map(gdk::ButtonEvent::button);
    let news = read(|s| {
        let pen = s.tablets.get(&name);
        let eraser_news = eraser && !pen.is_some_and(|p| p.eraser_detected);
        let button_news = button.is_some_and(|b| pen.cloned().unwrap_or_default().note_button(b));
        eraser_news || button_news
    });
    if news {
        update(|s| {
            let pen = s.pen_mut(&name);
            if eraser {
                pen.note_eraser();
            }
            if let Some(b) = button {
                pen.note_button(b);
            }
        });
    }
    news
}

#[derive(Default)]
struct PressureCache {
    device: Option<gdk::Device>,
    generation: u64,
    lut: Option<Rc<[f32; LUT_SIZE]>>,
}

pub(crate) fn map_pressure(event: Option<&gdk::Event>, raw: f32) -> f32 {
    let Some(event) = event else {
        return raw;
    };
    let Some(device) = event.device() else {
        return raw;
    };
    if event.device_tool().is_none() && device.source() != gdk::InputSource::Pen {
        return raw;
    }
    let current = generation();
    let lut = PRESSURE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.device.as_ref() != Some(&device) || cache.generation != current {
            let name = device.name();
            let curve = read(|s| s.tablets.get(devices::tablet_name(&name)).map(|p| p.pressure));
            cache.lut = curve.filter(|c| !c.is_identity()).map(|c| Rc::new(c.bake()));
            cache.device = Some(device);
            cache.generation = current;
        }
        cache.lut.clone()
    });
    lut.map_or(raw, |lut| sample_baked(&lut, raw).clamp(0.0, 1.0))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_button_counts_only_when_it_paints_on_that_device() {
        use gdk::ModifierType as M;
        let mut settings = InputSettings::default();
        let pen = DeviceId::Tablet("Wacom".into());
        assert!(held_buttons_paint(&settings, &DeviceId::Mouse, false, M::BUTTON1_MASK));
        assert!(!held_buttons_paint(&settings, &DeviceId::Mouse, false, M::BUTTON3_MASK));
        assert!(held_buttons_paint(&settings, &pen, false, M::BUTTON1_MASK));
        assert!(!held_buttons_paint(&settings, &pen, true, M::BUTTON1_MASK), "eraser unbound by default");

        settings.mouse.bindings.assign(MouseAction::Draw, Some(mouse::MouseButton::Right));
        assert!(held_buttons_paint(&settings, &DeviceId::Mouse, false, M::BUTTON3_MASK));
        assert!(!held_buttons_paint(&settings, &DeviceId::Mouse, false, M::BUTTON1_MASK));
        assert!(held_buttons_paint(&settings, &pen, false, M::BUTTON1_MASK), "the pen keeps its own tip");

        settings.mouse.bindings.assign(MouseAction::Draw, Some(mouse::MouseButton::Back));
        assert!(!held_buttons_paint(&settings, &DeviceId::Mouse, false, M::all()), "no mask past button 5");
    }
}
