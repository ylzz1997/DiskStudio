//! Install the immutable wimlib-backed fixture inside the isolated guest.
use super::*;
use partitions::{
    merge::{metadata, Catalogue, Node},
    platform::maintenance,
};

const MEDIA: &str = "Ntfs-Wof-Source.raw";
const UPSTREAM_START_LBA: u32 = 84_992;
const STREAM_SIZE: u64 = 192 * 1024 + 123;

pub(super) fn load() -> Result<Vec<u8>, String> {
    let expected = option_env!("DISKSTUDIO_ACCEPTANCE_WOF_IMAGE_SHA256")
        .ok_or("Expected SHA-256 for the immutable WOF fixture")?;
    let file =
        fs::File::open(std::env::current_exe().map_err(err)?.with_file_name(MEDIA)).map_err(err)?;
    if file.metadata().map_err(err)?.len() != SOURCE_BYTES {
        return Err("The immutable WOF fixture has the wrong length".into());
    }
    let mut bytes = Vec::new();
    file.take(SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() as u64 != SOURCE_BYTES || format!("{:x}", Sha256::digest(&bytes)) != expected {
        return Err("The immutable WOF fixture differs from its build digest".into());
    }
    Ok(bytes)
}

fn rebase(image: &mut [u8], at: u64) -> Result<(), String> {
    let (main, last) = image.split_at_mut(SOURCE_BYTES as usize - SECTOR as usize);
    if main.get(..512) != last.get(..512)
        || main.get(3..11) != Some(b"NTFS    ")
        || main[28..32] != UPSTREAM_START_LBA.to_le_bytes()
        || main[80..84] != [0; 4]
        || at % SECTOR != 0
    {
        return Err("The paired WOF fixture boot records differ or need a checksum".into());
    }
    let start = u32::try_from(at / SECTOR).map_err(err)?.to_le_bytes();
    for sector in [&mut main[..512], &mut last[..512]] {
        sector[28..32].copy_from_slice(&start);
    }
    Ok(())
}

fn known_node(catalogue: &Catalogue, algorithm: u32) -> Result<&Node, String> {
    let path = vec![format!("wof-{algorithm}.bin").encode_utf16().collect()];
    let id = catalogue
        .names
        .get(&path)
        .ok_or("Synthetic WOF file is missing")?;
    let node = &catalogue.nodes[id];
    let reparse = [metadata::WOF_TAG, 20, 1, 2, 1, algorithm, 0]
        .map(u32::to_le_bytes)
        .concat();
    if node.directory
        || node.metadata.get(&metadata::REPARSE) != Some(&reparse)
        || node.streams.len() != 3
        || !node.streams.iter().any(|stream| {
            stream.locator.name.is_empty()
                && stream.size == STREAM_SIZE
                && stream.storage_flags == ntfs_streams::SPARSE
                && stream.allocated_ranges.is_empty()
        })
        || !node.streams.iter().any(|stream| {
            stream.locator.name == metadata::WOF_STREAM && (1..STREAM_SIZE).contains(&stream.size)
        })
    {
        return Err("Synthetic WOF algorithms, layout or stream flags differ".into());
    }
    Ok(node)
}

pub(super) fn sample(found: &Catalogue, cancel: &AtomicBool) -> Result<(), String> {
    let expected = catalogue(&mut Cursor::new(load()?), cancel)?;
    for algorithm in 0..4 {
        let original = known_node(&expected, algorithm)?;
        let observed = known_node(found, algorithm)?;
        if original
            .streams
            .iter()
            .zip(&observed.streams)
            .any(|(left, right)| {
                left.locator.name != right.locator.name
                    || left.storage_flags != right.storage_flags
                    || left.size != right.size
                    || left.sha256 != right.sha256
            })
        {
            return Err(format!(
                "Compressed or alternate data differs for WOF algorithm {algorithm}"
            ));
        }
    }
    Ok(())
}

pub(super) fn prepare(
    fixture: &Fixture,
    mut image: Vec<u8>,
    register_source: bool,
) -> Result<String, String> {
    rebase(&mut image, fixture.source_at)?;
    let expected = format!("{:x}", Sha256::digest(&image));
    let cancel = AtomicBool::new(false);
    let image_catalogue = catalogue(&mut Cursor::new(&image), &cancel)?;
    for algorithm in 0..4 {
        known_node(&image_catalogue, algorithm)?;
    }
    crate::ntfs_file_clone::Plan::prepare(
        &mut Cursor::new(&image),
        surface_scan::Range {
            offset: fixture.target_at,
            size: TARGET_BYTES,
            sector: SECTOR,
        },
        0x3910_8133_4579_4021,
        &cancel,
    )?;
    if register_source {
        operation(
            A,
            Action::Create {
                offset: fixture.source_at,
                size: SOURCE_BYTES,
                kind: PartitionKind::Windows,
            },
        )?;
    }
    scope_with_target(fixture, false)?;
    write_source(fixture, image, &expected)?;
    scope_with_target(fixture, false)?;
    allocation_batch::set_online(A)?;
    prepare_target(fixture)
        .map(|result| format!("{result}\nSource image SHA-256 before mounting\n{expected}"))
}

pub(super) fn prepare_target(fixture: &Fixture) -> Result<String, String> {
    let (source_label, target_label, _) = Case::WofImage.labels();
    scope_with_target(fixture, false)?;
    if !available(
        &fat_resize_checks::model(B)?,
        fixture.target_at,
        TARGET_BYTES,
    ) || first_nonzero(&mut raw(B)?, fixture.target_at, TARGET_BYTES)?.is_some()
    {
        return Err("The original B fixture window is no longer available and zero".into());
    }
    let source = reallocation_checks::metadata_partition(
        A,
        Some(source_label),
        fixture.source_at / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let (_, bound) = resolved(A, source.number, fixture.source_at, SOURCE_BYTES, true)?;
    let cancel = AtomicBool::new(false);
    let mut reader = data_windows::reader(&bound)?;
    let observed = catalogue(&mut reader, &cancel)?;
    sample(&observed, &cancel)?;
    drop(reader);
    let target = create(
        B,
        fixture.target_at,
        TARGET_BYTES,
        FileSystem::Fat16,
        target_label,
        4096,
        false,
    )?;
    scope(fixture)?;
    Ok(format!(
        "WOF image prepared. A/{} {source_label} 96 MiB to B/{target} {target_label} 48 MiB.\nFour compressed streams match the immutable image.",
        source.number
    ))
}

fn write_source(fixture: &Fixture, image: Vec<u8>, expected: &str) -> Result<(), String> {
    let whole = selection(A, None)?;
    let range = surface_scan::Range {
        offset: fixture.source_at,
        size: SOURCE_BYTES,
        sector: SECTOR,
    };
    let context = maintenance::write_context(&whole, None)?;
    let exact = |ctx: &partitions::platform::TableReadContext| {
        if ctx
            .resolved
            .disk
            .serial_number
            .as_deref()
            .map(disk_identity::clean)
            != Some(A)
            || ctx.resolved.disk.logical_sector_size != Some(SECTOR)
            || ctx.resolved.size != 16 * GIB
            || ctx
                .parts
                .iter()
                .filter(|part| part.offset == range.offset && part.size == range.size)
                .count()
                != 1
        {
            return Err("Expected one newly registered 96 MiB A partition".into());
        }
        maintenance::protect_partitions(ctx, range)
    };
    exact(&context)?;
    let locks = data_windows::lock(&context.resolved)?;
    let current = maintenance::write_context(&whole, Some((&context, &locks)))?;
    exact(&current)?;
    let mut device = data_windows::exclusive_device(&current.resolved)?;
    let table = |device: &mut fs::File| {
        maintenance::capture_write_table(&current, device, &whole.target, range, true)?
            .ok_or_else(|| "The exact A extent requires a verified GPT".to_string())
    };
    let before = table(&mut device)?;
    let gpt = table_restore::archive_layout(&before)?;
    if gpt
        .entries
        .iter()
        .filter(|entry| {
            entry.offset == range.offset
                && entry.size == range.size
                && entry.attributes == 0
                && entry.kind == partitions::policy::BASIC_DATA
        })
        .count()
        != 1
    {
        return Err("A has no uniquely matching ordinary GPT entry for this fixture".into());
    }
    data_windows::persist_offline(&device)?;
    data_windows::confirm_writable_offline(&device)?;
    if table(&mut device)? != before
        || first_nonzero(&mut device, range.offset, range.size)?.is_some()
    {
        return Err("The newly registered WOF extent changed or contains data".into());
    }
    let mut input = Cursor::new(image);
    let mut region =
        data_io::Region::new(device.try_clone().map_err(err)?, range.offset, range.size)
            .map_err(err)?;
    let copied = data_io::copy_exact(&mut input, &mut region, range.size, |_| Ok(()))?;
    device.sync_all().map_err(err)?;
    region.rewind().map_err(err)?;
    if copied != expected || data_io::hash_exact(&mut region, range.size, |_| Ok(()))? != expected {
        return Err("The WOF fixture failed full readback".into());
    }
    if table(&mut device)? != before {
        return Err("A layout changed during WOF fixture seeding".into());
    }
    data_windows::confirm_writable_offline(&device)
}
