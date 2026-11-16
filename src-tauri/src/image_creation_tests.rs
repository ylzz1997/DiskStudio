use super::*;
use crate::image_convert::{Format, Identity};
use disk_core::table_check::{inspect_image, CheckState};
use std::io::Cursor;

fn options(table: Table, sector: u64) -> Options {
    Options {
        output: OutputOptions {
            format: Format::Raw,
            size: 4 * CHUNK as u64 + sector,
            sector,
        },
        table,
    }
}
fn id() -> uuid::Uuid {
    uuid::Uuid::parse_str("12345678-1234-4abc-8def-112233445566").unwrap()
}
fn identity() -> Identity {
    Identity {
        created: [1; 16],
        modified: [2; 16],
        vhd_timestamp: 12345,
        file_name: "disk.vmdk".into(),
    }
}
#[test]
fn gpt_both_sector_sizes_have_complete_matching_tables_and_exact_uuid() {
    for sector in [512, 4096] {
        let o = options(Table::Gpt, sector);
        let generator = Generator::new(o.clone(), id()).unwrap();
        let mut head = vec![0; CHUNK];
        let mut tail = vec![0; CHUNK];
        generator.fill(0, &mut head).unwrap();
        generator
            .fill(o.output.size - CHUNK as u64, &mut tail)
            .unwrap();
        let check = inspect_image(&head, &tail, o.output.size, "created.img").unwrap();
        assert_eq!(check.report.scheme, "GPT");
        assert_eq!(check.report.sector_size, Some(sector));
        assert_eq!(check.report.state, CheckState::Passed, "{:?}", check.report);
        assert_eq!(
            &head[sector as usize + 56..sector as usize + 72],
            &id().to_bytes_le()
        );
        assert_eq!(generator.summary().disk_id, Some(id().to_string()));
        assert!(head[2 * sector as usize..2 * sector as usize + 16384]
            .iter()
            .all(|b| *b == 0));
        // A corrupt backup header must be detected by the independent table inspector.
        let last = tail.len() - sector as usize;
        tail[last + 32] ^= 1;
        assert_ne!(
            inspect_image(&head, &tail, o.output.size, "bad.img")
                .unwrap()
                .report
                .state,
            CheckState::Passed
        );
    }
}
#[test]
fn empty_mbr_has_signature_and_no_boot_code_or_partition_entries() {
    let generator = Generator::new(options(Table::Mbr, 512), id()).unwrap();
    let mut head = vec![0; CHUNK];
    generator.fill(0, &mut head).unwrap();
    assert_eq!(&head[510..512], &[0x55, 0xaa]);
    assert!(head[..440].iter().all(|b| *b == 0));
    assert!(head[444..510].iter().all(|b| *b == 0));
    let report = inspect_image(&head, &vec![0; CHUNK], 4 * CHUNK as u64 + 512, "empty.img")
        .unwrap()
        .report;
    assert_eq!(report.scheme, "MBR");
    assert_eq!(report.state, CheckState::Warning, "{report:?}");
    assert_eq!(
        report
            .items
            .iter()
            .find(|i| i.code == "mbr-layout")
            .unwrap()
            .state,
        CheckState::Passed
    );
    assert_eq!(
        report
            .items
            .iter()
            .filter(|i| i.state != CheckState::Passed)
            .map(|i| i.code.as_str())
            .collect::<Vec<_>>(),
        vec!["mbr-sector"]
    );
    let blank = Generator::new(options(Table::None, 512), id()).unwrap();
    blank.fill(0, &mut head).unwrap();
    assert!(head.iter().all(|b| *b == 0));
    assert!(blank.summary().disk_id.is_none());
}
#[test]
fn geometry_boundaries_and_large_gpt_are_checked_without_allocating_disk_size() {
    let mut o = options(Table::Mbr, 512);
    o.output.size = 1u64 << 41;
    assert!(o.validate().is_ok());
    o.output.size += 512;
    assert!(o.validate().is_err());
    assert!(options(Table::Mbr, 4096).validate().is_err());
    o.table = Table::Gpt;
    o.output.size = 1u64 << 43;
    let generator = Generator::new(o.clone(), id()).unwrap();
    let mut h = vec![0; CHUNK];
    let mut t = vec![0; CHUNK];
    generator.fill(0, &mut h).unwrap();
    generator
        .fill(o.output.size - CHUNK as u64, &mut t)
        .unwrap();
    assert_eq!(&h[458..462], &u32::MAX.to_le_bytes());
    assert_eq!(
        inspect_image(&h, &t, o.output.size, "large.img")
            .unwrap()
            .report
            .state,
        CheckState::Passed
    );
    assert!(generator.fill(u64::MAX, &mut h).is_err());
    assert!(generator.fill(0, &mut []).is_err());
    assert!(Generator::new(o, uuid::Uuid::nil()).is_err());
}
#[test]
fn generated_contents_across_all_containers_are_verified_and_cancel_can_stop_each_pass() {
    for format in [
        Format::Raw,
        Format::VhdFixed,
        Format::VhdDynamic,
        Format::VdiDynamic,
        Format::VdiFixed,
        Format::VhdxDynamic,
        Format::VhdxFixed,
        Format::VmdkSparse,
        Format::Parallels,
    ] {
        let mut o = options(Table::Gpt, 512);
        o.output.format = format;
        let generator = Generator::new(o.clone(), id()).unwrap();
        let mut encoder = Encoder::new(Cursor::new(vec![]), o.output.clone(), identity()).unwrap();
        let mut ticks = vec![];
        generator
            .feed(&mut encoder, |s, n| {
                ticks.push((s, n));
                Ok(())
            })
            .unwrap();
        assert_eq!(ticks.last(), Some(&(Stage::Checking, o.output.size)));
        let (_, v) = encoder.finish(|_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(v.logical_bytes, o.output.size);
        for stop in [Stage::Generating, Stage::Checking] {
            let mut e = Encoder::new(Cursor::new(vec![]), o.output.clone(), identity()).unwrap();
            assert!(generator
                .feed(&mut e, |s, n| if s == stop && n == CHUNK as u64 {
                    Err("stopped".into())
                } else {
                    Ok(())
                })
                .is_err());
            assert!(e.finish(|_| Ok(()), |_| Ok(())).is_err());
        }
    }
}
