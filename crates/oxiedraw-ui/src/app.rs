use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::tools::Tool;
use oxiedraw_utils::geometry::Size;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::{ComponentParts, ComponentSender, SimpleComponent};

use crate::session::{GlobalState, SetActiveToolSlot};
use crate::tabs::TabManager;
use crate::{actions, left_bar, preferences_window, top_bar};

#[derive(Debug)]
pub(crate) struct AppInit {
    pub(crate) canvas: Size,
}

impl Default for AppInit {
    fn default() -> Self {
        Self {
            canvas: Size::new(2048, 2048),
        }
    }
}

#[derive(Debug)]
pub(crate) enum AppMsg {}

pub(crate) struct AppModel {
    // The tab manager owns every open document session; held here so it (and
    // therefore all sessions) lives for the lifetime of the window.
    #[allow(dead_code)]
    manager: Rc<TabManager>,
}

#[relm4::component(pub)]
impl SimpleComponent for AppModel {
    type Init = AppInit;
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("OxieDraw"),
            set_default_size: (1280, 800),

            #[name = "toast_overlay"]
            adw::ToastOverlay {
                #[name = "toast_layer"]
                gtk::Overlay {
                #[wrap(Some)]
                set_child = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    append: &top_bar_widget,

                    // Active document's tool-options bar is swapped in here.
                    #[name = "tool_options_slot"]
                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_vexpand: true,

                        append: &left_bar_widget,

                        gtk::Paned {
                            set_orientation: gtk::Orientation::Horizontal,
                            set_hexpand: true,
                            set_vexpand: true,
                            set_resize_start_child: true,
                            set_resize_end_child: false,
                            set_shrink_start_child: false,
                            set_shrink_end_child: false,
                            set_wide_handle: true,

                            #[wrap(Some)]
                            set_start_child = &gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_hexpand: true,
                                set_vexpand: true,

                                #[name = "tab_bar"]
                                adw::TabBar {},

                                #[name = "tab_view"]
                                adw::TabView {
                                    set_hexpand: true,
                                    set_vexpand: true,
                                },
                            },

                            // Active document's right sidebar is swapped in here.
                            #[wrap(Some)]
                            #[name = "right_bar_slot"]
                            set_end_child = &gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                            },
                        },
                    },
                },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display).add_resource_path(crate::ICON_RESOURCE_PATH);
        }
        gtk::Window::set_default_icon_name(crate::APP_ID);
        preferences_window::load_keybind_css();

        let history_capacity = crate::settings::AppSettings::load().history.capacity;
        let global = GlobalState::new();

        // Late-bound window-level tool setter so the left bar / key handler can
        // dispatch tool switches before the tab manager exists.
        let set_active_tool_late: SetActiveToolSlot = Rc::new(RefCell::new(None));

        let (top_bar_widget, apply_decorations) = top_bar::build();
        let apply_decorations: Rc<dyn Fn(bool)> = Rc::new(apply_decorations);

        let on_change_for_lb: Rc<dyn Fn(Tool)> = {
            let late = Rc::clone(&set_active_tool_late);
            Rc::new(move |t| {
                if let Some(f) = late.borrow().as_ref() {
                    f(t);
                }
            })
        };
        let (left_bar_widget, set_left_bar) = left_bar::build(&global.tools, &on_change_for_lb);
        let set_left_bar: Rc<dyn Fn(Tool)> = Rc::new(set_left_bar);

        let widgets = view_output!();

        global.toaster.bind(widgets.toast_overlay.clone(), &widgets.toast_layer);
        widgets.tab_bar.set_view(Some(&widgets.tab_view));

        let manager = Rc::new(TabManager {
            global: global.clone(),
            set_active_tool_late: Rc::clone(&set_active_tool_late),
            set_left_bar,
            tab_view: widgets.tab_view.clone(),
            tool_options_slot: widgets.tool_options_slot.clone(),
            right_bar_slot: widgets.right_bar_slot.clone(),
            sessions: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            root: root.clone(),
            history_capacity,
            untitled_counter: Cell::new(0),
        });

        // Publish the window-level tool setter now that the manager exists.
        {
            let manager_c = Rc::clone(&manager);
            let setter: Rc<dyn Fn(Tool)> = Rc::new(move |t| manager_c.set_active_tool(t));
            *set_active_tool_late.borrow_mut() = Some(setter);
        }

        manager.connect_tab_signals();
        manager.register_actions(&app_handle());

        register_window_actions(&manager, &root, &global, &apply_decorations);

        // Enter / Escape handler for the transform tool + Escape-deselect,
        // routed to the active document.
        install_key_handler(&root, &manager, &global);

        // Closing the window prompts to save any documents with unsaved changes.
        {
            let manager = Rc::clone(&manager);
            root.connect_close_request(move |_| manager.on_window_close_request());
        }

        // The first document is created by the splash loader once fonts are
        // loaded, then the window is revealed; building it earlier would show an
        // empty font dropdown. Until then the window stays hidden.
        root.set_visible(false);
        {
            let manager_for_finish = Rc::clone(&manager);
            let root_for_finish = root.clone();
            let canvas = init.canvas;
            let finish: Box<dyn FnOnce()> = Box::new(move || {
                manager_for_finish.new_document(canvas);
                root_for_finish.set_visible(true);
            });
            crate::splash::run(global.clone(), finish);
        }

        if let Some(start) = crate::STARTUP.get() {
            tracing::info!(elapsed_ms = start.elapsed().as_millis(), "app init complete");
            // Log again once the window is actually mapped (first frame), which
            // is the real "ready to use" point.
            let start = *start;
            let logged = std::cell::Cell::new(false);
            root.connect_map(move |_| {
                if !logged.replace(true) {
                    tracing::info!(
                        elapsed_ms = start.elapsed().as_millis(),
                        "window mapped - ready to use"
                    );
                }
            });
        }

        let model = Self { manager };
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {}
    }
}

