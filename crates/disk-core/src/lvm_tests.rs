use super::*;
use std::io::Cursor;

const MB: u64 = super::super::MIB;
const PV1: &str = "aaaaaa-bbbb-cccc-dddd-eeee-ffff-gggggg";
const PV2: &str = "hhhhhh-iiii-jjjj-kkkk-llll-mmmm-nnnnnn";
const MAIN: &str = "aaaaaa-bbbb-cccc-dddd-eeee-ffff-hhhhhh";
const RESCUE: &str = "aaaaaa-bbbb-cccc-dddd-eeee-ffff-iiiiii";
const EXTENT: usize = 65536;
const UNIT: usize = 4096;

fn metadata(seq: u64) -> String {
    format!(
        r#"
# ordinary LVM2 on-disk text
lab_vg {{
  id = "aaaaaa-bbbb-cccc-dddd-eeee-ffff-rrrrrr"
  seqno = {seq}
  status = ["RESIZEABLE", "READ", "WRITE"]
  extent_size = 128
  physical_volumes {{
    pv0 {{
      id = "{PV1}"
      device = "/dev/ignored-0"
      dev_size = 4096
      pe_start = 2048
      pe_count = 16
    }}
    pv1 {{
      id = "{PV2}"
      device = "/dev/ignored-1"
      dev_size = 4096
      pe_start = 2048
      pe_count = 16
    }}
  }}
  logical_volumes {{
    first {{
      id = "{MAIN}"
      status = ["READ", "WRITE", "VISIBLE"]
      segment_count = 2
      segment1 {{
        start_extent = 0
        extent_count = 4
        type = "striped"
        stripe_count = 2
        stripe_size = 8
        stripes = ["pv0", 0, "pv1", 1]
      }}
      segment2 {{
        start_extent = 4
        extent_count = 2
        type = "striped"
        stripe_count = 1
        stripes = ["pv0", 6]
      }}
    }}
    rescue {{
      id = "{RESCUE}"
      status = ["READ", "VISIBLE"]
      segment_count = 1
      segment1 {{
        start_extent = 0
        extent_count = 2
        type = "striped"
        stripe_count = 1
        stripes = ["pv0", 11]
      }}
    }}
  }}
}}
contents = "Text Format Volume Group"
version = 1
description = "fixture \"escaped\" with # text"
"#,
    )
}

