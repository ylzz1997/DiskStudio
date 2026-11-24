use super::{
    empty_mbr, extended, label, mbr_record, table_archive, u32le, vacant_ebr, Archive, Conversion,
    Extent, Kind, PartitionChange, Preview, Scheme, Uuid, EXTENDED,
};
use serde::Serialize;
use std::io::{Read, Seek};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Form {
    Primary,
    Logical,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlacementChange {
    pub offset: u64,
    pub from: Form,
    pub to: Form,
}

#[derive(Clone, Copy)]
struct Member {
    offset: u64,
    size: u64,
    kind: u8,
    form: Form,
    ebr: Option<u64>,
}

struct Layout {
    members: Vec<Member>,
    primary: Vec<u8>,
    extended_slot: Option<usize>,
}

// Call only after the archive has proved every entry and EBR link.
fn layout(archive: &Archive) -> Result<Layout, String> {
    let mut members = Vec::new();
    let primary = archive
        .extents
        .first()
        .filter(|extent| extent.kind == Kind::Mbr)
        .ok_or("请选择已通过检查的 MBR 整盘镜像。")?
        .bytes
        .clone();
    let mut extended_slot = None;
    for extent in &archive.extents {
        let slots = match extent.kind {
            Kind::Mbr => &extent.bytes[446..510],
            Kind::Ebr => &extent.bytes[446..462],
            _ => continue,
        };
        for (slot, entry) in slots.chunks_exact(16).enumerate() {
            if entry[0] != 0 {
                return Err(
                    "镜像中有活动分区。主分区和逻辑分区的转换仅适用于非活动数据镜像。".into(),
                );
            }
            let kind = entry[4];
            if kind == 0 {
                continue;
            }
            if extended(kind) {
                extended_slot = Some(slot);
                continue;
            }
            let ebr = (extent.kind == Kind::Ebr).then_some(extent.offset);
            members.push(Member {
                offset: extent.offset + u64::from(u32le(&entry[8..12])) * archive.sector,
                size: u64::from(u32le(&entry[12..16])) * archive.sector,
                kind,
                form: if ebr.is_some() {
                    Form::Logical
                } else {
                    Form::Primary
                },
                ebr,
            });
        }
    }
    members.sort_unstable_by_key(|part| part.offset);
    Ok(Layout {
        members,
        primary,
        extended_slot,
    })
}

fn record(bytes: &mut [u8], slot: usize) -> &mut [u8] {
    &mut bytes[446 + 16 * slot..462 + 16 * slot]
}

fn build_chain(
    reader: &mut (impl Read + Seek),
    source: &Archive,
    members: &[Member],
    extents: &mut Vec<Extent>,
    slot: usize,
    tick: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let sector = source.sector;
    let mut logical = members.iter().filter(|part| part.form == Form::Logical);
    let first = logical.next().ok_or("缺少逻辑分区。")?;
    let last = logical.next_back().unwrap_or(first);
    let first = first.offset;
    let end = last.offset + last.size;
    let lower = members
        .iter()
        .filter(|part| part.form == Form::Primary && part.offset < first)
        .map(|part| part.offset + part.size)
        .max()
        .unwrap_or(sector);
    if members
        .iter()
        .any(|part| part.form == Form::Primary && (first..end).contains(&part.offset))
    {
        return Err("扩展分区将跨过其他主分区；保留原有数据位置时无法进行此转换。".into());
    }

    let mut positions = Vec::new();
    let mut prior = sector;
    for part in members {
        if part.form == Form::Logical {
            tick()?;
            let at = match part.ebr.filter(|at| *at >= lower) {
                Some(at) => at,
                None => vacant_ebr(reader, source, prior.max(lower), part.offset)?,
            };
            positions.push((at, part));
        }
        prior = part.offset + part.size;
    }
    positions.sort_unstable_by_key(|&(at, _)| at);
    let base = positions[0].0 / sector;
    let end = end / sector;
    let container = &mut extents[0].bytes;
    mbr_record(container, slot, EXTENDED, base, end - base)?;
    for (index, &(offset, part)) in positions.iter().enumerate() {
        let at = offset / sector;
        let mut bytes = source
            .extents
            .iter()
            .find(|e| e.kind == Kind::Ebr && e.offset == offset)
            .map_or_else(|| empty_mbr(sector), |e| e.bytes.clone());
        bytes[446..510].fill(0);
        mbr_record(
            &mut bytes,
            0,
            part.kind,
            part.offset / sector - at,
            part.size / sector,
        )?;
        if let Some(&(next, _)) = positions.get(index + 1) {
            let next = next / sector;
            mbr_record(&mut bytes, 1, EXTENDED, next - base, end - next)?;
        }
        extents.push(Extent {
            kind: Kind::Ebr,
            offset,
            bytes,
        });
    }
    Ok(())
}

impl Conversion {
    pub(crate) fn prepare_placement(
        reader: &mut (impl Read + Seek),
        size: u64,
        sector: u64,
        source_key: String,
        selection: (u64, u64),
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let source = table_archive::capture(reader, size, sector, source_key, true, &mut tick)?;
        if source.validate()?.scheme != Scheme::Mbr.name() || size / sector > 1u64 << 32 {
            return Err("请选择已通过检查且容量在 MBR 范围内的整盘镜像。".into());
        }
        let Layout {
            mut members,
            mut primary,
            extended_slot,
        } = layout(&source)?;
        let selected = members
            .iter_mut()
            .find(|part| (part.offset, part.size) == selection)
            .ok_or("所选分区与镜像中的已校验范围不一致。")?;
        if !matches!(
            selected.kind,
            0x01 | 0x04 | 0x06 | 0x07 | 0x0b | 0x0c | 0x0e | 0x82 | 0x83 | 0x8e | 0xfd
        ) {
            return Err(format!(
                "不支持转换所选分区的类型 {}。",
                label(selected.kind)
            ));
        }
        let from = selected.form;
        let to = match from {
            Form::Primary => {
                let slot = (0..4)
                    .find(|&slot| {
                        let entry = record(&mut primary, slot);
                        entry[4] == selected.kind
                            && u64::from(u32le(&entry[8..12])) * sector == selected.offset
                    })
                    .ok_or("所选主分区条目已失效。")?;
                record(&mut primary, slot).fill(0);
                Form::Logical
            }
            Form::Logical => Form::Primary,
        };
        selected.form = to;
        let selected = *selected;
        let logical_count = members
            .iter()
            .filter(|part| part.form == Form::Logical)
            .count();
        if logical_count > disk_core::mbr::MAX_EBRS {
            return Err("转换后的 EBR 链超过 64 个扇区的支持上限。".into());
        }
        if let Some(slot) = extended_slot {
            record(&mut primary, slot).fill(0);
        }
        let container_slot = if logical_count > 0 {
            Some(
                extended_slot
                    .or_else(|| (0..4).find(|&slot| record(&mut primary, slot)[4] == 0))
                    .ok_or("MBR 中没有可用于扩展分区的表项。")?,
            )
        } else {
            None
        };
        if to == Form::Primary {
            let slot = (0..4)
                .find(|&slot| Some(slot) != container_slot && record(&mut primary, slot)[4] == 0)
                .ok_or("MBR 中没有空闲的主分区表项。")?;
            mbr_record(
                &mut primary,
                slot,
                selected.kind,
                selected.offset / sector,
                selected.size / sector,
            )?;
        }

        let previous = u32le(&primary[440..444]);
        let mut signature = u32le(&Uuid::new_v4().as_bytes()[..4]).max(1);
        if signature == previous {
            signature = signature.wrapping_add(1).max(1);
        }
        primary[440..444].copy_from_slice(&signature.to_le_bytes());
        let mut extents = vec![Extent {
            kind: Kind::Mbr,
            offset: 0,
            bytes: primary,
        }];
        if let Some(slot) = container_slot {
            build_chain(reader, &source, &members, &mut extents, slot, &mut tick)?;
        }
        let output = Archive {
            version: 1,
            size,
            sector,
            source: source.source.clone(),
            includes_boot: false,
            extents,
        };
        output.validate()?;
        let identity = |part: &Member| (part.offset, part.size, part.kind, part.form);
        if !layout(&output)?
            .members
            .iter()
            .map(identity)
            .eq(members.iter().map(identity))
        {
            return Err("生成的 MBR 未保留预期的分区范围、类型和主/逻辑形式。".into());
        }
        let partitions = members
            .iter()
            .map(|part| PartitionChange {
                offset: part.offset,
                size: part.size,
                old_type: label(part.kind),
                new_type: label(part.kind),
                logical: part.form == Form::Logical,
            })
            .collect();
        Self::finish_plan(
            reader,
            source,
            output,
            Preview {
                from: Scheme::Mbr,
                to: Scheme::Mbr,
                sector,
                disk_id: format!("{signature:08X}"),
                partitions,
                table_bytes: 0,
                placement: Some(PlacementChange {
                    offset: selected.offset,
                    from,
                    to,
                }),
                restored_from: None,
                gpt_repair: None,
                registration: None,
            },
            tick,
        )
    }
}
