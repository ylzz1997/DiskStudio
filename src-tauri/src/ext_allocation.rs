//! Conservative Ext2/3/4 allocation inspection. Uninitialized block groups are
//! retained in full; implicit allocation is never guessed from zeroed bitmaps.
use crate::used_space::{stop, Allocation};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn u16at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
}
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn at(r: &mut (impl Read + Seek), offset: u64, n: usize) -> Result<Vec<u8>, String> {
    r.seek(SeekFrom::Start(offset)).map_err(err)?;
    let mut b = vec![0; n];
    r.read_exact(&mut b).map_err(err)?;
    Ok(b)
}
fn crc(mut value: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        value ^= b as u32;
        for _ in 0..8 {
            value = (value >> 1) ^ (0x82f63b78 & 0u32.wrapping_sub(value & 1));
        }
    }
    value
}
fn crc16(mut value: u16, bytes: &[u8]) -> u16 {
    for &b in bytes {
        value ^= b as u16;
        for _ in 0..8 {
            value = (value >> 1) ^ (0xa001 & 0u16.wrapping_sub(value & 1));
        }
    }
    value
}
fn power(mut n: u64, p: u64) -> bool {
    if n == 0 {
        return false;
    }
    while n % p == 0 {
        n /= p;
    }
    n == 1
}

pub(crate) fn has_magic(r: &mut (impl Read + Seek), size: u64) -> Result<bool, String> {
    if size < 2048 {
        return Ok(false);
    }
    r.seek(SeekFrom::Start(1024 + 0x38)).map_err(err)?;
    let mut magic = [0; 2];
    r.read_exact(&mut magic).map_err(err)?;
    Ok(magic == [0x53, 0xef])
}

