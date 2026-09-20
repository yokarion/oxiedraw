//! The background encoder: takes captured frames from the UI thread, encodes
//! them and appends the records to a spool file. The file only ever grows
//! during a session, so a save can copy a committed byte range while new
//! frames keep arriving.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::RecordingError;
use super::codec::{Encoded, Frame, FrameEncoder};

/// Frames waiting for the encoder beyond this are dropped rather than queued;
/// at full size on a large canvas each one is hundreds of megabytes.
const QUEUE_DEPTH: usize = 2;

pub struct CapturedFrame {
    pub frame: Frame,
    pub halvings: u8,
    pub time_ms: u64,
    /// Drop an unchanged frame instead of recording a repeat.
    pub skip_unchanged: bool,
}

/// What has been fully written to the spool so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpoolProgress {
    pub bytes: u64,
    pub frames: u64,
    /// Bumped by every clear, so offsets taken before one are recognisable.
    pub generation: u64,
}

enum Message {
    Frame(CapturedFrame),
    Repeat(u64),
    Restart,
    Flush(mpsc::Sender<()>),
    Clear(mpsc::Sender<()>),
}

/// Handle to the encoder thread. Dropping it stops the thread, which deletes
/// the spool file.
pub struct FrameSink {
    tx: SyncSender<Message>,
    progress: Arc<Mutex<SpoolProgress>>,
    path: PathBuf,
}

impl FrameSink {
    pub fn spawn(path: PathBuf) -> Result<Self, RecordingError> {
        let file = File::create(&path)?;
        let progress = Arc::new(Mutex::new(SpoolProgress::default()));
        let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
        let worker = Worker {
            file,
            path: path.clone(),
            encoder: FrameEncoder::default(),
            progress: Arc::clone(&progress),
        };
        std::thread::Builder::new()
            .name("recording-encoder".into())
            .spawn(move || worker.run(&rx))?;
        Ok(Self { tx, progress, path })
    }

    /// Queue a frame. Returns false when the encoder is backed up and the frame was dropped.
    pub fn push(&self, frame: CapturedFrame) -> bool {
        match self.tx.try_send(Message::Frame(frame)) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }

    /// Hold the previous frame for one more tick, without a readback.
    pub fn push_repeat(&self, time_ms: u64) -> bool {
        self.tx.try_send(Message::Repeat(time_ms)).is_ok()
    }

    /// Make the next frame a keyframe.
    pub fn restart(&self) {
        let _ = self.tx.send(Message::Restart);
    }

    /// Wait until every queued frame is on disk. False on timeout.
    pub fn flush(&self, timeout: Duration) -> bool {
        self.round_trip(Message::Flush, timeout)
    }

    /// Throw away everything recorded in this spool.
    pub fn clear(&self, timeout: Duration) -> bool {
        self.round_trip(Message::Clear, timeout)
    }

