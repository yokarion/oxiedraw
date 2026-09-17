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
use crate::panels::tool_bar;
use crate::{actions, preferences_window, top_bar};

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
    #[allow(dead_code)]
    manager: Rc<TabManager>,
    #[allow(dead_code)]
    layouts: Rc<crate::dock::manager::LayoutManager>,
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
                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    append: &top_bar_widget,

                    append: &dock_widget,
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
        load_chrome_css();
        crate::panels::tool_windows::load_css();
        preferences_window::load_keybind_css();

        let mut settings = crate::settings::AppSettings::load();
        if settings.layout.sanitize() {
            settings.save();
        }
        let history_capacity = settings.history.capacity;
        let global = GlobalState::new();

        let set_active_tool_late: SetActiveToolSlot = Rc::new(RefCell::new(None));

        let on_change_for_lb: Rc<dyn Fn(Tool)> = {
            let late = Rc::clone(&set_active_tool_late);
            Rc::new(move |t| {
                if let Some(f) = late.borrow().as_ref() {
                    f(t);
                }
            })
        };
        let (tool_bar_widget, set_left_bar) = tool_bar::build(&global.tools, &on_change_for_lb);
        let set_left_bar: Rc<dyn Fn(Tool)> = Rc::new(set_left_bar);

        let tab_view = adw::TabView::builder().hexpand(true).vexpand(true).build();
        let tab_bar = adw::TabBar::builder().view(&tab_view).build();
        let canvas_area = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();
        canvas_area.append(&tab_bar);
        canvas_area.append(&tab_view);

        let dock = crate::dock::DockHost::new(
            settings.layout.active(),
            canvas_area.upcast_ref::<gtk::Widget>(),
        );
        let dock_widget = dock.widget();
        let layouts = crate::dock::manager::LayoutManager::new(&dock, settings.layout.clone());

        let (top_bar_widget, apply_decorations) =
            top_bar::build(&crate::dock::selector::build(&layouts, &root));
        let apply_decorations: Rc<dyn Fn(bool)> = Rc::new(apply_decorations);

        let widgets = view_output!();

        global.toaster.bind(widgets.toast_overlay.clone());

        let manager = Rc::new(TabManager {
            global: global.clone(),
            set_active_tool_late: Rc::clone(&set_active_tool_late),
            set_left_bar,
            tab_view,
            dock: Rc::clone(&dock),
            layouts: Rc::clone(&layouts),
            sessions: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            root: root.clone(),
            history_capacity,
            untitled_counter: Cell::new(0),
            last_autosave: Cell::new(std::time::Instant::now()),
        });

        {
            let manager_c = Rc::clone(&manager);
            let setter: Rc<dyn Fn(Tool)> = Rc::new(move |t| manager_c.set_active_tool(t));
            *set_active_tool_late.borrow_mut() = Some(setter);
        }

        {
            let manager = Rc::downgrade(&manager);
            let tool_bar_widget = tool_bar_widget.upcast::<gtk::Widget>();
            let refill: Rc<dyn Fn(&crate::dock::DockHost)> = Rc::new(move |host| {
                host.fill(crate::layout::PanelId::ToolBar, &tool_bar_widget);
                if let Some(manager) = manager.upgrade() {
                    manager.fill_document_panels();
                }
            });
            dock.set_refill(Rc::clone(&refill));
            refill(&dock);
        }

        // Weak, like every hook the dock holds: the layouts own the dock, so a
        // strong hold back is a cycle neither end gets out of.
        {
            let layouts = Rc::downgrade(&layouts);
            dock.set_resize_handler(Rc::new(move |path, px| {
                if let Some(layouts) = layouts.upgrade() {
                    layouts.store_split_size(&path, px);
                }
            }));
        }

        {
            let dock_for_leaves = Rc::downgrade(&dock);
            let dock_for_floats = Rc::downgrade(&dock);
            let manager_for_floats = Rc::downgrade(&manager);
            let layouts_for_read = Rc::downgrade(&layouts);
            let layouts_for_write = Rc::downgrade(&layouts);
            dock.edit.set_hooks(crate::dock::edit_overlay::EditHooks {
                layout: Rc::new(move || {
                    layouts_for_read
                        .upgrade()
                        .map(|layouts| layouts.current_layout())
                        .unwrap_or_default()
                }),
                leaves: Rc::new(move || {
                    dock_for_leaves
                        .upgrade()
                        .map(|dock| dock.leaves())
                        .unwrap_or_default()
                }),
                handles: {
                    let dock = Rc::downgrade(&dock);
                    Rc::new(move || {
                        dock.upgrade().map(|dock| dock.handles()).unwrap_or_default()
                    })
                },
                carry_begin: {
                    let dock_for_carry = Rc::downgrade(&dock);
                    let manager = Rc::downgrade(&manager);
                    Rc::new(move |movable| {
                        let dock = dock_for_carry.upgrade()?;
                        let widget = match movable {
                            crate::dock::edit_overlay::Movable::Panel(id) => {
                                dock.panel_widget(id)?
                            }
                            crate::dock::edit_overlay::Movable::Float(id) => manager
                                .upgrade()?
                                .active()?
                                .tool_windows
                                .visible_frames()
                                .into_iter()
                                .find(|(window, ..)| *window == id)
                                .map(|(_, widget, _)| widget)?,
                        };
                        dock.carry_begin(&widget)
                    })
                },
                carry_to: {
                    let dock = Rc::downgrade(&dock);
                    Rc::new(move |rect| {
                        if let Some(dock) = dock.upgrade() {
                            dock.carry_to(rect);
                        }
                    })
                },
                carry_end: {
                    let dock = Rc::downgrade(&dock);
                    Rc::new(move || {
                        if let Some(dock) = dock.upgrade() {
                            dock.carry_end();
                        }
                    })
                },
                floats: Rc::new(move || {
                    let (Some(dock), Some(manager)) =
                        (dock_for_floats.upgrade(), manager_for_floats.upgrade())
                    else {
                        return Vec::new();
                    };
                    let Some(session) = manager.active() else {
                        return Vec::new();
                    };
                    session
                        .tool_windows
                        .visible_frames()
                        .into_iter()
                        .filter_map(|(id, widget, anchor)| {
                            dock.bounds_of(&widget).map(|rect| (id, rect, anchor))
                        })
                        .collect()
                }),
                float_natural: {
                    let manager = Rc::downgrade(&manager);
                    Rc::new(move |window| {
                        manager
                            .upgrade()?
                            .active()?
                            .tool_windows
                            .natural_size(window)
                    })
                },
                float_resize: {
                    let manager = Rc::downgrade(&manager);
                    Rc::new(move |window, size| {
                        if let Some(session) = manager.upgrade().and_then(|m| m.active()) {
                            session.tool_windows.set_float_size(window, size);
                        }
                    })
                },
                request: Rc::new(move |request| match (layouts_for_write.upgrade(), request) {
                    (None, _) => {}
                    (Some(layouts), crate::dock::edit_overlay::EditRequest::Remove(panel)) => {
                        layouts.set_panel_visible(panel, false);
                    }
                    (
                        Some(layouts),
                        crate::dock::edit_overlay::EditRequest::Move { panel, target },
                    ) => {
                        layouts.edit(|layout| target.apply(&mut layout.root, panel));
                    }
                    (
                        Some(layouts),
                        crate::dock::edit_overlay::EditRequest::Anchor { window, anchor },
                    ) => {
                        layouts.set_float_anchor(window, anchor);
                    }
                    (
                        Some(layouts),
                        crate::dock::edit_overlay::EditRequest::Resize { window, size },
                    ) => {
                        layouts.set_float_size(window, size);
                    }
                }),
            });
        }
        {
            let dock = Rc::downgrade(&dock);
            layouts.connect_edit_mode(Rc::new(move |on| {
                if let Some(dock) = dock.upgrade() {
                    dock.edit.set_active(on);
                }
            }));
        }

        {
            let manager = Rc::downgrade(&manager);
            layouts.set_reanchor(Rc::new(move |layout| {
                let Some(manager) = manager.upgrade() else {
                    return;
                };
                for session in manager.sessions.borrow().iter() {
                    session.tool_windows.apply_anchors(layout);
                }
            }));
        }

        manager.connect_tab_signals();
        manager.register_actions(&app_handle());
        manager.start_autosave_timer();

        register_window_actions(&manager, &root, &global, &apply_decorations);

        install_key_handler(&root, &manager, &global);

        {
            let manager = Rc::clone(&manager);
            root.connect_close_request(move |_| manager.on_window_close_request());
        }

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

        let model = Self { manager, layouts };
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {}
    }
}

