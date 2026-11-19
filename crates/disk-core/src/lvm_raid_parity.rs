//! Linux md parity positions and bounded stripe reconstruction.
use super::*;
use crate::raid_field::products;

pub(super) struct Row {
    devices: usize,
    layout: u32,
    p: usize,
    q: Option<usize>,
}

impl Row {
    pub(super) fn new(row: u64, devices: usize, level: u32, layout: u32) -> Self {
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
                8 | 9 => {
                    let p = if layout == 8 {
                        rotation
                    } else {
                        devices - 1 - (rotation + 1) % devices
                    };
                    (p, (p + 1) % devices)
                }
                10 => {
                    let p = devices - 1 - rotation;
                    (p, (p + devices - 1) % devices)
                }
                16 | 18 => (devices - 2 - rotate(devices - 1), devices - 1),
                17 | 19 => (rotate(devices - 1), devices - 1),
                _ => (devices - 2, devices - 1),
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

    pub(super) fn data_lane(&self, column: usize) -> usize {
        match (self.layout, self.q) {
            (2 | 3, None) | (10, Some(_)) => (self.p + 1 + column) % self.devices,
            (18 | 19, Some(_)) => (self.p + 1 + column) % (self.devices - 1),
            (8 | 9, Some(0)) => column + 1,
            (8 | 9, Some(_)) => column + 2 * usize::from(column >= self.p),
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

    pub(super) fn rebuild(
        &self,
        readers: &mut [impl Read + Seek],
        windows: &[Option<Window>],
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
                window.read(readers, scratch)?;
                xor(&mut p_sum, scratch);
                for (sum, &byte) in q_sum.iter_mut().zip(scratch.iter()) {
                    *sum ^= field[coefficient[lane] as usize][byte as usize];
                }
            } else if other.replace(lane).is_some() {
                return Err(io::Error::other("Too many missing LVM2 data columns"));
            }
        }
        if let Some(p) = &windows[self.p] {
            p.read(readers, scratch)?;
            xor(&mut p_sum, scratch);
        }
        if let Some(q) = self.q.and_then(|q| windows[q].as_ref()) {
            q.read(readers, scratch)?;
            xor(&mut q_sum, scratch);
        }
        if !q_ready && p_ready && other.is_none() {
            out.copy_from_slice(&p_sum);
            return Ok(());
        }
        if !q_ready || (!p_ready && other.is_some()) {
            return Err(io::Error::other(
                "LVM2 parity required to rebuild this range is absent",
            ));
        }
        let b = other.map_or(0, |lane| coefficient[lane]);
        let divisor = coefficient[target] ^ b;
        let inverse = field[divisor as usize]
            .iter()
            .position(|&value| value == 1)
            .ok_or_else(|| io::Error::other("Invalid LVM2 RAID6 coefficients"))?;
        for index in 0..out.len() {
            let p = p_sum.get(index).copied().unwrap_or(0);
            let rebuilt = field[inverse][(q_sum[index] ^ field[b as usize][p as usize]) as usize];
            if p_ready && other.is_none() && rebuilt != p {
                return Err(io::Error::other("LVM2 RAID 校验结果不一致。"));
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
    use super::*;
    use std::io::Cursor;

    #[test]
    fn independent_syndromes_must_agree_when_one_raid6_data_lane_is_missing() {
        let row = Row::new(0, 5, 6, 8);
        let mut readers = [9u8, 152, 17, 0, 123].map(|byte| Cursor::new([byte]));
        let windows: Vec<_> = (0..5)
            .map(|member| {
                (member != 3).then_some(Window {
                    member,
                    offset: 0,
                    available: 1,
                })
            })
            .collect();
        let mut output = [0];
        let mut scratch = Vec::new();
        row.rebuild(&mut readers, &windows, 3, &mut output, &mut scratch)
            .unwrap();
        assert_eq!(output, [99]);
        readers[0].get_mut()[0] ^= 1;
        assert!(row
            .rebuild(&mut readers, &windows, 3, &mut output, &mut scratch)
            .is_err());
    }
}