/// Fetch the running `gtk::Application`. The component is only ever constructed
/// from inside a `RelmApp` run, so this is always present.
fn app_handle() -> gtk::Application {
    gtk::gio::Application::default()
        .and_then(|a| a.downcast::<gtk::Application>().ok())
        .expect("default gtk::Application")
}

/// Register the gio actions that need window-level context (zoom, preferences,
/// export, filters, brush manager) plus keyboard accelerators.
fn register_window_actions(
    manager: &Rc<TabManager>,
    root: &adw::ApplicationWindow,
    global: &GlobalState,
    apply_decorations: &Rc<dyn Fn(bool)>,
) {
    use gtk::gio;
    let app = app_handle();

    // Zoom actions target the active document's viewport.
    actions::register(manager.active_viewport_provider());

    // Pixel-view settings push to every open document's paintable.
    let apply_pixel_view: Rc<dyn Fn(&crate::settings::PixelViewSettings)> = {
        let manager = Rc::clone(manager);
        Rc::new(move |pv| {
            for s in manager.sessions.borrow().iter() {
                s.apply_pixel_view(pv);
            }
        })
    };
    apply_pixel_view(&crate::settings::AppSettings::load().pixel_view);

    // Preferences.
    {
        let win = root.clone();
        let apply_dec = Rc::clone(apply_decorations);
        let apply_pv = Rc::clone(&apply_pixel_view);
        let action = gio::SimpleAction::new("preferences", None);
        action.connect_activate(move |_, _| {
            preferences_window::show(&win, Rc::clone(&apply_dec), Rc::clone(&apply_pv));
        });
        app.add_action(&action);
    }

    // Export As (active document).
    {
        let manager = Rc::clone(manager);
        let win = root.clone();
        let action = gio::SimpleAction::new("export-as", None);
        action.connect_activate(move |_, _| {
            if let Some(s) = manager.active() {
                crate::export_window::show(&win, &s.viewport.canvas());
            }
        });
        app.add_action(&action);
    }

    // Brush manager (global brush library).
    {
        let win = root.clone();
        let brush_engine = global.brush_engine.clone();
        let default_brush_name = global.default_brush_name.clone();
        let action = gio::SimpleAction::new("brush-manager", None);
        action.connect_activate(move |_, _| {
            crate::brush_manager::show(&win, &brush_engine, default_brush_name.clone());
        });
        app.add_action(&action);
    }

    // Adjustment layers (create / edit non-destructive effects).
    {
        let manager = Rc::clone(manager);
        let win = root.clone();
        let toaster = global.toaster.clone();
        let action = gio::SimpleAction::new("layer-add-adjustment", None);
        action.connect_activate(move |_, _| {
            let Some(s) = manager.active() else { return };
            let ctx = crate::adjustments::AdjustmentContext {
                window: win.clone(),
                canvas: s.viewport.canvas(),
                redraw: s.viewport.redraw_handle(),
                history: Rc::clone(&s.history),
                toaster: toaster.clone(),
                refresh_layers: Rc::clone(&s.refresh_layers),
            };
            crate::adjustments::add_or_edit(&ctx);
        });
        app.add_action(&action);
    }

    // Filters (build a context from the active document on each invocation).
    {
        let filter_actions: &[(&str, fn(&crate::filters::FilterContext))] = &[
            ("filter-hsv", crate::filters::show_hsv),
            ("filter-invert", crate::filters::show_invert),
            ("filter-blur", crate::filters::show_blur),
            ("filter-sharpen", crate::filters::show_sharpen),
        ];
        for &(name, handler) in filter_actions {
            let manager = Rc::clone(manager);
            let win = root.clone();
            let toaster = global.toaster.clone();
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_, _| {
                let Some(s) = manager.active() else { return };
                let ctx = crate::filters::FilterContext {
                    window: win.clone(),
                    canvas: s.viewport.canvas(),
                    redraw: s.viewport.redraw_handle(),
                    history: Rc::clone(&s.history),
                    toaster: toaster.clone(),
                    refresh_layers: Rc::clone(&s.refresh_layers),
                    selected_ids: Rc::clone(&s.selected_layer_ids),
                };
                handler(&ctx);
            });
            app.add_action(&action);
        }
    }
}