    fn round_trip(&self, make: fn(mpsc::Sender<()>) -> Message, timeout: Duration) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx.send(make(reply_tx)).is_ok() && reply_rx.recv_timeout(timeout).is_ok()
    }

    #[must_use]
    pub fn progress(&self) -> SpoolProgress {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A read handle plus the progress it is consistent with. The handle stays
    /// valid even if the spool is cleared afterwards.
    pub fn open(&self) -> std::io::Result<(File, SpoolProgress)> {
        let guard = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        let file = File::open(&self.path)?;
        Ok((file, *guard))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

struct Worker {
    file: File,
    path: PathBuf,
    encoder: FrameEncoder,
    progress: Arc<Mutex<SpoolProgress>>,
}

impl Worker {
    fn run(mut self, rx: &mpsc::Receiver<Message>) {
        while let Ok(message) = rx.recv() {
            match message {
                Message::Frame(captured) => self.write_frame(captured),
                Message::Repeat(time_ms) => {
                    if let Some(record) = self.encoder.repeat(time_ms) {
                        self.append(&record);
                    }
                }
                Message::Restart => self.encoder.restart(),
                Message::Flush(reply) => {
                    let _ = reply.send(());
                }
                Message::Clear(reply) => {
                    self.clear();
                    let _ = reply.send(());
                }
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }

    fn write_frame(&mut self, captured: CapturedFrame) {
        let time_ms = captured.time_ms;
        let record = match self.encoder.encode(captured.frame, captured.halvings, time_ms) {
            Ok(Encoded::Record(record)) => record,
            Ok(Encoded::Unchanged) if !captured.skip_unchanged => match self.encoder.repeat(time_ms) {
                Some(record) => record,
                None => return,
            },
            Ok(Encoded::Unchanged) => return,
            Err(e) => {
                tracing::warn!(err = %e, "recording: frame encode failed");
                return;
            }
        };
        self.append(&record);
    }

    fn append(&mut self, record: &[u8]) {
        if let Err(e) = self.file.write_all(record) {
            tracing::warn!(err = %e, "recording: spool write failed");
            self.roll_back();
            return;
        }
        let mut p = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        p.bytes += record.len() as u64;
        p.frames += 1;
    }

    // A half-written record would corrupt everything after it, so cut it off and
    // start the next frame from a keyframe.
    fn roll_back(&mut self) {
        let committed = self.progress.lock().unwrap_or_else(PoisonError::into_inner).bytes;
        let _ = self.file.set_len(committed);
        let _ = self.file.seek(SeekFrom::End(0));
        self.encoder.restart();
    }

    // A fresh file rather than a truncate: a save may still be reading the old
    // one. Swapping the file and resetting the counters under one lock keeps
    // `open` from pairing the new file with the old byte count.
    fn clear(&mut self) {
        let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = std::fs::remove_file(&self.path);
        match File::create(&self.path) {
            Ok(file) => self.file = file,
            Err(e) => tracing::warn!(err = %e, "recording: could not recreate the spool"),
        }
        self.encoder.restart();
        *progress = SpoolProgress { bytes: 0, frames: 0, generation: progress.generation + 1 };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::recording::codec::{FrameDecoder, FrameHeader};

    fn spool_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oxiedraw_spool_{}_{tag}.bin", std::process::id()))
    }

    fn captured(value: u8, time_ms: u64, skip_unchanged: bool) -> CapturedFrame {
        CapturedFrame {
            frame: Frame { width: 8, height: 8, pixels: vec![value; 8 * 8 * 4] },
            halvings: 0,
            time_ms,
            skip_unchanged,
        }
    }

    fn frames_in(path: &Path) -> usize {
        let mut file = File::open(path).unwrap();
        let mut decoder = FrameDecoder::default();
        let mut count = 0;
        while let Some(h) = FrameHeader::read(&mut file).unwrap() {
            let mut payload = vec![0u8; h.payload_len as usize];
            file.read_exact(&mut payload).unwrap();
            if decoder.apply(&h, &payload).unwrap().is_some() {
                count += 1;
            }
        }
        count
    }

    #[test]
    fn frames_land_in_the_spool_and_progress_counts_them() {
        let path = spool_path("progress");
        let sink = FrameSink::spawn(path.clone()).unwrap();
        for frame in [captured(1, 100, true), captured(2, 200, true)] {
            assert!(sink.push(frame));
            assert!(sink.flush(Duration::from_secs(5)));
        }
        let p = sink.progress();
        assert_eq!(p.frames, 2);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), p.bytes);
        assert_eq!(frames_in(&path), 2);
        drop(sink);
    }

    #[test]
    fn unchanged_frames_are_skipped_or_repeated() {
        let path = spool_path("repeat");
        let sink = FrameSink::spawn(path.clone()).unwrap();
        for frame in [captured(1, 1, true), captured(1, 2, true), captured(1, 3, false)] {
            assert!(sink.push(frame));
            assert!(sink.flush(Duration::from_secs(5)));
        }
        assert!(sink.push_repeat(4));
        assert!(sink.flush(Duration::from_secs(5)));
        assert_eq!(sink.progress().frames, 3);
        assert_eq!(frames_in(&path), 3);
    }

    #[test]
    fn clear_empties_the_spool_but_an_open_handle_keeps_the_old_bytes() {
        let path = spool_path("clear");
        let sink = FrameSink::spawn(path.clone()).unwrap();
        sink.push(captured(3, 1, true));
        assert!(sink.flush(Duration::from_secs(5)));
        let (mut old, before) = sink.open().unwrap();
        assert!(sink.clear(Duration::from_secs(5)));
        let after = sink.progress();
        assert_eq!((after.bytes, after.frames), (0, 0));
        assert_ne!(after.generation, before.generation, "a clear is recognisable");
        let mut bytes = Vec::new();
        old.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, before.bytes);
    }

    #[test]
    fn dropping_the_sink_removes_the_spool() {
        let path = spool_path("drop");
        let sink = FrameSink::spawn(path.clone()).unwrap();
        sink.push(captured(1, 1, true));
        assert!(sink.flush(Duration::from_secs(5)));
        drop(sink);
        for _ in 0..100 {
            if !path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("spool file was not removed");
    }
}
