//! Independent checks for the NTFS formatter subprocess. The entire target is
//! synthetic memory and every request is checked by the parent.
use sha2::{Digest, Sha256};
use std::{
    io::{self, Cursor, Read, Write},
    process::{Command, Stdio},
    sync::atomic::AtomicBool,
};

const SIZE: usize = 24 * 1024 * 1024;
const START: u64 = 84_992;
const SERIAL: u64 = 0x0939_44a2_1c2e_0203;

#[test]
#[ignore = "requires the separately built Windows NTFS formatter"]
fn efs_clone_preserves_opaque_keys_and_last_ciphertext_sector() {
    use crate::{
        ntfs_streams,
        partitions::merge::{self, metadata},
    };

    let engine = std::env::var("DISKSTUDIO_TEST_NTFS_ENGINE").unwrap();
    let descriptor = fixture_acl();
    let mut fixture = vec![0; 76];
    fixture[8..12].copy_from_slice(&2u32.to_le_bytes());
    fixture[64..68].copy_from_slice(&76u32.to_le_bytes());
    fixture.extend_from_slice(&1u32.to_le_bytes());
    for value in [304u32, 20, 256, 48, 0, 28, 0, 1, 0, 0, 0, 0] {
        fixture.extend_from_slice(&value.to_le_bytes());
    }
    fixture.extend(0..=255u8);
    let length = fixture.len() as u32;
    fixture[..4].copy_from_slice(&length.to_le_bytes());
    metadata::validate_efs(&fixture).unwrap();
    let mut invalid = fixture.clone();
    invalid[92..96].copy_from_slice(&20u32.to_le_bytes());
    assert!(metadata::validate_efs(&invalid).is_err());
    let contents = [("", 4096u64), ("empty", 0), ("tail", 533)].map(|(name, size)| {
        let payload = (0..ntfs_streams::payload_size(size, ntfs_streams::ENCRYPTED))
            .map(|index| (index % 251 + 1) as u8)
            .collect::<Vec<_>>();
        (name, size, payload)
    });
    let mut plan = 5u32.to_le_bytes().to_vec();
    record(&mut plan, 1, 0, 0, "encrypted-directory", 0);
    record(&mut plan, 2, 1, 0, "encrypted.bin", 3);
    for (name, logical, payload) in &contents {
        plan.extend_from_slice(&(name.len() as u16).to_le_bytes());
        plan.extend_from_slice(&ntfs_streams::ENCRYPTED.to_le_bytes());
        plan.extend_from_slice(&logical.to_le_bytes());
        plan.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
        plan.extend_from_slice(payload);
    }
    record(&mut plan, 3, 0, 2, "encrypted-alias.bin", 0);
    for (id, flags, keys) in [(1, 0x4000u32, false), (2, 0x4020, true)] {
        record(&mut plan, 4, 0, id, "", 0);
        plan.extend_from_slice(&flags.to_le_bytes());
        plan.extend_from_slice(&(descriptor.len() as u32).to_le_bytes());
        plan.extend([0; 29]);
        plan.push(if keys { 4 } else { 0 });
        plan.extend([0; 4]);
        if keys {
            plan.extend_from_slice(&length.to_le_bytes());
        }
        plan.extend_from_slice(&descriptor);
        if keys {
            plan.extend_from_slice(&fixture);
        }
    }

    let cancel = AtomicBool::new(false);
    let tool = crate::ntfs_file_clone::Plan::engine().unwrap();
    for sector in [512, 4096] {
        let mut image = vec![0; SIZE];
        assert!(serve(&engine, sector, &mut image, &plan, false).0);
        let mut source = Cursor::new(image);
        let catalogue = merge::catalogue_file_clone(&mut source, &cancel).unwrap();
        assert!(merge::catalogue(&mut source, None, &cancel).is_err());
        let file = catalogue.names[&path(&["encrypted-directory", "encrypted.bin"])];
        assert_eq!(file, catalogue.names[&path(&["encrypted-alias.bin"])]);
        assert_eq!(catalogue.nodes[&file].metadata[&metadata::EFS], fixture);
        let range = crate::surface_scan::Range {
            offset: u64::from(sector) * (START + 100),
            size: 16 * 1024 * 1024,
            sector: u64::from(sector),
        };
        let clone =
            crate::ntfs_file_clone::Plan::prepare(&mut source, range, SERIAL + 1, &cancel).unwrap();
        let mut target = Cursor::new(vec![0xd4; range.size as usize]);
        clone
            .write(
                &tool,
                &mut source,
                &mut target,
                Write::flush,
                &cancel,
                |_, _, _| Ok(()),
            )
            .unwrap();
        clone.verify(&mut target, &cancel).unwrap();
        let cloned = merge::catalogue_file_clone(&mut target, &cancel).unwrap();
        let id = cloned.names[&path(&["encrypted-alias.bin"])];
        assert_eq!(cloned.nodes[&id].metadata[&metadata::EFS], fixture);
        assert_eq!(cloned.nodes[&id].streams.len(), contents.len());
        for (stream, (name, size, expected)) in cloned.nodes[&id].streams.iter().zip(&contents) {
            assert_eq!(stream.size, *size);
            assert_eq!(stream.storage_flags, ntfs_streams::ENCRYPTED);
            let raw_name = name
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            let mut actual = Vec::new();
            cloned
                .transfer_stream(&mut target, id, &raw_name, &mut actual, &cancel)
                .unwrap();
            assert_eq!(&actual, expected);
        }
        let ntfs = ntfs::Ntfs::new(&mut target).unwrap();
        let record = ntfs.file(&mut target, id.0).unwrap();
        let recovery = ntfs_streams::enumerate(&mut target, &record).unwrap();
        assert_eq!(recovery.len(), 2);
        assert!(recovery.iter().all(|stream| stream.reason.is_some()));
    }
}

