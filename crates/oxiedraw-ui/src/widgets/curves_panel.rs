//! Curves controls shared by the filter popup and the adjustment editor.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_core::curves::{Curve, CurveChannel, CurveSet, Histogram};
use oxiedraw_core::enum_meta::EnumMeta;
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};

use super::curve_editor::{CurveEditor, Ramp, Rgb};

const BLACK: Rgb = (0.0, 0.0, 0.0);
const SELECTOR_CLASS: &str = "oxiedraw-curve-channels";
const DOT_CLASS: &str = "oxiedraw-curve-channel-dot";

/// `histogram` runs once the panel is first shown, so an unopened page never reads back.
pub(crate) fn build(
    initial: CurveSet,
    histogram: impl FnOnce() -> Option<Histogram> + 'static,
    on_change: impl Fn(CurveSet) + 'static,
) -> gtk::Box {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();

    let editor = CurveEditor::new();
    let curves = Rc::new(Cell::new(initial));
    let channel = Rc::new(Cell::new(CurveChannel::Rgb));
    let levels: Rc<RefCell<Option<Histogram>>> = Rc::new(RefCell::new(None));

    let show_histogram = {
        let editor = editor.clone();
        let channel = Rc::clone(&channel);
        let levels = Rc::clone(&levels);
        move || {
            let levels = levels.borrow();
            editor.set_histogram(levels.as_ref().map(|h| &h.channel(channel.get())[..]));
        }
    };

    let show_channel = {
        let editor = editor.clone();
        let curves = Rc::clone(&curves);
        let channel = Rc::clone(&channel);
        let show_histogram = show_histogram.clone();
        move || {
            let (set, ch) = (curves.get(), channel.get());
            let ramp = Ramp {
                low: BLACK,
                high: ramp_color(ch),
            };
            editor.set_ramps(Some(ramp), Some(ramp));
            editor.set_color(curve_color(ch));
            editor.set_overlays(overlays(&set, ch));
            editor.set_curve(*set.curve(ch));
            show_histogram();
        }
    };
    show_channel();

    let selector = ChannelSelector::new({
        let channel = Rc::clone(&channel);
        move |picked| {
            channel.set(picked);
            show_channel();
        }
    });
    mark_edited(&selector.dots, &initial);

    {
        // Only the dots: the whole selector would form a reference cycle.
        let dots = selector.dots.clone();
        editor.connect_changed(move |curve| {
            let mut set = curves.get();
            *set.curve_mut(channel.get()) = curve;
            curves.set(set);
            mark_edited(&dots, &set);
            on_change(set);
        });
    }

    root.append(&selector.widget);
    root.append(&editor.widget);

    let pending = RefCell::new(Some(histogram));
    root.connect_map(move |_| {
        let Some(read) = pending.borrow_mut().take() else {
            return;
        };
        let levels = Rc::clone(&levels);
        let show_histogram = show_histogram.clone();
        glib::idle_add_local_once(move || {
            *levels.borrow_mut() = read();
            show_histogram();
        });
    });

    root
}

struct ChannelSelector {
    widget: gtk::Box,
    dots: Vec<gtk::Box>,
}

impl ChannelSelector {
    fn new(on_pick: impl Fn(CurveChannel) + 'static) -> Self {
        load_css();
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .hexpand(true)
            .css_classes(["linked", SELECTOR_CLASS])
            .build();
        let on_pick = Rc::new(on_pick);
        let mut first: Option<gtk::ToggleButton> = None;
        let mut dots = Vec::with_capacity(CurveChannel::ALL.len());
        for &channel in CurveChannel::ALL {
            // Hidden by opacity to keep the height; the center box keeps the title centered.
            let dot = gtk::Box::builder()
                .halign(gtk::Align::Center)
                .margin_top(3)
                .css_classes([DOT_CLASS])
                .opacity(0.0)
                .build();
            let content = gtk::CenterBox::builder()
                .orientation(gtk::Orientation::Vertical)
                .center_widget(&gtk::Label::new(Some(channel.label())))
                .end_widget(&dot)
                .build();

            let button = gtk::ToggleButton::builder()
                .child(&content)
                .tooltip_text(channel_name(channel))
                .active(channel == CurveChannel::Rgb)
                .build();
            match &first {
                Some(leader) => button.set_group(Some(leader)),
                None => first = Some(button.clone()),
            }
            let on_pick = Rc::clone(&on_pick);
            button.connect_toggled(move |b| {
                if b.is_active() {
                    on_pick(channel);
                }
            });
            widget.append(&button);
            dots.push(dot);
        }
        Self { widget, dots }
    }
}

fn mark_edited(dots: &[gtk::Box], set: &CurveSet) {
    for (&channel, dot) in CurveChannel::ALL.iter().zip(dots) {
        let edited = !set.curve(channel).is_identity();
        dot.set_opacity(if edited { 1.0 } else { 0.0 });
    }
}

const fn channel_name(channel: CurveChannel) -> &'static str {
    match channel {
        CurveChannel::Rgb => "All channels",
        CurveChannel::Red => "Red",
        CurveChannel::Green => "Green",
        CurveChannel::Blue => "Blue",
    }
}

fn load_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".oxiedraw-curve-channels > button:checked,
            .oxiedraw-curve-channels > button:checked:hover {
                background-color: @accent_bg_color;
                color: @accent_fg_color;
            }

            .oxiedraw-curve-channels > button:checked:active {
                background-color: color-mix(in srgb, @accent_bg_color 85%, black);
            }

            .oxiedraw-curve-channel-dot {
                min-width: 4px;
                min-height: 4px;
                border-radius: 2px;
                background-color: currentColor;
            }",
        );
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

fn overlays(set: &CurveSet, shown: CurveChannel) -> Vec<(Curve, Rgb)> {
    if shown != CurveChannel::Rgb {
        return Vec::new();
    }
    [CurveChannel::Red, CurveChannel::Green, CurveChannel::Blue]
        .into_iter()
        .map(|ch| (*set.curve(ch), curve_color(ch)))
        .filter(|(curve, _)| !curve.is_identity())
        .collect()
}

const fn curve_color(channel: CurveChannel) -> Rgb {
    match channel {
        CurveChannel::Rgb => (0.92, 0.92, 0.92),
        CurveChannel::Red => (0.95, 0.36, 0.36),
        CurveChannel::Green => (0.36, 0.85, 0.42),
        CurveChannel::Blue => (0.42, 0.6, 1.0),
    }
}

const fn ramp_color(channel: CurveChannel) -> Rgb {
    match channel {
        CurveChannel::Rgb => (1.0, 1.0, 1.0),
        CurveChannel::Red => (1.0, 0.0, 0.0),
        CurveChannel::Green => (0.0, 1.0, 0.0),
        CurveChannel::Blue => (0.0, 0.0, 1.0),
    }
}
