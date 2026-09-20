//! Export Recording: turns the recorded frames into an H.264 video or a folder
//! of PNG frames. The last choices are remembered in the app settings.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use oxiedraw_core::enum_meta::EnumMeta;
use oxiedraw_core::recording::RecordingError;
use oxiedraw_core::recording::export::{
    Background, Compression, ExportJob, ExportOptions, ExportProgress, ExportTarget, FrameRate, OutputFormat,
};
use relm4::gtk::{gio, glib};

use super::settings_window::{combo_row, switch_row};
use super::{Recorder, find_ffmpeg, group_thousands};
use crate::session::DocumentSession;
use crate::settings::AppSettings;

pub(super) fn show(parent: &adw::ApplicationWindow, session: &Rc<DocumentSession>) {
    let recorder = Rc::clone(&session.recording);
    let title = session.title.borrow().clone();
    let options = Rc::new(Cell::new(AppSettings::load().recording_export));
    let opts = options.get();

    let win = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(540)
        .default_height(660)
        .title("Export Recording")
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    header.set_show_start_title_buttons(false);
    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.add_css_class("flat");
    header.pack_start(&cancel_btn);
    let export_btn = gtk::Button::with_label("Export");
    export_btn.add_css_class("suggested-action");
    header.pack_end(&export_btn);
    root.append(&header);

    let progress_bar = gtk::ProgressBar::new();
    progress_bar.set_visible(false);
    root.append(&progress_bar);

    let info = gtk::Label::builder()
        .xalign(0.0)
        .margin_top(12)
        .margin_start(16)
        .margin_end(16)
        .build();
    info.add_css_class("dim-label");
    root.append(&info);

    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    root.append(&page);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&root));
    win.set_content(Some(&toasts));

    let format_group = adw::PreferencesGroup::builder().title("Format").build();
    let format_row = combo_row("Export format", &OutputFormat::labels(), opts.format.to_index());
    format_row.set_subtitle(opts.format.detail());
    let compression_row = combo_row("Compression", &Compression::labels(), opts.compression.to_index());
    let rate_row = combo_row("Frame rate", &FrameRate::labels(), opts.frame_rate.to_index());
    format_group.add(&format_row);
    format_group.add(&compression_row);
    format_group.add(&rate_row);
    page.add(&format_group);

    let canvas_group = adw::PreferencesGroup::builder().title("Canvas").build();
    let (canvas_row, include_canvas) = switch_row("Include canvas", opts.include_canvas, None);
    canvas_row.set_subtitle("Adds 20% of space around the artwork, in the canvas background colour");
    let background_row = adw::ComboRow::builder().title("Background").build();
    canvas_group.add(&canvas_row);
    canvas_group.add(&background_row);
    page.add(&canvas_group);

    let meta_group = adw::PreferencesGroup::builder().title("Metadata").build();
    let (meta_row, metadata) = switch_row(
        "Include Exif",
        opts.metadata,
        Some(
            "Adds the app name and version, when the recording started and ended, the number \
             of frames and the canvas size. PNG frames get it as an EXIF block, H.264 and \
             ProRes videos as their title and comment; an AVIF only keeps the start date. \
             Nothing about you or your computer is included.",
        ),
    );
    meta_group.add(&meta_row);
    page.add(&meta_group);

    // Video has no alpha channel, so its background list leaves Alpha out.
    let syncing = Rc::new(Cell::new(false));
    let fill_backgrounds = {
        let background_row = background_row.clone();
        let syncing = Rc::clone(&syncing);
        move |format: OutputFormat, current: Background| -> Background {
            let choices = Background::choices(format);
            let keep = if choices.contains(&current) { current } else { Background::White };
            let labels: Vec<&str> = choices.iter().map(|b| b.label()).collect();
            syncing.set(true);
            background_row.set_model(Some(&gtk::StringList::new(&labels)));
            background_row.set_selected(choices.iter().position(|b| *b == keep).unwrap_or(0) as u32);
            syncing.set(false);
            keep
        }
    };
    options.set(ExportOptions { background: fill_backgrounds(opts.format, opts.background), ..opts });

    let refresh_info: Rc<dyn Fn()> = {
        let recorder = Rc::clone(&recorder);
        let options = Rc::clone(&options);
        let info = info.clone();
        let export_btn = export_btn.clone();
        Rc::new(move || {
            let frames = recorder.stats().frames;
            let fps = options.get().frame_rate.fps();
            if frames == 0 {
                info.set_label("Nothing recorded yet");
            } else {
                #[allow(clippy::cast_precision_loss)]
                let secs = frames as f64 / f64::from(fps);
                info.set_label(&format!("{} frames - {secs:.1} s at {fps} fps", group_thousands(frames)));
            }
            export_btn.set_sensitive(frames > 0);
        })
    };
    refresh_info();

    let apply: Rc<dyn Fn()> = {
        let options = Rc::clone(&options);
        let refresh_info = Rc::clone(&refresh_info);
        let (format_row, compression_row, rate_row, background_row) =
            (format_row.clone(), compression_row.clone(), rate_row.clone(), background_row.clone());
        let (include_canvas, metadata) = (include_canvas.clone(), metadata.clone());
        let syncing = Rc::clone(&syncing);
        Rc::new(move || {
            if syncing.get() {
                return;
            }
            let format = OutputFormat::from_index(format_row.selected());
            format_row.set_subtitle(format.detail());
            let picked = Background::choices(format)
                .get(background_row.selected() as usize)
                .copied()
                .unwrap_or_default();
            let background = if format == options.get().format {
                picked
            } else {
                fill_backgrounds(format, options.get().background)
            };
            let next = ExportOptions {
                format,
                compression: Compression::from_index(compression_row.selected()),
                frame_rate: FrameRate::from_index(rate_row.selected()),
                include_canvas: include_canvas.is_active(),
                background,
                metadata: metadata.is_active(),
            };
            options.set(next);
            save_options(next);
            refresh_info();
        })
    };
    for row in [&format_row, &compression_row, &rate_row, &background_row] {
        let apply = Rc::clone(&apply);
        row.connect_selected_notify(move |_| apply());
    }
    for switch in [&include_canvas, &metadata] {
        let apply = Rc::clone(&apply);
        switch.connect_active_notify(move |_| apply());
    }

    let running: Rc<RefCell<Option<Arc<ExportProgress>>>> = Rc::new(RefCell::new(None));
    {
        let running = Rc::clone(&running);
        let win_c = win.clone();
        cancel_btn.connect_clicked(move |_| match running.borrow().as_ref() {
            Some(progress) => progress.cancel.store(true, Ordering::Relaxed),
            None => win_c.close(),
        });
    }
    {
        let running = Rc::clone(&running);
        win.connect_close_request(move |_| {
            if let Some(progress) = running.borrow().as_ref() {
                progress.cancel.store(true, Ordering::Relaxed);
            }
            glib::Propagation::Proceed
        });
    }

    let ui = Ui {
        win: win.clone(),
        export_btn: export_btn.clone(),
        progress_bar,
        info,
        page: page.clone(),
        toasts,
        running,
        refresh_info,
    };
    {
        let ui = Rc::new(ui);
        let options = Rc::clone(&options);
        export_btn.connect_clicked(move |_| {
            pick_target(&ui, &recorder, &title, options.get());
        });
    }

    win.present();
}

