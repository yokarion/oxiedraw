//! Export dialog: scale, format, preview, and saving.
//!
//! `show()` builds the dialog. Per-format option pages live in
//! `format_pages`, the cairo preview in `preview`, status labels in
//! `status`.

mod format_pages;
mod preview;
mod status;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::export::encode::{ExportError, export_pixels};
use oxiedraw_core::export::settings::{ExportFormat, ExportSettings};
use relm4::RelmWidgetExt as _;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::gio;
use relm4::gtk::glib;

use crate::settings::AppSettings;

use self::format_pages::{build_avif_page, build_jpeg_page, build_png_page, build_webp_page};
use self::preview::{build_preview_surface, draw_preview, format_alpha};
use self::status::{format_mime, format_scale, section_label, update_status_labels};

const SCALE_VALUES: &[f32] = &[0.125, 0.25, 0.5, 1.0, 1.5, 2.0, 4.0];
const SCALE_LABELS: &[&str] = &["0.125x", "0.25x", "0.5x", "1.0x", "1.5x", "2.0x", "4.0x"];

pub(crate) fn show(parent: &adw::ApplicationWindow, canvas: &Rc<RefCell<Canvas>>) {
    let canvas_size = canvas.borrow().size();
    let raw_bgra8 = match canvas.borrow_mut().read_pixels() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "export_window: read_pixels failed");
            return;
        }
    };
    let raw_bgra8 = Rc::new(raw_bgra8);

    let settings: Rc<RefCell<ExportSettings>> = Rc::new(RefCell::new(AppSettings::load().export));
    let preview_surface: Rc<RefCell<Option<gtk::cairo::ImageSurface>>> =
        Rc::new(RefCell::new(None));
    let preview_zoom: Rc<Cell<f64>> = Rc::new(Cell::new(1.0));
    let preview_pan: Rc<Cell<(f64, f64)>> = Rc::new(Cell::new((0.0, 0.0)));
    let pan_start: Rc<Cell<Option<(f64, f64, f64, f64)>>> = Rc::new(Cell::new(None));
    let preview_in_flight: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
    // Timer id for the debounce delay before spawning the encode thread.
    let preview_debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
    // Monotonically increasing; stale thread results whose generation no longer
    // matches are discarded without updating the UI.
    let preview_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
    // Timer id that updates the loading progress bar while encoding is in flight.
    let preview_pulse: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
    // Monotonic us when the encode thread currently in flight was spawned.
    let preview_encode_start: Rc<Cell<i64>> = Rc::new(Cell::new(0));
    // Precomputed estimate for the encode currently in flight, in us.
    let preview_estimate_us: Rc<Cell<i64>> = Rc::new(Cell::new(800_000));
    // Adaptive per-pixel encode rate in us/px; calibrated from measured render times.
    // 0.5 us/px ~= 1 s for a 1920x1080 image - a conservative starting point.
    let preview_est_us_per_px: Rc<Cell<f64>> = Rc::new(Cell::new(0.5));
    // Output pixel count for the encode currently in flight (used for calibration).
    let preview_out_pixels: Rc<Cell<f64>> = Rc::new(Cell::new(1.0));

    let win = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(1160)
        .default_height(740)
        .title("Export Image")
        .build();

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    header.set_show_start_title_buttons(false);
    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.add_css_class("flat");
    header.pack_start(&cancel_btn);
    let export_btn = gtk::Button::with_label("Export");
    export_btn.add_css_class("suggested-action");
    header.pack_end(&export_btn);
    root_box.append(&header);

    let progress_bar = gtk::ProgressBar::new();
    progress_bar.set_visible(false);
    root_box.append(&progress_bar);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_hexpand(true);
    paned.set_vexpand(true);
    paned.set_wide_handle(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    root_box.append(&paned);

    let left_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(440)
        .build();
    let left_box = gtk::Box::new(gtk::Orientation::Vertical, 20);
    left_box.set_margin_top(16);
    left_box.set_margin_bottom(16);
    left_box.set_margin_start(16);
    left_box.set_margin_end(16);
    left_scroll.set_child(Some(&left_box));
    paned.set_start_child(Some(&left_scroll));

    let right_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right_box.set_hexpand(true);
    right_box.set_vexpand(true);
    paned.set_end_child(Some(&right_box));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&root_box));
    win.set_content(Some(&toast_overlay));

    let status_dim_label = gtk::Label::new(None);
    status_dim_label.add_css_class("dim-label");
    status_dim_label.set_halign(gtk::Align::Start);
    let status_fmt_label = gtk::Label::new(None);
    status_fmt_label.add_css_class("dim-label");
    let status_size_label = gtk::Label::new(None);
    status_size_label.add_css_class("dim-label");
    status_size_label.set_halign(gtk::Align::End);
    status_size_label.set_hexpand(true);

    let drawing_area = gtk::DrawingArea::new();
    drawing_area.set_hexpand(true);
    drawing_area.set_vexpand(true);

    let preview_overlay = gtk::Overlay::new();
    preview_overlay.set_hexpand(true);
    preview_overlay.set_vexpand(true);
    preview_overlay.set_child(Some(&drawing_area));

    let loading_sub_lbl = gtk::Label::new(Some(""));
    loading_sub_lbl.add_css_class("dim-label");
    loading_sub_lbl.set_halign(gtk::Align::Start);

    let loading_card = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    loading_card.add_css_class("card");
    loading_card.set_margin_top(12);
    loading_card.set_margin_start(12);
    loading_card.set_halign(gtk::Align::Center);
    loading_card.set_valign(gtk::Align::Start);
    let loading_progress = gtk::ProgressBar::new();
    loading_progress.set_pulse_step(0.15);
    {
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 6);
        inner.set_margin_top(12);
        inner.set_margin_bottom(12);
        inner.set_margin_start(16);
        inner.set_margin_end(16);
        let top_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let spinner = gtk::Spinner::new();
        spinner.start();
        top_row.append(&spinner);
        let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let title_lbl = gtk::Label::new(Some("Rendering preview..."));
        title_lbl.add_css_class("heading");
        title_lbl.set_halign(gtk::Align::Start);
        text_box.append(&title_lbl);
        text_box.append(&loading_sub_lbl);
        top_row.append(&text_box);
        inner.append(&top_row);
        inner.append(&loading_progress);
        loading_card.append(&inner);
    }
    preview_overlay.add_overlay(&loading_card);

    let trigger_preview: Rc<dyn Fn()> = {
        let raw_c = Rc::clone(&raw_bgra8);
        let settings_c = Rc::clone(&settings);
        let surface_c = Rc::clone(&preview_surface);
        let da_c = drawing_area.clone();
        let loading_c = loading_card.clone();
        let sub_lbl_c = loading_sub_lbl.clone();
        let in_flight = Rc::clone(&preview_in_flight);
        let debounce = Rc::clone(&preview_debounce);
        let generation = Rc::clone(&preview_generation);
        let pulse = Rc::clone(&preview_pulse);
        let progress_c = loading_progress.clone();
        let encode_start = Rc::clone(&preview_encode_start);
        let estimate_us = Rc::clone(&preview_estimate_us);
        let est_per_px = Rc::clone(&preview_est_us_per_px);
        let out_pixels = Rc::clone(&preview_out_pixels);
        let cw = canvas_size.width;
        let ch = canvas_size.height;

        Rc::new(move || {
            // Cancel pending debounce and any stale idle poller.
            if let Some(id) = debounce.take() {
                id.remove();
            }
            if let Some(id) = in_flight.take() {
                id.remove();
            }

            {
                let s = settings_c.borrow();
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let dw = ((cw as f32 * s.scale).round() as u32).max(1);
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let dh = ((ch as f32 * s.scale).round() as u32).max(1);
                sub_lbl_c.set_label(&format!(
                    "Rasterizing {dw} x {dh} at {}",
                    format_scale(s.scale)
                ));
            }
            loading_c.set_visible(true);

            // Deterministic progress: elapsed / precomputed estimate.
            // One shared timer serves all rapid re-triggers.
            let prev_pulse = pulse.take();
            if prev_pulse.is_none() {
                let pb = progress_c.clone();
                let start_c = Rc::clone(&encode_start);
                let estimate_c = Rc::clone(&estimate_us);
                let id = glib::timeout_add_local(Duration::from_millis(80), move || {
                    let elapsed = glib::monotonic_time() - start_c.get();
                    #[allow(clippy::cast_precision_loss)]
                    let fraction =
                        (elapsed as f64 / estimate_c.get().max(1) as f64).clamp(0.0, 0.95);
                    pb.set_fraction(fraction);
                    glib::ControlFlow::Continue
                });
                pulse.set(Some(id));
            } else {
                pulse.set(prev_pulse);
            }

            // Bump generation so results from older threads get discarded.
            let preview_gen = generation.get().wrapping_add(1);
            generation.set(preview_gen);

            let raw_cc = Rc::clone(&raw_c);
            let settings_cc = Rc::clone(&settings_c);
            let surface_cc = Rc::clone(&surface_c);
            let da_cc = da_c.clone();
            let loading_cc = loading_c.clone();
            let in_flight_cc = Rc::clone(&in_flight);
            let debounce_cc = Rc::clone(&debounce);
            let generation_cc = Rc::clone(&generation);
            let pulse_cc = Rc::clone(&pulse);
            let encode_start_cc = Rc::clone(&encode_start);
            let estimate_us_cc = Rc::clone(&estimate_us);
            let est_per_px_cc = Rc::clone(&est_per_px);
            let out_pixels_cc = Rc::clone(&out_pixels);

            // 150 ms debounce: if the user keeps changing settings the timer
            // resets each time, so we only spawn one thread after they stop.
            let timer_id = glib::timeout_add_local_once(Duration::from_millis(150), move || {
                // Timer fired - clear our own slot.
                debounce_cc.set(None);

                let raw_snap: Vec<u8> = raw_cc.as_ref().clone();
                let settings_snap = settings_cc.borrow().clone();

                // Compute output pixel count for this specific encode so the
                // estimate scales correctly when the user changes scale/format.
                #[allow(clippy::cast_precision_loss)]
                let px = ((cw as f64 * settings_snap.scale as f64).round().max(1.0))
                    * ((ch as f64 * settings_snap.scale as f64).round().max(1.0));
                out_pixels_cc.set(px);
                // Recompute the estimate from the per-pixel rate calibrated by
                // the previous render - this way changing scale gives an
                // immediately accurate bar rather than reusing a stale number.
                estimate_us_cc.set((est_per_px_cc.get() * px).max(50_000.0) as i64);
                encode_start_cc.set(glib::monotonic_time());

                let (tx, rx) = mpsc::channel::<(Vec<u8>, u32, u32, bool)>();
                std::thread::spawn(move || {
                    let result = oxiedraw_core::export::encode::generate_preview_pixels(
                        &raw_snap,
                        cw,
                        ch,
                        &settings_snap,
                    );
                    let _ = tx.send(result);
                });

                let surface_ccc = Rc::clone(&surface_cc);
                let da_ccc = da_cc.clone();
                let loading_ccc = loading_cc.clone();
                let in_flight_ccc = Rc::clone(&in_flight_cc);
                let generation_ccc = Rc::clone(&generation_cc);
                let pulse_ccc = Rc::clone(&pulse_cc);
                let encode_start_ccc = Rc::clone(&encode_start_cc);
                let est_per_px_ccc = Rc::clone(&est_per_px_cc);
                let out_pixels_ccc = Rc::clone(&out_pixels_cc);

                let idle_id = glib::idle_add_local(move || match rx.try_recv() {
                    Ok((pixels, w, h, _)) => {
                        if generation_ccc.get() == preview_gen {
                            // Calibrate the per-pixel rate with an EWMA so that
                            // the next render at any scale gets a better estimate.
                            let elapsed = glib::monotonic_time() - encode_start_ccc.get();
                            let px = out_pixels_ccc.get();
                            if elapsed > 0 && px > 0.0 {
                                let measured = elapsed as f64 / px;
                                let smoothed = est_per_px_ccc.get() * 0.4 + measured * 0.6;
                                est_per_px_ccc.set(smoothed);
                            }

                            build_preview_surface(&pixels, w, h, &surface_ccc);
                            if let Some(id) = pulse_ccc.take() {
                                id.remove();
                            }
                            loading_ccc.set_visible(false);
                            da_ccc.queue_draw();
                        }
                        in_flight_ccc.set(None);
                        glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if generation_ccc.get() == preview_gen {
                            if let Some(id) = pulse_ccc.take() {
                                id.remove();
                            }
                            loading_ccc.set_visible(false);
                        }
                        in_flight_ccc.set(None);
                        glib::ControlFlow::Break
                    }
                });
                in_flight_cc.set(Some(idle_id));
            });
            debounce.set(Some(timer_id));
        })
    };

    let format_stack = gtk::Stack::new();
    format_stack.set_transition_type(gtk::StackTransitionType::None);
    format_stack.set_hhomogeneous(true);

    let (png_page, png_widgets) = build_png_page(&settings);
    format_stack.add_named(&png_page, Some("png"));
    let (jpeg_page, jpeg_widgets) = build_jpeg_page(&settings);
    format_stack.add_named(&jpeg_page, Some("jpeg"));
    let (webp_page, webp_widgets) = build_webp_page(&settings);
    format_stack.add_named(&webp_page, Some("webp"));
    let (avif_page, avif_widgets) = build_avif_page(&settings);
    format_stack.add_named(&avif_page, Some("avif"));

    left_box.append(&section_label("SCALING"));

    let scale_card = gtk::Box::new(gtk::Orientation::Vertical, 8);
    scale_card.add_css_class("card");
    scale_card.set_spacing(8);
    {
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 4);
        inner.set_margin_top(12);
        inner.set_margin_bottom(12);
        inner.set_margin_start(16);
        inner.set_margin_end(16);

        let top_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let scale_val_label = gtk::Label::new(Some("1.0x"));
        scale_val_label.add_css_class("title-1");
        scale_val_label.set_halign(gtk::Align::Start);
        scale_val_label.set_hexpand(true);
        top_row.append(&scale_val_label);
        let dim_label = gtk::Label::new(None);
        dim_label.add_css_class("dim-label");
        dim_label.set_halign(gtk::Align::End);
        top_row.append(&dim_label);
        inner.append(&top_row);

        {
            let s = settings.borrow();
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let dw = ((canvas_size.width as f32 * s.scale).round() as u32).max(1);
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let dh = ((canvas_size.height as f32 * s.scale).round() as u32).max(1);
            dim_label.set_label(&format!("{dw} x {dh} px"));
            scale_val_label.set_label(&format_scale(s.scale));
        }

        let scale_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 6.0, 1.0);
        scale_slider.set_draw_value(false);
        for (i, label) in SCALE_LABELS.iter().enumerate() {
            scale_slider.add_mark(i as f64, gtk::PositionType::Bottom, Some(label));
        }
        {
            let s = settings.borrow();
            let idx = SCALE_VALUES
                .iter()
                .position(|&v| (v - s.scale).abs() < 0.001)
                .unwrap_or(3);
            scale_slider.set_value(idx as f64);
        }
        inner.append(&scale_slider);
        scale_card.append(&inner);

        let settings_c = Rc::clone(&settings);
        let zoom_c = Rc::clone(&preview_zoom);
        let pan_c = Rc::clone(&preview_pan);
        let dim_label_c = dim_label.clone();
        let scale_val_label_c = scale_val_label.clone();
        let status_dim_c = status_dim_label.clone();
        let status_fmt_c = status_fmt_label.clone();
        let status_size_c = status_size_label.clone();
        let tp = Rc::clone(&trigger_preview);
        scale_slider.connect_value_changed(move |sl| {
            let idx = sl.value().round() as usize;
            let scale_val = SCALE_VALUES[idx.min(SCALE_VALUES.len() - 1)];
            settings_c.borrow_mut().scale = scale_val;
            save_settings(&settings_c.borrow());

            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let dw = ((canvas_size.width as f32 * scale_val).round() as u32).max(1);
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let dh = ((canvas_size.height as f32 * scale_val).round() as u32).max(1);
            dim_label_c.set_label(&format!("{dw} x {dh} px"));
            scale_val_label_c.set_label(&format_scale(scale_val));
            zoom_c.set(1.0);
            pan_c.set((0.0, 0.0));
            update_status_labels(
                &status_dim_c,
                &status_fmt_c,
                &status_size_c,
                canvas_size.width,
                canvas_size.height,
                &settings_c.borrow(),
            );
            tp.as_ref()();
        });
    }
    left_box.append(&scale_card);

    left_box.append(&section_label("OUTPUT FORMAT"));

    let format_btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    format_btn_box.add_css_class("linked");

    let png_btn = gtk::ToggleButton::with_label("PNG");
    let jpeg_btn = gtk::ToggleButton::with_label("JPEG");
    let webp_btn = gtk::ToggleButton::with_label("WebP");
    let avif_btn = gtk::ToggleButton::with_label("AVIF");
    jpeg_btn.set_group(Some(&png_btn));
    webp_btn.set_group(Some(&png_btn));
    avif_btn.set_group(Some(&png_btn));
    {
        let f = settings.borrow().format;
        match f {
            ExportFormat::Png => png_btn.set_active(true),
            ExportFormat::Jpeg => jpeg_btn.set_active(true),
            ExportFormat::Webp => webp_btn.set_active(true),
            ExportFormat::Avif => avif_btn.set_active(true),
        }
    }
    for btn in [&png_btn, &jpeg_btn, &webp_btn, &avif_btn] {
        btn.set_hexpand(true);
        format_btn_box.append(btn);
    }
    left_box.append(&format_btn_box);

    let tags_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    left_box.append(&tags_box);

    fn rebuild_tags(tags_box: &gtk::Box, format: ExportFormat) {
        while let Some(child) = tags_box.first_child() {
            tags_box.remove(&child);
        }
        for tag in format.tags() {
            let lbl = gtk::Label::new(Some(tag));
            lbl.add_css_class("export-tag");
            tags_box.append(&lbl);
        }
    }
    rebuild_tags(&tags_box, settings.borrow().format);

    let formats: &[(&gtk::ToggleButton, ExportFormat, &str)] = &[
        (&png_btn, ExportFormat::Png, "png"),
        (&jpeg_btn, ExportFormat::Jpeg, "jpeg"),
        (&webp_btn, ExportFormat::Webp, "webp"),
        (&avif_btn, ExportFormat::Avif, "avif"),
    ];
    for &(btn, fmt, stack_name) in formats {
        let settings_c = Rc::clone(&settings);
        let format_stack_c = format_stack.clone();
        let tags_box_c = tags_box.clone();
        let status_dim_c = status_dim_label.clone();
        let status_fmt_c = status_fmt_label.clone();
        let status_size_c = status_size_label.clone();
        let tp = Rc::clone(&trigger_preview);
        btn.connect_toggled(move |b| {
            if !b.is_active() {
                return;
            }
            settings_c.borrow_mut().format = fmt;
            save_settings(&settings_c.borrow());
            format_stack_c.set_visible_child_name(stack_name);
            rebuild_tags(&tags_box_c, fmt);
            update_status_labels(
                &status_dim_c,
                &status_fmt_c,
                &status_size_c,
                canvas_size.width,
                canvas_size.height,
                &settings_c.borrow(),
            );
            tp.as_ref()();
        });
    }

    let settings_label = section_label("FORMAT SETTINGS");
    left_box.append(&settings_label);
    {
        let f = settings.borrow().format;
        settings_label.set_label(&format!("{} SETTINGS", f.label()));
    }
    {
        let all_btns: Vec<(gtk::ToggleButton, ExportFormat)> = vec![
            (png_btn.clone(), ExportFormat::Png),
            (jpeg_btn.clone(), ExportFormat::Jpeg),
            (webp_btn.clone(), ExportFormat::Webp),
            (avif_btn.clone(), ExportFormat::Avif),
        ];
        for (btn, fmt) in all_btns {
            let lbl = settings_label.clone();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    lbl.set_label(&format!("{} SETTINGS", fmt.label()));
                }
            });
        }
    }
    {
        let f = settings.borrow().format;
        format_stack.set_visible_child_name(match f {
            ExportFormat::Png => "png",
            ExportFormat::Jpeg => "jpeg",
            ExportFormat::Webp => "webp",
            ExportFormat::Avif => "avif",
        });
    }
    left_box.append(&format_stack);

    let make_status_cb = {
        let settings_c = Rc::clone(&settings);
        let sd = status_dim_label.clone();
        let sf = status_fmt_label.clone();
        let ss = status_size_label.clone();
        let cw = canvas_size.width;
        let ch = canvas_size.height;
        let tp = Rc::clone(&trigger_preview);
        move || {
            update_status_labels(&sd, &sf, &ss, cw, ch, &settings_c.borrow());
            tp.as_ref()();
        }
    };
    png_widgets.connect_changed(Rc::new(make_status_cb.clone()));
    jpeg_widgets.connect_changed(Rc::new(make_status_cb.clone()));
    webp_widgets.connect_changed(Rc::new(make_status_cb.clone()));
    avif_widgets.connect_changed(Rc::new(make_status_cb));

    right_box.append(&preview_overlay);

    let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_bar.set_margin_top(6);
    status_bar.set_margin_bottom(6);
    status_bar.set_margin_start(12);
    status_bar.set_margin_end(12);
    {
        let dot = gtk::Label::new(Some("*"));
        dot.add_css_class("success");
        dot.inline_css("font-size: 10px;");
        status_bar.append(&dot);
    }
    status_bar.append(&status_dim_label);
    let sep2 = gtk::Label::new(Some("."));
    sep2.add_css_class("dim-label");
    status_bar.append(&sep2);
    status_bar.append(&status_fmt_label);
    status_bar.append(&status_size_label);
    right_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right_box.append(&status_bar);

    {
        let surface_c = Rc::clone(&preview_surface);
        let zoom_c = Rc::clone(&preview_zoom);
        let pan_c = Rc::clone(&preview_pan);
        let settings_c = Rc::clone(&settings);
        drawing_area.set_draw_func(move |_da, cr, w, h| {
            let show_alpha = format_alpha(&settings_c.borrow());
            draw_preview(cr, w, h, &surface_c, &zoom_c, &pan_c, show_alpha);
        });
    }

    {
        let ctrl = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        let zoom_c = Rc::clone(&preview_zoom);
        let da_c = drawing_area.clone();
        ctrl.connect_scroll(move |_, _dx, dy| {
            let factor = if dy < 0.0 { 1.1_f64 } else { 1.0 / 1.1 };
            zoom_c.set((zoom_c.get() * factor).clamp(0.01, 64.0));
            da_c.queue_draw();
            glib::Propagation::Stop
        });
        drawing_area.add_controller(ctrl);
    }
    {
        let gesture = gtk::GestureDrag::new();
        gesture.set_button(2);
        let pan_c = Rc::clone(&preview_pan);
        let pan_start_c = Rc::clone(&pan_start);
        gesture.connect_drag_begin(move |_, x, y| {
            let (px, py) = pan_c.get();
            pan_start_c.set(Some((x, y, px, py)));
        });
        let pan_c2 = Rc::clone(&preview_pan);
        let pan_start_c2 = Rc::clone(&pan_start);
        let da_c2 = drawing_area.clone();
        gesture.connect_drag_update(move |_, dx, dy| {
            if let Some((_, _, ox, oy)) = pan_start_c2.get() {
                pan_c2.set((ox + dx, oy + dy));
                da_c2.queue_draw();
            }
        });
        drawing_area.add_controller(gesture);

        let gesture2 = gtk::GestureDrag::new();
        gesture2.set_button(1);
        let pan_c3 = Rc::clone(&preview_pan);
        let pan_start_c3 = Rc::clone(&pan_start);
        gesture2.connect_drag_begin(move |_, x, y| {
            let (px, py) = pan_c3.get();
            pan_start_c3.set(Some((x, y, px, py)));
        });
        let pan_c4 = Rc::clone(&preview_pan);
        let pan_start_c4 = Rc::clone(&pan_start);
        let da_c4 = drawing_area.clone();
        gesture2.connect_drag_update(move |_, dx, dy| {
            if let Some((_, _, ox, oy)) = pan_start_c4.get() {
                pan_c4.set((ox + dx, oy + dy));
                da_c4.queue_draw();
            }
        });
        drawing_area.add_controller(gesture2);
    }

    trigger_preview.as_ref()();

    update_status_labels(
        &status_dim_label,
        &status_fmt_label,
        &status_size_label,
        canvas_size.width,
        canvas_size.height,
        &settings.borrow(),
    );

    {
        let win_c = win.clone();
        cancel_btn.connect_clicked(move |_| {
            win_c.close();
        });
    }

    {
        let settings_c = Rc::clone(&settings);
        let raw_c = Rc::clone(&raw_bgra8);
        let export_btn_c = export_btn.clone();
        let progress_c = progress_bar.clone();
        let toast_c = toast_overlay.clone();
        let win_c = win.clone();

        export_btn.connect_clicked(move |_btn| {
            let settings_snap = settings_c.borrow().clone();
            let ext = settings_snap.format.extension();

            let filter = gtk::FileFilter::new();
            let mime = format_mime(settings_snap.format);
            filter.set_name(Some(&format!("{} Image", settings_snap.format.label())));
            filter.add_pattern(&format!("*.{ext}"));
            filter.add_mime_type(mime);
            let store = gio::ListStore::new::<gtk::FileFilter>();
            store.append(&filter);

            let dialog = gtk::FileDialog::new();
            dialog.set_title("Export Image");
            dialog.set_modal(true);
            dialog.set_filters(Some(&store));
            dialog.set_initial_name(Some(&format!("untitled.{ext}")));

            let btn_c = export_btn_c.clone();
            let progress_cc = progress_c.clone();
            let toast_cc = toast_c.clone();
            let raw_cc = Rc::clone(&raw_c);
            let settings_cc = settings_snap.clone();
            let cw = canvas_size.width;
            let ch = canvas_size.height;

            dialog.save(Some(&win_c), None::<&gio::Cancellable>, move |result| {
                let Ok(file) = result else { return };
                let Some(mut path) = file.path() else { return };

                if path.extension().is_none_or(|e| e != ext) {
                    path.set_extension(ext);
                }

                run_export(
                    path,
                    raw_cc.as_ref().clone(),
                    cw,
                    ch,
                    settings_cc,
                    btn_c,
                    progress_cc,
                    toast_cc,
                );
            });
        });
    }

    load_export_css();
    win.present();
}