#[test]
#[ignore = "requires the separately built Windows NTFS formatter"]
fn wof_file_backing_survives_clone_with_a_full_valid_data_length() {
    use crate::partitions::merge::{self, metadata};

    let engine = std::env::var("DISKSTUDIO_TEST_NTFS_ENGINE").unwrap();
    let corpus = std::env::var_os("DISKSTUDIO_TEST_WOF_CORPUS").map(|directory| {
        let vm = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.vm")
            .canonicalize()
            .unwrap();
        let directory = std::path::PathBuf::from(directory).canonicalize().unwrap();
        assert!(directory != vm && directory.starts_with(vm));
        directory
    });
    let descriptor = fixture_acl();
    let mut plan = 8u32.to_le_bytes().to_vec();
    for (algorithm, chunk) in [4096u32, 32768, 8192, 16384].into_iter().enumerate() {
        let name = format!("wof-{algorithm}.bin");
        let (size, backing) = if let Some(directory) = &corpus {
            (
                192 * 1024 + 123,
                std::fs::read(directory.join(&name)).unwrap(),
            )
        } else {
            let size = chunk + 59;
            let mut backing = chunk.to_le_bytes().to_vec();
            backing.extend((0..size).map(|index| (index % 97) as u8));
            (size, backing)
        };
        record(&mut plan, 2, 0, 0, &name, 3);
        sparse_stream(&mut plan, "", size.into(), &[]);
        stream(&mut plan, "WofCompressedData", &backing);
        stream(&mut plan, "comment", b"ordinary alternate stream");
        let reparse = [metadata::WOF_TAG, 20, 1, 2, 1, algorithm as u32, 0]
            .map(u32::to_le_bytes)
            .concat();
        record(&mut plan, 4, 0, 1 + 2 * algorithm as u32, "", 0);
        plan.extend_from_slice(&0x420u32.to_le_bytes());
        plan.extend_from_slice(&(descriptor.len() as u32).to_le_bytes());
        plan.extend([0; 30]);
        plan.extend_from_slice(&(reparse.len() as u32).to_le_bytes());
        plan.extend_from_slice(&descriptor);
        plan.extend_from_slice(&reparse);
    }
    let cancel = AtomicBool::new(false);
    let tool = crate::ntfs_file_clone::Plan::engine().unwrap();
    for sector in [512, 4096] {
        let size = if sector == 4096 && corpus.is_some() {
            96 * 1024 * 1024
        } else {
            SIZE
        };
        let mut image = vec![0; size];
        assert!(serve(&engine, sector, &mut image, &plan, false).0);
        let mut source = Cursor::new(image);
        let catalogue = merge::catalogue_file_clone(&mut source, &cancel).unwrap();
        assert!(merge::catalogue(&mut source, None, &cancel).is_err());
        for algorithm in 0..4 {
            let id = catalogue.names[&path(&[&format!("wof-{algorithm}.bin")])];
            let node = &catalogue.nodes[&id];
            assert_eq!(node.streams.len(), 3);
            assert!(node.streams[0].allocated_ranges.is_empty());
        }
        let range = crate::surface_scan::Range {
            offset: u64::from(sector) * (START + 100),
            size: 16 * 1024 * 1024,
            sector: u64::from(sector),
        };
        let clone =
            crate::ntfs_file_clone::Plan::prepare(&mut source, range, SERIAL + 1, &cancel).unwrap();
        let mut target = Cursor::new(vec![0xd4; range.size as usize]);
        clone
            .write(
                &tool,
                &mut source,
                &mut target,
                Write::flush,
                &cancel,
                |_, _, _| Ok(()),
            )
            .unwrap();
        clone.verify(&mut target, &cancel).unwrap();
        if let Some(directory) = &corpus {
            if sector == 4096 {
                let mut image =
                    std::fs::File::create_new(directory.join("Ntfs-Wof-Source.raw")).unwrap();
                image.write_all(source.get_ref()).unwrap();
                image.sync_all().unwrap();
            }
            let restored = merge::catalogue_file_clone(&mut target, &cancel).unwrap();
            for algorithm in 0..4 {
                let name = format!("wof-{algorithm}.bin");
                let id = restored.names[&path(&[&name])];
                let destination = directory.join(format!("clone-{sector}-{algorithm}.bin"));
                let mut output = std::fs::File::create_new(destination).unwrap();
                restored
                    .transfer_stream(&mut target, id, metadata::WOF_STREAM, &mut output, &cancel)
                    .unwrap();
            }
        }
    }
}

