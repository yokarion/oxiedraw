//! Device identities and the input settings block keyed by them.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};

use super::bindings::PanMode;
use super::mouse::MouseProfile;
use super::pen::{PenProfile, TIP_BUTTON};
use super::touchscreen::TouchProfile;
use crate::settings::forgiving;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DeviceKind {
    Mouse,
    Pen,
    Touchscreen,
}

impl DeviceKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Mouse => "Mouse",
            Self::Pen => "Drawing Tablet",
            Self::Touchscreen => "Touchscreen",
        }
    }

    pub(crate) const fn icon_name(self) -> &'static str {
        match self {
            Self::Mouse => "input-mouse-symbolic",
            Self::Pen => "input-tablet-symbolic",
            Self::Touchscreen => "oxiedraw-touchscreen-symbolic",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum DeviceId {
    Mouse,
    Touchscreen,
    Tablet(String),
}

impl DeviceId {
    pub(crate) const fn kind(&self) -> DeviceKind {
        match self {
            Self::Mouse => DeviceKind::Mouse,
            Self::Touchscreen => DeviceKind::Touchscreen,
            Self::Tablet(_) => DeviceKind::Pen,
        }
    }

    pub(crate) fn title(&self) -> String {
        match self {
            Self::Tablet(name) if !name.is_empty() => format!("{} - {name}", self.kind().label()),
            other => other.kind().label().to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerAction {
    Draw,
    Erase,
    QuickMenu,
    Pan(PanMode),
}

impl PointerAction {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Draw => "Draw",
            Self::Erase => "Erase",
            Self::QuickMenu => "Quick menu",
            Self::Pan(PanMode::Hold) => "Pan (hold)",
            Self::Pan(PanMode::Toggle) => "Pan (toggle)",
        }
    }

    pub(crate) const fn paints(self) -> bool {
        matches!(self, Self::Draw | Self::Erase)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InputSettings {
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) mouse: MouseProfile,
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) touchscreen: TouchProfile,
    #[serde(default, deserialize_with = "tablet_entries")]
    pub(crate) tablets: BTreeMap<String, PenProfile>,
}

const LOGICAL_POINTER_PREFIX: &str = "Logical pointer for ";

pub(crate) fn tablet_name(device_name: &str) -> &str {
    device_name.strip_prefix(LOGICAL_POINTER_PREFIX).unwrap_or(device_name)
}

// Fix: profiles saved under the logical pointer's name fold into the tablet's own.
fn tablet_entries<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, PenProfile>, D::Error> {
    let (logical, mut tablets): (BTreeMap<String, PenProfile>, BTreeMap<String, PenProfile>) =
        forgiving_entries::<D, PenProfile>(deserializer)?
            .into_iter()
            .partition(|(name, _)| name.starts_with(LOGICAL_POINTER_PREFIX));
    for (name, pen) in logical {
        match tablets.entry(tablet_name(&name).to_owned()) {
            Entry::Vacant(slot) => {
                slot.insert(pen);
            }
            Entry::Occupied(mut slot) => slot.get_mut().absorb_detections(&pen),
        }
    }
    Ok(tablets)
}

fn forgiving_entries<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let raw = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|(name, value)| {
            let entry = serde_json::from_value(value).unwrap_or_else(|e| {
                tracing::warn!(%name, err = %e, "unreadable tablet settings, using defaults");
                T::default()
            });
            (name, entry)
        })
        .collect())
}

impl InputSettings {
    pub(crate) fn pen(&self, name: &str) -> PenProfile {
        self.tablets.get(name).cloned().unwrap_or_default()
    }

    pub(crate) fn pen_mut(&mut self, name: &str) -> &mut PenProfile {
        self.tablets.entry(name.to_owned()).or_default()
    }

    pub(crate) fn resolve_press(&self, device: &DeviceId, button: u32, eraser: bool) -> Option<PointerAction> {
        match device {
            DeviceId::Mouse => self.mouse.resolve(button),
            DeviceId::Tablet(name) => self.tablets.get(name).map_or_else(
                || PenProfile::default().resolve(button, eraser),
                |pen| pen.resolve(button, eraser),
            ),
            DeviceId::Touchscreen => self.touchscreen.plan().draw.then_some(PointerAction::Draw),
        }
    }

    fn pen_profiles(&self) -> impl Iterator<Item = PenProfile> + '_ {
        self.tablets.values().cloned().chain(std::iter::once(PenProfile::default()))
    }

    pub(crate) fn draw_button_filter(&self) -> u32 {
        let mut buttons: BTreeSet<u32> = self.mouse.draw_button().into_iter().collect();
        if self.pen_profiles().any(|p| p.uses_tip()) || self.touchscreen.plan().draw {
            buttons.insert(TIP_BUTTON);
        }
        single_or_any(&buttons)
    }

    pub(crate) fn hold_pan_button_filter(&self) -> u32 {
        let buttons: BTreeSet<u32> = self
            .mouse
            .hold_pan_button()
            .into_iter()
            .chain(self.pen_profiles().filter_map(|p| p.hold_pan_button()))
            .collect();
        single_or_any(&buttons)
    }
}

fn single_or_any(buttons: &BTreeSet<u32>) -> u32 {
    match buttons.iter().collect::<Vec<_>>().as_slice() {
        [only] => **only,
        _ => 0,
    }
}


