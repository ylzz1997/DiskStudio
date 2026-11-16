//! Fresh directory metadata for output preservation. Proofs stay in Rust and
//! cannot be supplied by the UI or an imported display-metadata record.
use crate::{file_metadata::Metadata, recovery_tree::Directory};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
#[cfg(test)]
#[path = "directory_metadata_tests.rs"]
mod tests;

#[derive(Clone, Debug)]
pub(crate) enum Proof {
    Ntfs {
        boot: [u8; 32],
        record: u64,
        sequence: u16,
        name: crate::ntfs_names::Name,
    },
    Fat(crate::fat_recovery::Location),
    Ext(crate::ext_recovery::DirectoryLocation),
}
pub(crate) fn boot(reader: &mut (impl Read + Seek)) -> Result<[u8; 32], String> {
    reader.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut bytes = [0; 512];
    reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    Ok(Sha256::digest(bytes).into())
}
pub(crate) fn read<R: Read + Seek + 'static>(
    mut reader: R,
    directory: &Directory,
    cancel: Arc<AtomicBool>,
) -> Result<Metadata, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("目录属性读取已取消。".into());
    }
    let Some(proof) = directory.metadata.directory_proof.as_ref() else {
        // Saved NTFS progress retains record/sequence and graph identities, but
        // never executable metadata. Re-read the actual record before copying.
        return read_saved_ntfs(&mut reader, directory);
    };
    match proof {
        Proof::Ntfs {
            boot: expected,
            record,
            sequence,
            name,
        } => {
            if &boot(&mut reader)? != expected {
                return Err("NTFS 卷身份已变化，未保留目录属性。".into());
            }
            let ntfs = ntfs::Ntfs::new(&mut reader).map_err(|e| e.to_string())?;
            let file = ntfs.file(&mut reader, *record).map_err(|e| e.to_string())?;
            if !file.is_directory()
                || file.sequence_number() != *sequence
                || !file.flags().contains(ntfs::NtfsFileFlags::IN_USE) != directory.deleted
                || crate::ntfs_names::directory(&mut reader, &file)? != *name
                || name.display() != directory.name
            {
                return Err("NTFS 目录身份、名称或状态已改变。".into());
            }
            Ok(crate::file_metadata::ntfs(&file))
        }
        Proof::Fat(location) => {
            crate::fat_recovery::directory_metadata(&mut reader, location, &directory.name)
        }
        Proof::Ext(location) => {
            crate::ext_recovery::directory_metadata(reader, location, &directory.name, cancel)
        }
    }
}

fn read_saved_ntfs(
    reader: &mut (impl Read + Seek),
    directory: &Directory,
) -> Result<Metadata, String> {
    let (record, sequence) = directory
        .id
        .strip_prefix("ntfs-")
        .and_then(|id| id.split_once('-'))
        .and_then(|(r, s)| Some((r.parse::<u64>().ok()?, s.parse::<u16>().ok()?)))
        .filter(|(r, s)| *r < 1 << 48 && *r != 5 && directory.id == format!("ntfs-{r}-{s}"))
        .ok_or("此目录没有可复核的目录记录。")?;
    let ntfs = ntfs::Ntfs::new(reader).map_err(|e| e.to_string())?;
    let file = ntfs.file(reader, record).map_err(|e| e.to_string())?;
    let name = crate::ntfs_names::directory(reader, &file)?;
    let utf16: Vec<u16> = name
        .raw
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let exact_name =
        String::from_utf16(&utf16).map_err(|_| "保存目录名称不能无损复核，请重新读取目录。")?;
    let parent = if name.parent.0 == 5 {
        crate::recovery_tree::ROOT.into()
    } else {
        format!("ntfs-{}-{}", name.parent.0, name.parent.1)
    };
    if !file.is_directory()
        || file.sequence_number() != sequence
        || !file.flags().contains(ntfs::NtfsFileFlags::IN_USE) != directory.deleted
        || exact_name != directory.name
        || directory.parent.as_deref() != Some(&parent)
    {
        return Err("保存目录的身份、名称、父目录或状态已改变。".into());
    }
    Ok(crate::file_metadata::ntfs(&file))
}
