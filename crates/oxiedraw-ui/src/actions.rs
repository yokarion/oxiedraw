use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;

use crate::canvas::Viewport;
use crate::settings::AppSettings;
use crate::settings::keybinds::ALL_ACTION_GROUPS;

// -- Viewport action handlers --------------------------------------------------

struct ViewportActionDef {
    id: &'static str,
    handler: fn(&Viewport),
}

const VIEWPORT_ACTIONS: &[ViewportActionDef] = &[
    ViewportActionDef {
        id: "zoom-in",
        handler: Viewport::zoom_in,
    },
    ViewportActionDef {
        id: "zoom-out",
        handler: Viewport::zoom_out,
    },
    ViewportActionDef {
        id: "zoom-fit",
        handler: Viewport::zoom_fit,
    },
    ViewportActionDef {
        id: "perf-graph",
        handler: Viewport::toggle_perf_graph,
    },
];

// -- Registration --------------------------------------------------------------

/// Register the zoom actions (which target the active document's viewport) and
/// apply keybindings from settings. Call once from `AppModel::init`.
///
/// `active_viewport` resolves the viewport of the currently focused tab at
/// activation time, so zoom always affects the document the user is looking at.
pub(crate) fn register(active_viewport: Rc<dyn Fn() -> Option<Viewport>>) {
    let Some(gio_app) = gio::Application::default() else {
        tracing::warn!("actions::register: no default application");
        return;
    };
    let Ok(app) = gio_app.downcast::<gtk::Application>() else {
        tracing::warn!("actions::register: default app is not a gtk::Application");
        return;
    };

    let settings = AppSettings::load();

    for def in VIEWPORT_ACTIONS {
        let provider = Rc::clone(&active_viewport);
        let handler = def.handler;
        let action = gio::SimpleAction::new(def.id, None);
        action.connect_activate(move |_, _| {
            if let Some(vp) = provider() {
                handler(&vp);
            }
        });
        app.add_action(&action);
    }

    apply_all_accels(&app, &settings);
}

/// Apply accels for every known action, respecting user overrides.
/// Called at startup and whenever settings change in the preferences window.
pub(crate) fn apply_all_accels(app: &gtk::Application, settings: &AppSettings) {
    for group in ALL_ACTION_GROUPS {
        for info in group.actions {
            // Modifier-only bindings qualify a canvas drag; they have no GAction.
            if crate::settings::keybinds::is_modifier_only(info.id) {
                continue;
            }
            let target = format!("app.{}", info.id);
            match info.resolve_accel(settings) {
                Some(a) => app.set_accels_for_action(&target, &[a]),
                None => app.set_accels_for_action(&target, &[]),
            }
        }
    }
}
