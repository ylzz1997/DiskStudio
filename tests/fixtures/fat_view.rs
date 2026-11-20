//! Deterministic FAT32 and exFAT fixtures for read-only reconstruction checks.
use disk_core::fat_format::{FatType, Plan};
use std::io::Cursor;

pub const FAT_CONTENT: &[u8] = b"FAT32 view";
pub const EXFAT_CONTENT: &[u8] = b"exFAT view";

pub fn fat32(sector: usize) -> Vec<u8> {
    let volume_bytes = 68_608 * sector;
    let plan = Plan::new(
        FatType::Fat32,
        volume_bytes as u64,
        sector as u32,
        sector as u32,
        "FATVIEW",
        0x37be_24d7,
        0,
    )
    .unwrap();
    let mut stream = Cursor::new(vec![0u8; volume_bytes]);
    plan.format(&mut stream, |_, _| Ok(())).unwrap();
    let mut image = stream.into_inner();
    let start = u16::from_le_bytes(image[14..16].try_into().unwrap()) as usize;
    let fat = u32::from_le_bytes(image[36..40].try_into().unwrap()) as usize;
    for at in [start, start + fat] {
        image[at * sector + 12..at * sector + 16].copy_from_slice(&0x0fff_ffffu32.to_le_bytes());
    }
    let root = (start + 2 * fat) * sector;
    let entry = &mut image[root + 32..root + 64];
    entry[..11].copy_from_slice(b"FATVIEW TXT");
    entry[11] = 0x20;
    entry[26] = 3;
    entry[28] = FAT_CONTENT.len() as u8;
    image[root + sector..root + sector + FAT_CONTENT.len()].copy_from_slice(FAT_CONTENT);
    image
}

pub fn exfat(sector: usize) -> Vec<u8> {
    let mut image = vec![0u8; sector * 2048];
    image[..11].copy_from_slice(b"\xeb\x76\x90EXFAT   ");
    image[72..80].copy_from_slice(&2048u64.to_le_bytes());
    for (offset, field) in [
        (80, 24u32),
        (84, 16),
        (88, 40),
        (92, 2008),
        (96, 2),
        (100, 0xe1ec_a27b),
    ] {
        image[offset..offset + 4].copy_from_slice(&field.to_le_bytes());
    }
    image[105] = 1;
    image[108] = sector.trailing_zeros() as u8;
    image[110] = 1;
    image[111] = 0x80;
    image[112] = 0xff;
    image[120..510].fill(0xf4);
    image[510..512].copy_from_slice(&[0x55, 0xaa]);
    for index in 1..=8 {
        image[sector * (index + 1) - 4..sector * (index + 1)].copy_from_slice(&[0, 0, 0x55, 0xaa]);
    }
    let sum = image[..11 * sector]
        .iter()
        .enumerate()
        .fold(0u32, |sum, (index, &byte)| {
            if matches!(index, 106 | 107 | 112) {
                sum
            } else {
                sum.rotate_right(1).wrapping_add(u32::from(byte))
            }
        })
        .to_le_bytes();
    for word in image[11 * sector..12 * sector].chunks_exact_mut(4) {
        word.copy_from_slice(&sum);
    }
    image.copy_within(..12 * sector, 12 * sector);
    let fat = 24 * sector;
    image[fat..fat + 4].copy_from_slice(&0xffff_fff8u32.to_le_bytes());
    image[fat + 4..fat + 16].fill(0xff);
    image[fat + 20..fat + 24].fill(0xff);
    let root = 40 * sector;
    let bitmap = &mut image[root..root + 32];
    bitmap[0] = 0x81;
    bitmap[20] = 3;
    bitmap[24] = 251;
    image[41 * sector] = 15;

    // ASCII upcase mappings followed by a compressed run of identity mappings.
    let upcase: Vec<u8> = (0u8..=127)
        .map(|c| u16::from(c.to_ascii_uppercase()))
        .chain([0xffff, 65_408])
        .flat_map(u16::to_le_bytes)
        .collect();
    let sum = upcase.iter().fold(0u32, |hash, &byte| {
        hash.rotate_right(1).wrapping_add(u32::from(byte))
    });
    let entry = &mut image[root + 32..root + 64];
    entry[0] = 0x82;
    entry[4..8].copy_from_slice(&sum.to_le_bytes());
    entry[20] = 5;
    entry[24..26].copy_from_slice(&(upcase.len() as u16).to_le_bytes());
    image[43 * sector..43 * sector + upcase.len()].copy_from_slice(&upcase);

    let entries = &mut image[root + 64..root + 160];
    entries[0] = 0x85;
    entries[1] = 2;
    entries[4] = 0x20;
    entries[32] = 0xc0;
    entries[33] = 3;
    entries[40] = EXFAT_CONTENT.len() as u8;
    entries[52] = 4;
    entries[56] = EXFAT_CONTENT.len() as u8;
    entries[64] = 0xc1;
    let mut name_hash = 0u16;
    for (bytes, c) in entries[66..]
        .chunks_exact_mut(2)
        .zip("exfat-view.txt".encode_utf16())
    {
        bytes.copy_from_slice(&c.to_le_bytes());
        for byte in (c as u8)
            .to_ascii_uppercase()
            .to_le_bytes()
            .into_iter()
            .chain([0])
        {
            name_hash = name_hash.rotate_right(1).wrapping_add(u16::from(byte));
        }
    }
    entries[35] = "exfat-view.txt".len() as u8;
    entries[36..38].copy_from_slice(&name_hash.to_le_bytes());
    let sum = entries
        .iter()
        .enumerate()
        .fold(0u16, |hash, (index, &byte)| {
            if matches!(index, 2 | 3) {
                hash
            } else {
                hash.rotate_right(1).wrapping_add(u16::from(byte))
            }
        });
    entries[2..4].copy_from_slice(&sum.to_le_bytes());
    image[42 * sector..42 * sector + EXFAT_CONTENT.len()].copy_from_slice(EXFAT_CONTENT);
    image
}
