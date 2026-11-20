//! Native WOF fixtures on the same checked source volume as the other clones.
use super::*;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::{Foundation::HANDLE, System::Ioctl::FSCTL_GET_REPARSE_POINT};

const PROVIDER_FILE: u32 = 2;
const COMMENT: &[u8] = b"ordinary alternate stream on a WOF file";

#[link(name = "Wofutil", kind = "raw-dylib")]
extern "system" {
    fn WofGetDriverVersion(file: HANDLE, provider: u32, version: *mut u32) -> i32;
    fn WofSetFileDataLocation(
        file: HANDLE,
        provider: u32,
        info: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Evidence {
    algorithm: u32,
    bytes: u64,
    logical_sha256: [u8; 32],
    reparse: Vec<u8>,
}

fn filename(algorithm: u32) -> String {
    let label = ["xpress4k", "lzx", "xpress8k", "xpress16k"][algorithm as usize];
    format!("native-wof-{label}.bin")
}

fn payload(algorithm: u32) -> Vec<u8> {
    (0..(192 * 1024 + 123))
        .map(|index| ((index / 113 + index % 41 + algorithm as usize * 17) % 251) as u8)
        .collect()
}

pub(super) fn seed(
    selected: &Selection,
    source: &data_windows::Resolved,
) -> Result<Vec<Evidence>, String> {
    let [volume] = source.volumes.as_slice() else {
        return Err("Expected one WOF fixture volume".into());
    };
    let volume_root = data_windows::volume_root(volume)?;
    let root = PathBuf::from(format!("{volume_root}\\"));
    let bind = |file: &fs::File| {
        data_windows::file_location(file)?.confirm_partition(
            &selected.target,
            &selected.layout_key,
            volume,
            (source.disk.number, source.offset, source.size),
        )
    };
    let root_handle = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&root)
        .map_err(err)?;
    bind(&root_handle)?;

    let mut evidence = Vec::new();
    for algorithm in [1, 2, 3, 0] {
        let name = filename(algorithm);
        let path = root.join(&name);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(err)?;
        bind(&file)?;
        let expected = payload(algorithm);
        file.write_all(&expected).map_err(err)?;
        file.sync_all().map_err(err)?;
        if algorithm == 0 {
            let mut comment = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join(format!("{name}:comment")))
                .map_err(err)?;
            bind(&comment)?;
            comment.write_all(COMMENT).map_err(err)?;
            comment.sync_all().map_err(err)?;
        }
        let mut info = [algorithm, 0u32];
        // SAFETY: the checked handle and Windows SDK's two ULONG inputs stay
        // alive for the duration of this synchronous call.
        let status = unsafe {
            WofSetFileDataLocation(
                file.as_raw_handle(),
                PROVIDER_FILE,
                info.as_mut_ptr().cast(),
                std::mem::size_of_val(&info) as u32,
            )
        };
        if status != 0 {
            let mut version = 0;
            // SAFETY: Windows writes one ULONG; the bound file stays open.
            let probe =
                unsafe { WofGetDriverVersion(file.as_raw_handle(), PROVIDER_FILE, &mut version) };
            return Err(format!(
                "Windows WOF compression failed for {name}: 0x{status:08x}\nWOF driver query on the same file: 0x{probe:08x} (version {version})"
            ));
        }
        drop(file);

        let raw = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .map_err(err)?;
        bind(&raw)?;
        let mut reparse = vec![0; 16 * 1024];
        let size = data_windows::control(&raw, FSCTL_GET_REPARSE_POINT, &[], &mut reparse)?;
        reparse.truncate(size as usize);
        drop(raw);
        if reparse.get(20..24) != Some(&algorithm.to_le_bytes()) {
            return Err(format!(
                "Windows returned a different WOF algorithm for {name}"
            ));
        }
        let logical = OpenOptions::new().read(true).open(&path).map_err(err)?;
        bind(&logical)?;
        let mut observed = Vec::new();
        logical
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut observed)
            .map_err(err)?;
        if observed != expected {
            return Err(format!("Windows logical WOF readback differs for {name}"));
        }
        evidence.push(Evidence {
            algorithm,
            bytes: observed.len() as u64,
            logical_sha256: Sha256::digest(&observed).into(),
            reparse,
        });
    }
    evidence.sort_by_key(|row| row.algorithm);
    Ok(evidence)
}

pub(super) fn sample(
    catalogue: &partitions::merge::Catalogue,
    evidence: &[Evidence],
) -> Result<(), String> {
    use partitions::merge::metadata;

    if evidence.len() != 4 {
        return Err("Expected four native WOF files".into());
    }
    for (algorithm, sample) in (0..4).zip(evidence) {
        let name = filename(algorithm);
        let id = catalogue
            .names
            .get(&vec![name.encode_utf16().collect()])
            .ok_or_else(|| format!("Missing native WOF file {name}"))?;
        let node = &catalogue.nodes[id];
        let named = |name: &[u8]| {
            node.streams
                .iter()
                .find(|stream| stream.locator.name == name)
        };
        let expected = payload(algorithm);
        if sample.algorithm != algorithm
            || sample.bytes != expected.len() as u64
            || sample.logical_sha256 != <[u8; 32]>::from(Sha256::digest(expected))
            || node.metadata.get(&metadata::REPARSE) != Some(&sample.reparse)
            || !named(&[]).is_some_and(|stream| stream.size == sample.bytes)
            || !named(metadata::WOF_STREAM).is_some_and(|stream| stream.size < sample.bytes)
            || algorithm == 0
                && !named(b"c\0o\0m\0m\0e\0n\0t\0").is_some_and(|stream| {
                    stream.sha256 == <[u8; 32]>::from(Sha256::digest(COMMENT))
                })
        {
            return Err(format!("Native WOF data or metadata differs for {name}"));
        }
    }
    Ok(())
}
