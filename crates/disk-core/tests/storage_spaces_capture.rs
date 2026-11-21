use disk_core::storage_spaces::{self, Allocation};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, ErrorKind, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
struct Manifest {
    slot: char,
    sector: u64,
    capacity: u64,
    regions: Vec<Region>,
    #[serde(default)]
    witnesses: Vec<Witness>,
    #[serde(default)]
    internal: Vec<Region>,
}

#[derive(Deserialize)]
struct Region {
    offset: u64,
    bytes: usize,
}

#[derive(Deserialize)]
struct Witness {
    #[serde(flatten)]
    region: Region,
    kind: u8,
    logical: u64,
}

#[derive(Clone)]
struct Capture {
    capacity: u64,
    position: u64,
    windows: Vec<(u64, Vec<u8>)>,
}

impl Capture {
    fn load(directory: &Path, manifest: &Manifest) -> Self {
        let mut packed =
            GzDecoder::new(File::open(directory.join(format!("{}.gz", manifest.slot))).unwrap());
        let windows = manifest
            .regions
            .iter()
            .chain(manifest.witnesses.iter().map(|marker| &marker.region))
            .chain(&manifest.internal)
            .map(|region| {
                assert!(region.bytes <= 5 * 1024 * 1024);
                let mut bytes = vec![0; region.bytes];
                packed.read_exact(&mut bytes).unwrap();
                (region.offset, bytes)
            })
            .collect();
        assert_eq!(packed.read(&mut [0; 1]).unwrap(), 0);
        Self {
            capacity: manifest.capacity,
            position: 0,
            windows,
        }
    }
}

const MIB: u64 = 1024 * 1024;
const WITNESS_OFFSETS: [u64; 18] = [
    0,
    4096,
    61440,
    65536,
    69632,
    131072,
    196608,
    262144,
    256 * MIB - 4096,
    256 * MIB,
    256 * MIB + 65536,
    512 * MIB - 4096,
    512 * MIB,
    512 * MIB + 65536,
    512 * MIB + 131072,
    768 * MIB,
    768 * MIB + 65536,
    1024 * MIB - 4096,
];

fn digest_witnesses(reader: &mut (impl Read + Seek)) -> String {
    let mut digest = Sha256::new();
    let mut bytes = [0; 4096];
    for offset in WITNESS_OFFSETS {
        reader.seek(SeekFrom::Start(offset)).unwrap();
        reader.read_exact(&mut bytes).unwrap();
        digest.update(bytes);
    }
    format!("{:x}", digest.finalize())
}

