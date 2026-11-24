use disk_core::fat_format::{FatType, Plan};
use std::{
    fs::OpenOptions,
    io::{Cursor, Write},
    path::Path,
};

const SECTOR: usize = 4096;
const MIB: usize = 1024 * 1024;
const VOLUME: usize = 4 * MIB;
const NOTE: &[u8] = b"DiskStudio 4Kn directory fixture\r\n";
const DELETED: &[u8] = b"DiskStudio 4Kn deleted fixture\r\n";

fn fat12(hidden: usize) -> Vec<u8> {
    let mut image = Cursor::new(vec![0; VOLUME]);
    Plan::new(
        FatType::Fat12,
        VOLUME as u64,
        SECTOR as u32,
        SECTOR as u32,
        "DS4KNVIEW",
        0x4453_344b,
        (hidden / SECTOR) as u32,
    )
    .unwrap()
    .format(&mut image, |_, _| Ok(()))
    .unwrap();
    let mut image = image.into_inner();
    let word = |offset| u16::from_le_bytes(image[offset..offset + 2].try_into().unwrap()) as usize;
    let fat_start = word(14) * SECTOR;
    let fat_size = word(22) * SECTOR;
    let root_start = fat_start + image[16] as usize * fat_size;
    let data_start = root_start + (word(17) * 32).div_ceil(SECTOR) * SECTOR;
    let cluster_size = image[13] as usize * SECTOR;
    for fat in [fat_start, fat_start + fat_size] {
        for cluster in [2, 3] {
            let pos = fat + cluster * 3 / 2;
            let packed = u16::from_le_bytes(image[pos..pos + 2].try_into().unwrap());
            let packed = if cluster % 2 == 0 {
                packed | 0x0fff
            } else {
                packed | 0xfff0
            };
            image[pos..pos + 2].copy_from_slice(&packed.to_le_bytes());
        }
    }
    directory_entry(
        &mut image[root_start + 32..root_start + 64],
        b"DOCS       ",
        0x10,
        2,
        0,
    );
    directory_entry(
        &mut image[root_start + 64..root_start + 96],
        b"\xe5OST    TXT",
        0x20,
        4,
        DELETED.len(),
    );
    directory_entry(
        &mut image[data_start..data_start + 32],
        b".          ",
        0x10,
        2,
        0,
    );
    directory_entry(
        &mut image[data_start + 32..data_start + 64],
        b"..         ",
        0x10,
        0,
        0,
    );
    directory_entry(
        &mut image[data_start + 64..data_start + 96],
        b"NOTE    TXT",
        0x20,
        3,
        NOTE.len(),
    );
    let note = data_start + cluster_size;
    image[note..note + NOTE.len()].copy_from_slice(NOTE);
    let deleted = data_start + 2 * cluster_size;
    image[deleted..deleted + DELETED.len()].copy_from_slice(DELETED);
    image
}

fn directory_entry(entry: &mut [u8], name: &[u8; 11], attributes: u8, cluster: u16, length: usize) {
    entry[..11].copy_from_slice(name);
    entry[11] = attributes;
    entry[26..28].copy_from_slice(&cluster.to_le_bytes());
    entry[28..32].copy_from_slice(&(length as u32).to_le_bytes());
}

fn mbr_partition(image: &mut [u8], start: usize) {
    let entry = &mut image[446..462];
    entry[4] = 0x01;
    entry[8..12].copy_from_slice(&((start / SECTOR) as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&((VOLUME / SECTOR) as u32).to_le_bytes());
    image[510..512].copy_from_slice(&[0x55, 0xaa]);
}

fn write_image(folder: &Path, name: &str, image: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(folder.join(name))
        .unwrap();
    file.write_all(image).unwrap();
    file.sync_all().unwrap();
}

#[test]
#[ignore = "creates ordinary 4Kn test images in a caller-prepared folder"]
fn creates_winpe_4kn_registration_fixture_files() {
    let folder =
        std::path::PathBuf::from(std::env::var_os("DISKSTUDIO_WINPE_4KN_FIXTURE").unwrap());
    let mut library = vec![0; 5 * MIB];
    library[MIB..].copy_from_slice(&fat12(MIB));
    mbr_partition(&mut library, MIB);
    write_image(&folder, "library.img", &library);

    let mut both = vec![0; 12 * MIB];
    for start in [MIB, 6 * MIB] {
        both[start..start + VOLUME].copy_from_slice(&fat12(start));
    }
    write_image(&folder, "searchboth.img", &both);
    mbr_partition(&mut both, 6 * MIB);
    write_image(&folder, "searchfree.img", &both);
}
