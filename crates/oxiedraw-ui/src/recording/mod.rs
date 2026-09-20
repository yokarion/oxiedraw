//! Timelapse recording: the per-document [`Recorder`], the top-bar record
//! button, and the settings and export windows. The recorder polls the canvas
//! on a short timer; a due frame is read back from the GPU without blocking and
//! handed to the core encoder thread, which appends it to a spool file in the
//! cache dir. Saves copy the part of the spool not yet in the project.

mod button;
mod export_window;
mod settings_window;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use adw::prelude::*;
use oxiedraw_core::recording::codec::Frame;
use oxiedraw_core::recording::segments::{
    FreshFrames, RecordingPayload, SegmentSource, entry_path, find_in_archive,
};
use oxiedraw_core::recording::spool::{CapturedFrame, FrameSink, SpoolProgress};
use oxiedraw_core::recording::{RecordingError, RecordingManifest, RecordingSettings, SegmentEntry, now_ms};
use relm4::gtk::{gio, glib};

use crate::canvas::Viewport;
use crate::tabs::TabManager;

pub(crate) use button::RecordButton;

const TICK: Duration = Duration::from_millis(100);
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// Deleting waits longer than a flush: the encoder may be busy with a keyframe,
/// and a half-applied delete would leave the spool and the index disagreeing.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Recorder {
    viewport: Viewport,
    /// True while the canvas holds something other than the document (a
    /// component being edited, pixels lifted by the transform tool).
    blocked: Rc<dyn Fn() -> bool>,
    settings: Cell<RecordingSettings>,
    recording: Cell<bool>,
    sink: RefCell<Option<FrameSink>>,
    /// The document's file, which holds the `saved` segments.
    source: RefCell<Option<PathBuf>>,
    saved: RefCell<Vec<SegmentEntry>>,
    /// How much of the spool is already in `source`. `None` once a delete or a
    /// load leaves nothing of it saved.
    persisted: Cell<Option<SpoolProgress>>,
    /// Bumped by delete, so a save that started before it can't restore the index.
    generation: Cell<u64>,
    last_version: Cell<Option<u64>>,
    last_capture: Cell<Option<Instant>>,
    /// The readback the GPU is working on: `(capture time, halvings, version)`.
    in_flight: Cell<Option<(u64, u8, u64)>>,
    /// Counts settings changes and deletes; saved state is the value a
    /// completed save was built from.
    edits: Cell<u64>,
    saved_edits: Cell<u64>,
    listeners: RefCell<Vec<Rc<dyn Fn()>>>,
    timer: RefCell<Option<glib::SourceId>>,
}

/// What a save handed out, for [`Recorder::saved_to`] once it lands.
pub(crate) struct SaveTicket {
    progress: SpoolProgress,
    generation: u64,
    edits: u64,
}

pub(crate) struct Stats {
    pub(crate) frames: u64,
    pub(crate) bytes: u64,
}

struct UnsavedRange {
    offset: u64,
    len: u64,
    frames: u64,
}

impl Recorder {
    pub(crate) fn new(viewport: Viewport, blocked: Rc<dyn Fn() -> bool>) -> Rc<Self> {
        Rc::new(Self {
            viewport,
            blocked,
            settings: Cell::new(RecordingSettings::default()),
            recording: Cell::new(false),
            sink: RefCell::new(None),
            source: RefCell::new(None),
            saved: RefCell::new(Vec::new()),
            persisted: Cell::new(None),
            generation: Cell::new(0),
            last_version: Cell::new(None),
            last_capture: Cell::new(None),
            in_flight: Cell::new(None),
            edits: Cell::new(0),
            saved_edits: Cell::new(0),
            listeners: RefCell::new(Vec::new()),
            timer: RefCell::new(None),
        })
    }

    pub(crate) fn is_recording(&self) -> bool {
        self.recording.get()
    }

    pub(crate) fn settings(&self) -> RecordingSettings {
        self.settings.get()
    }

    pub(crate) fn set_settings(&self, settings: RecordingSettings) {
        if self.settings.replace(settings) != settings {
            self.mark_edited();
            self.notify();
        }
    }

    fn mark_edited(&self) {
        self.edits.set(self.edits.get() + 1);
    }

    /// Settings changed or the recording was deleted since the last save.
    pub(crate) fn is_dirty(&self) -> bool {
        self.edits.get() != self.saved_edits.get()
    }

    pub(crate) fn connect_changed(&self, f: Rc<dyn Fn()>) {
        self.listeners.borrow_mut().push(f);
    }

    fn notify(&self) {
        let listeners = self.listeners.borrow().clone();
        for f in listeners {
            f();
        }
    }

