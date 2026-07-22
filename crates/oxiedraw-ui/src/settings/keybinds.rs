use super::AppSettings;

pub(crate) struct ActionGroup {
    pub(crate) label: &'static str,
    pub(crate) actions: &'static [ActionInfo],
}

pub(crate) struct ActionInfo {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) default_accel: Option<&'static str>,
}

impl ActionInfo {
    /// Resolve the effective accelerator for this action, applying user overrides.
    /// Returns `None` if the binding has been explicitly cleared by the user.
    pub(crate) fn resolve_accel<'a>(&'a self, settings: &'a AppSettings) -> Option<&'a str> {
        match settings.keybinds.get(self.id) {
            Some(Some(custom)) => Some(custom.as_str()),
            Some(None) => None,         // explicitly unbound by user
            None => self.default_accel, // use default
        }
    }
}

/// True for bindings that record a bare modifier (no key), used to qualify a
/// canvas pan-button drag rather than trigger a GAction. The keybind recorder
/// and `apply_all_accels` treat these specially.
pub(crate) fn is_modifier_only(id: &str) -> bool {
    matches!(id, "rotate-modifier" | "rotate-snap-modifier")
}

// Returns the user's current accelerator parts for `id`, or `None` if unbound.
pub(crate) fn accel_parts_for(id: &str, settings: &AppSettings) -> Option<Vec<String>> {
    for group in ALL_ACTION_GROUPS {
        for info in group.actions {
            if info.id == id {
                return info.resolve_accel(settings).map(format_accel);
            }
        }
    }
    None
}

// Splits a GTK accelerator string into display parts, e.g. "<Primary>g" => ["Ctrl", "G"].
pub(crate) fn format_accel(accel: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut key = accel.to_string();

    for (tag, display) in &[
        ("<Primary>", "Ctrl"),
        ("<Shift>", "Shift"),
        ("<Alt>", "Alt"),
        ("<Super>", "Super"),
    ] {
        if key.contains(tag) {
            parts.push((*display).to_string());
            key = key.replace(tag, "");
        }
    }

    // Modifier-only accels (e.g. "<Shift>") leave no key part.
    if key.is_empty() {
        return parts;
    }

    let display = match key.as_str() {
        "equal" => "=",
        "plus" => "+",
        "minus" => "-",
        "underscore" => "_",
        "question" => "?",
        "comma" => ",",
        "period" => ".",
        "slash" => "/",
        "backslash" => "\\",
        "semicolon" => ";",
        "colon" => ":",
        "apostrophe" => "'",
        "quotedbl" => "\"",
        "bracketleft" => "[",
        "bracketright" => "]",
        "space" => "Space",
        "Return" => "Enter",
        "Tab" => "Tab",
        "Delete" => "Del",
        "BackSpace" => "Backspace",
        "Escape" => "Esc",
        "Home" => "Home",
        "End" => "End",
        "Page_Up" => "PgUp",
        "Page_Down" => "PgDn",
        "Up" => "^",
        "Down" => "v",
        "Left" => "<-",
        "Right" => "->",
        s if s.len() == 1 && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) => {
            parts.push(s.to_uppercase());
            return parts;
        }
        s => {
            parts.push(s.to_string());
            return parts;
        }
    };

    parts.push(display.to_string());
    parts
}

