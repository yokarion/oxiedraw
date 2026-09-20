//! Frame records: the unit the recorder appends and the exporter replays.
//!
//! A record stores only the 64x64 tiles that changed since the previous frame,
//! as the byte-wise difference from it, so the untouched pixels inside a
//! changed tile become zeros. The tiles are stacked into one tall RGBA image and
//! PNG-compressed. A keyframe is the same record taken against an empty frame.

use std::io::{self, Read};

use super::RecordingError;

pub const TILE: u32 = 64;
pub const HEADER_LEN: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Key,
    Delta,
    /// Same pixels as the previous frame, kept only to hold time.
    Repeat,
}

impl FrameKind {
    const fn to_byte(self) -> u8 {
        match self {
            Self::Key => 0,
            Self::Delta => 1,
            Self::Repeat => 2,
        }
    }

    const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Key),
            1 => Some(Self::Delta),
            2 => Some(Self::Repeat),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: FrameKind,
    /// How many times the canvas was halved for this frame (0 = full size).
    pub halvings: u8,
    /// Position in the stream, wrapping. A delta whose predecessor is missing
    /// (a segment the project lost) is skipped instead of decoded onto the
    /// wrong frame.
    pub sequence: u16,
    pub width: u32,
    pub height: u32,
    /// Capture time, Unix milliseconds.
    pub time_ms: u64,
    pub payload_len: u32,
}

impl FrameHeader {
    fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.payload_len.to_le_bytes());
        out[4] = self.kind.to_byte();
        out[5] = self.halvings;
        out[6..8].copy_from_slice(&self.sequence.to_le_bytes());
        out[8..12].copy_from_slice(&self.width.to_le_bytes());
        out[12..16].copy_from_slice(&self.height.to_le_bytes());
        out[16..24].copy_from_slice(&self.time_ms.to_le_bytes());
        out
    }

    /// Read the next header, or `None` at a clean end of stream.
    pub fn read(reader: &mut impl Read) -> Result<Option<Self>, RecordingError> {
        let mut buf = [0u8; HEADER_LEN];
        let mut filled = 0;
        while filled < HEADER_LEN {
            match reader.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        if filled == 0 {
            return Ok(None);
        }
        if filled < HEADER_LEN {
            return Err(RecordingError::Corrupt("truncated frame header"));
        }
        let u32_at = |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        let kind = FrameKind::from_byte(buf[4]).ok_or(RecordingError::Corrupt("unknown frame kind"))?;
        let mut time = [0u8; 8];
        time.copy_from_slice(&buf[16..24]);
        Ok(Some(Self {
            kind,
            halvings: buf[5],
            sequence: u16::from_le_bytes([buf[6], buf[7]]),
            width: u32_at(8),
            height: u32_at(12),
            time_ms: u64::from_le_bytes(time),
            payload_len: u32_at(0),
        }))
    }
}

/// A decoded frame: premultiplied BGRA8, row-major, no padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub enum Encoded {
    Record(Vec<u8>),
    Unchanged,
}

#[derive(Default)]
pub struct FrameEncoder {
    previous: Option<(Frame, u8)>,
    sequence: u16,
}

impl FrameEncoder {
    /// Make the next frame a keyframe.
    pub fn restart(&mut self) {
        self.previous = None;
    }

    fn next_sequence(&mut self) -> u16 {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        sequence
    }

    pub fn encode(&mut self, frame: Frame, halvings: u8, time_ms: u64) -> Result<Encoded, RecordingError> {
        let expected = frame.width as usize * frame.height as usize * 4;
        if frame.pixels.len() != expected {
            return Err(RecordingError::Corrupt("frame buffer does not match its size"));
        }
        let base = self
            .previous
            .as_ref()
            .filter(|(p, h)| p.width == frame.width && p.height == frame.height && *h == halvings)
            .map(|(p, _)| p.pixels.as_slice());
        let grid = TileGrid::new(frame.width, frame.height);
        let changed: Vec<u32> = (0..grid.count())
            .filter(|&t| grid.differs(t, &frame.pixels, base))
            .collect();
        if base.is_some() && changed.is_empty() {
            return Ok(Encoded::Unchanged);
        }
        let kind = if base.is_some() { FrameKind::Delta } else { FrameKind::Key };
        let payload = encode_tiles(&grid, &changed, &frame.pixels, base)?;
        let sequence = self.next_sequence();
        let record = record_bytes(kind, halvings, sequence, frame.width, frame.height, time_ms, &payload)?;
        self.previous = Some((frame, halvings));
        Ok(Encoded::Record(record))
    }

