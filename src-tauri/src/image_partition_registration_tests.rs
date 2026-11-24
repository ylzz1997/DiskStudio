use super::*;
use disk_core::fat_format::{FatType, Plan};
use std::io::Cursor;

fn seed(image: &mut [u8], sector: u64, offset: u64) {
    let mut volume = Cursor::new(&mut image[offset as usize..(offset + MIB) as usize]);
    Plan::new(
        FatType::Fat12,
        MIB,
        sector as u32,
        sector as u32,
        "FOUND",
        92,
        (offset / sector) as u32,
    )
    .unwrap()
    .format(&mut volume, |_, _| Ok(()))
    .unwrap();
}
fn scan(image: &[u8], sector: u64, offsets: &[u64]) -> Vec<(Range, Candidate)> {
    let range = Range {
        offset: 0,
        size: image.len() as u64,
        sector,
    };
    let cancel = AtomicBool::new(false);
    offsets
        .iter()
        .map(|&at| {
            let candidate =
                crate::lost_partitions::probe(&mut Cursor::new(image), range, at, &cancel)
                    .unwrap()
                    .unwrap();
            (range, candidate)
        })
        .collect()
}
fn plan(
    image: &[u8],
    sector: u64,
    offsets: &[u64],
    allow_rebuild: bool,
    scheme: Scheme,
    form: Form,
) -> Result<(Conversion, Vec<Evidence>), String> {
    prepare(
        &mut Cursor::new(image),
        image.len() as u64,
        sector,
        "c".repeat(64),
        allow_rebuild,
        Options {
            form,
            new_table: scheme,
        },
        scan(image, sector, offsets),
        &AtomicBool::new(false),
    )
}
fn apply(source: &[u8], plan: &Conversion, evidence: &[Evidence]) -> Vec<u8> {
    let mut bytes = Vec::new();
    plan.reader(Cursor::new(source))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    plan.verify_output(&mut Cursor::new(&bytes), || Ok(()))
        .unwrap();
    lost_partition_restore::verify(&mut Cursor::new(&bytes), evidence, &AtomicBool::new(false))
        .unwrap();
    for e in evidence {
        let from = e.extent.offset as usize;
        let to = from + e.extent.size as usize;
        assert_eq!(&bytes[from..to], &source[from..to]);
    }
    bytes
}
fn ebr_base(image: &[u8], sector: u64) -> u64 {
    let container = image[446..510]
        .chunks_exact(16)
        .find(|e| super::super::extended(e[4]))
        .unwrap();
    u64::from(u32le(&container[8..12])) * sector
}