pub(crate) const ALL_ACTION_GROUPS: &[ActionGroup] = &[
    ActionGroup {
        label: "Application",
        actions: &[
            ActionInfo {
                id: "preferences",
                label: "Preferences",
                default_accel: Some("<Primary>comma"),
            },
            ActionInfo {
                id: "shortcuts",
                label: "Keyboard Shortcuts",
                default_accel: Some("<Primary>question"),
            },
            ActionInfo {
                id: "quit",
                label: "Quit OxieDraw",
                default_accel: Some("<Primary>q"),
            },
        ],
    },
    ActionGroup {
        label: "File",
        actions: &[
            ActionInfo {
                id: "new",
                label: "New",
                default_accel: Some("<Primary>n"),
            },
            ActionInfo {
                id: "open",
                label: "Open...",
                default_accel: Some("<Primary>o"),
            },
            ActionInfo {
                id: "save",
                label: "Save",
                default_accel: Some("<Primary>s"),
            },
            ActionInfo {
                id: "save-as",
                label: "Save As...",
                default_accel: None,
            },
            ActionInfo {
                id: "export-as",
                label: "Export As...",
                default_accel: Some("<Primary><Shift>e"),
            },
            ActionInfo {
                id: "close-tab",
                label: "Close Tab",
                default_accel: Some("<Primary>w"),
            },
        ],
    },
    ActionGroup {
        label: "Edit",
        actions: &[
            ActionInfo {
                id: "undo",
                label: "Undo",
                default_accel: Some("<Primary>z"),
            },
            ActionInfo {
                id: "redo",
                label: "Redo",
                default_accel: Some("<Primary><Shift>z"),
            },
            ActionInfo {
                id: "cut",
                label: "Cut",
                default_accel: Some("<Primary>x"),
            },
            ActionInfo {
                id: "copy",
                label: "Copy",
                default_accel: Some("<Primary>c"),
            },
            ActionInfo {
                id: "paste",
                label: "Paste",
                default_accel: Some("<Primary>v"),
            },
            ActionInfo {
                id: "select-all",
                label: "Select All",
                default_accel: Some("<Primary>a"),
            },
            ActionInfo {
                id: "deselect-all",
                label: "Deselect",
                default_accel: Some("<Primary><Shift>s"),
            },
            ActionInfo {
                id: "select-inverse",
                label: "Inverse Selection",
                default_accel: Some("<Primary><Shift>i"),
            },
        ],
    },
    ActionGroup {
        label: "Tools",
        actions: &[
            ActionInfo {
                id: "select-cursor",
                label: "Cursor",
                default_accel: Some("v"),
            },
            ActionInfo {
                id: "select-selection",
                label: "Selection",
                default_accel: Some("s"),
            },
            ActionInfo {
                id: "select-transform",
                label: "Transform",
                default_accel: Some("<Primary>t"),
            },
            ActionInfo {
                id: "select-brush",
                label: "Brush",
                default_accel: Some("b"),
            },
            ActionInfo {
                id: "select-picker",
                label: "Color Picker",
                default_accel: Some("i"),
            },
            ActionInfo {
                id: "select-fill",
                label: "Fill Bucket",
                default_accel: Some("g"),
            },
            ActionInfo {
                id: "select-text",
                label: "Text",
                default_accel: Some("t"),
            },
            ActionInfo {
                id: "select-crop",
                label: "Crop",
                default_accel: Some("<Shift>c"),
            },
            ActionInfo {
                id: "eraser-toggle",
                label: "Toggle Eraser",
                default_accel: Some("e"),
            },
        ],
    },
    ActionGroup {
        label: "Color",
        actions: &[ActionInfo {
            id: "swap-colors",
            label: "Swap Primary/Secondary Color",
            default_accel: Some("x"),
        }],
    },
    ActionGroup {
        label: "Layers",
        actions: &[
            ActionInfo {
                id: "rename",
                label: "Rename Layer / Component",
                default_accel: Some("F2"),
            },
            ActionInfo {
                id: "layer-duplicate",
                label: "Duplicate Layer",
                default_accel: Some("<Primary>d"),
            },
            ActionInfo {
                id: "layer-delete",
                label: "Delete Layer",
                default_accel: Some("Delete"),
            },
            ActionInfo {
                id: "layer-group",
                label: "Group Selected Layers",
                default_accel: Some("<Primary>g"),
            },
            ActionInfo {
                id: "layers-merge",
                label: "Merge Selected Layers",
                default_accel: None,
            },
        ],
    },
    ActionGroup {
        label: "Filters",
        actions: &[
            ActionInfo {
                id: "filter-hsv",
                label: "Hue/Saturation/Value",
                default_accel: Some("<Primary>u"),
            },
            ActionInfo {
                id: "filter-invert",
                label: "Invert",
                default_accel: Some("<Primary>i"),
            },
            ActionInfo {
                id: "filter-blur",
                label: "Blur",
                default_accel: None,
            },
            ActionInfo {
                id: "filter-sharpen",
                label: "Sharpen",
                default_accel: None,
            },
        ],
    },
    ActionGroup {
        label: "View",
        actions: &[
            ActionInfo {
                id: "zoom-in",
                label: "Zoom In",
                default_accel: Some("<Primary>equal"),
            },
            ActionInfo {
                id: "zoom-out",
                label: "Zoom Out",
                default_accel: Some("<Primary>minus"),
            },
            ActionInfo {
                id: "zoom-fit",
                label: "Zoom to Fit",
                default_accel: Some("<Primary>0"),
            },
            ActionInfo {
                id: "fullscreen",
                label: "Full Screen",
                default_accel: Some("F11"),
            },
            ActionInfo {
                id: "perf-graph",
                label: "Performance Graph",
                default_accel: Some("F3"),
            },
        ],
    },
    ActionGroup {
        label: "Canvas Navigation",
        // Modifier-only bindings (see `is_modifier_only`): held together with
        // the pan button (middle click / stylus pan) while dragging, not
        // pressed as keyboard shortcuts.
        actions: &[
            ActionInfo {
                id: "rotate-modifier",
                label: "Rotate canvas (hold + pan-drag)",
                default_accel: Some("<Shift>"),
            },
            ActionInfo {
                id: "rotate-snap-modifier",
                label: "Snap rotation to step (hold + pan-drag)",
                default_accel: Some("<Primary>"),
            },
        ],
    },
];