    /// A record that holds the previous frame for one more tick.
    pub fn repeat(&mut self, time_ms: u64) -> Option<Vec<u8>> {
        let (width, height, halvings) = self.previous.as_ref().map(|(p, h)| (p.width, p.height, *h))?;
        let sequence = self.next_sequence();
        record_bytes(FrameKind::Repeat, halvings, sequence, width, height, time_ms, &[]).ok()
    }
}

fn record_bytes(
    kind: FrameKind,
    halvings: u8,
    sequence: u16,
    width: u32,
    height: u32,
    time_ms: u64,
    payload: &[u8],
) -> Result<Vec<u8>, RecordingError> {
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| RecordingError::Corrupt("frame too large"))?;
    let header = FrameHeader { kind, halvings, sequence, width, height, time_ms, payload_len };
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[derive(Default)]
pub struct FrameDecoder {
    current: Option<Frame>,
    /// Sequence the next record must carry to build on [`Self::current`].
    expected: Option<u16>,
}

impl FrameDecoder {
    /// Apply one record. `None` when a delta or repeat has no frame to build on:
    /// the stream skipped one, or the size changed. Decoding resumes at the next
    /// keyframe.
    pub fn apply(&mut self, header: &FrameHeader, payload: &[u8]) -> Result<Option<&Frame>, RecordingError> {
        let follows = |f: &Frame, expected: Option<u16>| {
            f.width == header.width && f.height == header.height && expected == Some(header.sequence)
        };
        match header.kind {
            FrameKind::Key => {
                let len = header.width as usize * header.height as usize * 4;
                let mut frame = Frame { width: header.width, height: header.height, pixels: vec![0; len] };
                apply_tiles(&mut frame, payload)?;
                self.current = Some(frame);
            }
            FrameKind::Delta => {
                let expected = self.expected;
                match self.current.as_mut() {
                    Some(frame) if follows(frame, expected) => apply_tiles(frame, payload)?,
                    _ => {
                        self.expected = None;
                        return Ok(None);
                    }
                }
            }
            FrameKind::Repeat => {
                if !self.current.as_ref().is_some_and(|f| follows(f, self.expected)) {
                    self.expected = None;
                    return Ok(None);
                }
            }
        }
        self.expected = Some(header.sequence.wrapping_add(1));
        Ok(self.current.as_ref())
    }
}

struct TileGrid {
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
}

impl TileGrid {
    const fn new(width: u32, height: u32) -> Self {
        Self { width, height, cols: width.div_ceil(TILE), rows: height.div_ceil(TILE) }
    }

    const fn count(&self) -> u32 {
        self.cols * self.rows
    }

    /// `(byte offset of the first row, bytes per row, row count)` of tile `t`.
    fn span(&self, t: u32) -> (usize, usize, usize) {
        let x = (t % self.cols) * TILE;
        let y = (t / self.cols) * TILE;
        let w = TILE.min(self.width - x) as usize;
        let h = TILE.min(self.height - y) as usize;
        ((y as usize * self.width as usize + x as usize) * 4, w * 4, h)
    }

    fn stride(&self) -> usize {
        self.width as usize * 4
    }

    fn differs(&self, t: u32, pixels: &[u8], base: Option<&[u8]>) -> bool {
        let (start, row_len, rows) = self.span(t);
        (0..rows).any(|r| {
            let at = start + r * self.stride();
            let row = &pixels[at..at + row_len];
            base.map_or_else(|| row.iter().any(|&b| b != 0), |b| row != &b[at..at + row_len])
        })
    }
}

const TILE_BYTES: usize = (TILE * TILE * 4) as usize;

