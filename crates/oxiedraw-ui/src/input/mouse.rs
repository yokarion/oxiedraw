//! Mouse buttons and what each one does on the canvas.

use serde::{Deserialize, Serialize};

use super::bindings::{Bindings, InputAction, PanMode, Trigger};
use super::devices::PointerAction;
use crate::settings::forgiving;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl MouseButton {
    pub(crate) const fn number(self) -> u32 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
            Self::Back => 8,
            Self::Forward => 9,
        }
    }

    pub(crate) fn from_number(number: u32) -> Option<Self> {
        ALL_BUTTONS.iter().flatten().copied().find(|b| b.number() == number)
    }
}

impl Trigger for MouseButton {
    fn label(self) -> &'static str {
        match self {
            Self::Left => "Left button",
            Self::Middle => "Middle button",
            Self::Right => "Right button",
            Self::Back => "Back button",
            Self::Forward => "Forward button",
        }
    }
}

const ALL_BUTTONS: &[Option<MouseButton>] = &[
    Some(MouseButton::Left),
    Some(MouseButton::Middle),
    Some(MouseButton::Right),
    Some(MouseButton::Back),
    Some(MouseButton::Forward),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MouseAction {
    Draw,
    QuickMenu,
    Pan,
}

impl InputAction for MouseAction {
    type Trigger = MouseButton;
    const ALL: &'static [Self] = &[Self::Draw, Self::QuickMenu, Self::Pan];

    fn label(self) -> &'static str {
        match self {
            Self::Draw => "Draw",
            Self::QuickMenu => "Quick menu",
            Self::Pan => "Pan",
        }
    }

    fn choices(self) -> &'static [Option<MouseButton>] {
        ALL_BUTTONS
    }

    fn default_binding(self) -> Option<MouseButton> {
        Some(match self {
            Self::Draw => MouseButton::Left,
            Self::QuickMenu => MouseButton::Right,
            Self::Pan => MouseButton::Middle,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MouseProfile {
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) bindings: Bindings<MouseAction>,
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) pan_mode: PanMode,
}

impl MouseProfile {
    pub(crate) fn resolve(&self, button: u32) -> Option<PointerAction> {
        let action = self.bindings.action_for(MouseButton::from_number(button)?)?;
        Some(match action {
            MouseAction::Draw => PointerAction::Draw,
            MouseAction::QuickMenu => PointerAction::QuickMenu,
            MouseAction::Pan => PointerAction::Pan(self.pan_mode),
        })
    }

    pub(crate) fn draw_button(&self) -> Option<u32> {
        self.bindings.get(MouseAction::Draw).map(MouseButton::number)
    }

    pub(crate) fn hold_pan_button(&self) -> Option<u32> {
        (self.pan_mode == PanMode::Hold)
            .then(|| self.bindings.get(MouseAction::Pan).map(MouseButton::number))
            .flatten()
    }
}


#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use oxiedraw_core::enum_meta::EnumMeta;

    use super::*;
    use crate::input::bindings::check;

    #[test]
    fn table_is_well_formed() {
        check::table_is_well_formed::<MouseAction>();
    }

    #[test]
    fn every_assignment_keeps_the_table_sound() {
        assert_eq!(check::every_assignment_keeps_the_table_sound::<MouseAction>(), 60 * 3 * 5);
    }

    #[test]
    fn button_numbers_round_trip() {
        for button in ALL_BUTTONS.iter().flatten() {
            assert_eq!(MouseButton::from_number(button.number()), Some(*button));
        }
        assert_eq!(MouseButton::from_number(4), None);
    }

    #[test]
    fn defaults_match_the_old_hardwired_buttons() {
        let mouse = MouseProfile::default();
        assert_eq!(mouse.resolve(1), Some(PointerAction::Draw));
        assert_eq!(mouse.resolve(2), Some(PointerAction::Pan(PanMode::Hold)));
        assert_eq!(mouse.resolve(3), Some(PointerAction::QuickMenu));
        assert_eq!(mouse.resolve(8), None);
        assert_eq!(mouse.resolve(9), None);
    }

    #[test]
    fn every_combination_resolves_each_button_to_its_action() {
        for bindings in check::every_state::<MouseAction>() {
            for &pan_mode in PanMode::ALL {
                let mouse = MouseProfile { bindings: bindings.clone(), pan_mode };
                for button in ALL_BUTTONS.iter().flatten() {
                    let expected = match mouse.bindings.action_for(*button) {
                        Some(MouseAction::Draw) => Some(PointerAction::Draw),
                        Some(MouseAction::QuickMenu) => Some(PointerAction::QuickMenu),
                        Some(MouseAction::Pan) => Some(PointerAction::Pan(pan_mode)),
                        None => None,
                    };
                    assert_eq!(mouse.resolve(button.number()), expected, "{mouse:?} {button:?}");
                }
                assert_eq!(mouse.draw_button(), mouse.bindings.get(MouseAction::Draw).map(MouseButton::number));
                let hold = mouse.hold_pan_button();
                assert_eq!(hold.is_some(), pan_mode == PanMode::Hold);
                if let Some(number) = hold {
                    assert_eq!(mouse.resolve(number), Some(PointerAction::Pan(PanMode::Hold)));
                }
            }
        }
    }

    #[test]
    fn stealing_the_draw_button_moves_the_old_owner_to_a_free_one() {
        let mut mouse = MouseProfile::default();
        let displaced = mouse.bindings.assign(MouseAction::Draw, Some(MouseButton::Right));
        assert_eq!(displaced.len(), 1);
        assert_eq!(displaced[0].action, MouseAction::QuickMenu);
        assert_eq!(displaced[0].now, Some(MouseButton::Left));
        assert_eq!(mouse.resolve(3), Some(PointerAction::Draw));
        assert_eq!(mouse.resolve(1), Some(PointerAction::QuickMenu));
    }

    #[test]
    fn a_damaged_value_only_resets_its_own_field() {
        let json = r#"{ "bindings": { "draw": "laser" }, "pan_mode": "toggle" }"#;
        let mouse: MouseProfile = serde_json::from_str(json).unwrap();
        assert_eq!(mouse.bindings, Bindings::default());
        assert_eq!(mouse.pan_mode, PanMode::Toggle);
    }
}
