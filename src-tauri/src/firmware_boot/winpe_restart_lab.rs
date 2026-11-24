//! Explicit, ignored WinPE lab entry points. Validate the two disposable disks
//! before mounting any fixture volume or touching firmware state.
#[path = "../../../scripts/vm/guest-checks/src/winpe_lab_disks.rs"]
mod lab;

use super::*;
use crate::{
    data_tasks::Selection,
    system_migration::{Options as MigrationOptions, PartitionSelection, PartitionSize},
};
use std::{fs, path::Path, process::Command};

const A: &str = "DS-TEST-A-0001      _00000001.";
const B: &str = "DS-TEST-B-0001      _00000001.";

fn pair() -> (u32, u32) {
    let mut disks = lab::disk_inventory().unwrap();
    disks.sort_by_key(|disk| disk.serial_number.trim().to_owned());
    let [a, b] = disks.as_slice() else {
        panic!("Expected only the two WinPE test disks")
    };
    assert_eq!(a.serial_number.trim(), A);
    assert_eq!(b.serial_number.trim(), B);
    assert_ne!(a.index, b.index);
    for (disk, serial, namespace) in [(a, "DS-TEST-A-0001", A), (b, "DS-TEST-B-0001", B)] {
        lab::verify_disk_geometry(disk.index, serial, namespace, 4096).unwrap();
    }
    (a.index, b.index)
}

fn copy_tree(source: &Path, target: &Path) {
    for child in fs::read_dir(source).unwrap() {
        let child = child.unwrap();
        let dest = target.join(child.file_name());
        let kind = child.file_type().unwrap();
        if kind.is_dir() {
            fs::create_dir(&dest).unwrap();
            copy_tree(&child.path(), &dest);
        } else {
            assert!(kind.is_file());
            assert!(!dest.exists());
            fs::copy(child.path(), dest).unwrap();
        }
    }
}