fn set32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn set64(out: &mut [u8], at: usize, value: u64) {
    out[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn physical_image(id: &str, metadata: Option<&str>, wrap: bool) -> Cursor<Vec<u8>> {
    let mut image = vec![0; (2 * MB) as usize];
    let label = &mut image[512..1024];
    label[..8].copy_from_slice(b"LABELONE");
    set64(label, 8, 1);
    set32(label, 20, 32);
    label[24..32].copy_from_slice(b"LVM2 001");
    label[32..64].copy_from_slice(uuid(id).unwrap().as_bytes());
    set64(label, 64, 2 * MB);
    set64(label, 72, MB);
    if metadata.is_some() {
        set64(label, 104, 4096);
        set64(label, 112, MB - 4096);
    }
    let checksum = crc(&label[20..]);
    set32(label, 16, checksum);
    if let Some(metadata) = metadata {
        let mut bytes = metadata.as_bytes().to_vec();
        bytes.push(0);
        let offset = if wrap { MB - 4096 - 512 } else { 512 };
        let header = &mut image[4096..4608];
        header[4..20].copy_from_slice(MDA_MAGIC);
        set32(header, 20, 1);
        set64(header, 24, 4096);
        set64(header, 32, MB - 4096);
        set64(header, 40, offset);
        set64(header, 48, bytes.len() as u64);
        set32(header, 56, crc(&bytes));
        let checksum = crc(&header[4..]);
        set32(header, 0, checksum);
        let start = 4096 + offset as usize;
        let first = bytes.len().min(MB as usize - start);
        image[start..start + first].copy_from_slice(&bytes[..first]);
        image[4608..4608 + bytes.len() - first].copy_from_slice(&bytes[first..]);
    }
    Cursor::new(image)
}

#[test]
fn mixed_volume_reads_stripes_and_segments_from_explicit_members() {
    let mut one = physical_image(PV1, Some(&metadata(11)), true);
    let mut two = physical_image(PV2, None, false);
    let mut expected = Vec::new();
    for stripe in 0..(4 * EXTENT / UNIT) {
        let (destination, base) = if stripe % 2 == 0 {
            (one.get_mut(), MB as usize)
        } else {
            (two.get_mut(), MB as usize + EXTENT)
        };
        let start = base + stripe / 2 * UNIT;
        destination[start..start + UNIT].fill((stripe + 1) as u8);
        expected.extend_from_slice(&vec![(stripe + 1) as u8; UNIT]);
    }
    let base = MB as usize + 6 * EXTENT;
    one.get_mut()[base..base + 2 * EXTENT].fill(0xe0);
    expected.extend_from_slice(&vec![0xe0; 2 * EXTENT]);
    let group = open(vec![two, one], || Ok(())).unwrap();
    assert_eq!(group.inventory().name, "lab_vg");
    assert_eq!(group.inventory().member_count, 2);
    assert_eq!(group.inventory().supplied_members, 2);
    assert_eq!(group.inventory().sequence, 11);
    assert_eq!(group.inventory().volumes[0].profile, "mixed");
    let mut volume = group.reader(&uuid(MAIN).unwrap()).unwrap();
    let mut actual = Vec::new();
    volume.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, expected);
    for start in [UNIT - 2, 4 * EXTENT - 17, expected.len() - 31] {
        volume.seek(SeekFrom::Start(start as u64)).unwrap();
        let mut bytes = [0; 31];
        volume.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, expected[start..start + bytes.len()]);
    }
    assert!(volume.seek(SeekFrom::End(1)).is_err());
}

#[test]
fn missing_members_and_conflicts_do_not_publish_invalid_mappings() {
    let group = open(
        vec![physical_image(PV1, Some(&metadata(11)), false)],
        || Ok(()),
    )
    .unwrap();
    assert!(!group.inventory().volumes[0].readable);
    assert!(group.inventory().volumes[1].readable);
    assert!(group.reader(&uuid(MAIN).unwrap()).is_err());
    let make = |id, sequence| physical_image(id, Some(&metadata(sequence)), false);
    assert!(open(vec![make(PV1, 11), make(PV2, 12)], || Ok(())).is_err());
    assert!(open(vec![make(PV1, 11), make(PV1, 11)], || Ok(())).is_err());
    let mut bad = make(PV1, 11);
    bad.get_mut()[4608] ^= 1;
    assert!(open(vec![bad], || Ok(())).is_err());
    let overlap = metadata(11).replace("\"pv0\", 11", "\"pv0\", 1");
    assert!(open(vec![physical_image(PV1, Some(&overlap), false)], || Ok(())).is_err());
    let repeated = metadata(11).replace("seqno = 11", "seqno = 11\n  seqno = 11");
    assert!(open(vec![physical_image(PV1, Some(&repeated), false)], || Ok(())).is_err());
}