fn header(sector: u32, size: usize) -> Vec<u8> {
    [
        &b"DSN6"[..],
        &sector.to_le_bytes(),
        &(size as u64).to_le_bytes(),
        &START.to_le_bytes(),
        &SERIAL.to_le_bytes(),
        &4096u32.to_le_bytes(),
    ]
    .concat()
}

fn serve(
    executable: &str,
    sector: u32,
    image: &mut [u8],
    plan: &[u8],
    reject_write: bool,
) -> (bool, usize, usize) {
    let mut child = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut commands = child.stdin.take().unwrap();
    let mut requests = child.stdout.take().unwrap();
    commands.write_all(&header(sector, image.len())).unwrap();
    let mut plan = Cursor::new(plan);
    let mut command_bytes = vec![0; 128 * 1024];
    let mut writes = 0;
    let mut flushes = 0;
    let mut request = [0u8; 13];
    loop {
        match requests.read(&mut request[..1]) {
            Ok(0) => break,
            Ok(1) => (),
            result => panic!("Cannot read engine request: {result:?}"),
        }
        requests.read_exact(&mut request[1..]).unwrap();
        let at = usize::try_from(u64::from_le_bytes(request[1..9].try_into().unwrap())).unwrap();
        let size = u32::from_le_bytes(request[9..].try_into().unwrap()) as usize;
        if request[0] == 3 {
            assert_eq!((at, size), (0, 0));
            flushes += 1;
            commands.write_all(&[0]).unwrap();
            continue;
        }
        assert!((1..=1024 * 1024).contains(&size));
        if request[0] == 4 {
            assert_eq!(at, 0);
            let command = &mut command_bytes[..size];
            plan.read_exact(command).unwrap();
            commands.write_all(&[0]).unwrap();
            commands.write_all(command).unwrap();
            continue;
        }
        let range = &mut image[at..at.checked_add(size).unwrap()];
        match request[0] {
            1 => {
                commands.write_all(&[0]).unwrap();
                commands.write_all(range).unwrap();
            }
            2 => {
                writes += 1;
                if reject_write {
                    io::copy(&mut (&mut requests).take(size as u64), &mut io::sink()).unwrap();
                    commands.write_all(&[1]).unwrap();
                } else {
                    requests.read_exact(range).unwrap();
                    commands.write_all(&[0]).unwrap();
                }
            }
            other => panic!("Unknown engine request {other}"),
        }
    }
    drop(commands);
    let status = child.wait().unwrap();
    if status.success() {
        assert_eq!(plan.position(), plan.get_ref().len() as u64);
    }
    (status.success(), writes, flushes)
}

#[test]
#[ignore = "requires the separately built Windows NTFS formatter"]
fn bounded_format_and_readback() {
    let engine = std::env::var("DISKSTUDIO_TEST_NTFS_ENGINE").unwrap();
    for sector in [512, 4096] {
        let mut image = vec![0; SIZE];
        let (ok, writes, flushes) = serve(&engine, sector, &mut image, &[0; 4], false);
        assert!(ok && writes > 0 && flushes > 0);
        let mut input = Cursor::new(image.as_slice());
        let fs = ntfs::Ntfs::new(&mut input).unwrap();
        assert_eq!(u32::from(fs.sector_size()), sector);
        assert_eq!(fs.cluster_size(), 4096);
        assert_eq!(fs.size() + u64::from(sector), SIZE as u64);
        assert_eq!(fs.serial_number(), SERIAL);
        assert_eq!(
            u32::from_le_bytes(image[28..32].try_into().unwrap()) as u64,
            START
        );
        assert_eq!(
            fs.volume_name(&mut input)
                .unwrap()
                .unwrap()
                .name()
                .to_string_lossy(),
            "DISKCLONE",
        );
        crate::ntfs_allocation::inspect(&mut input, &AtomicBool::new(false)).unwrap();
        assert!(fs.root_directory(&mut input).unwrap().is_directory());
    }
}

fn record(plan: &mut Vec<u8>, operation: u8, parent: u32, source: u32, name: &str, streams: u16) {
    let units = name.encode_utf16().collect::<Vec<_>>();
    plan.extend_from_slice(&[operation, units.len() as u8]);
    plan.extend_from_slice(&streams.to_le_bytes());
    plan.extend_from_slice(&parent.to_le_bytes());
    plan.extend_from_slice(&source.to_le_bytes());
    plan.extend(units.into_iter().flat_map(u16::to_le_bytes));
}

fn stream(plan: &mut Vec<u8>, name: &str, contents: &[u8]) {
    stream_with_flags(plan, name, contents, 0);
}