    pub(crate) fn start(self: &Rc<Self>) -> Result<(), String> {
        if self.recording.get() {
            return Ok(());
        }
        if self.sink.borrow().is_none() {
            let sink = FrameSink::spawn(next_spool_path()?).map_err(|e| e.to_string())?;
            *self.sink.borrow_mut() = Some(sink);
        }
        self.last_version.set(None);
        self.last_capture.set(None);
        self.recording.set(true);
        self.ensure_timer();
        tracing::info!(target: "oxiedraw::doc", "recording started");
        self.notify();
        Ok(())
    }

    pub(crate) fn stop(&self) {
        if self.recording.replace(false) {
            tracing::info!(target: "oxiedraw::doc", "recording stopped");
            self.notify();
        }
    }

    pub(crate) fn toggle(self: &Rc<Self>) -> Result<(), String> {
        if self.recording.get() {
            self.stop();
            Ok(())
        } else {
            self.start()
        }
    }

    pub(crate) fn stats(&self) -> Stats {
        let saved = self.saved.borrow();
        let unsaved = self.sink.borrow().as_ref().map(|sink| self.unsaved_range(&sink.progress()));
        let (frames, bytes) = unsaved.map_or((0, 0), |r| (r.frames, r.len));
        Stats {
            frames: saved.iter().map(|s| s.frames).sum::<u64>() + frames,
            bytes: saved.iter().map(|s| s.bytes).sum::<u64>() + bytes,
        }
    }

    /// The part of the spool not in the project yet. A clear resets the spool,
    /// so an offset from before one covers nothing.
    fn unsaved_range(&self, now: &SpoolProgress) -> UnsavedRange {
        match self.persisted.get() {
            Some(p) if p.generation == now.generation => UnsavedRange {
                offset: p.bytes,
                len: now.bytes.saturating_sub(p.bytes),
                frames: now.frames.saturating_sub(p.frames),
            },
            _ => UnsavedRange { offset: 0, len: now.bytes, frames: now.frames },
        }
    }

    /// Throw every recorded frame away. The file keeps them until the next save.
    /// False when the encoder didn't confirm in time and nothing was changed.
    pub(crate) fn delete_all(&self) -> bool {
        if let Some(sink) = self.sink.borrow().as_ref()
            && !sink.clear(CLEAR_TIMEOUT)
        {
            tracing::warn!("recording: encoder did not confirm the clear in time");
            return false;
        }
        self.generation.set(self.generation.get() + 1);
        self.saved.borrow_mut().clear();
        self.persisted.set(None);
        self.last_version.set(None);
        self.mark_edited();
        tracing::info!(target: "oxiedraw::doc", "recording deleted");
        self.notify();
        true
    }

    /// Take over the recording of a project that was just opened from `path`.
    pub(crate) fn adopt(&self, manifest: Option<RecordingManifest>, path: &Path) {
        let manifest = manifest.unwrap_or_default();
        self.settings.set(manifest.settings);
        *self.saved.borrow_mut() = manifest.segments;
        *self.source.borrow_mut() = Some(path.to_path_buf());
        self.saved_edits.set(self.edits.get());
        self.notify();
    }

    /// Bring the spool up to the canvas as it is now and describe what the save
    /// should write. Call after anything that settles pending canvas edits.
    pub(crate) fn prepare_save(&self) -> (RecordingPayload, SaveTicket) {
        self.capture_now();
        let mut progress = None;
        let mut fresh = None;
        if let Some(sink) = self.sink.borrow().as_ref() {
            if !sink.flush(FLUSH_TIMEOUT) {
                tracing::warn!("recording: encoder busy, newest frames go in the next save");
            }
            match sink.open() {
                Ok((file, now)) => {
                    let range = self.unsaved_range(&now);
                    progress = Some(now);
                    fresh = Some(FreshFrames {
                        source: SegmentSource::new(file, range.offset, range.len),
                        frames: range.frames,
                    });
                }
                Err(e) => tracing::warn!(err = %e, "recording: spool unreadable, frames not saved"),
            }
        }
        let payload = RecordingPayload {
            settings: self.settings.get(),
            source: self.source.borrow().clone(),
            saved: self.saved.borrow().clone(),
            fresh,
        };
        let ticket = SaveTicket {
            progress: progress.unwrap_or_default(),
            generation: self.generation.get(),
            edits: self.edits.get(),
        };
        (payload, ticket)
    }

    /// The document now lives at `path`, holding `manifest` (as written). A
    /// delete or a settings change made while the save ran keeps the document
    /// dirty, since the file was built before it.
    pub(crate) fn saved_to(&self, path: &Path, manifest: Option<RecordingManifest>, ticket: &SaveTicket) {
        *self.source.borrow_mut() = Some(path.to_path_buf());
        if ticket.generation != self.generation.get() {
            return;
        }
        *self.saved.borrow_mut() = manifest.map(|m| m.segments).unwrap_or_default();
        self.persisted.set(Some(ticket.progress));
        self.saved_edits.set(ticket.edits);
        self.notify();
    }

