//! Frozen first-run evidence and a strictly read-only sparse-stream diagnosis.
use super::*;
use allocation_batch::{record, tables};
use std::{
    io::Read,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};
pub(super) const FAILED: &str = "c93dbf7f17e2f1fc34899368b82fc0f5d4dde4907d1d03299ca484f483764dd7";
pub(super) const BASE: &str = "DS-streams-630e261db31240df9832f174540b201c.bin";
pub(super) fn frozen() -> Result<(), String> {
    inventory()?;
    let path = format!(r"C:\ProgramData\DiskStudio-ntfs-streams-{FAILED}.jsonl");
    let mut f = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(err)?;
    let meta = f.metadata().map_err(err)?;
    if !meta.is_file()
        || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || meta.len() > 512 * 1024
    {
        return Err("Frozen evidence is not a bounded ordinary file".into());
    }
    let mut text = String::new();
    (&mut f)
        .take(512 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(err)?;
    let events = text
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    if events.len() != 4
        || events[0]["event"] != "start"
        || events[0]["helper"] != FAILED
        || events[0]["desktop"]
            != "47b332a72bb230e4b660b15df6bcdda452dd8358ccfaae360d134d31f6b08cde"
        || events[1]["event"] != "seed-begin"
        || events[1]["base"] != BASE
        || events[1]["items"] != 24
        || events[2]["event"] != "seed-passed"
        || events[2]["items"] != 24
        || events[3]["event"] != "failed"
        || !events[3]["error"]
            .as_str()
            .unwrap_or("")
            .contains(&format!("{BASE}:sparse"))
        || !events[3]["error"]
            .as_str()
            .unwrap_or("")
            .contains("普通/稀疏数据流的压缩单元字段无效")
    {
        return Err("Frozen first-run sequence differs; no test data opened".into());
    }
    Ok(())
}
pub fn inspect() -> Result<String, String> {
    frozen()?;
    let mut log = backup_protection::begin("ntfs-streams-inspect")?;
    let result = (|| {
        let gpt = tables()?;
        let layout = format_checks::layouts(&[])?;
        let source = recovery_output_checks::selected(A, "NTFS", 2 * GIB)?;
        let resolved = data_windows::resolve(&source, true, false)?;
        let mut reader = data_windows::reader(&resolved)?;
        let mut files = vec![];
        let mut cursor = ntfs_scan::Checkpoint::default();
        ntfs_scan::continue_scan(
            &mut reader,
            recovery::ScanMode::Existing,
            &mut cursor,
            |_, _| Ok(()),
            |c| files.push(c),
            |_| Ok(()),
        )?;
        let found = files.iter().filter(|c| c.name == BASE).collect::<Vec<_>>();
        if found.len() != 1 {
            return Err("Frozen base candidate is not unique".into());
        }
        let recovery::Locator::Ntfs {
            record: number,
            sequence,
            ..
        } = found[0].locator
        else {
            return Err("Wrong source filesystem".into());
        };
        let ntfs = ntfs::Ntfs::new(&mut reader).map_err(err)?;
        let file = ntfs.file(&mut reader, number).map_err(err)?;
        if file.sequence_number() != sequence {
            return Err("Source record changed".into());
        }
        let mut attrs = file.attributes();
        let mut count = 0;
        let mut headers = vec![];
        while let Some(item) = attrs.next(&mut reader) {
            count += 1;
            if count > 4096 {
                return Err("Too many attributes".into());
            }
            let item = item.map_err(err)?;
            let a = item.to_attribute().map_err(err)?;
            if a.ty().map_err(err)? != ntfs::NtfsAttributeType::Data || a.is_resident() {
                continue;
            }
            let b = a.record_bytes();
            let name = ntfs_streams::attribute_name(b)?;
            if !["plain", "sparse", "packed"].iter().any(|n| {
                n.encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .eq(name.iter().copied())
            }) {
                continue;
            }
            if b.len() < 72 {
                return Err("Header truncated".into());
            }
            let word = |at| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
            headers.push(serde_json::json!({"name":ntfs_streams::display(BASE,name),"flags":a.flags().bits(),"lowVcn":word(16),"highVcn":word(24),"unit":b[34],"reserved":&b[35..40],"allocated":word(40),"size":word(48),"initialized":word(56),"stored":if a.flags().bits()&0x8001!=0 {Some(word(64))} else {None}}));
        }
        if headers.len() != 3 {
            return Err("Expected three nonresident stream headers".into());
        }
        drop(reader);
        if tables()? != gpt || format_checks::layouts(&[])? != layout {
            return Err("GPT/layout changed during diagnosis".into());
        }
        record(
            &mut log,
            serde_json::json!({"event":"inspected","record":number,"sequence":sequence,"cluster":ntfs.cluster_size(),"headers":headers,"noSourceWriteOrLock":true}),
        )?;
        Ok(format!(
            "READ ONLY: three stream headers; no lock, source rewrite, export or retry.\n\n{}",
            serde_json::to_string_pretty(&headers).map_err(err)?
        ))
    })();
    if let Err(e) = &result {
        record(&mut log, serde_json::json!({"event":"failed","error":e}))?;
    }
    result
}
