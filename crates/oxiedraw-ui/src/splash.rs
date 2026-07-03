//! Startup splash window with staged loading progress.
//!
//! Shown immediately at launch while the slow startup work (brush library,
//! the system font database) runs in stages driven off the splash's frame clock
//! so each step can report progress instead of freezing on a blank window. The
//! font files are parsed in parallel on a background thread; the loader polls
//! for the result each frame. The window is a
//! rounded, undecorated card showing the banner art, an accent progress bar,
//! and three corner overlays: the current step (bottom-left), the logo plus
//! version (top-right), and the banner artist credit (bottom-right). When the
//! last stage finishes it runs the `finish` callback (which builds the first
//! document and reveals the main window) and closes itself.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use oxiedraw_core::text::fonts::FaceInfo;

use adw::prelude::*;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;

use crate::session::GlobalState;

/// 16:9 banner art and its artist credit, embedded from the data folder.
const BANNER_PNG: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/splash/banner.png"));
const BANNER_AUTHOR: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/splash/banner-author.txt"));

const SPLASH_WIDTH: i32 = 640;
const SPLASH_HEIGHT: i32 = 360; // 16:9

/// How long to keep loading fonts each tick before yielding to the main loop so
/// the progress bar can repaint. Larger batches keep total load time close to a
/// plain blocking scan; the short tick interval still leaves room to paint.
const FONT_TICK_BUDGET: Duration = Duration::from_millis(10);

/// Show the splash and start the staged loader. `finish` is invoked on the main
/// thread once loading completes (it builds the first document and reveals the
/// main window); the splash closes itself just after.
pub(crate) fn run(global: GlobalState, finish: Box<dyn FnOnce()>) {
    install_css();

    let banner = load_banner();
    let step_label = gtk::Label::builder().label("Starting").build();
    step_label.add_css_class("oxie-splash-pill");

    let progress = gtk::ProgressBar::builder().fraction(0.0).build();

    let card = build_card(banner.as_ref(), &step_label, &progress);

    let window = gtk::Window::builder()
        .resizable(false)
        .decorated(false)
        .default_width(SPLASH_WIDTH)
        .default_height(SPLASH_HEIGHT)
        .child(&card)
        .build();
    window.add_css_class("oxie-splash");
    if let Some(app) = gtk::gio::Application::default()
        .and_then(|a| a.downcast::<gtk::Application>().ok())
    {
        window.set_application(Some(&app));
    }
    window.present();

    start_loader(global, window, step_label, progress, finish);
}

/// One overlaid card: banner picture with the version pill (top-right), step
/// pill (bottom-left), artist credit (bottom-right), and progress bar (bottom).
fn build_card(
    banner: Option<&gdk::Texture>,
    step_label: &gtk::Label,
    progress: &gtk::ProgressBar,
) -> gtk::Overlay {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .hexpand(true)
        .vexpand(true)
        .build();
    if let Some(tex) = banner {
        picture.set_paintable(Some(tex));
    }

    let overlay = gtk::Overlay::builder().child(&picture).build();
    overlay.add_css_class("oxie-splash-card");
    overlay.set_size_request(SPLASH_WIDTH, SPLASH_HEIGHT);

    // Top-right: logo + version in a pill.
    let version = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(14)
        .margin_end(14)
        .build();
    version.add_css_class("oxie-splash-pill");
    let logo = gtk::Image::from_icon_name(crate::APP_ID);
    logo.set_pixel_size(18);
    version.append(&logo);
    version.append(&gtk::Label::new(Some(&format!("v{}", env!("CARGO_PKG_VERSION")))));
    overlay.add_overlay(&version);

    // Bottom-left: current step pill (sits above the progress bar).
    step_label.set_halign(gtk::Align::Start);
    step_label.set_valign(gtk::Align::End);
    step_label.set_margin_start(14);
    step_label.set_margin_bottom(20);
    overlay.add_overlay(step_label);

    // Bottom-right: banner artist credit.
    let author = gtk::Label::new(Some(&format!("Art by {}", BANNER_AUTHOR.trim())));
    author.add_css_class("oxie-splash-author");
    author.set_halign(gtk::Align::End);
    author.set_valign(gtk::Align::End);
    author.set_margin_end(14);
    author.set_margin_bottom(22);
    overlay.add_overlay(&author);

    // Bottom edge: accent progress bar, full width.
    progress.set_valign(gtk::Align::End);
    progress.set_hexpand(true);
    overlay.add_overlay(progress);

    overlay
}