#[test]
#[ignore = "requires the isolated 4Kn WinPE A/B guest with an empty Test B and verified ISO payload"]
fn prepares_winpe_media_on_test_b() {
    let (_, b) = pair();
    lab::verify_blank_disk(b, "DS-TEST-B-0001", B, 4096).unwrap();
    lab::check_drive_letter('T').unwrap();
    let roots: Vec<_> = ('C'..='Z')
        .filter_map(|letter| {
            let root = PathBuf::from(format!("{letter}:\\"));
            let wide: Vec<_> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
            if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) } != 5
            {
                return None;
            }
            let media = root.join("dspayload");
            media.join("sources\\boot.wim").is_file().then_some(media)
        })
        .collect();
    let [source] = roots.as_slice() else {
        panic!("Expected one read-only optical WinPE payload")
    };
    let diskpart = Path::new(r"X:\ds-handoff-part.txt");
    fs::write(
        diskpart,
        format!("select disk {b}\r\nconvert gpt\r\ncreate partition efi size=2048\r\nformat fs=fat32 quick label=DSWINPE\r\nassign letter=T\r\n"),
    ).unwrap();
    assert!(Command::new(r"X:\Windows\System32\diskpart.exe")
        .args(["/s", diskpart.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let (selection, part) = selection(b);
    let root = cap_std::fs::Dir::open_ambient_dir(r"T:\", cap_std::ambient_authority()).unwrap();
    let location = data_windows::file_location(&root.into_std_file()).unwrap();
    assert_eq!(location.target, selection.target);
    assert_eq!(location.disk_number(), b);
    assert_eq!(part.base.size, 2048 * 1024 * 1024);
    assert!(!Path::new(r"T:\EFI").exists());
    copy_tree(source, Path::new(r"T:\"));
    println!("WinPE optical payload copied to the verified Test B EFI partition.");
}

fn selection(index: u32) -> (Selection, crate::storage::LocalPartition) {
    select_partition(index, |part| part.role == "EFI 系统")
}

fn select_partition(
    index: u32,
    matches: impl Fn(&crate::storage::LocalPartition) -> bool,
) -> (Selection, crate::storage::LocalPartition) {
    let disks = crate::storage::read_snapshot().unwrap().disks;
    let disk = disks
        .into_iter()
        .find(|disk| disk.hardware.number == index)
        .unwrap();
    let mut parts = disk.partitions.into_iter().filter(matches);
    let part = parts
        .next()
        .expect("Expected one matching Test B partition");
    assert!(parts.next().is_none());
    let source = Selection {
        target: crate::partitions::Target::try_from(disk.hardware.identity.unwrap()).unwrap(),
        layout_key: disk.hardware.layout_key.unwrap(),
        partition: Some(part.number),
    };
    assert!(source.partition.is_some());
    (source, part)
}

#[test]
#[ignore = "schedules and immediately requests restart of the verified isolated WinPE guest"]
fn hands_off_to_the_prepared_winpe_partition() {
    let (_, b) = pair();
    let (source, _) = selection(b);
    let before = inspect().unwrap();
    let plan = prepare_winpe(
        WinPeRequest {
            revision: before.revision.clone(),
            source,
            task: None,
        },
        PathBuf::from(r"X:\DiskStudio.exe"),
        None,
    )
    .unwrap();
    assert_eq!(inspect().unwrap().revision, before.revision);
    println!(
        "WINPE HANDOFF: Boot{:04X}, partition {}, image {} bytes, payload SHA-256 {}",
        plan.entry, plan.media.partition, plan.media.wim_bytes, plan.media.app_sha256,
    );
    let outcome = winpe_restart(plan.token).unwrap();
    assert_eq!(outcome.report.order, before.order);
    assert_eq!(outcome.report.next, Some(plan.entry));
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
}

#[test]
#[ignore = "checks the firmware after the isolated WinPE restart"]
fn confirms_firmware_booted_the_winpe_partition() {
    let (_, b) = pair();
    let _ = selection(b);
    confirm_handoff();
}

fn confirm_handoff() {
    let report = inspect().unwrap();
    let current = report.current.expect("Missing BootCurrent");
    assert_eq!(report.next, None);
    assert!(!report.order.contains(&current));
    let entry = report
        .entries
        .iter()
        .find(|entry| entry.id == current)
        .unwrap();
    assert_eq!(entry.description.as_deref(), Some("DiskStudio WinPE"));
    println!("WINPE HANDOFF VERIFIED: BootCurrent Boot{current:04X}, BootNext consumed, ordinary order {:?}", report.order);
}

fn copy_lab_selection(index: u32) -> Selection {
    let disk = crate::storage::read_snapshot().unwrap().disks.into_iter()
        .find(|disk| disk.hardware.number == index).unwrap();
    Selection {
        target: crate::partitions::Target::try_from(disk.hardware.identity.unwrap()).unwrap(),
        layout_key: disk.hardware.layout_key.unwrap(),
        partition: None,
    }
}

fn copy_lab_media(index: u32) -> Selection {
    select_partition(index, |part| part.role == "数据分区" && part.base.fs == "FAT32").0
}

fn copy_lab_optical() -> PathBuf {
    let optical: Vec<_> = ('C'..='Z').filter_map(|letter| {
        let root = format!("{letter}:\\");
        let wide: Vec<_> = root.encode_utf16().chain([0]).collect();
        (unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) } == 5)
            .then(|| PathBuf::from(root).join("dspayload"))
            .filter(|path| path.join(r"sources\boot.wim").is_file())
    }).collect();
    let [path] = optical.as_slice() else { panic!("Expected one read-only WinPE media tree") };
    path.clone()
}

fn mount_copy_lab_media(migration: bool) {
    let (a, b) = pair();
    lab::verify_blank_disk(a, "DS-TEST-A-0001", A, 4096).unwrap();
    for letter in ['S', 'T', 'U'] {
        lab::check_drive_letter(letter).unwrap();
    }
    let mut script = if migration {
        let (windows, part) = select_partition(b, |part| part.base.fs == "NTFS");
        assert_eq!(part.base.start, 301989888);
        let source = data_windows::resolve(&windows, true, false).unwrap();
        assert_eq!(source.disk.number, b);
        assert_eq!(source.disk.is_boot, Some(false));
        assert_eq!(source.disk.is_system, Some(false));
        assert!(source.size > 8 * 1024 * 1024 * 1024);
        format!("select disk {b}\r\nselect partition {}\r\nshrink desired=1024 minimum=1024\r\n", windows.partition.unwrap())
    } else {
        lab::verify_blank_disk(b, "DS-TEST-B-0001", B, 4096).unwrap();
        format!(concat!(
            "select disk {a}\r\nconvert gpt\r\ncreate partition primary size=128\r\n",
            "format fs=ntfs quick label=DSCLONEDST\r\nassign letter=U\r\n",
            "select disk {b}\r\nconvert gpt\r\ncreate partition primary size=128\r\n",
            "format fs=ntfs quick label=DSCLONESRC\r\nassign letter=S\r\n"
        ), a=a, b=b)
    };
    script.push_str("create partition primary size=1000\r\nformat fs=fat32 quick label=DSWINPE\r\nassign letter=T\r\n");
    let commands = r"X:\ds-copy-fixture.txt";
    fs::write(commands, script).unwrap();
    assert!(Command::new(r"X:\Windows\System32\diskpart.exe")
        .args(["/s", commands]).status().unwrap().success());
    copy_tree(&copy_lab_optical(), &checked_lab_volume('T', b, &copy_lab_media(b)));
    if !migration {
        let (source, _) = select_partition(b, |p| p.base.size == QUEUED_SOURCE && p.base.fs == "NTFS");
        fs::write(checked_lab_volume('S', b, &source).join("offline-fixture.txt"), QUEUED_SAMPLE).unwrap();
    }
    println!("WINPE COPY FIXTURE READY");
}

#[test]
#[ignore = "requires the isolated 4Kn A/B guest with blank disks and verified optical media"]
fn prepares_queued_clone() {
    mount_copy_lab_media(false);
}

#[test]
#[ignore = "requires the isolated 4Kn A/B guest with the approved Windows fixture on B and blank A"]
fn prepares_queued_migration() {
    mount_copy_lab_media(true);
}

fn schedule_copy_lab(migration: bool, options: MigrationOptions) {
    let (a, b) = pair();
    let media = copy_lab_media(b);
    let task = if migration {
        let source = copy_lab_selection(b);
        let target = copy_lab_selection(a);
        assert_eq!(data_windows::resolve(&source, false, false).unwrap().disk.is_offline, Some(false));
        let (job, preview) = MigrationJob::prepare(source, target, &media.target, options).unwrap();
        (Task::Migrate(job), preview)
    } else {
        let (source, _) = select_partition(b, |p| p.base.size == QUEUED_SOURCE && p.base.fs == "NTFS");
        let (target, _) = select_partition(a, |p| p.base.size == QUEUED_SOURCE && p.base.fs == "NTFS");
        let (job, preview) = CloneJob::prepare(source, target, crate::used_space::Mode::AllSectors, &media.target).unwrap();
        (Task::Clone(job), preview)
    };
    let before = inspect().unwrap();
    let plan = prepare_winpe(
        WinPeRequest { revision: before.revision, source: media, task: None },
        PathBuf::from(r"X:\DiskStudio.exe"), Some(task),
    ).unwrap();
    assert!(plan.task.as_ref().unwrap().destructive);
    let result = winpe_restart(plan.token).unwrap();
    assert_eq!(result.report.next, Some(plan.entry));
    assert_eq!(result.report.order, before.order);
    assert!(result.error.is_none(), "{:?}", result.error);
}

#[test]
#[ignore = "arms a one-time clone in the verified isolated WinPE guest"]
fn schedules_queued_clone() {
    schedule_copy_lab(false, MigrationOptions::default());
}

#[test]
#[ignore = "arms a one-time Windows migration in the verified isolated WinPE guest"]
fn schedules_queued_migration() {
    schedule_copy_lab(true, MigrationOptions::default());
}

const GROW_SAMPLE: &[u8] = b"Same file and volume after growing the copied NTFS partition.\r\n";
const GROWN_SIZE: u64 = 20 * disk_core::MIB;
const WINDOWS_SIZE: u64 = 12 * 1024 * disk_core::MIB;
const MOVED_WINDOWS: u64 = 320 * disk_core::MIB;
const MOVED_DATA: u64 = 13 * 1024 * disk_core::MIB;

fn resize_migration_sizes(b: u32, relocate: bool) -> Vec<crate::system_migration::PartitionSize> {
    let (_, windows) = select_partition(b, |part| part.base.start == 301989888 && part.base.fs == "NTFS");
    let (_, data) = select_partition(b, |part| part.base.size == 16 * disk_core::MIB && part.base.fs == "NTFS");
    assert!(windows.base.size > WINDOWS_SIZE);
    vec![
        crate::system_migration::PartitionSize { partition: windows.number, bytes: Some(WINDOWS_SIZE), start_bytes: relocate.then_some(MOVED_WINDOWS) },
        crate::system_migration::PartitionSize { partition: data.number, bytes: Some(GROWN_SIZE), start_bytes: relocate.then_some(MOVED_DATA) },
    ]
}

#[test]
#[ignore = "requires clean Windows fixture B, blank A and the verified read-only WinPE payload"]
fn prepares_queued_migration_resize() {
    prepare_resize_fixture(false);
}

#[test]
#[ignore = "requires the Windows resize fixture on B, blank A and the verified read-only WinPE payload"]
fn prepares_queued_migration_relocation() {
    prepare_resize_fixture(true);
}

fn prepare_resize_fixture(existing_sample: bool) {
    let (a, b) = pair();
    lab::verify_blank_disk(a, "DS-TEST-A-0001", A, 4096).unwrap();
    let (media, part) = select_partition(b, |part| part.role == "数据分区" && part.base.fs == "FAT32");
    assert_eq!(part.base.size, 1000 * disk_core::MIB);
    let letter = part.drive_letter.unwrap().chars().next().unwrap();
    let root = checked_lab_volume(letter, b, &media);
    let optical = copy_lab_optical();
    for item in ["DiskStudio.exe", r"sources\boot.wim"] {
        assert!(root.join(item).is_file());
        let mut permissions = fs::metadata(root.join(item)).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(root.join(item), permissions).unwrap();
        fs::copy(optical.join(item), root.join(item)).unwrap();
    }
    let data_setup = if existing_sample {
        let (data, part) = select_partition(b, |part| part.base.size == 16 * disk_core::MIB && part.base.fs == "NTFS");
        let letter = part.drive_letter.unwrap().chars().next().unwrap();
        assert_eq!(fs::read(checked_lab_volume(letter, b, &data).join("resize.txt")).unwrap(), GROW_SAMPLE);
        String::new()
    } else {
        lab::check_drive_letter('V').unwrap();
        format!("select disk {b}\r\ncreate partition primary size=16\r\nformat fs=ntfs quick label=DSRESIZE\r\nassign letter=V\r\n")
    };
    let path = r"X:\ds-copy-resize-fixtures.txt";
    fs::write(path, format!("select disk {a}\r\nconvert gpt\r\n{data_setup}")).unwrap();
    assert!(Command::new(r"X:\Windows\System32\diskpart.exe").args(["/s", path])
        .status().unwrap().success());
    if !existing_sample {
        let (data, _) = select_partition(b, |part| part.base.size == 16 * disk_core::MIB && part.base.fs == "NTFS");
        fs::write(checked_lab_volume('V', b, &data).join("resize.txt"), GROW_SAMPLE).unwrap();
    }
    assert_eq!(resize_migration_sizes(b, existing_sample).len(), 2);
    println!("WINPE RESIZE MIGRATION FIXTURE READY");
}

#[test]
#[ignore = "schedules migration with Windows shrink and data growth in the verified isolated guest"]
fn schedules_queued_migration_resize() {
    let (_, b) = pair();
    schedule_copy_lab(true, MigrationOptions { sizes: resize_migration_sizes(b, false), ..MigrationOptions::default() });
}

#[test]
#[ignore = "moves and resizes two NTFS partitions in the verified isolated guest"]
fn schedules_queued_migration_relocation() {
    let (_, b) = pair();
    schedule_copy_lab(true, MigrationOptions { sizes: resize_migration_sizes(b, true), ..MigrationOptions::default() });
}

fn selection_migration_options(b: u32) -> MigrationOptions {
    let (_, media) = select_partition(b, |part| part.base.size == 1000 * disk_core::MIB && part.base.fs == "FAT32");
    let (_, data) = select_partition(b, |part| part.base.size == 16 * disk_core::MIB && part.base.fs == "NTFS");
    assert_ne!(media.base.start, data.base.start);
    MigrationOptions {
        selection: PartitionSelection::Selected { partitions: vec![data.number] },
        sizes: vec![PartitionSize { partition: data.number, bytes: None, start_bytes: Some(media.base.start) }],
    }
}

#[test]
#[ignore = "reports the verified GPT and startup dependencies in the isolated selection fixture"]
fn probes_queued_migration_selection() {
    let (_, b) = pair();
    let mut options = selection_migration_options(b);
    options.sizes.clear();
    let selection = copy_lab_selection(b);
    let source = data_windows::resolve(&selection, true, false).unwrap();
    let locks = data_windows::state_locks(&source).unwrap();
    let source = data_windows::resolve_locked(&selection, false, &source, &locks).unwrap();
    let file = data_windows::exclusive_reader(&source).unwrap();
    let mut view = crate::data_io::Region::for_file_view(&file, 0, source.size).unwrap();
    let (windows, table) = crate::boot_inspection::virtualization::check_windows(
        &mut view, source.size, 4096, None,
    ).unwrap();
    let layout = crate::table_restore::archive_layout(&table).unwrap();
    let dependencies = windows.gpt_dependencies(&mut view, 4096, layout.guid.parse().unwrap()).unwrap();
    let numbers = source.migration_partition_numbers(&selection, &layout).unwrap();
    for entry in &layout.entries {
        let number = numbers.iter().find(|row| row.slot == entry.slot).unwrap();
        println!(
            "p{} slot{} at {} size {} ordinary {} provider {} BCD {}",
            number.number, entry.slot, entry.offset / disk_core::MIB, entry.size / disk_core::MIB,
            crate::table_restore::ordinary(entry), number.can_omit,
            dependencies.contains(&entry.guid.parse().unwrap()),
        );
    }
    let plan = crate::system_migration::Plan::prepare_layout(
        &mut view, source.size, source.size, 4096, None, &options, |_| Ok(numbers),
    ).unwrap();
    println!("Selection: {}", serde_json::to_value(plan.preview()).unwrap()["partitionSelection"]);
}

#[test]
#[ignore = "copies the system and selected NTFS volume while omitting the verified WinPE FAT32 data volume"]
fn schedules_queued_migration_selection() {
    let (_, b) = pair();
    schedule_copy_lab(true, selection_migration_options(b));
}

#[test]
#[ignore = "uses the batch handoff with one destination in the verified isolated 4Kn A/B guest"]
fn schedules_queued_migration_batch() {
    use crate::data_tasks::system_migration_tasks::Destination;

    let (a, b) = pair();
    let media = copy_lab_media(b);
    let source = copy_lab_selection(b);
    let options = selection_migration_options(b);
    let (batch, preview) = MigrationBatch::prepare(source, vec![Destination {
        selection: copy_lab_selection(a),
        partition_sizes: options.sizes,
        partition_selection: options.selection,
    }], &media).unwrap();
    let before = inspect().unwrap();
    let plan = prepare_winpe(
        WinPeRequest { revision: before.revision, source: media, task: None },
        PathBuf::from(r"X:\DiskStudio.exe"), Some((Task::MigrateBatch(batch), preview)),
    ).unwrap();
    assert!(plan.task.as_ref().unwrap().destructive);
    let result = winpe_restart(plan.token).unwrap();
    assert_eq!(result.report.next, Some(plan.entry));
    assert_eq!(result.report.order, before.order);
    assert!(result.error.is_none(), "{:?}", result.error);
}

#[test]
#[ignore = "checks NTFS geometry, the Windows boot chain and an ordinary file on the offline copy"]
fn verifies_queued_migration_resize() {
    verify_resize_migration(false);
}

#[test]
#[ignore = "checks relocated NTFS geometry, cleared old offsets, Windows boot chain and the copied file"]
fn verifies_queued_migration_relocation() {
    verify_resize_migration(true);
}

fn verify_resize_migration(relocated: bool) {
    let (a, b) = pair();
    confirm_handoff();
    assert!(claim_winpe_task().unwrap().is_none());
    let options = MigrationOptions { sizes: resize_migration_sizes(b, relocated), ..MigrationOptions::default() };
    let selection = copy_lab_selection(b);
    let resolved = data_windows::resolve(&selection, true, false).unwrap();
    let a = lab::verify_disk_geometry(a, "DS-TEST-A-0001", A, 4096).unwrap();
    let b = lab::verify_disk_geometry(b, "DS-TEST-B-0001", B, 4096).unwrap();
    lab::verify_offline(&a).unwrap();
    let bytes = 16 * 1024 * 1024 * 1024;
    let mut source = crate::data_io::Region::for_file_view(&b, 0, bytes).unwrap();
    let mut destination = crate::data_io::Region::for_file_view(&a, 0, bytes).unwrap();
    let plan = crate::system_migration::Plan::prepare_layout(&mut source, bytes, bytes, 4096, None, &options,
        |layout| resolved.migration_partition_numbers(&selection, layout)).unwrap();
    plan.verify_output(&mut destination).unwrap();
    let change = plan.volume_resizes().find(|part| part.new_bytes == GROWN_SIZE).unwrap();
    let mut view = crate::data_io::Region::for_file_view(&a, change.new_offset, change.new_bytes).unwrap();
    let mut ntfs = ntfs::Ntfs::new(&mut view).unwrap();
    ntfs.read_upcase_table(&mut view).unwrap();
    assert_eq!(crate::boot_inspection::ntfs_file(&ntfs, &mut view, &["resize.txt"], 4096).unwrap().unwrap(), GROW_SAMPLE);
    if relocated {
        use std::io::{Read, Seek, SeekFrom};
        for (&start, size) in [MOVED_WINDOWS, MOVED_DATA].iter().zip([WINDOWS_SIZE, GROWN_SIZE]) {
            let change = plan.volume_resizes().find(|part| part.new_bytes == size).unwrap();
            assert_eq!(change.new_offset, start);
            assert_ne!(change.offset, start);
            destination.seek(SeekFrom::Start(change.offset)).unwrap();
            let mut retired = vec![0; disk_core::MIB as usize];
            destination.read_exact(&mut retired).unwrap();
            assert!(retired.iter().all(|&byte| byte == 0));
        }
        println!("WINPE RELOCATION VERIFIED: Windows start 320 MiB, data start 13312 MiB, original boot areas cleared");
    }
    println!("WINPE RESIZE MIGRATION VERIFIED: offline target, resized NTFS volumes, 12 GiB Windows boot chain, matching marker, consumed ticket");
}

#[test]
#[ignore = "verifies the selected copy on the two isolated 4Kn test disks"]
fn verifies_queued_migration_selection() {
    use std::io::{Read, Seek, SeekFrom};

    let (a, b) = pair();
    confirm_handoff();
    assert!(claim_winpe_task().unwrap().is_none());
    let options = selection_migration_options(b);
    let selection = copy_lab_selection(b);
    let resolved = data_windows::resolve(&selection, true, false).unwrap();
    let a = lab::verify_disk_geometry(a, "DS-TEST-A-0001", A, 4096).unwrap();
    let b = lab::verify_disk_geometry(b, "DS-TEST-B-0001", B, 4096).unwrap();
    lab::verify_offline(&a).unwrap();
    let bytes = 16 * 1024 * 1024 * 1024;
    let mut source = crate::data_io::Region::for_file_view(&b, 0, bytes).unwrap();
    let mut destination = crate::data_io::Region::for_file_view(&a, 0, bytes).unwrap();
    let plan = crate::system_migration::Plan::prepare_layout(&mut source, bytes, bytes, 4096, None, &options,
        |layout| resolved.migration_partition_numbers(&selection, layout)).unwrap();
    plan.verify_output(&mut destination).unwrap();
    let before = loader::checked_gpt(&b, bytes, 4096).unwrap();
    let after = loader::checked_gpt(&a, bytes, 4096).unwrap();
    let media = before.partitions.iter().find(|part| part.range.end - part.range.start == 1000 * disk_core::MIB).unwrap();
    let data_bytes = 16 * disk_core::MIB;
    let data = before.partitions.iter().find(|part| part.range.end - part.range.start == data_bytes).unwrap();
    assert_eq!(after.disk_guid, before.disk_guid);
    assert_eq!(after.partitions.len() + 1, before.partitions.len());
    assert!(after.partitions.iter().all(|part| part.guid != media.guid));
    let retained = after.partitions.iter().find(|part| part.guid == data.guid).unwrap();
    assert_eq!(retained.range, media.range.start..media.range.start + data_bytes);
    let mut view = crate::data_io::Region::for_file_view(&a, retained.range.start, data_bytes).unwrap();
    let mut ntfs = ntfs::Ntfs::new(&mut view).unwrap();
    ntfs.read_upcase_table(&mut view).unwrap();
    assert_eq!(crate::boot_inspection::ntfs_file(&ntfs, &mut view, &["resize.txt"], 4096).unwrap().unwrap(), GROW_SAMPLE);
    let mut block = vec![0; disk_core::MIB as usize];
    for range in [retained.range.end..media.range.end, data.range.clone()] {
        destination.seek(SeekFrom::Start(range.start)).unwrap();
        for _ in range.step_by(block.len()) {
            destination.read_exact(&mut block).unwrap();
            assert!(block.iter().all(|&byte| byte == 0));
        }
    }
    println!("WINPE SELECTION VERIFIED: 1000 MiB FAT32 omitted, 16 MiB NTFS moved, marker and boot chain verified");
}

#[test]
#[ignore = "checks the offline clone and the known NTFS file on the two verified test disks"]
fn verifies_queued_clone() {
    let (a, b) = pair();
    confirm_handoff();
    assert!(claim_winpe_task().unwrap().is_none());
    let verify = |index, serial, namespace, offline| {
        let device = lab::verify_disk_geometry(index, serial, namespace, 4096).unwrap();
        if offline { lab::verify_offline(&device).unwrap() }
        let bytes = 16 * 1024 * 1024 * 1024;
        let layout = loader::checked_gpt(&device, bytes, 4096).unwrap();
        let part = layout.partitions.iter().find(|p| p.range.end - p.range.start == QUEUED_SOURCE).unwrap();
        let mut view = crate::data_io::Region::for_file_view(device, part.range.start, QUEUED_SOURCE).unwrap();
        let hash = crate::data_io::hash_exact(&mut view, QUEUED_SOURCE, |_| Ok(())).unwrap();
        view.seek(SeekFrom::Start(0)).unwrap();
        let mut ntfs = ntfs::Ntfs::new(&mut view).unwrap();
        ntfs.read_upcase_table(&mut view).unwrap();
        assert_eq!(crate::boot_inspection::ntfs_file(&ntfs, &mut view, &["offline-fixture.txt"], 4096).unwrap().unwrap(), QUEUED_SAMPLE);
        hash
    };
    let destination = verify(a, "DS-TEST-A-0001", A, true);
    // The online source can receive filesystem metadata writes after the copy releases its locks.
    verify(b, "DS-TEST-B-0001", B, false);
    println!("WINPE QUEUED CLONE VERIFIED: partition SHA-256 {destination}, matching NTFS file, ticket consumed");
}

#[test]
#[ignore = "verifies the migrated Windows boot chain on offline test disks"]
fn verifies_queued_migration() {
    let (a, b) = pair();
    confirm_handoff();
    assert!(claim_winpe_task().unwrap().is_none());
    let a = lab::verify_disk_geometry(a, "DS-TEST-A-0001", A, 4096).unwrap();
    let b = lab::verify_disk_geometry(b, "DS-TEST-B-0001", B, 4096).unwrap();
    lab::verify_offline(&a).unwrap();
    let bytes = 16 * 1024 * 1024 * 1024;
    let mut source = crate::data_io::Region::for_file_view(&b, 0, bytes).unwrap();
    let mut destination = crate::data_io::Region::for_file_view(&a, 0, bytes).unwrap();
    let plan = crate::system_migration::Plan::prepare(&mut source, bytes, bytes, 4096, None).unwrap();
    plan.verify_output(&mut destination).unwrap();
    println!("WINPE QUEUED MIGRATION VERIFIED: offline destination, target Windows boot chain verified, ticket consumed");
}

fn adk_selection(b: u32) -> Selection {
    let (selection, part) = select_partition(b, |part| {
        part.role == "数据分区" && part.base.fs.eq_ignore_ascii_case("FAT32")
    });
    assert_eq!(part.base.size, 2048 * 1024 * 1024);
    selection
}

#[test]
#[ignore = "requires the isolated 4Kn guest, blank Test A/B and the verified ADK optical fixture"]
fn assembles_and_publishes_adk_winpe() {
    let (a, b) = pair();
    lab::verify_blank_disk(a, "DS-TEST-A-0001", A, 4096).unwrap();
    lab::verify_blank_disk(b, "DS-TEST-B-0001", B, 4096).unwrap();
    let roots: Vec<_> = ('C'..='Z')
        .filter_map(|letter| {
            let root = format!("{letter}:\\");
            let wide: Vec<_> = root.encode_utf16().chain([0]).collect();
            (unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) } == 5)
                .then(|| PathBuf::from(root).join("dsadk"))
                .filter(|path| path.join(r"en-us\winpe.wim").is_file())
        })
        .collect();
    let [optical] = roots.as_slice() else {
        panic!("Expected one ADK optical fixture")
    };
    lab::prepare_volume(a, 'R', "DSADKWORK").unwrap();
    lab::check_drive_letter('T').unwrap();
    let script = r"X:\ds-adk-part.txt";
    fs::write(script, format!(
        "select disk {b}\r\nconvert gpt\r\ncreate partition primary size=2048\r\nformat fs=fat32 quick label=DSWINPE\r\nassign letter=T\r\n"
    )).unwrap();
    assert!(Command::new(r"X:\Windows\System32\diskpart.exe")
        .args(["/s", script])
        .status()
        .unwrap()
        .success());
    let selection = adk_selection(b);
    let target = cap_std::fs::Dir::open_ambient_dir(r"T:\", cap_std::ambient_authority()).unwrap();
    let location = data_windows::file_location(&target.into_std_file()).unwrap();
    assert_eq!(location.disk_number(), b);
    assert_eq!(location.target, selection.target);

    let source = Path::new(r"R:\adk");
    let scratch = Path::new(r"R:\work");
    let app = Path::new(r"R:\DiskStudio.exe");
    fs::create_dir(source).unwrap();
    fs::create_dir(scratch).unwrap();
    copy_tree(optical, source);
    fs::copy(r"X:\DiskStudio.exe", app).unwrap();
    fs::copy(std::env::current_exe().unwrap(), r"R:\dshandoff.exe").unwrap();
    let result = crate::data_tasks::install_adk_for_lab(selection, source, scratch, app);
    println!(
        "Remaining staging entries: {}",
        scratch.read_dir().unwrap().count()
    );
    result.unwrap();
    assert!(scratch.read_dir().unwrap().next().is_none());
    assert!(Path::new(r"T:\sources\boot.wim").is_file());
    assert!(Path::new(r"T:\EFI\BOOT\BOOTX64.EFI").is_file());
    println!("WINPE ADK VERIFIED: all installed files read back, private scratch removed");
}

#[test]
#[ignore = "schedules firmware restart to the isolated Test B ADK WinPE partition"]
fn restarts_into_adk_winpe() {
    let (_, b) = pair();
    let before = inspect().unwrap();
    let plan = prepare_winpe(
        WinPeRequest {
            revision: before.revision.clone(),
            source: adk_selection(b),
            task: None,
        },
        PathBuf::from(r"X:\DiskStudio.exe"),
        None,
    )
    .unwrap();
    let result = winpe_restart(plan.token).unwrap();
    assert_eq!(result.report.order, before.order);
    assert_eq!(result.report.next, Some(plan.entry));
    assert!(result.error.is_none(), "{:?}", result.error);
}

#[test]
#[ignore = "verifies the installed ADK WIM after the isolated Test B boot"]
fn confirms_adk_winpe_boot() {
    let (_, b) = pair();
    let _ = adk_selection(b);
    confirm_handoff();
    let mut app = fs::File::open(r"X:\DiskStudio.exe").unwrap();
    let length = app.metadata().unwrap().len();
    let sha = crate::data_io::hash_exact(&mut app, length, |_| Ok(())).unwrap();
    println!("WINPE ADK PAYLOAD: SHA-256 {sha}");
}

const QUEUED_SOURCE: u64 = 128 * 1024 * 1024;
const QUEUED_OUTPUT: u64 = 1024 * 1024 * 1024;
const QUEUED_SAMPLE: &[u8] =
    b"DiskStudio WinPE reboot backup fixture 1b6406245ea24df88327d3dd95085788\r\n";

fn checked_lab_volume(letter: char, disk: u32, selection: &Selection) -> PathBuf {
    let root = PathBuf::from(format!("{letter}:\\"));
    let dir = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
    let resolved = data_windows::resolve(selection, true, false).unwrap();
    let location = data_windows::file_location(&dir).unwrap();
    assert_eq!(location.disk_number(), disk);
    location
        .confirm_partition(
            &selection.target,
            &selection.layout_key,
            &resolved.volumes[0],
            (disk, resolved.offset, resolved.size),
        )
        .unwrap();
    root
}

#[test]
#[ignore = "requires the isolated 4Kn A/B guest with blank disks and the verified optical payload"]
fn prepares_queued_backup() {
    let (a, b) = pair();
    lab::verify_blank_disk(a, "DS-TEST-A-0001", A, 4096).unwrap();
    lab::verify_blank_disk(b, "DS-TEST-B-0001", B, 4096).unwrap();
    for letter in ['S', 'T', 'U'] {
        lab::check_drive_letter(letter).unwrap();
    }
    let media: Vec<_> = ('C'..='Z')
        .filter_map(|letter| {
            let root = format!("{letter}:\\");
            let wide: Vec<_> = root.encode_utf16().chain([0]).collect();
            (unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) } == 5)
                .then(|| PathBuf::from(root).join("dspayload"))
                .filter(|path| path.join(r"sources\boot.wim").is_file())
        })
        .collect();
    let [optical] = media.as_slice() else {
        panic!("Expected one read-only WinPE media tree")
    };
    let script = r"X:\ds-queued-partitions.txt";
    fs::write(script, format!(concat!(
        "select disk {a}\r\nconvert gpt\r\ncreate partition primary size=128\r\n",
        "format fs=ntfs quick label=DSBKSRC\r\nassign letter=S\r\n",
        "select disk {b}\r\nconvert gpt\r\ncreate partition primary size=2048\r\n",
        "format fs=fat32 quick label=DSWINPE\r\nassign letter=T\r\n",
        "create partition primary size=1024\r\nformat fs=ntfs quick label=DSBKOUT\r\nassign letter=U\r\n"
    ), a=a, b=b)).unwrap();
    assert!(Command::new(r"X:\Windows\System32\diskpart.exe")
        .args(["/s", script])
        .status()
        .unwrap()
        .success());
    let (source, _) = select_partition(a, |p| p.base.size == QUEUED_SOURCE && p.base.fs == "NTFS");
    let (output, _) = select_partition(b, |p| p.base.size == QUEUED_OUTPUT && p.base.fs == "NTFS");
    let source_root = checked_lab_volume('S', a, &source);
    let output_root = checked_lab_volume('U', b, &output);
    fs::create_dir(output_root.join("backups")).unwrap();
    let boot = adk_selection(b);
    let boot_root = checked_lab_volume('T', b, &boot);
    fs::write(source_root.join("offline-fixture.txt"), QUEUED_SAMPLE).unwrap();
    copy_tree(optical, &boot_root);
    println!("WINPE QUEUED BACKUP FIXTURES READY");
}