#[test]
#[ignore = "requires the independently captured Windows logical-sector witnesses"]
fn reconstruct_windows_fixed_mirror_bytes() {
    let directory = PathBuf::from(
        std::env::var_os("DISKSTUDIO_SPACES_WITNESS_CAPTURE")
            .expect("set witness capture directory"),
    );
    let manifests: [Manifest; 2] =
        serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    for offset in WITNESS_OFFSETS {
        let occurrences = |manifest: &Manifest| {
            manifest
                .witnesses
                .iter()
                .filter(|marker| marker.kind == 1 && marker.logical == offset)
                .count()
        };
        assert_eq!(occurrences(&manifests[0]), 1, "mirror A {offset:#x}");
        assert_eq!(occurrences(&manifests[1]), 1, "mirror B {offset:#x}");
    }
    let [mut a, mut b] = manifests
        .each_ref()
        .map(|manifest| Capture::load(&directory, manifest));
    let cache_directory = PathBuf::from(
        std::env::var_os("DISKSTUDIO_SPACES_CACHE_CAPTURE")
            .expect("set complete mirror cache capture directory"),
    );
    let cache_manifests: [Manifest; 2] =
        serde_json::from_slice(&fs::read(cache_directory.join("manifest.json")).unwrap()).unwrap();
    for (member, manifest) in [&mut a, &mut b].into_iter().zip(&cache_manifests) {
        let cache = Capture::load(&cache_directory, manifest);
        member
            .windows
            .extend(cache.windows.into_iter().skip(manifest.regions.len()));
    }
    let mut group = storage_spaces::Group::open(vec![b.clone(), a.clone()], 4096).unwrap();
    let id = |name| {
        group
            .metadata()
            .spaces
            .values()
            .find(|space| space.name == name)
            .unwrap()
            .id
    };
    let mirror = id("DS-TEST-MIRROR");
    let mirror_sha = "e2c8ccaa7ccb2bfcdd0525871678d3eecd71ec94e666f644a659f6f488206a06";
    let mut rebuilt = group.reader(mirror, false).unwrap();
    assert_eq!(digest_witnesses(&mut rebuilt), mirror_sha);
    assert_eq!(rebuilt.seek(SeekFrom::End(0)).unwrap(), 1024 * MIB);
    assert_eq!(rebuilt.read(&mut [0; 1]).unwrap(), 0);
    for offset in [60 * 1024, 256 * MIB - 4096, 512 * MIB - 4096] {
        let mut block = [0; 8192];
        rebuilt.seek(SeekFrom::Start(offset)).unwrap();
        rebuilt.read_exact(&mut block).unwrap();
        assert_eq!(
            u64::from_le_bytes(block[24..32].try_into().unwrap()),
            offset
        );
        assert_eq!(
            u64::from_le_bytes(block[4120..4128].try_into().unwrap()),
            offset + 4096
        );
    }

    for member in [a.clone(), b.clone()] {
        let mut single = storage_spaces::Group::open(vec![member], 4096).unwrap();
        assert!(single.reader(mirror, false).is_err());
        assert_eq!(
            digest_witnesses(&mut single.reader(mirror, true).unwrap()),
            mirror_sha
        );
    }

    let mut incomplete = [a.clone(), b.clone()];
    for member in &mut incomplete {
        for record in [48, 49] {
            let span = &mut member.windows[2].1[4096 + record * 64 + 30];
            assert_eq!(*span, 4);
            *span = 3;
        }
    }
    let mut incomplete = storage_spaces::Group::open(Vec::from(incomplete), 4096).unwrap();
    assert!(incomplete.reader(mirror, false).is_err());

    let mut dirty = b.clone();
    let (_, directory) = dirty
        .windows
        .iter_mut()
        .find(|(offset, _)| *offset == 0x41000000)
        .unwrap();
    directory[0x400000] = 1;
    let mut dirty = storage_spaces::Group::open(vec![a.clone(), dirty], 4096).unwrap();
    assert!(dirty.reader(mirror, false).is_err());

    let mut divergent = b;
    let target = manifests[1]
        .witnesses
        .iter()
        .find(|marker| marker.kind == 1 && marker.logical == 0)
        .unwrap();
    let (_, bytes) = divergent
        .windows
        .iter_mut()
        .find(|(offset, _)| *offset == target.region.offset)
        .unwrap();
    bytes[4095] ^= 1;
    let mut group = storage_spaces::Group::open(vec![a, divergent], 4096).unwrap();
    let error = group
        .reader(mirror, false)
        .unwrap()
        .read_exact(&mut [0; 4096])
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
}

