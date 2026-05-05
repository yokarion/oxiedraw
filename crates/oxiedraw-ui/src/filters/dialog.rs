//! Reusable non-blocking filter popup: an `adw::Window` with a header bar
//! carrying a Cancel button (start) and an Apply button (end), plus an empty
//! content box the caller fills with the filter's controls.
//!
//! The window is non-modal so the canvas stays interactive and the live
//! preview is visible behind it. Apply commits; Cancel or closing the window
//! (button, Esc, or the title-bar close) drops the preview. A guard makes the
//! close-request handler invoke the cancel callback only when Apply did not
//! already close the window.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use relm4::gtk;

const CONTENT_MARGIN: i32 = 16;

pub(super) struct Dialog {
    pub window: adw::Window,
    /// Vertical box the caller appends controls to.
    pub content: gtk::Box,
}

pub(super) fn build(
    parent: &adw::ApplicationWindow,
    title: &str,
    on_apply: Rc<dyn Fn()>,
    on_cancel: Rc<dyn Fn()>,
) -> Dialog {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(false)
        .title(title)
        .default_width(420)
        .resizable(true)
        .build();

    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();

    let cancel_btn = gtk::Button::with_label("Cancel");
    let apply_btn = gtk::Button::with_label("Apply");
    apply_btn.add_css_class("suggested-action");
    header.pack_start(&cancel_btn);
    header.pack_end(&apply_btn);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(CONTENT_MARGIN)
        .margin_bottom(CONTENT_MARGIN)
        .margin_start(CONTENT_MARGIN)
        .margin_end(CONTENT_MARGIN)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    // Applied-guard: when Apply closes the window the close-request handler
    // must not also run cancel (which would undo the just-committed work).
    let applied = Rc::new(Cell::new(false));

    {
        let window = window.clone();
        let applied = Rc::clone(&applied);
        apply_btn.connect_clicked(move |_| {
            applied.set(true);
            on_apply();
            window.close();
        });
    }
    {
        let window = window.clone();
        cancel_btn.connect_clicked(move |_| window.close());
    }
    window.connect_close_request(move |_| {
        if !applied.get() {
            on_cancel();
        }
        gtk::glib::Propagation::Proceed
    });

    // Esc closes the window (which runs the cancel path), since a plain
    // adw::Window does not close on Escape on its own.
    {
        let key = gtk::EventControllerKey::new();
        let window_c = window.clone();
        key.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::Escape {
                window_c.close();
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        window.add_controller(key);
    }

    Dialog { window, content }
}
