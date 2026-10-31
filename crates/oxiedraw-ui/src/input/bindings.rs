//! Action-to-trigger tables shared by every input device kind.

use std::collections::BTreeMap;
use std::fmt::Debug;

use oxiedraw_core::enum_meta::EnumMeta;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub(crate) trait Trigger: Copy + Eq + Debug + Serialize + DeserializeOwned + 'static {
    fn label(self) -> &'static str;
}

pub(crate) trait InputAction:
    Copy + Ord + Debug + Serialize + DeserializeOwned + 'static
{
    type Trigger: Trigger;
    const ALL: &'static [Self];
    fn label(self) -> &'static str;
    fn choices(self) -> &'static [Option<Self::Trigger>];
    fn default_binding(self) -> Option<Self::Trigger>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PanMode {
    #[default]
    Hold,
    Toggle,
}

impl EnumMeta for PanMode {
    const ALL: &'static [Self] = &[Self::Hold, Self::Toggle];

    fn label(self) -> &'static str {
        match self {
            Self::Hold => "Hold",
            Self::Toggle => "Toggle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Displaced<A: InputAction> {
    pub(crate) action: A,
    pub(crate) lost: A::Trigger,
    pub(crate) now: Option<A::Trigger>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    bound = "",
    from = "BTreeMap<A, Option<A::Trigger>>",
    into = "BTreeMap<A, Option<A::Trigger>>"
)]
pub(crate) struct Bindings<A: InputAction> {
    slots: BTreeMap<A, Option<A::Trigger>>,
}

impl<A: InputAction> Default for Bindings<A> {
    fn default() -> Self {
        Self {
            slots: A::ALL.iter().map(|&a| (a, a.default_binding())).collect(),
        }
    }
}

impl<K: InputAction> From<BTreeMap<K, Option<K::Trigger>>> for Bindings<K> {
    fn from(stored: BTreeMap<K, Option<K::Trigger>>) -> Self {
        let mut bindings = Self::default();
        for &action in K::ALL {
            if let Some(&binding) = stored.get(&action) {
                bindings.assign(action, binding);
            }
        }
        bindings
    }
}

impl<K: InputAction> From<Bindings<K>> for BTreeMap<K, Option<K::Trigger>> {
    fn from(bindings: Bindings<K>) -> Self {
        bindings.slots
    }
}

impl<A: InputAction> Bindings<A> {
    pub(crate) fn get(&self, action: A) -> Option<A::Trigger> {
        self.slots.get(&action).copied().flatten()
    }

    pub(crate) fn action_for(&self, trigger: A::Trigger) -> Option<A> {
        self.slots
            .iter()
            .find(|(_, bound)| **bound == Some(trigger))
            .map(|(action, _)| *action)
    }

    pub(crate) fn assign(&mut self, action: A, binding: Option<A::Trigger>) -> Vec<Displaced<A>> {
        if !action.choices().contains(&binding) || self.get(action) == binding {
            return Vec::new();
        }
        let owner = binding.and_then(|t| self.action_for(t));
        self.slots.insert(action, binding);
        let (Some(victim), Some(lost)) = (owner, binding) else {
            return Vec::new();
        };
        let now = self.fallback(victim);
        self.slots.insert(victim, now);
        vec![Displaced { action: victim, lost, now }]
    }

    fn fallback(&self, action: A) -> Option<A::Trigger> {
        let choices = action.choices();
        if choices.contains(&None) {
            return None;
        }
        choices
            .iter()
            .flatten()
            .copied()
            .find(|&t| self.action_for(t).is_none())
    }
}


#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) mod check {
    use std::collections::BTreeMap;

    use super::{Bindings, InputAction};

    pub(crate) fn has_no_clash<A: InputAction>(bindings: &Bindings<A>) -> bool {
        let bound: Vec<_> = A::ALL.iter().filter_map(|&a| bindings.get(a)).collect();
        bound
            .iter()
            .enumerate()
            .all(|(i, t)| !bound[i + 1..].contains(t))
    }

    pub(crate) fn every_state<A: InputAction>() -> Vec<Bindings<A>> {
        let mut states = vec![BTreeMap::new()];
        for &action in A::ALL {
            states = states
                .into_iter()
                .flat_map(|slots: BTreeMap<A, Option<A::Trigger>>| {
                    action.choices().iter().map(move |&choice| {
                        let mut next = slots.clone();
                        next.insert(action, choice);
                        next
                    })
                })
                .collect();
        }
        states
            .into_iter()
            .map(|slots| Bindings { slots })
            .filter(has_no_clash)
            .collect()
    }

    pub(crate) fn table_is_well_formed<A: InputAction>() {
        let defaults = Bindings::<A>::default();
        assert!(has_no_clash(&defaults), "defaults clash: {defaults:?}");
        for &action in A::ALL {
            let choices = action.choices();
            assert!(!choices.is_empty(), "{action:?} has no choices");
            assert!(choices.contains(&action.default_binding()), "{action:?} default not offered");
            for (i, c) in choices.iter().enumerate() {
                assert!(!choices[i + 1..].contains(c), "{action:?} offers {c:?} twice");
            }
        }
    }

    pub(crate) fn every_assignment_keeps_the_table_sound<A: InputAction>() -> usize {
        let mut checked = 0;
        for state in every_state::<A>() {
            for &action in A::ALL {
                for &choice in action.choices() {
                    let mut next = state.clone();
                    let displaced = next.assign(action, choice);
                    assert_eq!(next.get(action), choice, "{action:?} <- {choice:?} on {state:?}");
                    assert!(has_no_clash(&next), "clash after {action:?} <- {choice:?} on {state:?}");
                    for &other in A::ALL.iter().filter(|&&o| o != action) {
                        let moved = displaced.iter().find(|d| d.action == other);
                        match moved {
                            None => assert_eq!(next.get(other), state.get(other), "{other:?} moved silently"),
                            Some(d) => {
                                assert_eq!(Some(d.lost), choice);
                                assert_eq!(state.get(other), choice);
                                assert_eq!(next.get(other), d.now);
                                assert!(other.choices().contains(&d.now));
                                if other.choices().contains(&None) {
                                    assert_eq!(d.now, None, "{other:?} should unbind");
                                } else {
                                    assert!(d.now.is_some(), "{other:?} left without a binding");
                                }
                            }
                        }
                    }
                    let round_trip: Bindings<A> =
                        serde_json::from_str(&serde_json::to_string(&next).unwrap()).unwrap();
                    assert_eq!(round_trip, next);
                    checked += 1;
                }
            }
        }
        checked
    }
}
