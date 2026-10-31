//! Touchscreen finger gestures.

use serde::{Deserialize, Serialize};

use super::bindings::{Bindings, InputAction, Trigger};
use crate::settings::forgiving;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TouchGesture {
    OneFingerDrag,
    TwoFingerDrag,
    ThreeFingerDrag,
    TwoFingerRotate,
    ThreeFingerRotate,
    TwoFingerTap,
    ThreeFingerTap,
    FourFingerTap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GestureKind {
    Drag,
    Rotate,
    Tap,
}

impl TouchGesture {
    pub(crate) const ALL: [Self; 8] = [
        Self::OneFingerDrag,
        Self::TwoFingerDrag,
        Self::ThreeFingerDrag,
        Self::TwoFingerRotate,
        Self::ThreeFingerRotate,
        Self::TwoFingerTap,
        Self::ThreeFingerTap,
        Self::FourFingerTap,
    ];

    pub(crate) const fn kind(self) -> GestureKind {
        match self {
            Self::OneFingerDrag | Self::TwoFingerDrag | Self::ThreeFingerDrag => GestureKind::Drag,
            Self::TwoFingerRotate | Self::ThreeFingerRotate => GestureKind::Rotate,
            Self::TwoFingerTap | Self::ThreeFingerTap | Self::FourFingerTap => GestureKind::Tap,
        }
    }

    pub(crate) const fn fingers(self) -> usize {
        match self {
            Self::OneFingerDrag => 1,
            Self::TwoFingerDrag | Self::TwoFingerRotate | Self::TwoFingerTap => 2,
            Self::ThreeFingerDrag | Self::ThreeFingerRotate | Self::ThreeFingerTap => 3,
            Self::FourFingerTap => 4,
        }
    }

    pub(crate) fn tap(fingers: usize) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|g| g.kind() == GestureKind::Tap && g.fingers() == fingers)
    }
}

impl Trigger for TouchGesture {
    fn label(self) -> &'static str {
        match self {
            Self::OneFingerDrag => "One finger drag",
            Self::TwoFingerDrag => "Two fingers drag",
            Self::ThreeFingerDrag => "Three fingers drag",
            Self::TwoFingerRotate => "Two fingers rotate gesture",
            Self::ThreeFingerRotate => "Three fingers rotate gesture",
            Self::TwoFingerTap => "Two fingers tap",
            Self::ThreeFingerTap => "Three fingers tap",
            Self::FourFingerTap => "Four fingers tap",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TouchAction {
    Draw,
    Pan,
    Rotate,
    Undo,
    Redo,
    QuickMenu,
}

impl InputAction for TouchAction {
    type Trigger = TouchGesture;
    const ALL: &'static [Self] = &[
        Self::Draw,
        Self::Pan,
        Self::Rotate,
        Self::Undo,
        Self::Redo,
        Self::QuickMenu,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Draw => "Draw",
            Self::Pan => "Pan",
            Self::Rotate => "Rotate",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::QuickMenu => "Quick menu",
        }
    }