fn stream_with_flags(plan: &mut Vec<u8>, name: &str, contents: &[u8], flags: u16) {
    let units = name.encode_utf16().collect::<Vec<_>>();
    plan.extend_from_slice(&(units.len() as u16).to_le_bytes());
    plan.extend_from_slice(&flags.to_le_bytes());
    plan.extend_from_slice(&(contents.len() as u64).to_le_bytes());
    plan.extend(units.into_iter().flat_map(u16::to_le_bytes));
    plan.extend_from_slice(contents);
}

fn sparse_stream(plan: &mut Vec<u8>, name: &str, size: u64, segments: &[(u64, &[u8])]) {
    let units = name.encode_utf16().collect::<Vec<_>>();
    plan.extend_from_slice(&(units.len() as u16).to_le_bytes());
    plan.extend_from_slice(&crate::ntfs_streams::SPARSE.to_le_bytes());
    plan.extend_from_slice(&size.to_le_bytes());
    plan.extend(units.into_iter().flat_map(u16::to_le_bytes));
    plan.extend_from_slice(&(segments.len() as u32).to_le_bytes());
    for &(start, data) in segments {
        plan.extend_from_slice(&start.to_le_bytes());
        plan.extend_from_slice(&(data.len() as u64).to_le_bytes());
        plan.extend_from_slice(data);
    }
}

fn path(parts: &[&str]) -> Vec<Vec<u16>> {
    parts
        .iter()
        .map(|part| part.encode_utf16().collect())
        .collect()
}

fn fixture_acl() -> [u8; 52] {
    let mut descriptor = [0u8; 52];
    descriptor[0] = 1;
    descriptor[2..4].copy_from_slice(&0x9004u16.to_le_bytes());
    descriptor[4..8].copy_from_slice(&20u32.to_le_bytes());
    descriptor[8..12].copy_from_slice(&32u32.to_le_bytes());
    descriptor[16..20].copy_from_slice(&44u32.to_le_bytes());
    descriptor[20..32].copy_from_slice(&[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]);
    descriptor[32..44].copy_from_slice(&[1, 1, 0, 0, 0, 0, 0, 5, 19, 0, 0, 0]);
    descriptor[44..].copy_from_slice(&[2, 0, 8, 0, 0, 0, 0, 0]);
    descriptor
}

fn ea_records(values: &[(&[u8], &[u8], u8)]) -> ([u8; 8], Vec<u8>) {
    let mut data = Vec::new();
    let mut packed = 0u16;
    let mut required = 0u16;
    for (index, &(name, value, flags)) in values.iter().enumerate() {
        let len = 9 + name.len() + value.len();
        let last = index + 1 == values.len();
        let span = if last { len } else { len.next_multiple_of(4) };
        let next = if last { 0 } else { span as u32 };
        data.extend_from_slice(&next.to_le_bytes());
        data.extend_from_slice(&[flags, name.len() as u8]);
        data.extend_from_slice(&(value.len() as u16).to_le_bytes());
        data.extend_from_slice(name);
        data.push(0);
        data.extend_from_slice(value);
        data.resize(data.len() + span - len, 0);
        packed += (len - 4) as u16;
        required += u16::from(flags != 0);
    }
    let mut info = [0; 8];
    info[..2].copy_from_slice(&packed.to_le_bytes());
    info[2..4].copy_from_slice(&required.to_le_bytes());
    info[4..].copy_from_slice(&(data.len() as u32).to_le_bytes());
    (info, data)
}

fn check_ea_name_cache(bytes: &[u8], name: &[Vec<u16>], info: &[u8; 8], links: usize) {
    let mut reader = Cursor::new(bytes);
    let cancel = AtomicBool::new(false);
    let catalogue = crate::partitions::merge::catalogue(&mut reader, None, &cancel).unwrap();
    let id = catalogue.names[name];
    let fs = ntfs::Ntfs::new(&mut reader).unwrap();
    let file = fs.file(&mut reader, id.0).unwrap();
    let mut attrs = file.attributes();
    let mut seen = 0;
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.unwrap();
        let attr = item.to_attribute().unwrap();
        if attr.ty().unwrap() == ntfs::NtfsAttributeType::FileName {
            let mut header = [0; 64];
            attr.value(&mut reader)
                .unwrap()
                .attach(&mut reader)
                .read_exact(&mut header)
                .unwrap();
            assert_eq!(&header[60..64], &[info[0], info[1], 0, 0]);
            seen += 1;
        }
    }
    assert_eq!(seen, links);
}

