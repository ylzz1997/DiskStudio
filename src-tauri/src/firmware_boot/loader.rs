use super::{model, Loader, LoaderVerification};
use crate::{
    boot_inspection, data_io::Region, data_tasks::Selection, data_windows as device,
    fat_recovery::boot_files::Volume, image_browser, maintenance_media as media,
};
use disk_core::table_check::{GptIdentity, GptPartitionIdentity};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek},
    path::Path,
    sync::Mutex,
};
use uuid::Uuid;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};

const MAX_LOADER_SIZE: usize = 512 * 1024 * 1024;
const MAX_LOADER_HEADER: usize = 1024 * 1024;
const MAX_SECURITY_LOADER_SIZE: usize = 64 * 1024 * 1024;
static CANDIDATES: Mutex<BTreeMap<Uuid, Candidate>> = Mutex::new(BTreeMap::new());

#[derive(Clone)]
pub(super) struct Candidate {
    source: Selection,
    path: Vec<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Witness {
    bytes_sha256: [u8; 32],
    boot_sha256: [u8; 32],
    gpt_sha256: [u8; 32],
    disk: [u8; 16],
    partition: Uuid,
    slot: u32,
    start_lba: u64,
    blocks: u64,
    online: bool,
}

#[derive(PartialEq, Eq)]
struct Observation<T> {
    boot_sha256: [u8; 32],
    value: T,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct WinPeWitness {
    pub(super) media: super::WinPeMedia,
    pub(super) path: Vec<u8>,
    loader: Witness,
    bcd: [u8; 32],
    sdi: [u8; 32],
}

fn formatted(digest: [u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn checked_gpt(file: &File, size: u64, sector: u64) -> Result<GptIdentity, String> {
    image_browser::checked_layout(
        &mut Region::for_metadata(file, 0, size).map_err(device::err)?,
        "firmware-source.img",
        size,
        sector,
        &mut || Ok(()),
    )?
    .gpt_identity
    .ok_or_else(|| "EFI 启动项来源需要一致的主备 GPT。".into())
}

fn gpt_sha256(layout: &GptIdentity) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(layout.disk_guid);
    for entry in &layout.partitions {
        hash.update(entry.number.to_le_bytes());
        hash.update(entry.guid);
        hash.update(entry.kind);
        hash.update(entry.attributes.to_le_bytes());
        hash.update(entry.range.start.to_le_bytes());
        hash.update(entry.range.end.to_le_bytes());
    }
    hash.finalize().into()
}

fn observe_fat<T>(
    file: &File,
    entry: &GptPartitionIdentity,
    measure: &mut impl FnMut(&Volume, &mut Region<&File>) -> Result<T, String>,
) -> Result<Observation<T>, String> {
    let mut reader =
        Region::for_file_view(file, entry.range.start, entry.range.end - entry.range.start)
            .map_err(device::err)?;
    let volume = Volume::open(&mut reader)?;
    if volume.bits() != 32 {
        return Err("EFI 启动项来源需要 FAT32 EFI 系统分区。".into());
    }
    let value = measure(&volume, &mut reader)?;
    Ok(Observation {
        boot_sha256: volume.boot_sha256(),
        value,
    })
}

fn confirm_same_observation<T: PartialEq>(first: &T, second: T) -> Result<T, String> {
    if *first != second {
        return Err("两次读取的 EFI 分区或加载器不同，请重新读取。".into());
    }
    Ok(second)
}

fn with_volume<T: PartialEq, R>(
    source: &Selection,
    mut measure: impl FnMut(&Volume, &mut Region<&File>) -> Result<T, String>,
    finish: impl FnOnce(
        Observation<T>,
        &GptIdentity,
        &GptPartitionIdentity,
        u64,
        bool,
    ) -> Result<R, String>,
) -> Result<R, String> {
    let _operation = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    if source.partition.is_none() {
        return Err("请选择 GPT 磁盘的 EFI 系统分区。".into());
    }
    let whole_source = Selection {
        partition: None,
        ..source.clone()
    };
    let identity = device::resolve(&whole_source, false, false)?;
    if identity.disk.is_offline == Some(false) {
        return observe_online(
            source,
            device::OnlineBootSource::resolve(source)?,
            &mut measure,
            finish,
        );
    }

    let selected = device::resolve(source, false, false)?;
    let initial = device::resolve(&whole_source, true, false)?;
    if initial.disk.is_offline != Some(true) || initial.disk.partition_style != Some(2) {
        return Err("EFI 系统分区的磁盘状态已变化，请刷新后重选。".into());
    }
    let locks = device::state_locks(&initial)?;
    let whole = device::resolve_locked(&whole_source, false, &initial, &locks)?;
    let file = device::device(&whole, false)?;
    device::verify_offline_handle(&whole, &file)?;
    let sector = whole.disk.logical_sector_size.ok_or("逻辑扇区大小未知。")?;
    let layout = checked_gpt(&file, whole.size, sector)?;
    let entry = layout
        .partitions
        .iter()
        .find(|part| {
            part.kind == device::EFI_SYSTEM_TYPE.to_bytes_le()
                && part.range.start == selected.offset
                && part.range.end - part.range.start == selected.size
        })
        .ok_or("所选范围与 EFI 系统分区不一致。")?;
    whole.verify_boot_loader_layout(&file, &layout, Uuid::from_bytes_le(entry.guid))?;
    let observed = observe_fat(&file, entry, &mut measure)?;
    device::resolve_locked(&whole_source, false, &whole, &locks)?;
    finish(observed, &layout, entry, sector, false)
}

fn observe_online<T: PartialEq, R>(
    source: &Selection,
    source_disk: device::OnlineBootSource,
    measure: &mut impl FnMut(&Volume, &mut Region<&File>) -> Result<T, String>,
    finish: impl FnOnce(
        Observation<T>,
        &GptIdentity,
        &GptPartitionIdentity,
        u64,
        bool,
    ) -> Result<R, String>,
) -> Result<R, String> {
    let sector = source_disk.sector();
    let first_file = source_disk.open()?;
    let first_gpt = checked_gpt(&first_file, source_disk.size(), sector)?;
    let first_entry = source_disk.check_layout(&first_file, &first_gpt)?;
    let first = observe_fat(&first_file, first_entry, measure)?;

    // Reopen the device and FAT view for an independent observation.
    let second_file = source_disk.open()?;
    let second_gpt = checked_gpt(&second_file, source_disk.size(), sector)?;
    let second_entry = source_disk.check_layout(&second_file, &second_gpt)?;
    let second =
        confirm_same_observation(&first, observe_fat(&second_file, second_entry, measure)?)?;
    if first_gpt != second_gpt
        || checked_gpt(&second_file, source_disk.size(), sector)? != second_gpt
    {
        return Err("在线 EFI 分区的主备 GPT 在读取期间变化。".into());
    }
    source_disk.revalidate(source, &second_file)?;
    source_disk.check_layout(&second_file, &second_gpt)?;
    finish(second, &second_gpt, second_entry, sector, true)
}

pub(super) fn verify_winpe<T>(
    source: &Selection,
    executable: &Path,
    run: impl FnOnce(WinPeWitness) -> Result<T, String>,
) -> Result<T, String> {
    let _operation = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let mut executable = File::open(executable).map_err(device::err)?;
    let mut hash = Sha256::new();
    let count = io::copy(
        &mut (&mut executable).take(MAX_LOADER_SIZE as u64 + 1),
        &mut hash,
    )
    .map_err(device::err)?;
    if count == 0 || count > MAX_LOADER_SIZE as u64 {
        return Err("DiskStudio 程序超出启动介质的文件大小限制。".into());
    }
    let app: [u8; 32] = hash.finalize().into();
    let loader_path: Vec<String> = media::FALLBACK.split('/').map(str::to_owned).collect();
    observe_online(
        source,
        device::OnlineBootSource::winpe(source)?,
        &mut |volume, reader| {
            let loader = volume
                .read_file(
                    reader,
                    &["efi", "boot", "bootx64.efi"],
                    MAX_SECURITY_LOADER_SIZE,
                )?
                .ok_or("找不到 WinPE 的 EFI 加载器。")?;
            media::check_fallback(&loader)?;
            let bcd = volume
                .read_file(
                    reader,
                    &["efi", "microsoft", "boot", "bcd"],
                    16 * 1024 * 1024,
                )?
                .ok_or("找不到 WinPE 的 EFI BCD。")?;
            let bcd_sha = Sha256::digest(&bcd).into();
            crate::boot_configuration::verify_winpe(bcd, false)?;
            let (sdi_sha, sdi, _) = volume
                .measure_file(reader, &["boot", "boot.sdi"], 16 * 1024 * 1024, 8)?
                .ok_or("找不到 WinPE 的 boot.sdi。")?;
            if sdi != b"$SDI0001" {
                return Err("WinPE 的 boot.sdi 文件头无效。".into());
            }
            let (wim_sha, header, wim_bytes) = volume
                .measure_file(reader, &["sources", "boot.wim"], u32::MAX as usize, 208)?
                .ok_or("找不到 sources\\boot.wim。")?;
            let wim_image = media::wim_index(&header, wim_bytes)?;
            let (app_sha, _, _) = volume
                .measure_file(reader, &[media::APP_PATH], MAX_LOADER_SIZE, 0)?
                .ok_or("WinPE 分区中缺少 DiskStudio.exe。")?;
            if app_sha != app {
                return Err("WinPE 分区中的 DiskStudio.exe 与当前程序不一致。".into());
            }
            Ok((
                <[u8; 32]>::from(Sha256::digest(&loader)),
                bcd_sha,
                sdi_sha,
                wim_sha,
                wim_bytes,
                wim_image,
            ))
        },
        |observed, layout, entry, sector, online| {
            let (loader_sha, bcd, sdi, wim, wim_bytes, wim_image) = observed.value;
            let partition = Uuid::from_bytes_le(entry.guid);
            let witness = Witness {
                bytes_sha256: loader_sha,
                boot_sha256: observed.boot_sha256,
                gpt_sha256: gpt_sha256(layout),
                disk: layout.disk_guid,
                partition,
                slot: entry.number,
                start_lba: entry.range.start / sector,
                blocks: (entry.range.end - entry.range.start) / sector,
                online,
            };
            let path = model::gpt_device_path(
                &loader_path,
                witness.slot,
                partition,
                witness.start_lba,
                witness.blocks,
            )?;
            run(WinPeWitness {
                media: super::WinPeMedia {
                    partition,
                    wim_bytes,
                    wim_image,
                    wim_sha256: formatted(wim),
                    app_sha256: formatted(app),
                },
                path,
                loader: witness,
                bcd,
                sdi,
            })
        },
    )
}

pub(super) fn list(source: Selection) -> Result<Vec<Loader>, String> {
    let paths = with_volume(
        &source,
        |volume, reader| volume.efi_files(reader, MAX_LOADER_SIZE),
        |observed, _, _, _, _| Ok(observed.value),
    )?;
    let mut retained = BTreeMap::new();
    let entries = paths
        .into_iter()
        .map(|path| {
            let id = Uuid::new_v4();
            let label = format!("\\{}", path.join("\\"));
            retained.insert(
                id,
                Candidate {
                    source: source.clone(),
                    path,
                },
            );
            Loader { id, path: label }
        })
        .collect();
    *CANDIDATES.lock().map_err(|_| "EFI 文件列表已失效。")? = retained;
    Ok(entries)
}

pub(super) fn candidate(id: Uuid) -> Result<Candidate, String> {
    CANDIDATES
        .lock()
        .map_err(|_| "EFI 文件列表已失效。")?
        .get(&id)
        .cloned()
        .ok_or_else(|| "EFI 文件列表已失效，请重新读取。".into())
}

impl Candidate {
    pub(super) fn security_source(&self) -> Result<(Vec<u8>, Uuid), String> {
        let path: Vec<_> = self.path.iter().map(String::as_str).collect();
        with_volume(
            &self.source,
            |volume, reader| {
                let bytes = volume
                    .read_file(reader, &path, MAX_SECURITY_LOADER_SIZE)?
                    .ok_or("EFI 加载器已移动或不存在。")?;
                verify_machine(&bytes)?;
                Ok(bytes)
            },
            |observed, _, entry, _, _| Ok((observed.value, Uuid::from_bytes_le(entry.guid))),
        )
    }

    pub(super) fn verification(&self) -> Result<LoaderVerification, String> {
        self.verify(|witness| {
            Ok(LoaderVerification {
                sha256: witness
                    .bytes_sha256
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                partition: witness.partition,
            })
        })
    }

    pub(super) fn verify<T>(
        &self,
        run: impl FnOnce(Witness) -> Result<T, String>,
    ) -> Result<T, String> {
        with_volume(
            &self.source,
            |volume, reader| loader_sha256(volume, reader, &self.path),
            |observed, layout, entry, sector, online| {
                run(Witness {
                    bytes_sha256: observed.value,
                    boot_sha256: observed.boot_sha256,
                    gpt_sha256: gpt_sha256(layout),
                    disk: layout.disk_guid,
                    partition: Uuid::from_bytes_le(entry.guid),
                    slot: entry.number,
                    start_lba: entry.range.start / sector,
                    blocks: (entry.range.end - entry.range.start) / sector,
                    online,
                })
            },
        )
    }

    pub(super) fn device_path(&self, witness: &Witness) -> Result<Vec<u8>, String> {
        model::gpt_device_path(
            &self.path,
            witness.slot,
            witness.partition,
            witness.start_lba,
            witness.blocks,
        )
    }
}

fn loader_sha256(
    volume: &Volume,
    reader: &mut (impl Read + Seek),
    path: &[String],
) -> Result<[u8; 32], String> {
    let path: Vec<_> = path.iter().map(String::as_str).collect();
    let (digest, prefix) = volume
        .file_sha256_and_prefix(reader, &path, MAX_LOADER_SIZE, MAX_LOADER_HEADER)?
        .ok_or("EFI 加载器已移动或不存在。")?;
    verify_machine(&prefix)?;
    Ok(digest)
}

fn verify_machine(bytes: &[u8]) -> Result<(), String> {
    let machine = boot_inspection::pe_machine(bytes, &[10])?;
    let mut process = 0;
    let mut native = 0;
    if unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) } == 0
        || machine != native
    {
        return Err("EFI 加载器的架构与本机 Windows 不匹配。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_source_rejects_changed_catalog_bpb_or_loader_digest() {
        let first = Observation {
            boot_sha256: [1; 32],
            value: vec!["BOOTX64.EFI"],
        };
        assert!(confirm_same_observation(
            &first,
            Observation {
                boot_sha256: [1; 32],
                value: vec!["BOOTX64.EFI"],
            }
        )
        .is_ok());
        assert!(confirm_same_observation(
            &first,
            Observation {
                boot_sha256: [1; 32],
                value: vec!["BOOTX64.EFI", "BOOTAA64.EFI"],
            }
        )
        .is_err());
        let first = Observation {
            boot_sha256: [1; 32],
            value: [2; 32],
        };
        assert!(confirm_same_observation(
            &first,
            Observation {
                boot_sha256: [3; 32],
                value: [2; 32],
            }
        )
        .is_err());
        assert!(confirm_same_observation(
            &first,
            Observation {
                boot_sha256: [1; 32],
                value: [3; 32],
            }
        )
        .is_err());
    }

    #[test]
    fn boot_witness_binds_non_target_gpt_attributes_and_raw_slot_number() {
        let mut layout = GptIdentity {
            disk_guid: [1; 16],
            partitions: vec![
                GptPartitionIdentity {
                    number: 2,
                    guid: [2; 16],
                    kind: device::EFI_SYSTEM_TYPE.to_bytes_le(),
                    attributes: 0,
                    range: 4_096..8_192,
                },
                GptPartitionIdentity {
                    number: 9,
                    guid: [3; 16],
                    kind: [4; 16],
                    attributes: 0,
                    range: 8_192..12_288,
                },
            ],
        };
        let baseline = gpt_sha256(&layout);
        layout.partitions[1].attributes ^= 1 << 61;
        assert_ne!(gpt_sha256(&layout), baseline);
        layout.partitions[1].attributes = 0;
        layout.partitions[0].number = 3;
        assert_ne!(gpt_sha256(&layout), baseline);
    }
}
