//! STREAMINFO checks PCM interleaved in sample order, signed and byte aligned.
use super::{require, PResult};
use md5::{Digest, Md5};

pub(super) fn fits(sample: i64, width: u32) -> bool {
    let limit = 1i64 << (width - 1);
    (-limit..limit).contains(&sample)
}

#[derive(Default)]
pub(super) struct Pcm {
    channels: [Vec<i64>; 8],
    count: usize,
    samples: usize,
    packed: Vec<u8>,
    digest: Md5,
}
impl Pcm {
    pub(super) fn configure(&mut self, channels: usize, samples: usize) {
        self.count = channels;
        self.samples = samples;
        for channel in &mut self.channels[..channels] {
            channel.clear();
            channel.reserve(samples);
        }
    }

    pub(super) fn channel(&mut self, i: usize) -> &mut Vec<i64> {
        &mut self.channels[i]
    }

    pub(super) fn append(&mut self, assignment: u32, width: u32) -> PResult<()> {
        let [first, second, ..] = &mut self.channels;
        for (a, b) in first.iter_mut().zip(second) {
            (*a, *b) = match assignment {
                8 => (*a, *a - *b),
                9 => (*a + *b, *b),
                10 => {
                    let doubled = (*a << 1) | (*b & 1);
                    ((doubled + *b) >> 1, (doubled - *b) >> 1)
                }
                _ => break,
            };
        }
        let bytes = width.div_ceil(8) as usize;
        self.packed.clear();
        self.packed.reserve(self.count * self.samples * bytes);
        for i in 0..self.samples {
            for channel in &self.channels[..self.count] {
                let sample = channel[i];
                require(fits(sample, width))?;
                self.packed
                    .extend_from_slice(&sample.to_le_bytes()[..bytes]);
            }
        }
        self.digest.update(&self.packed);
        Ok(())
    }

    pub(super) fn finish(self) -> [u8; 16] {
        self.digest.finalize().into()
    }
}