    fn choices(self) -> &'static [Option<TouchGesture>] {
        use TouchGesture as G;
        match self {
            Self::Draw => &[None, Some(G::OneFingerDrag)],
            Self::Pan => &[Some(G::OneFingerDrag), Some(G::TwoFingerDrag), Some(G::ThreeFingerDrag)],
            Self::Rotate => &[Some(G::TwoFingerRotate), Some(G::ThreeFingerRotate), None],
            Self::Undo => &[Some(G::TwoFingerTap), Some(G::ThreeFingerTap), Some(G::FourFingerTap), None],
            Self::Redo => &[None, Some(G::TwoFingerTap), Some(G::ThreeFingerTap), Some(G::FourFingerTap)],
            Self::QuickMenu => &[Some(G::ThreeFingerTap), Some(G::TwoFingerTap), Some(G::FourFingerTap)],
        }
    }

    fn default_binding(self) -> Option<TouchGesture> {
        self.choices()[0]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Navigation {
    pub(crate) pan: bool,
    pub(crate) zoom: bool,
    pub(crate) rotate: bool,
}

impl Navigation {
    pub(crate) const fn any(self) -> bool {
        self.pan || self.zoom || self.rotate
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TouchPlan {
    pub(crate) draw: bool,
    pan_fingers: Option<usize>,
    rotate_fingers: Option<usize>,
    taps: [Option<TouchAction>; 5],
}

impl TouchPlan {
    pub(crate) fn navigation(&self, fingers: usize) -> Navigation {
        if fingers == 0 || (self.draw && fingers == 1) {
            return Navigation::default();
        }
        let pan = self.pan_fingers.is_some_and(|n| fingers >= n);
        Navigation {
            pan,
            zoom: pan && fingers >= 2,
            rotate: self.rotate_fingers.is_some_and(|n| fingers >= n),
        }
    }

    pub(crate) fn tap(&self, fingers: usize) -> Option<TouchAction> {
        self.taps.get(fingers).copied().flatten()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TouchProfile {
    #[serde(default, deserialize_with = "forgiving")]
    pub(crate) bindings: Bindings<TouchAction>,
}

impl TouchProfile {
    pub(crate) fn plan(&self) -> TouchPlan {
        let fingers = |action| self.bindings.get(action).map(TouchGesture::fingers);
        let mut taps = [None; 5];
        for (count, slot) in taps.iter_mut().enumerate() {
            *slot = TouchGesture::tap(count).and_then(|g| self.bindings.action_for(g));
        }
        TouchPlan {
            draw: self.bindings.get(TouchAction::Draw).is_some(),
            pan_fingers: fingers(TouchAction::Pan),
            rotate_fingers: fingers(TouchAction::Rotate),
            taps,
        }
    }
}


#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::input::bindings::check;

    #[test]
    fn table_is_well_formed() {
        check::table_is_well_formed::<TouchAction>();
        for action in TouchAction::ALL {
            let kinds: Vec<_> = action.choices().iter().flatten().map(|g| g.kind()).collect();
            assert!(kinds.windows(2).all(|w| w[0] == w[1]), "{action:?} mixes gesture kinds");
        }
    }

    #[test]
    fn every_assignment_keeps_the_table_sound() {
        let states = check::every_state::<TouchAction>().len();
        let choices: usize = TouchAction::ALL.iter().map(|a| a.choices().len()).sum();
        assert_eq!(states, 315);
        assert_eq!(check::every_assignment_keeps_the_table_sound::<TouchAction>(), states * choices);
    }

    #[test]
    fn defaults_keep_the_old_gestures() {
        let plan = TouchProfile::default().plan();
        assert!(!plan.draw);
        assert_eq!(plan.navigation(1), Navigation { pan: true, zoom: false, rotate: false });
        assert_eq!(plan.navigation(2), Navigation { pan: true, zoom: true, rotate: true });
        assert_eq!(plan.tap(2), Some(TouchAction::Undo));
        assert_eq!(plan.tap(3), Some(TouchAction::QuickMenu));
        assert_eq!(plan.tap(4), None);
    }

    #[test]
    fn one_finger_drawing_pushes_pan_to_two_fingers() {
        let mut touch = TouchProfile::default();
        let displaced = touch.bindings.assign(TouchAction::Draw, Some(TouchGesture::OneFingerDrag));
        assert_eq!(displaced[0].action, TouchAction::Pan);
        assert_eq!(displaced[0].now, Some(TouchGesture::TwoFingerDrag));
        let plan = touch.plan();
        assert!(plan.draw);
        assert!(!plan.navigation(1).any());
        assert_eq!(plan.navigation(2), Navigation { pan: true, zoom: true, rotate: true });
    }

    #[test]
    fn swapping_taps_moves_the_quick_menu_off_its_tap() {
        let mut touch = TouchProfile::default();
        touch.bindings.assign(TouchAction::Undo, Some(TouchGesture::ThreeFingerTap));
        let plan = touch.plan();
        assert_eq!(plan.tap(3), Some(TouchAction::Undo));
        assert_eq!(plan.tap(2), Some(TouchAction::QuickMenu));
    }

    #[test]
    fn every_combination_plans_what_it_binds() {
        for bindings in check::every_state::<TouchAction>() {
            let touch = TouchProfile { bindings };
            let plan = touch.plan();
            let get = |a| touch.bindings.get(a);
            assert_eq!(plan.draw, get(TouchAction::Draw).is_some());
            for fingers in 0..=5 {
                let nav = plan.navigation(fingers);
                let reaches = |a| get(a).is_some_and(|g: TouchGesture| fingers >= g.fingers());
                let drawing = plan.draw && fingers == 1;
                let pan = fingers > 0 && !drawing && reaches(TouchAction::Pan);
                assert_eq!(nav.pan, pan, "{touch:?} with {fingers}");
                assert_eq!(nav.zoom, pan && fingers >= 2);
                assert_eq!(nav.rotate, fingers > 0 && !drawing && reaches(TouchAction::Rotate));
                if drawing {
                    assert!(!nav.any());
                }
                let tapped = TouchGesture::tap(fingers).and_then(|g| touch.bindings.action_for(g));
                assert_eq!(plan.tap(fingers), tapped);
                if let Some(action) = tapped {
                    assert!(matches!(action, TouchAction::Undo | TouchAction::Redo | TouchAction::QuickMenu));
                }
            }
            assert_eq!(plan.tap(1), None);
            assert!(plan.navigation(3).pan, "pan always reaches three fingers");
        }
    }
}