fn mirror_metadata() -> String {
    let header = metadata(11);
    let header = header.split("  logical_volumes {").next().unwrap();
    let mut text = format!(
        r#"{header}
 logical_volumes {{
    mirror {{
      id = "{MAIN}"
      status = ["READ", "WRITE", "VISIBLE"]
      flags = []
      segment_count = 1
      segment1 {{
        start_extent = 0
        extent_count = 2
        type = "raid1"
        device_count = 2
        region_size = 128
        raids = ["mirror_rmeta_0", "mirror_rimage_0", "mirror_rmeta_1", "mirror_rimage_1"]
      }}
    }}
"#
    );
    let mut id = 0;
    for lane in 0..2 {
        for (role, start, count) in [("rmeta", 0, 1), ("rimage", 2, 2)] {
            id += 1;
            text.push_str(&format!(
                r#"
  mirror_{role}_{lane} {{
    id = "aaaaaa-bbbb-cccc-dddd-eeee-ffff-{id:06}"
    status = ["READ", "WRITE"]
    flags = []
    segment_count = 1
    segment1 {{
      start_extent = 0
      extent_count = {count}
      type = "striped"
      stripe_count = 1
      stripes = ["pv{lane}", {start}]
    }}
  }}"#
            ));
        }
    }
    text.push_str("\n }\n}\ncontents = \"Text Format Volume Group\"\nversion = 1\n");
    text
}

fn mirror_fixture(text: &str, epochs: [u64; 2]) -> Vec<Cursor<Vec<u8>>> {
    [PV1, PV2]
        .into_iter()
        .enumerate()
        .map(|(lane, id)| {
            let mut image = physical_image(id, Some(text), false);
            let media = image.get_mut();
            let sb = &mut media[MB as usize..][..512];
            sb[..4].copy_from_slice(b"DmRd");
            set32(sb, 4, 1);
            set32(sb, 8, 2);
            set32(sb, 12, lane as u32);
            set64(sb, 16, epochs[lane]);
            for offset in [32, 40, 64] {
                set64(sb, offset, u64::MAX);
            }
            set32(sb, 48, 1);
            set32(sb, 72, 1);
            set64(sb, 88, 256);
            set64(sb, 112, 256);
            let bitmap = &mut media[(MB + 4096) as usize..][..257];
            set32(bitmap, 0, 0x6d746962);
            set32(bitmap, 4, 4);
            set64(bitmap, 24, epochs[lane]);
            set64(bitmap, 32, epochs[lane]);
            set64(bitmap, 40, 256);
            set32(bitmap, 52, EXTENT as u32);
            bitmap[256] = 0xfc;
            for (at, byte) in media[MB as usize + 2 * EXTENT..][..2 * EXTENT]
                .iter_mut()
                .enumerate()
            {
                *byte = (at % 223) as u8;
            }
            image
        })
        .collect()
}

#[test]
fn raid1_hides_components_checks_mirrors_and_binds_raid_state() {
    let text = mirror_metadata();
    let group = open(mirror_fixture(&text, [8, 8]), || Ok(())).unwrap();
    assert_eq!(group.inventory().volumes.len(), 1);
    let lv = &group.inventory().volumes[0];
    assert_eq!(lv.profile, "raid1");
    assert!(lv.readable);
    let id = lv.id.clone();
    let proof = group.fingerprint();
    let mut reader = group.reader(&id).unwrap();
    let mut data = Vec::new();
    reader.read_to_end(&mut data).unwrap();
    assert_eq!(data.len(), 2 * EXTENT);
    assert!(data.iter().enumerate().all(|(i, &v)| v == (i % 223) as u8));
    let refreshed = open(mirror_fixture(&text, [9, 9]), || Ok(())).unwrap();
    assert_ne!(proof, refreshed.fingerprint());
    let mut divergent = mirror_fixture(&text, [8, 8]);
    divergent[1].get_mut()[MB as usize + 2 * EXTENT + 17] ^= 1;
    let mut reader = open(divergent, || Ok(())).unwrap().reader(&id).unwrap();
    assert!(reader.read_exact(&mut [0; 512]).is_err());
    assert_eq!(reader.stream_position().unwrap(), 0);
}

#[test]
fn raid1_rejects_incomplete_dirty_conflicting_and_invalid_memberships() {
    let text = mirror_metadata();
    let mut dirty = mirror_fixture(&text, [8, 8]);
    dirty[0].get_mut()[(MB + 4096 + 256) as usize] |= 2;
    let mut reshaping = mirror_fixture(&text, [8, 8]);
    set32(reshaping[1].get_mut(), MB as usize + 60, 1);
    let mut unreadable = mirror_fixture(&text, [8, 8]);
    unreadable.pop();
    let duplicate = text.replace(
        "\"mirror_rmeta_1\", \"mirror_rimage_1\"]",
        "\"mirror_rmeta_0\", \"mirror_rimage_0\"]",
    );
    for media in [
        dirty,
        reshaping,
        unreadable,
        mirror_fixture(&text, [8, 9]),
        mirror_fixture(&duplicate, [8, 8]),
    ] {
        let group = open(media, || Ok(())).unwrap();
        assert!(!group.inventory().volumes[0].readable);
        assert!(group.reader(&uuid(MAIN).unwrap()).is_err());
    }
}

#[test]
fn degraded_mirror_requires_opt_in_and_a_clean_survivor() {
    let text = mirror_metadata();
    let mut only = mirror_fixture(&text, [8, 8]);
    only.pop();
    let strict = open(only.clone(), || Ok(())).unwrap();
    assert!(!strict.inventory().volumes[0].readable);
    let header = MB as usize;
    set64(only[0].get_mut(), header + 24, 2);
    let group = open_with_raid_policy(only.clone(), true, || Ok(())).unwrap();
    let volume = &group.inventory().volumes[0];
    assert!(volume.readable && volume.degraded_raid);
    let id = volume.id.clone();
    let mut bytes = Vec::new();
    group.reader(&id).unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len(), 2 * EXTENT);
    assert!(bytes
        .iter()
        .enumerate()
        .all(|(index, &byte)| byte == (index % 223) as u8));
    set64(only[0].get_mut(), header + 24, 1);
    let group = open_with_raid_policy(only, true, || Ok(())).unwrap();
    assert!(!group.inventory().volumes[0].readable);
    let marked_missing = text.replace(
        "device = \"/dev/ignored-1\"",
        "device = \"/dev/ignored-1\"\n      flags = [\"MISSING\"]",
    );
    let group =
        open_with_raid_policy(mirror_fixture(&marked_missing, [8, 8]), true, || Ok(())).unwrap();
    assert_eq!(group.inventory().supplied_members, 1);
    assert!(group.inventory().volumes[0].degraded_raid);
}

fn thin_metadata() -> String {
    let base = metadata(12);
    let header = base.split("  logical_volumes {").next().unwrap();
    let mut text = format!("{header}  logical_volumes {{\n");
    for (index, name, size, status, spec) in [
        (
            1,
            "pool",
            6,
            r#""READ", "WRITE", "VISIBLE""#,
            r#"
            type = "thin-pool"
            metadata = "pool_tmeta"
            pool = "pool_tdata"
            transaction_id = 9
            chunk_size = 128
            zero_new_blocks = 1"#,
        ),
        (
            2,
            "pool_tmeta",
            1,
            r#""READ", "WRITE""#,
            r#"
            type = "striped"
            stripe_count = 1
            stripes = ["pv0", 0]"#,
        ),
        (
            3,
            "pool_tdata",
            6,
            r#""READ", "WRITE""#,
            r#"
            type = "striped"
            stripe_count = 1
            stripes = ["pv1", 0]"#,
        ),
        (
            4,
            "old",
            3,
            r#""READ", "VISIBLE""#,
            r#"
            type = "thin"
            thin_pool = "pool"
            origin = "thin"
            transaction_id = 9
            device_id = 2"#,
        ),
        (
            5,
            "thin",
            3,
            r#""READ", "VISIBLE""#,
            r#"
            type = "thin"
            thin_pool = "pool"
            transaction_id = 7
            device_id = 1"#,
        ),
        (
            6,
            "external",
            2,
            r#""READ", "VISIBLE""#,
            r#"
            type = "thin"
            thin_pool = "pool"
            transaction_id = 9
            device_id = 3
            external_origin = "origin""#,
        ),
        (
            7,
            "origin",
            1,
            r#""READ", "VISIBLE""#,
            r#"
            type = "striped"
            stripe_count = 1
            stripes = ["pv1", 6]"#,
        ),
    ] {
        text.push_str(&format!(
            r#"
            {name} {{
                id = "aaaaaa-bbbb-cccc-dddd-eeee-ffff-{index:06}"
                status = [{status}]
                segment_count = 1
                segment1 {{
                    start_extent = 0
                    extent_count = {size}
                    {spec}
                }}
            }}
            "#
        ));
    }
    text.push_str("  }\n}\ncontents = \"Text Format Volume Group\"\nversion = 1\n");
    text
}

fn thin_sum(block: &mut [u8], xor: u32) {
    let sum = !btrfs_disk::util::btrfs_csum_data(&block[4..]) ^ xor;
    set32(block, 0, sum);
}

fn fixture_block(image: &mut Cursor<Vec<u8>>, index: u64) -> &mut [u8] {
    &mut image.get_mut()[MB as usize + index as usize * UNIT..][..UNIT]
}

fn thin_node(
    image: &mut Cursor<Vec<u8>>,
    index: u64,
    internal: bool,
    words: usize,
    entries: &[(u64, &[u64])],
) {
    let block = fixture_block(image, index);
    let slots = (UNIT - 32) / ((words + 1) * 8) / 3 * 3;
    set32(block, 4, if internal { 1 } else { 2 });
    set64(block, 8, index);
    set32(block, 16, entries.len() as u32);
    set32(block, 20, slots as u32);
    set32(block, 24, (words * 8) as u32);
    for (offset, (key, value)) in entries.iter().enumerate() {
        assert_eq!(value.len(), words);
        set64(block, 32 + offset * 8, *key);
        for (word, &value) in value.iter().enumerate() {
            set64(block, 32 + (slots + offset * words + word) * 8, value);
        }
    }
    thin_sum(block, 121107);
}

fn thin_fixture(text: &str) -> Vec<Cursor<Vec<u8>>> {
    let mut metadata = physical_image(PV1, Some(text), false);
    let mut data = physical_image(PV2, None, false);
    let sb = fixture_block(&mut metadata, 0);
    for (offset, value) in [
        (32, 27022010),
        (48, 9),
        (64, 6),
        (72, 3),
        (80, 3),
        (192, 16),
        (200, 11),
        (320, 1),
        (328, 2),
        (344, 16),
    ] {
        set64(sb, offset, value);
    }
    for (offset, value) in [(40, 2), (44, 1), (336, 128), (340, 8)] {
        set32(sb, offset, value);
    }
    thin_sum(sb, 160774);
    thin_node(
        &mut metadata,
        1,
        false,
        1,
        &[(1, &[4]), (2, &[5]), (3, &[6])],
    );
    thin_node(
        &mut metadata,
        2,
        false,
        3,
        &[(1, &[2, 7, 0]), (2, &[2, 9, 1]), (3, &[0, 9, 1])],
    );
    thin_node(&mut metadata, 3, false, 2, &[(0, &[10, 16317])]);
    thin_node(&mut metadata, 4, true, 1, &[(0, &[7]), (2, &[8])]);
    thin_node(&mut metadata, 5, false, 1, &[(0, &[0]), (2, &[2 << 24])]);
    thin_node(&mut metadata, 6, false, 1, &[]);
    thin_node(&mut metadata, 7, false, 1, &[(0, &[3 << 24])]);
    thin_node(&mut metadata, 8, false, 1, &[(2, &[2 << 24])]);
    let bitmap = fixture_block(&mut metadata, 10);
    set64(bitmap, 8, 10);
    bitmap[16] = 0x62;
    thin_sum(bitmap, 240779);
    for (index, byte) in [(0, 0x11), (1, 0xee), (2, 0x22), (3, 0x33), (6, 0x44)] {
        data.get_mut()[MB as usize + index * EXTENT..][..EXTENT].fill(byte);
    }
    vec![metadata, data]
}

#[test]
fn thin_snapshots_use_their_own_checked_mapping_and_holes() {
    let text = thin_metadata();
    for (name, profile, contents) in [
        ("old", "thinSnapshot", &[0x11, 0, 0x22][..]),
        ("thin", "thin", &[0x33, 0, 0x22][..]),
        ("external", "thin", &[0x44, 0][..]),
    ] {
        let group = open(thin_fixture(&text), || Ok(())).unwrap();
        assert_eq!(group.inventory().volumes.len(), 5);
        let volume = group
            .inventory()
            .volumes
            .iter()
            .find(|lv| lv.name == name)
            .unwrap();
        assert!(volume.readable, "{:?}", volume.reason);
        assert_eq!(volume.profile, profile);
        let id = volume.id.clone();
        let mut reader = group.reader(&id).unwrap();
        let mut data = Vec::new();
        reader.read_to_end(&mut data).unwrap();
        assert_eq!(data.len(), contents.len() * EXTENT);
        for (&byte, chunk) in contents.iter().zip(data.chunks_exact(EXTENT)) {
            assert!(chunk.iter().all(|&actual| actual == byte));
        }
    }
}

#[test]
fn thin_rejects_corrupt_stale_missing_and_ambiguous_metadata() {
    let text = thin_metadata();
    let result = |fixture| {
        open(fixture, || Ok(()))
            .unwrap()
            .inventory()
            .volumes
            .iter()
            .find(|lv| lv.name == "thin")
            .unwrap()
            .readable
    };
    let mut bad_node = thin_fixture(&text);
    fixture_block(&mut bad_node[0], 7)[48] ^= 1;
    let mut free_block = thin_fixture(&text);
    let bitmap = fixture_block(&mut free_block[0], 10);
    bitmap[16] &= 0x3f;
    thin_sum(bitmap, 240779);
    let mut cycle = thin_fixture(&text);
    thin_node(&mut cycle[0], 4, true, 1, &[(0, &[4]), (2, &[8])]);
    let mut missing = thin_fixture(&text);
    missing.pop();
    for fixture in [
        bad_node,
        free_block,
        cycle,
        missing,
        thin_fixture(&text.replace("transaction_id = 9", "transaction_id = 10")),
        thin_fixture(&text.replace("device_id = 2", "device_id = 1")),
    ] {
        assert!(!result(fixture));
    }
    let first = open(thin_fixture(&text), || Ok(())).unwrap().fingerprint();
    let mut updated = thin_fixture(&text);
    thin_node(&mut updated[0], 7, false, 1, &[(0, &[0])]);
    assert!(result(updated.clone()));
    assert_ne!(first, open(updated, || Ok(())).unwrap().fingerprint());
}

fn snapshot_metadata(sparse: bool) -> String {
    let base = metadata(13);
    let header = base.split("  logical_volumes {").next().unwrap();
    let visible = if sparse { "" } else { r#", "VISIBLE""# };
    let origin = if sparse {
        r#"type = "zero""#
    } else {
        r#"
        type = "striped"
        stripe_count = 1
        stripes = ["pv0", 0]
    "#
    };
    format!(
        r#"{header}
      logical_volumes {{
        origin {{
          id = "{MAIN}"
          status = ["READ", "WRITE"{visible}]
          segment_count = 1
          segment1 {{
            start_extent = 0
            extent_count = 4
            {origin}
          }}
        }}
        point {{
          id = "{RESCUE}"
          status = ["READ", "WRITE"]
          segment_count = 1
          segment1 {{
            start_extent = 0
            extent_count = 2
            type = "striped"
            stripe_count = 1
            stripes = ["pv1", 0]
          }}
        }}
        snapshot0 {{
          id = "aaaaaa-bbbb-cccc-dddd-eeee-ffff-ssssss"
          status = ["READ", "WRITE", "VISIBLE"]
          segment_count = 1
          segment1 {{
            start_extent = 0
            extent_count = 4
            type = "snapshot"
            origin = "origin"
            cow_store = "point"
            chunk_size = 8
          }}
        }}
      }}
    }}
    contents = "Text Format Volume Group"
    version = 1
    "#
    )
}