#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::input::bindings::check;
    use crate::input::mouse::{MouseAction, MouseButton};
    use crate::input::pen::{PenAction, PenInput};
    use crate::input::touchscreen::{TouchAction, TouchGesture};

    const ALL_BUTTONS: [u32; 6] = [1, 2, 3, 8, 9, 42];

    #[test]
    fn defaults_filter_exactly_the_old_buttons() {
        let settings = InputSettings::default();
        assert_eq!(settings.draw_button_filter(), 1);
        assert_eq!(settings.hold_pan_button_filter(), 2);
    }

    #[test]
    fn filters_never_hide_a_press_a_device_needs() {
        let tablet = DeviceId::Tablet("Wacom".into());
        let devices = [DeviceId::Mouse, tablet.clone(), DeviceId::Tablet("Unknown".into()), DeviceId::Touchscreen];
        for mouse in check::every_state::<MouseAction>() {
            for pen in check::every_state::<PenAction>() {
                for pan_mode in [PanMode::Hold, PanMode::Toggle] {
                    let mut settings = InputSettings::default();
                    settings.mouse.bindings = mouse.clone();
                    settings.mouse.pan_mode = pan_mode;
                    let profile = settings.pen_mut("Wacom");
                    profile.bindings = pen.clone();
                    profile.pan_mode = pan_mode;
                    let draw = settings.draw_button_filter();
                    let pan = settings.hold_pan_button_filter();
                    for device in &devices {
                        for button in ALL_BUTTONS {
                            for eraser in [false, true] {
                                let action = settings.resolve_press(device, button, eraser);
                                if action.is_some_and(PointerAction::paints) {
                                    assert!(draw == 0 || draw == button, "{device:?} {button} hidden by {draw}");
                                }
                                if action == Some(PointerAction::Pan(PanMode::Hold)) {
                                    assert!(pan == 0 || pan == button, "{device:?} {button} hidden by {pan}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn devices_resolve_with_their_own_profile() {
        let mut settings = InputSettings::default();
        settings.mouse.bindings.assign(MouseAction::Draw, Some(MouseButton::Right));
        settings.pen_mut("Surface Pen").note_eraser();
        settings.pen_mut("Intuos").assign(PenAction::Draw, None);
        settings.touchscreen.bindings.assign(TouchAction::Draw, Some(TouchGesture::OneFingerDrag));

        let surface = DeviceId::Tablet("Surface Pen".into());
        let intuos = DeviceId::Tablet("Intuos".into());
        let unknown = DeviceId::Tablet("Huion".into());
        assert_eq!(settings.resolve_press(&DeviceId::Mouse, 3, false), Some(PointerAction::Draw));
        assert_eq!(settings.resolve_press(&DeviceId::Mouse, 1, false), Some(PointerAction::QuickMenu));
        assert_eq!(settings.resolve_press(&surface, 1, true), Some(PointerAction::Erase));
        assert_eq!(settings.resolve_press(&surface, 1, false), Some(PointerAction::Draw));
        assert_eq!(settings.resolve_press(&intuos, 1, false), None);
        assert_eq!(settings.resolve_press(&unknown, 1, false), Some(PointerAction::Draw));
        assert_eq!(settings.resolve_press(&unknown, 1, true), None);
        assert_eq!(settings.resolve_press(&DeviceId::Touchscreen, 1, false), Some(PointerAction::Draw));
        assert_eq!(settings.draw_button_filter(), 0);
    }

    #[test]
    fn one_broken_tablet_keeps_the_rest() {
        let json = r#"{
            "mouse": { "pan_mode": "toggle" },
            "tablets": {
                "Good": { "bindings": { "pan": "third-button" } },
                "Bad": { "bindings": 7 }
            }
        }"#;
        let settings: InputSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.mouse.pan_mode, PanMode::Toggle);
        assert_eq!(settings.pen("Good").bindings.get(PenAction::Pan), Some(PenInput::ThirdButton));
        assert_eq!(settings.pen("Good").bindings.get(PenAction::QuickMenu), Some(PenInput::LowerButton));
        assert_eq!(settings.pen("Bad"), PenProfile::default());
    }

    #[test]
    fn logical_pointer_names_resolve_to_the_tablet() {
        assert_eq!(tablet_name("Logical pointer for Wacom Intuos S Pen"), "Wacom Intuos S Pen");
        assert_eq!(tablet_name("Wacom Intuos S Pen"), "Wacom Intuos S Pen");
        assert_eq!(tablet_name("Logical pointer for "), "");
    }

    #[test]
    fn logical_pointer_profiles_fold_into_the_tablet() {
        let json = r#"{ "tablets": {
            "Wacom": { "bindings": { "draw": null }, "erase_customized": true },
            "Logical pointer for Wacom": { "eraser_detected": true, "detected_buttons": ["upper-button"] },
            "Logical pointer for Huion": { "pan_mode": "toggle" }
        } }"#;
        let settings: InputSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.tablets.keys().collect::<Vec<_>>(), ["Huion", "Wacom"]);
        let wacom = settings.pen("Wacom");
        assert_eq!(wacom.bindings.get(PenAction::Draw), None);
        assert!(wacom.eraser_detected);
        assert_eq!(wacom.bindings.get(PenAction::Erase), None);
        assert!(wacom.detected_buttons.contains(&PenInput::UpperButton));
        assert_eq!(settings.pen("Huion").pan_mode, PanMode::Toggle);
    }

    #[test]
    fn device_titles() {
        assert_eq!(DeviceId::Mouse.title(), "Mouse");
        assert_eq!(DeviceId::Touchscreen.title(), "Touchscreen");
        assert_eq!(DeviceId::Tablet("XP-Pen".into()).title(), "Drawing Tablet - XP-Pen");
        assert_eq!(DeviceId::Tablet(String::new()).title(), "Drawing Tablet");
    }
}