// ---------------------------------------------------------------------------
// Staged loader
// ---------------------------------------------------------------------------

/// Progress band boundaries on the bar. Two labelled steps: brushes, then a
/// single "Loading fonts" step that spans parsing the font files (in parallel,
/// off-thread) and then rendering the per-family previews.
const BRUSHES_END: f64 = 0.05;
const PARSE_END: f64 = 0.45;
const PREVIEWS_END: f64 = 0.92;

enum Phase {
    AnnounceBrushes,
    LoadBrushes,
    ScanFonts,
    AwaitFonts {
        rx: Receiver<Vec<FaceInfo>>,
        parsed: Arc<AtomicUsize>,
        total: usize,
        start: Instant,
    },
    ScanPreviews,
    RenderPreviews {
        families: Rc<Vec<String>>,
        index: usize,
        start: Instant,
    },
    AnnounceStartup,
    Startup,
    Closing,
}

/// Map `index/total` progress onto the bar band `[start, end]`.
#[allow(clippy::cast_precision_loss)]
fn ratio(start: f64, end: f64, index: usize, total: usize) -> f64 {
    if total == 0 {
        end
    } else {
        start + (end - start) * (index as f64 / total as f64)
    }
}

struct Loader {
    global: GlobalState,
    window: gtk::Window,
    step: gtk::Label,
    progress: gtk::ProgressBar,
    phase: Phase,
    finish: Option<Box<dyn FnOnce()>>,
    /// When the staged loader began, for the total-startup timing log.
    launch: Instant,
}