fn snapshot_fixture(text: &str) -> Vec<Cursor<Vec<u8>>> {
    let mut origin = physical_image(PV1, Some(text), false);
    let mut store = physical_image(PV2, None, false);
    origin.get_mut()[MB as usize..][..4 * EXTENT].fill(0x41);
    let header = fixture_block(&mut store, 0);
    header[..4].copy_from_slice(b"SnAp");
    set32(header, 4, 1);
    set32(header, 8, 1);
    set32(header, 12, 8);
    let exceptions = fixture_block(&mut store, 1);
    set64(exceptions, 0, 4);
    set64(exceptions, 8, 2);
    set64(exceptions, 16, 1);
    set64(exceptions, 24, 3);
    fixture_block(&mut store, 2).fill(0x42);
    fixture_block(&mut store, 3).fill(0x43);
    vec![origin, store]
}

#[test]
fn classic_snapshot_overlays_saved_chunks_on_origin_or_zero() {
    for sparse in [false, true] {
        let group = open(snapshot_fixture(&snapshot_metadata(sparse)), || Ok(())).unwrap();
        let visible: Vec<_> = group
            .inventory()
            .volumes
            .iter()
            .map(|lv| lv.name.as_str())
            .collect();
        assert_eq!(
            visible,
            if sparse {
                vec!["point"]
            } else {
                vec!["origin", "point"]
            }
        );
        let info = group.inventory().volumes.last().unwrap();
        assert_eq!(info.profile, "snapshot");
        assert_eq!(info.size, 4 * EXTENT as u64);
        assert!(info.readable, "{:?}", info.reason);
        let mut disk = group.reader(&uuid(RESCUE).unwrap()).unwrap();
        disk.seek(SeekFrom::Start(UNIT as u64 - 8)).unwrap();
        let mut crossing = [0; 16];
        disk.read_exact(&mut crossing).unwrap();
        assert_eq!(crossing[..8], [if sparse { 0 } else { 0x41 }; 8]);
        assert_eq!(crossing[8..], [0x43; 8]);
        disk.seek(SeekFrom::Start(4 * UNIT as u64)).unwrap();
        disk.read_exact(&mut crossing).unwrap();
        assert_eq!(crossing, [0x42; 16]);
    }
}