fn app_handle() -> gtk::Application {
    gtk::gio::Application::default()
        .and_then(|a| a.downcast::<gtk::Application>().ok())
        .expect("default gtk::Application")
}

fn register_window_actions(
    manager: &Rc<TabManager>,
    root: &adw::ApplicationWindow,
    global: &GlobalState,
    apply_decorations: &Rc<dyn Fn(bool)>,
) {
    use gtk::gio;
    let app = app_handle();

    actions::register(manager.active_viewport_provider());

    let apply_pixel_view: Rc<dyn Fn(&crate::settings::PixelViewSettings)> = {
        let manager = Rc::clone(manager);
        Rc::new(move |pv| {
            for s in manager.sessions.borrow().iter() {
                s.apply_pixel_view(pv);
            }
        })
    };
    apply_pixel_view(&crate::settings::AppSettings::load().pixel_view);

    {
        let win = root.clone();
        let apply_dec = Rc::clone(apply_decorations);
        let apply_pv = Rc::clone(&apply_pixel_view);
        let autosave = global.autosave.clone();
        let action = gio::SimpleAction::new("preferences", None);
        action.connect_activate(move |_, _| {
            preferences_window::show(
                &win,
                Rc::clone(&apply_dec),
                Rc::clone(&apply_pv),
                autosave.clone(),
            );
        });
        app.add_action(&action);
    }

    {
        let manager = Rc::clone(manager);
        let win = root.clone();
        let action = gio::SimpleAction::new("export-as", None);
        action.connect_activate(move |_, _| {
            if let Some(s) = manager.active() {
                (s.liquify_flush)();
                crate::export_window::show(&win, &s.viewport.canvas());
            }
        });
        app.add_action(&action);
    }

    {
        use oxiedraw_core::color::ColorSlot;
        let colors = global.colors.clone();
        let action = gio::SimpleAction::new("swap-colors", None);
        action.connect_activate(move |_, _| {
            let next = match colors.selected.get() {
                ColorSlot::Primary => ColorSlot::Secondary,
                ColorSlot::Secondary => ColorSlot::Primary,
            };
            colors.selected.set(next);
            colors.notify_changed();
        });
        app.add_action(&action);
    }

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
                create_layer: Rc::clone(&s.create_adjustment_layer),
            };
            crate::adjustments::add_or_edit(&ctx);
        });
        app.add_action(&action);
    }

    {
        let filter_actions: &[(&str, fn(&crate::filters::FilterContext))] = &[
            ("filter-hsv", crate::filters::show_hsv),
            ("filter-curves", crate::filters::show_curves),
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

fn focus_is_text_editable(win: &adw::ApplicationWindow) -> bool {
    gtk::prelude::GtkWindowExt::focus(win)
        .is_some_and(|w: gtk::Widget| w.is::<gtk::Editable>())
}

// Capture phase, so every branch here must stand down while a text field has
// focus - otherwise Enter, Escape and Delete are taken out from under it.
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
        if session.text_edit.is_active() {
            let _ = session.text_edit.handle_key(keyval, state);
            return glib::Propagation::Stop;
        }
        if keyval == gdk::Key::Escape && session.escape_component_edit() {
            return glib::Propagation::Stop;
        }
        let active = tools.active.get();

        if matches!(keyval, gdk::Key::Delete | gdk::Key::KP_Delete)
            && !focus_is_text_editable(&manager.root)
        {
            if session.delete_selection() {
                return glib::Propagation::Stop;
            }
            app_handle().activate_action("layer-delete", None);
            return glib::Propagation::Stop;
        }

        let typing = focus_is_text_editable(&manager.root);

        if active == Tool::Transform && !typing {
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
        if active == Tool::Liquify && !typing {
            return match keyval {
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    app_handle().activate_action("liquify-apply", None);
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    app_handle().activate_action("liquify-cancel", None);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            };
        }
        if active == Tool::Pattern && !typing {
            return match keyval {
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    (session.pattern_apply)();
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    (session.pattern_cancel)();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            };
        }
        if keyval == gdk::Key::Escape {
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

fn load_chrome_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".oxiedraw-chrome {
            background-color: @window_bg_color;
        }

        .oxiedraw-chrome list,
        .oxiedraw-chrome listview.view {
            background-color: transparent;
            color: inherit;
        }

        .oxiedraw-chrome:dir(ltr) {
            border-right: 1px solid color-mix(in srgb, currentColor 15%, transparent);
            border-left-style: none;
        }

        .oxiedraw-chrome:dir(rtl) {
            border-left: 1px solid color-mix(in srgb, currentColor 15%, transparent);
            border-right-style: none;
        }

        paned .oxiedraw-chrome {
            border-style: none;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