#[test]
#[ignore = "requires the separately built Windows NTFS writer"]
fn streams_links_short_names_permissions_and_eas_survive_a_smaller_clone() {
    let engine = std::env::var("DISKSTUDIO_TEST_NTFS_ENGINE").unwrap();
    let contents = (0..65553).map(|n| (n % 253) as u8).collect::<Vec<_>>();
    let note = b"named stream contents";
    let times = [
        132_100_000_000_000_001u64,
        132_200_000_000_000_002,
        132_300_000_000_000_003,
    ];
    let descriptor = fixture_acl();
    let attributes = 0x0004_0021u32;
    let directory_attributes = 0x1000_0000u32;
    let large_ea = vec![0xa5; 4199];
    let file_ea = ea_records(&[(b"AUDIT", b"first", 0x80), (b"BINARY", &large_ea, 0)]);
    let directory_ea = ea_records(&[(b"EMPTY", b"", 0)]);
    let mut plan = 9u32.to_le_bytes().to_vec();
    record(&mut plan, 1, 0, 0, "档案", 0);
    record(&mut plan, 2, 1, 0, "样本.bin", 2);
    stream(&mut plan, "", &contents);
    stream(&mut plan, "note", note);
    record(&mut plan, 3, 0, 2, "副本.bin", 0);
    record(&mut plan, 1, 1, 0, "空目录", 0);
    record(&mut plan, 6, 1, 2, "样本~1.BIN", 0);
    record(&mut plan, 6, 0, 2, "COPY~1.BIN", 0);
    record(&mut plan, 6, 1, 4, "EMPTY~1", 0);
    for (record_id, attributes, ea) in [
        (2, attributes, &file_ea),
        (4, directory_attributes, &directory_ea),
    ] {
        record(&mut plan, 4, 0, record_id, "", 0);
        plan.extend_from_slice(&attributes.to_le_bytes());
        plan.extend_from_slice(&(descriptor.len() as u32).to_le_bytes());
        plan.extend(times.into_iter().flat_map(u64::to_le_bytes));
        plan.extend_from_slice(&(ea.1.len() as u32).to_le_bytes());
        plan.extend([0; 6]);
        plan.extend_from_slice(&descriptor);
        plan.extend_from_slice(&ea.0);
        plan.extend_from_slice(&ea.1);
    }
    for sector in [512, 4096] {
        let mut image = vec![0; SIZE];
        assert!(serve(&engine, sector, &mut image, &plan, false).0);
        let catalogue = crate::partitions::merge::catalogue(
            &mut Cursor::new(image.as_slice()),
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        let file = catalogue.names[&path(&["档案", "样本.bin"])];
        assert_eq!(catalogue.names[&path(&["副本.bin"])], file);
        assert_eq!(catalogue.short_names[&path(&["档案", "样本~1.BIN"])], file);
        assert_eq!(catalogue.short_names[&path(&["COPY~1.BIN"])], file);
        let directory_id = catalogue.names[&path(&["档案", "空目录"])];
        assert_eq!(
            catalogue.short_names[&path(&["档案", "EMPTY~1"])],
            directory_id
        );
        let directory = &catalogue.nodes[&directory_id];
        assert!(directory.directory);
        assert_eq!(directory.attributes, directory_attributes);
        let node = &catalogue.nodes[&file];
        assert_eq!(node.times, times);
        assert_eq!(node.attributes, attributes);
        assert_eq!(catalogue.descriptor(file).unwrap(), descriptor);
        for (node, ea) in [(node, &file_ea), (directory, &directory_ea)] {
            assert_eq!(
                crate::partitions::merge::metadata::extended_attributes(node).unwrap(),
                Some((ea.0.as_slice(), ea.1.as_slice()))
            );
        }
        let streams = &node.streams;
        assert_eq!(streams.len(), 2);
        for (stream, name, expected) in [
            (&streams[0], "", &contents[..]),
            (&streams[1], "note", &note[..]),
        ] {
            assert_eq!(
                stream.locator.name,
                name.encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
            );
            assert_eq!(stream.size, expected.len() as u64);
            assert_eq!(stream.sha256, <[u8; 32]>::from(Sha256::digest(expected)));
        }
        let range = crate::surface_scan::Range {
            offset: u64::from(sector) * (START + 100),
            size: 16 * 1024 * 1024,
            sector: u64::from(sector),
        };
        let cancel = AtomicBool::new(false);
        let mut source = Cursor::new(image.as_slice());
        let clone =
            crate::ntfs_file_clone::Plan::prepare(&mut source, range, SERIAL + 1, &cancel).unwrap();
        clone.revalidate(&mut source, &cancel).unwrap();
        let mut target = Cursor::new(vec![0xd4; range.size as usize]);
        let tool = crate::ntfs_file_clone::Plan::engine().unwrap();
        let digest = clone
            .write(
                &tool,
                &mut source,
                &mut target,
                Write::flush,
                &cancel,
                |_, _, _| Ok(()),
            )
            .unwrap();
        clone.verify(&mut target, &cancel).unwrap();
        check_ea_name_cache(
            target.get_ref(),
            &path(&["档案", "样本.bin"]),
            &file_ea.0,
            4,
        );
        check_ea_name_cache(
            target.get_ref(),
            &path(&["档案", "空目录"]),
            &directory_ea.0,
            2,
        );
        assert_eq!(digest, format!("{:x}", Sha256::digest(target.into_inner())));
    }
}

#[test]
#[ignore = "requires the separately built Windows NTFS formatter"]
fn sparse_streams_keep_holes_and_allocated_zeroes_on_smaller_targets() {
    let engine = std::env::var("DISKSTUDIO_TEST_NTFS_ENGINE").unwrap();
    let size = 25 * 1024 * 1024 + 113;
    let allocated_zeroes = [0; 4096];
    let tail = [0x5a; 113];
    let mut plan = 1u32.to_le_bytes().to_vec();
    record(&mut plan, 2, 0, 0, "sparse.bin", 4);
    sparse_stream(
        &mut plan,
        "",
        size,
        &[(4096, &allocated_zeroes), (size - 113, &tail)],
    );
    sparse_stream(&mut plan, "empty-holes", 8192, &[]);
    sparse_stream(&mut plan, "solid-zeroes", 4096, &[(0, &allocated_zeroes)]);
    stream(&mut plan, "text", b"plain");
    for sector in [512, 4096] {
        let mut image = vec![0; SIZE];
        assert!(serve(&engine, sector, &mut image, &plan, false).0);
        let mut source = Cursor::new(image.as_slice());
        let cancel = AtomicBool::new(false);
        let catalogue = crate::partitions::merge::catalogue(&mut source, None, &cancel).unwrap();
        let id = catalogue.names[&path(&["sparse.bin"])];
        let checks = [
            (vec![4096..8192, (size - 113)..size], size, &[0x5a; 113][..]),
            (vec![], 8192, &[][..]),
            (Vec::from([0..4096]), 4096, &[][..]),
        ];
        for (stream, (ranges, size, suffix)) in catalogue.nodes[&id].streams.iter().zip(checks) {
            assert_eq!(stream.storage_flags, crate::ntfs_streams::SPARSE);
            assert_eq!(stream.allocated_ranges, ranges);
            assert_eq!(stream.size, size);
            let mut expected = vec![0; size as usize];
            expected[size as usize - suffix.len()..].copy_from_slice(suffix);
            assert_eq!(stream.sha256, <[u8; 32]>::from(Sha256::digest(expected)));
        }
        let range = crate::surface_scan::Range {
            offset: u64::from(sector) * (START + 100),
            size: 16 * 1024 * 1024,
            sector: u64::from(sector),
        };
        let clone =
            crate::ntfs_file_clone::Plan::prepare(&mut source, range, SERIAL + 1, &cancel).unwrap();
        let mut target = Cursor::new(vec![0xd4; range.size as usize]);
        let tool = crate::ntfs_file_clone::Plan::engine().unwrap();
        let digest = clone
            .write(
                &tool,
                &mut source,
                &mut target,
                Write::flush,
                &cancel,
                |_, _, _| Ok(()),
            )
            .unwrap();
        clone.verify(&mut target, &cancel).unwrap();
        let copy = crate::partitions::merge::catalogue(&mut target, None, &cancel).unwrap();
        let file_id = copy.names[&path(&["sparse.bin"])];
        let fs = ntfs::Ntfs::new(&mut target).unwrap();
        let file = fs.file(&mut target, file_id.0).unwrap();
        let readable = crate::ntfs_streams::enumerate(&mut target, &file).unwrap();
        assert_eq!(readable.len(), 3);
        assert!(readable.iter().all(|stream| stream.reason.is_none()));
        let holes = "empty-holes"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let stream = readable
            .iter()
            .find(|stream| stream.identity.name == holes)
            .unwrap();
        let mut zeros = Vec::new();
        let length = crate::ntfs_streams::extract(
            &mut target,
            &file,
            &stream.identity,
            &mut zeros,
            u64::MAX,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(length, 8192);
        assert_eq!(zeros, vec![0; 8192]);
        assert_eq!(digest, format!("{:x}", Sha256::digest(target.into_inner())));
    }
}

#[test]
#[ignore = "requires the separately built Windows NTFS formatter"]
fn rejected_geometry_or_write_is_terminal() {
    let engine = std::env::var("DISKSTUDIO_TEST_NTFS_ENGINE").unwrap();
    let mut image = vec![0; SIZE];
    assert_eq!(
        serve(&engine, 1024, &mut image, &[0; 4], false),
        (false, 0, 0)
    );
    assert_eq!(
        serve(&engine, 512, &mut image, &[0; 4], true),
        (false, 1, 0)
    );
    assert!(image.iter().all(|&byte| byte == 0));
}

#[test]
#[ignore = "requires the separately built Windows NTFS formatter"]
fn lznt1_streams_and_directory_compression_survive_a_smaller_clone() {
    let engine = std::env::var("DISKSTUDIO_TEST_NTFS_ENGINE").unwrap();
    let flags = crate::ntfs_streams::LZNT1;
    let mut mixed = vec![0x31; 64 * 1024];
    let mut state = 0xf74c8ee2u32;
    for _ in 0..64 * 1024 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        mixed.push(state as u8);
    }
    mixed.resize(3 * 64 * 1024, 0);
    mixed.extend(std::iter::repeat_n(0xa1, 507));
    let mut plan = 4u32.to_le_bytes().to_vec();
    record(&mut plan, 1, 0, 0, "compressed", 0);
    record(&mut plan, 2, 1, 0, "mixed.bin", 4);
    stream_with_flags(&mut plan, "", &mixed, flags);
    stream_with_flags(&mut plan, "empty", &[], flags);
    stream_with_flags(&mut plan, "small", b"repeated repeated repeated", flags);
    stream(&mut plan, "text", b"plain");
    let descriptor = fixture_acl();
    for (id, compressed_index) in [(1, 1), (2, 0)] {
        record(&mut plan, 4, 0, id, "", 0);
        plan.extend_from_slice(&0x800u32.to_le_bytes());
        plan.extend_from_slice(&(descriptor.len() as u32).to_le_bytes());
        plan.extend([0; 28]);
        plan.extend([compressed_index, 0, 0, 0, 0, 0]);
        plan.extend_from_slice(&descriptor);
    }
    for sector in [512, 4096] {
        let mut image = vec![0; SIZE];
        assert!(serve(&engine, sector, &mut image, &plan, false).0);
        let mut source = Cursor::new(image.as_slice());
        let cancel = AtomicBool::new(false);
        let catalogue = crate::partitions::merge::catalogue(&mut source, None, &cancel).unwrap();
        let folder = catalogue.names[&path(&["compressed"])];
        assert!(catalogue.nodes[&folder].index_compressed);
        let id = catalogue.names[&path(&["compressed", "mixed.bin"])];
        let node = &catalogue.nodes[&id];
        assert_eq!(node.attributes, 0x800);
        for (stream, (flags, expected)) in node.streams.iter().zip([
            (flags, mixed.as_slice()),
            (flags, &[][..]),
            (flags, &b"repeated repeated repeated"[..]),
            (0, &b"plain"[..]),
        ]) {
            assert_eq!(stream.storage_flags, flags);
            assert_eq!(stream.size, expected.len() as u64);
            assert_eq!(stream.sha256, <[u8; 32]>::from(Sha256::digest(expected)));
        }
        let range = crate::surface_scan::Range {
            offset: u64::from(sector) * (START + 100),
            size: 16 * 1024 * 1024,
            sector: u64::from(sector),
        };
        let clone =
            crate::ntfs_file_clone::Plan::prepare(&mut source, range, SERIAL + 1, &cancel).unwrap();
        let mut target = Cursor::new(vec![0xd4; range.size as usize]);
        let tool = crate::ntfs_file_clone::Plan::engine().unwrap();
        let digest = clone
            .write(
                &tool,
                &mut source,
                &mut target,
                Write::flush,
                &cancel,
                |_, _, _| Ok(()),
            )
            .unwrap();
        clone.verify(&mut target, &cancel).unwrap();
        assert_eq!(digest, format!("{:x}", Sha256::digest(target.into_inner())));
    }
}

#[test]
#[ignore = "requires the separately built Windows NTFS formatter"]
fn object_ids_and_reparse_indexes_keep_exact_metadata_and_new_references() {
    use crate::partitions::merge::{self, metadata};

    let engine = std::env::var("DISKSTUDIO_TEST_NTFS_ENGINE").unwrap();
    let object_id = std::array::from_fn::<_, 64, _>(|index| (index * 43 + 19) as u8);
    let file_link = link_data(
        0xa000000c,
        Some(1),
        "unresolved-target",
        "unresolved-target",
    );
    let junction = link_data(0xa0000003, None, r"\??\S:\directory", r"S:\directory");
    let descriptor = fixture_acl();
    let more_ids = (128..192u8)
        .map(|salt| {
            std::array::from_fn::<_, 64, _>(|index| {
                (index as u8).wrapping_mul(43).wrapping_add(salt)
            })
        })
        .collect::<Vec<_>>();
    let mut plan = (8 + 2 * more_ids.len() as u32).to_le_bytes().to_vec();
    record(&mut plan, 2, 0, 0, "original.bin", 1);
    stream(&mut plan, "", b"data with object identity");
    record(&mut plan, 1, 0, 0, "junction", 0);
    record(&mut plan, 2, 0, 0, "link.bin", 2);
    stream(&mut plan, "", &[]);
    stream(&mut plan, "comment", b"link-local data");
    record(&mut plan, 3, 0, 1, "ordinary-link.bin", 0);
    record(&mut plan, 3, 0, 3, "alternate-link.bin", 0);
    for (id, flags, identity, reparse) in [
        (1, 0x20u32, Some(&object_id), &[][..]),
        (2, 0x400, None, junction.as_slice()),
        (3, 0x400, None, file_link.as_slice()),
    ] {
        record(&mut plan, 4, 0, id, "", 0);
        plan.extend_from_slice(&flags.to_le_bytes());
        plan.extend_from_slice(&(descriptor.len() as u32).to_le_bytes());
        plan.extend([0; 29]);
        plan.push(u8::from(identity.is_some()) | (u8::from(!reparse.is_empty()) << 1));
        plan.extend_from_slice(&(reparse.len() as u32).to_le_bytes());
        plan.extend_from_slice(&descriptor);
        if let Some(identity) = identity {
            plan.extend_from_slice(identity);
        }
        plan.extend_from_slice(reparse);
    }
    for (index, identity) in more_ids.iter().enumerate() {
        record(&mut plan, 2, 0, 0, &format!("indexed-{index:02}.bin"), 0);
        record(&mut plan, 4, 0, 9 + 2 * index as u32, "", 0);
        plan.extend_from_slice(&0x20u32.to_le_bytes());
        plan.extend_from_slice(&(descriptor.len() as u32).to_le_bytes());
        plan.extend([0; 29]);
        plan.extend([1, 0, 0, 0, 0]);
        plan.extend_from_slice(&descriptor);
        plan.extend_from_slice(identity);
    }
    for sector in [512, 4096] {
        let mut image = vec![0; SIZE];
        assert!(serve(&engine, sector, &mut image, &plan, false).0);
        let mut source = Cursor::new(image.as_slice());
        let cancel = AtomicBool::new(false);
        let source_nodes = merge::catalogue_file_clone(&mut source, &cancel).unwrap();
        let fs = ntfs::Ntfs::new(&mut source).unwrap();
        let index = fs.file(&mut source, 25).unwrap();
        let mut attrs = index.attributes();
        let mut has_allocation = false;
        while let Some(attribute) = attrs.next(&mut source) {
            has_allocation |= attribute.unwrap().to_attribute().unwrap().ty().unwrap()
                == ntfs::NtfsAttributeType::IndexAllocation;
        }
        assert!(has_allocation);
        assert!(merge::catalogue(&mut source, None, &cancel).is_err());
        for (name, ty, expected) in [
            ("original.bin", metadata::OBJECT_ID, object_id.as_slice()),
            ("junction", metadata::REPARSE, junction.as_slice()),
            ("link.bin", metadata::REPARSE, file_link.as_slice()),
        ] {
            let id = source_nodes.names[&path(&[name])];
            assert_eq!(source_nodes.nodes[&id].metadata[&ty], expected);
            assert_eq!(
                source_nodes.nodes[&id].link_version_flag,
                ty == metadata::REPARSE
            );
        }
        assert_eq!(
            source_nodes.names[&path(&["link.bin"])],
            source_nodes.names[&path(&["alternate-link.bin"])]
        );
        let range = crate::surface_scan::Range {
            offset: u64::from(sector) * (START + 100),
            size: 16 * 1024 * 1024,
            sector: u64::from(sector),
        };
        let clone =
            crate::ntfs_file_clone::Plan::prepare(&mut source, range, SERIAL + 1, &cancel).unwrap();
        let mut target = Cursor::new(vec![0xd4; range.size as usize]);
        let tool = crate::ntfs_file_clone::Plan::engine().unwrap();
        clone
            .write(
                &tool,
                &mut source,
                &mut target,
                Write::flush,
                &cancel,
                |_, _, _| Ok(()),
            )
            .unwrap();
        clone.verify(&mut target, &cancel).unwrap();
        let target_nodes = merge::catalogue_file_clone(&mut target, &cancel).unwrap();
        let target_id = target_nodes.names[&path(&["original.bin"])];
        assert_ne!(source_nodes.names[&path(&["original.bin"])], target_id);
        let mut target = target.into_inner();
        let expected_entry = [
            &object_id[..16],
            &(target_id.0 | u64::from(target_id.1) << 48).to_le_bytes(),
            &object_id[16..],
        ]
        .concat();
        let offsets = target
            .windows(expected_entry.len())
            .enumerate()
            .filter_map(|(at, row)| (row == expected_entry).then_some(at))
            .collect::<Vec<_>>();
        assert!(!offsets.is_empty(), "object index contains all four fields");
        // B-tree splits can retain old copies of this entry in node slack.
        for at in offsets {
            target[at + 16 + 6] ^= 0x40;
        }
        let failure = clone.verify(&mut Cursor::new(target), &cancel).unwrap_err();
        assert!(failure.contains("对象 ID 或重解析点索引"), "{failure}");
    }
}

fn link_data(tag: u32, flags: Option<u32>, substitute: &str, print: &str) -> Vec<u8> {
    let substitute = substitute
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let print = print
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let names = substitute.len() + print.len() + 4;
    let length = (8 + 4 * usize::from(flags.is_some()) + names) as u16;
    let mut data = tag.to_le_bytes().to_vec();
    data.extend_from_slice(&length.to_le_bytes());
    data.extend([0; 4]);
    data.extend_from_slice(&(substitute.len() as u16).to_le_bytes());
    data.extend_from_slice(&((substitute.len() + 2) as u16).to_le_bytes());
    data.extend_from_slice(&(print.len() as u16).to_le_bytes());
    if let Some(flags) = flags {
        data.extend_from_slice(&flags.to_le_bytes());
    }
    data.extend_from_slice(&substitute);
    data.extend([0; 2]);
    data.extend_from_slice(&print);
    data.extend([0; 2]);
    data
}