#[test]
fn classic_snapshot_rejects_missing_invalid_and_ambiguous_stores() {
    let text = snapshot_metadata(false);
    let mut missing = snapshot_fixture(&text);
    missing.pop();
    let mut invalidated = snapshot_fixture(&text);
    set32(fixture_block(&mut invalidated[1], 0), 4, 0);
    let mut duplicate = snapshot_fixture(&text);
    set64(fixture_block(&mut duplicate[1], 1), 16, 4);
    let mut metadata_alias = snapshot_fixture(&text);
    set64(fixture_block(&mut metadata_alias[1], 1), 24, 1);
    let mut incomplete = snapshot_fixture(&text);
    set64(fixture_block(&mut incomplete[1], 1), 32, 2);
    for fixture in [missing, invalidated, duplicate, metadata_alias, incomplete] {
        let group = open(fixture, || Ok(())).unwrap();
        assert!(group.inventory().volumes[0].readable);
        assert!(!group.inventory().volumes[1].readable);
        assert!(group.reader(&uuid(RESCUE).unwrap()).is_err());
    }
    let merging = text.replace("cow_store", "merge_store");
    let group = open(snapshot_fixture(&merging), || Ok(())).unwrap();
    assert!(group.inventory().volumes.iter().all(|lv| !lv.readable));
    let first = open(snapshot_fixture(&text), || Ok(()))
        .unwrap()
        .fingerprint();
    let mut shifted = snapshot_fixture(&text);
    set64(fixture_block(&mut shifted[1], 1), 0, 5);
    let second = open(shifted, || Ok(())).unwrap();
    assert!(second.inventory().volumes[1].readable);
    assert_ne!(first, second.fingerprint());
}
