//! In-memory Btrfs fixture shared by the core and recovery-path tests.
const SUPER_BYTES: usize = 4096;
const SUPER_OFFSETS: [u64; 1] = [64 * 1024];
const SYSTEM_ARRAY: usize = 0x32b;
const HEADER: usize = 0x65;
const KEY_BYTES: usize = 17;
const CHUNK_TREE: u64 = 3;
const ROOT_TREE: u64 = 1;
const CHUNK_ITEM: u8 = 228;
const DEVICE_ITEM: u8 = 216;
const ROOT_ITEM: u8 = 132;
const INODE_ITEM: u8 = 1;
const DIR_INDEX: u8 = 96;
const SYSTEM: u64 = 2;
const METADATA: u64 = 4;
const DUP: u64 = 32;

fn crc32c(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82f63b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub const MIB: u64 = 1024 * 1024;
const SIZE: u64 = 8 * MIB;
const FSID: [u8; 16] = [3; 16];
const UUID: [u8; 16] = [9; 16];

fn put(out: &mut [u8], at: usize, value: &[u8]) {
    out[at..at + value.len()].copy_from_slice(value);
}

pub fn set32(out: &mut [u8], at: usize, value: u32) {
    put(out, at, &value.to_le_bytes());
}

pub fn set64(out: &mut [u8], at: usize, value: u64) {
    put(out, at, &value.to_le_bytes());
}

pub fn sign(out: &mut [u8]) {
    let checksum = crc32c(&out[32..]);
    set32(out, 0, checksum);
}

fn key(out: &mut [u8], object_id: u64, kind: u8, offset: u64) {
    set64(out, 0, object_id);
    out[8] = kind;
    set64(out, 9, offset);
}

fn device() -> Vec<u8> {
    let mut item = vec![0; 98];
    set64(&mut item, 0, 1);
    set64(&mut item, 8, SIZE);
    put(&mut item, 66, &UUID);
    put(&mut item, 82, &FSID);
    item
}

fn chunk(kind: u64, physical: &[u64]) -> Vec<u8> {
    let mut item = vec![0; 48 + 32 * physical.len()];
    set64(&mut item, 0, MIB);
    set64(&mut item, 8, 2);
    set64(&mut item, 16, 65536);
    set64(&mut item, 24, kind);
    set32(&mut item, 40, 4096);
    put(&mut item, 44, &(physical.len() as u16).to_le_bytes());
    for (i, &at) in physical.iter().enumerate() {
        let start = 48 + i * 32;
        set64(&mut item, start, 1);
        set64(&mut item, start + 8, at);
        put(&mut item, start + 16, &UUID);
    }
    item
}

fn leaf(out: &mut [u8], logical: u64, owner: u64, items: &[(u64, u8, u64, Vec<u8>)]) {
    let mut end = out.len();
    put(out, 32, &FSID);
    set64(out, 48, logical);
    set64(out, 80, 10);
    set64(out, 88, owner);
    set32(out, 96, items.len() as u32);
    for (i, (object_id, kind, offset, bytes)) in items.iter().enumerate() {
        let header = HEADER + i * 25;
        end -= bytes.len();
        key(&mut out[header..], *object_id, *kind, *offset);
        set32(out, header + 17, (end - HEADER) as u32);
        set32(out, header + 21, bytes.len() as u32);
        put(out, end, bytes);
    }
    sign(out);
}

pub fn image() -> Vec<u8> {
    let mut image = vec![0; SIZE as usize];
    let start = SUPER_OFFSETS[0] as usize;
    let sb = &mut image[start..start + SUPER_BYTES];
    put(sb, 32, &FSID);
    set64(sb, 48, start as u64);
    put(sb, 64, b"_BHRfS_M");
    set64(sb, 0x48, 10);
    set64(sb, 0x50, 9 * MIB);
    set64(sb, 0x58, 8 * MIB);
    set64(sb, 0x70, SIZE);
    set64(sb, 0x88, 1);
    set32(sb, 0x90, 4096);
    set32(sb, 0x94, 4096);
    set64(sb, 0xa4, 10);
    put(sb, 0xc9, &device());
    let system = chunk(SYSTEM | DUP, &[MIB, 2 * MIB]);
    set32(sb, 0xa0, (KEY_BYTES + system.len()) as u32);
    key(&mut sb[SYSTEM_ARRAY..], 256, CHUNK_ITEM, 8 * MIB);
    put(sb, SYSTEM_ARRAY + KEY_BYTES, &system);
    sign(sb);

    for at in [MIB as usize, 2 * MIB as usize] {
        leaf(
            &mut image[at..at + 4096],
            8 * MIB,
            CHUNK_TREE,
            &[
                (1, DEVICE_ITEM, 1, device()),
                (256, CHUNK_ITEM, 8 * MIB, system.clone()),
                (256, CHUNK_ITEM, 9 * MIB, chunk(METADATA, &[3 * MIB])),
                (256, CHUNK_ITEM, 10 * MIB, chunk(1, &[4 * MIB])),
            ],
        );
    }
    let mut root = vec![0; 239];
    set64(&mut root, 160, 10);
    set64(&mut root, 168, 256);
    set64(&mut root, 176, 9 * MIB + 4096);
    set32(&mut root, 216, 1);
    let at = 3 * MIB as usize;
    let mut csum_root = root.clone();
    set64(&mut csum_root, 168, 0);
    set64(&mut csum_root, 176, 9 * MIB + 8192);
    let mut extent_root = csum_root.clone();
    set64(&mut extent_root, 176, 9 * MIB + 12288);
    let mut subvolume = root.clone();
    set64(&mut subvolume, 176, 9 * MIB + 16384);
    leaf(
        &mut image[at..at + 4096],
        9 * MIB,
        ROOT_TREE,
        &[
            (2, ROOT_ITEM, 0, extent_root),
            (5, ROOT_ITEM, 0, root),
            (7, ROOT_ITEM, 0, csum_root),
            (300, ROOT_ITEM, 0, subvolume),
        ],
    );
    let mut directory = vec![0; 160];
    set64(&mut directory, 0, 10);
    set32(&mut directory, 52, 0x41ed);
    set64(&mut directory, 136, 1_700_000_000);
    let name = b"test-\xff";
    let mut entry = vec![0; 30 + name.len()];
    key(&mut entry, 257, INODE_ITEM, 0);
    put(&mut entry, 27, &(name.len() as u16).to_le_bytes());
    entry[29] = 1;
    put(&mut entry, 30, name);
    let mut subvolume_entry = vec![0; 38];
    key(&mut subvolume_entry, 300, ROOT_ITEM, u64::MAX);
    put(&mut subvolume_entry, 27, &8u16.to_le_bytes());
    subvolume_entry[29] = 2;
    put(&mut subvolume_entry, 30, b"snapshot");
    let mut inode = vec![0; 160];
    set64(&mut inode, 0, 10);
    set64(&mut inode, 16, 4103);
    set32(&mut inode, 52, 0x81a4);
    let mut extent = vec![0; 53];
    set64(&mut extent, 0, 10);
    set64(&mut extent, 8, 4096);
    extent[20] = 1;
    set64(&mut extent, 21, 10 * MIB);
    set64(&mut extent, 29, 4096);
    set64(&mut extent, 45, 4096);
    leaf(
        &mut image[at + 4096..at + 8192],
        9 * MIB + 4096,
        5,
        &[
            (256, INODE_ITEM, 0, directory),
            (256, DIR_INDEX, 2, entry),
            (256, DIR_INDEX, 3, subvolume_entry),
            (257, INODE_ITEM, 0, inode),
            (257, 108, 0, extent),
        ],
    );
    let data = &mut image[4 * MIB as usize..4 * MIB as usize + 4096];
    data.fill(23);
    let checksum = crc32c(data).to_le_bytes();
    leaf(
        &mut image[at + 8192..at + 12288],
        9 * MIB + 8192,
        7,
        &[(u64::MAX - 9, 128, 10 * MIB, checksum.to_vec())],
    );
    let mut allocation = vec![0; 24];
    set64(&mut allocation, 0, 1);
    set64(&mut allocation, 8, 10);
    set64(&mut allocation, 16, 1);
    leaf(
        &mut image[at + 12288..at + 16384],
        9 * MIB + 12288,
        2,
        &[(10 * MIB, 168, 4096, allocation)],
    );
    let mut subvolume_directory = vec![0; 160];
    set64(&mut subvolume_directory, 0, 10);
    set32(&mut subvolume_directory, 52, 0x41c0);
    set64(&mut subvolume_directory, 136, 1_700_000_000);
    leaf(
        &mut image[at + 16384..at + 20480],
        9 * MIB + 16384,
        300,
        &[(256, INODE_ITEM, 0, subvolume_directory)],
    );
    image
}

pub fn allocation_image(separate_groups: bool, declared_data: u64) -> Vec<u8> {
    allocation_image_with_mirror(separate_groups, declared_data, false)
}

pub fn allocation_image_with_mirror(
    separate_groups: bool,
    declared_data: u64,
    with_mirror: bool,
) -> Vec<u8> {
    let mut image = image();
    let size = if with_mirror { 68 * MIB } else { SIZE };
    image.resize(size as usize, 0);
    let metadata = 3 * MIB as usize;
    let logical = 9 * MIB;
    let metadata_nodes = 6 + u64::from(separate_groups);
    let mut device = device();
    set64(&mut device, 8, size);
    set64(&mut device, 16, 4 * MIB);
    let superblock = &mut image[(64 * 1024)..][..4096];
    set64(superblock, 0x38, 1);
    set64(superblock, 0x70, size);
    set64(superblock, 0x78, (metadata_nodes + 2) * 4096);
    set64(superblock, 0xb4, if separate_groups { 8 } else { 0 });
    put(superblock, 0xc9, &device);
    sign(superblock);
    if with_mirror {
        let mut copy: [u8; 4096] = superblock.try_into().unwrap();
        set64(&mut copy, 0x30, 64 * MIB);
        sign(&mut copy);
        put(&mut image, (64 * MIB) as usize, &copy);
    }
    let chunk_uuid = [0x42; 16];
    for offset in [MIB, 2 * MIB] {
        let buffer = &mut image[offset as usize..][..4096];
        buffer.fill(0);
        leaf(
            buffer,
            8 * MIB,
            CHUNK_TREE,
            &[
                (1, DEVICE_ITEM, 1, device.clone()),
                (
                    256,
                    CHUNK_ITEM,
                    8 * MIB,
                    chunk(SYSTEM | DUP, &[MIB, 2 * MIB]),
                ),
                (256, CHUNK_ITEM, logical, chunk(METADATA, &[3 * MIB])),
                (256, CHUNK_ITEM, 10 * MIB, chunk(1, &[4 * MIB])),
            ],
        );
        put(buffer, 64, &chunk_uuid);
        sign(buffer);
    }
    let root_item = |address, directory| {
        let mut root = vec![0; 239];
        set64(&mut root, 160, 10);
        set64(&mut root, 168, directory);
        set64(&mut root, 176, address);
        set32(&mut root, 216, 1);
        root
    };
    let mut roots = vec![
        (2, ROOT_ITEM, 0, root_item(logical + 12288, 0)),
        (4, ROOT_ITEM, 0, root_item(logical + 20480, 0)),
        (5, ROOT_ITEM, 0, root_item(logical + 4096, 256)),
        (7, ROOT_ITEM, 0, root_item(logical + 8192, 0)),
        (300, ROOT_ITEM, 0, root_item(logical + 16384, 256)),
    ];
    if separate_groups {
        roots.insert(4, (11, ROOT_ITEM, 0, root_item(logical + 24576, 0)));
    }
    image[metadata..][..4096].fill(0);
    leaf(&mut image[metadata..][..4096], logical, ROOT_TREE, &roots);
    let device_items = [(1, 8), (2, 8), (3, 9), (4, 10)].map(|(physical, logical)| {
        let mut data = vec![0; 48];
        set64(&mut data, 0, CHUNK_TREE);
        set64(&mut data, 8, 256);
        set64(&mut data, 16, logical * MIB);
        set64(&mut data, 24, MIB);
        put(&mut data, 32, &chunk_uuid);
        (1, 204, physical * MIB, data)
    });
    leaf(
        &mut image[metadata + 20480..][..4096],
        logical + 20480,
        4,
        &device_items,
    );
    let group_items = [
        (8 * MIB, 4096, SYSTEM | DUP),
        (logical, metadata_nodes * 4096, METADATA),
        (10 * MIB, declared_data, 1),
    ]
    .map(|(logical, used, kind)| {
        let mut item = vec![0; 24];
        set64(&mut item, 0, used);
        set64(&mut item, 8, 256);
        set64(&mut item, 16, kind);
        (logical, 192, MIB, item)
    });
    let extent_item = |kind| {
        let mut item = vec![0; 24];
        set64(&mut item, 0, 1);
        set64(&mut item, 8, 10);
        set64(&mut item, 16, kind);
        item
    };
    let mut extents = vec![(8 * MIB, 169, 0, extent_item(2))];
    extents.extend((0..metadata_nodes).map(|i| (logical + i * 4096, 169, 0, extent_item(2))));
    extents.push((10 * MIB, 168, 4096, extent_item(1)));
    if separate_groups {
        leaf(
            &mut image[metadata + 24576..][..4096],
            logical + 24576,
            11,
            &group_items,
        );
    } else {
        extents.extend(group_items);
        extents.sort_by_key(|&(object, kind, offset, _)| (object, kind, offset));
    }
    image[metadata + 12288..][..4096].fill(0);
    leaf(
        &mut image[metadata + 12288..][..4096],
        logical + 12288,
        2,
        &extents,
    );
    image
}

pub fn snapshot_image(live_copy: bool) -> Vec<u8> {
    historical_image(live_copy, false)
}

pub fn retained_image(live_copy: bool) -> Vec<u8> {
    historical_image(live_copy, true)
}

fn historical_image(live_copy: bool, retained: bool) -> Vec<u8> {
    fn root(block: u64, generation: u64, directory: u64) -> Vec<u8> {
        let mut item = vec![0; 311];
        set64(&mut item, 160, generation);
        set64(&mut item, 168, directory);
        set64(&mut item, 176, block);
        set32(&mut item, 216, 1);
        set64(&mut item, 239, generation);
        item
    }
    fn inode(generation: u64, size: u64, mode: u32) -> Vec<u8> {
        let mut item = vec![0; 160];
        set64(&mut item, 0, generation);
        set64(&mut item, 16, size);
        set32(&mut item, 40, 1);
        set32(&mut item, 52, mode);
        item
    }
    fn link(inode: u64, kind: u8, file_kind: u8, name: &[u8]) -> Vec<u8> {
        let mut item = vec![0; 30 + name.len()];
        key(
            &mut item,
            inode,
            kind,
            if kind == ROOT_ITEM { u64::MAX } else { 0 },
        );
        put(&mut item, 27, &(name.len() as u16).to_le_bytes());
        item[29] = file_kind;
        put(&mut item, 30, name);
        item
    }
    let mut image = image();
    let at = 3 * MIB as usize;
    let logical = 9 * MIB;
    let mut origin = root(logical + 4096, 10, 256);
    put(&mut origin, 247, &[0x57; 16]);
    set64(&mut origin, 303, 2);
    let mut snapshot = root(logical + 16384, 8, 256);
    set64(&mut snapshot, 208, 1);
    put(&mut snapshot, 247, &[0x38; 16]);
    put(&mut snapshot, 263, &[0x57; 16]);
    set64(&mut snapshot, 303, 8);
    let roots = &mut image[at..at + 4096];
    roots.fill(0);
    let mut current_roots = vec![
        (2, ROOT_ITEM, 0, root(logical + 12288, 10, 0)),
        (5, ROOT_ITEM, 0, origin.clone()),
        (7, ROOT_ITEM, 0, root(logical + 8192, 10, 0)),
    ];
    if !retained {
        current_roots.push((300, ROOT_ITEM, 8, snapshot));
    }
    leaf(roots, logical, ROOT_TREE, &current_roots);

    let mut live = vec![
        (256, INODE_ITEM, 0, inode(2, 0, 0x41ed)),
        (256, DIR_INDEX, 2, link(257, INODE_ITEM, 1, b"renamed.txt")),
        (257, INODE_ITEM, 0, inode(6, 0, 0x81a4)),
    ];
    if !retained {
        live.insert(2, (256, DIR_INDEX, 3, link(300, ROOT_ITEM, 2, b"snapshot")));
    }
    if live_copy {
        live.insert(
            live.len() - 1,
            (256, DIR_INDEX, 4, link(258, INODE_ITEM, 1, b"restored.txt")),
        );
        live.push((258, INODE_ITEM, 0, inode(6, 4103, 0x81a4)));
    }
    let current = &mut image[at + 4096..at + 8192];
    current.fill(0);
    leaf(current, logical + 4096, 5, &live);

    let mut extent = vec![0; 53];
    set64(&mut extent, 0, 6);
    set64(&mut extent, 8, 4096);
    extent[20] = 1;
    set64(&mut extent, 21, 10 * MIB);
    set64(&mut extent, 29, 4096);
    set64(&mut extent, 45, 4096);
    let past = &mut image[at + 16384..at + 20480];
    past.fill(0);
    leaf(
        past,
        logical + 16384,
        if retained { 5 } else { 300 },
        &[
            (256, INODE_ITEM, 0, inode(2, 0, 0x41c0)),
            (256, DIR_INDEX, 2, link(257, INODE_ITEM, 1, b"old-name.txt")),
            (256, DIR_INDEX, 3, link(259, INODE_ITEM, 2, b"documents")),
            (257, INODE_ITEM, 0, inode(6, 0, 0x81a4)),
            (258, INODE_ITEM, 0, inode(6, 4103, 0x81a4)),
            (258, 108, 0, extent),
            (259, INODE_ITEM, 0, inode(5, 0, 0x41ed)),
            (259, DIR_INDEX, 2, link(258, INODE_ITEM, 1, b"deleted.txt")),
        ],
    );
    set64(past, 80, 8);
    sign(past);

    if retained {
        // An older transaction kept its own extent and checksum trees. The
        // current trees may be empty after unlinking the last directory link.
        let checksum = crc32c(&image[(4 * MIB) as usize..][..4096]);
        let checksum_leaf = &mut image[at + 20480..][..4096];
        leaf(
            checksum_leaf,
            logical + 20480,
            7,
            &[(u64::MAX - 9, 128, 10 * MIB, checksum.to_le_bytes().to_vec())],
        );
        set64(checksum_leaf, 80, 8);
        sign(checksum_leaf);
        let mut allocation = vec![0; 24];
        set64(&mut allocation, 0, 1);
        set64(&mut allocation, 8, 6);
        set64(&mut allocation, 16, 1);
        let extent_leaf = &mut image[at + 24576..][..4096];
        leaf(
            extent_leaf,
            logical + 24576,
            2,
            &[(10 * MIB, 168, 4096, allocation)],
        );
        set64(extent_leaf, 80, 8);
        sign(extent_leaf);
        for (offset, owner) in [(8192, 7), (12288, 2)] {
            let block = &mut image[at + offset..][..4096];
            block.fill(0);
            leaf(block, logical + offset as u64, owner, &[]);
        }
        set64(&mut origin, 160, 8);
        set64(&mut origin, 176, logical + 16384);
        set64(&mut origin, 239, 8);
        let roots = &mut image[at + 28672..][..4096];
        leaf(
            roots,
            logical + 28672,
            ROOT_TREE,
            &[
                (2, ROOT_ITEM, 0, root(logical + 24576, 8, 0)),
                (5, ROOT_ITEM, 0, origin),
                (7, ROOT_ITEM, 0, root(logical + 20480, 8, 0)),
            ],
        );
        set64(roots, 80, 8);
        sign(roots);
        for offset in [MIB as usize, 2 * MIB as usize] {
            let block = &mut image[offset..][..4096];
            set64(block, 80, 6);
            sign(block);
        }
        let sb = &mut image[SUPER_OFFSETS[0] as usize..][..4096];
        set64(sb, 0xa4, 6);
        let backup = &mut sb[SYSTEM_ARRAY + 2048..];
        for (slot, (tx, delta, extent, checksum)) in
            [(8, 28672, 24576, 20480), (10, 0, 12288, 8192)]
                .into_iter()
                .enumerate()
        {
            let entry = &mut backup[slot * 168..][..168];
            for (index, (pointer, generation)) in [
                (logical + delta, tx),
                (8 * MIB, 6),
                (logical + extent, tx),
                (0, 0),
                (0, 0),
                (logical + checksum, tx),
            ]
            .into_iter()
            .enumerate()
            {
                set64(entry, index * 16, pointer);
                set64(entry, index * 16 + 8, generation);
            }
            set64(entry, 96, SIZE);
            set64(entry, 112, 1);
        }
        sign(sb);
    }
    image
}
