//! Drawing tablet pens: tip, eraser end, barrel buttons and the pressure curve.

use std::collections::BTreeSet;

use oxiedraw_core::curves::Curve;
use serde::{Deserialize, Serialize};

use super::bindings::{Bindings, Displaced, InputAction, PanMode, Trigger};
use super::devices::PointerAction;
use crate::settings::forgiving;

pub(crate) const TIP_BUTTON: u32 = 1;

// GDK numbers the barrel buttons BTN_STYLUS = 3, BTN_STYLUS2 = 2, BTN_STYLUS3 = 8.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PenInput {
    Tip,
    EraserTip,
    LowerButton,
    UpperButton,
    ThirdButton,
}

const PEN_BUTTONS: [PenInput; 3] =
    [PenInput::LowerButton, PenInput::UpperButton, PenInput::ThirdButton];

impl PenInput {
    pub(crate) const fn button_number(self) -> u32 {
        match self {
            Self::Tip | Self::EraserTip => TIP_BUTTON,
            Self::LowerButton => 3,
            Self::UpperButton => 2,
            Self::ThirdButton => 8,
        }
    }

    pub(crate) fn from_press(button: u32, eraser: bool) -> Option<Self> {
        match button {
            TIP_BUTTON if eraser => Some(Self::EraserTip),
            TIP_BUTTON => Some(Self::Tip),
            _ => PEN_BUTTONS.into_iter().find(|b| b.button_number() == button),
        }
    }

    pub(crate) const fn is_button(self) -> bool {
        !matches!(self, Self::Tip | Self::EraserTip)
    }
}

impl Trigger for PenInput {
    fn label(self) -> &'static str {
        match self {
            Self::Tip => "Pen press",
            Self::EraserTip => "Eraser tip",
            Self::LowerButton => "Lower barrel button",
            Self::UpperButton => "Upper barrel button",
            Self::ThirdButton => "Third barrel button",
        }
    }
}

const BUTTON_CHOICES: &[Option<PenInput>] = &[
    Some(PenInput::LowerButton),
    Some(PenInput::UpperButton),
    Some(PenInput::ThirdButton),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PenAction {
    Draw,
    Erase,
    QuickMenu,
    Pan,
}

impl InputAction for PenAction {
    type Trigger = PenInput;
    const ALL: &'static [Self] = &[Self::Draw, Self::Erase, Self::QuickMenu, Self::Pan];

    fn label(self) -> &'static str {
        match self {
            Self::Draw => "Draw",
            Self::Erase => "Erase",
            Self::QuickMenu => "Quick menu",
            Self::Pan => "Pan",
        }
    }

    fn choices(self) -> &'static [Option<PenInput>] {
        match self {
            Self::Draw => &[Some(PenInput::Tip), None],
            Self::Erase => &[Some(PenInput::EraserTip), None],
            Self::QuickMenu | Self::Pan => BUTTON_CHOICES,
        }
    }

    fn default_binding(self) -> Option<PenInput> {
        match self {
            Self::Draw => Some(PenInput::Tip),
            Self::Erase => None,
            Self::QuickMenu => Some(PenInput::LowerButton),
            Self::Pan => Some(PenInput::UpperButton),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PenProfile {
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) bindings: Bindings<PenAction>,
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) pan_mode: PanMode,
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) pressure: Curve,
    #[serde(default)]
    pub(crate) eraser_detected: bool,
    #[serde(default)]
    pub(crate) erase_customized: bool,
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) detected_buttons: BTreeSet<PenInput>,
}

impl PenProfile {
    pub(crate) fn resolve(&self, button: u32, eraser: bool) -> Option<PointerAction> {
        let action = self.bindings.action_for(PenInput::from_press(button, eraser)?)?;
        Some(match action {
            PenAction::Draw => PointerAction::Draw,
            PenAction::Erase => PointerAction::Erase,
            PenAction::QuickMenu => PointerAction::QuickMenu,
            PenAction::Pan => PointerAction::Pan(self.pan_mode),
        })
    }

    pub(crate) fn assign(&mut self, action: PenAction, binding: Option<PenInput>) -> Vec<Displaced<PenAction>> {
        if action == PenAction::Erase {
            self.erase_customized = true;
        }
        self.bindings.assign(action, binding)
    }

    pub(crate) fn note_eraser(&mut self) -> bool {
        if self.eraser_detected {
            return false;
        }
        self.eraser_detected = true;
        if !self.erase_customized {
            self.bindings.assign(PenAction::Erase, Some(PenInput::EraserTip));
        }
        true
    }

    pub(crate) fn note_button(&mut self, button: u32) -> bool {
        PenInput::from_press(button, false)
            .filter(|input| input.is_button())
            .is_some_and(|input| self.detected_buttons.insert(input))
    }

    pub(crate) fn absorb_detections(&mut self, other: &Self) {
        if other.eraser_detected {
            self.note_eraser();
        }
        self.detected_buttons.extend(&other.detected_buttons);
    }