    /// Every recorded frame, in order. Both sources are opened here, together,
    /// so a save landing mid-export can't move or duplicate frames.
    pub(crate) fn export_sources(&self) -> Result<Vec<SegmentSource>, RecordingError> {
        self.capture_now();
        let entries: Vec<String> = self.saved.borrow().iter().map(|s| entry_path(&s.name)).collect();
        let mut sources = match self.source.borrow().as_deref() {
            Some(path) if !entries.is_empty() => find_in_archive(path, &entries)?,
            _ => Vec::new(),
        };
        if let Some(sink) = self.sink.borrow().as_ref() {
            let _ = sink.flush(FLUSH_TIMEOUT);
            let (file, now) = sink.open()?;
            let range = self.unsaved_range(&now);
            if range.len > 0 {
                sources.push(SegmentSource::new(file, range.offset, range.len));
            }
        }
        Ok(sources)
    }

    /// Stop for good: the encoder thread exits and deletes its spool.
    pub(crate) fn shutdown(&self) {
        self.recording.set(false);
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
        self.sink.borrow_mut().take();
    }

    fn ensure_timer(self: &Rc<Self>) {
        if self.timer.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local(TICK, move || {
            let _span = oxiedraw_utils::frame_profile::span(oxiedraw_utils::frame_profile::Stage::Timers);
            let Some(recorder) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            recorder.tick();
            glib::ControlFlow::Continue
        });
        *self.timer.borrow_mut() = Some(id);
    }

    fn tick(&self) {
        let canvas = self.viewport.canvas();
        let Ok(mut canvas) = canvas.try_borrow_mut() else {
            return;
        };
        if self.in_flight.get().is_some() {
            self.collect(&mut canvas, false);
            return;
        }
        if !self.recording.get() {
            return;
        }
        let settings = self.settings.get();
        let version = canvas.pixels_version();
        let changed = self.last_version.get() != Some(version);
        let drawing = canvas.is_drawing();
        let now = Instant::now();
        let due = settings.frequency.interval().map_or(changed && !drawing, |interval| {
            self.last_capture.get().is_none_or(|t| now.duration_since(t) >= interval)
                && (changed || !settings.skip_unchanged)
                && (!drawing || settings.mid_stroke)
        });
        if !due || (self.blocked)() || canvas.capture_blocked() {
            return;
        }
        if !changed {
            // Holding an unchanged frame needs no readback.
            self.last_capture.set(Some(now));
            if let Some(sink) = self.sink.borrow().as_ref() {
                sink.push_repeat(now_ms());
            }
            return;
        }
        if self.begin(&mut canvas) {
            self.last_capture.set(Some(now));
        }
    }

    fn begin(&self, canvas: &mut oxiedraw_core::canvas::Canvas) -> bool {
        let halvings = self.settings.get().scale.halvings();
        match canvas.begin_frame_capture(u32::from(halvings)) {
            Ok(true) => {
                // Read the version after the capture starts: it recomposites a
                // pending liquify bake, which bumps it.
                self.in_flight.set(Some((now_ms(), halvings, canvas.pixels_version())));
                true
            }
            Ok(false) => false,
            Err(e) => {
                tracing::warn!(err = %e, "recording: capture failed to start");
                false
            }
        }
    }

    fn collect(&self, canvas: &mut oxiedraw_core::canvas::Canvas, wait: bool) {
        let Some((time_ms, halvings, version)) = self.in_flight.get() else {
            return;
        };
        if !canvas.frame_capture_pending() {
            self.in_flight.set(None);
            return;
        }
        let mut pixels = Vec::new();
        match canvas.take_frame_capture(&mut pixels, wait) {
            Ok(Some((width, height))) => {
                self.in_flight.set(None);
                let settings = self.settings.get();
                let captured = CapturedFrame {
                    frame: Frame { width, height, pixels },
                    halvings,
                    time_ms,
                    // Per-stroke recording only fires on a change, so a frame
                    // that turns out identical is never worth holding.
                    skip_unchanged: settings.skip_unchanged || settings.frequency.interval().is_none(),
                };
                // A dropped frame stays uncaptured, so the next tick takes it again.
                match self.sink.borrow().as_ref() {
                    Some(sink) if sink.push(captured) => self.last_version.set(Some(version)),
                    Some(_) => tracing::debug!("recording: encoder backed up, frame dropped"),
                    None => {}
                }
            }
            Ok(None) => {}
            Err(e) => {
                self.in_flight.set(None);
                tracing::warn!(err = %e, "recording: capture readback failed");
            }
        }
    }

