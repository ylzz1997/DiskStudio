//! Create genuine encrypted files by the Windows API on a bound A volume.
use super::*;
use std::{io, os::windows::ffi::OsStrExt};
use windows_sys::Win32::Storage::FileSystem::{EncryptFileW, FileEncryptionStatusW};

const DIRECTORY: &str = "efs-directory";
const FILE: &str = "native-efs.bin";
const ADS: &str = "alternate";
const EMPTY: &str = "empty";

fn bytes(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251 + 1) as u8).collect()
}

fn encrypt(path: &Path) -> Result<(), String> {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    // SAFETY: the terminated string stays live through both synchronous calls.
    if unsafe { EncryptFileW(wide.as_ptr()) } == 0 {
        return Err(format!(
            "Native EncryptFileW failed: {}",
            io::Error::last_os_error()
        ));
    }
    let mut status = 0;
    // SAFETY: Windows writes one ULONG and both pointers are valid.
    if unsafe { FileEncryptionStatusW(wide.as_ptr(), &mut status) } == 0 || status != 1 {
        return Err(format!(
            "Expected native FILE_IS_ENCRYPTED status, got {status}"
        ));
    }
    Ok(())
}

pub(super) fn seed(selected: &Selection, source: &data_windows::Resolved) -> Result<(), String> {
    let [volume] = source.volumes.as_slice() else {
        return Err("Expected exactly one fixture volume for Windows EFS".into());
    };
    let root = PathBuf::from(format!("{}\\", data_windows::volume_root(volume)?));
    let confirm = |file: &fs::File| {
        data_windows::file_location(file)?.confirm_partition(
            &selected.target,
            &selected.layout_key,
            volume,
            (source.disk.number, source.offset, source.size),
        )
    };
    let check_directory = |path: &Path| {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(err)?;
        confirm(&file)
    };
    check_directory(&root)?;
    let dir = root.join(DIRECTORY);
    fs::create_dir(&dir).map_err(err)?;
    check_directory(&dir)?;

    let ordinary = dir.join(FILE);
    let named = dir.join(format!("{FILE}:{ADS}"));
    let empty = dir.join(format!("{FILE}:{EMPTY}"));
    let streams = [
        (ordinary.as_path(), bytes(65_553)),
        (&named, bytes(1_699)),
        (&empty, vec![]),
    ];
    for (path, data) in &streams {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(err)?;
        confirm(&file)?;
        file.write_all(data).map_err(err)?;
        file.sync_all().map_err(err)?;
    }
    encrypt(&ordinary)?;
    encrypt(&dir)?;
    for (path, data) in &streams {
        let file = OpenOptions::new().read(true).open(path).map_err(err)?;
        confirm(&file)?;
        let mut logical = Vec::new();
        file.take(data.len() as u64 + 1)
            .read_to_end(&mut logical)
            .map_err(err)?;
        if logical != *data {
            return Err("Native EFS logical readback differs under the original account".into());
        }
    }
    Ok(())
}

pub(super) fn sample(found: &partitions::merge::Catalogue) -> Result<(), String> {
    use partitions::merge::metadata;
    let dir = DIRECTORY.encode_utf16().collect::<Vec<_>>();
    let id = found
        .names
        .get(std::slice::from_ref(&dir))
        .ok_or("Missing native EFS directory")?;
    let directory = &found.nodes[id];
    if !directory.directory || directory.attributes & metadata::ENCRYPTED_ATTRIBUTE == 0 {
        return Err("Native EFS directory inheritance differs".into());
    }
    let id = found
        .names
        .get(&vec![dir, FILE.encode_utf16().collect()])
        .ok_or("Missing native EFS sample")?;
    let file = &found.nodes[id];
    let keys = file
        .metadata
        .get(&metadata::EFS)
        .ok_or("Native $EFS stream is missing")?;
    metadata::validate_efs(keys)?;
    if file.attributes & metadata::ENCRYPTED_ATTRIBUTE == 0 || file.streams.len() != 3 {
        return Err("Native EFS file flags or stream count differ".into());
    }
    for (name, size) in [("", 65_553u64), (ADS, 1_699), (EMPTY, 0)] {
        let utf16 = name
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let stream = file
            .streams
            .iter()
            .find(|s| s.locator.name == utf16)
            .ok_or("A native EFS data stream is missing")?;
        let mut padded_plaintext = bytes(size as usize);
        padded_plaintext.resize(
            ntfs_streams::payload_size(size, ntfs_streams::ENCRYPTED) as usize,
            0,
        );
        if stream.size != size
            || stream.storage_flags != ntfs_streams::ENCRYPTED
            || (size != 0 && stream.sha256 == <[u8; 32]>::from(Sha256::digest(padded_plaintext)))
        {
            return Err("Native EFS data stream length, flags or ciphertext differs".into());
        }
    }
    Ok(())
}