/// Whether the window's focused widget is an editable text field (entry, spin
/// button, editable label). Used to avoid intercepting editing keys like Delete
/// in the capture-phase key handler.
fn focus_is_text_editable(win: &adw::ApplicationWindow) -> bool {
    gtk::prelude::GtkWindowExt::focus(win)
        .is_some_and(|w: gtk::Widget| w.is::<gtk::Editable>())
}

/// Window-level key handler: Enter/Escape drive the transform tool, Escape
/// clears a selection. All routed to the active document.
fn install_key_handler(
    root: &adw::ApplicationWindow,
    manager: &Rc<TabManager>,
    global: &GlobalState,
) {
    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
    let tools = global.tools.clone();
    let manager = Rc::clone(manager);
    key_ctrl.connect_key_pressed(move |_, keyval, _, state| {
        let Some(session) = manager.active() else {
            return glib::Propagation::Proceed;
        };
        // While a text box is being edited, the editor swallows every key so
        // no application keybind (tool letters, etc.) fires mid-typing.
        if session.text_edit.is_active() {
            let _ = session.text_edit.handle_key(keyval, state);
            return glib::Propagation::Stop;
        }
        // Escape leaves component edit mode first, regardless of active tool.
        if keyval == gdk::Key::Escape && session.escape_component_edit() {
            return glib::Propagation::Stop;
        }
        let active = tools.active.get();

        // Delete: erase the in-selection pixels, or remove the active layer.
        // The layer-delete action itself cancels any in-progress transform
        // first, so it's safe to route both the key and the layers context menu
        // through it. This capture-phase handler runs before focused widgets, so
        // skip it while a text field (layer rename, numeric inputs) has focus.
        if matches!(keyval, gdk::Key::Delete | gdk::Key::KP_Delete)
            && !focus_is_text_editable(&manager.root)
        {
            if session.delete_selection() {
                return glib::Propagation::Stop;
            }
            app_handle().activate_action("layer-delete", None);
            return glib::Propagation::Stop;
        }

        if active == Tool::Transform {
            return match keyval {
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    (session.transform_apply)();
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    (session.transform_cancel)();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            };
        }
        if keyval == gdk::Key::Escape {
            // Cancel whatever the active tool has in progress.
            if active == Tool::Crop {
                session.cancel_crop();
                manager.set_active_tool(Tool::Cursor);
                return glib::Propagation::Stop;
            }
            if matches!(active, Tool::Selection(_)) {
                session.escape_deselect();
                return glib::Propagation::Stop;
            }
        }
        glib::Propagation::Proceed
    });
    root.add_controller(key_ctrl);
}
