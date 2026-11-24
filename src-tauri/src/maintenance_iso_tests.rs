use super::*;
use std::io::Cursor;

fn worker(efi: Vec<u8>, bios: Option<Vec<u8>>) -> Vec<u8> {
    let inputs = [
        ("efi/boot/bootx64.efi", vec![0xab; 3991]),
        ("boot/locale-zh/boot.sdi", vec![0xec; 17]),
        ("sources/boot.wim", vec![0x51; 4100]),
        ("帮助/引导.txt", b"README".to_vec()),
    ];
    let members = inputs
        .iter()
        .map(|(path, file)| Member {
            path: (*path).into(),
            size: file.len() as u32,
        })
        .collect::<Vec<_>>();
    let image = Image::prepare(&members, &efi, bios.as_deref()).unwrap();
    let mut output = Vec::new();
    image
        .write(&mut output, |i, n, out| {
            assert_eq!(n as usize, inputs[i].1.len());
            out.write_all(&inputs[i].1).map_err(|err| err.to_string())
        })
        .unwrap();
    image.verify_prefix(&mut Cursor::new(&output)).unwrap();
    for ((offset, size), (_, input)) in image.file_extents().iter().zip(inputs) {
        let at = *offset as usize;
        assert_eq!(&output[at..at + *size as usize], input);
    }
    let primary = &output[16 * BLOCK..17 * BLOCK];
    let joliet = &output[18 * BLOCK..19 * BLOCK];
    assert_eq!(&primary[..7], b"\x01CD001\x01");
    assert_eq!(&joliet[88..91], b"%/E");
    assert_eq!(
        &output[17 * BLOCK + 7..17 * BLOCK + 30],
        b"EL TORITO SPECIFICATION"
    );
    assert_eq!(output.len() as u64, image.size);
    let mut damaged = output.clone();
    damaged[BOOT_CATALOG * BLOCK + 30] ^= 1;
    assert!(image.verify_prefix(&mut Cursor::new(damaged)).is_err());
    output
}

#[test]
fn verifies_separate_uefi_and_bios_catalog_entries_and_payloads() {
    assert_eq!(primary("bootmgr", false).as_deref(), Some("BOOTMGR"));
    assert_eq!(primary("bootmgr.efi", false).as_deref(), Some("BOOTMGR.EFI"));
    let efi = vec![0x93; 2048];
    let bios = vec![0x86; 4096];
    for include_bios in [false, true] {
        let output = worker(efi.clone(), include_bios.then(|| bios.clone()));
        let catalog = &output[BOOT_CATALOG * BLOCK..(BOOT_CATALOG + 1) * BLOCK];
        assert_eq!(catalog[1], if include_bios { 0 } else { 0xef });
        assert_eq!(
            catalog[..32]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .fold(0u16, u16::wrapping_add),
            0
        );
        let extract = |entry: usize| {
            assert_eq!(&catalog[entry..entry + 2], &[0x88, 0]);
            let sectors =
                u16::from_le_bytes(catalog[entry + 6..entry + 8].try_into().unwrap()) as usize;
            let start = u32::from_le_bytes(catalog[entry + 8..entry + 12].try_into().unwrap())
                as usize
                * BLOCK;
            &output[start..start + sectors * 512]
        };
        if include_bios {
            assert_eq!(&catalog[64..68], &[0x91, 0xef, 1, 0]);
            assert_eq!(extract(32), bios);
            assert_eq!(extract(96), efi);
        } else {
            assert_eq!(extract(32), efi);
        }
    }
}

#[test]
fn rejects_conflicting_paths_and_traversal_and_covers_wim_resource_bounds() {
    let efi = vec![0; 2048];
    for paths in [["Boot/a", "boot/A"], ["boot", "boot/x"], ["../boot", "x"]] {
        let members = paths.map(|path| Member {
            path: path.into(),
            size: 512,
        });
        assert!(Image::prepare(&members, &efi, None).is_err());
    }
    let mut header = vec![0; 208];
    header[..8].copy_from_slice(b"MSWIM\0\0\0");
    header[8..12].copy_from_slice(&208u32.to_le_bytes());
    header[40..44].copy_from_slice(&[1, 0, 1, 0]);
    header[44..48].copy_from_slice(&2u32.to_le_bytes());
    header[96..104].copy_from_slice(&12u64.to_le_bytes());
    header[104..112].copy_from_slice(&220u64.to_le_bytes());
    header[120..124].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        crate::maintenance_media::wim_index(&header, 256).unwrap(),
        2
    );
    assert!(crate::maintenance_media::wim_index(&header, 228).is_err());
    header[120..124].fill(0);
    assert!(crate::maintenance_media::wim_index(&header, 256).is_err());
}
