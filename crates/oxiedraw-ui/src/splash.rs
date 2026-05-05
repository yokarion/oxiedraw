//! Startup splash window with staged loading progress.
//!
//! Shown immediately at launch while the slow startup work (brush library,
//! the system font database) runs in stages on a glib timeout so each step can
//! report progress instead of freezing on a blank window. The window is a
//! rounded, undecorated card showing the banner art, an accent progress bar,
//! and three corner overlays: the current step (bottom-left), the logo plus
//! version (top-right), and the banner artist credit (bottom-right). When the
//! last stage finishes it runs the `finish` callback (which builds the first
//! document and reveals the main window) and closes itself.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

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

/// Minimum time the "Loading fonts (i/N)" stage stays up. On a warm cache the
/// previews render in a fraction of a second, too fast to read the count, so
/// rendering is paced across at least this long. A genuinely slow machine just
/// takes however long it needs.
const MIN_FONT_DISPLAY: f64 = 1.4;

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

/// Progress band boundaries on the bar. Loading the font files is folded into
/// the "Loading basics" step; rendering the previews is the slow, clearly
/// counted "Loading fonts" step.
const BRUSHES_END: f64 = 0.05;
const FILES_END: f64 = 0.40;
const PREVIEWS_END: f64 = 0.92;

enum Phase {
    AnnounceBasics,
    LoadBrushes,
    ScanFonts,
    LoadFontFiles { files: Rc<Vec<PathBuf>>, index: usize },
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
        phase: Phase::AnnounceBasics,
        finish: Some(finish),
    }));

    // Drive the loader from the splash window's frame clock: a tick callback
    // fires once per frame and the widget repaints right after, so every label
    // / progress update is actually shown (a plain timeout starved the redraw).
    driver.add_tick_callback(move |_widget, _clock| {
        let mut l = loader.borrow_mut();
        match std::mem::replace(&mut l.phase, Phase::Closing) {
            Phase::AnnounceBasics => {
                l.step.set_label("Loading basics");
                l.progress.set_fraction(0.01);
                l.phase = Phase::LoadBrushes;
                glib::ControlFlow::Continue
            }
            Phase::LoadBrushes => {
                l.global.load_brushes();
                l.progress.set_fraction(BRUSHES_END);
                l.phase = Phase::ScanFonts;
                glib::ControlFlow::Continue
            }
            Phase::ScanFonts => {
                let files = Rc::new(oxiedraw_core::text::fonts::system_font_files());
                l.phase = Phase::LoadFontFiles { files, index: 0 };
                glib::ControlFlow::Continue
            }
            Phase::LoadFontFiles { files, mut index } => {
                let total = files.len();
                let tick = Instant::now();
                {
                    let mut engine = l.global.text_engine.borrow_mut();
                    while index < total && tick.elapsed() < FONT_TICK_BUDGET {
                        engine.load_font_path(&files[index]);
                        index += 1;
                    }
                }
                let frac = ratio(BRUSHES_END, FILES_END, index, total);
                l.progress.set_fraction(frac);
                l.phase = if index >= total {
                    Phase::ScanPreviews
                } else {
                    Phase::LoadFontFiles { files, index }
                };
                glib::ControlFlow::Continue
            }
            Phase::ScanPreviews => {
                let families = Rc::new(l.global.text_engine.borrow().available_families());
                l.step.set_label(&format!("Loading fonts (0/{})", families.len()));
                l.progress.set_fraction(FILES_END);
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
                // Pace rendering so the count is legible: only render up to the
                // share of fonts the elapsed time has "earned" within the
                // minimum display window (a slow machine simply lags behind it).
                #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let target = (((start.elapsed().as_secs_f64() / MIN_FONT_DISPLAY) * total as f64)
                    .ceil() as usize)
                    .min(total);
                let tick = Instant::now();
                {
                    let mut engine = l.global.text_engine.borrow_mut();
                    while index < target && tick.elapsed() < FONT_TICK_BUDGET {
                        l.global
                            .font_previews
                            .render_one(&mut engine, &families[index], color);
                        index += 1;
                    }
                }
                l.progress.set_fraction(ratio(FILES_END, PREVIEWS_END, index, total));
                l.step.set_label(&format!("Loading fonts ({index}/{total})"));
                let done = index >= total && start.elapsed().as_secs_f64() >= MIN_FONT_DISPLAY;
                l.phase = if done {
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
                if let Some(finish) = l.finish.take() {
                    finish();
                }
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
