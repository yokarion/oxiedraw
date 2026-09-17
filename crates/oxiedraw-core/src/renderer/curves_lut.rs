//! Curves LUT atlas: one row per distinct `CurveSet`, found by content. A claim
//! round never evicts its own rows, since the batched preview reads them all in
//! one submit.

use ash::{Device, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::resources::{Buffer, Image};
use crate::curves::{CurveSet, LUT_SIZE};

const ATLAS_ROWS: usize = 32;

pub(super) const ROW_BYTES: usize = LUT_SIZE * 8;

pub(super) struct CurveLutAtlas {
    pub image: Image,
    pub staging: Buffer,
    pub rows: RowTable,
}

impl CurveLutAtlas {
    pub(super) fn new(device: &Device, allocator: &mut Allocator) -> Result<Self, RendererError> {
        let image = Image::new_2d(
            device,
            allocator,
            "curves-lut",
            vk::Format::R16G16B16A16_SFLOAT,
            vk::Extent2D {
                width: LUT_SIZE as u32,
                height: ATLAS_ROWS as u32,
            },
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        let staging = Buffer::new(
            device,
            allocator,
            "curves-lut-staging",
            (ROW_BYTES * ATLAS_ROWS) as vk::DeviceSize,
            vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::CpuToGpu,
        )?;
        Ok(Self {
            image,
            staging,
            rows: RowTable::new(ATLAS_ROWS),
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            self.staging.destroy(device, allocator);
            self.image.destroy(device, allocator);
        }
    }
}

struct Row {
    curves: CurveSet,
    round: u64,
}

pub(super) struct RowTable {
    rows: Vec<Option<Row>>,
    round: u64,
}

impl RowTable {
    fn new(len: usize) -> Self {
        Self {
            rows: (0..len).map(|_| None).collect(),
            round: 0,
        }
    }

    pub(super) fn find(&self, curves: &CurveSet) -> Option<u32> {
        self.rows
            .iter()
            .position(|row| row.as_ref().is_some_and(|r| r.curves == *curves))
            .map(|i| i as u32)
    }

    pub(super) const fn begin_round(&mut self) {
        self.round += 1;
    }

    /// `(row, needs upload)`, or `None` when every row is claimed this round.
    pub(super) fn claim(&mut self, curves: &CurveSet) -> Option<(u32, bool)> {
        let round = self.round;
        if let Some(row) = self.find(curves) {
            if let Some(r) = self.rows[row as usize].as_mut() {
                r.round = round;
            }
            return Some((row, false));
        }
        let free = self.rows.iter().position(Option::is_none);
        let row = free.or_else(|| {
            self.rows
                .iter()
                .enumerate()
                .filter_map(|(i, row)| row.as_ref().map(|r| (i, r.round)))
                .filter(|&(_, claimed)| claimed < round)
                .min_by_key(|&(_, claimed)| claimed)
                .map(|(i, _)| i)
        })?;
        self.rows[row] = Some(Row {
            curves: *curves,
            round,
        });
        Some((row as u32, true))
    }

    pub(super) fn release(&mut self, row: u32) {
        if let Some(slot) = self.rows.get_mut(row as usize) {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curves::{Curve, CurvePoint};

    fn set(black: u8) -> CurveSet {
        CurveSet {
            red: Curve::from_points(&[CurvePoint::new(0, black), CurvePoint::new(255, 255)]),
            ..CurveSet::default()
        }
    }

    #[test]
    fn a_known_set_reuses_its_row() {
        let mut table = RowTable::new(2);
        table.begin_round();
        assert_eq!(table.claim(&set(1)), Some((0, true)));
        table.begin_round();
        assert_eq!(table.claim(&set(1)), Some((0, false)));
        assert_eq!(table.find(&set(1)), Some(0));
    }

    #[test]
    fn eviction_takes_the_oldest_row_and_spares_this_round() {
        let mut table = RowTable::new(2);
        table.begin_round();
        table.claim(&set(1));
        table.begin_round();
        table.claim(&set(2));
        table.begin_round();
        assert_eq!(table.claim(&set(3)), Some((0, true)), "set 1 is the oldest");
        assert_eq!(table.find(&set(1)), None);
        assert_eq!(table.claim(&set(4)), Some((1, true)));
        assert_eq!(table.claim(&set(5)), None, "both rows are claimed this round");
        assert_eq!(table.find(&set(3)), Some(0));
    }

    #[test]
    fn released_rows_are_forgotten() {
        let mut table = RowTable::new(1);
        table.begin_round();
        table.claim(&set(1));
        table.release(0);
        assert_eq!(table.find(&set(1)), None);
        assert_eq!(table.claim(&set(2)), Some((0, true)));
    }
}