    pub(crate) fn uses_tip(&self) -> bool {
        self.bindings.get(PenAction::Draw).is_some() || self.bindings.get(PenAction::Erase).is_some()
    }

    pub(crate) fn hold_pan_button(&self) -> Option<u32> {
        (self.pan_mode == PanMode::Hold)
            .then(|| self.bindings.get(PenAction::Pan).map(PenInput::button_number))
            .flatten()
    }
}


#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use oxiedraw_core::curves::CurvePoint;
    use oxiedraw_core::enum_meta::EnumMeta;

    use super::*;
    use crate::input::bindings::check;

    const ALL_INPUTS: [PenInput; 5] = [
        PenInput::Tip,
        PenInput::EraserTip,
        PenInput::LowerButton,
        PenInput::UpperButton,
        PenInput::ThirdButton,
    ];

    fn press_of(input: PenInput) -> (u32, bool) {
        (input.button_number(), input == PenInput::EraserTip)
    }

    #[test]
    fn table_is_well_formed() {
        check::table_is_well_formed::<PenAction>();
    }

    #[test]
    fn every_assignment_keeps_the_table_sound() {
        assert_eq!(check::every_assignment_keeps_the_table_sound::<PenAction>(), 24 * 10);
    }

    #[test]
    fn presses_map_back_to_their_input() {
        for input in ALL_INPUTS {
            let (button, eraser) = press_of(input);
            assert_eq!(PenInput::from_press(button, eraser), Some(input));
        }
        assert_eq!(PenInput::from_press(3, true), Some(PenInput::LowerButton));
        assert_eq!(PenInput::from_press(9, false), None);
    }

    #[test]
    fn defaults_keep_the_old_behaviour_except_the_unseen_eraser() {
        let pen = PenProfile::default();
        assert_eq!(pen.resolve(1, false), Some(PointerAction::Draw));
        assert_eq!(pen.resolve(1, true), None);
        assert_eq!(pen.resolve(3, false), Some(PointerAction::QuickMenu));
        assert_eq!(pen.resolve(2, false), Some(PointerAction::Pan(PanMode::Hold)));
        assert_eq!(pen.resolve(8, false), None);
    }

    #[test]
    fn every_combination_resolves_each_input_to_its_action() {
        for bindings in check::every_state::<PenAction>() {
            for &pan_mode in PanMode::ALL {
                let pen = PenProfile { bindings: bindings.clone(), pan_mode, ..PenProfile::default() };
                for input in ALL_INPUTS {
                    let expected = match pen.bindings.action_for(input) {
                        Some(PenAction::Draw) => Some(PointerAction::Draw),
                        Some(PenAction::Erase) => Some(PointerAction::Erase),
                        Some(PenAction::QuickMenu) => Some(PointerAction::QuickMenu),
                        Some(PenAction::Pan) => Some(PointerAction::Pan(pan_mode)),
                        None => None,
                    };
                    let (button, eraser) = press_of(input);
                    assert_eq!(pen.resolve(button, eraser), expected, "{pen:?} {input:?}");
                }
                let hold = pen.hold_pan_button();
                assert_eq!(hold.is_some(), pan_mode == PanMode::Hold);
                if let Some(number) = hold {
                    assert_eq!(pen.resolve(number, false), Some(PointerAction::Pan(PanMode::Hold)));
                }
                assert_eq!(pen.uses_tip(), pen.resolve(1, false).is_some() || pen.resolve(1, true).is_some());
            }
        }
    }

    #[test]
    fn an_eraser_end_binds_erase_once() {
        let mut pen = PenProfile::default();
        assert!(pen.note_eraser());
        assert!(!pen.note_eraser());
        assert_eq!(pen.resolve(1, true), Some(PointerAction::Erase));
    }

    #[test]
    fn an_eraser_end_respects_a_chosen_unbind() {
        let mut pen = PenProfile::default();
        pen.assign(PenAction::Erase, None);
        assert!(pen.erase_customized);
        assert!(pen.note_eraser());
        assert_eq!(pen.resolve(1, true), None);
    }

    #[test]
    fn buttons_are_detected_once_and_the_tip_never() {
        let mut pen = PenProfile::default();
        assert!(!pen.note_button(1));
        assert!(pen.note_button(3));
        assert!(!pen.note_button(3));
        assert!(pen.note_button(8));
        assert!(!pen.note_button(42));
        assert_eq!(
            pen.detected_buttons.iter().copied().collect::<Vec<_>>(),
            vec![PenInput::LowerButton, PenInput::ThirdButton]
        );
    }

    #[test]
    fn profiles_survive_a_save() {
        let mut pen = PenProfile::default();
        pen.note_eraser();
        pen.note_button(8);
        pen.pan_mode = PanMode::Toggle;
        pen.assign(PenAction::Pan, Some(PenInput::ThirdButton));
        pen.pressure = Curve::from_points(&[CurvePoint::new(0, 20), CurvePoint::new(255, 200)]);
        let back: PenProfile = serde_json::from_str(&serde_json::to_string(&pen).unwrap()).unwrap();
        assert_eq!(back, pen);
    }
}
