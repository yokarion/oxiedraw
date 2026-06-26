mod color_picker;
mod components;
mod crop_properties;
mod layers;
mod text_properties;

use std::cell::RefCell;
use std::rc::Rc;

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::ColorState;
use oxiedraw_core::components::ComponentLibrary;
use oxiedraw_core::document::LayerState;
use oxiedraw_core::history::HistoryStack;
use oxiedraw_core::text::fonts::TextEngine;
use oxiedraw_core::tools::{CropState, Tool, ToolState};
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::canvas::RedrawHandle;

const WIDTH: i32 = 300;
const STACK_NORMAL: &str = "normal";
const STACK_CROP: &str = "crop";

/// Build the right sidebar.
///
/// Returns the widget, a setter that switches the visible panel when the
/// active tool changes, and a callback to rebuild the layers list from canvas
/// state (call after undo/redo).
pub(crate) fn build(
    colors: ColorState,
    layers: &LayerState,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    crop: &CropState,
    tools: &ToolState,
    layer_clipboard: &Rc<RefCell<Option<crate::clipboard::LayerClipboard>>>,
    toaster: &crate::toaster::Toaster,
    select_layer_content: &Rc<dyn Fn(usize)>,
    history: &Rc<RefCell<HistoryStack>>,
    components: &Rc<RefCell<ComponentLibrary>>,
    on_edit_component: &Rc<dyn Fn(String)>,
    component_exit: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
    text_edit_slot: &Rc<RefCell<Option<crate::text_edit::TextEdit>>>,
    text_engine: &Rc<RefCell<TextEngine>>,
    font_previews: &crate::font_previews::FontPreviews,
    prepare_delete: &Rc<dyn Fn() -> bool>,
    prepare_reorder: &Rc<dyn Fn()>,
) -> (
    gtk::Widget,
    Rc<dyn Fn(Tool)>,
    Rc<dyn Fn()>,
    Rc<dyn Fn() -> Vec<String>>,
    Rc<dyn Fn()>,
    Rc<dyn Fn()>,
    Rc<dyn Fn(Option<String>)>,
    Rc<dyn Fn()>,
    Rc<dyn Fn()>,
) {
    // Normal panel: [editing-text panel] + color picker + layer list.
    let normal_pane = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .wide_handle(true)
        .build();
    normal_pane.set_start_child(Some(&color_picker::build(colors)));

    // "Editing text" properties panel (hidden until a text box is edited).
    let (text_panel, refresh_text_panel) =
        text_properties::build(text_edit_slot, text_engine, font_previews);

    let (
        layers_widget,
        refresh_layers,
        selected_ids,
        reinstall_actions,
        refresh_components,
        set_component_edit,
        begin_rename,
    ) = layers::build(
        layers,
        canvas,
        redraw,
        layer_clipboard,
        toaster,
        select_layer_content,
        history,
        components,
        on_edit_component,
        component_exit,
        prepare_delete,
        prepare_reorder,
    );
    normal_pane.set_end_child(Some(&layers_widget));

    // Crop properties panel.
    let crop_panel = crop_properties::build(crop);

    // The normal page puts the (collapsible) text panel above the rest, split
    // by a drag handle so its height is resizable. The handle is hidden while
    // the text panel is, since GtkPaned drops it when a child isn't visible.
    normal_pane.set_vexpand(true);
    let normal_root = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .wide_handle(true)
        .vexpand(true)
        .build();
    normal_root.set_start_child(Some(&text_panel));
    normal_root.set_end_child(Some(&normal_pane));

    // When the text panel is shown, size the divider to its natural height so
    // every control is visible by default (the user can then drag to resize).
    {
        let pane = normal_root.clone();
        text_panel.connect_map(move |w| {
            let (_, natural, _, _) = w.measure(gtk::Orientation::Vertical, WIDTH);
            pane.set_position(natural);
        });
    }

    let stack = gtk::Stack::builder()
        .width_request(WIDTH)
        .transition_type(gtk::StackTransitionType::None)
        .build();
    stack.add_named(&normal_root, Some(STACK_NORMAL));
    stack.add_named(&crop_panel, Some(STACK_CROP));

    let initial = if tools.active.get() == Tool::Crop {
        STACK_CROP
    } else {
        STACK_NORMAL
    };
    stack.set_visible_child_name(initial);

    let setter: Rc<dyn Fn(Tool)> = {
        let stack = stack.clone();
        Rc::new(move |t: Tool| {
            let page = if t == Tool::Crop {
                STACK_CROP
            } else {
                STACK_NORMAL
            };
            stack.set_visible_child_name(page);
        })
    };

    // `set_component_edit` (the banner/tab gating) comes straight from the
    // layers panel - the edit banner now lives in the tab-bar slot there.
    (
        stack.upcast(),
        setter,
        refresh_layers,
        selected_ids,
        reinstall_actions,
        refresh_components,
        set_component_edit,
        begin_rename,
        refresh_text_panel,
    )
}
