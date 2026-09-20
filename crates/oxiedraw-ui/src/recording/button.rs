//! The top-bar record control: a split button whose face starts or stops
//! recording and whose arrow opens Start/Stop, Export and Recording Settings.
//! Idle it shows a camera with no background; while recording, a red stop
//! square on a red-outlined, red-tinted button.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use relm4::gtk::{gdk, gio};

const TOGGLE: &str = "app.recording-toggle";
const CLASS: &str = "oxiedraw-record";
const RECORDING_CLASS: &str = "recording";
const IDLE_ICON: &str = "oxiedraw-record-symbolic";
const RECORDING_ICON: &str = "oxiedraw-record-stop-symbolic";

// No background until recording; a divider between the halves, each lighting
// up on its own hover, instead of Adwaita's two separately shaded halves.
const CSS: &str = "
splitbutton.oxiedraw-record > button,
splitbutton.oxiedraw-record > menubutton > button {
    background-color: transparent;
    box-shadow: none;
}
splitbutton.oxiedraw-record > button:hover,
splitbutton.oxiedraw-record > menubutton > button:hover {
    background-color: alpha(currentColor, 0.08);
}
splitbutton.oxiedraw-record > button:active,
splitbutton.oxiedraw-record > menubutton > button:active,
splitbutton.oxiedraw-record > menubutton > button:checked {
    background-color: alpha(currentColor, 0.16);
}
splitbutton.oxiedraw-record > separator {
    min-width: 1px;
    background-color: alpha(currentColor, 0.25);
}
splitbutton.oxiedraw-record.recording {
    background-color: alpha(@destructive_bg_color, 0.3);
    outline: 1px solid @destructive_bg_color;
    outline-offset: -1px;
}
splitbutton.oxiedraw-record.recording > button {
    color: @destructive_color;
}
splitbutton.oxiedraw-record.recording > separator {
    background-color: alpha(@destructive_color, 0.35);
}
";

#[derive(Clone)]
pub(crate) struct RecordButton {
    button: adw::SplitButton,
    recording: Rc<Cell<bool>>,
    menu: gio::Menu,
}

impl RecordButton {
    pub(crate) fn new() -> Self {
        load_css();
        let menu = gio::Menu::new();
        menu.append(Some("Start Recording"), Some(TOGGLE));
        menu.append(Some("Export..."), Some("app.recording-export"));
        menu.append(Some("Recording Settings..."), Some("app.recording-settings"));

        let button = adw::SplitButton::builder()
            .icon_name(IDLE_ICON)
            .menu_model(&menu)
            .action_name(TOGGLE)
            .tooltip_text("Start recording")
            .dropdown_tooltip("Recording options")
            .valign(gtk::Align::Center)
            .margin_end(6)
            .build();
        button.add_css_class(CLASS);
        crate::top_bar::style_control(&button);

        Self { button, recording: Rc::new(Cell::new(false)), menu }
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.button.clone().upcast()
    }

    pub(crate) fn set_recording(&self, on: bool) {
        if self.recording.replace(on) == on {
            return;
        }
        let (icon, label, tooltip) = if on {
            self.button.add_css_class(RECORDING_CLASS);
            (RECORDING_ICON, "Stop Recording", "Stop recording")
        } else {
            self.button.remove_css_class(RECORDING_CLASS);
            (IDLE_ICON, "Start Recording", "Start recording")
        };
        self.button.set_icon_name(icon);
        self.menu.remove(0);
        self.menu.insert(0, Some(label), Some(TOGGLE));
        self.button.set_tooltip_text(Some(tooltip));
    }
}

fn load_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}
