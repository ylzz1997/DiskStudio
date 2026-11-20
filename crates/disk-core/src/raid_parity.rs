//! Linux md parity positions and bounded stripe reconstruction.
use crate::lvm::MAX_MEMBERS;
use crate::raid_field::products;
use std::io;

pub(crate) struct Row {
    devices: usize,
    layout: u32,
    p: usize,
    q: Option<usize>,
}

impl Row {
    pub(crate) fn new(row: u64, devices: usize, level: u32, layout: u32) -> Self {
        let rotate = |width: usize| (row % width as u64) as usize;
        let rotation = rotate(devices);
        let (p, q) = if level == 5 {
            let p = match layout {
                0 | 2 => devices - 1 - rotation,
                1 | 3 => rotation,
                4 => 0,
                _ => devices - 1,
            };
            (p, None)
        } else {
            let (p, q) = match layout {
                0..=3 | 8 | 9 => {
                    let p = match layout {
                        0 | 2 => devices - 1 - rotation,
                        9 => devices - 1 - (rotation + 1) % devices,
                        _ => rotation,
                    };
                    (p, (p + 1) % devices)
                }
                4 => (0, 1),
                5 => (devices - 2, devices - 1),
                10 => {
                    let p = devices - 1 - rotation;
                    (p, (p + devices - 1) % devices)
                }
                16 | 18 => (devices - 2 - rotate(devices - 1), devices - 1),
                17 | 19 => (rotate(devices - 1), devices - 1),
                20 => (0, devices - 1),
                _ => unreachable!("unsupported RAID6 layout"),
            };
            (p, Some(q))
        };
        Self {
            devices,
            layout,
            p,
            q,
        }
    }

    pub(crate) fn data_lane(&self, column: usize) -> usize {
        match (self.layout, self.q) {
            (2 | 3, None) | (10, Some(_)) => (self.p + 1 + column) % self.devices,
            (2 | 3, Some(_)) => (self.p + 2 + column) % self.devices,
            (18 | 19, Some(_)) => (self.p + 1 + column) % (self.devices - 1),
            (0 | 1 | 8 | 9, Some(0)) => column + 1,
            (0 | 1 | 4 | 8 | 9, Some(_)) => column + 2 * usize::from(column >= self.p),
            _ => column + usize::from(column >= self.p),
        }
    }

    fn coefficients(&self, field: &[[u8; 256]; 256]) -> [u8; MAX_MEMBERS] {
        let mut table = [0; MAX_MEMBERS];
        if let Some(q) = self.q {
            let ddf = matches!(self.layout, 8..=10);
            let first = if ddf { 0 } else { (q + 1) % self.devices };
            let mut coefficient = 1;
            for index in 0..self.devices {
                let lane = (first + index) % self.devices;
                if lane != self.p && lane != q {
                    table[lane] = coefficient;
                }
                // DDF includes zero-valued positions for both parity lanes.
                if ddf || table[lane] != 0 {
                    coefficient = field[2][coefficient as usize];
                }
            }
        }
        table
    }

