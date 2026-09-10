use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use super::floating::{FloatAnchor, FloatSize, FloatingPlacement, ToolWindowId};
use super::panel::PanelId;
use super::presets::{self, DEFAULT_LAYOUT_NAME};
use super::tree::LayoutNode;
use oxiedraw_core::enum_meta::EnumMeta;

// Everything here is repaired on load, never rejected: a settings file is
// user-editable, and a bad tree must not cost someone their window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Layout {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) preset: Option<String>,
    pub(crate) root: LayoutNode,
    #[serde(default)]
    pub(crate) floating: Vec<FloatingPlacement>,
}

impl Layout {
    pub(crate) fn sanitize(&mut self) -> bool {
        let tree_changed = self.root.sanitize();
        let unnamed = self.name.trim().is_empty();
        if unnamed {
            self.name = DEFAULT_LAYOUT_NAME.to_string();
        }
        let mut dropped = false;
        let mut seen: Vec<ToolWindowId> = Vec::new();
        self.floating.retain(|p| {
            let keep = !seen.contains(&p.window);
            seen.push(p.window);
            dropped |= !keep;
            keep
        });
        let mut added = false;
        for window in ToolWindowId::ALL {
            if !seen.contains(window) {
                self.floating.push(FloatingPlacement {
                    window: *window,
                    anchor: window.spec().default_anchor,
                    size: FloatSize::default(),
                });
                added = true;
            }
        }
        let mut resized = false;
        for placement in &mut self.floating {
            resized |= placement.size.sanitize();
        }
        tree_changed || unnamed || dropped || added || resized
    }

    pub(crate) fn is_panel_visible(&self, id: PanelId) -> bool {
        self.root.contains_panel(id)
    }

    pub(crate) fn set_panel_visible(&mut self, id: PanelId, visible: bool) {
        if visible == self.is_panel_visible(id) {
            return;
        }
        if visible {
            self.root.dock_panel(id, id.spec().default_side);
        } else if id.spec().removable {
            self.root.remove_panel(id);
        }
    }

    pub(crate) fn anchor_of(&self, window: ToolWindowId) -> FloatAnchor {
        self.floating
            .iter()
            .find(|p| p.window == window)
            .map_or_else(|| window.spec().default_anchor, |p| p.anchor)
    }

    pub(crate) fn set_anchor(&mut self, window: ToolWindowId, anchor: FloatAnchor) {
        if let Some(existing) = self.floating.iter_mut().find(|p| p.window == window) {
            existing.anchor = anchor;
        } else {
            self.floating.push(FloatingPlacement {
                window,
                anchor,
                size: FloatSize::default(),
            });
        }
    }

    pub(crate) fn float_size(&self, window: ToolWindowId) -> FloatSize {
        self.floating
            .iter()
            .find(|p| p.window == window)
            .map(|p| p.size)
            .unwrap_or_default()
    }

    pub(crate) fn set_float_size(&mut self, window: ToolWindowId, size: FloatSize) {
        if let Some(existing) = self.floating.iter_mut().find(|p| p.window == window) {
            existing.size = size;
        } else {
            self.floating.push(FloatingPlacement {
                window,
                anchor: window.spec().default_anchor,
                size,
            });
        }
    }

    pub(crate) fn duplicated(&self, name: String) -> Self {
        Self {
            name,
            preset: None,
            root: self.root.clone(),
            floating: self.floating.clone(),
        }
    }

    pub(crate) fn reset_to_preset(&mut self) -> bool {
        let Some(original) = self
            .preset
            .as_deref()
            .and_then(presets::preset_by_key)
        else {
            return false;
        };
        self.root = original.root;
        self.floating = original.floating;
        true
    }
}

static FALLBACK: LazyLock<Layout> = LazyLock::new(presets::default_layout);

impl Default for Layout {
    fn default() -> Self {
        FALLBACK.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LayoutSettings {
    pub(crate) layouts: Vec<Layout>,
    pub(crate) current: String,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            layouts: presets::builtin_layouts(),
            current: DEFAULT_LAYOUT_NAME.to_string(),
        }
    }
}

impl LayoutSettings {
    pub(crate) fn sanitize(&mut self) -> bool {
        let was_empty = self.layouts.is_empty();
        if was_empty {
            self.layouts = presets::builtin_layouts();
        }
        let mut changed = was_empty;
        for layout in &mut self.layouts {
            changed |= layout.sanitize();
        }
        let mut taken: Vec<String> = Vec::new();
        for index in 0..self.layouts.len() {
            let name = self.layouts[index].name.clone();
            if taken.contains(&name) {
                let unique = unique_among(&taken, &name);
                self.layouts[index].name.clone_from(&unique);
                taken.push(unique);
                changed = true;
            } else {
                taken.push(name);
            }
        }
        if !self.layouts.iter().any(|l| l.name == self.current) {
            self.current = self.layouts.first().map_or_else(
                || DEFAULT_LAYOUT_NAME.to_string(),
                |l| l.name.clone(),
            );
            changed = true;
        }
        changed
    }