#[test]
#[ignore = "arms the one-time boot and backup in the already provisioned isolated WinPE guest"]
fn schedules_queued_backup() {
    let (a, b) = pair();
    let boot = adk_selection(b);
    let (source, _) = select_partition(a, |p| p.base.size == QUEUED_SOURCE && p.base.fs == "NTFS");
    let (output, _) = select_partition(b, |p| p.base.size == QUEUED_OUTPUT && p.base.fs == "NTFS");
    let output_root = checked_lab_volume('U', b, &output);
    let (task, preview) = Backup::prepare(
        source,
        &boot.target,
        &output_root.join(r"backups\queued.dsbackup"),
    )
    .unwrap();
    let before = inspect().unwrap();
    let plan = prepare_winpe(
        WinPeRequest {
            revision: before.revision.clone(),
            source: boot,
            task: None,
        },
        PathBuf::from(r"X:\DiskStudio.exe"),
        Some((Task::Backup(task), preview)),
    )
    .unwrap();
    assert!(plan.task.is_some());
    assert_eq!(with_firmware(read_handoff).unwrap(), None);
    let result = winpe_restart(plan.token).unwrap();
    assert_eq!(result.report.next, Some(plan.entry));
    assert!(result.error.is_none(), "{:?}", result.error);
}

#[test]
#[ignore = "validates the completed archive after the automatic WinPE continuation"]
fn verifies_queued_backup() {
    let (_, b) = pair();
    confirm_handoff();
    assert!(claim_winpe_task().unwrap().is_none());
    let (selection, part) =
        select_partition(b, |p| p.base.size == QUEUED_OUTPUT && p.base.fs == "NTFS");
    let letter = part.drive_letter.unwrap().chars().next().unwrap();
    assert_ne!(letter, 'U');
    let root = checked_lab_volume(letter, b, &selection).join("backups");
    let archive =
        crate::backup_archive::Archive::open(fs::File::open(root.join("queued.dsbackup")).unwrap())
            .unwrap();
    let mut chain = crate::backup_archive::Chain::new(archive);
    assert_eq!(chain.header().kind, "partition");
    assert_eq!(chain.header().size, QUEUED_SOURCE);
    assert_eq!(chain.header().sector, 4096);
    chain.verify(|_| Ok(())).unwrap();
    let digest = chain.header().sha256.clone();
    let mut reader = chain.reader();
    let mut ntfs = ntfs::Ntfs::new(&mut reader).unwrap();
    ntfs.read_upcase_table(&mut reader).unwrap();
    assert_eq!(
        crate::boot_inspection::ntfs_file(&ntfs, &mut reader, &["offline-fixture.txt"], 4096)
            .unwrap()
            .unwrap(),
        QUEUED_SAMPLE
    );
    assert!(!root.join("queued.dsbackup.partial").exists());
    println!("WINPE QUEUED BACKUP VERIFIED: archive SHA-256 {digest}, NTFS file matches, ticket consumed");
}

