//! Native NTFS object ID and link fixtures on a verified A partition.
use super::*;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_CREATE_OR_GET_OBJECT_ID, FSCTL_GET_OBJECT_ID, FSCTL_GET_REPARSE_POINT,
    FSCTL_SET_REPARSE_POINT,
};

const FILE: &str = "identity.bin";
const HARD_LINK: &str = "identity-alias.bin";
const SYMBOLIC_LINK: &str = "relative-link.bin";
const DIRECTORY: &str = "identity-directory";
const JUNCTION: &str = "identity-junction";
const SYMLINK_TAG: u32 = 0xa000000c;
const MOUNT_POINT_TAG: u32 = 0xa0000003;
const CONTENTS: &[u8] = b"NTFS object identity fixture";

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Evidence {
    file: Vec<u8>,
    directory: Vec<u8>,
}

fn create_identity(file: &fs::File) -> Result<Vec<u8>, String> {
    let mut expected = vec![0; 64];
    if data_windows::control(file, FSCTL_CREATE_OR_GET_OBJECT_ID, &[], &mut expected)? != 64
        || expected[..16] == [0; 16]
    {
        return Err("Windows did not create a complete object ID".into());
    }
    let mut returned = [0; 64];
    if data_windows::control(file, FSCTL_GET_OBJECT_ID, &[], &mut returned)? != 64
        || returned.as_slice() != expected.as_slice()
    {
        return Err("Windows object ID readback differs".into());
    }
    Ok(expected)
}

fn junction_data(substitute: &str, display: &str) -> Vec<u8> {
    let substitute = substitute
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let display = display
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let length = (8 + substitute.len() + display.len() + 4) as u16;
    let mut data = MOUNT_POINT_TAG.to_le_bytes().to_vec();
    data.extend_from_slice(&length.to_le_bytes());
    data.extend([0; 4]);
    data.extend_from_slice(&(substitute.len() as u16).to_le_bytes());
    data.extend_from_slice(&((substitute.len() + 2) as u16).to_le_bytes());
    data.extend_from_slice(&(display.len() as u16).to_le_bytes());
    data.extend_from_slice(&substitute);
    data.extend([0; 2]);
    data.extend_from_slice(&display);
    data.extend([0; 2]);
    data
}

fn read_link(file: &fs::File) -> Result<Vec<u8>, String> {
    let mut data = vec![0; 16 * 1024];
    let size = data_windows::control(file, FSCTL_GET_REPARSE_POINT, &[], &mut data)?;
    data.truncate(size as usize);
    Ok(data)
}

pub(super) fn seed(
    selected: &Selection,
    source: &data_windows::Resolved,
) -> Result<Evidence, String> {
    let [volume] = source.volumes.as_slice() else {
        return Err("Expected one A fixture volume".into());
    };
    let volume_root = data_windows::volume_root(volume)?;
    let volume_id = volume_root
        .strip_prefix(r"\\?\")
        .filter(|name| name.starts_with("Volume{") && name.ends_with('}'))
        .ok_or("Unexpected fixture volume root")?;
    let root = PathBuf::from(format!("{volume_root}\\"));
    let bind = |file: &fs::File| {
        data_windows::file_location(file)?.confirm_partition(
            &selected.target,
            &selected.layout_key,
            volume,
            (source.disk.number, source.offset, source.size),
        )
    };
    let open = |path: &Path, directory: bool, writable: bool, create: bool| {
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            };
        let handle = OpenOptions::new()
            .read(true)
            .write(writable)
            .create_new(create)
            .custom_flags(flags)
            .open(path)
            .map_err(err)?;
        bind(&handle)?;
        Ok::<_, String>(handle)
    };
    let _root = open(&root, true, false, false)?;
    let mut file = open(&root.join(FILE), false, true, true)?;
    file.write_all(CONTENTS).map_err(err)?;
    file.sync_all().map_err(err)?;
    drop(file);
    fs::hard_link(root.join(FILE), root.join(HARD_LINK)).map_err(err)?;
    let file_id = create_identity(&open(&root.join(FILE), false, true, false)?)?;

    let directory = root.join(DIRECTORY);
    fs::create_dir(&directory).map_err(err)?;
    let directory_id = create_identity(&open(&directory, true, true, false)?)?;

    let junction = root.join(JUNCTION);
    fs::create_dir(&junction).map_err(err)?;
    let junction = open(&junction, true, true, false)?;
    let expected = junction_data(
        &format!(r"\??\{volume_id}\{DIRECTORY}"),
        &format!(r"{volume_root}\{DIRECTORY}"),
    );
    data_windows::control(&junction, FSCTL_SET_REPARSE_POINT, &expected, &mut [])?;
    if read_link(&junction)? != expected {
        return Err("Windows junction readback differs".into());
    }

    let link_path = root.join(SYMBOLIC_LINK);
    std::os::windows::fs::symlink_file(FILE, &link_path).map_err(err)?;
    let link = read_link(&open(&link_path, false, false, false)?)?;
    if !matches_relative_link(&link)? {
        return Err("Windows relative symbolic link readback differs".into());
    }
    Ok(Evidence {
        file: file_id,
        directory: directory_id,
    })
}

fn matches_relative_link(data: &[u8]) -> Result<bool, String> {
    if partitions::merge::metadata::reparse_tag(data, false)? != SYMLINK_TAG
        || data[16..20] != 1u32.to_le_bytes()
    {
        return Ok(false);
    }
    let start = 20 + usize::from(u16::from_le_bytes(data[8..10].try_into().unwrap()));
    let size = usize::from(u16::from_le_bytes(data[10..12].try_into().unwrap()));
    Ok(data[start..start + size]
        .iter()
        .copied()
        .eq(FILE.encode_utf16().flat_map(u16::to_le_bytes)))
}

pub(super) fn sample(
    catalogue: &partitions::merge::Catalogue,
    evidence: &Evidence,
) -> Result<(), String> {
    use partitions::merge::metadata::{self, OBJECT_ID, REPARSE};
    let id = |name: &str| {
        catalogue
            .names
            .get(&vec![name.encode_utf16().collect()])
            .copied()
            .ok_or_else(|| format!("Missing NTFS link fixture {name}"))
    };
    let file = id(FILE)?;
    let node = &catalogue.nodes[&file];
    let directory = &catalogue.nodes[&id(DIRECTORY)?];
    let symlink = &catalogue.nodes[&id(SYMBOLIC_LINK)?];
    let junction = &catalogue.nodes[&id(JUNCTION)?];
    if id(HARD_LINK)? != file
        || evidence.file.len() != 64
        || evidence.directory.len() != 64
        || node.metadata.get(&OBJECT_ID) != Some(&evidence.file)
        || directory.metadata.get(&OBJECT_ID) != Some(&evidence.directory)
        || !node.streams.iter().any(|stream| {
            stream.locator.name.is_empty()
                && stream.sha256 == <[u8; 32]>::from(Sha256::digest(CONTENTS))
        })
        || !matches_relative_link(
            symlink
                .metadata
                .get(&REPARSE)
                .ok_or("Missing symbolic link data")?,
        )?
        || metadata::reparse_tag(
            junction
                .metadata
                .get(&REPARSE)
                .ok_or("Missing junction data")?,
            true,
        )? != MOUNT_POINT_TAG
    {
        return Err("Native object ID or link fixture differs from its expected data".into());
    }
    Ok(())
}