struct Ui {
    win: adw::Window,
    export_btn: gtk::Button,
    progress_bar: gtk::ProgressBar,
    info: gtk::Label,
    page: adw::PreferencesPage,
    toasts: adw::ToastOverlay,
    running: Rc<RefCell<Option<Arc<ExportProgress>>>>,
    refresh_info: Rc<dyn Fn()>,
}

fn pick_target(ui: &Rc<Ui>, recorder: &Rc<Recorder>, title: &str, options: ExportOptions) {
    let dialog = gtk::FileDialog::new();
    dialog.set_modal(true);
    let base = format!("{} timelapse", title.replace('/', "-"));
    let ui = Rc::clone(ui);
    let recorder = Rc::clone(recorder);
    let title = title.to_string();
    let (name, mime) = match options.format {
        OutputFormat::Avif => ("Animated AVIF", "image/avif"),
        OutputFormat::ProRes => ("QuickTime Video", "video/quicktime"),
        OutputFormat::H264 | OutputFormat::ImageSequence => ("MP4 Video", "video/mp4"),
    };
    match options.format {
        OutputFormat::H264 | OutputFormat::Avif | OutputFormat::ProRes => {
            let ext = options.format.extension();
            let filter = gtk::FileFilter::new();
            filter.set_name(Some(name));
            filter.add_pattern(&format!("*.{ext}"));
            filter.add_mime_type(mime);
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            dialog.set_title("Export Timelapse");
            dialog.set_filters(Some(&filters));
            dialog.set_initial_name(Some(&format!("{base}.{ext}")));
            dialog.save(Some(&ui.win.clone()), None::<&gio::Cancellable>, move |result| {
                let Some(chosen) = result.ok().and_then(|f| f.path()) else { return };
                let Some(ffmpeg) = find_ffmpeg() else {
                    toast(&ui.toasts, "FFmpeg is not installed");
                    return;
                };
                let (path, renamed) = with_extension(chosen, ext);
                // The chooser asked about the name the user typed; adding the
                // extension can land on a different file that already exists.
                let confirm = renamed && path.exists();
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let window = ui.win.clone();
                let begin: Box<dyn FnOnce()> = Box::new(move || {
                    start(&ui, &recorder, &title, options, ExportTarget::Video { path, ffmpeg });
                });
                if confirm {
                    confirm_replace(&window, &name, begin);
                } else {
                    begin();
                }
            });
        }
        OutputFormat::ImageSequence => {
            dialog.set_title("Choose a Folder for the Frames");
            dialog.select_folder(Some(&ui.win.clone()), None::<&gio::Cancellable>, move |result| {
                let Some(parent) = result.ok().and_then(|f| f.path()) else { return };
                let dir = unique_dir(&parent, &base);
                let prefix = base.replace(' ', "_");
                start(&ui, &recorder, &title, options, ExportTarget::Sequence { dir, prefix });
            });
        }
    }
}