#[test]
#[ignore = "requires the full read-only capture of the Windows thin witness fixture"]
fn windows_thin_markers_extend_beyond_direct_allocations() {
    let directory = PathBuf::from(
        std::env::var_os("DISKSTUDIO_SPACES_FULL_CAPTURE")
            .expect("set full witness capture directory"),
    );
    let manifests: [Manifest; 2] =
        serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    let [a, b] = manifests
        .each_ref()
        .map(|manifest| Capture::load(&directory, manifest));
    let mut members = [a.clone(), b.clone()];
    let pool = storage_spaces::read_member(&mut members[0], 4096).unwrap();
    let thin = pool
        .spaces
        .values()
        .find(|space| space.name == "DS-TEST-THIN")
        .unwrap()
        .id;
    let direct: Vec<_> = pool
        .allocations
        .iter()
        .filter(|allocation| allocation.space == thin)
        .collect();
    assert_eq!(direct.len(), 2);
    assert!(direct.iter().all(|allocation| allocation.flags == 0
        && allocation.virtual_block == 0
        && allocation.span == 1
        && allocation.member_block == 12));

    let physical_direct_start = pool.partition.start + (12 + 2) * 256 * MIB;
    let mut digest = Sha256::new();
    for offset in WITNESS_OFFSETS {
        let matching: Vec<_> = manifests
            .iter()
            .enumerate()
            .flat_map(|(slot, manifest)| {
                manifest
                    .witnesses
                    .iter()
                    .filter(move |marker| marker.kind == 2 && marker.logical == offset)
                    .map(move |marker| (slot, marker))
            })
            .collect();
        let [(slot, marker)] = matching.as_slice() else {
            panic!("expected a unique thin marker at {offset:#x}")
        };
        if offset < 512 * MIB {
            assert!((physical_direct_start..physical_direct_start + 256 * MIB)
                .contains(&marker.region.offset));
        } else {
            assert!(marker.region.offset < physical_direct_start);
        }
        let mut bytes = [0; 4096];
        members[*slot]
            .seek(SeekFrom::Start(marker.region.offset))
            .unwrap();
        members[*slot].read_exact(&mut bytes).unwrap();
        digest.update(bytes);
    }
    assert_eq!(
        format!("{:x}", digest.finalize()),
        "8956dc0a5434c4671d735d07408c790d3d8d98c6e992b28843e87b2d87b0d5b9"
    );
    let mut group = storage_spaces::Group::open(vec![a, b], 4096).unwrap();
    assert!(group.reader(thin, false).is_err());
}

#[test]
#[ignore = "requires the exact internal thin cache captured in the isolated guest"]
fn windows_thin_checkpoint_describes_cached_witnesses() {
    let directory = PathBuf::from(
        std::env::var_os("DISKSTUDIO_SPACES_INTERNAL_CAPTURE")
            .expect("set internal capture directory"),
    );
    let manifests: [Manifest; 2] =
        serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    let mut members = manifests
        .each_ref()
        .map(|manifest| Capture::load(&directory, manifest));
    let pool = storage_spaces::read_member(&mut members[0], 4096).unwrap();
    let mut guid = pool
        .spaces
        .values()
        .find(|space| space.name == "DS-TEST-THIN")
        .unwrap()
        .guid;
    guid[..4].reverse();
    guid[4..6].reverse();
    guid[6..8].reverse();
    let cache = manifests[0].internal[0].offset;
    let le32 = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().unwrap());
    let mut validate = |at, magic: &[u8; 8], length| {
        let mut bytes = vec![0; length];
        members[0].seek(SeekFrom::Start(cache + at)).unwrap();
        members[0].read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes[..8], magic);
        assert_eq!(bytes[8..24], guid);
        assert_eq!(le32(&bytes[24..28]), 1);
        assert_eq!(le32(&bytes[28..32]) as usize, length);
        let crc = le32(&bytes[36..40]);
        bytes[36..40].fill(0);
        assert_eq!(crc32fast::hash(&bytes), crc);
        bytes
    };
    validate(0, b"SPCACHE\0", 96);
    for slot in 0..6 {
        let record = validate(0x2000 + 4096 * slot, b"SPSLOT\0\0", 4096);
        assert_eq!(le32(&record[40..44]), slot as u32 + 1);
    }
    let header = validate(0x202000, b"SPCHECK\0", 80);
    assert_eq!(le32(&header[40..44]), 7);
    assert_eq!(le32(&header[72..76]), 96);
    assert_eq!(le32(&header[76..80]), 4);
    let mut records = [0; 96];
    members[0].seek(SeekFrom::Start(cache + 0x203000)).unwrap();
    members[0].read_exact(&mut records).unwrap();
    assert_eq!(crc32fast::hash(&records), le32(&header[48..52]));

    let offsets = &WITNESS_OFFSETS[12..];
    for &offset in offsets {
        let record = records
            .chunks_exact(24)
            .find(|entry| {
                let start = u64::from_le_bytes(entry[..8].try_into().unwrap());
                (start..start + 128 * 1024).contains(&offset)
            })
            .unwrap();
        let start = u64::from_le_bytes(record[..8].try_into().unwrap());
        let slot = u64::from(le32(&record[8..12]));
        let bitmap = le32(&record[16..20]);
        let page = (offset - start) / 4096;
        assert_ne!(bitmap & (1 << page), 0);
        let column = (page / 16) as usize;
        let physical = cache + 0x240000 + slot * 64 * 1024 + page % 16 * 4096;
        let mut cached = [0; 4096];
        members[column].seek(SeekFrom::Start(physical)).unwrap();
        members[column].read_exact(&mut cached).unwrap();
        assert_eq!(&cached[..16], b"DS-SPACES-DATA1\0");
        assert_eq!(
            u64::from_le_bytes(cached[24..32].try_into().unwrap()),
            offset
        );
    }
}

