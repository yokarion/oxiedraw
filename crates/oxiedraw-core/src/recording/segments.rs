//! Where recorded frames live on disk - entries in a project archive and a
//! range of the spool - plus reading them back in order and writing them into
//! a new archive on save.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::codec::{FrameHeader, HEADER_LEN};
use super::{MANIFEST_ENTRY, RecordingError, RecordingManifest, RecordingSettings, SEGMENT_DIR, SegmentEntry};

/// A byte range of an open file that holds whole frame records. Keeping the
/// file open means a save renaming a new project over it can't move the bytes.
#[derive(Debug, Clone)]
pub struct SegmentSource {
    file: Arc<File>,
    pub offset: u64,
    pub len: u64,
}

impl SegmentSource {
    #[must_use]
    pub fn new(file: File, offset: u64, len: u64) -> Self {
        Self { file: Arc::new(file), offset, len }
    }

    fn reader(&self) -> io::Result<SharedFile> {
        let mut reader = SharedFile(Arc::clone(&self.file));
        reader.seek(SeekFrom::Start(self.offset))?;
        Ok(reader)
    }
}

// Sources are read one after another, so sharing the file position is safe.
struct SharedFile(Arc<File>);

impl Read for SharedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(buf)
    }
}

impl Seek for SharedFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        (&*self.0).seek(pos)
    }
}

/// Locate the named entries in the project archive at `path`, keeping the order
/// of `entries`. Entries that are missing are left out.
pub fn find_in_archive(path: &Path, entries: &[String]) -> Result<Vec<SegmentSource>, RecordingError> {
    let mut found = locate(path, entries)?;
    Ok(entries.iter().filter_map(|e| found.remove(e)).collect())
}

fn locate(path: &Path, entries: &[String]) -> Result<HashMap<String, SegmentSource>, RecordingError> {
    let mut found = HashMap::new();
    if entries.is_empty() {
        return Ok(found);
    }
    let file = Arc::new(File::open(path)?);
    // Seeking past the data keeps a scan of a large recording to header reads.
    let mut archive = tar::Archive::new(&*file);
    for entry in archive.entries_with_seek()? {
        let entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if entries.contains(&name) {
            let source = SegmentSource {
                file: Arc::clone(&file),
                offset: entry.raw_file_position(),
                len: entry.size(),
            };
            found.insert(name, source);
        }
    }
    Ok(found)
}

/// Reads frame records across a list of segments, in order.
pub struct FrameReader {
    pending: VecDeque<SegmentSource>,
    current: Option<(BufReader<SharedFile>, u64)>,
}

impl FrameReader {
    #[must_use]
    pub fn new(sources: Vec<SegmentSource>) -> Self {
        Self { pending: sources.into(), current: None }
    }

    /// The next record's header, or `None` once every segment is exhausted.
    /// Follow it with [`Self::read_payload`] or [`Self::skip_payload`].
    pub fn next_header(&mut self) -> Result<Option<FrameHeader>, RecordingError> {
        loop {
            match self.current.as_mut() {
                Some((_, 0)) => self.current = None,
                Some((reader, left)) => {
                    if *left < HEADER_LEN as u64 {
                        return Err(RecordingError::Corrupt("segment ends inside a frame header"));
                    }
                    let header = FrameHeader::read(reader)?
                        .ok_or(RecordingError::Corrupt("segment shorter than its entry"))?;
                    *left -= HEADER_LEN as u64;
                    if u64::from(header.payload_len) > *left {
                        return Err(RecordingError::Corrupt("frame runs past its segment"));
                    }
                    return Ok(Some(header));
                }
                None => {
                    let Some(source) = self.pending.pop_front() else {
                        return Ok(None);
                    };
                    self.current = Some((BufReader::new(source.reader()?), source.len));
                }
            }
        }
    }

    pub fn read_payload(&mut self, header: &FrameHeader) -> Result<Vec<u8>, RecordingError> {
        let (reader, left) = self.current.as_mut().ok_or(RecordingError::Corrupt("no open segment"))?;
        let mut payload = vec![0u8; header.payload_len as usize];
        reader.read_exact(&mut payload)?;
        *left -= u64::from(header.payload_len);
        Ok(payload)
    }