    pub(crate) fn active_index(&self) -> usize {
        self.layouts
            .iter()
            .position(|l| l.name == self.current)
            .unwrap_or(0)
    }

    pub(crate) fn active(&self) -> &Layout {
        self.layouts
            .iter()
            .find(|l| l.name == self.current)
            .or_else(|| self.layouts.first())
            .unwrap_or(&FALLBACK)
    }

    pub(crate) fn active_mut(&mut self) -> &mut Layout {
        if self.layouts.is_empty() {
            self.layouts = presets::builtin_layouts();
            self.current = DEFAULT_LAYOUT_NAME.to_string();
        }
        let index = self.active_index();
        &mut self.layouts[index]
    }

    pub(crate) fn unique_name(&self, base: &str) -> String {
        let taken: Vec<String> = self.layouts.iter().map(|l| l.name.clone()).collect();
        unique_among(&taken, base)
    }

    pub(crate) fn duplicate_active(&mut self) -> String {
        let name = self.unique_name(&format!("{} copy", self.active().name));
        let copy = self.active().duplicated(name.clone());
        self.layouts.push(copy);
        self.current.clone_from(&name);
        name
    }

    // The last layout cannot be deleted; there is always one to be in.
    pub(crate) fn remove(&mut self, name: &str) -> bool {
        if self.layouts.len() <= 1 {
            return false;
        }
        let Some(index) = self.layouts.iter().position(|l| l.name == name) else {
            return false;
        };
        self.layouts.remove(index);
        if self.current == name {
            let next = index.min(self.layouts.len().saturating_sub(1));
            self.current = self.layouts[next].name.clone();
        }
        true
    }

    pub(crate) fn rename(&mut self, from: &str, to: &str) -> bool {
        let to = to.trim();
        if to.is_empty() || self.layouts.iter().any(|l| l.name == to) {
            return false;
        }
        let Some(layout) = self.layouts.iter_mut().find(|l| l.name == from) else {
            return false;
        };
        layout.name = to.to_string();
        if self.current == from {
            self.current = to.to_string();
        }
        true
    }
}