fn encode_tiles(
    grid: &TileGrid,
    changed: &[u32],
    pixels: &[u8],
    base: Option<&[u8]>,
) -> Result<Vec<u8>, RecordingError> {
    let count = u32::try_from(changed.len()).map_err(|_| RecordingError::Corrupt("too many tiles"))?;
    let mut out = Vec::new();
    out.extend_from_slice(&count.to_le_bytes());
    let mut bitmap = vec![0u8; (grid.count() as usize).div_ceil(8)];
    for &t in changed {
        bitmap[t as usize / 8] |= 1 << (t % 8);
    }
    out.extend_from_slice(&bitmap);
    if changed.is_empty() {
        return Ok(out);
    }

    let mut strip = vec![0u8; TILE_BYTES * changed.len()];
    let tile_row = TILE as usize * 4;
    for (i, &t) in changed.iter().enumerate() {
        let (start, row_len, rows) = grid.span(t);
        for r in 0..rows {
            let src = start + r * grid.stride();
            let dst = i * TILE_BYTES + r * tile_row;
            let cur = &pixels[src..src + row_len];
            let out_row = &mut strip[dst..dst + row_len];
            match base {
                Some(b) => {
                    for ((o, &c), &p) in out_row.iter_mut().zip(cur).zip(&b[src..src + row_len]) {
                        *o = c.wrapping_sub(p);
                    }
                }
                None => out_row.copy_from_slice(cur),
            }
        }
    }

    let strip_height = TILE * count;
    let mut encoder = png::Encoder::new(&mut out, TILE, strip_height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Default);
    let mut writer = encoder.write_header().map_err(|e| RecordingError::Png(e.to_string()))?;
    writer
        .write_image_data(&strip)
        .map_err(|e| RecordingError::Png(e.to_string()))?;
    writer.finish().map_err(|e| RecordingError::Png(e.to_string()))?;
    Ok(out)
}

