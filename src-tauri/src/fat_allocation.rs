//! Allocation inspection is stricter than recovery: a dirty or inconsistent
//! volume must never supply ranges that a copy is allowed to discard.
use super::*;
use crate::used_space::Allocation;
use std::sync::atomic::AtomicBool;

pub fn inspect<R: Read + Seek>(r: &mut R, cancel: &AtomicBool) -> Result<Allocation, String> {
    let size = r.seek(SeekFrom::End(0)).map_err(e)?;
    let fs = Fs::load(r)?;
    let boot = at(r, 0, 512)?;
    let mut map = Allocation::new(
        if fs.bits == 64 {
            "exFAT".into()
        } else {
            format!("FAT{}", fs.bits)
        },
        size,
    );
    map.require(0, fs.heap)?;
    if fs.bits == 64 {
        let sector = 1usize << boot[108];
        // Only a clean, single-FAT exFAT volume can provide an authoritative
        // bitmap. Media-failure, unknown/reserved flags and TexFAT fail closed.
        if u16at(&boot, 106) != 0 {
            return Err("exFAT 卷不干净、存在介质错误或未知状态，不能按已用空间复制。".into());
        }
        let main = at(r, 0, sector * 12)?;
        let backup = at(r, (sector * 12) as u64, sector * 12)?;
        let mut checksum = 0u32;
        for (i, &v) in backup[..sector * 11].iter().enumerate() {
            if ![106, 107, 112].contains(&i) {
                checksum = checksum.rotate_right(1).wrapping_add(v as u32);
            }
        }
        if backup[sector * 11..]
            .chunks_exact(4)
            .any(|v| u32at(v, 0) != checksum)
            || main[..sector * 11]
                .iter()
                .zip(&backup[..sector * 11])
                .enumerate()
                .any(|(i, (a, b))| ![106, 107, 112].contains(&i) && a != b)
        {
            return Err("exFAT 主备引导区不一致或校验失败。".into());
        }
        // Loading the root and allocation bitmap also records their exact
        // physical reads in the caller's Audit. Inspect the upcase table too:
        // its allocation is needed even though it is not used to find free bits.
        let root = fs.directory(r, fs.root, false, 0)?;
        let entries: Vec<_> = root
            .chunks_exact(32)
            .take_while(|x| x[0] != 0)
            .filter(|x| x[0] == 0x82)
            .collect();
        if entries.len() != 1 {
            return Err("exFAT 大小写表缺失或不唯一。".into());
        }
        let entry = entries[0];
        let length = u64at(entry, 24);
        if length == 0 || length > 131072 {
            return Err("exFAT 大小写表长度异常。".into());
        }
        let bytes = fs.content(r, u32at(entry, 20), length, false, false)?;
        let actual = bytes
            .iter()
            .fold(0u32, |h, &v| h.rotate_right(1).wrapping_add(v as u32));
        if actual != u32at(entry, 4) {
            return Err("exFAT 大小写表校验失败。".into());
        }
        for i in 0..fs.count as usize {
            if i % 65536 == 0 {
                crate::used_space::stop(cancel)?;
            }
            if fs.bitmap[i / 8] & (1 << (i % 8)) == 0 {
                map.free(fs.heap + i as u64 * fs.cluster, fs.cluster)?;
            }
        }
    } else {
        let length = ((fs.count as u64 + 2) * fs.bits as u64).div_ceil(8);
        if length > 64 * 1024 * 1024 || length > fs.fat_len {
            return Err("FAT 分配表超出 64 MiB 分析预算或声明范围。".into());
        }
        let table = at(r, fs.fat, length as usize)?;
        if fs.mirror && fs.copies == 2 && at(r, fs.fat + fs.fat_len, length as usize)? != table {
            return Err("FAT 主副分配表不一致。".into());
        }
        let entry = |c: u32| -> u32 {
            match fs.bits {
                12 => {
                    let v = u16at(&table, c as usize * 3 / 2) as u32;
                    if c & 1 == 0 {
                        v & 0xfff
                    } else {
                        v >> 4
                    }
                }
                16 => u16at(&table, c as usize * 2) as u32,
                _ => u32at(&table, c as usize * 4) & 0xfffffff,
            }
        };
        let clean_mask = match fs.bits {
            16 => 0xc000,
            32 => 0xc000000,
            _ => 0,
        };
        let reserved_mask = match fs.bits {
            12 => 0xf00,
            16 => 0xff00,
            _ => 0xfffff00,
        };
        if entry(1) & clean_mask != clean_mask
            || entry(0) & reserved_mask != reserved_mask
            || entry(0) as u8 != boot[21]
        {
            return Err("FAT 卷未干净卸载、存在 I/O 错误或保留项不合法。".into());
        }
        if fs.bits == 32 {
            let sector = u16at(&boot, 11) as usize;
            let backup = u16at(&boot, 50) as u64;
            if backup == 0 || backup == 0xffff || backup >= u16at(&boot, 14) as u64 {
                return Err("FAT32 备份引导扇区不可验证。".into());
            }
            if at(r, backup * sector as u64, 512)? != boot {
                return Err("FAT32 主备引导扇区不一致。".into());
            }
            if fs.root < 2 || fs.root >= fs.count + 2 || entry(fs.root) == 0 {
                return Err("FAT32 根目录被标为空闲。".into());
            }
            map.require(fs.offset(fs.root)?, fs.cluster)?;
        }
        for c in 2..fs.count + 2 {
            if c % 65536 == 0 {
                crate::used_space::stop(cancel)?;
            }
            let value = entry(c);
            if value == 0 {
                map.free(fs.offset(c)?, fs.cluster)?;
            } else if value < 2
                || (value
                    < match fs.bits {
                        12 => 0xff0,
                        16 => 0xfff0,
                        _ => 0xffffff0,
                    }
                    && value >= fs.count + 2)
            {
                return Err("FAT 分配链含越界簇引用。".into());
            }
        }
    }
    map.validate()?;
    Ok(map)
}
