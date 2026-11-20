//! Shared chunk mapping for Linux md and dm-raid RAID10 layouts.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    Near,
    Far,
    Offset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Layout {
    pub format: Format,
    pub copies: u8,
    #[serde(default)]
    pub far_sets: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Groups {
    All,
    Legacy,
    Fixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Geometry {
    pub near_copies: u8,
    pub far_copies: u8,
    pub far_offset: bool,
    pub groups: Groups,
}

impl Layout {
    pub(crate) const NEAR_PAIR: Self = Self {
        format: Format::Near,
        copies: 2,
        far_sets: false,
    };

    pub(crate) fn geometry(self) -> Geometry {
        let near = self.format == Format::Near;
        Geometry {
            near_copies: if near { self.copies } else { 1 },
            far_copies: if near { 1 } else { self.copies },
            far_offset: self.format == Format::Offset,
            groups: if self.far_sets {
                Groups::Fixed
            } else {
                Groups::All
            },
        }
    }

    pub(crate) fn replica(
        self,
        devices: usize,
        far_section_rows: u64,
        chunk: u64,
        copy: usize,
    ) -> (usize, u64) {
        self.geometry()
            .replica(devices, far_section_rows, chunk, copy)
    }
}

impl Geometry {
    pub(crate) fn from_md(code: u32) -> Option<Self> {
        let groups = match code >> 17 {
            0 => Groups::All,
            1 => Groups::Legacy,
            2 => Groups::Fixed,
            _ => return None,
        };
        Some(Self {
            near_copies: code as u8,
            far_copies: (code >> 8) as u8,
            far_offset: code & (1 << 16) != 0,
            groups,
        })
    }

    pub(crate) fn conventional(self) -> Option<Layout> {
        let (format, copies) = match (self.near_copies, self.far_copies, self.far_offset) {
            (2..=u8::MAX, 1, false) if self.groups == Groups::All => {
                (Format::Near, self.near_copies)
            }
            (1, 2..=u8::MAX, false) if self.groups != Groups::Legacy => {
                (Format::Far, self.far_copies)
            }
            (1, 2..=u8::MAX, true) if self.groups != Groups::Legacy => {
                (Format::Offset, self.far_copies)
            }
            _ => return None,
        };
        Some(Layout {
            format,
            copies,
            far_sets: self.groups == Groups::Fixed,
        })
    }

    pub(crate) fn copies(self) -> usize {
        usize::from(self.near_copies) * usize::from(self.far_copies)
    }

    pub(crate) fn valid(self, devices: usize) -> bool {
        (2..=devices).contains(&self.copies())
    }

    pub(crate) fn section_rows(self, member_chunks: u64) -> u64 {
        member_chunks / u64::from(self.far_copies)
    }

    pub(crate) fn logical_chunks(self, member_chunks: u64, devices: usize) -> Option<u64> {
        let logical: u64 = (u128::from(self.section_rows(member_chunks)) * devices as u128
            / u128::from(self.near_copies))
        .try_into()
        .ok()?;
        (logical != 0).then_some(logical)
    }

    pub(crate) fn replica(
        self,
        devices: usize,
        section_rows: u64,
        chunk: u64,
        copy: usize,
    ) -> (usize, u64) {
        let near = usize::from(self.near_copies);
        let far = usize::from(self.far_copies);
        let first = chunk * near as u64;
        let adjacent = (first % devices as u64) as usize + copy / far;
        let base = first / devices as u64;
        let far_copy = copy % far;
        let mut lane = adjacent % devices;
        let row = if self.far_offset {
            base * far as u64 + far_copy as u64
        } else {
            base + section_rows * far_copy as u64
        };
        let row = row + (adjacent / devices) as u64;
        if far_copy == 0 {
            return (lane, row);
        }
        if self.groups == Groups::All {
            return ((lane + near * far_copy) % devices, row);
        }
        let group = match self.groups {
            Groups::All => devices,
            Groups::Legacy => devices / far,
            Groups::Fixed => self.copies(),
        };
        // Match md's pre-wrap boundary test, including its legacy variant.
        let last = (devices / group - 1) * group;
        if near == 1 {
            let start = (lane / group * group).min(last);
            let width = if start == last { devices - last } else { group };
            return (start + (lane - start + far_copy) % width, row);
        }
        for _ in 0..far_copy {
            let next = lane + near;
            lane = if devices % group != 0 && next > last {
                last + (next - last) % (devices - last)
            } else {
                lane / group * group + next % group
            };
        }
        (lane, row)
    }

    pub(crate) fn non_overlapping(self, devices: usize, member_chunks: u64, logical: u64) -> bool {
        // A period of `devices` logical chunks advances every lane by the same
        // number of rows. Within each lane and row residue, the full array is
        // therefore represented by bounded intervals of consecutive periods.
        let period = devices as u64;
        let advance = u64::from(self.near_copies)
            * if self.far_offset {
                u64::from(self.far_copies)
            } else {
                1
            };
        let mut intervals = Vec::new();
        for chunk in 0..logical.min(period) {
            let repeats = (logical - 1 - chunk) / period;
            for copy in 0..self.copies() {
                let (lane, row) =
                    self.replica(devices, self.section_rows(member_chunks), chunk, copy);
                if lane >= devices
                    || u128::from(row) + u128::from(repeats) * u128::from(advance)
                        >= u128::from(member_chunks)
                {
                    return false;
                }
                intervals.push((
                    lane,
                    row % advance,
                    row / advance,
                    row / advance + repeats,
                    chunk,
                ));
            }
        }
        intervals.sort_unstable();
        intervals.dedup();
        intervals.windows(2).all(|pair| {
            let (lane, residue, _, end, _) = pair[0];
            let (next_lane, next_residue, start, _, _) = pair[1];
            lane != next_lane || residue != next_residue || end < start
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advanced_offsets_and_legacy_groups_require_non_overlapping_bounded_rows() {
        let offset = Geometry::from_md(0x10202).unwrap();
        assert_eq!(offset.logical_chunks(12, 6), Some(18));
        assert_eq!(offset.replica(6, 6, 4, 3), (5, 3));
        assert!(offset.non_overlapping(6, 12, 18));
        assert!(!offset.non_overlapping(5, 12, 15));
        let legacy = Geometry::from_md(0x20201).unwrap();
        let fixed = Geometry::from_md(0x40201).unwrap();
        assert_eq!(legacy.replica(6, 6, 1, 1), (2, 6));
        assert_eq!(fixed.replica(6, 6, 1, 1), (0, 6));
        assert!(legacy.non_overlapping(6, 12, 36));
        let colliding = Geometry::from_md(0x20202).unwrap();
        assert!(!colliding.non_overlapping(5, 12, 15));
        let large = 1 << 40;
        assert!(offset.non_overlapping(6, large, offset.logical_chunks(large, 6).unwrap()));
    }
}