impl Read for Capture {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        for (base, window) in &self.windows {
            let Some(offset) = self.position.checked_sub(*base) else {
                continue;
            };
            if offset >= window.len() as u64 {
                continue;
            }
            let data = &window[offset as usize..];
            let count = output.len().min(data.len());
            output[..count].copy_from_slice(&data[..count]);
            self.position += count as u64;
            return Ok(count);
        }
        // Uncaptured bytes may contain data. Supplying zeros would conceal unverified reads.
        Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            "outside captured regions",
        ))
    }
}

impl Seek for Capture {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => self.capacity.checked_add_signed(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
        }
        .ok_or(ErrorKind::InvalidInput)?;
        Ok(self.position)
    }
}

#[test]
#[ignore = "requires the captured Windows A/B fixture archived by Ubuntu"]
fn windows_mirror_and_thin_offline_metadata() {
    let directory = PathBuf::from(
        std::env::var_os("DISKSTUDIO_SPACES_CAPTURE").expect("set capture directory"),
    );
    let manifests: [Manifest; 2] =
        serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    let [a, b] = manifests
        .each_ref()
        .map(|manifest| Capture::load(&directory, manifest));
    let sector = manifests[0].sector;
    assert_eq!(sector, 4096);
    assert_eq!(sector, manifests[1].sector);
    let read = |capture: &Capture| storage_spaces::read_member(&mut capture.clone(), sector);
    let left = read(&a).unwrap();
    let right = read(&b).unwrap();
    assert_eq!(left.name, "DS-TEST-POOL");
    assert_eq!(left.members.len(), 2);
    assert_ne!(left.member, right.member);
    assert_ne!(left.layout_digest, right.layout_digest);
    assert_eq!(left.database_digest, right.database_digest);
    assert_eq!(left.partition, (16 << 20)..(b.capacity - (1 << 20)));
    assert_eq!(
        left.spaces
            .values()
            .filter(|space| !space.name.is_empty())
            .map(|space| space.name.as_str())
            .collect::<Vec<_>>(),
        ["DS-TEST-MIRROR", "DS-TEST-THIN"]
    );
    assert_eq!(left.allocations.len(), 20);
    assert_eq!(
        left.allocation_owners.keys().copied().collect::<Vec<_>>(),
        [31, 36, 63]
    );
    assert!(left.allocations.contains(&Allocation {
        flags: 0,
        group: 2,
        span: 4,
        space: 5,
        virtual_block: 0,
        column: 0,
        copy: 0,
        member: 2,
        member_block: 6,
    }));
    assert_eq!(
        storage_spaces::missing_members(std::slice::from_ref(&left))
            .unwrap()
            .len(),
        1
    );
    assert!(storage_spaces::missing_members(&[read(&a).unwrap(), read(&a).unwrap()]).is_err());
    assert!(storage_spaces::missing_members(&[left, right])
        .unwrap()
        .is_empty());

    for index in [12, 4096 + 12, 4608 + 64 + 13] {
        let mut damaged = a.clone();
        damaged.windows[2].1[index] ^= 1;
        assert!(read(&damaged).is_err(), "accepted damage at {index:#x}");
    }
    let mut stale = b.clone();
    // Mutate an additional record with no interpreted fields. Framing still
    // passes, but the two member databases must no longer be combined.
    stale.windows[2].1[4096 + 34 * 64 + 53] ^= 1;
    assert!(storage_spaces::missing_members(&[read(&a).unwrap(), read(&stale).unwrap()]).is_err());
}