fn unique_among(taken: &[String], base: &str) -> String {
    if !taken.iter().any(|n| n == base) {
        return base.to_string();
    }
    (2..=taken.len() + 2)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| !taken.iter().any(|n| n == candidate))
        .unwrap_or_else(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::floating::Corner;

    #[test]
    fn defaults_hold_the_shipped_preset() {
        let mut settings = LayoutSettings::default();
        assert_eq!(settings.layouts.len(), 1);
        assert_eq!(settings.active().name, DEFAULT_LAYOUT_NAME);
        assert!(!settings.sanitize(), "the default block needs no repair");
    }

    #[test]
    fn an_unknown_selection_falls_back_to_the_first_layout() {
        let mut settings = LayoutSettings {
            current: "Deleted Layout".to_string(),
            ..Default::default()
        };
        assert!(settings.sanitize());
        assert_eq!(settings.current, DEFAULT_LAYOUT_NAME);
        assert_eq!(settings.active().name, DEFAULT_LAYOUT_NAME);
    }

    #[test]
    fn an_empty_list_is_reseeded() {
        let mut settings = LayoutSettings {
            layouts: Vec::new(),
            current: "gone".to_string(),
        };
        assert!(settings.sanitize());
        assert_eq!(settings.active().name, DEFAULT_LAYOUT_NAME);
    }

    #[test]
    fn duplicate_names_are_made_unique_and_keep_the_selection() {
        let base = presets::default_layout();
        let mut settings = LayoutSettings {
            layouts: vec![base.clone(), base.clone(), base],
            current: DEFAULT_LAYOUT_NAME.to_string(),
        };
        assert!(settings.sanitize());
        let names: Vec<&str> = settings.layouts.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                DEFAULT_LAYOUT_NAME,
                "Default Layout 2",
                "Default Layout 3"
            ]
        );
        assert_eq!(settings.active().name, DEFAULT_LAYOUT_NAME);
    }

    #[test]
    fn duplicating_selects_the_copy_and_drops_the_preset_link() {
        let mut settings = LayoutSettings::default();
        let name = settings.duplicate_active();
        assert_eq!(name, "Default Layout copy");
        assert_eq!(settings.current, name);
        assert_eq!(settings.layouts.len(), 2);
        assert!(
            settings.active().preset.is_none(),
            "a copy is the user's own, so Reset does not apply to it"
        );
        assert_eq!(settings.duplicate_active(), "Default Layout copy copy");
    }

    #[test]
    fn the_last_layout_cannot_be_deleted() {
        let mut settings = LayoutSettings::default();
        assert!(!settings.remove(DEFAULT_LAYOUT_NAME));
        assert_eq!(settings.layouts.len(), 1);
    }

    #[test]
    fn deleting_the_selected_layout_selects_a_neighbour() {
        let mut settings = LayoutSettings::default();
        let copy = settings.duplicate_active();
        assert!(settings.remove(&copy));
        assert_eq!(settings.current, DEFAULT_LAYOUT_NAME);
        assert!(settings.layouts.iter().all(|l| l.name != copy));
    }

    #[test]
    fn renaming_moves_the_selection_with_it() {
        let mut settings = LayoutSettings::default();
        assert!(settings.rename(DEFAULT_LAYOUT_NAME, "Painting"));
        assert_eq!(settings.current, "Painting");
        assert!(!settings.rename("Painting", "  "), "blank names rejected");
        settings.duplicate_active();
        assert!(
            !settings.rename("Painting", "Painting copy"),
            "a name already in use is rejected"
        );
    }

    #[test]
    fn hiding_and_restoring_a_panel_uses_its_default_side() {
        let mut layout = presets::default_layout();
        layout.set_panel_visible(PanelId::ColorPicker, false);
        assert!(!layout.is_panel_visible(PanelId::ColorPicker));

        layout.set_panel_visible(PanelId::ColorPicker, true);
        assert_eq!(
            layout.root.side_of(PanelId::ColorPicker),
            Some(PanelId::ColorPicker.spec().default_side)
        );
    }

    #[test]
    fn reset_restores_a_preset_but_leaves_a_copy_alone() {
        let mut layout = presets::default_layout();
        layout.set_panel_visible(PanelId::Layers, false);
        layout.set_anchor(
            crate::layout::ToolWindowId::Crop,
            FloatAnchor::Corner(Corner::BottomLeft),
        );
        assert!(layout.reset_to_preset());
        assert!(layout.is_panel_visible(PanelId::Layers));
        assert_eq!(
            layout.anchor_of(crate::layout::ToolWindowId::Crop),
            crate::layout::ToolWindowId::Crop.spec().default_anchor
        );

        let mut copy = layout.duplicated("Mine".to_string());
        assert!(!copy.reset_to_preset(), "a copy has nothing to reset to");
    }

    #[test]
    fn a_layout_missing_its_floating_block_gets_the_defaults() {
        let json = format!(
            r#"{{ "name": "Old", "root": {}, "floating": [] }}"#,
            serde_json::to_string(&LayoutNode::Canvas).expect("tree")
        );
        let mut layout: Layout = serde_json::from_str(&json).expect("parse");
        assert!(layout.sanitize());
        assert_eq!(layout.floating.len(), ToolWindowId::ALL.len());
        assert_eq!(
            layout.anchor_of(ToolWindowId::TextProperties),
            ToolWindowId::TextProperties.spec().default_anchor
        );
    }

    #[test]
    fn a_duplicate_traded_for_a_missing_window_still_counts_as_a_repair() {
        let first = ToolWindowId::ALL[0];
        let mut settings = LayoutSettings::default();
        let layout = settings.active_mut();
        layout.floating.retain(|p| p.window != first);
        layout.floating.push(FloatingPlacement {
            window: layout.floating[0].window,
            anchor: FloatAnchor::Corner(Corner::BottomLeft),
            size: FloatSize::default(),
        });

        let before = layout.floating.len();
        assert!(layout.sanitize(), "a traded duplicate is still a repair");
        assert_eq!(layout.floating.len(), before);
        assert_eq!(layout.floating.len(), ToolWindowId::ALL.len());
        assert!(!layout.sanitize(), "and once repaired it stays repaired");
    }

    #[test]
    fn settings_round_trip_through_json() {
        let mut settings = LayoutSettings::default();
        settings.duplicate_active();
        settings
            .active_mut()
            .set_panel_visible(PanelId::CanvasInfo, false);

        let json = serde_json::to_string(&settings).expect("serialise");
        let mut back: LayoutSettings = serde_json::from_str(&json).expect("deserialise");
        assert!(!back.sanitize(), "a round trip needs no repair");
        assert_eq!(back.current, settings.current);
        assert!(!back.active().is_panel_visible(PanelId::CanvasInfo));
    }
}