const RESTORED_SAMPLE: &[u8] = b"Encrypted incremental 4Kn WinPE restore sample 4ed7652e\r\n";
const LAB_PASSWORD: &str = "restore-lab-key";

fn with_fixture_password<R>(work: impl FnOnce() -> R) -> R {
    use crate::{
        backup_encryption::Password,
        backup_secrets::{self, Answer, Purpose, Reply},
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    std::thread::scope(|scope| {
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let active = std::sync::Arc::clone(&done);
        let responder = scope.spawn(move || {
            while !active.load(Ordering::Relaxed) {
                if let Some(prompt) = backup_secrets::status() {
                    let value = Password::new(LAB_PASSWORD.into()).unwrap();
                    let confirmation = (prompt.purpose == Purpose::Create)
                        .then(|| Password::new(LAB_PASSWORD.into()).unwrap());
                    backup_secrets::submit(Reply {
                        id: prompt.id,
                        answer: Answer::Password {
                            value,
                            confirmation,
                        },
                    })
                    .unwrap();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        done.store(true, Ordering::Relaxed);
        responder.join().unwrap();
        outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

fn make_restore_archive(
    source: Selection,
    path: PathBuf,
    parent: Option<crate::backup_archive::Chain>,
) -> String {
    use crate::data_tasks::{self, BackupOptions, Work};
    let incremental = parent.is_some();
    let (id, cancel) = data_tasks::start("WinPE restore fixture").unwrap();
    let result = with_fixture_password(|| {
        data_tasks::run(
            &id,
            &cancel,
            Work::ArchiveBackup {
                source,
                path,
                parent,
                options: BackupOptions {
                    compression: true,
                    incremental,
                    encryption: incremental,
                    ..Default::default()
                },
            },
        )
    });
    let digest = result.as_ref().unwrap().1.clone().unwrap();
    data_tasks::finish(&id, result);
    digest
}

#[test]
#[ignore = "builds an encrypted incremental fixture on the verified isolated test disks"]
fn prepares_queued_restore() {
    prepares_queued_backup();
    let (a, b) = pair();
    let (source, _) = select_partition(a, |p| p.base.size == QUEUED_SOURCE && p.base.fs == "NTFS");
    let (storage, _) = select_partition(b, |p| p.base.size == QUEUED_OUTPUT && p.base.fs == "NTFS");
    let marker = checked_lab_volume('S', a, &source).join("offline-fixture.txt");
    let archive_directory = checked_lab_volume('U', b, &storage).join("backups");
    let parent_file = archive_directory.join("base.dsbackup");
    make_restore_archive(source.clone(), parent_file.clone(), None);
    fs::write(&marker, RESTORED_SAMPLE).unwrap();
    let parent = backup_files::Input::open(&parent_file, Some(&source.target))
        .unwrap()
        .archive(|| Ok(()))
        .unwrap()
        .unwrap();
    let expected = make_restore_archive(
        source,
        archive_directory.join("latest.dsbackup"),
        Some(crate::backup_archive::Chain::new(parent)),
    );
    fs::write(
        &marker,
        b"Target contents before restore, expected to be replaced.\r\n",
    )
    .unwrap();
    fs::write(archive_directory.join("expected.sha256"), expected).unwrap();
    println!("WINPE QUEUED RESTORE FIXTURE READY: encrypted incremental archive and separate original target");
}

#[test]
#[ignore = "arms the destructive restore in the verified isolated WinPE guest"]
fn schedules_queued_restore() {
    let (a, b) = pair();
    let media = adk_selection(b);
    let (destination, _) =
        select_partition(a, |p| p.base.size == QUEUED_SOURCE && p.base.fs == "NTFS");
    let (storage, _) = select_partition(b, |p| p.base.size == QUEUED_OUTPUT && p.base.fs == "NTFS");
    let root = checked_lab_volume('U', b, &storage).join("backups");
    let (task, preview) = with_fixture_password(|| {
        Restore::prepare(
            destination,
            &media.target,
            &root.join("latest.dsbackup"),
            |_| Some(root.join("base.dsbackup")),
        )
    })
    .unwrap()
    .unwrap();
    let before = inspect().unwrap();
    let plan = prepare_winpe(
        WinPeRequest {
            revision: before.revision,
            source: media,
            task: None,
        },
        PathBuf::from(r"X:\DiskStudio.exe"),
        Some((Task::Restore(task), preview)),
    )
    .unwrap();
    assert!(plan.task.as_ref().unwrap().destructive);
    let result = winpe_restart(plan.token).unwrap();
    assert_eq!(result.report.next, Some(plan.entry));
    assert!(result.error.is_none(), "{:?}", result.error);
}

#[test]
#[ignore = "independently checks the exact offline A partition after the queued restore"]
fn verifies_queued_restore() {
    let (a, b) = pair();
    confirm_handoff();
    assert!(claim_winpe_task().unwrap().is_none());
    let (storage, part) =
        select_partition(b, |p| p.base.size == QUEUED_OUTPUT && p.base.fs == "NTFS");
    let letter = part.drive_letter.unwrap().chars().next().unwrap();
    assert_ne!(letter, 'U');
    let root = checked_lab_volume(letter, b, &storage).join("backups");
    let expected = fs::read_to_string(root.join("expected.sha256")).unwrap();
    let disk = lab::verify_disk_geometry(a, "DS-TEST-A-0001", A, 4096).unwrap();
    lab::verify_offline(&disk).unwrap();
    let gpt = loader::checked_gpt(&disk, 16 * 1024 * 1024 * 1024, 4096).unwrap();
    let parts: Vec<_> = gpt
        .partitions
        .iter()
        .filter(|p| p.range.end - p.range.start == QUEUED_SOURCE)
        .collect();
    let [part] = parts.as_slice() else {
        panic!("Expected one restored test partition")
    };
    let mut volume = crate::used_space::View::bounded(
        disk,
        part.range.start,
        QUEUED_SOURCE,
        16 * 1024 * 1024 * 1024,
    )
    .unwrap();
    assert_eq!(
        crate::data_io::hash_exact(&mut volume, QUEUED_SOURCE, |_| Ok(())).unwrap(),
        expected
    );
    volume.seek(SeekFrom::Start(0)).unwrap();
    let mut ntfs = ntfs::Ntfs::new(&mut volume).unwrap();
    ntfs.read_upcase_table(&mut volume).unwrap();
    assert_eq!(
        crate::boot_inspection::ntfs_file(&ntfs, &mut volume, &["offline-fixture.txt"], 4096)
            .unwrap()
            .unwrap(),
        RESTORED_SAMPLE
    );
    println!("WINPE QUEUED RESTORE VERIFIED: offline target SHA-256 {expected}, restored file matches, ticket consumed");
}