    pub fn skip_payload(&mut self, header: &FrameHeader) -> Result<(), RecordingError> {
        let (reader, left) = self.current.as_mut().ok_or(RecordingError::Corrupt("no open segment"))?;
        reader.seek_relative(i64::from(header.payload_len))?;
        *left -= u64::from(header.payload_len);
        Ok(())
    }
}

/// Frames recorded since the last save: a committed byte range of the spool.
pub struct FreshFrames {
    pub source: SegmentSource,
    pub frames: u64,
}

/// Everything a save needs to write the recording into a project archive.
pub struct RecordingPayload {
    pub settings: RecordingSettings,
    /// The archive holding the segments saved so far.
    pub source: Option<PathBuf>,
    pub saved: Vec<SegmentEntry>,
    pub fresh: Option<FreshFrames>,
}

impl RecordingPayload {
    fn is_empty(&self) -> bool {
        self.saved.is_empty()
            && self.fresh.as_ref().is_none_or(|f| f.source.len == 0)
            && self.settings == RecordingSettings::default()
    }
}

/// Append `recording.json` and the segments, copying the saved ones out of the
/// source archive (dropping any it lost). Returns the manifest written.
pub fn write_to_archive<W: Write>(
    archive: &mut tar::Builder<W>,
    payload: &RecordingPayload,
) -> Result<Option<RecordingManifest>, RecordingError> {
    if payload.is_empty() {
        return Ok(None);
    }
    let paths: Vec<String> = payload.saved.iter().map(|s| entry_path(&s.name)).collect();
    let mut sources = payload.source.as_deref().map_or_else(HashMap::new, |path| {
        locate(path, &paths).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), err = %e, "recording: saved segments unreadable");
            HashMap::new()
        })
    });
    let mut carried: Vec<(SegmentEntry, SegmentSource)> = Vec::new();
    for (entry, path) in payload.saved.iter().zip(&paths) {
        match sources.remove(path) {
            Some(source) if source.len == entry.bytes => carried.push((entry.clone(), source)),
            _ => tracing::warn!(entry = %path, "recording: segment missing from the source, dropped"),
        }
    }

    let mut manifest = RecordingManifest {
        settings: payload.settings,
        segments: carried.iter().map(|(e, _)| e.clone()).collect(),
    };
    let fresh = payload.fresh.as_ref().filter(|f| f.source.len > 0);
    if let Some(fresh) = fresh {
        manifest.segments.push(SegmentEntry {
            name: next_segment_name(&manifest.segments),
            frames: fresh.frames,
            bytes: fresh.source.len,
        });
        carried.push((manifest.segments[manifest.segments.len() - 1].clone(), fresh.source.clone()));
    }

    let json = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| RecordingError::Io(io::Error::other(e)))?;
    append(archive, MANIFEST_ENTRY, json.len() as u64, Cursor::new(json))?;
    for (entry, source) in &carried {
        append(archive, &entry_path(&entry.name), source.len, source.reader()?)?;
    }
    Ok(Some(manifest))
}

/// Archive path of a segment named in the manifest.
#[must_use]
pub fn entry_path(name: &str) -> String {
    format!("{SEGMENT_DIR}{name}")
}

fn next_segment_name(existing: &[SegmentEntry]) -> String {
    let next = existing
        .iter()
        .filter_map(|s| s.name.strip_prefix("seg-")?.strip_suffix(".bin")?.parse::<u32>().ok())
        .max()
        .map_or(0, |n| n + 1);
    format!("seg-{next:05}.bin")
}

fn append<W: Write>(
    archive: &mut tar::Builder<W>,
    name: &str,
    len: u64,
    data: impl Read,
) -> Result<(), RecordingError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(len);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, name, Exact { inner: data, left: len })?;
    Ok(())
}