fn run_export(
    path: PathBuf,
    raw_bgra8: Vec<u8>,
    canvas_w: u32,
    canvas_h: u32,
    settings: ExportSettings,
    export_btn: gtk::Button,
    progress_bar: gtk::ProgressBar,
    toast_overlay: adw::ToastOverlay,
) {
    export_btn.set_sensitive(false);
    export_btn.set_label("Exporting...");
    progress_bar.set_visible(true);
    progress_bar.pulse();

    let pulse_handle: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
    {
        let pb = progress_bar.clone();
        let id = glib::timeout_add_local(Duration::from_millis(80), move || {
            pb.pulse();
            glib::ControlFlow::Continue
        });
        pulse_handle.set(Some(id));
    }

    let (tx, rx) = mpsc::channel::<Result<PathBuf, String>>();

    std::thread::spawn(move || {
        let result = export_pixels(&raw_bgra8, canvas_w, canvas_h, &settings, &path)
            .map(|()| path.clone())
            .map_err(|e: ExportError| e.to_string());
        let _ = tx.send(result);
    });

    let btn = export_btn.clone();
    let pb = progress_bar.clone();
    let toast = toast_overlay.clone();
    let ph = pulse_handle.clone();

    glib::idle_add_local(move || match rx.try_recv() {
        Ok(result) => {
            if let Some(id) = ph.take() {
                id.remove();
            }
            pb.set_visible(false);
            pb.set_fraction(0.0);

            match result {
                Ok(exported_path) => {
                    btn.set_label("Exported!");
                    btn.set_sensitive(true);

                    let path_str = exported_path.display().to_string();
                    let t = adw::Toast::new(&format!("File exported at {path_str}"));
                    t.set_button_label(Some("Open"));
                    let folder = exported_path
                        .parent().map_or_else(|| exported_path.clone(), |p: &std::path::Path| p.to_path_buf());
                    t.connect_button_clicked(move |_| {
                        let uri = format!("file://{}", folder.display());
                        if let Err(e) = gio::AppInfo::launch_default_for_uri(
                            &uri,
                            None::<&gio::AppLaunchContext>,
                        ) {
                            tracing::warn!(error = %e, "failed to open folder");
                        }
                    });
                    t.set_timeout(5);
                    toast.add_toast(t);

                    let btn2 = btn.clone();
                    glib::timeout_add_local_once(Duration::from_secs(2), move || {
                        btn2.set_label("Export");
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "export failed");
                    btn.set_label("Export");
                    btn.set_sensitive(true);

                    let t = adw::Toast::new(&format!("Export failed: {e}"));
                    t.set_timeout(4);
                    toast.add_toast(t);
                }
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => {
            if let Some(id) = ph.take() {
                id.remove();
            }
            pb.set_visible(false);
            glib::ControlFlow::Break
        }
    });
}

pub(super) fn save_settings(settings: &ExportSettings) {
    let mut app = AppSettings::load();
    app.export = settings.clone();
    app.save();
}

fn load_export_css() {
    let css = r"
.export-tag {
    border: 1px solid alpha(@borders, 0.7);
    border-radius: 100px;
    padding: 2px 10px;
    font-size: 12px;
    background: alpha(@card_bg_color, 0.5);
}
";
    let provider = gtk::CssProvider::new();
    provider.load_from_string(css);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