fn start(ui: &Rc<Ui>, recorder: &Rc<Recorder>, title: &str, options: ExportOptions, target: ExportTarget) {
    let output = match &target {
        ExportTarget::Video { path, .. } => path.clone(),
        ExportTarget::Sequence { dir, .. } => dir.clone(),
    };
    let sources = match recorder.export_sources() {
        Ok(sources) => sources,
        Err(e) => {
            toast(&ui.toasts, &format!("Export failed: {e}"));
            return;
        }
    };
    let progress = Arc::new(ExportProgress::default());
    *ui.running.borrow_mut() = Some(Arc::clone(&progress));
    set_busy(ui, true);

    let job_title = title.to_string();
    let software = format!("OxieDraw {}", crate::settings::APP_VERSION);
    let margin_rgb = crate::canvas_paintable::backdrop_rgb8();
    let (tx, rx) = mpsc::channel::<Result<u64, RecordingError>>();
    let worker_progress = Arc::clone(&progress);
    std::thread::spawn(move || {
        let job = ExportJob { sources, options, target, margin_rgb, title: job_title, software };
        let _ = tx.send(oxiedraw_core::recording::export::run(&job, &worker_progress));
    });

    let ui = Rc::clone(ui);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => {
                let done = progress.done.load(Ordering::Relaxed);
                let total = progress.total.load(Ordering::Relaxed);
                if total > 0 {
                    #[allow(clippy::cast_precision_loss)]
                    ui.progress_bar.set_fraction(done as f64 / total as f64);
                    ui.info.set_label(&format!(
                        "Exporting frame {} of {}...",
                        group_thousands(done.min(total)),
                        group_thousands(total)
                    ));
                } else {
                    ui.progress_bar.pulse();
                }
                return glib::ControlFlow::Continue;
            }
            Err(mpsc::TryRecvError::Disconnected) => Err(RecordingError::Corrupt("export worker stopped")),
        };
        ui.running.borrow_mut().take();
        set_busy(&ui, false);
        match result {
            Ok(frames) => {
                tracing::info!(target: "oxiedraw::doc", path = %output.display(), frames, "recording exported");
                let t = adw::Toast::new(&format!("Exported to {}", output.display()));
                t.set_button_label(Some("Open Folder"));
                let folder = if output.is_dir() {
                    output.clone()
                } else {
                    output.parent().map_or_else(|| output.clone(), Path::to_path_buf)
                };
                t.connect_button_clicked(move |_| {
                    let uri = format!("file://{}", folder.display());
                    if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
                        tracing::warn!(error = %e, "failed to open folder");
                    }
                });
                t.set_timeout(6);
                ui.toasts.add_toast(t);
            }
            Err(RecordingError::Cancelled) => toast(&ui.toasts, "Export cancelled"),
            Err(e) => {
                tracing::error!(error = %e, "recording export failed");
                toast(&ui.toasts, &format!("Export failed: {e}"));
            }
        }
        glib::ControlFlow::Break
    });
}