pub fn inspect(r: &mut (impl Read + Seek), cancel: &AtomicBool) -> Result<Allocation, String> {
    let size = r.seek(SeekFrom::End(0)).map_err(err)?;
    let s = at(r, 1024, 1024)?;
    let compat = u32at(&s, 0x5c);
    let inc = u32at(&s, 0x60);
    let ro = u32at(&s, 0x64);
    if u16at(&s, 0x38) != 0xef53 || u16at(&s, 0x3a) != 1 || u32at(&s, 0xe8) != 0 || inc & 4 != 0 {
        return Err("Ext 文件系统未干净卸载或需要日志恢复。".into());
    }
    // Orphan-file support is safe only with ORPHAN_PRESENT absent (excluded
    // below). Encryption changes file contents, not block-bitmap semantics;
    // allocated encrypted blocks are copied verbatim, never decrypted here.
    if compat & !(0x3c | 0x1000) != 0
        || inc & !(0x2 | 0x40 | 0x80 | 0x200 | 0x2000 | 0x10000) != 0
        || ro & !(0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x100 | 0x400) != 0
    {
        return Err(format!("Ext 含当前不能安全解释分配位图的特性（compat={compat:#x}, incompat={inc:#x}, ro_compat={ro:#x}）。"));
    }
    let log = u32at(&s, 0x18);
    if log > 2 || u32at(&s, 0x1c) != log {
        return Err("Ext 已用空间处理支持 1/2/4 KiB 普通块，不支持 bigalloc。".into());
    }
    let block = 1024u64 << log;
    let wide = inc & 0x80 != 0;
    let metadata = ro & 0x400 != 0;
    let gdt = ro & 0x10 != 0;
    let blocks = u32at(&s, 4) as u64 | ((u32at(&s, 0x150) as u64) << 32);
    let first = u32at(&s, 0x14) as u64;
    let bpg = u32at(&s, 0x20) as u64;
    let ipg = u32at(&s, 0x28) as u64;
    let inode_size = if u32at(&s, 0x4c) == 0 {
        128
    } else {
        u16at(&s, 0x58) as u64
    };
    if first != u64::from(block == 1024)
        || bpg == 0
        || bpg > block * 8
        || bpg % 8 != 0
        || ipg == 0
        || ipg > block * 8
        || inode_size < 128
        || inode_size > block
        || !inode_size.is_power_of_two()
        || blocks <= first
        || blocks.checked_mul(block).is_none_or(|n| n > size)
        || (!wide && u32at(&s, 0x150) != 0)
    {
        return Err("Ext 文件系统几何或 inode 表范围无效。".into());
    }
    if inc & 0x2000 != 0 && !metadata {
        return Err("Ext 校验和种子缺少 metadata_csum 特性。".into());
    }
    if metadata && (s[0x175] != 1 || crc(!0, &s[..0x3fc]) != u32at(&s, 0x3fc)) {
        return Err("Ext 超级块校验失败。".into());
    }
    let seed = if inc & 0x2000 != 0 {
        u32at(&s, 0x270)
    } else {
        crc(!0, &s[0x68..0x78])
    };
    let groups = (blocks - first).div_ceil(bpg);
    let ds = if wide { u16at(&s, 0xfe) as usize } else { 32 };
    if groups > 65536
        || (wide && ds != 64)
        || !matches!(ds, 32 | 64)
        || u32at(&s, 0) as u64 > groups * ipg
    {
        return Err("Ext 块组数量或描述符不支持。".into());
    }
    let descriptors = at(r, (first + 1) * block, groups as usize * ds)?;
    let table_blocks = (ipg * inode_size).div_ceil(block);
    let mut map = Allocation::new(
        if inc & 0x40 != 0 {
            "Ext4"
        } else if compat & 4 != 0 {
            "Ext3"
        } else {
            "Ext2"
        },
        size,
    );
    map.require(0, (first + 1) * block)?;
    let mut free_total = 0u64;
    let mut uninit = 0;
    for (group, d) in descriptors.chunks_exact(ds).enumerate() {
        stop(cancel)?;
        let index = (group as u32).to_le_bytes();
        if metadata {
            let mut raw = d.to_vec();
            raw[0x1e..0x20].fill(0);
            if crc(crc(seed, &index), &raw) as u16 != u16at(d, 0x1e) {
                return Err("Ext 块组描述符校验失败。".into());
            }
        } else if gdt {
            let mut sum = crc16(crc16(!0, &s[0x68..0x78]), &index);
            sum = crc16(sum, &d[..0x1e]);
            sum = crc16(sum, &d[0x20..]);
            if sum != u16at(d, 0x1e) {
                return Err("Ext 块组 CRC16 校验失败。".into());
            }
        }
        let flags = u16at(d, 0x12);
        if flags & !7 != 0 || (flags & 3 != 0 && !metadata && !gdt) {
            return Err("Ext 未初始化块组缺少可信校验或状态未知。".into());
        }
        let pointer = |lo: usize, hi: usize| -> u64 {
            u32at(d, lo) as u64 | if wide { (u32at(d, hi) as u64) << 32 } else { 0 }
        };
        let bb = pointer(0, 0x20);
        let ib = pointer(4, 0x24);
        let it = pointer(8, 0x28);
        for (start, count) in [(bb, 1), (ib, 1), (it, table_blocks)] {
            if start < first || start.checked_add(count).is_none_or(|n| n > blocks) {
                return Err("Ext 分配元数据位置越界。".into());
            }
            map.require(start * block, count * block)?;
        }
        let start = first + group as u64 * bpg;
        let count = bpg.min(blocks - start);
        let free = u16at(d, 0xc) as u64
            | if wide {
                (u16at(d, 0x2c) as u64) << 16
            } else {
                0
            };
        if free > count {
            return Err("Ext 块组空闲计数越界。".into());
        }
        free_total += free;
        if ro & 1 == 0 || group <= 1 || [3, 5, 7].iter().any(|&p| power(group as u64, p)) {
            let reserved = if compat & 0x10 != 0 {
                u16at(&s, 0xce) as u64
            } else {
                0
            };
            let n = 1 + (groups * ds as u64).div_ceil(block) + reserved;
            if n > count {
                return Err("Ext 超级块副本或保留描述符越界。".into());
            }
            map.require(start * block, n * block)?;
        }
        if flags & 2 != 0 {
            uninit += 1;
            continue;
        }
        let bitmap = at(r, bb * block, block as usize)?;
        if metadata {
            let expected = u16at(d, 0x18) as u32
                | if wide {
                    (u16at(d, 0x38) as u32) << 16
                } else {
                    0
                };
            let actual = crc(seed, &bitmap[..(bpg / 8) as usize]);
            if (if wide { actual } else { actual & 0xffff }) != expected {
                return Err("Ext 块分配位图校验失败。".into());
            }
        }
        let mut zeros = 0;
        for i in 0..count {
            if bitmap[i as usize / 8] & (1 << (i % 8)) == 0 {
                zeros += 1;
                map.free((start + i) * block, block)?;
            }
        }
        if zeros != free {
            return Err("Ext 分配位图与空闲块计数不一致。".into());
        }
    }
    let advertised = u32at(&s, 0xc) as u64
        | if wide {
            (u32at(&s, 0x158) as u64) << 32
        } else {
            0
        };
    if advertised != free_total {
        return Err("Ext 超级块与块组空闲计数不一致。".into());
    }
    if uninit > 0 {
        map.note = "完整保留未初始化的块组，数量为 {count}。".into();
        map.note_count = Some(uninit);
    }
    map.validate()?;
    Ok(map)
}