fn apply_tiles(frame: &mut Frame, payload: &[u8]) -> Result<(), RecordingError> {
    let grid = TileGrid::new(frame.width, frame.height);
    let bitmap_len = (grid.count() as usize).div_ceil(8);
    if payload.len() < 4 + bitmap_len {
        return Err(RecordingError::Corrupt("short tile table"));
    }
    let count = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    if count == 0 {
        return Ok(());
    }
    let bitmap = &payload[4..4 + bitmap_len];
    let tiles: Vec<u32> = (0..grid.count())
        .filter(|&t| bitmap[t as usize / 8] & (1 << (t % 8)) != 0)
        .collect();
    if tiles.len() != count {
        return Err(RecordingError::Corrupt("tile count does not match the bitmap"));
    }

    let decoder = png::Decoder::new(&payload[4 + bitmap_len..]);
    let mut reader = decoder.read_info().map_err(|e| RecordingError::Png(e.to_string()))?;
    let mut strip = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut strip)
        .map_err(|e| RecordingError::Png(e.to_string()))?;
    if info.width != TILE || info.height as usize != TILE as usize * count || info.color_type != png::ColorType::Rgba {
        return Err(RecordingError::Corrupt("tile strip has the wrong shape"));
    }

    let tile_row = TILE as usize * 4;
    let stride = grid.stride();
    for (i, &t) in tiles.iter().enumerate() {
        let (start, row_len, rows) = grid.span(t);
        for r in 0..rows {
            let dst = start + r * stride;
            let src = i * TILE_BYTES + r * tile_row;
            for (p, &d) in frame.pixels[dst..dst + row_len].iter_mut().zip(&strip[src..src + row_len]) {
                *p = p.wrapping_add(d);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, fill: impl Fn(u32, u32) -> [u8; 4]) -> Frame {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&fill(x, y));
            }
        }
        Frame { width, height, pixels }
    }

    fn decode_all(records: &[Vec<u8>]) -> Vec<Frame> {
        let mut stream: Vec<u8> = records.concat();
        let mut reader = io::Cursor::new(std::mem::take(&mut stream));
        let mut decoder = FrameDecoder::default();
        let mut out = Vec::new();
        while let Some(header) = FrameHeader::read(&mut reader).unwrap() {
            let mut payload = vec![0u8; header.payload_len as usize];
            reader.read_exact(&mut payload).unwrap();
            if let Some(f) = decoder.apply(&header, &payload).unwrap() {
                out.push(f.clone());
            }
        }
        out
    }

    fn record(encoded: Encoded) -> Vec<u8> {
        match encoded {
            Encoded::Record(r) => r,
            Encoded::Unchanged => panic!("expected a record"),
        }
    }

    #[test]
    fn key_then_delta_round_trips() {
        let a = frame(150, 70, |x, y| [x as u8, y as u8, 7, 255]);
        let mut b = a.clone();
        b.pixels[(10 * 150 + 140) * 4] = 99;
        let mut enc = FrameEncoder::default();
        let r1 = record(enc.encode(a.clone(), 1, 10).unwrap());
        let r2 = record(enc.encode(b.clone(), 1, 20).unwrap());
        assert_eq!(decode_all(&[r1, r2]), vec![a, b]);
    }

    #[test]
    fn delta_carries_only_changed_tiles() {
        let a = frame(256, 256, |_, _| [0, 0, 0, 0]);
        let mut b = a.clone();
        b.pixels[0] = 200;
        let mut enc = FrameEncoder::default();
        record(enc.encode(a, 0, 0).unwrap());
        let r = record(enc.encode(b, 0, 1).unwrap());
        let count = u32::from_le_bytes([r[HEADER_LEN], r[HEADER_LEN + 1], r[HEADER_LEN + 2], r[HEADER_LEN + 3]]);
        assert_eq!(count, 1);
    }

    #[test]
    fn identical_frame_is_unchanged_and_repeat_holds_it() {
        let a = frame(32, 32, |x, _| [x as u8, 0, 0, 255]);
        let mut enc = FrameEncoder::default();
        let r1 = record(enc.encode(a.clone(), 0, 0).unwrap());
        assert!(matches!(enc.encode(a.clone(), 0, 5).unwrap(), Encoded::Unchanged));
        let r2 = enc.repeat(5).unwrap();
        assert_eq!(decode_all(&[r1, r2]), vec![a.clone(), a]);
    }

    #[test]
    fn size_change_starts_a_new_keyframe() {
        let a = frame(40, 40, |_, _| [1, 2, 3, 255]);
        let b = frame(20, 30, |_, _| [9, 9, 9, 255]);
        let mut enc = FrameEncoder::default();
        let r1 = record(enc.encode(a.clone(), 0, 0).unwrap());
        let r2 = record(enc.encode(b.clone(), 0, 1).unwrap());
        assert_eq!(r2[4], FrameKind::Key.to_byte());
        assert_eq!(decode_all(&[r1, r2]), vec![a, b]);
    }

    #[test]
    fn empty_keyframe_still_sets_the_size() {
        let a = frame(64, 64, |_, _| [0, 0, 0, 0]);
        let mut enc = FrameEncoder::default();
        let r = record(enc.encode(a.clone(), 0, 0).unwrap());
        assert_eq!(decode_all(&[r]), vec![a]);
    }

    // A segment the project lost: the deltas after it must not be applied to the
    // frame before the gap, which would decode as garbage.
    #[test]
    fn deltas_after_a_missing_record_are_skipped_until_a_keyframe() {
        let mut enc = FrameEncoder::default();
        let a = frame(64, 64, |x, _| [x as u8, 0, 0, 255]);
        let mut b = a.clone();
        b.pixels[0] = 9;
        let mut c = b.clone();
        c.pixels[4] = 7;
        let first = record(enc.encode(a.clone(), 0, 0).unwrap());
        let _lost = record(enc.encode(b, 0, 1).unwrap());
        let after_gap = record(enc.encode(c.clone(), 0, 2).unwrap());
        assert_eq!(decode_all(&[first.clone(), after_gap.clone()]), vec![a], "the gap frame is dropped");

        let mut fresh = FrameEncoder::default();
        let key = record(fresh.encode(c.clone(), 0, 3).unwrap());
        assert_eq!(decode_all(&[first, after_gap, key]).last(), Some(&c), "a keyframe resumes decoding");
    }

    #[test]
    fn delta_without_a_base_is_skipped() {
        let a = frame(16, 16, |_, _| [5, 5, 5, 255]);
        let mut b = a.clone();
        b.pixels[3] = 0;
        let mut enc = FrameEncoder::default();
        record(enc.encode(a, 0, 0).unwrap());
        let delta = record(enc.encode(b, 0, 1).unwrap());
        assert!(decode_all(&[delta]).is_empty());
    }
}
