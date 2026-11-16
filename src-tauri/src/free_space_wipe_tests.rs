use super::*;
use crate::used_space::{Audit, Builder, VolumeReport};
use std::io::{self, Cursor, SeekFrom};
use std::sync::atomic::Ordering;

fn fixture() -> (Cursor<Vec<u8>>, used_space::Plan) {
    let mut io = Cursor::new(vec![0x79; 32768]);
    let cancel = AtomicBool::new(false);
    let mut b = Builder::new(32768, 512).unwrap();
    {
        let mut a = Audit::new(&mut io, 0, 32768, &mut b.proof, &cancel).unwrap();
        a.read_exact(&mut [0; 512]).unwrap();
    }
    b.free(4096, 4096).unwrap();
    b.free(16384, 8192).unwrap();
    b.volumes.push(VolumeReport {
        offset: 0,
        size: 32768,
        filesystem: "NTFS".into(),
        skipped_bytes: 12288,
        note: String::new(),
    });
    (io, b.finish().unwrap())
}
fn options() -> engine::WipeOptions {
    engine::WipeOptions {
        pattern: engine::Pattern::Bytes { hex: "2A".into() },
        passes: 2,
    }
}

#[test]
fn free_space_wipe_preserves_every_other_byte_and_partition_neighbors() {
    let (original, plan) = fixture();
    let mut device =
        Cursor::new([vec![0x31; 4096], original.into_inner(), vec![0x32; 4096]].concat());
    let mut view = used_space::View::new(&mut device, 4096, 32768).unwrap();
    let mut stats = engine::WipeProgress::default();
    let result = execute(
        &mut view,
        &plan,
        &options(),
        &AtomicBool::new(false),
        &mut stats,
        |_| panic!("not random"),
        |_| Ok(()),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.free_bytes, 12288);
    assert_eq!(stats.written_bytes, 24576);
    assert_eq!(stats.readback_bytes, 24576);
    assert_eq!(stats.verified_passes, 2);
    assert!(result.preserved_sha256.is_some());
    let bytes = device.into_inner();
    assert!(bytes[..4096].iter().all(|&b| b == 0x31));
    assert!(bytes[36864..].iter().all(|&b| b == 0x32));
    for (i, &b) in bytes[4096..36864].iter().enumerate() {
        let free = plan
            .free_ranges()
            .iter()
            .any(|s| s.start <= i as u64 && (i as u64) < s.end);
        assert_eq!(b, if free { 0x2a } else { 0x79 });
    }
}

#[test]
fn stale_metadata_unknown_filesystems_and_cancellation_never_start_writing() {
    for case in 0..5 {
        let (mut io, mut plan) = fixture();
        let cancel = AtomicBool::new(case == 4);
        match case {
            0 => io.get_mut()[0] ^= 1,
            1 => plan.volumes[0].filesystem = "未识别".into(),
            2 => plan.volumes[0].filesystem = "Ext4".into(),
            3 => plan.volumes.push(plan.volumes[0].clone()),
            _ => {}
        }
        let before = io.get_ref().clone();
        let mut stats = engine::WipeProgress::default();
        assert!(execute(
            &mut io,
            &plan,
            &options(),
            &cancel,
            &mut stats,
            |_| Ok(()),
            |_| Ok(()),
            |_| {}
        )
        .is_err());
        assert_eq!(io.get_ref(), &before);
        assert_eq!(stats.written_bytes, 0);
    }
}

#[test]
fn corrupt_retained_data_or_free_readback_and_sync_errors_never_claim_success() {
    for case in 0..3 {
        let (mut io, plan) = fixture();
        let mut stats = engine::WipeProgress::default();
        let result = execute(
            &mut io,
            &plan,
            &engine::WipeOptions {
                passes: 1,
                ..options()
            },
            &AtomicBool::new(false),
            &mut stats,
            |_| Ok(()),
            |r| match case {
                0 => {
                    r.get_mut()[1024] ^= 1;
                    Ok(())
                }
                1 => {
                    r.get_mut()[4096] ^= 1;
                    Ok(())
                }
                _ => Err(io::Error::other("flush failed")),
            },
            |_| {},
        );
        assert!(result.is_err());
        if case == 0 {
            assert!(result.unwrap_err().contains("保留区域"));
        } else {
            assert_eq!(stats.verified_passes, 0);
        }
    }
}

struct Short {
    io: Cursor<Vec<u8>>,
    writes: usize,
}
impl Seek for Short {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.io.seek(p)
    }
}
impl Read for Short {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.io.read(b)
    }
}
impl Write for Short {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        self.io.write(&b[..17])
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn short_write_and_cancellation_preserve_partial_counts_without_retry() {
    let (io, plan) = fixture();
    let mut short = Short { io, writes: 0 };
    let mut stats = engine::WipeProgress::default();
    assert!(execute(
        &mut short,
        &plan,
        &options(),
        &AtomicBool::new(false),
        &mut stats,
        |_| Ok(()),
        |_| Ok(()),
        |_| {}
    )
    .is_err());
    assert_eq!(short.writes, 1);
    assert_eq!(stats.written_bytes, 17);
    assert_eq!(stats.verified_passes, 0);
    let (mut io, plan) = fixture();
    let cancel = AtomicBool::new(false);
    let result = execute(
        &mut io,
        &plan,
        &options(),
        &cancel,
        &mut stats,
        |_| Ok(()),
        |_| Ok(()),
        |p| {
            if let Progress::Wipe(p) = p {
                if p.written_bytes > 0 {
                    cancel.store(true, Ordering::Relaxed)
                }
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(stats.written_bytes, 4096);
    assert_eq!(stats.verified_passes, 0);
    assert!(io.get_ref()[16384..].iter().all(|&b| b == 0x79));
}