fn start_loader(
    global: GlobalState,
    window: gtk::Window,
    step: gtk::Label,
    progress: gtk::ProgressBar,
    finish: Box<dyn FnOnce()>,
) {
    let driver = window.clone();
    let loader = Rc::new(RefCell::new(Loader {
        global,
        window,
        step,
        progress,
        phase: Phase::AnnounceBrushes,
        finish: Some(finish),
        launch: Instant::now(),
    }));

    // Drive the loader from the splash window's frame clock: a tick callback
    // fires once per frame and the widget repaints right after, so every label
    // / progress update is actually shown (a plain timeout starved the redraw).
    driver.add_tick_callback(move |_widget, _clock| {
        let mut l = loader.borrow_mut();
        match std::mem::replace(&mut l.phase, Phase::Closing) {
            Phase::AnnounceBrushes => {
                // Label the brush step one frame ahead so it paints before
                // load_brushes() blocks the next tick.
                l.step.set_label("Loading brushes");
                l.progress.set_fraction(0.01);
                l.phase = Phase::LoadBrushes;
                glib::ControlFlow::Continue
            }
            Phase::LoadBrushes => {
                let t = Instant::now();
                l.global.load_brushes();
                tracing::info!(elapsed_ms = t.elapsed().as_millis(), "startup: brushes loaded");
                l.progress.set_fraction(BRUSHES_END);
                l.step.set_label("Finding fonts");
                l.phase = Phase::ScanFonts;
                glib::ControlFlow::Continue
            }
            Phase::ScanFonts => {
                let t = Instant::now();
                let files = oxiedraw_core::text::fonts::system_font_files();
                let total = files.len();
                tracing::info!(
                    files = total,
                    elapsed_ms = t.elapsed().as_millis(),
                    "startup: scanned system font files"
                );
                // Parse the font files in parallel off the UI thread; the splash
                // keeps animating and polls `parsed` for a running count.
                let parsed = Arc::new(AtomicUsize::new(0));
                let rx = oxiedraw_core::text::fonts::spawn_font_load(files, parsed.clone());
                l.step.set_label("Loading fonts");
                l.phase = Phase::AwaitFonts { rx, parsed, total, start: Instant::now() };
                glib::ControlFlow::Continue
            }
            Phase::AwaitFonts { rx, parsed, total, start } => {
                let done = parsed.load(Ordering::Relaxed).min(total);
                l.progress.set_fraction(ratio(BRUSHES_END, PARSE_END, done, total));
                match rx.try_recv() {
                    Ok(faces) => {
                        l.global.text_engine.borrow_mut().add_faces(faces);
                        tracing::info!(
                            files = total,
                            elapsed_ms = start.elapsed().as_millis(),
                            "startup: parsed font files"
                        );
                        l.progress.set_fraction(PARSE_END);
                        l.phase = Phase::ScanPreviews;
                    }
                    Err(TryRecvError::Empty) => {
                        l.phase = Phase::AwaitFonts { rx, parsed, total, start };
                    }
                    // The worker dropped without a result (shouldn't happen); the
                    // font list is simply whatever landed, so press on.
                    Err(TryRecvError::Disconnected) => {
                        tracing::warn!("startup: font parse worker disconnected");
                        l.phase = Phase::ScanPreviews;
                    }
                }
                glib::ControlFlow::Continue
            }
            Phase::ScanPreviews => {
                let families = Rc::new(l.global.text_engine.borrow().available_families());
                l.step.set_label(&format!("Loading fonts (0/{})", families.len()));
                l.progress.set_fraction(PARSE_END);
                l.phase = Phase::RenderPreviews {
                    families,
                    index: 0,
                    start: Instant::now(),
                };
                glib::ControlFlow::Continue
            }
            Phase::RenderPreviews { families, mut index, start } => {
                let total = families.len();
                let color = crate::font_previews::theme_text_color();
                // Render as many as fit the frame budget, then yield so the bar
                // repaints. No artificial pacing - previews are fast enough now
                // that startup shouldn't wait on a legible count.
                let tick = Instant::now();
                {
                    let mut engine = l.global.text_engine.borrow_mut();
                    while index < total && tick.elapsed() < FONT_TICK_BUDGET {
                        l.global
                            .font_previews
                            .render_one(&mut engine, &families[index], color);
                        index += 1;
                    }
                }
                l.progress.set_fraction(ratio(PARSE_END, PREVIEWS_END, index, total));
                l.step.set_label(&format!("Loading fonts ({index}/{total})"));
                l.phase = if index >= total {
                    tracing::info!(
                        families = total,
                        elapsed_ms = start.elapsed().as_millis(),
                        "startup: rendered font previews"
                    );
                    Phase::AnnounceStartup
                } else {
                    Phase::RenderPreviews { families, index, start }
                };
                glib::ControlFlow::Continue
            }
            Phase::AnnounceStartup => {
                l.step.set_label("Starting up");
                l.progress.set_fraction(0.95);
                l.phase = Phase::Startup;
                glib::ControlFlow::Continue
            }
            Phase::Startup => {
                // Fonts + previews are ready: build the first document and reveal
                // the main window.
                let t = Instant::now();
                if let Some(finish) = l.finish.take() {
                    finish();
                }
                tracing::info!(
                    elapsed_ms = t.elapsed().as_millis(),
                    total_ms = l.launch.elapsed().as_millis(),
                    "startup: first document ready"
                );
                l.progress.set_fraction(1.0);
                // Brief beat at 100% before handing off to the main window.
                let window = l.window.clone();
                glib::timeout_add_local_once(Duration::from_millis(180), move || window.close());
                glib::ControlFlow::Break
            }
            Phase::Closing => glib::ControlFlow::Break,
        }
    });
}

// ---------------------------------------------------------------------------
// Assets + styling
// ---------------------------------------------------------------------------

fn load_banner() -> Option<gdk::Texture> {
    let bytes = glib::Bytes::from_static(BANNER_PNG);
    match gdk::Texture::from_bytes(&bytes) {
        Ok(tex) => Some(tex),
        Err(e) => {
            tracing::warn!(%e, "failed to decode splash banner");
            None
        }
    }
}

/// Install the splash CSS once. Transparent window so the card's rounded
/// corners show; the progress bar is forced to the libadwaita accent colour.
fn install_css() {
    use std::sync::OnceLock;
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.set(()).is_err() {
        return;
    }

    let css = r"
        .oxie-splash { background: transparent; }
        .oxie-splash-card {
            border-radius: 16px;
            background-color: #11151f;
        }
        .oxie-splash-pill {
            background-color: rgba(0, 0, 0, 0.55);
            color: #ffffff;
            border-radius: 999px;
            padding: 4px 12px;
            font-weight: 600;
        }
        .oxie-splash-author {
            color: rgba(255, 255, 255, 0.7);
            font-size: 11px;
            text-shadow: 0 1px 2px rgba(0, 0, 0, 0.6);
        }
        .oxie-splash progressbar { min-height: 4px; }
        .oxie-splash progressbar > trough {
            min-height: 4px;
            background-color: rgba(0, 0, 0, 0.4);
        }
        .oxie-splash progressbar > trough > progress {
            min-height: 4px;
            background-color: @accent_bg_color;
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