    pub(crate) fn verify_sample(
        &self,
        size: usize,
        mut read: impl FnMut(usize, &mut [u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let field = products();
        let coefficient = self.coefficients(field);
        let mut p = vec![0; size];
        let mut q = vec![0; if self.q.is_some() { size } else { 0 }];
        let mut scratch = vec![0; size];
        for (lane, &factor) in coefficient.iter().enumerate().take(self.devices) {
            read(lane, &mut scratch)?;
            if Some(lane) != self.q {
                xor(&mut p, &scratch);
            }
            if Some(lane) == self.q {
                xor(&mut q, &scratch);
            } else if lane != self.p {
                for (sum, &byte) in q.iter_mut().zip(&scratch) {
                    *sum ^= field[factor as usize][byte as usize];
                }
            }
        }
        if p.iter().chain(&q).any(|&byte| byte != 0) {
            return Err(io::Error::other("RAID 条带校验样本不一致。"));
        }
        Ok(())
    }

    pub(crate) fn rebuild<R: ?Sized, W>(
        &self,
        readers: &mut R,
        read: impl Fn(&mut R, &W, &mut [u8]) -> io::Result<()>,
        windows: &[Option<W>],
        target: usize,
        out: &mut [u8],
        scratch: &mut Vec<u8>,
    ) -> io::Result<()> {
        let field = products();
        let coefficient = self.coefficients(field);
        let mut other = None;
        let p_ready = windows[self.p].is_some();
        let q_ready = self.q.is_some_and(|q| windows[q].is_some());
        let mut p_sum = vec![0; if p_ready { out.len() } else { 0 }];
        let mut q_sum = vec![0; if q_ready { out.len() } else { 0 }];
        scratch.resize(out.len(), 0);
        for (lane, window) in windows.iter().enumerate() {
            if lane == self.p || Some(lane) == self.q || lane == target {
                continue;
            }
            if let Some(window) = window {
                read(readers, window, scratch)?;
                xor(&mut p_sum, scratch);
                for (sum, &byte) in q_sum.iter_mut().zip(scratch.iter()) {
                    *sum ^= field[coefficient[lane] as usize][byte as usize];
                }
            } else if other.replace(lane).is_some() {
                return Err(io::Error::other("Too many missing RAID data columns"));
            }
        }
        if let Some(p) = &windows[self.p] {
            read(readers, p, scratch)?;
            xor(&mut p_sum, scratch);
        }
        if let Some(q) = self.q.and_then(|q| windows[q].as_ref()) {
            read(readers, q, scratch)?;
            xor(&mut q_sum, scratch);
        }
        if !q_ready && p_ready && other.is_none() {
            out.copy_from_slice(&p_sum);
            return Ok(());
        }
        if !q_ready || (!p_ready && other.is_some()) {
            return Err(io::Error::other(
                "RAID parity required to rebuild this range is absent",
            ));
        }
        let b = other.map_or(0, |lane| coefficient[lane]);
        let divisor = coefficient[target] ^ b;
        let inverse = field[divisor as usize]
            .iter()
            .position(|&value| value == 1)
            .ok_or_else(|| io::Error::other("Invalid RAID6 coefficients"))?;
        for index in 0..out.len() {
            let p = p_sum.get(index).copied().unwrap_or(0);
            let rebuilt = field[inverse][(q_sum[index] ^ field[b as usize][p as usize]) as usize];
            if p_ready && other.is_none() && rebuilt != p {
                return Err(io::Error::other("RAID 校验结果不一致。"));
            }
            out[index] = rebuilt;
        }
        Ok(())
    }
}

fn xor(sum: &mut [u8], buffer: &[u8]) {
    for (sum, byte) in sum.iter_mut().zip(buffer) {
        *sum ^= byte;
    }
}

#[cfg(test)]
mod tests {
    use super::Row;

    #[test]
    fn md_raid6_layouts_restore_data_with_any_two_missing_members() {
        // Each row contains logical data [3, 39, 211], their XOR, and the
        // Linux md Q syndrome in the physical order for that layout.
        let vectors = [
            (0, 0, [38, 3, 39, 211, 247], [1, 2, 3]),
            (1, 1, [3, 247, 144, 39, 211], [0, 3, 4]),
            (2, 2, [39, 211, 247, 38, 3], [4, 0, 1]),
            (3, 4, [38, 3, 39, 211, 247], [1, 2, 3]),
            (4, 0, [247, 38, 3, 39, 211], [2, 3, 4]),
            (5, 0, [3, 39, 211, 247, 38], [0, 1, 2]),
            (16, 1, [3, 39, 247, 211, 38], [0, 1, 3]),
            (17, 1, [3, 247, 39, 211, 38], [0, 2, 3]),
            (18, 1, [39, 211, 247, 3, 144], [3, 0, 1]),
            (19, 1, [211, 247, 3, 39, 73], [2, 3, 0]),
            (20, 0, [247, 3, 39, 211, 38], [1, 2, 3]),
        ];
        for (layout, index, mut members, expected_lanes) in vectors {
            let row = Row::new(index, members.len(), 6, layout);
            assert_eq!(
                [0, 1, 2].map(|column| row.data_lane(column)),
                expected_lanes,
                "layout {layout}"
            );
            row.verify_sample(1, |lane, out| {
                out[0] = members[lane];
                Ok(())
            })
            .unwrap_or_else(|error| panic!("layout {layout}: {error}"));
            for target in expected_lanes {
                for second in 0..members.len() {
                    let windows = [0, 1, 2, 3, 4]
                        .map(|lane| (lane != target && lane != second).then_some(lane));
                    let mut out = [0];
                    row.rebuild(
                        &mut members,
                        |readers, &lane, out| {
                            out[0] = readers[lane];
                            Ok(())
                        },
                        &windows,
                        target,
                        &mut out,
                        &mut Vec::new(),
                    )
                    .unwrap_or_else(|error| {
                        panic!("layout {layout}, missing {target}/{second}: {error}")
                    });
                    assert_eq!(
                        out[0], members[target],
                        "layout {layout}, missing {target}/{second}"
                    );
                }
            }
        }
    }

    #[test]
    fn independent_syndromes_must_agree_when_one_raid6_data_lane_is_missing() {
        let row = Row::new(0, 5, 6, 8);
        let mut readers = [9u8, 152, 17, 0, 123].map(|byte| [byte]);
        let windows: Vec<_> = (0..5)
            .map(|member| (member != 3).then_some(member))
            .collect();
        let mut output = [0];
        let mut scratch = Vec::new();
        row.rebuild(
            &mut readers,
            |readers, member, out| {
                out.copy_from_slice(&readers[*member]);
                Ok(())
            },
            &windows,
            3,
            &mut output,
            &mut scratch,
        )
        .unwrap();
        assert_eq!(output, [99]);
        readers[0][0] ^= 1;
        assert!(row
            .rebuild(
                &mut readers,
                |readers, member, out| {
                    out.copy_from_slice(&readers[*member]);
                    Ok(())
                },
                &windows,
                3,
                &mut output,
                &mut scratch
            )
            .is_err());
    }
}