    // Blocking: the canvas as it is right now becomes the newest frame, so a save
    // or export always ends on what the user sees.
    fn capture_now(&self) {
        let canvas = self.viewport.canvas();
        let Ok(mut canvas) = canvas.try_borrow_mut() else {
            return;
        };
        self.collect(&mut canvas, true);
        if !self.recording.get() || canvas.is_drawing() || (self.blocked)() || canvas.capture_blocked() {
            return;
        }
        if self.last_version.get() == Some(canvas.pixels_version()) {
            return;
        }
        if self.begin(&mut canvas) {
            self.collect(&mut canvas, true);
        }
    }
}

fn next_spool_path() -> Result<PathBuf, String> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    static CLEANED: std::sync::Once = std::sync::Once::new();
    let dir = crate::settings::recording_spool_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    CLEANED.call_once(|| remove_stale_spools(&dir));
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("{}-{n}.spool", std::process::id())))
}

// Spools of a process that crashed or was killed are named after a pid that is gone.
fn remove_stale_spools(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let pid = name.split('-').next().and_then(|p| p.parse::<u32>().ok());
        let alive = pid.is_some_and(|p| p == std::process::id() || Path::new(&format!("/proc/{p}")).exists());
        if !alive {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `ffmpeg` on `PATH`, if there is one.
pub(crate) fn find_ffmpeg() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("ffmpeg"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn show_missing_ffmpeg(window: &adw::ApplicationWindow) {
    let dialog = gtk::AlertDialog::builder()
        .message("FFmpeg is not installed")
        .detail(
            "Recording needs FFmpeg to turn the recorded frames into a video. Install it \
             with your package manager (for example \"sudo pacman -S ffmpeg\" or \"sudo apt \
             install ffmpeg\"), then try again.",
        )
        .modal(true)
        .build();
    dialog.set_buttons(&["OK"]);
    dialog.choose(Some(window), None::<&gio::Cancellable>, |_| {});
}

/// A small info button that shows `text` in a popover.
fn hint_button(text: &str) -> gtk::MenuButton {
    let label = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .max_width_chars(40)
        .xalign(0.0)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let popover = gtk::Popover::builder().child(&label).build();
    let button = gtk::MenuButton::builder()
        .icon_name("dialog-information-symbolic")
        .popover(&popover)
        .valign(gtk::Align::Center)
        .tooltip_text("More about this option")
        .build();
    button.add_css_class("flat");
    button.add_css_class("circular");
    button
}

fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn format_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

pub(crate) fn register_actions(manager: &Rc<TabManager>, app: &gtk::Application) {
    let add = |name: &str, run: fn(&TabManager, &Rc<crate::session::DocumentSession>)| {
        let manager = Rc::clone(manager);
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| {
            if let Some(session) = manager.active() {
                run(&manager, &session);
            }
        });
        app.add_action(&action);
    };
    add("recording-toggle", |manager, session| {
        // Stopping never needs FFmpeg; only starting is gated.
        if !session.recording.is_recording() && find_ffmpeg().is_none() {
            show_missing_ffmpeg(&manager.root);
            return;
        }
        if let Err(e) = session.recording.toggle() {
            manager.global.toaster.error(&format!("Could not start recording: {e}"));
        }
    });
    add("recording-export", |manager, session| {
        if find_ffmpeg().is_none() {
            show_missing_ffmpeg(&manager.root);
            return;
        }
        export_window::show(&manager.root, session);
    });
    // Settings needs no FFmpeg, and locking it away would leave someone without
    // FFmpeg unable to delete a recording or turn AutoStart off.
    add("recording-settings", |manager, session| settings_window::show(&manager.root, session));
}

/// Start recording a project that asks for it on open.
pub(crate) fn auto_start(manager: &TabManager, recorder: &Rc<Recorder>) {
    if !recorder.settings().auto_start {
        return;
    }
    if find_ffmpeg().is_none() {
        manager.global.toaster.info("Recording not started: FFmpeg is not installed");
        return;
    }
    if let Err(e) = recorder.start() {
        manager.global.toaster.error(&format!("Could not start recording: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::{format_bytes, group_thousands};

    #[test]
    fn sizes_read_in_kilobytes_then_megabytes() {
        assert_eq!(format_bytes(0), "0 KB");
        assert_eq!(format_bytes(1500), "2 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn frame_counts_get_thousands_separators() {
        assert_eq!(group_thousands(7), "7");
        assert_eq!(group_thousands(1284), "1,284");
        assert_eq!(group_thousands(1_000_000), "1,000,000");
    }
}