fn set_busy(ui: &Ui, busy: bool) {
    ui.export_btn.set_sensitive(!busy);
    ui.export_btn.set_label(if busy { "Exporting..." } else { "Export" });
    ui.page.set_sensitive(!busy);
    ui.progress_bar.set_visible(busy);
    ui.progress_bar.set_fraction(0.0);
    if !busy {
        (ui.refresh_info)();
    }
}

fn toast(toasts: &adw::ToastOverlay, text: &str) {
    let t = adw::Toast::new(text);
    t.set_timeout(4);
    toasts.add_toast(t);
}

/// The path with `ext`, and whether it had to be added. `set_extension` would
/// eat the tail of a name like "sketch v1.2".
fn with_extension(path: PathBuf, ext: &str) -> (PathBuf, bool) {
    if path.extension().is_some_and(|e| e == ext) {
        return (path, false);
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{ext}"));
    (path.with_file_name(name), true)
}

fn confirm_replace(window: &adw::Window, name: &str, begin: Box<dyn FnOnce()>) {
    let dialog = gtk::AlertDialog::builder()
        .message(format!("Replace \"{name}\"?"))
        .detail("A file with that name already exists in this folder.")
        .modal(true)
        .build();
    dialog.set_buttons(&["Cancel", "Replace"]);
    dialog.set_cancel_button(0);
    dialog.set_default_button(0);
    dialog.choose(Some(window), None::<&gio::Cancellable>, move |result| {
        if result == Ok(1) {
            begin();
        }
    });
}

/// `parent/base`, or `parent/base (2)`, ... - the first that doesn't exist yet.
fn unique_dir(parent: &Path, base: &str) -> PathBuf {
    let first = parent.join(base);
    if !first.exists() {
        return first;
    }
    (2..10_000)
        .map(|n| parent.join(format!("{base} ({n})")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

fn save_options(options: ExportOptions) {
    let mut app = AppSettings::load();
    app.recording_export = options;
    app.save();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{unique_dir, with_extension};

    #[test]
    fn extension_is_added_without_eating_a_dotted_name() {
        let named = |s: &str| PathBuf::from(format!("/tmp/{s}"));
        assert_eq!(with_extension(named("sketch v1.2"), "mp4"), (named("sketch v1.2.mp4"), true));
        assert_eq!(with_extension(named("clip"), "mp4"), (named("clip.mp4"), true));
        assert_eq!(with_extension(named("clip.mp4"), "mp4"), (named("clip.mp4"), false));
    }

    #[test]
    fn unique_dir_skips_existing_names() {
        let parent = std::env::temp_dir().join(format!("oxiedraw_unique_{}", std::process::id()));
        std::fs::create_dir_all(parent.join("t")).expect("test io");
        assert_eq!(unique_dir(&parent, "t"), parent.join("t (2)"));
        assert_eq!(unique_dir(&parent, "fresh"), parent.join("fresh"));
        std::fs::remove_dir_all(parent).ok();
    }
}
