use super::*;
use std::io::Cursor;
fn identity() -> Identity {
    Identity {
        created: [0x13; 16],
        modified: [0x29; 16],
        vhd_timestamp: 12345,
        file_name: "synthetic.vmdk".into(),
    }
}
fn options(format: Format) -> Options {
    Options {
        format,
        size: 4 * CHUNK as u64 + 512,
        sector: 512,
    }
}
fn feed(format: Format) -> (Encoder<Cursor<Vec<u8>>>, Vec<u8>) {
    let o = options(format);
    let mut source = vec![0; o.size as usize];
    for (i, b) in source[..CHUNK].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    source[3 * CHUNK..].fill(0x67);
    let mut e = Encoder::new(Cursor::new(vec![]), o, identity()).unwrap();
    for (n, b) in source.chunks(CHUNK).enumerate() {
        e.push((n * CHUNK) as u64, b).unwrap();
    }
    for (n, b) in source.chunks(CHUNK).enumerate() {
        e.verify_source((n * CHUNK) as u64, b).unwrap();
    }
    (e, source)
}
fn u32at(b: &[u8], at: usize, be: bool) -> u32 {
    let a = b[at..at + 4].try_into().unwrap();
    if be {
        u32::from_be_bytes(a)
    } else {
        u32::from_le_bytes(a)
    }
}
// Independent test decoder uses only bytes read from the encoded file, never
// the encoder's map or offsets. qemu-img integration checks are separate.
fn decode(b: &[u8], format: Format, size: usize) -> Vec<u8> {
    if matches!(format, Format::Raw | Format::VhdFixed) {
        return b[..size].to_vec();
    }
    let mut out = vec![0; size];
    if matches!(format, Format::VhdxDynamic | Format::VhdxFixed) {
        // Locate regions by GUID, metadata by item ID, and data using the
        // interleaved BAT. No encoder state or helper supplies any offset.
        assert_eq!(&b[..8], b"vhdxfile");
        let regions = 192 * 1024;
        let mut bat_at = 0;
        let mut meta_at = 0;
        for i in 0..u32at(b, regions + 8, false) as usize {
            let p = regions + 16 + i * 32;
            let at = u64::from_le_bytes(b[p + 16..p + 24].try_into().unwrap()) as usize;
            match &b[p..p + 4] {
                [0x66, 0x77, 0xc2, 0x2d] => bat_at = at,
                [0x06, 0xa2, 0x7c, 0x8b] => meta_at = at,
                _ => panic!("unexpected region"),
            }
        }
        let mut block = 0;
        let mut sector = 0;
        for i in 0..u16::from_le_bytes(b[meta_at + 10..meta_at + 12].try_into().unwrap()) as usize {
            let p = meta_at + 32 + i * 32;
            let at = meta_at + u32at(b, p + 16, false) as usize;
            match &b[p..p + 4] {
                [0x37, 0x67, 0xa1, 0xca] => block = u32at(b, at, false) as usize,
                [0x1d, 0xbf, 0x41, 0x81] => sector = u32at(b, at, false) as usize,
                _ => {}
            }
        }
        let ratio = (1 << 23) * sector / block;
        for (i, piece) in out.chunks_mut(block).enumerate() {
            let p = bat_at + (i + i / ratio) * 8;
            let entry = u64::from_le_bytes(b[p..p + 8].try_into().unwrap());
            if entry & 7 == 2 {
                assert_eq!(entry, 2);
                continue;
            }
            assert_eq!(entry & 0xfffff, 6);
            let at = (entry & !0xfffff) as usize;
            piece.copy_from_slice(&b[at..at + piece.len()]);
        }
        return out;
    }
    if format == Format::VmdkSparse {
        assert_eq!(&b[..4], b"KDMV");
        let block = u64::from_le_bytes(b[20..28].try_into().unwrap()) as usize * 512;
        let gd = u64::from_le_bytes(b[56..64].try_into().unwrap()) as usize * 512;
        let rgd = u64::from_le_bytes(b[48..56].try_into().unwrap()) as usize * 512;
        for (i, piece) in out.chunks_mut(block).enumerate() {
            let gt = u32at(b, gd + i / 512 * 4, false) as usize * 512;
            let rgt = u32at(b, rgd + i / 512 * 4, false) as usize * 512;
            assert_eq!(&b[gt..gt + 2048], &b[rgt..rgt + 2048]);
            let at = u32at(b, gt + i % 512 * 4, false) as usize * 512;
            if at != 0 {
                piece.copy_from_slice(&b[at..at + piece.len()]);
            }
        }
        return out;
    }
    if format == Format::Parallels {
        assert_eq!(&b[..16], b"WithouFreSpacExt");
        let block = u32at(b, 28, false) as usize * 512;
        for (i, piece) in out.chunks_mut(block).enumerate() {
            let at = u32at(b, 64 + i * 4, false) as usize * block;
            if at != 0 {
                piece.copy_from_slice(&b[at..at + piece.len()]);
            }
        }
        return out;
    }
    let (map_at, data, block) = if matches!(format, Format::VdiDynamic | Format::VdiFixed) {
        (
            u32at(b, 340, false) as usize,
            u32at(b, 344, false) as usize,
            u32at(b, 376, false) as usize,
        )
    } else {
        (
            u64::from_be_bytes(b[528..536].try_into().unwrap()) as usize,
            0,
            u32at(b, 544, true) as usize,
        )
    };
    for (n, piece) in out.chunks_mut(block).enumerate() {
        let slot = u32at(b, map_at + n * 4, format == Format::VhdDynamic);
        if slot >= 0xfffffffe {
            continue;
        }
        let at = if format == Format::VhdDynamic {
            slot as usize * 512 + 512
        } else {
            data + slot as usize * block
        };
        piece.copy_from_slice(&b[at..at + piece.len()]);
    }
    out
}
#[test]
fn four_formats_preserve_holes_nonzero_blocks_tail_and_logical_digest() {
    for f in [
        Format::Raw,
        Format::VhdFixed,
        Format::VhdDynamic,
        Format::VdiDynamic,
    ] {
        let (e, source) = feed(f);
        let (io, v) = e.finish(|_| Ok(()), |_| Ok(())).unwrap();
        let b = io.into_inner();
        assert_eq!(v.file_bytes, b.len() as u64);
        assert_eq!(v.sha256, format!("{:x}", Sha256::digest(&source)));
        assert_eq!(decode(&b, f, source.len()), source);
        if f.sparse() {
            assert_eq!(v.allocated_blocks, 3);
            assert!(b.len() < source.len());
        }
        if matches!(f, Format::VhdFixed | Format::VhdDynamic) {
            let foot = &b[b.len() - 512..];
            assert_eq!(&foot[..8], b"conectix");
            let sum = foot
                .iter()
                .enumerate()
                .filter(|(i, _)| !(64..68).contains(i))
                .fold(0u32, |s, (_, v)| s + u32::from(*v));
            assert_eq!(u32at(foot, 64, true), !sum);
            if f == Format::VhdDynamic {
                assert_eq!(&b[..512], foot);
            }
        }
    }
}
#[test]
fn sparse_all_zero_image_has_no_allocated_payload() {
    for f in [
        Format::VhdDynamic,
        Format::VdiDynamic,
        Format::VhdxDynamic,
        Format::VmdkSparse,
        Format::Parallels,
    ] {
        let mut e = Encoder::new(Cursor::new(vec![]), options(f), identity()).unwrap();
        for n in 0..4 {
            e.push(n * CHUNK as u64, &vec![0; CHUNK]).unwrap();
        }
        e.push(4 * CHUNK as u64, &[0; 512]).unwrap();
        for n in 0..4 {
            e.verify_source(n * CHUNK as u64, &vec![0; CHUNK]).unwrap();
        }
        e.verify_source(4 * CHUNK as u64, &[0; 512]).unwrap();
        let (io, v) = e.finish(|_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(v.allocated_blocks, 0);
        assert!(io.get_ref().len() <= 4 * CHUNK);
        assert!(decode(io.get_ref(), f, options(f).size as usize)
            .iter()
            .all(|b| *b == 0));
    }
}
#[test]
fn fixed_vdi_allocates_zero_blocks_has_fixed_type_and_checks_tail() {
    let (e, source) = feed(Format::VdiFixed);
    let (io, v) = e.finish(|_| Ok(()), |_| Ok(())).unwrap();
    assert_eq!(u32at(io.get_ref(), 76, false), 2);
    assert_eq!(v.allocated_blocks, 5);
    assert!(v.file_bytes > source.len() as u64);
    assert_eq!(decode(io.get_ref(), Format::VdiFixed, source.len()), source);
    let (e, _) = feed(Format::VdiFixed);
    assert!(e
        .finish(
            |io| {
                let last = io.get_ref().len() - 1;
                io.get_mut()[last] = 1;
                Ok(())
            },
            |_| Ok(())
        )
        .is_err());
}
#[test]
fn rejects_invalid_geometry_existing_output_out_of_order_and_incomplete_input() {
    for f in [Format::VhdFixed, Format::VhdDynamic, Format::VdiDynamic] {
        let mut o = options(f);
        o.sector = 4096;
        assert!(o.validate().is_err());
    }
    let mut o = options(Format::Raw);
    o.sector = 4096;
    o.size = 8 * CHUNK as u64;
    assert!(o.validate().is_ok());
    for size in [
        0,
        4 * CHUNK as u64 - 512,
        4 * CHUNK as u64 + 1,
        (1u64 << 43) + 512,
    ] {
        o.size = size;
        assert!(o.validate().is_err());
    }
    assert!(Encoder::new(Cursor::new(vec![0; 512]), options(Format::Raw), identity()).is_err());
    let mut e = Encoder::new(Cursor::new(vec![]), options(Format::Raw), identity()).unwrap();
    assert!(e.push(512, &vec![0; CHUNK]).is_err());
    assert!(e.push(0, &vec![0; CHUNK]).is_err());
    assert!(e.finish(|_| Ok(()), |_| Ok(())).is_err());
}
#[test]
fn refuses_flush_failure_corrupted_readback_and_cancellation() {
    let (e, _) = feed(Format::Raw);
    assert!(e
        .finish(|_| Err("flush failed".into()), |_| Ok(()))
        .is_err());
    let (e, _) = feed(Format::VdiDynamic);
    assert!(e
        .finish(
            |io| {
                io.get_mut()[1024] ^= 1;
                Ok(())
            },
            |_| Ok(())
        )
        .is_err());
    let (e, _) = feed(Format::VhdDynamic);
    assert!(e
        .finish(
            |io| {
                let at = 2560 + 512;
                io.get_mut()[at] ^= 1;
                Ok(())
            },
            |_| Ok(())
        )
        .is_err());
    let (e, _) = feed(Format::VhdFixed);
    assert!(e
        .finish(
            |_| Ok(()),
            |done| if done >= CHUNK as u64 {
                Err("canceled".into())
            } else {
                Ok(())
            }
        )
        .is_err());
}

#[test]
fn second_source_pass_is_mandatory_and_detects_mid_conversion_changes() {
    let o = options(Format::VhdDynamic);
    let source = vec![7; o.size as usize];
    for changed in [false, true] {
        let mut e = Encoder::new(Cursor::new(vec![]), o.clone(), identity()).unwrap();
        for (n, b) in source.chunks(CHUNK).enumerate() {
            e.push((n * CHUNK) as u64, b).unwrap();
        }
        if changed {
            for (n, b) in source.chunks(CHUNK).enumerate() {
                let mut b = b.to_vec();
                if n == 1 {
                    b[36] ^= 1;
                }
                e.verify_source((n * CHUNK) as u64, &b).unwrap();
            }
        }
        assert!(e.finish(|_| Ok(()), |_| Ok(())).is_err());
    }
}

#[test]
fn additional_formats_preserve_contents_and_exact_unrounded_capacity() {
    for f in [
        Format::VhdxDynamic,
        Format::VhdxFixed,
        Format::VmdkSparse,
        Format::Parallels,
    ] {
        let (e, source) = feed(f);
        let (io, v) = e.finish(|_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(v.logical_bytes, source.len() as u64);
        assert_eq!(v.file_bytes, io.get_ref().len() as u64);
        assert_eq!(v.sha256, format!("{:x}", Sha256::digest(&source)));
        assert_eq!(decode(io.get_ref(), f, source.len()), source);
    }
}

#[test]
fn vhdx_4kn_late_allocation_zero_blocks_and_complete_tail_padding() {
    // Nonzero data occurs after zero slices in the same 8 MiB block, then a
    // full sparse block and a partial final block. Check fixed and dynamic.
    let mut source = vec![0; 16 * CHUNK + 4096];
    source[3 * CHUNK + 5] = 0x65;
    source[16 * CHUNK + 4095] = 0x42;
    for f in [Format::VhdxDynamic, Format::VhdxFixed] {
        let mut e = Encoder::new(
            Cursor::new(vec![]),
            Options {
                format: f,
                size: source.len() as u64,
                sector: 4096,
            },
            identity(),
        )
        .unwrap();
        for (i, b) in source.chunks(CHUNK).enumerate() {
            e.push((i * CHUNK) as u64, b).unwrap();
        }
        for (i, b) in source.chunks(CHUNK).enumerate() {
            e.verify_source((i * CHUNK) as u64, b).unwrap();
        }
        let (io, v) = e.finish(|_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(
            v.allocated_blocks,
            if f == Format::VhdxFixed { 3 } else { 2 }
        );
        assert_eq!(decode(io.get_ref(), f, source.len()), source);
        assert!(io.get_ref()[io.get_ref().len() - (8 * CHUNK - 4096)..]
            .iter()
            .all(|v| *v == 0));
    }
}

#[test]
fn new_containers_reject_metadata_payload_and_tail_corruption_and_cancel() {
    for f in [
        Format::VhdxDynamic,
        Format::VhdxFixed,
        Format::VmdkSparse,
        Format::Parallels,
    ] {
        for corrupt in [0, 1, 2] {
            let (e, _) = feed(f);
            let data = e.data_start as usize;
            assert!(e
                .finish(
                    |io| {
                        let at = match corrupt {
                            0 => 0,
                            1 => data + 32,
                            _ => io.get_ref().len() - 1,
                        };
                        io.get_mut()[at] ^= 1;
                        Ok(())
                    },
                    |_| Ok(())
                )
                .is_err());
        }
        let (e, _) = feed(f);
        assert!(e
            .finish(
                |_| Ok(()),
                |done| if done >= CHUNK as u64 {
                    Err("stopped".into())
                } else {
                    Ok(())
                }
            )
            .is_err());
    }
}

#[test]
fn large_vhdx_bat_interleaves_bitmap_slots_and_keeps_64bit_offsets() {
    for sector in [512, 4096] {
        let o = Options {
            format: Format::VhdxDynamic,
            size: 1u64 << 43,
            sector,
        };
        let mut e = Encoder::new(Cursor::new(vec![]), o, identity()).unwrap();
        let ratio = ((1u64 << 23) * sector / (8 * CHUNK as u64)) as usize;
        e.map[ratio - 1] = 1024;
        e.map[ratio] = 1025;
        let meta = e.metadata();
        let bat = &meta.last().unwrap().1;
        assert!(bat.len() <= 16 * CHUNK);
        let entry = |i: usize| u64::from_le_bytes(bat[i * 8..i * 8 + 8].try_into().unwrap());
        assert_eq!(entry(0), 2);
        assert_eq!(entry(ratio), 0); // reserved bitmap, no parent
        assert_eq!(
            entry(ratio - 1),
            (e.data_start + 1024 * 8 * CHUNK as u64) | 6
        );
        assert_eq!(
            entry(ratio + 1),
            (e.data_start + 1025 * 8 * CHUNK as u64) | 6
        );
    }
}

#[test]
fn vmdk_descriptor_uses_native_basename_and_rejects_injection_or_address_overflow() {
    for name in [
        "../other.vmdk",
        "disk\"\nparentCID=12345678.vmdk",
        "C:\\disk.vmdk",
        "disk\0.vmdk",
        "disk.img",
    ] {
        let mut id = identity();
        id.file_name = name.into();
        assert!(Encoder::new(Cursor::new(vec![]), options(Format::VmdkSparse), id).is_err());
    }
    let mut id = identity();
    id.file_name = "测试 合成.vmdk".into();
    let e = Encoder::new(Cursor::new(vec![]), options(Format::VmdkSparse), id).unwrap();
    let meta = e.metadata();
    let d = String::from_utf8_lossy(&meta[0].1[512..21 * 512]);
    assert!(d.contains("SPARSE \"测试 合成.vmdk\""));
    assert!(d.contains("parentCID=ffffffff"));
    let mut o = options(Format::VmdkSparse);
    o.size = 2040 * 1024u64.pow(3);
    assert!(o.validate().is_ok());
    let l = containers::vmdk_layout(o.size.div_ceil(CHUNK as u64));
    assert!((o.size + l.data) / 512 < u32::MAX as u64);
    o.size += 512;
    assert!(o.validate().is_err());
}