#[test]
fn rebuilds_blank_mbr_and_gpt_from_multiple_verified_candidates() {
    let offsets: Vec<_> = [1, 3, 5, 7, 9, 11].map(|n| n * MIB).into();
    for sector in [512, 4096] {
        let mut source = vec![0; (16 * MIB) as usize];
        for &offset in &offsets {
            seed(&mut source, sector, offset);
        }
        let selected = scan(&source, sector, &offsets);
        for scheme in [Scheme::Mbr, Scheme::Gpt] {
            let (plan, evidence) = prepare(
                &mut Cursor::new(&source),
                source.len() as u64,
                sector,
                "c".repeat(64),
                true,
                Options {
                    form: Form::Auto,
                    new_table: scheme,
                },
                selected.clone(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(plan.preview.registration.as_ref().unwrap().rebuilt_table);
            assert_eq!(plan.preview.to, scheme);
            assert_eq!(plan.preview.partitions.len(), 6);
            let logical = plan.preview.partitions.iter().filter(|p| p.logical).count();
            assert_eq!(logical, if scheme == Scheme::Mbr { 3 } else { 0 });
            let bytes = apply(&source, &plan, &evidence);
            assert_eq!(&bytes[510..512], &[0x55, 0xaa]);
            assert_eq!(bytes[450], if scheme == Scheme::Mbr { 0x01 } else { 0xee });
        }
    }
}

#[test]
fn preserves_current_primary_entries_existing_ebr_data_and_non_table_bytes() {
    let sector = 4096;
    let mut source = table_archive::tests::mbr_4kn();
    for offset in [4 * MIB, 7 * MIB] {
        seed(&mut source, sector, offset);
    }
    let (task, evidence) = plan(
        &source,
        sector,
        &[4 * MIB, 7 * MIB],
        false,
        Scheme::Mbr,
        Form::Auto,
    )
    .unwrap();
    assert!(task.preview.partitions[0].logical);
    assert!(!task.preview.partitions[1].logical);
    let output = apply(&source, &task, &evidence);
    assert_eq!(&output[446..478], &source[446..478]);
    assert_eq!(&output[512..4096], &source[512..4096]);
    for at in [MIB, 2 * MIB, 3 * MIB] {
        let at = at as usize;
        assert_eq!(&output[at + 446..at + 462], &source[at + 446..at + 462]);
        assert_eq!(&output[at + 512..at + 4096], &source[at + 512..at + 4096]);
    }
}

#[test]
fn can_extend_the_ebr_chain_backwards_and_repair_an_isolated_signature_loss() {
    let sector = 512;
    let mut blank = vec![0; (12 * MIB) as usize];
    for offset in [3 * MIB, 6 * MIB] {
        seed(&mut blank, sector, offset);
    }
    let (first, evidence) =
        plan(&blank, sector, &[6 * MIB], true, Scheme::Mbr, Form::Logical).unwrap();
    let mut first_copy = apply(&blank, &first, &evidence);
    let old_root = ebr_base(&first_copy, sector);
    first_copy[510] = 0;
    let (task, evidence) = plan(
        &first_copy,
        sector,
        &[3 * MIB],
        true,
        Scheme::Mbr,
        Form::Logical,
    )
    .unwrap();
    assert!(task.preview.registration.as_ref().unwrap().rebuilt_table);
    let output = apply(&first_copy, &task, &evidence);
    let new_root = ebr_base(&output, sector);
    assert!(new_root < old_root);
    let at = old_root as usize;
    assert_eq!(&output[at + 446..at + 462], &first_copy[at + 446..at + 462]);
    assert_eq!(&output[510..512], &[0x55, 0xaa]);
}

#[test]
fn rejects_occupied_ranges_nonzero_metadata_and_changed_candidate_evidence() {
    let mut source = table_archive::tests::mbr_4kn();
    let sector = 4096;
    seed(&mut source, sector, 6 * MIB);
    assert!(plan(
        &source,
        sector,
        &[6 * MIB],
        false,
        Scheme::Mbr,
        Form::Primary
    )
    .is_err());
    let mut source = vec![0; (8 * MIB) as usize];
    seed(&mut source, sector, 3 * MIB);
    source[(2 * MIB) as usize..(3 * MIB) as usize].fill(0x5f);
    assert!(plan(
        &source,
        sector,
        &[3 * MIB],
        true,
        Scheme::Mbr,
        Form::Logical
    )
    .err()
    .unwrap()
    .contains("全零 EBR"));
    source[4096] = 0x7f;
    assert!(
        plan(&source, sector, &[3 * MIB], true, Scheme::Gpt, Form::Auto)
            .err()
            .unwrap()
            .contains("保留扇区")
    );
    source[4096] = 0;
    let (task, evidence) =
        plan(&source, sector, &[3 * MIB], true, Scheme::Gpt, Form::Auto).unwrap();
    source[(3 * MIB) as usize + 510] = 0;
    assert!(lost_partition_restore::verify(
        &mut Cursor::new(&source),
        &evidence,
        &AtomicBool::new(false)
    )
    .is_err());
    assert!(task
        .verify_source(&mut Cursor::new(&source), || Ok(()))
        .is_ok());
    seed(&mut source, sector, 3 * MIB);
    source[(8 * MIB - sector) as usize..(8 * MIB - sector + 8) as usize]
        .copy_from_slice(b"EFI PART");
    assert!(
        plan(&source, sector, &[3 * MIB], true, Scheme::Gpt, Form::Auto)
            .err()
            .unwrap()
            .contains("GPT 证据")
    );
}