// tar pads a short source with nothing and writes a corrupt archive; fail the
// save instead.
struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let max = usize::try_from(self.left).unwrap_or(usize::MAX).min(buf.len());
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "recording segment ended early"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::recording::codec::{Encoded, Frame, FrameEncoder};

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oxiedraw_segments_{}_{tag}", std::process::id()))
    }

    fn records(values: &[u8], start_ms: u64) -> Vec<u8> {
        let mut enc = FrameEncoder::default();
        let mut out = Vec::new();
        for (i, &v) in values.iter().enumerate() {
            let frame = Frame { width: 4, height: 4, pixels: vec![v; 64] };
            if let Encoded::Record(r) = enc.encode(frame, 0, start_ms + i as u64).unwrap() {
                out.extend_from_slice(&r);
            }
        }
        out
    }

    fn fresh(tag: &str, bytes: &[u8], frames: u64) -> FreshFrames {
        let path = temp(tag);
        std::fs::write(&path, bytes).unwrap();
        FreshFrames { source: SegmentSource::new(File::open(&path).unwrap(), 0, bytes.len() as u64), frames }
    }

    fn write_archive(path: &Path, payload: &RecordingPayload) -> Option<RecordingManifest> {
        let mut builder = tar::Builder::new(File::create(path).unwrap());
        let manifest = write_to_archive(&mut builder, payload).unwrap();
        builder.finish().unwrap();
        manifest
    }

    fn count_frames(sources: Vec<SegmentSource>) -> u64 {
        let mut reader = FrameReader::new(sources);
        let mut n = 0;
        while let Some(h) = reader.next_header().unwrap() {
            reader.skip_payload(&h).unwrap();
            n += 1;
        }
        n
    }

    #[test]
    fn default_settings_and_no_frames_write_nothing() {
        let path = temp("empty.tar");
        let payload = RecordingPayload {
            settings: RecordingSettings::default(),
            source: None,
            saved: Vec::new(),
            fresh: None,
        };
        assert!(write_archive(&path, &payload).is_none());
    }

    #[test]
    fn second_save_carries_the_first_segment_and_adds_one() {
        let first = temp("first.tar");
        let bytes = records(&[1, 2, 3], 0);
        let m1 = write_archive(
            &first,
            &RecordingPayload {
                settings: RecordingSettings::default(),
                source: None,
                saved: Vec::new(),
                fresh: Some(fresh("fresh1", &bytes, 3)),
            },
        )
        .unwrap();
        assert_eq!(m1.segments.len(), 1);
        assert_eq!(m1.segments[0].name, "seg-00000.bin");

        let second = temp("second.tar");
        let more = records(&[4, 5], 10);
        let m2 = write_archive(
            &second,
            &RecordingPayload {
                settings: RecordingSettings::default(),
                source: Some(first.clone()),
                saved: m1.segments.clone(),
                fresh: Some(fresh("fresh2", &more, 2)),
            },
        )
        .unwrap();
        let names: Vec<String> = m2.segments.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, ["seg-00000.bin", "seg-00001.bin"]);

        let found = find_in_archive(&second, &names.iter().map(|n| entry_path(n)).collect::<Vec<_>>()).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(count_frames(found), 5);
    }

    // An export reading a project while a save renames a new file over it.
    #[test]
    fn located_segments_survive_the_file_being_replaced() {
        let archive = temp("replaced.tar");
        let payload = RecordingPayload {
            settings: RecordingSettings::default(),
            source: None,
            saved: Vec::new(),
            fresh: Some(fresh("replaced_fresh", &records(&[1, 2], 0), 2)),
        };
        let manifest = write_archive(&archive, &payload).unwrap();
        let found = find_in_archive(&archive, &[entry_path(&manifest.segments[0].name)]).unwrap();

        let other = temp("replacement");
        std::fs::write(&other, vec![0xAB; 4096]).unwrap();
        std::fs::rename(&other, &archive).unwrap();
        assert_eq!(count_frames(found), 2);
    }

    #[test]
    fn a_segment_gone_from_the_source_is_dropped() {
        let path = temp("gone.tar");
        let manifest = write_archive(
            &path,
            &RecordingPayload {
                settings: RecordingSettings::default(),
                source: Some(temp("does-not-exist.tar")),
                saved: vec![SegmentEntry { name: "seg-00000.bin".into(), frames: 1, bytes: 10 }],
                fresh: None,
            },
        )
        .unwrap();
        assert!(manifest.segments.is_empty());
    }

    #[test]
    fn short_source_fails_instead_of_writing_a_corrupt_archive() {
        let path = temp("short.tar");
        let mut f = fresh("short", &records(&[1], 0), 1);
        f.source.len += 100;
        let mut builder = tar::Builder::new(File::create(&path).unwrap());
        let payload = RecordingPayload {
            settings: RecordingSettings::default(),
            source: None,
            saved: Vec::new(),
            fresh: Some(f),
        };
        assert!(write_to_archive(&mut builder, &payload).is_err());
    }
}
