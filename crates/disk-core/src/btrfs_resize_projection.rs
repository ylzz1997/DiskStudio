//! Read-only copy of the resized layout. Relocated stripes alias the source,
//! while the resize transaction's metadata writes are retained in memory.
use super::*;
use std::ops::Bound::{Excluded, Unbounded};

const PAGE: usize = 4096;
const MAX_METADATA_PAGES: usize = 128 * 1024 * 1024 / PAGE;

pub struct ProjectedResize<R> {
    view: Overlay<R>,
    size: u64,
    identity: [u8; 32],
}

impl<R> ProjectedResize<R> {
    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }
}

pub fn project_resize<R: Read + Seek>(
    mut source: R,
    plan: &ResizePlan,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<ProjectedResize<R>, String> {
    if source.seek(SeekFrom::End(0)).map_err(err)? != plan.before {
        return Err("Btrfs 源范围与缩放计划不同。".into());
    }
    let mut view = Overlay {
        source,
        source_len: plan.before,
        max_len: plan.before.max(plan.after),
        position: 0,
        aliases: plan
            .relocation
            .projected_ranges()
            .map(|(start, from, len)| (start, (from, start + len)))
            .collect(),
        pages: BTreeMap::new(),
    };
    let current = resize_source(&mut view, plan)?;
    let identity = resize_transaction(&mut view, plan, current, checkpoint)?;
    view.position = 0;
    Ok(ProjectedResize {
        view,
        size: plan.after,
        identity,
    })
}

impl<R: Read + Seek> Read for ProjectedResize<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let len = output
            .len()
            .min((self.size - self.view.position).min(usize::MAX as u64) as usize);
        self.view.read(&mut output[..len])
    }
}

impl<R> Seek for ProjectedResize<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        seek(&mut self.view.position, self.size, from)
    }
}

struct Overlay<R> {
    source: R,
    source_len: u64,
    max_len: u64,
    position: u64,
    /// Target start -> (source start, target end).
    aliases: BTreeMap<u64, (u64, u64)>,
    pages: BTreeMap<u64, Box<[u8; PAGE]>>,
}

impl<R: Read + Seek> Read for Overlay<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let at = self.position;
        let limit = at.saturating_add(output.len() as u64).min(self.max_len);
        if at == limit {
            return Ok(0);
        }
        let page = at / PAGE as u64 * PAGE as u64;
        if let Some(data) = self.pages.get(&page) {
            let start = (at - page) as usize;
            let len = (limit - at).min((PAGE - start) as u64) as usize;
            output[..len].copy_from_slice(&data[start..start + len]);
            self.position += len as u64;
            return Ok(len);
        }

        let mut stop = limit;
        if let Some((&next, _)) = self.pages.range((Excluded(at), Unbounded)).next() {
            stop = stop.min(next);
        }
        let (source, end) = match self.aliases.range(..=at).next_back() {
            Some((&start, &(from, end))) if at < end => (from + (at - start), end),
            _ => {
                let end = self
                    .aliases
                    .range((Excluded(at), Unbounded))
                    .next()
                    .map_or(self.max_len, |(&next, _)| next);
                (at, end)
            }
        };
        stop = stop.min(end);
        let len = if source < self.source_len {
            let len = (stop - at).min(self.source_len - source) as usize;
            self.source.seek(SeekFrom::Start(source))?;
            self.source.read_exact(&mut output[..len])?;
            len
        } else {
            let len = (stop - at) as usize;
            output[..len].fill(0);
            len
        };
        self.position += len as u64;
        Ok(len)
    }
}

impl<R> Seek for Overlay<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        seek(&mut self.position, self.max_len, from)
    }
}

impl<R> Write for Overlay<R> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.position % PAGE as u64 != 0
            || data.len() % PAGE != 0
            || data.len() as u64 > self.max_len - self.position
        {
            return Err(io::Error::other(
                "Unaligned or out-of-range Btrfs projection write",
            ));
        }
        for page in data.chunks_exact(PAGE) {
            if self.pages.len() >= MAX_METADATA_PAGES && !self.pages.contains_key(&self.position) {
                return Err(io::Error::other(
                    "Btrfs resize exceeds the in-memory transaction limit",
                ));
            }
            self.pages
                .insert(self.position, Box::new(page.try_into().unwrap()));
            self.position += PAGE as u64;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // The projection is temporary. The destination copy performs the actual
        // durable writes and complete readback.
        Ok(())
    }
}

fn seek(position: &mut u64, size: u64, from: SeekFrom) -> io::Result<u64> {
    let next = match from {
        SeekFrom::Start(n) => i128::from(n),
        SeekFrom::Current(delta) => i128::from(*position) + i128::from(delta),
        SeekFrom::End(delta) => i128::from(size) + i128::from(delta),
    };
    if next < 0 || next > i128::from(size) {
        return Err(io::Error::other(
            "Btrfs projection seek outside the selected extent",
        ));
    }
    *position = next as u64;
    Ok(*position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn relocation_reads_alias_source_with_transaction_writes_taking_precedence() {
        let original = [vec![1; PAGE], vec![2; PAGE], vec![3; PAGE], vec![4; PAGE]].concat();
        let page = PAGE as u64;
        let mut overlay = Overlay {
            source: Cursor::new(&original),
            source_len: 4 * page,
            max_len: 5 * page,
            position: 0,
            aliases: BTreeMap::from([(page, (2 * page, 3 * page))]),
            pages: BTreeMap::new(),
        };
        overlay.seek(SeekFrom::Start(2 * page)).unwrap();
        overlay.write_all(&[5; PAGE]).unwrap();
        let mut resized = ProjectedResize {
            view: overlay,
            size: 5 * page,
            identity: [0; 32],
        };
        resized.seek(SeekFrom::Start(page - 2)).unwrap();
        let mut crossing = [0; 4];
        resized.read_exact(&mut crossing).unwrap();
        assert_eq!(crossing, [1, 1, 3, 3]);
        resized.seek(SeekFrom::Start(2 * page - 2)).unwrap();
        resized.read_exact(&mut crossing).unwrap();
        assert_eq!(crossing, [3, 3, 5, 5]);
        resized.seek(SeekFrom::End(-4)).unwrap();
        resized.read_exact(&mut crossing).unwrap();
        assert_eq!(crossing, [0; 4]);
        assert_eq!(resized.read(&mut crossing).unwrap(), 0);
        assert!(resized.seek(SeekFrom::End(1)).is_err());
        assert_eq!(original[2 * PAGE], 3);
    }
}
